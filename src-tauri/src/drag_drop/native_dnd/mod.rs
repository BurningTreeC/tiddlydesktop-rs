//! Native drag-and-drop protocol handling for Linux
//!
//! This module provides a unified interface for native drag-and-drop protocols:
//! - Wayland: wl_data_device protocol
//! - X11: XDND protocol
//!
//! The key benefit of using native protocols is that they correctly track which
//! surface/window the pointer is over during drag operations, which GTK's
//! abstraction layer doesn't properly expose on Wayland.

mod native_dnd_wayland;
mod native_dnd_x11;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// Display server type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayServer {
    Wayland,
    X11,
    Unknown,
}

static DISPLAY_SERVER: OnceLock<DisplayServer> = OnceLock::new();
static INITIALIZED: AtomicBool = AtomicBool::new(false);

/// Detect which display server we're running on
fn detect_display_server() -> DisplayServer {
    *DISPLAY_SERVER.get_or_init(|| {
        // Check WAYLAND_DISPLAY first
        if std::env::var("WAYLAND_DISPLAY").is_ok() {
            eprintln!("[TiddlyDesktop] Native DnD: Detected Wayland display server");
            return DisplayServer::Wayland;
        }

        // Check XDG_SESSION_TYPE
        if let Ok(session_type) = std::env::var("XDG_SESSION_TYPE") {
            match session_type.as_str() {
                "wayland" => {
                    eprintln!("[TiddlyDesktop] Native DnD: Detected Wayland via XDG_SESSION_TYPE");
                    return DisplayServer::Wayland;
                }
                "x11" => {
                    eprintln!("[TiddlyDesktop] Native DnD: Detected X11 via XDG_SESSION_TYPE");
                    return DisplayServer::X11;
                }
                _ => {}
            }
        }

        // Check DISPLAY for X11
        if std::env::var("DISPLAY").is_ok() {
            eprintln!("[TiddlyDesktop] Native DnD: Detected X11 via DISPLAY");
            return DisplayServer::X11;
        }

        eprintln!("[TiddlyDesktop] Native DnD: Could not detect display server");
        DisplayServer::Unknown
    })
}

/// Get the current display server type
pub fn get_display_server() -> DisplayServer {
    detect_display_server()
}

/// Initialize the native DnD protocol handler
pub fn init() -> Result<bool, String> {
    if INITIALIZED.swap(true, Ordering::SeqCst) {
        return Ok(true); // Already initialized
    }

    match detect_display_server() {
        DisplayServer::Wayland => {
            match native_dnd_wayland::init() {
                Ok(true) => {
                    eprintln!("[TiddlyDesktop] Native DnD: Wayland backend initialized");
                    Ok(true)
                }
                Ok(false) => {
                    eprintln!("[TiddlyDesktop] Native DnD: Wayland not available");
                    Ok(false)
                }
                Err(e) => {
                    eprintln!("[TiddlyDesktop] Native DnD: Wayland init error: {}", e);
                    Err(e)
                }
            }
        }
        DisplayServer::X11 => {
            match native_dnd_x11::init() {
                Ok(true) => {
                    eprintln!("[TiddlyDesktop] Native DnD: X11 backend initialized");
                    Ok(true)
                }
                Ok(false) => {
                    eprintln!("[TiddlyDesktop] Native DnD: X11 not available");
                    Ok(false)
                }
                Err(e) => {
                    eprintln!("[TiddlyDesktop] Native DnD: X11 init error: {}", e);
                    Err(e)
                }
            }
        }
        DisplayServer::Unknown => {
            eprintln!("[TiddlyDesktop] Native DnD: Unknown display server, not initializing");
            Ok(false)
        }
    }
}

/// Register a window/surface with its label for drag target tracking
///
/// On X11: `id` is the X11 window ID
/// On Wayland: `id` is the wl_surface protocol ID
pub fn register_surface(id: u32, label: &str) {
    match detect_display_server() {
        DisplayServer::Wayland => {
            native_dnd_wayland::register_surface(id, label);
        }
        DisplayServer::X11 => {
            native_dnd_x11::register_window(id, label);
        }
        DisplayServer::Unknown => {}
    }
}
