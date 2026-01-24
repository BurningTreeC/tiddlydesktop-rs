//! Input injection for resetting WebKitGTK pointer state on Linux
//!
//! After a GTK drag operation (re-entry + drop scenario), WebKitGTK's internal
//! pointer event tracking can become corrupted, causing pointerdown events to
//! stop firing. This module provides platform-specific input injection to
//! send a real native click that resets WebKitGTK's state.
//!
//! Backends:
//! - X11: Uses XTest via enigo's xdo backend
//! - Wayland + GNOME/KDE: Uses libei via enigo's libei backend
//! - Wayland + wlroots: No input injection available (falls back to JS mousedown)

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Once;

/// Cached detection of display server type
static DISPLAY_SERVER_DETECTED: Once = Once::new();
static IS_WAYLAND: AtomicBool = AtomicBool::new(false);
static IS_X11: AtomicBool = AtomicBool::new(false);

/// Result of attempting input injection
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum InjectResult {
    /// Successfully injected input event
    Success,
    /// libei not available (wlroots or old compositor)
    LibeiUnavailable,
    /// X11/XTest not available
    X11Unavailable,
    /// Unknown display server
    UnknownDisplayServer,
    /// Other error during injection
    Error,
}

/// Detect whether we're running on X11 or Wayland
fn detect_display_server() {
    DISPLAY_SERVER_DETECTED.call_once(|| {
        // Check WAYLAND_DISPLAY first
        if std::env::var("WAYLAND_DISPLAY").is_ok() {
            IS_WAYLAND.store(true, Ordering::SeqCst);
            eprintln!("[TiddlyDesktop] Input inject: Detected Wayland display server");
            return;
        }

        // Check XDG_SESSION_TYPE
        if let Ok(session_type) = std::env::var("XDG_SESSION_TYPE") {
            match session_type.as_str() {
                "wayland" => {
                    IS_WAYLAND.store(true, Ordering::SeqCst);
                    eprintln!("[TiddlyDesktop] Input inject: Detected Wayland via XDG_SESSION_TYPE");
                    return;
                }
                "x11" => {
                    IS_X11.store(true, Ordering::SeqCst);
                    eprintln!("[TiddlyDesktop] Input inject: Detected X11 via XDG_SESSION_TYPE");
                    return;
                }
                _ => {}
            }
        }

        // Check GDK_BACKEND
        if let Ok(gdk_backend) = std::env::var("GDK_BACKEND") {
            if gdk_backend.contains("wayland") {
                IS_WAYLAND.store(true, Ordering::SeqCst);
                eprintln!("[TiddlyDesktop] Input inject: Detected Wayland via GDK_BACKEND");
                return;
            }
            if gdk_backend.contains("x11") {
                IS_X11.store(true, Ordering::SeqCst);
                eprintln!("[TiddlyDesktop] Input inject: Detected X11 via GDK_BACKEND");
                return;
            }
        }

        // Check DISPLAY for X11
        if std::env::var("DISPLAY").is_ok() {
            IS_X11.store(true, Ordering::SeqCst);
            eprintln!("[TiddlyDesktop] Input inject: Detected X11 via DISPLAY");
            return;
        }

        eprintln!("[TiddlyDesktop] Input inject: Could not detect display server");
    });
}

/// Check if we're on Wayland
#[allow(dead_code)]
pub fn is_wayland() -> bool {
    detect_display_server();
    IS_WAYLAND.load(Ordering::SeqCst)
}

/// Check if we're on X11
#[allow(dead_code)]
pub fn is_x11() -> bool {
    detect_display_server();
    IS_X11.load(Ordering::SeqCst)
}

/// Inject a mouse click at the specified screen coordinates
/// This sends a real native mouse button press/release through the input system
pub fn inject_click(screen_x: i32, screen_y: i32) -> InjectResult {
    detect_display_server();

    if IS_X11.load(Ordering::SeqCst) {
        inject_click_x11(screen_x, screen_y)
    } else if IS_WAYLAND.load(Ordering::SeqCst) {
        inject_click_wayland(screen_x, screen_y)
    } else {
        eprintln!("[TiddlyDesktop] Input inject: Unknown display server, cannot inject");
        InjectResult::UnknownDisplayServer
    }
}

/// Inject click on X11 using enigo's xdo backend
fn inject_click_x11(screen_x: i32, screen_y: i32) -> InjectResult {
    eprintln!(
        "[TiddlyDesktop] Input inject: Attempting X11 click at ({}, {})",
        screen_x, screen_y
    );

    // Try using enigo with xdo backend
    match inject_with_enigo(screen_x, screen_y) {
        Ok(()) => {
            eprintln!("[TiddlyDesktop] Input inject: X11 click successful via enigo");
            InjectResult::Success
        }
        Err(e) => {
            eprintln!("[TiddlyDesktop] Input inject: X11 enigo failed: {}", e);
            // Try GDK's test_simulate_button as fallback
            inject_click_gdk(screen_x, screen_y)
        }
    }
}

/// Inject click on Wayland
/// NOTE: libei requires RemoteDesktop portal permission which shows a dialog - too intrusive!
/// Instead, we'll try to reset WebKitGTK's state through webkit2gtk's evaluate_javascript API.
/// This is handled separately in linux.rs via reset_webkit_via_evaluate_javascript().
fn inject_click_wayland(_screen_x: i32, _screen_y: i32) -> InjectResult {
    eprintln!(
        "[TiddlyDesktop] Input inject: Wayland detected - will try webkit2gtk evaluate_javascript"
    );
    // Signal that we should try the webkit2gtk approach instead
    // This is handled in reset_webkit_pointer_state() in linux.rs
    InjectResult::LibeiUnavailable
}

/// Try to inject click using enigo (works for both X11 and Wayland with appropriate backend)
fn inject_with_enigo(screen_x: i32, screen_y: i32) -> Result<(), String> {
    use enigo::{Enigo, Mouse, Settings, Coordinate, Button, Direction};

    // Create enigo instance
    let mut enigo = Enigo::new(&Settings::default())
        .map_err(|e| format!("Failed to create Enigo: {:?}", e))?;

    // Move to position
    enigo
        .move_mouse(screen_x, screen_y, Coordinate::Abs)
        .map_err(|e| format!("Failed to move mouse: {:?}", e))?;

    // Small delay to let the move register
    std::thread::sleep(std::time::Duration::from_millis(10));

    // Click (press and release)
    enigo
        .button(Button::Left, Direction::Click)
        .map_err(|e| format!("Failed to click: {:?}", e))?;

    Ok(())
}

/// Fallback: try using GDK's test_simulate_button (X11 only via XTest)
fn inject_click_gdk(screen_x: i32, screen_y: i32) -> InjectResult {
    eprintln!(
        "[TiddlyDesktop] Input inject: Trying GDK test_simulate_button at ({}, {})",
        screen_x, screen_y
    );

    // Get the default display and screen
    let display = match gdk::Display::default() {
        Some(d) => d,
        None => {
            eprintln!("[TiddlyDesktop] Input inject: No GDK display");
            return InjectResult::X11Unavailable;
        }
    };

    let screen = display.default_screen();

    // Get the root window
    let root_window = screen.root_window();
    if root_window.is_none() {
        eprintln!("[TiddlyDesktop] Input inject: No root window");
        return InjectResult::X11Unavailable;
    }
    let root_window = root_window.unwrap();

    // Try to simulate button press
    let press_result = gdk::test_simulate_button(
        &root_window,
        screen_x,
        screen_y,
        1, // Left button
        gdk::ModifierType::empty(),
        gdk::EventType::ButtonPress,
    );

    if !press_result {
        eprintln!("[TiddlyDesktop] Input inject: GDK button press simulation failed");
        return InjectResult::X11Unavailable;
    }

    // Small delay
    std::thread::sleep(std::time::Duration::from_millis(10));

    // Simulate button release
    let release_result = gdk::test_simulate_button(
        &root_window,
        screen_x,
        screen_y,
        1, // Left button
        gdk::ModifierType::empty(),
        gdk::EventType::ButtonRelease,
    );

    if !release_result {
        eprintln!("[TiddlyDesktop] Input inject: GDK button release simulation failed");
        return InjectResult::X11Unavailable;
    }

    eprintln!("[TiddlyDesktop] Input inject: GDK test_simulate_button successful");
    InjectResult::Success
}

/// Inject a click and return whether a fallback (mousedown handler) is needed
/// Returns true if injection failed and JS mousedown fallback should be enabled
pub fn inject_click_or_need_fallback(screen_x: i32, screen_y: i32) -> bool {
    match inject_click(screen_x, screen_y) {
        InjectResult::Success => {
            eprintln!("[TiddlyDesktop] Input inject: Click injected successfully, no fallback needed");
            false
        }
        result => {
            eprintln!(
                "[TiddlyDesktop] Input inject: Injection failed ({:?}), mousedown fallback needed",
                result
            );
            true
        }
    }
}
