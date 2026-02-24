// Allow dead code on Android - many functions are desktop-only
#![cfg_attr(target_os = "android", allow(dead_code))]

use std::{collections::HashMap, path::PathBuf, process::{Child, Command}, sync::{Arc, Mutex, OnceLock}};
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

/// Global AppHandle for IPC callbacks that need Tauri access
static GLOBAL_APP_HANDLE: OnceLock<tauri::AppHandle> = OnceLock::new();

/// Resolved data directory — respects portable mode.
/// Set once at startup; used by wiki_storage, lan_sync, etc. instead of
/// `app.path().app_data_dir()` which always returns the system location.
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Get the resolved data directory (portable or system).
/// Falls back to the provided `app` handle if not yet initialized.
pub fn get_data_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    if let Some(dir) = DATA_DIR.get() {
        return Ok(dir.clone());
    }
    // Fallback: not yet initialized, resolve now
    let dir = resolve_data_dir(app)?;
    let _ = DATA_DIR.set(dir.clone());
    Ok(dir)
}

/// Resolve the data directory, checking for portable mode.
/// Portable mode: `portable` or `portable.txt` marker next to the executable,
/// or `tiddlydesktop.html` already exists next to the executable.
fn resolve_data_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    #[cfg(not(target_os = "android"))]
    {
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                if exe_dir.join("portable").exists()
                    || exe_dir.join("portable.txt").exists()
                    || exe_dir.join("tiddlydesktop.html").exists()
                {
                    return Ok(exe_dir.to_path_buf());
                }
            }
        }
    }
    app.path().app_data_dir().map_err(|e| e.to_string())
}

/// Get the global AppHandle (for use by drag_drop module to emit events)
pub fn get_global_app_handle() -> Option<tauri::AppHandle> {
    GLOBAL_APP_HANDLE.get().cloned()
}

/// Global IPC server for sending messages to wiki processes
static GLOBAL_IPC_SERVER: OnceLock<Arc<ipc::IpcServer>> = OnceLock::new();

/// Embedded TiddlyWiki resources ZIP for Android extraction
/// Generated at build time by build.rs
#[cfg(target_os = "android")]
static TIDDLYWIKI_ZIP: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/tiddlywiki.zip"));

/// JNI crate for Android JNI_OnLoad
#[cfg(target_os = "android")]
use jni;

/// Linux: Activate a window - uses X11 _NET_ACTIVE_WINDOW on X11, urgency hint on Wayland
#[cfg(target_os = "linux")]
fn linux_activate_window(gtk_window: &gtk::ApplicationWindow) {
    use gtk::prelude::{GtkWindowExt, WidgetExt};

    // Get the GDK window
    let gdk_window = match gtk_window.window() {
        Some(w) => w,
        None => {
            eprintln!("[Linux] No GDK window available");
            return;
        }
    };

    // Check if we're on X11 or Wayland using the native_dnd module's detection
    match drag_drop::native_dnd::get_display_server() {
        drag_drop::native_dnd::DisplayServer::X11 => {
            // X11: Use _NET_ACTIVE_WINDOW protocol
            x11_activate_window_impl(gtk_window, &gdk_window);
        }
        _ => {
            // Wayland: Best effort - urgency hint + present
            // Wayland prevents focus stealing by design; user must click the flashing taskbar
            eprintln!("[Wayland] Setting urgency hint (focus stealing not allowed on Wayland)");
            gtk_window.set_urgency_hint(true);
            gtk_window.present();
            // Clear urgency after a moment
            let win = gtk_window.clone();
            gtk::glib::timeout_add_local_once(
                std::time::Duration::from_millis(100),
                move || { win.set_urgency_hint(false); }
            );
        }
    }
}

/// X11-specific window activation using _NET_ACTIVE_WINDOW protocol
#[cfg(target_os = "linux")]
fn x11_activate_window_impl(gtk_window: &gtk::ApplicationWindow, gdk_window: &gtk::gdk::Window) {
    use gtk::prelude::GtkWindowExt;
    use gtk::glib::translate::ToGlibPtr;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{self, ConnectionExt};

    // Get the X11 window ID
    extern "C" {
        fn gdk_x11_window_get_xid(window: *mut gtk::gdk::ffi::GdkWindow) -> u32;
    }
    let xid = unsafe { gdk_x11_window_get_xid(gdk_window.to_glib_none().0) };
    if xid == 0 {
        eprintln!("[X11] Could not get X11 window ID");
        return;
    }

    eprintln!("[X11] Activating window with XID: {}", xid);

    // Also call GTK present for good measure
    gtk_window.present();

    // Connect to X11 and send _NET_ACTIVE_WINDOW message
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let (conn, screen_num) = x11rb::connect(None)?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;

        // Get the _NET_ACTIVE_WINDOW atom
        let atom_cookie = conn.intern_atom(false, b"_NET_ACTIVE_WINDOW")?;
        let atom = atom_cookie.reply()?.atom;

        // Send client message to root window
        let event = xproto::ClientMessageEvent {
            response_type: xproto::CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window: xid,
            type_: atom,
            data: xproto::ClientMessageData::from([
                1u32,  // Source indication: 1 = application
                0,     // Timestamp (0 = current)
                0,     // Currently active window (0 = none)
                0, 0,
            ]),
        };

        conn.send_event(
            false,
            root,
            xproto::EventMask::SUBSTRUCTURE_REDIRECT | xproto::EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )?;
        conn.flush()?;

        eprintln!("[X11] Sent _NET_ACTIVE_WINDOW message for XID {}", xid);
        Ok(())
    })();

    if let Err(e) = result {
        eprintln!("[X11] Failed to activate window: {}", e);
    }
}

/// Linux: Enable smooth (kinetic) scrolling on a WebKitGTK webview.
/// WebKitGTK defaults to non-smooth scrolling; this enables momentum/inertial scrolling.
#[cfg(target_os = "linux")]
fn enable_smooth_scrolling(window: &tauri::WebviewWindow) {
    use gtk::prelude::*;
    fn find_webkit(widget: &impl IsA<gtk::Widget>) -> Option<gtk::Widget> {
        let w = widget.upcast_ref::<gtk::Widget>();
        if w.type_().name().contains("WebKit") {
            return Some(w.clone());
        }
        if let Some(c) = w.downcast_ref::<gtk::Container>() {
            for child in c.children() {
                if let Some(found) = find_webkit(&child) {
                    return Some(found);
                }
            }
        }
        None
    }

    extern "C" {
        fn webkit_web_view_get_settings(
            web_view: *mut gtk::glib::gobject_ffi::GObject,
        ) -> *mut gtk::glib::gobject_ffi::GObject;
        fn webkit_settings_set_enable_smooth_scrolling(
            settings: *mut gtk::glib::gobject_ffi::GObject,
            enabled: i32,
        );
    }

    if let Ok(gtk_window) = window.gtk_window() {
        if let Some(webview_widget) = find_webkit(&gtk_window) {
            unsafe {
                use gtk::glib::object::ObjectExt;
                let wv_ptr = webview_widget.as_ptr() as *mut gtk::glib::gobject_ffi::GObject;
                let settings = webkit_web_view_get_settings(wv_ptr);
                if !settings.is_null() {
                    webkit_settings_set_enable_smooth_scrolling(settings, 1);
                }
            }
        }
    }
}

/// Linux: Set up a GtkHeaderBar on a window for reliable title display
/// This works around WebKitGTK's broken title propagation
/// Title starts empty - JavaScript will set the real title once TiddlyWiki loads
#[cfg(target_os = "linux")]
fn setup_header_bar(window: &tauri::WebviewWindow) {
    use gtk::prelude::{BoxExt, ButtonExt, ContainerExt, EventBoxExt, GtkWindowExt, HeaderBarExt, LabelExt, OverlayExt, StyleContextExt, WidgetExt, WidgetExtManual};
    use gtk::glib;

    // On X11, skip custom HeaderBar and use server-side decorations (WM titlebar).
    // CSD resize borders depend on _GTK_FRAME_EXTENTS support which many X11 WMs lack,
    // making windows unresizable. Wayland handles CSD natively so HeaderBar works fine.
    if matches!(drag_drop::native_dnd::get_display_server(), drag_drop::native_dnd::DisplayServer::X11) {
        return;
    }

    if let Ok(gtk_window) = window.gtk_window() {
        let header_bar = gtk::HeaderBar::new();
        header_bar.set_show_close_button(false); // We'll add our own
        header_bar.set_has_subtitle(false);

        // Create an EventBox that spans the full width and height for dragging
        let event_box = gtk::EventBox::new();
        event_box.set_visible_window(false);
        event_box.set_above_child(false); // Let child buttons receive clicks
        event_box.set_hexpand(true);
        event_box.set_vexpand(true);
        event_box.set_halign(gtk::Align::Fill);
        event_box.set_valign(gtk::Align::Fill);
        // Force minimum height to fill HeaderBar (typically ~46px on GNOME)
        event_box.set_size_request(-1, 46);

        // Use an Overlay: title label centered, close button overlaid on right
        let overlay = gtk::Overlay::new();
        overlay.set_hexpand(true);
        overlay.set_vexpand(true);
        overlay.set_valign(gtk::Align::Fill);

        // Title label - truly centered in the full width, styled as a titlebar title
        let title_label = gtk::Label::new(None);
        title_label.set_ellipsize(pango::EllipsizeMode::End);
        title_label.set_halign(gtk::Align::Center);
        title_label.set_valign(gtk::Align::Center);
        title_label.set_hexpand(true);
        title_label.style_context().add_class("title");
        overlay.add(&title_label); // Base widget

        // Favicon icon overlaid on the left - initially hidden, shown when set via set_window_icon
        let icon_image = gtk::Image::new();
        icon_image.set_halign(gtk::Align::Start);
        icon_image.set_valign(gtk::Align::Center);
        icon_image.set_margin_start(8);
        icon_image.set_widget_name("headerbar-favicon");
        icon_image.set_visible(false); // Hidden until favicon is set
        icon_image.set_no_show_all(true); // Don't show with show_all()
        overlay.add_overlay(&icon_image);

        // Button box for window controls (minimize and close) overlaid on the right
        let button_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        button_box.set_halign(gtk::Align::End);
        button_box.set_valign(gtk::Align::Center);
        button_box.set_margin_end(4);

        // Minimize button (first, so it appears above close)
        let minimize_button = gtk::Button::from_icon_name(Some("window-minimize-symbolic"), gtk::IconSize::Menu);
        minimize_button.style_context().add_class("titlebutton");
        minimize_button.style_context().add_class("minimize");
        let win_weak_minimize = glib::object::ObjectExt::downgrade(&gtk_window);
        minimize_button.connect_clicked(move |_| {
            if let Some(win) = win_weak_minimize.upgrade() {
                win.iconify();
            }
        });
        button_box.pack_start(&minimize_button, false, false, 0);

        // Close button (second, so it appears below minimize)
        let close_button = gtk::Button::from_icon_name(Some("window-close-symbolic"), gtk::IconSize::Menu);
        close_button.style_context().add_class("titlebutton");
        close_button.style_context().add_class("close");
        let win_weak_close = glib::object::ObjectExt::downgrade(&gtk_window);
        close_button.connect_clicked(move |_| {
            if let Some(win) = win_weak_close.upgrade() {
                win.close();
            }
        });
        button_box.pack_start(&close_button, false, false, 0);

        overlay.add_overlay(&button_box);

        event_box.add(&overlay);

        // Enable events on the event box for dragging
        event_box.add_events(
            gdk::EventMask::BUTTON_PRESS_MASK
            | gdk::EventMask::BUTTON_RELEASE_MASK
            | gdk::EventMask::POINTER_MOTION_MASK
        );

        // Use fixed 8-pixel threshold for window dragging (standard desktop feel)
        // This is independent of gtk-dnd-drag-threshold which we reduce for content drags
        let drag_threshold = 8;

        // Track drag state: (start_x, start_y, button, time)
        let drag_start: std::rc::Rc<std::cell::RefCell<Option<(f64, f64, u32, u32)>>> =
            std::rc::Rc::new(std::cell::RefCell::new(None));

        let win_weak = glib::object::ObjectExt::downgrade(&gtk_window);
        let drag_start_press = drag_start.clone();
        event_box.connect_button_press_event(move |_widget, event| {
            if event.button() == 1 {
                if let Some(win) = win_weak.upgrade() {
                    match event.event_type() {
                        gdk::EventType::DoubleButtonPress => {
                            // Clear any pending drag
                            *drag_start_press.borrow_mut() = None;
                            if win.is_maximized() {
                                win.unmaximize();
                            } else {
                                win.maximize();
                            }
                            return glib::Propagation::Stop;
                        }
                        gdk::EventType::ButtonPress => {
                            // Store press position, don't start drag yet
                            let (root_x, root_y) = event.root();
                            *drag_start_press.borrow_mut() = Some((root_x, root_y, event.button(), event.time()));
                            return glib::Propagation::Stop;
                        }
                        _ => {}
                    }
                }
            }
            glib::Propagation::Proceed
        });

        // Handle motion - start drag only after threshold exceeded
        let win_weak_motion = glib::object::ObjectExt::downgrade(&gtk_window);
        let drag_start_motion = drag_start.clone();
        event_box.connect_motion_notify_event(move |_widget, event| {
            // Copy the data out of the RefCell to avoid holding the borrow
            // while we later need to borrow_mut
            let drag_data = *drag_start_motion.borrow();
            if let Some((start_x, start_y, button, time)) = drag_data {
                let (current_x, current_y) = event.root();
                let dx = (current_x - start_x).abs();
                let dy = (current_y - start_y).abs();

                if dx > drag_threshold as f64 || dy > drag_threshold as f64 {
                    // Threshold exceeded, start the drag
                    *drag_start_motion.borrow_mut() = None;
                    if let Some(win) = win_weak_motion.upgrade() {
                        win.begin_move_drag(
                            button as i32,
                            start_x as i32,
                            start_y as i32,
                            time,
                        );
                    }
                    return glib::Propagation::Stop;
                }
            }
            glib::Propagation::Proceed
        });

        // Clear drag state on button release
        let drag_start_release = drag_start.clone();
        event_box.connect_button_release_event(move |_widget, event| {
            if event.button() == 1 {
                *drag_start_release.borrow_mut() = None;
            }
            glib::Propagation::Proceed
        });

        header_bar.set_custom_title(Some(&event_box));
        gtk_window.set_titlebar(Some(&header_bar));
        header_bar.show_all();
    }
}

/// Linux: Check if GNOME's auto-maximize is enabled
/// Returns true only if we're on GNOME and auto-maximize is set to true
#[cfg(target_os = "linux")]
fn linux_gnome_auto_maximize_enabled() -> bool {
    use std::process::Command;

    // Check if we're on GNOME
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    if !desktop.to_lowercase().contains("gnome") {
        return false;
    }

    // Check the gsettings value
    if let Ok(output) = Command::new("gsettings")
        .args(["get", "org.gnome.mutter", "auto-maximize"])
        .output()
    {
        let value = String::from_utf8_lossy(&output.stdout);
        return value.trim() == "true";
    }

    false
}

/// Linux: Clamp window dimensions to avoid GNOME's auto-maximize
/// GNOME auto-maximizes windows that are "almost fullscreen" (>~80% of monitor).
/// This function clamps dimensions to 80% of primary monitor to prevent that.
/// Only applies when GNOME's auto-maximize setting is enabled.
#[cfg(target_os = "linux")]
fn linux_clamp_window_size(width: f64, height: f64) -> (f64, f64) {
    use gtk::prelude::MonitorExt;

    // Only clamp if GNOME auto-maximize is enabled
    if !linux_gnome_auto_maximize_enabled() {
        return (width, height);
    }

    let display = gtk::gdk::Display::default().expect("No display");
    let monitor = display.primary_monitor()
        .or_else(|| display.monitor_at_point(0, 0));

    if let Some(monitor) = monitor {
        let geometry = monitor.geometry();
        let max_width = (geometry.width() as f64 * 0.80).floor();
        let max_height = (geometry.height() as f64 * 0.80).floor();

        let clamped_width = width.min(max_width);
        let clamped_height = height.min(max_height);

        if clamped_width != width || clamped_height != height {
            eprintln!("[Linux/GNOME] Clamped window size from {}x{} to {}x{} (80% of {}x{}) to prevent auto-maximize",
                width, height, clamped_width, clamped_height, geometry.width(), geometry.height());
        }

        (clamped_width, clamped_height)
    } else {
        (width, height)
    }
}

/// Linux: Finalize window state after creation
/// - Centers window if no saved position exists
/// - Handles maximize state
#[cfg(target_os = "linux")]
fn linux_finalize_window_state(window: &tauri::WebviewWindow, saved_state: &Option<crate::types::WindowState>) {
    use gtk::prelude::{GtkWindowExt, MonitorExt, WidgetExt};

    if let Ok(gtk_window) = window.gtk_window() {
        // If no saved state at all, center the window on the primary monitor
        if saved_state.is_none() {
            let (win_width, win_height) = gtk_window.size();
            let display = gtk_window.display();
            let monitor = display.primary_monitor()
                .or_else(|| display.monitor_at_point(0, 0));

            if let Some(monitor) = monitor {
                let geometry = monitor.geometry();
                let center_x = geometry.x() + (geometry.width() - win_width).max(0) / 2;
                let center_y = geometry.y() + (geometry.height() - win_height).max(0) / 2;
                gtk_window.move_(center_x, center_y);
                eprintln!("[Linux] Centered window at ({}, {}) on monitor {}x{}",
                    center_x, center_y, geometry.width(), geometry.height());
            }
        }

        // Handle maximize state
        if saved_state.as_ref().map(|s| s.maximized).unwrap_or(false) {
            gtk_window.maximize();
        }
    }
}

/// Windows flag to prevent console window from appearing
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Windows/macOS: Validate saved window position against current monitor configuration.
/// Returns adjusted (x, y) position in logical pixels that's guaranteed to be on a visible monitor.
///
/// If the saved position is on a currently visible monitor, returns it unchanged.
/// Otherwise, falls back to the monitor containing the mouse cursor and centers the window there.
///
/// Note: Saved state is in logical pixels. Monitor APIs return physical pixels, so we convert
/// using each monitor's scale factor for accurate comparison.
fn validate_window_position(
    app: &tauri::AppHandle,
    saved_state: &crate::types::WindowState,
) -> (f64, f64) {

    // Saved state is in logical pixels
    let saved_x = saved_state.x as f64;
    let saved_y = saved_state.y as f64;
    let win_width = saved_state.width as f64;
    let win_height = saved_state.height as f64;

    // Get all available monitors
    let monitors = match app.available_monitors() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[Window Position] Failed to get monitors: {}, using saved position", e);
            return (saved_x, saved_y);
        }
    };

    if monitors.is_empty() {
        eprintln!("[Window Position] No monitors found, using saved position");
        return (saved_x, saved_y);
    }

    // Check if saved position (logical) is within any monitor's bounds (converted to logical)
    // We check if the top-left corner of the window is on a monitor
    for monitor in &monitors {
        let scale = monitor.scale_factor();
        let pos = monitor.position();
        let size = monitor.size();
        // Convert physical to logical
        let mon_x = pos.x as f64 / scale;
        let mon_y = pos.y as f64 / scale;
        let mon_width = size.width as f64 / scale;
        let mon_height = size.height as f64 / scale;

        // Check if the saved position is within this monitor
        if saved_x >= mon_x && saved_x < mon_x + mon_width &&
           saved_y >= mon_y && saved_y < mon_y + mon_height {
            eprintln!("[Window Position] Saved position ({}, {}) is on monitor '{}' at logical ({}, {})",
                saved_x, saved_y,
                monitor.name().map(String::as_str).unwrap_or("unknown"),
                mon_x, mon_y);
            return (saved_x, saved_y);
        }
    }

    // Saved position is not on any visible monitor - fall back to cursor position
    eprintln!("[Window Position] Saved position ({}, {}) not on any visible monitor", saved_x, saved_y);

    // Get cursor position to find the "active" monitor
    // cursor_position() returns physical pixels
    let cursor_pos = match app.cursor_position() {
        Ok(pos) => pos,
        Err(e) => {
            eprintln!("[Window Position] Failed to get cursor position: {}, using primary monitor", e);
            // Fall back to primary monitor
            if let Ok(Some(primary)) = app.primary_monitor() {
                let scale = primary.scale_factor();
                let pos = primary.position();
                let size = primary.size();
                // Convert to logical for centering calculation
                let mon_x = pos.x as f64 / scale;
                let mon_y = pos.y as f64 / scale;
                let mon_width = size.width as f64 / scale;
                let mon_height = size.height as f64 / scale;
                let center_x = mon_x + (mon_width - win_width) / 2.0;
                let center_y = mon_y + (mon_height - win_height) / 2.0;
                eprintln!("[Window Position] Centering on primary monitor at logical ({}, {})", center_x, center_y);
                return (center_x, center_y);
            }
            // Last resort: use first monitor
            let monitor = &monitors[0];
            let scale = monitor.scale_factor();
            let pos = monitor.position();
            let size = monitor.size();
            let mon_x = pos.x as f64 / scale;
            let mon_y = pos.y as f64 / scale;
            let mon_width = size.width as f64 / scale;
            let mon_height = size.height as f64 / scale;
            let center_x = mon_x + (mon_width - win_width) / 2.0;
            let center_y = mon_y + (mon_height - win_height) / 2.0;
            return (center_x, center_y);
        }
    };

    // Find the monitor containing the cursor (cursor is in physical pixels)
    let cursor_x = cursor_pos.x;
    let cursor_y = cursor_pos.y;

    for monitor in &monitors {
        let pos = monitor.position();
        let size = monitor.size();
        // Compare cursor in physical pixel space
        let mon_x_phys = pos.x as f64;
        let mon_y_phys = pos.y as f64;
        let mon_width_phys = size.width as f64;
        let mon_height_phys = size.height as f64;

        if cursor_x >= mon_x_phys && cursor_x < mon_x_phys + mon_width_phys &&
           cursor_y >= mon_y_phys && cursor_y < mon_y_phys + mon_height_phys {
            // Center window on this monitor, return logical coordinates
            let scale = monitor.scale_factor();
            let mon_x = mon_x_phys / scale;
            let mon_y = mon_y_phys / scale;
            let mon_width = mon_width_phys / scale;
            let mon_height = mon_height_phys / scale;
            let center_x = mon_x + (mon_width - win_width) / 2.0;
            let center_y = mon_y + (mon_height - win_height) / 2.0;
            eprintln!("[Window Position] Cursor at ({}, {}), centering on monitor '{}' at logical ({}, {})",
                cursor_x, cursor_y,
                monitor.name().map(String::as_str).unwrap_or("unknown"),
                center_x, center_y);
            return (center_x, center_y);
        }
    }

    // Cursor not on any monitor (shouldn't happen), use primary or first monitor
    eprintln!("[Window Position] Cursor position ({}, {}) not on any monitor, using primary", cursor_x, cursor_y);
    if let Ok(Some(primary)) = app.primary_monitor() {
        let scale = primary.scale_factor();
        let pos = primary.position();
        let size = primary.size();
        let mon_x = pos.x as f64 / scale;
        let mon_y = pos.y as f64 / scale;
        let mon_width = size.width as f64 / scale;
        let mon_height = size.height as f64 / scale;
        let center_x = mon_x + (mon_width - win_width) / 2.0;
        let center_y = mon_y + (mon_height - win_height) / 2.0;
        return (center_x, center_y);
    }

    // Absolute fallback
    let monitor = &monitors[0];
    let scale = monitor.scale_factor();
    let pos = monitor.position();
    let size = monitor.size();
    let mon_x = pos.x as f64 / scale;
    let mon_y = pos.y as f64 / scale;
    let mon_width = size.width as f64 / scale;
    let mon_height = size.height as f64 / scale;
    let center_x = mon_x + (mon_width - win_width) / 2.0;
    let center_y = mon_y + (mon_height - win_height) / 2.0;
    (center_x, center_y)
}

/// Platform-specific drag-drop handling
mod drag_drop;

/// Auto-grant WebView permissions (camera, mic, geolocation) on desktop
#[cfg(not(target_os = "android"))]
mod permissions;

/// Force-link FFI functions for wry fork on Windows
/// The wry fork declares these as `extern "C"` and expects them to be provided.
/// Without this, the linker may not include them in the final binary.
#[cfg(target_os = "windows")]
mod _force_ffi_link {
    #[used]
    static _LINK_HAS_INTERNAL_DRAG: extern "C" fn() -> i32 =
        super::drag_drop::windows::tiddlydesktop_has_internal_drag;
    #[used]
    static _LINK_IS_TEXT_SELECTION_DRAG: extern "C" fn() -> i32 =
        super::drag_drop::windows::tiddlydesktop_is_text_selection_drag;
    #[used]
    static _LINK_IS_TIDDLER_DRAG: extern "C" fn() -> i32 =
        super::drag_drop::windows::tiddlydesktop_is_tiddler_drag;
    #[used]
    static _LINK_CLEAR_INTERNAL_DRAG: extern "C" fn() =
        super::drag_drop::windows::tiddlydesktop_clear_internal_drag;
    #[used]
    static _LINK_IS_OVER_DROPPABLE: extern "C" fn() -> i32 =
        super::drag_drop::windows::tiddlydesktop_is_over_droppable;
}

/// Inter-process communication for multi-process wiki architecture
/// (Desktop only - not used on Android which doesn't support multi-process wikis)
#[cfg_attr(target_os = "android", allow(dead_code))]
mod ipc;

/// JavaScript initialization scripts for wiki windows
mod init_script;

/// Core data types
mod types;
pub use types::{WikiEntry, ExternalAttachmentsConfig, AuthUrlEntry, SessionAuthConfig, WikiConfigs, EditionInfo, PluginInfo, FolderStatus};

/// Clipboard operations
mod clipboard;

/// Utility functions
mod utils;

#[cfg(not(target_os = "android"))]
mod media_server;

/// Chrome-like user agent for Linux WebKitGTK.
/// WebKitGTK's default UA looks like Safari-on-Linux which triggers bot detection
/// on services like YouTube (Error 153) and SoundCloud (CAPTCHA). Real Safari
/// only runs on macOS/iOS, so the Linux+Safari combo is flagged as suspicious.
#[cfg(target_os = "linux")]
const LINUX_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// Wiki storage and recent files management
mod wiki_storage;

/// TiddlyWiki HTML manipulation
#[cfg_attr(target_os = "android", allow(dead_code))]
mod tiddlywiki_html;

/// Cross-platform file system abstraction (desktop: std::fs, Android: SAF)
mod fs_abstraction;

/// Android-specific implementations (SAF, permissions, etc.)
#[cfg(target_os = "android")]
mod android;

/// PDFium-based PDF rendering (replaces PDF.js)
mod pdf_renderer;

/// LAN Sync: real-time tiddler synchronization across devices on the same network
#[allow(dead_code)]
mod lan_sync;

/// Relay Sync: cloud relay transport for cross-network sync
#[allow(dead_code)]
mod relay_sync;

/// Helper trait to conditionally add platform-specific plugins to the Tauri builder.
/// On Android, this adds the Android FS plugin for SAF support.
trait BuilderExt<R: tauri::Runtime> {
    fn with_platform_plugins(self) -> Self;
}

impl<R: tauri::Runtime> BuilderExt<R> for tauri::Builder<R> {
    #[cfg(target_os = "android")]
    fn with_platform_plugins(self) -> Self {
        self.plugin(tauri_plugin_android_fs::init())
    }

    #[cfg(not(target_os = "android"))]
    fn with_platform_plugins(self) -> Self {
        self // No additional plugins needed on desktop
    }
}

use chrono::Local;
use tauri::{
    image::Image,
    http::{Request, Response},
    Emitter, Manager, WebviewUrl, WebviewWindowBuilder,
};

// Menu and tray are only available on desktop platforms
#[cfg(not(target_os = "android"))]
use tauri::{
    menu::{MenuBuilder, MenuItemBuilder},
    tray::TrayIconBuilder,
};
#[cfg(target_os = "macos")]
use tauri::menu::{Menu, PredefinedMenuItem, Submenu};

/// Helper trait to apply desktop-only window properties (title, size)
/// On Android, these methods don't exist on WebviewWindowBuilder
#[allow(dead_code)]
trait DesktopWindowExt<'a> {
    fn with_title_if_desktop(self, title: &str) -> Self;
    fn with_inner_size_if_desktop(self, width: f64, height: f64) -> Self;
}

#[cfg(not(target_os = "android"))]
impl<'a> DesktopWindowExt<'a> for WebviewWindowBuilder<'a, tauri::Wry, tauri::AppHandle<tauri::Wry>> {
    fn with_title_if_desktop(self, title: &str) -> Self {
        self.title(title)
    }
    fn with_inner_size_if_desktop(self, width: f64, height: f64) -> Self {
        self.inner_size(width, height)
    }
}

#[cfg(target_os = "android")]
impl<'a> DesktopWindowExt<'a> for WebviewWindowBuilder<'a, tauri::Wry, tauri::AppHandle<tauri::Wry>> {
    fn with_title_if_desktop(self, _title: &str) -> Self {
        self // Android windows are fullscreen, no title
    }
    fn with_inner_size_if_desktop(self, _width: f64, _height: f64) -> Self {
        self // Android windows are fullscreen, no size
    }
}

/// Determine storage mode — delegates to resolve_data_dir and caches in DATA_DIR.
/// Supports portable mode on all desktop platforms (Windows, Linux, macOS):
/// place a `portable` or `portable.txt` marker file next to the executable.
fn determine_storage_mode(app: &tauri::App) -> Result<PathBuf, String> {
    let dir = resolve_data_dir(app.handle())?;
    let _ = DATA_DIR.set(dir.clone());
    Ok(dir)
}

/// Get the user editions directory path
/// Respects portable mode — editions stored next to exe in portable installs
fn get_user_editions_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let data_dir = get_data_dir(app)?;
    Ok(data_dir.join("editions"))
}

/// A running wiki child process (separate process per wiki)
/// Fields stored for potential future use (process management, cleanup)
#[allow(dead_code)]
struct WikiProcess {
    pid: u32,
    path: String,
}

/// App state
struct AppState {
    /// Mapping of encoded paths to actual file paths
    wiki_paths: Mutex<HashMap<String, PathBuf>>,
    /// Mapping of window labels to wiki paths (for duplicate detection in same-process mode)
    open_wikis: Mutex<HashMap<String, String>>,
    /// Running wiki child processes (keyed by wiki path for duplicate detection)
    wiki_processes: Mutex<HashMap<String, WikiProcess>>,
    /// Next available port for wiki folder servers
    next_port: Mutex<u16>,
    /// Path to the main wiki file (tiddlydesktop.html)
    main_wiki_path: PathBuf,
    /// Mapping of cached folder wiki paths to original folder paths (Android)
    #[allow(dead_code)]
    folder_wiki_paths: Mutex<HashMap<String, String>>,
    /// Mapping of SAF URIs to local mirror paths (Android) for sync on close
    #[allow(dead_code)]
    saf_wiki_mappings: Mutex<HashMap<String, String>>,
}

/// Get the bundled index.html content
/// On desktop, returns the path; on Android, extracts from assets
#[cfg(not(target_os = "android"))]
fn get_bundled_index_path(app: &tauri::App) -> Result<PathBuf, String> {
    // Use our helper that prefers exe-relative paths (avoids baked-in CI paths)
    let resource_path = get_resource_dir_path(app.handle())
        .ok_or_else(|| "Failed to get resource directory".to_string())?;
    let resource_path = utils::normalize_path(resource_path);

    let possible_sources = [
        resource_path.join("resources").join("index.html"),
        resource_path.join("index.html"),
    ];

    for source in &possible_sources {
        if source.exists() {
            return Ok(source.clone());
        }
    }

    // Development fallback (cargo runs from src-tauri directory)
    let dev_sources = [
        PathBuf::from("../src/index.html"),
        PathBuf::from("src/index.html"),
    ];
    for dev_source in &dev_sources {
        if dev_source.exists() {
            return Ok(dev_source.clone());
        }
    }

    Err(format!("Could not find source index.html. Tried: {:?}", possible_sources))
}

/// Get the bundled index.html content on Android
/// Returns the content as bytes since we can't use filesystem paths for APK assets
#[cfg(target_os = "android")]
fn get_bundled_index_content(app: &tauri::App) -> Result<Vec<u8>, String> {
    use tauri::Manager;

    // On Android, assets are bundled in the APK and accessed via asset resolver
    let resolver = app.asset_resolver();

    // Try to get index.html from the frontend dist
    if let Some(asset) = resolver.get("index.html".into()) {
        return Ok(asset.bytes.to_vec());
    }

    Err("Could not find bundled index.html in Android assets".to_string())
}

/// Get content of any bundled asset on Android
/// The path should be relative to the bundle root (e.g., "resources/tiddlywiki/boot/boot.js")
#[cfg(target_os = "android")]
pub fn get_bundled_asset_content(app: &tauri::AppHandle, path: &str) -> Result<Vec<u8>, String> {
    use tauri::Manager;

    let resolver = app.asset_resolver();

    if let Some(asset) = resolver.get(path.into()) {
        return Ok(asset.bytes.to_vec());
    }

    Err(format!("Could not find bundled asset: {}", path))
}

/// Get content of any bundled asset as string on Android
#[cfg(target_os = "android")]
pub fn get_bundled_asset_string(app: &tauri::AppHandle, path: &str) -> Result<String, String> {
    let bytes = get_bundled_asset_content(app, path)?;
    String::from_utf8(bytes).map_err(|e| format!("Invalid UTF-8 in {}: {}", path, e))
}

/// Extract all tiddlywiki resources from ZIP to app data directory
/// This is called once on first Android launch to make resources available via filesystem
#[cfg(target_os = "android")]
pub fn extract_tiddlywiki_resources(app: &tauri::App) -> Result<PathBuf, String> {
    use std::io::Read;
    use tauri::Manager;

    let data_dir = app.path().app_data_dir()
        .map_err(|e| format!("Failed to get app data dir: {}", e))?;

    // The tiddlywiki resources will be extracted to <app_data>/tiddlywiki/
    // Note: Node binary is in jniLibs (not in the ZIP)
    let tw_dir = data_dir.join("tiddlywiki");

    // Check if already extracted by looking for a marker file
    let marker_file = tw_dir.join(".extracted");
    if marker_file.exists() {
        eprintln!("[TiddlyDesktop] TiddlyWiki resources already extracted to {:?}", tw_dir);
        return Ok(data_dir);
    }

    eprintln!("[TiddlyDesktop] Extracting TiddlyWiki resources to {:?}...", data_dir);

    // Use the embedded ZIP (included at compile time via include_bytes!)
    let cursor = std::io::Cursor::new(TIDDLYWIKI_ZIP);
    let mut archive = zip::ZipArchive::new(cursor)
        .map_err(|e| format!("Failed to open ZIP: {}", e))?;

    let total_files = archive.len();
    eprintln!("[TiddlyDesktop] ZIP contains {} files ({} bytes)", total_files, TIDDLYWIKI_ZIP.len());

    let mut extracted_count = 0;
    let mut failed_count = 0;

    for i in 0..total_files {
        let mut file = match archive.by_index(i) {
            Ok(f) => f,
            Err(_) => {
                failed_count += 1;
                continue;
            }
        };

        // ZIP entries have paths like "tiddlywiki/..."
        // Extract directly to data_dir
        let outpath = data_dir.join(file.name());

        if file.is_dir() {
            let _ = std::fs::create_dir_all(&outpath);
        } else {
            if let Some(parent) = outpath.parent() {
                let _ = std::fs::create_dir_all(parent);
            }

            let mut contents = Vec::new();
            if file.read_to_end(&mut contents).is_ok() {
                if std::fs::write(&outpath, &contents).is_ok() {
                    extracted_count += 1;
                } else {
                    failed_count += 1;
                }
            } else {
                failed_count += 1;
            }
        }

        // Log progress every 1000 files
        let current = extracted_count + failed_count;
        if current % 1000 == 0 {
            eprintln!("[TiddlyDesktop] Extraction progress: {}/{} files", current, total_files);
        }
    }

    eprintln!("[TiddlyDesktop] Extracted {} files ({} failed)", extracted_count, failed_count);

    // Write marker file with version to track app updates
    // Format: "version:X.Y.Z" - allows needs_resource_extraction to detect version changes
    let current_version = env!("CARGO_PKG_VERSION");
    std::fs::write(&marker_file, format!("version:{}", current_version))
        .map_err(|e| format!("Failed to write marker file: {}", e))?;

    eprintln!("[TiddlyDesktop] Marker file written with version {}", current_version);

    Ok(data_dir)
}

/// Get the path to extracted tiddlywiki resources on Android
#[cfg(target_os = "android")]
pub fn get_extracted_resources_dir(app: &tauri::AppHandle) -> Option<PathBuf> {
    app.path().app_data_dir().ok()
}

/// Check if TiddlyWiki resources need to be extracted (first run or app update)
/// Returns true if:
/// - Marker file doesn't exist (first run)
/// - Marker file exists but version doesn't match current app version (update)
#[cfg(target_os = "android")]
pub fn needs_resource_extraction(app: &tauri::App) -> bool {
    use tauri::Manager;
    let current_version = env!("CARGO_PKG_VERSION");

    if let Ok(data_dir) = app.path().app_data_dir() {
        let marker_file = data_dir.join("tiddlywiki").join(".extracted");
        if !marker_file.exists() {
            return true; // First run
        }

        // Check if version matches
        if let Ok(marker_content) = std::fs::read_to_string(&marker_file) {
            // Marker format: "version:X.Y.Z"
            if let Some(version) = marker_content.strip_prefix("version:") {
                let stored_version = version.trim();
                if stored_version != current_version {
                    eprintln!("[TiddlyDesktop] App updated from {} to {}, re-extracting resources", stored_version, current_version);
                    // Delete old tiddlywiki folder to ensure clean extraction
                    let tw_dir = data_dir.join("tiddlywiki");
                    if let Err(e) = std::fs::remove_dir_all(&tw_dir) {
                        eprintln!("[TiddlyDesktop] Warning: Failed to remove old tiddlywiki dir: {}", e);
                    }
                    // Also delete old node-bin folder
                    let node_dir = data_dir.join("node-bin");
                    if let Err(e) = std::fs::remove_dir_all(&node_dir) {
                        eprintln!("[TiddlyDesktop] Warning: Failed to remove old node-bin dir: {}", e);
                    }
                    return true;
                }
                return false; // Version matches, no extraction needed
            }
        }

        // Marker file exists but has old format - re-extract
        true
    } else {
        true // If we can't determine, assume extraction is needed
    }
}

/// Ensure main wiki file exists, extracting from resources if needed
/// Also handles migration when bundled version is newer than existing
#[cfg(not(target_os = "android"))]
fn ensure_main_wiki_exists(app: &tauri::App) -> Result<PathBuf, String> {
    let wiki_dir = determine_storage_mode(app)?;
    std::fs::create_dir_all(&wiki_dir).map_err(|e| format!("Failed to create wiki dir: {}", e))?;

    let main_wiki_path = wiki_dir.join("tiddlydesktop.html");
    let bundled_path = get_bundled_index_path(app)?;

    if !main_wiki_path.exists() {
        // First run: copy from bundled resources
        std::fs::copy(&bundled_path, &main_wiki_path)
            .map_err(|e| format!("Failed to copy wiki: {}", e))?;
        println!("Created main wiki from {:?}", bundled_path);
    } else {
        // Check if we need to migrate to a newer version
        let existing_html = std::fs::read_to_string(&main_wiki_path)
            .map_err(|e| format!("Failed to read existing wiki: {}", e))?;
        let bundled_html = std::fs::read_to_string(&bundled_path)
            .map_err(|e| format!("Failed to read bundled wiki: {}", e))?;

        let existing_version = tiddlywiki_html::extract_tiddler_from_html(&existing_html, "$:/TiddlyDesktop/AppVersion")
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(0);
        let bundled_version = tiddlywiki_html::extract_tiddler_from_html(&bundled_html, "$:/TiddlyDesktop/AppVersion")
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(0);

        if bundled_version > existing_version {
            println!("Migrating to newer version...");

            // Extract user data from existing wiki
            let wiki_list = tiddlywiki_html::extract_tiddler_from_html(&existing_html, "$:/TiddlyDesktop/WikiList");

            // Start with bundled HTML
            let mut new_html = bundled_html;

            // Inject user data into new HTML
            if let Some(list) = wiki_list {
                println!("Preserving wiki list during migration");
                new_html = tiddlywiki_html::inject_tiddler_into_html(&new_html, "$:/TiddlyDesktop/WikiList", "application/json", &list);
            }

            // Write the migrated wiki
            std::fs::write(&main_wiki_path, new_html)
                .map_err(|e| format!("Failed to write migrated wiki: {}", e))?;
            println!("Migration complete");
        }
    }

    Ok(main_wiki_path)
}

/// Android version: Extract bundled index.html from APK assets to app data
#[cfg(target_os = "android")]
fn ensure_main_wiki_exists(app: &tauri::App) -> Result<PathBuf, String> {
    let wiki_dir = determine_storage_mode(app)?;
    std::fs::create_dir_all(&wiki_dir).map_err(|e| format!("Failed to create wiki dir: {}", e))?;

    let main_wiki_path = wiki_dir.join("tiddlydesktop.html");
    let bundled_content = get_bundled_index_content(app)?;
    let bundled_html = String::from_utf8(bundled_content.clone())
        .map_err(|e| format!("Invalid UTF-8 in bundled index.html: {}", e))?;

    if !main_wiki_path.exists() {
        // First run: write bundled content
        std::fs::write(&main_wiki_path, &bundled_content)
            .map_err(|e| format!("Failed to write wiki: {}", e))?;
        println!("Created main wiki from bundled assets");
    } else {
        // Check if we need to migrate to a newer version
        let existing_html = std::fs::read_to_string(&main_wiki_path)
            .map_err(|e| format!("Failed to read existing wiki: {}", e))?;

        let existing_version = tiddlywiki_html::extract_tiddler_from_html(&existing_html, "$:/TiddlyDesktop/AppVersion")
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(0);
        let bundled_version = tiddlywiki_html::extract_tiddler_from_html(&bundled_html, "$:/TiddlyDesktop/AppVersion")
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(0);

        if bundled_version > existing_version {
            println!("Migrating to newer version...");

            // Extract user data from existing wiki
            let wiki_list = tiddlywiki_html::extract_tiddler_from_html(&existing_html, "$:/TiddlyDesktop/WikiList");

            // Start with bundled HTML
            let mut new_html = bundled_html;

            // Inject user data into new HTML
            if let Some(list) = wiki_list {
                println!("Preserving wiki list during migration");
                new_html = tiddlywiki_html::inject_tiddler_into_html(&new_html, "$:/TiddlyDesktop/WikiList", "application/json", &list);
            }

            // Write the migrated wiki
            std::fs::write(&main_wiki_path, new_html)
                .map_err(|e| format!("Failed to write migrated wiki: {}", e))?;
            println!("Migration complete");
        }
    }

    Ok(main_wiki_path)
}

/// Create a backup of the wiki file before saving
/// If custom_backup_dir is Some, backups go there; otherwise to .backups folder next to wiki
/// backup_count: None = default 20, Some(0) = unlimited, Some(n) = keep n backups
async fn create_backup(path: &PathBuf, custom_backup_dir: Option<&str>, backup_count: Option<u32>) -> Result<(), String> {
    if !path.exists() {
        return Ok(()); // No backup needed for new files
    }

    let parent = path.parent().ok_or("No parent directory")?;
    let filename = path.file_stem().and_then(|s| s.to_str()).unwrap_or("wiki");

    // Determine backup directory
    let backup_dir = if let Some(custom_dir) = custom_backup_dir {
        PathBuf::from(custom_dir)
    } else {
        // Default: .backups folder next to the wiki
        parent.join(format!("{}.backups", filename))
    };

    tokio::fs::create_dir_all(&backup_dir)
        .await
        .map_err(|e| format!("Failed to create backup dir: {}", e))?;

    // Create timestamped backup filename
    let timestamp = Local::now().format("%Y%m%d-%H%M%S");
    let backup_name = format!("{}.{}.html", filename, timestamp);
    let backup_path = backup_dir.join(backup_name);

    // Copy current file to backup
    tokio::fs::copy(path, &backup_path)
        .await
        .map_err(|e| format!("Failed to create backup: {}", e))?;

    // Clean up old backups (default: keep last 20, 0 = unlimited)
    let keep = backup_count.unwrap_or(20);
    if keep > 0 {
        cleanup_old_backups(&backup_dir, keep as usize).await;
    }

    Ok(())
}

/// Remove old backups, keeping only the most recent ones
async fn cleanup_old_backups(backup_dir: &PathBuf, keep: usize) {
    if keep == 0 {
        return; // 0 means unlimited, don't delete anything
    }

    if let Ok(mut entries) = tokio::fs::read_dir(backup_dir).await {
        let mut backups: Vec<PathBuf> = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().map(|e| e == "html").unwrap_or(false) {
                backups.push(path);
            }
        }

        // Sort by name (which includes timestamp) descending
        backups.sort();
        backups.reverse();

        // Remove old backups
        for old_backup in backups.into_iter().skip(keep) {
            let _ = tokio::fs::remove_file(old_backup).await;
        }
    }
}

/// Load wiki content from disk
#[tauri::command]
async fn load_wiki(_app: tauri::AppHandle, path: String) -> Result<String, String> {
    // Android: Handle content:// URIs via SAF
    #[cfg(target_os = "android")]
    if path.starts_with("content://") || path.starts_with("{") {
        return android::saf::read_document_string(&path);
    }

    // Desktop: Validate and read from filesystem
    let validated_path = drag_drop::sanitize::validate_wiki_path(&path)?;

    tokio::fs::read_to_string(&validated_path)
        .await
        .map_err(|e| format!("Failed to read wiki: {}", e))
}

/// Save wiki content to disk with backup
#[tauri::command]
async fn save_wiki(app: tauri::AppHandle, path: String, content: String) -> Result<(), String> {
    // Android: Handle content:// URIs via SAF
    #[cfg(target_os = "android")]
    if path.starts_with("content://") || path.starts_with("{") {
        // Check if backups are enabled for this wiki
        if should_create_backup_android(&app, &path) {
            let custom_backup_dir = get_wiki_backup_dir(&app, &path);

            if let Ok(Some(backup_dir_uri)) = android::saf::get_backup_directory(&path, custom_backup_dir.as_deref()) {
                // Get filename stem for backup naming
                let filename_stem = android::saf::get_display_name(&path)
                    .map(|name| {
                        name.strip_suffix(".html")
                            .or_else(|| name.strip_suffix(".htm"))
                            .unwrap_or(&name)
                            .to_string()
                    })
                    .unwrap_or_else(|_| "wiki".to_string());

                // Create backup
                if let Err(e) = android::saf::create_backup(&path, &backup_dir_uri, &filename_stem) {
                    eprintln!("[TiddlyDesktop] Failed to create Android backup: {}", e);
                } else {
                    // Clean up old backups
                    let backup_count = wiki_storage::get_wiki_backup_count(&app, &path).unwrap_or(20);
                    let _ = android::saf::cleanup_old_backups(&backup_dir_uri, &filename_stem, backup_count as usize);
                }
            } else {
                eprintln!("[TiddlyDesktop] Android backup skipped: no backup directory available");
            }
        }

        return android::saf::write_document_string(&path, &content);
    }

    // Desktop: Validate and write to filesystem
    let validated_path = drag_drop::sanitize::validate_wiki_path_for_write(&path)?;

    // Check if backups are enabled for this wiki
    let state = app.state::<AppState>();
    if should_create_backup(&app, &state, &path) {
        let backup_dir = get_wiki_backup_dir(&app, &path);
        let backup_count = wiki_storage::get_wiki_backup_count(&app, &path);
        match create_backup(&validated_path, backup_dir.as_deref(), backup_count).await {
            Ok(()) => {},
            Err(e) => {
                // Log but don't block the save — backup failure should not prevent saving
                eprintln!("[TiddlyDesktop] Backup failed (non-fatal): {}", e);
            }
        }
    }

    // Write to a temp file first, then rename for atomic operation
    let temp_path = validated_path.with_extension("tmp");

    tokio::fs::write(&temp_path, &content)
        .await
        .map_err(|e| format!("Failed to write temp file: {}", e))?;

    // Try rename first, fall back to direct write if it fails (Windows file locking)
    if let Err(_) = tokio::fs::rename(&temp_path, &validated_path).await {
        let _ = tokio::fs::remove_file(&temp_path).await;
        tokio::fs::write(&validated_path, &content)
            .await
            .map_err(|e| format!("Failed to save file: {}", e))?;
    }

    Ok(())
}

/// Set window title
/// On Linux, navigates the HeaderBar widget tree to find and update the title label
#[tauri::command]
async fn set_window_title(app: tauri::AppHandle, label: String, title: String) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(&label) {
        #[cfg(target_os = "linux")]
        {
            use gtk::prelude::{BinExt, GtkWindowExt, HeaderBarExt, LabelExt};
            use gtk::glib::Cast;

            let mut updated_header = false;
            if let Ok(gtk_window) = window.gtk_window() {
                // Navigate: GtkWindow → HeaderBar → EventBox → Overlay → Label
                if let Some(titlebar) = gtk_window.titlebar() {
                    if let Some(header_bar) = titlebar.downcast_ref::<gtk::HeaderBar>() {
                        if let Some(custom_title) = header_bar.custom_title() {
                            if let Some(event_box) = custom_title.downcast_ref::<gtk::EventBox>() {
                                if let Some(overlay) = event_box.child() {
                                    if let Some(overlay) = overlay.downcast_ref::<gtk::Overlay>() {
                                        if let Some(label) = overlay.child() {
                                            if let Some(title_label) = label.downcast_ref::<gtk::Label>() {
                                                title_label.set_text(&title);
                                                updated_header = true;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // Fallback: set WM title (used on X11 with server-side decorations)
            if !updated_header {
                let _ = window.set_title(&title);
            }
        }

        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        {
            window.set_title(&title).map_err(|e| e.to_string())?;
        }

        #[cfg(target_os = "android")]
        {
            // Android doesn't support setting window titles
            let _ = (window, &title);
        }
    }
    Ok(())
}

/// Set headerbar colors on Linux
/// Applies custom CSS to style the HeaderBar background and title color
#[tauri::command]
async fn set_headerbar_colors(
    app: tauri::AppHandle,
    label: String,
    background: String,
    foreground: String,
) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        eprintln!("[TiddlyDesktop] set_headerbar_colors called: label={}, bg={}, fg={}", label, background, foreground);

        // Schedule GTK operations on the main thread via Tauri
        let app_clone = app.clone();
        let _ = app.run_on_main_thread(move || {
            use gtk::prelude::{CssProviderExt, GtkWindowExt, WidgetExt};
            use gtk::glib::Cast;

            eprintln!("[TiddlyDesktop] HeaderBar: on main thread, looking for window '{}'", label);

            if let Some(window) = app_clone.get_webview_window(&label) {
                eprintln!("[TiddlyDesktop] HeaderBar: found webview window");
                if let Ok(gtk_window) = window.gtk_window() {
                    eprintln!("[TiddlyDesktop] HeaderBar: got gtk_window");
                    if let Some(titlebar) = gtk_window.titlebar() {
                        eprintln!("[TiddlyDesktop] HeaderBar: got titlebar");
                        if let Some(header_bar) = titlebar.downcast_ref::<gtk::HeaderBar>() {
                            eprintln!("[TiddlyDesktop] HeaderBar: got header_bar, applying CSS");

                            // Use widget name for specific targeting
                            header_bar.set_widget_name("td-headerbar");

                            let css = format!(
                                r#"
                                #td-headerbar {{
                                    background: {};
                                    background-image: none;
                                    box-shadow: none;
                                    border: none;
                                }}
                                #td-headerbar * {{
                                    color: {};
                                }}
                                #td-headerbar .title {{
                                    color: {};
                                }}
                                #td-headerbar button.titlebutton {{
                                    border-radius: 50%;
                                    min-width: 24px;
                                    min-height: 24px;
                                    padding: 4px;
                                }}
                                #td-headerbar button.titlebutton:hover {{
                                    background-color: alpha({}, 0.15);
                                }}
                                "#,
                                background, foreground, foreground, foreground
                            );

                            let css_provider = gtk::CssProvider::new();
                            if let Err(e) = css_provider.load_from_data(css.as_bytes()) {
                                eprintln!("[TiddlyDesktop] Failed to load headerbar CSS: {}", e);
                                return;
                            }

                            // Add to the default screen so it applies globally
                            if let Some(screen) = gtk::gdk::Screen::default() {
                                gtk::StyleContext::add_provider_for_screen(
                                    &screen,
                                    &css_provider,
                                    gtk::STYLE_PROVIDER_PRIORITY_USER,
                                );
                                eprintln!("[TiddlyDesktop] HeaderBar: CSS applied to screen successfully");
                            } else {
                                eprintln!("[TiddlyDesktop] HeaderBar: no default screen found");
                            }
                        } else {
                            eprintln!("[TiddlyDesktop] HeaderBar: titlebar is not a HeaderBar");
                        }
                    } else {
                        eprintln!("[TiddlyDesktop] HeaderBar: no titlebar found");
                    }
                } else {
                    eprintln!("[TiddlyDesktop] HeaderBar: failed to get gtk_window");
                }
            } else {
                eprintln!("[TiddlyDesktop] HeaderBar: window '{}' not found", label);
            }
        });
    }

    #[cfg(not(target_os = "linux"))]
    {
        // Headerbar colors only apply to Linux GTK headerbar
        let _ = (app, label, background, foreground);
    }

    Ok(())
}

/// Maximum size for favicon data URIs (1MB encoded, ~750KB decoded)
/// This prevents memory exhaustion from maliciously large favicons
const MAX_FAVICON_DATA_URI_SIZE: usize = 1024 * 1024;

/// Internal helper to set window icon from favicon data URI
/// Used by both the Tauri command and window creation code
fn set_window_icon_internal(
    app: &tauri::AppHandle,
    label: &str,
    favicon_data_uri: Option<&str>,
) -> Result<(), String> {
    use base64::{engine::general_purpose::STANDARD, Engine};

    let window = app.get_webview_window(label)
        .ok_or_else(|| format!("Window not found: {}", label))?;

    // Decode the favicon bytes (used for both window icon and headerbar on Linux)
    let favicon_bytes = match favicon_data_uri {
        Some(data_uri) => {
            // Security: Check data URI size to prevent memory exhaustion
            if data_uri.len() > MAX_FAVICON_DATA_URI_SIZE {
                return Err(format!(
                    "Favicon data URI too large ({} bytes, max {} bytes)",
                    data_uri.len(),
                    MAX_FAVICON_DATA_URI_SIZE
                ));
            }

            // Parse data URI: "data:image/png;base64,iVBOR..."
            let base64_data = data_uri
                .split(',')
                .nth(1)
                .ok_or("Invalid data URI format")?;

            Some(STANDARD
                .decode(base64_data)
                .map_err(|e| format!("Base64 decode error: {}", e))?)
        }
        None => None,
    };

    // Set the window/taskbar icon
    let icon = match &favicon_bytes {
        Some(bytes) => {
            Image::from_bytes(bytes)
                .map_err(|e| format!("Image decode error: {}", e))?
        }
        None => {
            // Fallback to default app icon
            Image::from_bytes(include_bytes!("../icons/icon.png"))
                .map_err(|e| format!("Failed to load default icon: {}", e))?
        }
    };

    // set_icon is not available on Android
    #[cfg(not(target_os = "android"))]
    window.set_icon(icon)
        .map_err(|e| format!("Failed to set icon: {}", e))?;

    #[cfg(target_os = "android")]
    {
        let _ = icon; // Silence unused warning
        let _ = window; // Window APIs are desktop-only
    }

    // Linux: Also update the headerbar favicon icon
    #[cfg(target_os = "linux")]
    {
        use gtk::prelude::{BinExt, Cast, ContainerExt, GtkWindowExt, HeaderBarExt, ImageExt, WidgetExt};
        use gtk::gdk_pixbuf::Pixbuf;
        use gtk::gio::MemoryInputStream;
        use gtk::glib::Bytes;

        if let Ok(gtk_window) = window.gtk_window() {
            // Navigate: GtkWindow → HeaderBar → EventBox → Overlay → find Image by name
            if let Some(titlebar) = gtk_window.titlebar() {
                if let Some(header_bar) = titlebar.downcast_ref::<gtk::HeaderBar>() {
                    if let Some(custom_title) = header_bar.custom_title() {
                        if let Some(event_box) = custom_title.downcast_ref::<gtk::EventBox>() {
                            if let Some(overlay_widget) = event_box.child() {
                                if let Some(overlay) = overlay_widget.downcast_ref::<gtk::Overlay>() {
                                    // Find the favicon Image widget by name
                                    for child in overlay.children() {
                                        if child.widget_name() == "headerbar-favicon" {
                                            if let Some(image) = child.downcast_ref::<gtk::Image>() {
                                                match &favicon_bytes {
                                                    Some(bytes) => {
                                                        // Load at full resolution, then scale with highest-quality interpolation
                                                        let glib_bytes = Bytes::from(bytes);
                                                        let stream = MemoryInputStream::from_bytes(&glib_bytes);
                                                        if let Ok(full_pixbuf) = Pixbuf::from_stream(&stream, gtk::gio::Cancellable::NONE) {
                                                            // Scale to 20x20, using Hyper for sharpest result
                                                            let scaled = full_pixbuf.scale_simple(20, 20, gtk::gdk_pixbuf::InterpType::Hyper);
                                                            if let Some(pixbuf) = scaled {
                                                                image.set_from_pixbuf(Some(&pixbuf));
                                                                image.set_visible(true);
                                                            }
                                                        }
                                                    }
                                                    None => {
                                                        // Hide the favicon icon when reset to default
                                                        image.set_visible(false);
                                                    }
                                                }
                                            }
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// Set window icon from favicon data URI
/// Call with None to reset to default app icon
#[tauri::command]
fn set_window_icon(
    app: tauri::AppHandle,
    label: String,
    favicon_data_uri: Option<String>,
) -> Result<(), String> {
    set_window_icon_internal(&app, &label, favicon_data_uri.as_deref())
}

/// Get current window label
#[tauri::command]
fn get_window_label(window: tauri::Window) -> String {
    window.label().to_string()
}

/// Check if backups should be created for a wiki path
/// Checks both if it's the main wiki (always no backup) and the user's backups_enabled setting
fn should_create_backup(app: &tauri::AppHandle, state: &AppState, path: &str) -> bool {
    // Don't backup the main TiddlyDesktop wiki
    // Use canonicalized paths for robust comparison (handles symlinks, relative paths, etc.)
    let path_buf = PathBuf::from(path);
    let main_wiki = &state.main_wiki_path;
    // Empty main_wiki_path means we're in a wiki child process — skip the main wiki check
    if main_wiki.as_os_str().len() > 0 {
        if let (Ok(canonical_path), Ok(canonical_main)) = (
            dunce::canonicalize(&path_buf),
            dunce::canonicalize(main_wiki)
        ) {
            if canonical_path == canonical_main {
                return false;
            }
        } else {
            // Fallback to string comparison if canonicalization fails
            let main_wiki_str = main_wiki.to_string_lossy();
            if utils::paths_equal(path, &main_wiki_str) {
                return false;
            }
        }
    }
    // Check if backups are enabled for this wiki in the recent files list
    let entries = wiki_storage::load_recent_files_from_disk(app);
    for entry in entries {
        if utils::paths_equal(&entry.path, path) {
            return entry.backups_enabled;
        }
    }
    // Default to enabled for wikis not in the list
    true
}

/// Check if backups should be created for an Android SAF wiki
/// Simplified version that doesn't try to canonicalize paths (which doesn't work for content:// URIs)
#[cfg(target_os = "android")]
fn should_create_backup_android(app: &tauri::AppHandle, path: &str) -> bool {
    // Check if backups are enabled for this wiki in the recent files list
    let entries = wiki_storage::load_recent_files_from_disk(app);
    for entry in entries {
        if utils::paths_equal(&entry.path, path) {
            return entry.backups_enabled;
        }
    }
    // Default to enabled for wikis not in the list
    true
}

/// Get custom backup directory for a wiki path (if set)
fn get_wiki_backup_dir(app: &tauri::AppHandle, path: &str) -> Option<String> {
    let entries = wiki_storage::load_recent_files_from_disk(app);
    for entry in entries {
        if utils::paths_equal(&entry.path, path) {
            return entry.backup_dir.clone();
        }
    }
    None
}

/// Get path to main wiki file
#[tauri::command]
fn get_main_wiki_path(state: tauri::State<AppState>) -> String {
    state.main_wiki_path.to_string_lossy().to_string()
}

/// Show an alert dialog
#[tauri::command]
async fn show_alert(app: tauri::AppHandle, message: String) -> Result<(), String> {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
    app.dialog()
        .message(message)
        .kind(MessageDialogKind::Info)
        .title("TiddlyWiki")
        .buttons(MessageDialogButtons::Ok)
        .blocking_show();
    Ok(())
}

/// Show a confirm dialog
#[tauri::command]
async fn show_confirm(app: tauri::AppHandle, message: String) -> Result<bool, String> {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
    let result = app.dialog()
        .message(message)
        .kind(MessageDialogKind::Warning)
        .title("TiddlyWiki")
        .buttons(MessageDialogButtons::OkCancel)
        .blocking_show();
    Ok(result)
}

/// Close the current window (used after confirming unsaved changes)
#[tauri::command]
fn close_window(window: tauri::Window) {
    #[cfg(not(target_os = "android"))]
    let _ = window.destroy();
    #[cfg(target_os = "android")]
    {
        // Android doesn't have direct window control - just drop the reference
        drop(window);
    }
}

/// Close a window by its label (used by tm-close-window)
#[tauri::command]
fn close_window_by_label(app: tauri::AppHandle, label: String) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(&label) {
        #[cfg(not(target_os = "android"))]
        window.destroy().map_err(|e| e.to_string())?;
        #[cfg(target_os = "android")]
        {
            // Android doesn't have direct window control - just drop the reference
            drop(window);
        }
        Ok(())
    } else {
        Err(format!("Window '{}' not found", label))
    }
}

/// Toggle fullscreen mode for the current window (used by tm-full-screen)
#[tauri::command]
fn toggle_fullscreen(window: tauri::WebviewWindow) -> Result<bool, String> {
    #[cfg(not(target_os = "android"))]
    {
        let is_fullscreen = window.is_fullscreen().map_err(|e| e.to_string())?;
        window
            .set_fullscreen(!is_fullscreen)
            .map_err(|e| e.to_string())?;
        Ok(!is_fullscreen)
    }
    #[cfg(target_os = "android")]
    {
        // Android is always fullscreen
        let _ = window;
        Ok(true)
    }
}

/// Set the zoom level for the current window (1.0 = 100%)
#[tauri::command]
fn set_zoom_level(window: tauri::WebviewWindow, level: f64) -> Result<(), String> {
    #[cfg(not(target_os = "android"))]
    {
        let clamped = level.max(0.2).min(5.0);
        window.set_zoom(clamped).map_err(|e| e.to_string())
    }
    #[cfg(target_os = "android")]
    {
        let _ = (window, level);
        Err("Zoom not available on Android".to_string())
    }
}

/// Print the current page (used by tm-print)
#[tauri::command]
fn print_page(window: tauri::WebviewWindow) -> Result<(), String> {
    #[cfg(not(target_os = "android"))]
    {
        window.print().map_err(|e| e.to_string())
    }
    #[cfg(target_os = "android")]
    {
        // Print not available on Android
        let _ = window;
        Err("Print not available on Android".to_string())
    }
}

/// Show a save file dialog and write content to the selected file (used by tm-download-file)
#[tauri::command]
#[allow(unused_variables)]
async fn download_file(
    app: tauri::AppHandle,
    window: tauri::Window,
    filename: String,
    content: String,
    content_type: Option<String>,
) -> Result<String, String> {
    use tauri_plugin_dialog::DialogExt;

    eprintln!("[download_file] Called: filename={}, content_type={:?}, content_len={}", filename, content_type, content.len());

    // Determine file filter based on content type or filename extension
    let extension = std::path::Path::new(&filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    let filter_name = match content_type.as_deref() {
        Some("application/json") => "JSON files",
        Some("text/html") => "HTML files",
        Some("text/plain") => "Text files",
        Some("text/csv") => "CSV files",
        _ => match extension {
            "json" => "JSON files",
            "html" | "htm" => "HTML files",
            "txt" => "Text files",
            "csv" => "CSV files",
            "tid" => "Tiddler files",
            _ => "All files",
        },
    };

    let extensions: &[&str] = match extension {
        "" => &["*"],
        ext => &[ext],
    };

    // Show save dialog (async variant to avoid blocking the tokio runtime,
    // which can deadlock on Linux/WebKitGTK where dialogs need the GTK main loop).
    // Parent window is set so the dialog appears correctly on desktop platforms.
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut builder = app.dialog()
        .file()
        .set_file_name(&filename)
        .add_filter(filter_name, extensions);
    #[cfg(not(target_os = "android"))]
    {
        builder = builder.set_parent(&window);
    }
    builder.save_file(move |file_path| {
        let _ = tx.send(file_path);
    });

    eprintln!("[download_file] Save dialog launched, waiting for user...");
    let file_path = rx.await.map_err(|_| "Dialog channel closed".to_string())?;
    eprintln!("[download_file] Dialog returned: {:?}", file_path.as_ref().map(|p| p.to_string()));

    match file_path {
        Some(path) => {
            // Write the content to the file
            let path_str = path.to_string();
            tokio::fs::write(&path_str, &content)
                .await
                .map_err(|e| format!("Failed to write file: {}", e))?;
            eprintln!("[download_file] File written successfully: {}", path_str);
            Ok(path_str)
        }
        None => Err("Save cancelled".to_string()),
    }
}

/// Fetch a URL via Rust (bypasses CORS restrictions for wikifile:// pages).
/// Simple GET-only version used by plugin library loading.
#[cfg(not(target_os = "android"))]
#[tauri::command]
async fn fetch_url(url: String) -> Result<String, String> {
    let resp = reqwest::get(&url)
        .await
        .map_err(|e| format!("Fetch failed: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.text()
        .await
        .map_err(|e| format!("Failed to read response: {}", e))
}

/// Fetch a single plugin/tiddler JSON from a TiddlyWiki plugin library.
/// The library at e.g. https://tiddlywiki.com/library/v5.3.8/index.html serves
/// individual plugin tiddlers as JSON at relative paths like
/// recipes/library/tiddlers/{double-encoded-title}.json
/// The title must be double-URI-encoded (the library stores files that way).
#[cfg(not(target_os = "android"))]
#[tauri::command]
async fn fetch_library_plugin(url: String, title: String) -> Result<String, String> {
    // Construct the plugin JSON URL from the library URL
    // Library URL: https://tiddlywiki.com/library/v5.3.8/index.html
    // Plugin URL:  https://tiddlywiki.com/library/v5.3.8/recipes/library/tiddlers/{double-encoded}.json
    let base_url = url.trim_end_matches("index.html").trim_end_matches('/');

    // Double-encode the title (library stores files with double-encoded names)
    fn uri_encode(s: &str) -> String {
        let mut result = String::new();
        for byte in s.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    result.push(byte as char);
                }
                _ => {
                    result.push_str(&format!("%{:02X}", byte));
                }
            }
        }
        result
    }
    let double_encoded = uri_encode(&uri_encode(&title));
    let plugin_url = format!("{}/recipes/library/tiddlers/{}.json", base_url, double_encoded);

    eprintln!("[PluginLibrary] Fetching plugin: {}", plugin_url);
    let resp = reqwest::get(&plugin_url)
        .await
        .map_err(|e| format!("Fetch failed: {}", e))?;
    if !resp.status().is_success() {
        return Err(format!("Plugin not found (HTTP {}): {}", resp.status(), title));
    }
    let json = resp.text()
        .await
        .map_err(|e| format!("Read failed: {}", e))?;
    eprintln!("[PluginLibrary] Plugin fetched: {} bytes for {}", json.len(), title);
    Ok(json)
}

/// Full HTTP request proxy for tm-http-request (bypasses CORS).
/// Returns a JSON object with status, statusText, headers, and data fields.
#[cfg(not(target_os = "android"))]
#[tauri::command]
async fn http_request(
    url: String,
    method: Option<String>,
    body: Option<String>,
    headers: Option<std::collections::HashMap<String, String>>,
    binary: Option<bool>,
    username: Option<String>,
    password: Option<String>,
    bearer_token: Option<String>,
) -> Result<serde_json::Value, String> {
    use base64::Engine;
    let client = reqwest::Client::new();
    let method_str = method.as_deref().unwrap_or("GET");
    let req_method = reqwest::Method::from_bytes(method_str.as_bytes())
        .map_err(|_| format!("Invalid HTTP method: {}", method_str))?;

    let mut builder = client.request(req_method, &url);

    // Set headers
    if let Some(ref hdrs) = headers {
        for (key, value) in hdrs {
            builder = builder.header(key.as_str(), value.as_str());
        }
    }

    // Set auth
    if let Some(ref token) = bearer_token {
        builder = builder.bearer_auth(token);
    } else if let Some(ref user) = username {
        builder = builder.basic_auth(user, password.as_deref());
    }

    // Set body
    if let Some(ref b) = body {
        builder = builder.body(b.clone());
    }

    let resp = builder.send().await.map_err(|e| format!("Request failed: {}", e))?;

    let status = resp.status().as_u16();
    let status_text = resp.status().canonical_reason().unwrap_or("").to_string();

    // Collect response headers
    let mut resp_headers = serde_json::Map::new();
    for (key, value) in resp.headers() {
        if let Ok(v) = value.to_str() {
            resp_headers.insert(key.as_str().to_string(), serde_json::Value::String(v.to_string()));
        }
    }

    // Read body
    let data = if binary.unwrap_or(false) {
        let bytes = resp.bytes().await.map_err(|e| format!("Failed to read response: {}", e))?;
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    } else {
        resp.text().await.map_err(|e| format!("Failed to read response: {}", e))?
    };

    Ok(serde_json::json!({
        "status": status,
        "statusText": status_text,
        "headers": resp_headers,
        "data": data
    }))
}

/// Extract a video poster frame using ffmpeg (desktop only)
/// Returns a data:image/jpeg;base64 URI, or None if ffmpeg is unavailable
/// Results are cached in {data_dir}/poster_cache/{md5}.jpg
#[cfg(not(target_os = "android"))]
#[tauri::command]
async fn extract_video_poster(app: tauri::AppHandle, path: String) -> Result<Option<String>, String> {
    use md5;
    use std::process::Command;

    // Security: validate path
    let path_buf = PathBuf::from(&path);
    if drag_drop::sanitize::validate_file_path(&path).is_none() {
        return Err("Invalid path".into());
    }
    let canonical = dunce::canonicalize(&path_buf).map_err(|e| format!("File not found: {}", e))?;
    if !drag_drop::sanitize::is_user_accessible_path(&canonical) {
        return Err("Access denied".into());
    }

    // Cache directory
    let data_dir = get_data_dir(&app)?;
    let cache_dir = data_dir.join("poster_cache");
    let _ = std::fs::create_dir_all(&cache_dir);

    let hash = format!("{:x}", md5::compute(path.as_bytes()));
    let cache_path = cache_dir.join(format!("{}.jpg", hash));

    // Return cached poster if it exists
    if cache_path.exists() {
        match tokio::fs::read(&cache_path).await {
            Ok(data) => {
                let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &data);
                return Ok(Some(format!("data:image/jpeg;base64,{}", b64)));
            }
            Err(_) => {} // Fall through to regenerate
        }
    }

    // Find ffmpeg binary
    let ffmpeg = find_ffmpeg();
    let ffmpeg = match ffmpeg {
        Some(f) => f,
        None => return Ok(None), // ffmpeg not available - not an error
    };

    // Extract poster: frame at 0.5s, scaled to 480px width, quality 8
    let cache_path_str = cache_path.to_string_lossy().to_string();
    let result = tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new(&ffmpeg);
        cmd.args([
                "-ss", "0.5",
                "-i", &path,
                "-vframes", "1",
                "-vf", "scale=480:-1",
                "-q:v", "8",
                "-f", "image2",
                "-y",
                &cache_path_str,
            ]);
        #[cfg(target_os = "windows")]
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd.output()
    }).await.map_err(|e| format!("Task error: {}", e))?;

    match result {
        Ok(output) => {
            if output.status.success() && cache_path.exists() {
                match tokio::fs::read(&cache_path).await {
                    Ok(data) => {
                        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &data);
                        Ok(Some(format!("data:image/jpeg;base64,{}", b64)))
                    }
                    Err(e) => Err(format!("Failed to read poster: {}", e)),
                }
            } else {
                Ok(None) // ffmpeg failed - probably not a video file
            }
        }
        Err(e) => {
            eprintln!("[TiddlyDesktop] ffmpeg error: {}", e);
            Ok(None)
        }
    }
}

/// Stub for Android — poster extraction is handled natively in WikiActivity.kt
#[cfg(target_os = "android")]
#[tauri::command]
async fn extract_video_poster(_app: tauri::AppHandle, _path: String) -> Result<Option<String>, String> {
    Ok(None)
}

// ---- PDFium Commands ----

/// Open a PDF from base64-encoded data. Returns handle + page metadata.
#[tauri::command]
fn pdf_open(data_base64: String) -> Result<pdf_renderer::PdfOpenResult, String> {
    let bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        &data_base64,
    ).map_err(|e| format!("Invalid base64: {}", e))?;
    pdf_renderer::pdf_open(bytes)
}

/// Open a PDF from a filesystem path. Used for tdasset:// URLs on WebKitGTK
/// where cross-scheme fetch fails.
#[tauri::command]
fn pdf_open_file(path: String) -> Result<pdf_renderer::PdfOpenResult, String> {
    let validated_path = drag_drop::sanitize::validate_user_file_path(&path)?;
    let bytes = std::fs::read(&validated_path)
        .map_err(|e| format!("Failed to read file {}: {}", path, e))?;
    pdf_renderer::pdf_open(bytes)
}

/// Render a single page as PNG.
#[tauri::command]
fn pdf_render_page(handle: u64, page_num: u32, width_px: u32) -> Result<pdf_renderer::PdfPageRenderResult, String> {
    pdf_renderer::pdf_render_page(handle, page_num, width_px)
}

/// Close a PDF document and release its handle.
#[tauri::command]
fn pdf_close(handle: u64) {
    pdf_renderer::pdf_close(handle)
}

/// Hit-test: find character index at pixel position on a rendered page.
#[tauri::command]
fn pdf_char_at_pos(handle: u64, page_num: u32, pixel_x: i32, pixel_y: i32, render_width: u32) -> Result<i32, String> {
    pdf_renderer::pdf_char_at_pos(handle, page_num, pixel_x, pixel_y, render_width)
}

/// Get selection highlight rectangles for a character range, in device pixels.
#[tauri::command]
fn pdf_selection_rects(handle: u64, page_num: u32, start_idx: u32, end_idx: u32, render_width: u32) -> Result<Vec<pdf_renderer::SelectionRect>, String> {
    pdf_renderer::pdf_selection_rects(handle, page_num, start_idx, end_idx, render_width)
}

/// Extract text for a character range.
#[tauri::command]
fn pdf_get_text(handle: u64, page_num: u32, start_idx: u32, end_idx: u32) -> Result<String, String> {
    pdf_renderer::pdf_get_text(handle, page_num, start_idx, end_idx)
}

/// Get total character count for a page.
#[tauri::command]
fn pdf_char_count(handle: u64, page_num: u32) -> Result<u32, String> {
    pdf_renderer::pdf_char_count(handle, page_num)
}

/// Media server state — held in Tauri managed state.
/// Contains the localhost HTTP server that serves token-registered media files.
/// Used on Linux (GStreamer needs HTTP URLs) and for folder wikis on all platforms
/// (HTTP-origin pages can't load tdasset:// custom scheme).
#[cfg(not(target_os = "android"))]
struct MediaServerState {
    server: media_server::MediaServer,
}

/// Register a media file with the localhost HTTP server and return its URL.
/// Only registered files can be served — this is the per-file allowlist mechanism.
/// The wiki's JavaScript calls this for each video/audio element it encounters.
#[cfg(not(target_os = "android"))]
#[tauri::command]
fn register_media_url(
    state: tauri::State<'_, MediaServerState>,
    path: String,
) -> Result<String, String> {
    // Validate path (same checks as tdasset:// protocol handler)
    if drag_drop::sanitize::validate_file_path(&path).is_none() {
        return Err("Invalid path".into());
    }

    let path_buf = PathBuf::from(&path);
    let canonical = dunce::canonicalize(&path_buf)
        .map_err(|e| format!("File not found: {}", e))?;

    if !drag_drop::sanitize::is_user_accessible_path(&canonical) {
        return Err("Access denied".into());
    }

    let token = state.server.register(canonical);
    Ok(format!("http://127.0.0.1:{}/media/{}", state.server.port(), token))
}

/// Stub for Android — media server not used there.
#[cfg(target_os = "android")]
#[tauri::command]
fn register_media_url() -> Result<String, String> {
    Err("Not available on this platform".into())
}

/// Find ffmpeg binary, checking common locations
#[cfg(not(target_os = "android"))]
fn find_ffmpeg() -> Option<String> {
    // Try PATH first
    let mut cmd = std::process::Command::new("ffmpeg");
    cmd.arg("-version");
    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);
    if let Ok(output) = cmd.output() {
        if output.status.success() {
            return Some("ffmpeg".into());
        }
    }

    // macOS: check Homebrew locations
    #[cfg(target_os = "macos")]
    {
        for path in &["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg"] {
            if PathBuf::from(path).exists() {
                return Some(path.to_string());
            }
        }
    }

    // Linux: check common paths
    #[cfg(target_os = "linux")]
    {
        for path in &["/usr/bin/ffmpeg", "/usr/local/bin/ffmpeg"] {
            if PathBuf::from(path).exists() {
                return Some(path.to_string());
            }
        }
    }

    None
}

/// Check if a path is a directory (used for file drop handling)
/// Security: Validates path before checking to prevent filesystem reconnaissance
#[tauri::command]
fn is_directory(path: String) -> Result<bool, String> {
    // Security: Validate path doesn't contain traversal sequences
    if drag_drop::sanitize::validate_file_path(&path).is_none() {
        return Err("Invalid path".to_string());
    }

    // Path must be absolute
    let path_buf = PathBuf::from(&path);
    if !path_buf.is_absolute() {
        return Err("Path must be absolute".to_string());
    }

    Ok(path_buf.is_dir())
}

/// Get current window state (size, position, monitor) for saving
#[tauri::command]
fn get_window_state_info(window: tauri::WebviewWindow) -> Result<serde_json::Value, String> {
    let size = window.inner_size().map_err(|e| e.to_string())?;
    let position = window.outer_position().map_err(|e| e.to_string())?;
    let is_maximized = window.is_maximized().unwrap_or(false);

    // Get scale factor to convert physical pixels to logical pixels
    // Tauri's inner_size() and outer_position() return physical pixels,
    // but WindowBuilder::position() and inner_size() expect logical pixels
    let scale_factor = window.scale_factor().unwrap_or(1.0);
    let logical_width = (size.width as f64 / scale_factor).round() as u32;
    let logical_height = (size.height as f64 / scale_factor).round() as u32;
    let logical_x = (position.x as f64 / scale_factor).round() as i32;
    let logical_y = (position.y as f64 / scale_factor).round() as i32;

    // Get the monitor this window is on, including its position for unique identification
    // (monitor name alone isn't unique if you have multiple identical monitors)
    // Monitor position is also in physical pixels, convert to logical
    let (monitor_name, monitor_x, monitor_y) = window.current_monitor()
        .ok()
        .flatten()
        .map(|m| {
            let pos = m.position();
            let logical_mon_x = (pos.x as f64 / scale_factor).round() as i32;
            let logical_mon_y = (pos.y as f64 / scale_factor).round() as i32;
            (m.name().map(|n| n.to_string()), logical_mon_x, logical_mon_y)
        })
        .unwrap_or((None, 0, 0));

    Ok(serde_json::json!({
        "width": logical_width,
        "height": logical_height,
        "x": logical_x,
        "y": logical_y,
        "monitor_name": monitor_name,
        "monitor_x": monitor_x,
        "monitor_y": monitor_y,
        "maximized": is_maximized
    }))
}

/// Get saved window state for a wiki path
#[tauri::command]
fn get_saved_window_state(app: tauri::AppHandle, path: String) -> Option<types::WindowState> {
    wiki_storage::get_window_state(&app, &path)
}

/// JavaScript for injecting a custom find bar UI
/// This is used on platforms without native find-in-page UI (Linux, Windows)
const FIND_BAR_JS: &str = r#"
(function() {
    var HIGHLIGHT_CLASS = 'td-find-highlight';
    var CURRENT_CLASS = 'td-find-current';

    // Get colour from palette via TiddlyDesktop helper or use fallback
    function getColour(name, fallback) {
        if (window.TiddlyDesktop && typeof window.TiddlyDesktop.getColour === 'function') {
            return window.TiddlyDesktop.getColour(name, fallback);
        }
        return fallback;
    }

    // Get palette colors
    var pageBackground = getColour('page-background', '#f0f0f0');
    var background = getColour('background', '#ffffff');
    var foreground = getColour('foreground', '#333333');
    var tabBorder = getColour('tab-border', '#cccccc');
    var tabBackground = getColour('tab-background', '#eeeeee');
    var mutedForeground = getColour('muted-foreground', '#666666');
    var primary = getColour('primary', '#5778d8');

    // Add highlight styles if not present
    if (!document.getElementById('td-find-styles')) {
        var style = document.createElement('style');
        style.id = 'td-find-styles';
        style.textContent = '.' + HIGHLIGHT_CLASS + '{background:#ffeb3b;color:#000;border-radius:2px;}' +
            '.' + CURRENT_CLASS + '{background:#ff9800;color:#000;box-shadow:0 0 0 2px #ff9800;}';
        document.head.appendChild(style);
    }

    // Check if find bar already exists
    var existingBar = document.getElementById('td-find-bar');
    if (existingBar) {
        existingBar.style.display = 'flex';
        var input = existingBar.querySelector('input');
        if (input) {
            input.focus();
            input.select();
        }
        return;
    }

    // Create find bar
    var bar = document.createElement('div');
    bar.id = 'td-find-bar';
    bar.style.cssText = 'position:fixed;top:0;left:0;right:0;display:flex;align-items:center;gap:8px;padding:8px 12px;background:' + pageBackground + ';border-bottom:1px solid ' + tabBorder + ';z-index:999999;font-family:system-ui,sans-serif;font-size:14px;box-shadow:0 2px 8px rgba(0,0,0,0.15);';

    var input = document.createElement('input');
    input.type = 'text';
    input.placeholder = 'Find in page...';
    input.style.cssText = 'flex:1;max-width:300px;padding:6px 10px;border:1px solid ' + tabBorder + ';border-radius:4px;font-size:14px;outline:none;background:' + background + ';color:' + foreground + ';';

    var info = document.createElement('span');
    info.style.cssText = 'color:' + mutedForeground + ';min-width:100px;text-align:center;';
    info.textContent = '';

    var prevBtn = document.createElement('button');
    prevBtn.textContent = '▲';
    prevBtn.title = 'Previous (Shift+F3, Shift+Enter, Ctrl/Cmd+Shift+G)';
    prevBtn.style.cssText = 'padding:4px 10px;border:1px solid ' + tabBorder + ';border-radius:4px;background:' + background + ';color:' + foreground + ';cursor:pointer;font-size:12px;';

    var nextBtn = document.createElement('button');
    nextBtn.textContent = '▼';
    nextBtn.title = 'Next (F3, Enter, Ctrl/Cmd+G)';
    nextBtn.style.cssText = 'padding:4px 10px;border:1px solid ' + tabBorder + ';border-radius:4px;background:' + background + ';color:' + foreground + ';cursor:pointer;font-size:12px;';

    var closeBtn = document.createElement('button');
    closeBtn.textContent = '✕';
    closeBtn.title = 'Close (Escape)';
    closeBtn.style.cssText = 'padding:4px 10px;border:none;background:transparent;cursor:pointer;font-size:16px;color:' + mutedForeground + ';';

    bar.appendChild(input);
    bar.appendChild(info);
    bar.appendChild(prevBtn);
    bar.appendChild(nextBtn);
    bar.appendChild(closeBtn);
    document.body.appendChild(bar);

    var highlights = [];
    var currentIndex = -1;
    var lastSearch = '';
    var searchTimeout = null;

    function clearHighlights() {
        highlights.forEach(function(span) {
            var parent = span.parentNode;
            if (parent) {
                parent.replaceChild(document.createTextNode(span.textContent), span);
                parent.normalize();
            }
        });
        highlights = [];
        currentIndex = -1;
    }

    function highlightMatches(term) {
        clearHighlights();
        if (!term) {
            info.textContent = '';
            return;
        }

        var termLower = term.toLowerCase();
        var walker = document.createTreeWalker(
            document.body,
            NodeFilter.SHOW_TEXT,
            {
                acceptNode: function(node) {
                    // Skip the find bar itself and script/style elements
                    var parent = node.parentElement;
                    if (!parent) return NodeFilter.FILTER_REJECT;
                    if (parent.closest('#td-find-bar')) return NodeFilter.FILTER_REJECT;
                    var tag = parent.tagName;
                    if (tag === 'SCRIPT' || tag === 'STYLE' || tag === 'NOSCRIPT') {
                        return NodeFilter.FILTER_REJECT;
                    }
                    if (node.textContent.toLowerCase().indexOf(termLower) !== -1) {
                        return NodeFilter.FILTER_ACCEPT;
                    }
                    return NodeFilter.FILTER_REJECT;
                }
            }
        );

        var nodesToProcess = [];
        var textNode;
        while (textNode = walker.nextNode()) {
            nodesToProcess.push(textNode);
        }

        nodesToProcess.forEach(function(node) {
            var text = node.textContent;
            var textLower = text.toLowerCase();
            var idx = 0;
            var lastIdx = 0;
            var frag = document.createDocumentFragment();

            while ((idx = textLower.indexOf(termLower, lastIdx)) !== -1) {
                // Add text before match
                if (idx > lastIdx) {
                    frag.appendChild(document.createTextNode(text.substring(lastIdx, idx)));
                }
                // Add highlighted match
                var span = document.createElement('span');
                span.className = HIGHLIGHT_CLASS;
                span.textContent = text.substring(idx, idx + term.length);
                frag.appendChild(span);
                highlights.push(span);
                lastIdx = idx + term.length;
            }

            // Add remaining text
            if (lastIdx < text.length) {
                frag.appendChild(document.createTextNode(text.substring(lastIdx)));
            }

            node.parentNode.replaceChild(frag, node);
        });

        if (highlights.length > 0) {
            currentIndex = 0;
            updateCurrent();
            info.textContent = '1 of ' + highlights.length;
            info.style.color = mutedForeground;
        } else {
            info.textContent = 'No matches';
            info.style.color = getColour('alert-highlight', '#c00');
        }
    }

    function updateCurrent() {
        highlights.forEach(function(span, i) {
            if (i === currentIndex) {
                span.classList.add(CURRENT_CLASS);
                span.scrollIntoView({ behavior: 'smooth', block: 'center' });
            } else {
                span.classList.remove(CURRENT_CLASS);
            }
        });
    }

    function goToMatch(delta) {
        if (highlights.length === 0) return;
        currentIndex = (currentIndex + delta + highlights.length) % highlights.length;
        updateCurrent();
        info.textContent = (currentIndex + 1) + ' of ' + highlights.length;
    }

    function doSearch() {
        var term = input.value;
        if (term === lastSearch) return;
        lastSearch = term;
        highlightMatches(term);
    }

    function closeBar() {
        bar.style.display = 'none';
        clearHighlights();
        lastSearch = '';
        info.textContent = '';
        document.removeEventListener('keydown', globalKeyHandler, true);
    }

    function globalKeyHandler(e) {
        if (bar.style.display === 'none') return;

        if (e.key === 'F3') {
            e.preventDefault();
            e.stopPropagation();
            goToMatch(e.shiftKey ? -1 : 1);
            input.focus();
        } else if ((e.key === 'g' || e.key === 'G') && (e.ctrlKey || e.metaKey)) {
            // Ctrl+G / Cmd+G - Find next, Ctrl+Shift+G / Cmd+Shift+G - Find previous
            e.preventDefault();
            e.stopPropagation();
            goToMatch(e.shiftKey ? -1 : 1);
            input.focus();
        } else if (e.key === 'Escape') {
            e.preventDefault();
            e.stopPropagation();
            closeBar();
        }
    }

    document.addEventListener('keydown', globalKeyHandler, true);

    input.addEventListener('input', function() {
        if (searchTimeout) clearTimeout(searchTimeout);
        searchTimeout = setTimeout(doSearch, 200);
    });

    input.addEventListener('keydown', function(e) {
        if (e.key === 'Enter' || e.key === 'F3') {
            e.preventDefault();
            if (searchTimeout) {
                clearTimeout(searchTimeout);
                doSearch();
            }
            goToMatch(e.shiftKey ? -1 : 1);
        } else if (e.key === 'Escape') {
            e.preventDefault();
            closeBar();
        }
    });

    prevBtn.addEventListener('click', function(e) {
        e.preventDefault();
        goToMatch(-1);
        input.focus();
    });

    nextBtn.addEventListener('click', function(e) {
        e.preventDefault();
        goToMatch(1);
        input.focus();
    });

    closeBtn.addEventListener('click', function(e) {
        e.preventDefault();
        closeBar();
    });

    input.focus();
})();
"#;

/// Show the find-in-page UI for the webview
/// Platform-specific implementations:
/// - Windows (WebView2): Injects custom find bar (no native UI)
/// - macOS (WKWebView): Uses performTextFinderAction for native find bar
/// - Linux (WebKitGTK): Injects custom find bar (no native UI)
#[tauri::command]
fn show_find_in_page(window: tauri::WebviewWindow) -> Result<(), String> {
    show_find_in_page_impl(&window)
}

#[cfg(target_os = "windows")]
fn show_find_in_page_impl(window: &tauri::WebviewWindow) -> Result<(), String> {
    // WebView2 doesn't have a built-in find bar UI
    // Inject a custom find bar that uses window.find() API
    let _ = window.eval(FIND_BAR_JS);
    Ok(())
}

#[cfg(target_os = "macos")]
fn show_find_in_page_impl(window: &tauri::WebviewWindow) -> Result<(), String> {
    // Use the same JavaScript find bar as Linux/Windows for consistency
    let _ = window.eval(FIND_BAR_JS);
    Ok(())
}

#[cfg(target_os = "linux")]
fn show_find_in_page_impl(window: &tauri::WebviewWindow) -> Result<(), String> {
    // WebKitGTK doesn't have a built-in find bar UI
    // Inject a custom find bar that uses window.find() API
    let _ = window.eval(FIND_BAR_JS);
    Ok(())
}

#[cfg(target_os = "android")]
fn show_find_in_page_impl(window: &tauri::WebviewWindow) -> Result<(), String> {
    // Android WebView: Inject custom find bar
    let _ = window.eval(FIND_BAR_JS);
    Ok(())
}

/// Start a native OS drag operation with the provided data
/// Called from JavaScript when the pointer leaves the window during an internal drag
#[tauri::command]
fn start_native_drag(
    window: tauri::WebviewWindow,
    data: drag_drop::NativeDragData,
    x: i32,
    y: i32,
    image_data: Option<Vec<u8>>,
    image_offset_x: Option<i32>,
    image_offset_y: Option<i32>,
) -> Result<(), String> {
    drag_drop::start_native_drag_impl(&window, data, x, y, image_data, image_offset_x, image_offset_y)
}

/// Prepare for a potential native drag operation
/// Called from JavaScript when an internal drag starts
#[tauri::command]
fn prepare_native_drag(
    window: tauri::WebviewWindow,
    data: drag_drop::NativeDragData,
) -> Result<(), String> {
    drag_drop::prepare_native_drag_impl(&window, data)
}

/// Clean up native drag preparation
/// Called from JavaScript when an internal drag ends normally (within the window)
#[tauri::command]
fn cleanup_native_drag() -> Result<(), String> {
    drag_drop::cleanup_native_drag_impl()
}

/// Get pending drag data for cross-wiki drops.
/// Called from JavaScript when a drag enters the window to check if there's
/// cross-wiki drag data available (IPC-based fallback for platforms where
/// native drag tracking doesn't work, e.g., Windows without custom IDropTarget).
#[tauri::command]
fn get_pending_drag_data(target_window: String) -> Option<drag_drop::PendingDragDataResponse> {
    drag_drop::get_pending_drag_data_impl(&target_window)
}

/// Get external drop file paths stored by WRY patch via FFI.
/// Called from JavaScript when a native DOM drop fires to get the original file paths.
/// Returns the paths and clears the storage (one-time read).
#[tauri::command]
fn get_external_drop_paths() -> Option<Vec<String>> {
    drag_drop::get_external_drop_paths_impl()
}

/// Update the drag icon during an active native drag operation
/// Called from JavaScript to change the drag image mid-drag
#[tauri::command]
fn update_drag_icon(
    image_data: Vec<u8>,
    offset_x: i32,
    offset_y: i32,
) -> Result<(), String> {
    drag_drop::update_drag_icon_impl(image_data, offset_x, offset_y)
}

/// Set the pending drag icon before a drag starts
/// Called from JavaScript during drag preparation so the icon is ready for drag-begin
#[cfg(target_os = "linux")]
#[tauri::command]
fn set_pending_drag_icon(image_data: Vec<u8>, offset_x: i32, offset_y: i32) -> Result<(), String> {
    drag_drop::set_pending_drag_icon_impl(image_data, offset_x, offset_y)
}

#[cfg(not(target_os = "linux"))]
#[tauri::command]
fn set_pending_drag_icon(_image_data: Vec<u8>, _offset_x: i32, _offset_y: i32) -> Result<(), String> {
    Ok(()) // No-op on other platforms
}

/// Toggle drag destination handling on WebKitWebView
/// When disabled, WebKitGTK's native handling takes over (shows caret in editables)
/// When enabled, our custom handling intercepts drags
/// Called from JavaScript when entering/leaving editable elements during drag
#[tauri::command]
fn set_drag_dest_enabled(window: tauri::Window, enabled: bool) -> Result<(), String> {
    drag_drop::set_drag_dest_enabled_impl(window.label(), enabled);
    Ok(())
}

/// Temporarily ungrab the seat to allow focus changes during drag
/// Called from JavaScript when hovering over an editable element
#[tauri::command]
fn ungrab_seat_for_focus(window: tauri::Window) -> Result<(), String> {
    drag_drop::ungrab_seat_for_focus_impl(window.label());
    Ok(())
}

/// Set whether cursor is over a droppable widget (Windows only)
/// Called from JavaScript during dragenter/dragleave on $droppable elements.
/// This controls the cursor effect for internal drags - shows "copy" over droppables,
/// "no drop" elsewhere.
#[tauri::command]
fn set_over_droppable(over: bool) {
    #[cfg(target_os = "windows")]
    drag_drop::windows::set_over_droppable(over);
    #[cfg(not(target_os = "windows"))]
    let _ = over; // Suppress unused warning on non-Windows
}

/// Set the internal drag state (Windows only)
/// Called from JavaScript at dragstart to tell the native layer what type of drag is happening.
/// This is more reliable than the WebView2 DragStarting event because JS dragstart fires earlier.
/// drag_type: "tiddler", "link", "text", or "none" (to clear)
#[tauri::command]
fn set_internal_drag_type(drag_type: String) {
    #[cfg(target_os = "windows")]
    drag_drop::windows::set_internal_drag_type_from_js(&drag_type);
    #[cfg(not(target_os = "windows"))]
    let _ = drag_type; // Suppress unused warning on non-Windows
}

/// Check if a file is a valid TiddlyWiki HTML file
/// Returns Ok(()) if valid, Err with reason if not
fn validate_tiddlywiki_file(path: &std::path::Path) -> Result<(), String> {
    let path_str = path.to_string_lossy();

    // Check if this is an Android SAF URI
    let is_saf_uri = path_str.starts_with("content://") || path_str.starts_with('{');

    if !is_saf_uri {
        // Desktop filesystem path - do standard validation
        // Check file exists and is a file
        if !path.exists() {
            return Err(format!("File does not exist: {}", path.display()));
        }
        if !path.is_file() {
            return Err(format!("Path is not a file: {}", path.display()));
        }

        // Check extension
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();
        if ext != "html" && ext != "htm" {
            return Err(format!("File must have .html or .htm extension, got: .{}", ext));
        }
    }

    // Read the first 100KB of the file to check for TiddlyWiki markers
    // TiddlyWiki headers and meta tags are always near the top
    // Use fs_abstraction for Android SAF support
    let buffer = fs_abstraction::read_file(path)?;
    let buffer = if buffer.len() > 100_000 {
        buffer[..100_000].to_vec()
    } else {
        buffer
    };

    let content = String::from_utf8_lossy(&buffer);

    // Check for TiddlyWiki markers (must have at least one)
    let markers = [
        // TiddlyWiki5 meta tag (most reliable marker)
        r#"<meta name="tiddlywiki-version""#,
        r#"<meta name='tiddlywiki-version'"#,
        // TiddlyWiki5 tiddler store
        r#"class="tiddlywiki-tiddler-store""#,
        r#"class='tiddlywiki-tiddler-store'"#,
        // Legacy TiddlyWiki store area
        r#"id="storeArea""#,
        r#"id='storeArea'"#,
        // TiddlyWiki application name
        r#"name="application-name" content="TiddlyWiki"#,
        // Boot kernel markers
        r#"$:/boot/boot.js"#,
        r#"$:/boot/bootprefix.js"#,
    ];

    let has_marker = markers.iter().any(|marker| content.contains(marker));

    if !has_marker {
        return Err("File does not appear to be a TiddlyWiki HTML file. Missing required TiddlyWiki markers.".to_string());
    }

    // Additional safety check: make sure it looks like HTML
    let content_lower = content.to_lowercase();
    if !content_lower.contains("<!doctype html") && !content_lower.contains("<html") {
        return Err("File does not appear to be a valid HTML document.".to_string());
    }

    Ok(())
}

/// Async version of validate_tiddlywiki_file
async fn validate_tiddlywiki_file_async(path: &std::path::Path) -> Result<(), String> {
    let path_str = path.to_string_lossy();
    let path_buf = path.to_path_buf();

    // Check if this is an Android SAF URI
    let is_saf_uri = path_str.starts_with("content://") || path_str.starts_with('{');

    if !is_saf_uri {
        // Desktop filesystem path - do standard validation
        // Check file exists and is a file
        if !path_buf.exists() {
            return Err(format!("File does not exist: {}", path_buf.display()));
        }
        if !path_buf.is_file() {
            return Err(format!("Path is not a file: {}", path_buf.display()));
        }

        // Check extension
        let ext = path_buf.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_lowercase())
            .unwrap_or_default();
        if ext != "html" && ext != "htm" {
            return Err(format!("File must have .html or .htm extension, got: .{}", ext));
        }
    }

    // Read the first 100KB using fs_abstraction for Android SAF support
    // Use blocking read in spawn_blocking since fs_abstraction is sync
    let path_for_read = path_buf.clone();
    let buffer = tokio::task::spawn_blocking(move || {
        fs_abstraction::read_file(&path_for_read)
    }).await
        .map_err(|e| format!("Failed to spawn blocking task: {}", e))?
        .map_err(|e| format!("Failed to read file: {}", e))?;

    let buffer = if buffer.len() > 100_000 {
        buffer[..100_000].to_vec()
    } else {
        buffer
    };

    let content = String::from_utf8_lossy(&buffer);

    // Check for TiddlyWiki markers
    let markers = [
        r#"<meta name="tiddlywiki-version""#,
        r#"<meta name='tiddlywiki-version'"#,
        r#"class="tiddlywiki-tiddler-store""#,
        r#"class='tiddlywiki-tiddler-store'"#,
        r#"id="storeArea""#,
        r#"id='storeArea'"#,
        r#"name="application-name" content="TiddlyWiki"#,
        r#"$:/boot/boot.js"#,
        r#"$:/boot/bootprefix.js"#,
    ];

    let has_marker = markers.iter().any(|marker| content.contains(marker));

    if !has_marker {
        return Err("File does not appear to be a TiddlyWiki HTML file. Missing required TiddlyWiki markers.".to_string());
    }

    let content_lower = content.to_lowercase();
    if !content_lower.contains("<!doctype html") && !content_lower.contains("<html") {
        return Err("File does not appear to be a valid HTML document.".to_string());
    }

    Ok(())
}

/// Check if a port is available by attempting to bind to it
fn is_port_available(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

/// Get the next available port for a wiki folder server
/// Checks that the port is actually free before returning it
fn allocate_port(state: &AppState) -> u16 {
    let mut port = state.next_port.lock().unwrap();

    // Find an available port, starting from the current value
    let mut attempts = 0;
    const MAX_ATTEMPTS: u16 = 1000; // Don't search forever

    while attempts < MAX_ATTEMPTS {
        if is_port_available(*port) {
            let allocated = *port;
            *port += 1;
            eprintln!("[TiddlyDesktop] Allocated port {} for wiki folder server", allocated);
            return allocated;
        }
        eprintln!("[TiddlyDesktop] Port {} is in use, trying next", *port);
        *port += 1;
        attempts += 1;
    }

    // Fallback: return the current port and hope for the best
    eprintln!("[TiddlyDesktop] Warning: Could not find available port after {} attempts", MAX_ATTEMPTS);
    let allocated = *port;
    *port += 1;
    allocated
}

/// Check if system Node.js is available and compatible (v18+)
#[cfg(not(target_os = "android"))]
fn find_system_node() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let node_name = "node.exe";
    #[cfg(not(target_os = "windows"))]
    let node_name = "node";

    // Check if node is in PATH
    let mut cmd = Command::new(node_name);
    cmd.arg("--version");
    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);

    if let Ok(output) = cmd.output() {
        if output.status.success() {
            let version = String::from_utf8_lossy(&output.stdout);
            // Parse version (e.g., "v20.11.0" -> 20)
            if let Some(major) = version.trim().strip_prefix('v')
                .and_then(|v| v.split('.').next())
                .and_then(|m| m.parse::<u32>().ok())
            {
                // Require Node.js v18 or higher
                if major >= 18 {
                    println!("Found system Node.js {} in PATH", version.trim());
                    return Some(PathBuf::from(node_name));
                } else {
                    println!("System Node.js {} is too old (need v18+), using bundled", version.trim());
                }
            }
        }
    }
    None
}

/// Find Node.js executable without needing an AppHandle (for use before Tauri setup)
#[cfg(not(target_os = "android"))]
fn find_node_executable() -> Option<PathBuf> {
    // First, try system Node.js
    if let Some(system_node) = find_system_node() {
        return Some(system_node);
    }

    // Fall back to bundled Node.js relative to exe
    #[cfg(target_os = "windows")]
    let node_name = "node.exe";
    #[cfg(not(target_os = "windows"))]
    let node_name = "node";

    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();

    let possible_paths = [
        exe_dir.join(node_name),
        exe_dir.join("resources").join("binaries").join(node_name),
        exe_dir.join("..").join("lib").join("tiddlydesktop-rs").join("resources").join("binaries").join(node_name),
    ];

    for path in &possible_paths {
        if path.exists() {
            eprintln!("[TiddlyDesktop] Using bundled Node.js at {:?}", path);
            return Some(path.clone());
        }
    }

    None
}

/// Get path to Node.js binary (prefer system, fall back to bundled)
#[cfg(not(target_os = "android"))]
fn get_node_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    // First, try to use system Node.js if available and compatible
    if let Some(system_node) = find_system_node() {
        return Ok(system_node);
    }

    // Fall back to bundled Node.js
    let resource_path = get_resource_dir_path(app)
        .ok_or_else(|| "Failed to get resource directory".to_string())?;
    let resource_path = utils::normalize_path(resource_path);

    #[cfg(target_os = "windows")]
    let node_name = "node.exe";
    #[cfg(not(target_os = "windows"))]
    let node_name = "node";

    // Tauri sidecars are placed in the same directory as the main executable
    let exe_dir = std::env::current_exe()
        .map_err(|e| format!("Failed to get exe path: {}", e))?
        .parent()
        .ok_or("Failed to get exe directory")?
        .to_path_buf();

    // Try different possible locations for bundled Node.js
    let possible_paths = [
        exe_dir.join(node_name),
        resource_path.join("resources").join("binaries").join(node_name),
        resource_path.join("binaries").join(node_name),
    ];

    for path in &possible_paths {
        if path.exists() {
            println!("Using bundled Node.js at {:?}", path);
            return Ok(path.clone());
        }
    }

    Err(format!("Node.js not found. Install Node.js v18+ or ensure bundled binary exists. Tried: {:?}", possible_paths))
}

/// Get path to bundled TiddlyWiki
fn get_tiddlywiki_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let resource_path = get_resource_dir_path(app)
        .ok_or_else(|| "Failed to get resource directory".to_string())?;
    let resource_path = utils::normalize_path(resource_path);

    // Try multiple possible locations for tiddlywiki.js:
    let candidates = [
        // Tarball structure: lib/tiddlydesktop-rs/tiddlywiki/tiddlywiki.js
        resource_path.join("tiddlywiki").join("tiddlywiki.js"),
        // Tauri bundle structure with resources/ prefix
        resource_path.join("resources").join("tiddlywiki").join("tiddlywiki.js"),
        // CI copies TiddlyWiki5 into existing resources/tiddlywiki/, creating a subdirectory
        resource_path.join("tiddlywiki").join("TiddlyWiki5").join("tiddlywiki.js"),
        resource_path.join("resources").join("tiddlywiki").join("TiddlyWiki5").join("tiddlywiki.js"),
    ];

    for candidate in &candidates {
        if candidate.exists() {
            return Ok(candidate.clone());
        }
    }

    // Development path
    let dev_path = PathBuf::from("src-tauri/resources/tiddlywiki/tiddlywiki.js");
    if dev_path.exists() {
        let canonical = dev_path.canonicalize().map_err(|e| e.to_string())?;
        return Ok(utils::normalize_path(canonical));
    }

    Err(format!("TiddlyWiki not found at {:?}, {:?}, {:?}, {:?}, or {:?}",
        candidates[0], candidates[1], candidates[2], candidates[3], dev_path))
}

/// Ensure required plugins and autosave are enabled for a wiki folder
fn ensure_wiki_folder_config(wiki_path: &PathBuf) {
    // Ensure required plugins are in tiddlywiki.info
    let info_path = wiki_path.join("tiddlywiki.info");
    if info_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&info_path) {
            if let Ok(mut info) = serde_json::from_str::<serde_json::Value>(&content) {
                let required_plugins = vec!["tiddlywiki/tiddlyweb", "tiddlywiki/filesystem"];
                let mut modified = false;

                let plugins_array = info.get_mut("plugins")
                    .and_then(|v| v.as_array_mut());

                if let Some(arr) = plugins_array {
                    for plugin_path in &required_plugins {
                        if !arr.iter().any(|p| p.as_str() == Some(*plugin_path)) {
                            arr.push(serde_json::Value::String(plugin_path.to_string()));
                            modified = true;
                        }
                    }
                } else {
                    // Create plugins array with required plugins
                    let plugins: Vec<serde_json::Value> = required_plugins.iter()
                        .map(|p| serde_json::Value::String(p.to_string()))
                        .collect();
                    info["plugins"] = serde_json::Value::Array(plugins);
                    modified = true;
                }

                if modified {
                    if let Ok(updated_content) = serde_json::to_string_pretty(&info) {
                        if let Err(e) = std::fs::write(&info_path, updated_content) {
                            eprintln!("Warning: Failed to update tiddlywiki.info: {}", e);
                        } else {
                            println!("Added required plugins to tiddlywiki.info");
                        }
                    }
                }
            }
        }
    }

    // Ensure autosave is enabled
    let tiddlers_dir = wiki_path.join("tiddlers");
    let autosave_tiddler = tiddlers_dir.join("$__config_AutoSave.tid");

    // Only create if the tiddlers folder exists and autosave tiddler doesn't
    if tiddlers_dir.exists() && !autosave_tiddler.exists() {
        let autosave_content = "title: $:/config/AutoSave\n\nyes";
        if let Err(e) = std::fs::write(&autosave_tiddler, autosave_content) {
            eprintln!("Warning: Failed to enable autosave: {}", e);
        } else {
            println!("Enabled autosave for wiki folder");
        }
    }
}

/// Wait for TCP server with exponential backoff
fn wait_for_server_ready(port: u16, process: &mut Child, timeout: std::time::Duration) -> Result<(), String> {
    use std::net::TcpStream;
    use std::time::Instant;

    let start = Instant::now();
    let addr: std::net::SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
    let mut delay = std::time::Duration::from_millis(50);

    loop {
        // Check if process died
        if let Ok(Some(status)) = process.try_wait() {
            return Err(format!("Server exited with status: {}", status));
        }

        // Try to connect
        if TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(100)).is_ok() {
            println!("Server ready on port {} ({:.1}s)", port, start.elapsed().as_secs_f64());
            return Ok(());
        }

        // Check timeout
        if start.elapsed() >= timeout {
            return Err(format!("Server failed to start within {:?}", timeout));
        }

        std::thread::sleep(delay);
        delay = (delay * 2).min(std::time::Duration::from_secs(1)); // Cap at 1s
    }
}

/// Open a wiki folder in a separate process with its own server
/// Returns WikiEntry so frontend can update its wiki list
#[cfg(not(target_os = "android"))]
#[tauri::command]
async fn open_wiki_folder(app: tauri::AppHandle, path: String, _tiddler_title: Option<String>) -> Result<WikiEntry, String> {
    // Security: Validate path is a user-accessible directory
    let path_buf = drag_drop::sanitize::validate_user_directory_path(&path)?;
    let state = app.state::<AppState>();

    // Get folder name
    let folder_name = path_buf
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Unknown")
        .to_string();

    // Verify it's a wiki folder
    if !utils::is_wiki_folder(&path_buf) {
        return Err("Not a valid wiki folder (missing tiddlywiki.info)".to_string());
    }

    // Check if this wiki folder is already open (tracked as a wiki process)
    {
        let wiki_processes = state.wiki_processes.lock().unwrap();
        if wiki_processes.contains_key(&path) {
            // Wiki folder already open - send focus request via IPC
            eprintln!("[TiddlyDesktop] Wiki folder already open in separate process: {}", path);
            if let Some(server) = GLOBAL_IPC_SERVER.get() {
                if let Err(e) = server.send_focus_window(&path) {
                    eprintln!("[TiddlyDesktop] Failed to send focus request: {}", e);
                }
            }
            // Get existing favicon from storage
            let existing_favicon = wiki_storage::get_wiki_favicon(&app, &path);
            return Ok(WikiEntry {
                path: path.clone(),
                filename: folder_name,
                display_path: Some(path.clone()),
                favicon: existing_favicon,
                is_folder: true,
                backups_enabled: false,
                backup_dir: None,
                backup_count: None,
                group: None,
                sync_enabled: false,
                sync_id: None,
                sync_peers: vec![],
        relay_room: None,
            });
        }
    }

    // Extract favicon from the wiki folder
    let favicon = tiddlywiki_html::extract_favicon_from_folder(&path_buf).await;

    // Allocate a port for this server
    let port = allocate_port(&state);

    // Get the path to our own executable
    let exe_path = std::env::current_exe()
        .map_err(|e| format!("Failed to get executable path: {}", e))?;

    // Spawn the wiki folder process
    eprintln!("[TiddlyDesktop] Spawning wiki folder process: {} --wiki-folder {} --port {}",
        exe_path.display(), path, port);

    let mut cmd = Command::new(&exe_path);
    cmd.arg("--wiki-folder").arg(&path)
       .arg("--port").arg(port.to_string());

    // Pass IPC auth token to child process via environment variable
    if let Some(token) = ipc::get_auth_token() {
        cmd.env(ipc::AUTH_TOKEN_ENV_VAR, token);
    }

    // Pass collab WS port so child process can connect
    #[cfg(not(target_os = "android"))]
    {
        let collab_port = lan_sync::get_collab_port();
        if collab_port > 0 {
            cmd.env("COLLAB_WS_PORT", collab_port.to_string());
        }
    }

    // Set TIDDLYWIKI_PLUGIN_PATH so Node.js can find user-installed plugins from {app_data}/plugins/.
    // Bundled plugins live in tiddlywiki/plugins/ and are found automatically by TiddlyWiki.
    if let Ok(data_dir) = get_data_dir(&app) {
        let plugins_dir = data_dir.join("plugins");
        let _ = std::fs::create_dir_all(&plugins_dir);
        let sep = if cfg!(windows) { ";" } else { ":" };
        let mut paths = vec![plugins_dir.to_string_lossy().to_string()];
        if let Ok(existing) = std::env::var("TIDDLYWIKI_PLUGIN_PATH") {
            if !existing.is_empty() {
                paths.push(existing);
            }
        }
        let plugin_path = paths.join(sep);
        cmd.env("TIDDLYWIKI_PLUGIN_PATH", &plugin_path);
        eprintln!("[TiddlyDesktop] Set TIDDLYWIKI_PLUGIN_PATH={}", plugin_path);
    }

    // Set TIDDLYWIKI_EDITION_PATH so TiddlyWiki can find user editions.
    // Include our {data_dir}/editions dir and any existing global TIDDLYWIKI_EDITION_PATH.
    if let Ok(data_dir) = get_data_dir(&app) {
        let editions_dir = data_dir.join("editions");
        let _ = std::fs::create_dir_all(&editions_dir);
        let sep = if cfg!(windows) { ";" } else { ":" };
        let edition_path = match std::env::var("TIDDLYWIKI_EDITION_PATH") {
            Ok(existing) if !existing.is_empty() => {
                format!("{}{}{}", editions_dir.display(), sep, existing)
            }
            _ => editions_dir.to_string_lossy().to_string(),
        };
        cmd.env("TIDDLYWIKI_EDITION_PATH", &edition_path);
        eprintln!("[TiddlyDesktop] Set TIDDLYWIKI_EDITION_PATH={}", edition_path);
    }

    #[cfg(target_os = "windows")]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    // Wiki folder processes run independently - they survive when landing page closes

    let child = cmd.spawn()
        .map_err(|e| format!("Failed to spawn wiki folder process: {}", e))?;

    let pid = child.id();
    eprintln!("[TiddlyDesktop] Wiki folder process spawned with PID: {}", pid);

    // Windows: Assign to job object so it gets killed when parent exits
    #[cfg(target_os = "windows")]
    drag_drop::windows_job::assign_process_to_job(pid);

    // Track the process (using wiki_processes like single-file wikis)
    state.wiki_processes.lock().unwrap().insert(path.clone(), WikiProcess {
        pid,
        path: path.clone(),
    });

    // Spawn a thread to wait for the process to exit and clean up
    let app_handle = app.clone();
    let path_clone = path.clone();
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
        eprintln!("[TiddlyDesktop] Wiki folder process {} exited", pid);
        // Clean up tracking
        let state = app_handle.state::<AppState>();
        state.wiki_processes.lock().unwrap().remove(&path_clone);

        // Exit app if no more wikis and no windows
        let wiki_count = state.wiki_processes.lock().unwrap().len();
        let has_windows = app_handle.webview_windows().len() > 0;
        if wiki_count == 0 && !has_windows {
            eprintln!("[TiddlyDesktop] No more wikis or windows, exiting");
            app_handle.exit(0);
        }
    });

    // Create the wiki entry
    let entry = WikiEntry {
        path: path.clone(),
        filename: folder_name,
        display_path: Some(path),
        favicon,
        is_folder: true,
        backups_enabled: false, // Not applicable for folder wikis (they use autosave)
        backup_dir: None,
        backup_count: None,
        group: None,
        sync_enabled: false,
        sync_id: None,
        sync_peers: vec![],
        relay_room: None,
    };

    // Add to recent files list
    let _ = wiki_storage::add_to_recent_files(&app, entry.clone());

    // Return the wiki entry so frontend can update its list
    Ok(entry)
}

/// Android implementation of open_wiki_folder
/// WikiActivity starts its own HTTP server in the :wiki process, independent of Tauri.
///
/// For folder wikis, Node.js renders the initial HTML, which is saved to a temp file.
/// WikiActivity loads this HTML and handles TiddlyWeb protocol for ongoing edits.
///
/// Each folder wiki opens in a separate WikiActivity (visible in recent apps)
#[cfg(target_os = "android")]
#[tauri::command]
async fn open_wiki_folder(app: tauri::AppHandle, path: String, _tiddler_title: Option<String>) -> Result<WikiEntry, String> {
    // Run the entire folder wiki opening on a blocking thread so it doesn't
    // hold up the Tauri async runtime (Node.js server startup polls for up to 5s).
    // This allows other commands (like open_wiki_window) to run concurrently.
    let app_clone = app.clone();
    let path_clone = path.clone();
    tokio::task::spawn_blocking(move || {
        open_wiki_folder_blocking(app_clone, path_clone)
    })
    .await
    .map_err(|e| format!("Task join error: {}", e))?
}

#[cfg(target_os = "android")]
fn open_wiki_folder_blocking(app: tauri::AppHandle, path: String) -> Result<WikiEntry, String> {
    // Verify this is a valid wiki folder (has tiddlywiki.info)
    let is_saf_uri = path.starts_with("content://") || path.starts_with('{');

    if is_saf_uri {
        // Check for tiddlywiki.info via SAF
        if android::saf::find_in_directory(&path, "tiddlywiki.info")?.is_none() {
            return Err("Not a valid wiki folder (missing tiddlywiki.info). Use edition selector to initialize.".to_string());
        }
    } else {
        // Filesystem path - validate
        let path_buf = PathBuf::from(&path);
        if !path_buf.join("tiddlywiki.info").exists() {
            return Err("Not a valid wiki folder (missing tiddlywiki.info). Use edition selector to initialize.".to_string());
        }
    }

    // Get a display name for the wiki
    let wiki_name = if is_saf_uri {
        android::saf::get_display_name(&path).unwrap_or_else(|_| "Wiki Folder".to_string())
    } else {
        PathBuf::from(&path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Wiki Folder")
            .to_string()
    };

    // For SAF URIs: copy to local storage in this process (has AppHandle for SAF access),
    // then pass the local path to WikiActivity so it can start Node.js from it.
    // The sync watcher also runs here (needs SAF access for writing back).
    // For filesystem paths: pass directly, WikiActivity starts Node.js from it.
    let folder_local_path = if is_saf_uri {
        eprintln!("[TiddlyDesktop] Copying SAF wiki to local storage...");
        let local_path = android::node_bridge::copy_saf_wiki_to_local(&path)?;

        // Prepare tiddlywiki.info for Android
        let info_path = PathBuf::from(&local_path).join("tiddlywiki.info");
        if info_path.exists() {
            if let Err(e) = android::node_bridge::prepare_info_for_android(&info_path) {
                eprintln!("[TiddlyDesktop] Warning: Failed to prepare tiddlywiki.info: {}", e);
            }
        }

        // Track the mapping for cleanup (sync watcher now runs in WikiActivity/Kotlin)
        android::node_bridge::track_wiki_local_path(&path, &local_path);
        eprintln!("[TiddlyDesktop] SAF wiki copied to: {}", local_path);
        Some(local_path)
    } else {
        None
    };

    // Node.js server is started in the :wiki process (WikiActivity.onCreate)
    // so it's protected by the foreground service from the start.
    // Duplicate wiki check is handled by try_bring_wiki_to_front() inside launch_wiki_activity().
    android::wiki_activity::launch_wiki_activity(
        &path,
        &wiki_name,
        true, // is_folder
        None, // No server URL — WikiActivity starts the server itself
        false, // backups not applicable for folder wikis
        0, // backup_count not applicable
        folder_local_path.as_deref(), // Local path for SAF wikis (Node.js reads from here)
        None, // No backup dir for folder wikis
        None, // No tiddler title navigation
    )?;

    // Create wiki entry for the recent files list
    let entry = WikiEntry {
        path: path.clone(),
        filename: wiki_name.clone(),
        display_path: Some(android::saf::get_display_path(&path)),
        favicon: None,
        backups_enabled: false, // Not applicable for folder wikis (autosave to tiddler files)
        backup_dir: None,
        backup_count: None,
        group: None,
        sync_enabled: false,
        sync_id: None,
        sync_peers: vec![],
        relay_room: None,
        is_folder: true,
    };

    // Add to recent files
    let _ = wiki_storage::add_to_recent_files(&app, entry.clone());

    Ok(entry)
}

/// Check if a path is a wiki folder
#[tauri::command]
fn check_is_wiki_folder(_app: tauri::AppHandle, path: String) -> Result<bool, String> {
    // Security: Validate path is a user-accessible directory
    let path_buf = drag_drop::sanitize::validate_user_directory_path(&path)?;
    Ok(utils::is_wiki_folder(&path_buf))
}

/// Get list of available TiddlyWiki editions
#[tauri::command]
async fn get_available_editions(app: tauri::AppHandle) -> Result<Vec<EditionInfo>, String> {
    let tw_path = get_tiddlywiki_path(&app)?;
    let bundled_editions_dir = tw_path.parent()
        .ok_or("Failed to get TiddlyWiki directory")?
        .join("editions");

    if !bundled_editions_dir.exists() {
        return Err("Editions directory not found".to_string());
    }

    // Get user editions directory and create it if it doesn't exist
    let user_editions_dir = get_user_editions_dir(&app)?;
    if !user_editions_dir.exists() {
        let _ = std::fs::create_dir_all(&user_editions_dir);
    }

    // Common editions with friendly names and descriptions
    let edition_metadata: std::collections::HashMap<&str, (&str, &str)> = [
        ("server", ("Server", "Basic Node.js server wiki - recommended for most users")),
        ("empty", ("Empty", "Minimal empty wiki with no content")),
        ("full", ("Full", "Full-featured wiki with many plugins")),
        ("dev", ("Developer", "Development edition with extra tools")),
        ("tw5.com", ("TW5 Documentation", "Full TiddlyWiki documentation")),
        ("introduction", ("Introduction", "Introduction and tutorial content")),
        ("prerelease", ("Prerelease", "Latest prerelease features")),
    ].iter().cloned().collect();

    // Editions to skip (test/internal editions)
    let skip_editions = ["test", "testcommonjs", "pluginlibrary", "tiddlydesktop-rs"];

    // Helper to read editions from a directory
    let read_editions_from_dir = |dir: &PathBuf, is_user_edition: bool, skip_ids: &[&str]| -> Vec<EditionInfo> {
        if !dir.exists() {
            return Vec::new();
        }
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if !path.is_dir() {
                    return None;
                }
                let name = path.file_name()?.to_str()?;

                // Skip if in skip list
                if skip_ids.contains(&name) {
                    return None;
                }
                // Skip if no tiddlywiki.info
                if !path.join("tiddlywiki.info").exists() {
                    return None;
                }

                let (display_name, description) = edition_metadata
                    .get(name)
                    .map(|(n, d)| (n.to_string(), d.to_string()))
                    .unwrap_or_else(|| {
                        (name.replace('-', " ").replace('_', " "), format!("{} edition", name))
                    });

                Some(EditionInfo {
                    id: name.to_string(),
                    name: display_name,
                    description,
                    is_user_edition,
                })
            })
            .collect()
    };

    let mut editions = Vec::new();

    // First add the common/recommended built-in editions in order
    let priority_editions = ["server", "empty", "full", "dev"];
    for edition_id in &priority_editions {
        let edition_path = bundled_editions_dir.join(edition_id);
        if edition_path.exists() && edition_path.join("tiddlywiki.info").exists() {
            let (name, desc) = edition_metadata
                .get(*edition_id)
                .map(|(n, d)| (n.to_string(), d.to_string()))
                .unwrap_or_else(|| {
                    (edition_id.replace('-', " ").replace('_', " "), format!("{} edition", edition_id))
                });
            editions.push(EditionInfo {
                id: edition_id.to_string(),
                name,
                description: desc,
                is_user_edition: false,
            });
        }
    }

    // Then add user editions (sorted alphabetically)
    let mut user_editions = read_editions_from_dir(&user_editions_dir, true, &skip_editions);
    user_editions.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    let user_edition_ids: Vec<String> = user_editions.iter().map(|e| e.id.clone()).collect();
    editions.extend(user_editions);

    // Then add other built-in editions alphabetically (excluding priority and user editions with same id)
    let mut skip_for_builtin: Vec<&str> = skip_editions.to_vec();
    skip_for_builtin.extend(priority_editions.iter());
    for id in &user_edition_ids {
        skip_for_builtin.push(id.as_str());
    }
    let mut other_builtin = read_editions_from_dir(&bundled_editions_dir, false, &skip_for_builtin);
    other_builtin.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    editions.extend(other_builtin);

    // On Android, also scan custom editions synced from SAF
    #[cfg(target_os = "android")]
    {
        let settings = wiki_storage::load_app_settings(&app).unwrap_or_default();
        if let Some(ref uri) = settings.custom_edition_path_uri {
            match android::node_bridge::sync_custom_editions_from_saf(uri) {
                Ok(local_path) => {
                    let custom_dir = PathBuf::from(&local_path);
                    // Collect already-known edition IDs to avoid duplicates
                    let existing_ids: Vec<String> = editions.iter().map(|e| e.id.clone()).collect();
                    let skip_custom: Vec<&str> = existing_ids.iter().map(|s| s.as_str()).collect();
                    let mut custom_editions = read_editions_from_dir(&custom_dir, true, &skip_custom);
                    custom_editions.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
                    eprintln!("[TiddlyDesktop] Found {} custom editions from SAF", custom_editions.len());
                    editions.extend(custom_editions);
                }
                Err(e) => {
                    eprintln!("[TiddlyDesktop] Warning: Failed to sync custom editions: {}", e);
                }
            }
        }
    }

    println!("Editions: {} total ({} user editions from {:?})", editions.len(), user_edition_ids.len(), user_editions_dir);

    Ok(editions)
}

/// Get list of available TiddlyWiki plugins
#[tauri::command]
async fn get_available_plugins(app: tauri::AppHandle) -> Result<Vec<PluginInfo>, String> {
    let tw_path = get_tiddlywiki_path(&app)?;
    let plugins_dir = tw_path.parent()
        .ok_or("Failed to get TiddlyWiki directory")?
        .join("plugins")
        .join("tiddlywiki");

    if !plugins_dir.exists() {
        return Err("Plugins directory not found".to_string());
    }

    let mut plugins = Vec::new();

    // Categories for organizing plugins
    let editor_plugins = ["codemirror", "codemirror-autocomplete", "codemirror-closebrackets",
        "codemirror-closetag", "codemirror-mode-css", "codemirror-mode-javascript",
        "codemirror-mode-markdown", "codemirror-mode-xml", "codemirror-search-replace"];
    let utility_plugins = ["markdown", "highlight", "katex", "jszip", "xlsx-utils", "qrcode", "innerwiki", "tiddlydesktop-rs-commands"];
    let storage_plugins = ["browser-storage", "filesystem", "tiddlyweb"];

    if let Ok(entries) = std::fs::read_dir(&plugins_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let plugin_info_path = path.join("plugin.info");
                if plugin_info_path.exists() {
                    if let Ok(content) = std::fs::read_to_string(&plugin_info_path) {
                        if let Ok(info) = serde_json::from_str::<serde_json::Value>(&content) {
                            let short_id = path.file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("")
                                .to_string();
                            // Use tiddlywiki/{name} format to match tiddlywiki.info convention
                            let id = format!("tiddlywiki/{}", short_id);

                            // Skip internal/core plugins
                            if short_id == "tiddlyweb" || short_id == "filesystem" || short_id == "tiddlydesktop-rs" || short_id.starts_with("test") {
                                continue;
                            }

                            let name = info.get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or(&short_id)
                                .to_string();

                            let description = info.get("description")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();

                            // Determine category
                            let category = if editor_plugins.iter().any(|p| short_id.starts_with(p)) {
                                "Editor"
                            } else if utility_plugins.contains(&short_id.as_str()) {
                                "Utility"
                            } else if storage_plugins.contains(&short_id.as_str()) {
                                "Storage"
                            } else {
                                "Other"
                            }.to_string();

                            plugins.push(PluginInfo {
                                id,
                                name,
                                description,
                                category,
                            });
                        }
                    }
                }
            }
        }
    }

    // Scan user-installed plugins from {data_dir}/plugins/ (two-level: {author}/{name}/plugin.info)
    {
        if let Ok(data_dir) = get_data_dir(&app) {
            let user_plugins_dir = data_dir.join("plugins");
            let existing_ids: std::collections::HashSet<String> = plugins.iter().map(|p| p.id.clone()).collect();
            if user_plugins_dir.exists() {
                if let Ok(author_entries) = std::fs::read_dir(&user_plugins_dir) {
                    for author_entry in author_entries.flatten() {
                        let author_path = author_entry.path();
                        if !author_path.is_dir() { continue; }
                        let author_name = author_path.file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("")
                            .to_string();
                        if let Ok(plugin_entries) = std::fs::read_dir(&author_path) {
                            for plugin_entry in plugin_entries.flatten() {
                                let plugin_path = plugin_entry.path();
                                if !plugin_path.is_dir() { continue; }
                                let plugin_info_path = plugin_path.join("plugin.info");
                                if !plugin_info_path.exists() { continue; }
                                let plugin_name = plugin_path.file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("")
                                    .to_string();
                                let id = format!("{}/{}", author_name, plugin_name);
                                if existing_ids.contains(&plugin_name) || existing_ids.contains(&id) {
                                    continue;
                                }
                                if let Ok(content) = std::fs::read_to_string(&plugin_info_path) {
                                    if let Ok(info) = serde_json::from_str::<serde_json::Value>(&content) {
                                        let name = info.get("name")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or(&plugin_name)
                                            .to_string();
                                        let description = info.get("description")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        plugins.push(PluginInfo {
                                            id,
                                            name,
                                            description,
                                            category: "Custom".to_string(),
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // On Android, also scan custom plugins synced from SAF
    #[cfg(target_os = "android")]
    {
        let settings = wiki_storage::load_app_settings(&app).unwrap_or_default();
        if let Some(ref uri) = settings.custom_plugin_path_uri {
            match android::node_bridge::sync_custom_plugins_from_saf(uri) {
                Ok(local_path) => {
                    let custom_dir = PathBuf::from(&local_path);
                    // Scan two-level deep: {custom_plugins}/{author}/{name}/plugin.info
                    let existing_ids: std::collections::HashSet<String> = plugins.iter().map(|p| p.id.clone()).collect();
                    if let Ok(author_entries) = std::fs::read_dir(&custom_dir) {
                        for author_entry in author_entries.flatten() {
                            let author_path = author_entry.path();
                            if !author_path.is_dir() { continue; }
                            let author_name = author_path.file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("")
                                .to_string();

                            if let Ok(plugin_entries) = std::fs::read_dir(&author_path) {
                                for plugin_entry in plugin_entries.flatten() {
                                    let plugin_path = plugin_entry.path();
                                    if !plugin_path.is_dir() { continue; }
                                    let plugin_info_path = plugin_path.join("plugin.info");
                                    if !plugin_info_path.exists() { continue; }

                                    let plugin_name = plugin_path.file_name()
                                        .and_then(|n| n.to_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let id = format!("{}/{}", author_name, plugin_name);

                                    if existing_ids.contains(&plugin_name) || existing_ids.contains(&id) {
                                        continue;
                                    }

                                    if let Ok(content) = std::fs::read_to_string(&plugin_info_path) {
                                        if let Ok(info) = serde_json::from_str::<serde_json::Value>(&content) {
                                            let name = info.get("name")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or(&plugin_name)
                                                .to_string();
                                            let description = info.get("description")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("")
                                                .to_string();

                                            plugins.push(PluginInfo {
                                                id,
                                                name,
                                                description,
                                                category: "Custom".to_string(),
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    }
                    eprintln!("[TiddlyDesktop] Scanned custom plugins from SAF");
                }
                Err(e) => {
                    eprintln!("[TiddlyDesktop] Warning: Failed to sync custom plugins: {}", e);
                }
            }
        }
    }

    // Sort by category, then by name
    plugins.sort_by(|a, b| {
        let cat_order = |c: &str| match c {
            "Editor" => 0,
            "Utility" => 1,
            "Storage" => 2,
            "Custom" => 3,
            _ => 4,
        };
        cat_order(&a.category).cmp(&cat_order(&b.category))
            .then_with(|| a.name.cmp(&b.name))
    });

    Ok(plugins)
}

/// Get the list of plugins currently installed in a wiki
#[tauri::command]
async fn get_wiki_installed_plugins(app: tauri::AppHandle, path: String, is_folder: bool) -> Result<Vec<String>, String> {
    if is_folder {
        // For folder wikis: read tiddlywiki.info plugins array + scan tiddlers/ for plugin tiddler files
        // On Android, folder wiki paths are SAF JSON URIs — resolve to local mirror
        #[cfg(target_os = "android")]
        let local_path = android::node_bridge::get_or_create_local_copy(&path)?;
        #[cfg(not(target_os = "android"))]
        let local_path = path.clone();
        let path_buf = PathBuf::from(&local_path);
        let mut installed = Vec::new();

        // 1. Read tiddlywiki.info plugins array
        let info_path = path_buf.join("tiddlywiki.info");
        if info_path.exists() {
            let content = std::fs::read_to_string(&info_path)
                .map_err(|e| format!("Failed to read tiddlywiki.info: {}", e))?;
            if let Ok(info) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(plugins) = info.get("plugins").and_then(|v| v.as_array()) {
                    for p in plugins {
                        if let Some(name) = p.as_str() {
                            installed.push(name.to_string());
                        }
                    }
                }
            }
        }

        // 2. Scan tiddlers/ for drag-and-dropped plugin tiddler files
        let tiddlers_dir = path_buf.join("tiddlers");
        if tiddlers_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&tiddlers_dir) {
                for entry in entries.flatten() {
                    let file_path = entry.path();
                    if file_path.is_file() {
                        let ext = file_path.extension().and_then(|e| e.to_str()).unwrap_or("");
                        match ext {
                            "tid" => {
                                // Read header lines of .tid file looking for plugin-type and title
                                if let Ok(content) = std::fs::read_to_string(&file_path) {
                                    let mut has_plugin_type = false;
                                    let mut title = None;
                                    for line in content.lines() {
                                        if line.is_empty() { break; } // End of headers
                                        if line.starts_with("plugin-type: plugin") {
                                            has_plugin_type = true;
                                        }
                                        if let Some(t) = line.strip_prefix("title: ") {
                                            title = Some(t.to_string());
                                        }
                                    }
                                    if has_plugin_type {
                                        if let Some(t) = title {
                                            let id = t.strip_prefix("$:/plugins/").unwrap_or(&t);
                                            if !installed.contains(&id.to_string()) {
                                                installed.push(id.to_string());
                                            }
                                        }
                                    }
                                }
                            }
                            "json" => {
                                // Parse JSON looking for plugin-type field
                                if let Ok(content) = std::fs::read_to_string(&file_path) {
                                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                                        if json.get("plugin-type").and_then(|v| v.as_str()) == Some("plugin") {
                                            if let Some(t) = json.get("title").and_then(|v| v.as_str()) {
                                                let id = t.strip_prefix("$:/plugins/").unwrap_or(t);
                                                if !installed.contains(&id.to_string()) {
                                                    installed.push(id.to_string());
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        Ok(installed)
    } else {
        // For single-file wikis: use Node.js to extract plugin list
        let temp_dir = std::env::temp_dir().join(format!("tiddlydesktop-plugins-{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir)
            .map_err(|e| format!("Failed to create temp directory: {}", e))?;
        // Create empty tiddlers dir
        let _ = std::fs::create_dir_all(temp_dir.join("tiddlers"));
        // Write minimal tiddlywiki.info
        std::fs::write(temp_dir.join("tiddlywiki.info"), r#"{"plugins":[]}"#)
            .map_err(|e| format!("Failed to write tiddlywiki.info: {}", e))?;

        let temp_dir_str = temp_dir.to_string_lossy().to_string();

        #[cfg(target_os = "android")]
        let run_result = {
            // On Android, wiki path is a SAF JSON URI — copy to local temp file first
            let wiki_uri: serde_json::Value = serde_json::from_str(&path)
                .map_err(|e| format!("Failed to parse wiki path JSON: {}", e))?;
            let uri = wiki_uri.get("uri").and_then(|v| v.as_str())
                .ok_or("Missing 'uri' in wiki path JSON")?;
            let wiki_bytes = android::saf::read_document_bytes(uri)?;
            let local_wiki = temp_dir.join("source_wiki.html");
            std::fs::write(&local_wiki, &wiki_bytes)
                .map_err(|e| format!("Failed to write local wiki copy: {}", e))?;
            let local_wiki_str = local_wiki.to_string_lossy().to_string();

            let args: Vec<&str> = vec![
                &temp_dir_str,
                "--load", &local_wiki_str,
                "--output", &temp_dir_str,
                "--render", "$:/core/save/all", "plugins.txt", "text/plain",
                "$:/core/templates/plain-text-tiddler",
                "[plugin-type[plugin]removeprefix[$:/plugins/]]",
            ];
            android::node_bridge::run_tiddlywiki_command(&args)
        };

        #[cfg(not(target_os = "android"))]
        let run_result = {
            let node_path = get_node_path(&app)?;
            let tw_path = get_tiddlywiki_path(&app)?;
            let tw_dir = tw_path.parent().ok_or("Failed to get TiddlyWiki directory")?;

            let mut cmd = Command::new(&node_path);
            cmd.arg(&tw_path)
                .arg(&temp_dir)
                .arg("--load")
                .arg(&path)
                .arg("--output")
                .arg(&temp_dir)
                .arg("--render")
                .arg("$:/core/save/all")
                .arg("plugins.txt")
                .arg("text/plain")
                .arg("$:/core/templates/plain-text-tiddler")
                .arg("[plugin-type[plugin]removeprefix[$:/plugins/]]")
                .current_dir(tw_dir);
            #[cfg(target_os = "windows")]
            cmd.creation_flags(CREATE_NO_WINDOW);

            let output = cmd.output()
                .map_err(|e| format!("Failed to run TiddlyWiki: {}", e))?;
            if output.status.success() {
                Ok(String::from_utf8_lossy(&output.stdout).to_string())
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                Err(format!("Failed to extract plugins: {}", stderr))
            }
        };

        let result = match run_result {
            Ok(_) => {
                let plugins_file = temp_dir.join("plugins.txt");
                if plugins_file.exists() {
                    let content = std::fs::read_to_string(&plugins_file)
                        .map_err(|e| format!("Failed to read plugins output: {}", e))?;
                    Ok(content.lines()
                        .map(|l| l.trim())
                        .filter(|l| !l.is_empty())
                        .map(|l| l.to_string())
                        .collect())
                } else {
                    Ok(Vec::new())
                }
            }
            Err(e) => Err(e),
        };

        let _ = std::fs::remove_dir_all(&temp_dir);
        result
    }
}

/// Install/update plugins in an existing wiki
#[tauri::command]
async fn install_plugins_to_wiki(app: tauri::AppHandle, path: String, is_folder: bool, plugins: Vec<String>) -> Result<(), String> {
    if is_folder {
        // For folder wikis: update tiddlywiki.info plugins array
        // On Android, folder wiki paths are SAF JSON URIs — resolve to local mirror
        #[cfg(target_os = "android")]
        let local_path = android::node_bridge::get_or_create_local_copy(&path)?;
        #[cfg(not(target_os = "android"))]
        let local_path = path.clone();
        let path_buf = PathBuf::from(&local_path);

        let info_path = path_buf.join("tiddlywiki.info");
        if !info_path.exists() {
            return Err("tiddlywiki.info not found in wiki folder".to_string());
        }
        let content = std::fs::read_to_string(&info_path)
            .map_err(|e| format!("Failed to read tiddlywiki.info: {}", e))?;
        let mut info: serde_json::Value = serde_json::from_str(&content)
            .map_err(|e| format!("Failed to parse tiddlywiki.info: {}", e))?;

        // Build new plugins array, keeping tiddlyweb and filesystem if they were present
        let old_plugins: Vec<String> = info.get("plugins")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();

        let mut new_plugins: Vec<String> = plugins.clone();
        // Ensure server plugins are preserved for folder wikis
        for server_plugin in &["tiddlywiki/tiddlyweb", "tiddlywiki/filesystem"] {
            if old_plugins.contains(&server_plugin.to_string()) && !new_plugins.contains(&server_plugin.to_string()) {
                new_plugins.push(server_plugin.to_string());
            }
        }

        info["plugins"] = serde_json::Value::Array(
            new_plugins.iter().map(|p| serde_json::Value::String(p.clone())).collect()
        );

        let updated = serde_json::to_string_pretty(&info)
            .map_err(|e| format!("Failed to serialize tiddlywiki.info: {}", e))?;
        std::fs::write(&info_path, &updated)
            .map_err(|e| format!("Failed to write tiddlywiki.info: {}", e))?;

        // On Android, sync the updated tiddlywiki.info back to SAF
        #[cfg(target_os = "android")]
        {
            // Find tiddlywiki.info in the SAF folder and write it back
            if let Ok(Some(info_uri)) = android::saf::find_in_directory(&path, "tiddlywiki.info") {
                android::saf::write_document_string(&info_uri, &updated)?;
            }
        }

        Ok(())
    } else {
        // For single-file wikis: rebuild with new plugin set via Node.js
        // Create temp directory for the rebuild
        let temp_dir = std::env::temp_dir().join(format!("tiddlydesktop-rebuild-{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir)
            .map_err(|e| format!("Failed to create temp directory: {}", e))?;
        let _ = std::fs::create_dir_all(temp_dir.join("tiddlers"));

        // Write tiddlywiki.info with the desired plugins
        let info = serde_json::json!({
            "plugins": plugins
        });
        std::fs::write(temp_dir.join("tiddlywiki.info"), serde_json::to_string_pretty(&info).unwrap())
            .map_err(|e| format!("Failed to write tiddlywiki.info: {}", e))?;

        // Strip server plugins
        strip_server_plugins_from_info(&temp_dir.join("tiddlywiki.info"))?;

        let temp_dir_str = temp_dir.to_string_lossy().to_string();
        let output_dir = temp_dir.join("output");
        let output_dir_str = output_dir.to_string_lossy().to_string();
        let output_filename = "wiki.html";

        #[cfg(target_os = "android")]
        let build_result = {
            // On Android, wiki path is a SAF JSON URI — copy to local temp file first
            let wiki_uri: serde_json::Value = serde_json::from_str(&path)
                .map_err(|e| format!("Failed to parse wiki path JSON: {}", e))?;
            let uri = wiki_uri.get("uri").and_then(|v| v.as_str())
                .ok_or("Missing 'uri' in wiki path JSON")?.to_string();
            let wiki_bytes = android::saf::read_document_bytes(&uri)?;
            let local_wiki = temp_dir.join("source_wiki.html");
            std::fs::write(&local_wiki, &wiki_bytes)
                .map_err(|e| format!("Failed to write local wiki copy: {}", e))?;
            let local_wiki_str = local_wiki.to_string_lossy().to_string();

            let args: Vec<&str> = vec![
                &temp_dir_str,
                "--load", &local_wiki_str,
                "--output", &output_dir_str,
                "--render", "$:/core/save/all", output_filename, "text/plain",
            ];
            let result = android::node_bridge::run_tiddlywiki_command(&args);

            // On success, write rebuilt wiki back to SAF
            if result.is_ok() {
                let built_file = output_dir.join(output_filename);
                if built_file.exists() {
                    let built_bytes = std::fs::read(&built_file)
                        .map_err(|e| format!("Failed to read rebuilt wiki: {}", e))?;
                    android::saf::write_document_bytes(&uri, &built_bytes)?;
                }
            }
            result
        };

        #[cfg(not(target_os = "android"))]
        let build_result = {
            let wiki_path = PathBuf::from(&path);
            if !wiki_path.exists() {
                let _ = std::fs::remove_dir_all(&temp_dir);
                return Err("Wiki file not found".to_string());
            }

            // Create backup before rebuilding
            {
                let entries = wiki_storage::load_recent_files_from_disk(&app);
                let entry = entries.iter().find(|e| utils::paths_equal(&e.path, &path));
                let backups_enabled = entry.map(|e| e.backups_enabled).unwrap_or(true);
                if backups_enabled {
                    let backup_dir = get_wiki_backup_dir(&app, &path)
                        .map(PathBuf::from)
                        .unwrap_or_else(|| {
                            wiki_path.parent().unwrap_or(std::path::Path::new(".")).join("backups")
                        });
                    let _ = std::fs::create_dir_all(&backup_dir);
                    let filename = wiki_path.file_name().unwrap_or_default().to_string_lossy();
                    let backup_name = format!("{}.plugin-backup.{}", filename, chrono::Local::now().format("%Y%m%d-%H%M%S"));
                    let _ = std::fs::copy(&wiki_path, backup_dir.join(&backup_name));
                }
            }

            let wiki_path_str = wiki_path.to_string_lossy().to_string();
            let node_path = get_node_path(&app)?;
            let tw_path = get_tiddlywiki_path(&app)?;
            let tw_dir = tw_path.parent().ok_or("Failed to get TiddlyWiki directory")?;

            // Use original filename for desktop output
            let desktop_output_filename = wiki_path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("wiki.html");

            let mut build_cmd = Command::new(&node_path);
            build_cmd.arg(&tw_path)
                .arg(&temp_dir)
                .arg("--load")
                .arg(&wiki_path)
                .arg("--output")
                .arg(&output_dir)
                .arg("--render")
                .arg("$:/core/save/all")
                .arg(desktop_output_filename)
                .arg("text/plain")
                .current_dir(tw_dir);

            // Set TIDDLYWIKI_PLUGIN_PATH for user-installed plugins
            if let Ok(data_dir) = get_data_dir(&app) {
                let sep = if cfg!(windows) { ";" } else { ":" };
                let plugins_dir = data_dir.join("plugins");
                let _ = std::fs::create_dir_all(&plugins_dir);
                let mut paths = vec![plugins_dir.to_string_lossy().to_string()];
                if let Ok(existing) = std::env::var("TIDDLYWIKI_PLUGIN_PATH") {
                    if !existing.is_empty() {
                        paths.push(existing);
                    }
                }
                build_cmd.env("TIDDLYWIKI_PLUGIN_PATH", paths.join(sep));
            }

            #[cfg(target_os = "windows")]
            build_cmd.creation_flags(CREATE_NO_WINDOW);

            let build_output = build_cmd.output()
                .map_err(|e| format!("Failed to rebuild wiki: {}", e))?;
            if build_output.status.success() {
                // Copy output back over original
                let built_file = output_dir.join(desktop_output_filename);
                if built_file.exists() {
                    std::fs::copy(&built_file, &wiki_path)
                        .map_err(|e| format!("Failed to copy rebuilt wiki: {}", e))?;
                }
                Ok(String::new())
            } else {
                let stderr = String::from_utf8_lossy(&build_output.stderr);
                let stdout = String::from_utf8_lossy(&build_output.stdout);
                Err(format!("Wiki rebuild failed:\n{}\n{}", stdout, stderr))
            }
        };

        if let Err(e) = build_result {
            let _ = std::fs::remove_dir_all(&temp_dir);
            return Err(e);
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
        Ok(())
    }
}

/// Initialize a new wiki folder with the specified edition and plugins
#[cfg(not(target_os = "android"))]
#[tauri::command]
async fn init_wiki_folder(app: tauri::AppHandle, path: String, edition: String, plugins: Vec<String>) -> Result<(), String> {
    // Security: Validate path is safe and within user directories
    if drag_drop::sanitize::validate_file_path(&path).is_none() {
        return Err("Invalid path".to_string());
    }

    let path_buf = PathBuf::from(&path);

    if !path_buf.is_absolute() {
        return Err("Path must be absolute".to_string());
    }

    // Security: Check path is in user-accessible location
    if !drag_drop::sanitize::is_user_accessible_path(&path_buf) {
        return Err("Access to system directories is not allowed".to_string());
    }

    // Verify the folder exists
    if !path_buf.exists() {
        std::fs::create_dir_all(&path_buf)
            .map_err(|e| format!("Failed to create directory: {}", e))?;
    }

    // Check if already initialized
    if path_buf.join("tiddlywiki.info").exists() {
        return Err("Folder already contains a TiddlyWiki".to_string());
    }

    println!("Initializing wiki folder:");
    println!("  Target folder: {:?}", path_buf);
    println!("  Edition: {}", edition);
    println!("  Additional plugins: {:?}", plugins);

    // Use Node.js + TiddlyWiki to initialize the wiki
    let node_path = get_node_path(&app)?;
    let tw_path = get_tiddlywiki_path(&app)?;

    println!("  Node.js: {:?}", node_path);
    println!("  TiddlyWiki: {:?}", tw_path);

    // Run tiddlywiki --init <edition>
    let mut cmd = Command::new(&node_path);
    cmd.arg(&tw_path)
        .arg(&path_buf)
        .arg("--init")
        .arg(&edition);
    // Set TIDDLYWIKI_EDITION_PATH so TiddlyWiki can find user editions
    // Include our {app_data}/editions dir and any existing global TIDDLYWIKI_EDITION_PATH
    {
        let user_editions_dir = get_user_editions_dir(&app)?;
        let sep = if cfg!(windows) { ";" } else { ":" };
        let edition_path = match std::env::var("TIDDLYWIKI_EDITION_PATH") {
            Ok(existing) if !existing.is_empty() => {
                format!("{}{}{}", user_editions_dir.display(), sep, existing)
            }
            _ => user_editions_dir.to_string_lossy().to_string(),
        };
        cmd.env("TIDDLYWIKI_EDITION_PATH", &edition_path);
    }
    // Set TIDDLYWIKI_PLUGIN_PATH for user-installed plugins
    if let Ok(data_dir) = get_data_dir(&app) {
        let plugins_dir = data_dir.join("plugins");
        let _ = std::fs::create_dir_all(&plugins_dir);
        let sep = if cfg!(windows) { ";" } else { ":" };
        let mut paths = vec![plugins_dir.to_string_lossy().to_string()];
        if let Ok(existing) = std::env::var("TIDDLYWIKI_PLUGIN_PATH") {
            if !existing.is_empty() {
                paths.push(existing);
            }
        }
        cmd.env("TIDDLYWIKI_PLUGIN_PATH", paths.join(sep));
    }
    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let output = cmd.output()
        .map_err(|e| format!("Failed to run TiddlyWiki init: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!("TiddlyWiki init failed:\n{}\n{}", stdout, stderr));
    }

    // Verify initialization succeeded
    let info_path = path_buf.join("tiddlywiki.info");
    if !info_path.exists() {
        return Err("Initialization failed - tiddlywiki.info not created".to_string());
    }

    // Always ensure required plugins for server are present
    // Plus any additional user-selected plugins
    let required_plugins = vec!["tiddlywiki/tiddlyweb", "tiddlywiki/filesystem"];

    let content = std::fs::read_to_string(&info_path)
        .map_err(|e| format!("Failed to read tiddlywiki.info: {}", e))?;

    let mut info: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse tiddlywiki.info: {}", e))?;

    // Get or create plugins array
    let plugins_array = info.get_mut("plugins")
        .and_then(|v| v.as_array_mut());

    if let Some(arr) = plugins_array {
        // Add required plugins first
        for plugin_path in &required_plugins {
            if !arr.iter().any(|p| p.as_str() == Some(*plugin_path)) {
                arr.push(serde_json::Value::String(plugin_path.to_string()));
            }
        }
        // Add user-selected plugins
        for plugin in &plugins {
            let plugin_path = format!("tiddlywiki/{}", plugin);
            if !arr.iter().any(|p| p.as_str() == Some(&plugin_path)) {
                arr.push(serde_json::Value::String(plugin_path));
            }
        }
    } else {
        // Create new plugins array with required + user plugins
        let mut all_plugins: Vec<serde_json::Value> = required_plugins.iter()
            .map(|p| serde_json::Value::String(p.to_string()))
            .collect();
        for plugin in &plugins {
            all_plugins.push(serde_json::Value::String(format!("tiddlywiki/{}", plugin)));
        }
        info["plugins"] = serde_json::Value::Array(all_plugins);
    }

    // Write back
    let updated_content = serde_json::to_string_pretty(&info)
        .map_err(|e| format!("Failed to serialize tiddlywiki.info: {}", e))?;
    std::fs::write(&info_path, updated_content)
        .map_err(|e| format!("Failed to write tiddlywiki.info: {}", e))?;

    println!("Ensured tiddlyweb and filesystem plugins are present");

    // Create tiddlers folder if it doesn't exist
    let tiddlers_dir = path_buf.join("tiddlers");
    if !tiddlers_dir.exists() {
        std::fs::create_dir_all(&tiddlers_dir)
            .map_err(|e| format!("Failed to create tiddlers directory: {}", e))?;
    }

    // Enable autosave by creating the config tiddler
    let autosave_tiddler = tiddlers_dir.join("$__config_AutoSave.tid");
    let autosave_content = "title: $:/config/AutoSave\n\nyes";
    std::fs::write(&autosave_tiddler, autosave_content)
        .map_err(|e| format!("Failed to create autosave config: {}", e))?;

    println!("Enabled autosave for wiki folder");
    println!("Wiki folder initialized successfully");
    Ok(())
}

/// Initialize a wiki folder on Android
/// Copies edition files via SAF to create a new wiki folder structure
#[cfg(target_os = "android")]
#[tauri::command]
async fn init_wiki_folder(app: tauri::AppHandle, path: String, edition: String, plugins: Vec<String>) -> Result<WikiEntry, String> {
    use tauri::Manager;

    eprintln!("[TiddlyDesktop] Initializing wiki folder on Android:");
    eprintln!("  Path: {}", path);
    eprintln!("  Edition: {}", edition);

    // On Android, resources are extracted to app_data_dir/tiddlywiki/
    let data_dir = app.path().app_data_dir()
        .map_err(|e| format!("Failed to get app data dir: {}", e))?;

    // Look up the edition: bundled first, then user editions, then custom editions (SAF-synced)
    let bundled_edition_path = data_dir.join("tiddlywiki/editions").join(&edition);
    let user_edition_path = data_dir.join("editions").join(&edition);
    let custom_edition_path = data_dir.join("custom_editions").join(&edition);

    let edition_path = if bundled_edition_path.exists() {
        bundled_edition_path
    } else if user_edition_path.exists() {
        user_edition_path
    } else if custom_edition_path.exists() {
        custom_edition_path
    } else {
        // Try syncing custom editions from SAF in case they haven't been synced yet
        let settings = wiki_storage::load_app_settings(&app).unwrap_or_default();
        if let Some(ref uri) = settings.custom_edition_path_uri {
            let _ = android::node_bridge::sync_custom_editions_from_saf(uri);
        }
        // Re-check custom editions after sync
        if custom_edition_path.exists() {
            custom_edition_path
        } else {
            return Err(format!("Edition '{}' not found", edition));
        }
    };

    eprintln!("  Edition path: {:?}", edition_path);

    // Read tiddlywiki.info from the edition
    let tiddlywiki_info_path = edition_path.join("tiddlywiki.info");
    if !tiddlywiki_info_path.exists() {
        return Err(format!("Edition '{}' is missing tiddlywiki.info", edition));
    }

    let tiddlywiki_info = tokio::fs::read_to_string(&tiddlywiki_info_path).await
        .map_err(|e| format!("Failed to read tiddlywiki.info: {}", e))?;

    // Parse and modify tiddlywiki.info to add required server plugins
    let mut info: serde_json::Value = serde_json::from_str(&tiddlywiki_info)
        .map_err(|e| format!("Failed to parse tiddlywiki.info: {}", e))?;

    // Ensure plugins array exists and add required server plugins
    if let Some(obj) = info.as_object_mut() {
        let plugins_entry = obj.entry("plugins").or_insert(serde_json::json!([]));
        if let Some(plugins_array) = plugins_entry.as_array_mut() {
            // Add tiddlyweb and filesystem plugins if not already present
            let required_plugins = ["tiddlywiki/tiddlyweb", "tiddlywiki/filesystem"];
            for plugin in required_plugins {
                let plugin_value = serde_json::Value::String(plugin.to_string());
                if !plugins_array.contains(&plugin_value) {
                    plugins_array.push(plugin_value);
                }
            }
            // Add user-selected plugins
            for plugin in &plugins {
                let plugin_path = format!("tiddlywiki/{}", plugin);
                let plugin_value = serde_json::Value::String(plugin_path);
                if !plugins_array.contains(&plugin_value) {
                    plugins_array.push(plugin_value);
                }
            }
        }
    }

    let modified_info = serde_json::to_string_pretty(&info)
        .map_err(|e| format!("Failed to serialize tiddlywiki.info: {}", e))?;

    // Create tiddlywiki.info in the target folder
    let tiddlywiki_info_uri = android::saf::create_file(&path, "tiddlywiki.info", Some("application/json"))?;
    android::saf::write_document_string(&tiddlywiki_info_uri, &modified_info)?;
    eprintln!("  Created tiddlywiki.info with server plugins and {} user plugins", plugins.len());

    // Create tiddlers directory
    let tiddlers_uri = android::saf::find_or_create_subdirectory(&path, "tiddlers")?;
    eprintln!("  Created tiddlers directory");

    // Copy default tiddlers from the edition (if any exist)
    let edition_tiddlers_path = edition_path.join("tiddlers");
    if edition_tiddlers_path.exists() {
        if let Ok(entries) = std::fs::read_dir(&edition_tiddlers_path) {
            for entry in entries.flatten() {
                let file_name = entry.file_name();
                let file_name_str = file_name.to_string_lossy();

                // Skip directories and hidden files
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(true) {
                    continue;
                }
                if file_name_str.starts_with('.') {
                    continue;
                }

                // Read file content
                if let Ok(content) = tokio::fs::read_to_string(entry.path()).await {
                    // Determine MIME type
                    let mime_type = if file_name_str.ends_with(".tid") {
                        "text/plain"
                    } else if file_name_str.ends_with(".json") {
                        "application/json"
                    } else if file_name_str.ends_with(".meta") {
                        "text/plain"
                    } else {
                        "application/octet-stream"
                    };

                    // Create file in tiddlers directory
                    if let Ok(new_uri) = android::saf::create_file(&tiddlers_uri, &file_name_str, Some(mime_type)) {
                        let _ = android::saf::write_document_string(&new_uri, &content);
                        eprintln!("  Copied tiddler: {}", file_name_str);
                    }
                }
            }
        }
    }

    eprintln!("[TiddlyDesktop] Wiki folder initialized successfully");

    // Give SAF time to sync the newly created files
    // This is important on Android where SAF operations may be async
    std::thread::sleep(std::time::Duration::from_millis(500));

    // Now open the wiki directly
    // This avoids a race condition where SAF might not have synced the file yet
    eprintln!("[TiddlyDesktop] Opening newly created folder wiki...");

    // Get display name
    let wiki_name = android::saf::get_display_name(&path).unwrap_or_else(|_| "Wiki Folder".to_string());

    // Start Node.js TiddlyWiki server for this folder wiki
    // Since this is a newly initialized folder, we need to copy it to local for Node.js access
    // Changes are automatically synced back to SAF every 2 seconds
    eprintln!("[TiddlyDesktop] Starting Node.js TiddlyWiki server...");
    let (server_url, _local_path) = android::node_bridge::start_saf_wiki_server(&path)
        .map_err(|e| format!("Failed to start wiki server: {}. Make sure Node.js is available.", e))?;
    eprintln!("[TiddlyDesktop] Node.js server started at: {}", server_url);

    // Foreground service is now started from WikiActivity.onCreate() (same :wiki process)

    // Launch WikiActivity with the Node.js server URL
    android::wiki_activity::launch_wiki_activity(
        &path,
        &wiki_name,
        true, // is_folder
        Some(&server_url), // Node.js server URL
        false, // backups not applicable for folder wikis
        0, // backup_count not applicable
        None, // No local path needed - server already running
        None, // No backup dir for folder wikis
        None, // No tiddler title navigation
    )?;

    // Create wiki entry for the recent files list
    let entry = WikiEntry {
        path: path.clone(),
        filename: wiki_name.clone(),
        display_path: Some(android::saf::get_display_path(&path)),
        favicon: None,
        backups_enabled: false,
        backup_dir: None,
        backup_count: None,
        group: None,
        sync_enabled: false,
        sync_id: None,
        sync_peers: vec![],
        relay_room: None,
        is_folder: true,
    };

    // Add to recent files
    let _ = wiki_storage::add_to_recent_files(&app, entry.clone());

    Ok(entry)
}



/// Create symlinks so that relative includeWikis paths in tiddlywiki.info
/// resolve from a temp build directory to the bundled editions.
/// For example, if tiddlywiki.info has includeWikis: [{"path": "../tw5.com"}],
/// and the wiki is at /tmp/build-XXX/, this creates a symlink at
/// /tmp/tw5.com → <editions_dir>/tw5.com
#[cfg(not(target_os = "android"))]
fn resolve_include_wikis_paths(wiki_dir: &std::path::Path, editions_dir: &std::path::Path) {
    let info_path = wiki_dir.join("tiddlywiki.info");
    let content = match std::fs::read_to_string(&info_path) {
        Ok(c) => c,
        Err(_) => return,
    };
    let info: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return,
    };

    let include_wikis = match info.get("includeWikis").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return,
    };

    for entry in include_wikis {
        let rel_path = if let Some(s) = entry.as_str() {
            s.to_string()
        } else if let Some(p) = entry.get("path").and_then(|v| v.as_str()) {
            p.to_string()
        } else {
            continue;
        };

        // Check if the relative path already resolves
        if wiki_dir.join(&rel_path).exists() {
            continue;
        }

        // Strip leading "../" to get the edition name
        let stripped = rel_path.trim_start_matches("../").trim_start_matches("..\\");
        let top_dir = stripped.split('/').next().unwrap_or(stripped);
        let top_dir = top_dir.split('\\').next().unwrap_or(top_dir);

        let edition_path = editions_dir.join(top_dir);
        if !edition_path.exists() {
            eprintln!("[TiddlyDesktop] Warning: bundled edition '{}' not found at {:?}", top_dir, edition_path);
            continue;
        }

        // Create symlink at the location the relative path resolves to
        let link_path = match wiki_dir.parent() {
            Some(parent) => parent.join(top_dir),
            None => continue,
        };

        if link_path.exists() || link_path.symlink_metadata().is_ok() {
            continue; // Already exists
        }

        #[cfg(unix)]
        {
            if let Err(e) = std::os::unix::fs::symlink(&edition_path, &link_path) {
                eprintln!("[TiddlyDesktop] Failed to create symlink {:?} → {:?}: {}", link_path, edition_path, e);
            }
        }
        #[cfg(windows)]
        {
            if let Err(e) = std::os::windows::fs::symlink_dir(&edition_path, &link_path) {
                // Symlink may require admin on Windows; fall back to junction
                eprintln!("[TiddlyDesktop] symlink_dir failed, trying junction: {}", e);
                let _ = std::process::Command::new("cmd")
                    .args(["/C", "mklink", "/J",
                        &link_path.to_string_lossy(),
                        &edition_path.to_string_lossy()])
                    .creation_flags(CREATE_NO_WINDOW)
                    .output();
            }
        }
    }
}

/// Strip tiddlyweb and filesystem plugins from a tiddlywiki.info file.
/// These plugins are designed for client-server folder wikis and cause problems
/// in standalone single-file wikis.
fn strip_server_plugins_from_info(info_path: &std::path::Path) -> Result<(), String> {
    let content = std::fs::read_to_string(info_path)
        .map_err(|e| format!("Failed to read tiddlywiki.info: {}", e))?;
    let mut info: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse tiddlywiki.info: {}", e))?;
    if let Some(arr) = info.get_mut("plugins").and_then(|v| v.as_array_mut()) {
        arr.retain(|p| {
            let name = p.as_str().unwrap_or("");
            name != "tiddlywiki/tiddlyweb" && name != "tiddlywiki/filesystem"
        });
    }
    // Remove includeWikis — relative paths break in the temp build directory,
    // and included content (e.g. tw5.com docs) isn't needed for single-file wikis
    if let Some(obj) = info.as_object_mut() {
        obj.remove("includeWikis");
    }
    let updated = serde_json::to_string_pretty(&info)
        .map_err(|e| format!("Failed to serialize tiddlywiki.info: {}", e))?;
    std::fs::write(info_path, updated)
        .map_err(|e| format!("Failed to write tiddlywiki.info: {}", e))?;
    Ok(())
}

/// Create a single-file wiki with the specified edition and plugins
#[cfg(not(target_os = "android"))]
#[tauri::command]
async fn create_wiki_file(app: tauri::AppHandle, path: String, edition: String, plugins: Vec<String>) -> Result<(), String> {
    // Security: Validate path for writing a wiki file
    let output_path = drag_drop::sanitize::validate_wiki_path_for_write(&path)?;

    // Ensure it has .html extension
    let output_path = if output_path.extension().map(|e| e == "html" || e == "htm").unwrap_or(false) {
        output_path
    } else {
        output_path.with_extension("html")
    };

    // Security: Check path is in user-accessible location
    if !drag_drop::sanitize::is_user_accessible_path(&output_path) {
        return Err("Access to system directories is not allowed".to_string());
    }

    println!("Creating single-file wiki:");
    println!("  Output: {:?}", output_path);
    println!("  Edition: {}", edition);
    println!("  Plugins: {:?}", plugins);

    // Use Node.js to build the wiki
    let node_path = get_node_path(&app)?;
    let tw_path = get_tiddlywiki_path(&app)?;
    let tw_dir = tw_path.parent().ok_or("Failed to get TiddlyWiki directory")?;

    // Create a temporary directory for the build
    let temp_dir = std::env::temp_dir().join(format!("tiddlydesktop-build-{}", std::process::id()));
    std::fs::create_dir_all(&temp_dir)
        .map_err(|e| format!("Failed to create temp directory: {}", e))?;

    println!("  Temp dir: {:?}", temp_dir);

    // Initialize the temp directory with the selected edition
    let mut init_cmd = Command::new(&node_path);
    init_cmd.arg(&tw_path)
        .arg(&temp_dir)
        .arg("--init")
        .arg(&edition);
    // Set TIDDLYWIKI_EDITION_PATH so TiddlyWiki can find user editions
    // Include our {app_data}/editions dir and any existing global TIDDLYWIKI_EDITION_PATH
    {
        let user_editions_dir = get_user_editions_dir(&app)?;
        let sep = if cfg!(windows) { ";" } else { ":" };
        let edition_path = match std::env::var("TIDDLYWIKI_EDITION_PATH") {
            Ok(existing) if !existing.is_empty() => {
                format!("{}{}{}", user_editions_dir.display(), sep, existing)
            }
            _ => user_editions_dir.to_string_lossy().to_string(),
        };
        init_cmd.env("TIDDLYWIKI_EDITION_PATH", &edition_path);
    }
    #[cfg(target_os = "windows")]
    init_cmd.creation_flags(CREATE_NO_WINDOW);
    let init_output = init_cmd.output()
        .map_err(|e| format!("Failed to run TiddlyWiki init: {}", e))?;

    if !init_output.status.success() {
        let _ = std::fs::remove_dir_all(&temp_dir);
        let stderr = String::from_utf8_lossy(&init_output.stderr);
        return Err(format!("TiddlyWiki init failed: {}", stderr));
    }

    // Add plugins to tiddlywiki.info if any selected
    if !plugins.is_empty() {
        let info_path = temp_dir.join("tiddlywiki.info");
        if info_path.exists() {
            let content = std::fs::read_to_string(&info_path)
                .map_err(|e| format!("Failed to read tiddlywiki.info: {}", e))?;
            let mut info: serde_json::Value = serde_json::from_str(&content)
                .map_err(|e| format!("Failed to parse tiddlywiki.info: {}", e))?;

            let plugins_array = info.get_mut("plugins")
                .and_then(|v| v.as_array_mut());

            if let Some(arr) = plugins_array {
                for plugin in &plugins {
                    let plugin_path = format!("tiddlywiki/{}", plugin);
                    if !arr.iter().any(|p| p.as_str() == Some(&plugin_path)) {
                        arr.push(serde_json::Value::String(plugin_path));
                    }
                }
            } else {
                let plugin_values: Vec<serde_json::Value> = plugins.iter()
                    .map(|p| serde_json::Value::String(format!("tiddlywiki/{}", p)))
                    .collect();
                info["plugins"] = serde_json::Value::Array(plugin_values);
            }

            let updated_content = serde_json::to_string_pretty(&info)
                .map_err(|e| format!("Failed to serialize tiddlywiki.info: {}", e))?;
            std::fs::write(&info_path, updated_content)
                .map_err(|e| format!("Failed to write tiddlywiki.info: {}", e))?;
        }
    }

    // Strip server-only plugins that don't work in single-file wikis
    let info_path = temp_dir.join("tiddlywiki.info");
    if info_path.exists() {
        strip_server_plugins_from_info(&info_path)?;
    }

    // Resolve includeWikis paths — create symlinks so relative paths
    // (e.g. "../tw5.com") resolve from the temp directory to bundled editions
    let editions_dir = tw_dir.join("editions");
    resolve_include_wikis_paths(&temp_dir, &editions_dir);

    // Get the output filename
    let output_filename = output_path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("wiki.html");

    // Build the single-file wiki
    let mut build_cmd = Command::new(&node_path);
    build_cmd.arg(&tw_path)
        .arg(&temp_dir)
        .arg("--output")
        .arg(temp_dir.join("output"))
        .arg("--render")
        .arg("$:/core/save/all")
        .arg(output_filename)
        .arg("text/plain")
        .current_dir(tw_dir);
    // Set TIDDLYWIKI_PLUGIN_PATH and TIDDLYWIKI_EDITION_PATH for the build step
    if let Ok(data_dir) = get_data_dir(&app) {
        let sep = if cfg!(windows) { ";" } else { ":" };
        let plugins_dir = data_dir.join("plugins");
        let _ = std::fs::create_dir_all(&plugins_dir);
        let mut plugin_paths = vec![plugins_dir.to_string_lossy().to_string()];
        if let Ok(existing) = std::env::var("TIDDLYWIKI_PLUGIN_PATH") {
            if !existing.is_empty() {
                plugin_paths.push(existing);
            }
        }
        build_cmd.env("TIDDLYWIKI_PLUGIN_PATH", plugin_paths.join(sep));
        let editions_dir = data_dir.join("editions");
        let _ = std::fs::create_dir_all(&editions_dir);
        let edition_path = match std::env::var("TIDDLYWIKI_EDITION_PATH") {
            Ok(existing) if !existing.is_empty() => {
                format!("{}{}{}", editions_dir.display(), sep, existing)
            }
            _ => editions_dir.to_string_lossy().to_string(),
        };
        build_cmd.env("TIDDLYWIKI_EDITION_PATH", &edition_path);
    }
    #[cfg(target_os = "windows")]
    build_cmd.creation_flags(CREATE_NO_WINDOW);
    let build_output = build_cmd.output()
        .map_err(|e| format!("Failed to build wiki: {}", e))?;

    if !build_output.status.success() {
        let _ = std::fs::remove_dir_all(&temp_dir);
        let stderr = String::from_utf8_lossy(&build_output.stderr);
        let stdout = String::from_utf8_lossy(&build_output.stdout);
        return Err(format!("Wiki build failed:\n{}\n{}", stdout, stderr));
    }

    // Move the output file to the target location
    let built_file = temp_dir.join("output").join(output_filename);
    if !built_file.exists() {
        let _ = std::fs::remove_dir_all(&temp_dir);
        return Err("Build succeeded but output file not found".to_string());
    }

    // Ensure parent directory exists
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create output directory: {}", e))?;
    }

    std::fs::copy(&built_file, &output_path)
        .map_err(|e| format!("Failed to copy wiki to destination: {}", e))?;

    // Clean up temp directory
    let _ = std::fs::remove_dir_all(&temp_dir);

    println!("Single-file wiki created successfully: {:?}", output_path);
    Ok(())
}

/// Android implementation of create_wiki_file using Node.js via JNI
#[cfg(target_os = "android")]
#[tauri::command]
async fn create_wiki_file(_app: tauri::AppHandle, path: String, edition: String, plugins: Vec<String>) -> Result<(), String> {
    use std::path::Path;

    eprintln!("[TiddlyDesktop] Android create_wiki_file:");
    eprintln!("  path: {}", path);
    eprintln!("  edition: {}", edition);
    eprintln!("  plugins: {:?}", plugins);

    // The path from Android is a content:// URI from SAF
    // We need to:
    // 1. Build the wiki to a temp file using Node.js
    // 2. Copy the temp file to the SAF location

    // Create a temporary file for the build output
    // Use app cache dir on Android (std::env::temp_dir() returns /tmp which isn't writable)
    let temp_dir = {
        use tauri::Manager;
        _app.path().app_cache_dir()
            .unwrap_or_else(|_| std::env::temp_dir())
            .join("tmp")
    };
    std::fs::create_dir_all(&temp_dir)
        .map_err(|e| format!("Failed to create temp directory: {}", e))?;
    let temp_file = temp_dir.join(format!("wiki-{}.html", std::process::id()));
    let temp_path = temp_file.to_str().ok_or("Invalid temp path")?;

    // Build the wiki using Node.js
    android::node_bridge::build_wiki_file(&edition, temp_path, &plugins)?;

    // Read the built wiki
    let wiki_content = std::fs::read_to_string(&temp_file)
        .map_err(|e| format!("Failed to read built wiki: {}", e))?;

    eprintln!("[TiddlyDesktop] Read built wiki: {} bytes", wiki_content.len());
    eprintln!("[TiddlyDesktop] Writing to SAF path: {}", path);

    // Write to the SAF location
    match android::saf::write_document_string(&path, &wiki_content) {
        Ok(()) => {
            eprintln!("[TiddlyDesktop] SAF write successful");
        }
        Err(e) => {
            eprintln!("[TiddlyDesktop] SAF write FAILED: {}", e);
            return Err(e);
        }
    }

    // Clean up temp file
    let _ = std::fs::remove_file(&temp_file);

    eprintln!("[TiddlyDesktop] Wiki created successfully at: {}", path);
    Ok(())
}

/// Add tiddlyweb and filesystem plugins to tiddlywiki.info for proper folder wiki operation.
#[cfg(not(target_os = "android"))]
fn add_server_plugins_to_tiddlywiki_info(info_path: &std::path::Path) -> Result<(), String> {
    use std::io::{Read, Write};

    // Read the existing tiddlywiki.info
    let mut file = std::fs::File::open(info_path)
        .map_err(|e| format!("Failed to open tiddlywiki.info: {}", e))?;
    let mut content = String::new();
    file.read_to_string(&mut content)
        .map_err(|e| format!("Failed to read tiddlywiki.info: {}", e))?;
    drop(file);

    // Parse as JSON
    let mut info: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse tiddlywiki.info: {}", e))?;

    // Ensure plugins array exists
    if info.get("plugins").is_none() {
        info["plugins"] = serde_json::json!([]);
    }

    // Get the plugins array
    let plugins = info["plugins"].as_array_mut()
        .ok_or_else(|| "plugins is not an array".to_string())?;

    // Required server plugins
    let required_plugins = [
        "tiddlywiki/tiddlyweb",
        "tiddlywiki/filesystem",
    ];

    // Add each required plugin if not already present
    for plugin in &required_plugins {
        let plugin_str = serde_json::Value::String(plugin.to_string());
        if !plugins.contains(&plugin_str) {
            plugins.push(plugin_str);
            println!("Added plugin to tiddlywiki.info: {}", plugin);
        }
    }

    // Write back
    let updated_content = serde_json::to_string_pretty(&info)
        .map_err(|e| format!("Failed to serialize tiddlywiki.info: {}", e))?;
    let mut file = std::fs::File::create(info_path)
        .map_err(|e| format!("Failed to create tiddlywiki.info: {}", e))?;
    file.write_all(updated_content.as_bytes())
        .map_err(|e| format!("Failed to write tiddlywiki.info: {}", e))?;

    Ok(())
}

/// Convert a wiki between single-file and folder formats
#[cfg(not(target_os = "android"))]
#[tauri::command]
async fn convert_wiki(app: tauri::AppHandle, source_path: String, dest_path: String, to_folder: bool) -> Result<(), String> {
    // Security: Validate source path
    if drag_drop::sanitize::validate_file_path(&source_path).is_none() {
        return Err("Invalid source path".to_string());
    }
    let source = PathBuf::from(&source_path);
    if !source.is_absolute() {
        return Err("Source path must be absolute".to_string());
    }
    if !drag_drop::sanitize::is_user_accessible_path(&source) {
        return Err("Access to system directories is not allowed".to_string());
    }

    // Security: Validate destination path
    if drag_drop::sanitize::validate_file_path(&dest_path).is_none() {
        return Err("Invalid destination path".to_string());
    }
    let dest = PathBuf::from(&dest_path);
    if !dest.is_absolute() {
        return Err("Destination path must be absolute".to_string());
    }
    if !drag_drop::sanitize::is_user_accessible_path(&dest) {
        return Err("Access to system directories is not allowed".to_string());
    }

    if !source.exists() {
        return Err("Source wiki does not exist".to_string());
    }

    let node_path = get_node_path(&app)?;
    let tw_path = get_tiddlywiki_path(&app)?;

    if to_folder {
        // Convert single-file to folder: tiddlywiki --load <file> --savewikifolder <folder>
        println!("Converting single-file wiki to folder:");
        println!("  Source: {:?}", source);
        println!("  Destination: {:?}", dest);

        // Create destination folder
        std::fs::create_dir_all(&dest)
            .map_err(|e| format!("Failed to create destination folder: {}", e))?;

        let mut cmd = Command::new(&node_path);
        cmd.arg(&tw_path)
            .arg("--load")
            .arg(&source)
            // Strip TiddlyDesktop-injected tiddlers that shouldn't be in standalone wikis
            .arg("--deletetiddlers")
            .arg("[prefix[$:/plugins/tiddlywiki/tiddlydesktop-rs]]")
            .arg("--deletetiddlers")
            .arg("[prefix[$:/plugins/tiddlydesktop-rs]]")
            .arg("--deletetiddlers")
            .arg("[prefix[$:/temp/tiddlydesktop]]")
            .arg("--savewikifolder")
            .arg(&dest);
        #[cfg(target_os = "windows")]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let output = cmd.output()
            .map_err(|e| format!("Failed to run conversion: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(format!("Conversion failed:\n{}\n{}", stdout, stderr));
        }

        // Verify tiddlywiki.info was created
        let info_path = dest.join("tiddlywiki.info");
        if !info_path.exists() {
            return Err("Conversion failed - tiddlywiki.info not created".to_string());
        }

        // Add tiddlyweb and filesystem plugins to tiddlywiki.info for proper folder wiki operation
        if let Err(e) = add_server_plugins_to_tiddlywiki_info(&info_path) {
            println!("Warning: Failed to add server plugins to tiddlywiki.info: {}", e);
        }

        println!("Successfully converted to folder wiki: {:?}", dest);
    } else {
        // Convert folder to single-file: tiddlywiki <folder> --render '$:/core/save/all' 'output.html' 'text/plain'
        println!("Converting folder wiki to single-file:");
        println!("  Source: {:?}", source);
        println!("  Destination: {:?}", dest);

        // Ensure destination has .html extension
        let dest = if dest.extension().map(|e| e == "html" || e == "htm").unwrap_or(false) {
            dest
        } else {
            dest.with_extension("html")
        };

        let output_filename = dest.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("wiki.html");

        // Create a temp output directory
        let temp_output = std::env::temp_dir().join(format!("tiddlydesktop-convert-{}", std::process::id()));
        std::fs::create_dir_all(&temp_output)
            .map_err(|e| format!("Failed to create temp directory: {}", e))?;

        let mut cmd = Command::new(&node_path);
        cmd.arg(&tw_path)
            .arg(&source)
            // Remove server-only plugins that don't work in single-file wikis
            .arg("--deletetiddlers")
            .arg("$:/plugins/tiddlywiki/tiddlyweb")
            .arg("--deletetiddlers")
            .arg("$:/plugins/tiddlywiki/filesystem")
            // Strip TiddlyDesktop-injected tiddlers that shouldn't be in standalone wikis
            .arg("--deletetiddlers")
            .arg("[prefix[$:/plugins/tiddlywiki/tiddlydesktop-rs]]")
            .arg("--deletetiddlers")
            .arg("[prefix[$:/plugins/tiddlydesktop-rs]]")
            .arg("--deletetiddlers")
            .arg("[prefix[$:/temp/tiddlydesktop]]")
            .arg("--output")
            .arg(&temp_output)
            .arg("--render")
            .arg("$:/core/save/all")
            .arg(output_filename)
            .arg("text/plain");
        #[cfg(target_os = "windows")]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let output = cmd.output()
            .map_err(|e| format!("Failed to run conversion: {}", e))?;

        if !output.status.success() {
            let _ = std::fs::remove_dir_all(&temp_output);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(format!("Conversion failed:\n{}\n{}", stdout, stderr));
        }

        // Move the output file to the destination
        let built_file = temp_output.join(output_filename);
        if !built_file.exists() {
            let _ = std::fs::remove_dir_all(&temp_output);
            return Err("Conversion succeeded but output file not found".to_string());
        }

        // Ensure parent directory exists
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create output directory: {}", e))?;
        }

        std::fs::copy(&built_file, &dest)
            .map_err(|e| format!("Failed to copy wiki to destination: {}", e))?;

        // Clean up
        let _ = std::fs::remove_dir_all(&temp_output);

        println!("Successfully converted to single-file wiki: {:?}", dest);
    }

    Ok(())
}

/// Android implementation of convert_wiki using Node.js via SAF
#[cfg(target_os = "android")]
#[tauri::command]
async fn convert_wiki(_app: tauri::AppHandle, source_path: String, dest_path: String, to_folder: bool) -> Result<(), String> {
    eprintln!("[TiddlyDesktop] convert_wiki on Android:");
    eprintln!("  Source: {}", source_path);
    eprintln!("  Dest: {}", dest_path);
    eprintln!("  To folder: {}", to_folder);

    // Both paths should be SAF URIs on Android
    let is_source_saf = source_path.starts_with("content://") || source_path.starts_with('{');
    let is_dest_saf = dest_path.starts_with("content://") || dest_path.starts_with('{');

    if !is_source_saf {
        return Err("Source path must be a SAF URI on Android".to_string());
    }
    if !is_dest_saf {
        return Err("Destination path must be a SAF URI on Android".to_string());
    }

    if to_folder {
        // Convert single-file to folder
        // First verify source is a file (not a folder)
        if android::saf::is_directory(&source_path) {
            return Err("Source is already a folder wiki".to_string());
        }

        // Verify destination folder exists by trying to list it
        // This is more reliable than is_directory() for tree URIs from the folder picker
        let entries = match android::saf::list_directory_entries(&dest_path) {
            Ok(entries) => entries,
            Err(e) => {
                eprintln!("[TiddlyDesktop] Failed to list destination folder: {}", e);
                return Err("Destination folder does not exist or is not accessible. Please create it first using the folder picker.".to_string());
            }
        };

        // Check if destination is empty or a wiki folder
        if !entries.is_empty() {
            // Check if it's already a wiki folder
            let has_tiddlywiki_info = entries.iter().any(|e| e.name == "tiddlywiki.info");
            if has_tiddlywiki_info {
                return Err("Destination folder already contains a wiki. Please choose an empty folder.".to_string());
            }
            // Allow if folder has other files but warn
            eprintln!("[TiddlyDesktop] Warning: Destination folder is not empty");
        }

        android::node_bridge::convert_file_to_folder(&source_path, &dest_path)?;

        eprintln!("[TiddlyDesktop] Successfully converted to folder wiki");
    } else {
        // Convert folder to single-file
        // First verify source is a folder wiki
        if !android::saf::is_directory(&source_path) {
            return Err("Source is not a folder".to_string());
        }

        // Check for tiddlywiki.info
        if android::saf::find_in_directory(&source_path, "tiddlywiki.info")?.is_none() {
            return Err("Source is not a valid wiki folder (missing tiddlywiki.info)".to_string());
        }

        android::node_bridge::convert_folder_to_file(&source_path, &dest_path)?;

        eprintln!("[TiddlyDesktop] Successfully converted to single-file wiki");
    }

    Ok(())
}

#[tauri::command]
fn check_folder_status(path: String) -> Result<FolderStatus, String> {
    let path_buf = PathBuf::from(&path);
    let name = path_buf.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Unknown")
        .to_string();

    // If path doesn't exist yet, return empty status (for new folder creation)
    if !path_buf.exists() {
        // Still validate the path format
        if drag_drop::sanitize::validate_file_path(&path).is_none() {
            return Err("Invalid path".to_string());
        }
        if !path_buf.is_absolute() {
            return Err("Path must be absolute".to_string());
        }
        return Ok(FolderStatus {
            is_wiki: false,
            is_empty: true,
            has_files: false,
            path: path.clone(),
            name,
        });
    }

    // Security: Validate path is a user-accessible directory
    let validated_path = drag_drop::sanitize::validate_user_directory_path(&path)?;

    let is_wiki = validated_path.join("tiddlywiki.info").exists();
    let has_files = std::fs::read_dir(&validated_path)
        .map(|entries| entries.count() > 0)
        .unwrap_or(false);

    Ok(FolderStatus {
        is_wiki,
        is_empty: !has_files,
        has_files,
        path: path.clone(),
        name,
    })
}

/// Reveal file in system file manager
#[cfg(not(target_os = "android"))]
#[tauri::command]
async fn reveal_in_folder(path: String) -> Result<(), String> {
    // Security: Validate path doesn't contain traversal sequences
    if drag_drop::sanitize::validate_file_path(&path).is_none() {
        return Err("Invalid path".to_string());
    }

    let path_buf = PathBuf::from(&path);

    // Path must be absolute
    if !path_buf.is_absolute() {
        return Err("Path must be absolute".to_string());
    }

    // Security: Block access to system directories
    if !drag_drop::sanitize::is_user_accessible_path(&path_buf) {
        return Err("Access to system directories is not allowed".to_string());
    }

    #[cfg(target_os = "linux")]
    {
        let folder = path_buf.parent().unwrap_or(&path_buf);
        std::process::Command::new("xdg-open")
            .arg(folder)
            .spawn()
            .map_err(|e| e.to_string())?;
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("-R")
            .arg(&path)
            .spawn()
            .map_err(|e| e.to_string())?;
    }

    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer")
            .arg("/select,")
            .arg(&path)
            .spawn()
            .map_err(|e| e.to_string())?;
    }

    Ok(())
}

/// Android implementation: open folder containing the wiki in file manager
#[cfg(target_os = "android")]
#[tauri::command]
async fn reveal_in_folder(path: String) -> Result<(), String> {
    android::saf::reveal_in_file_manager(&path)
}

// ============================================================================
// Android-specific commands for SAF file picking
// ============================================================================

/// Pick a wiki HTML file using Android's SAF file picker.
/// Returns the content:// URI of the selected file, or None if cancelled.
#[cfg(target_os = "android")]
#[tauri::command]
async fn android_pick_wiki_file() -> Result<Option<String>, String> {
    android::saf::pick_wiki_file().await
}

/// Pick a directory using Android's SAF directory picker.
/// Returns the content:// URI of the selected directory, or None if cancelled.
#[cfg(target_os = "android")]
#[tauri::command]
async fn android_pick_directory() -> Result<Option<String>, String> {
    android::saf::pick_directory().await
}

/// Create a new wiki file using Android's SAF save dialog.
/// Returns the content:// URI of the created file, or None if cancelled.
#[cfg(target_os = "android")]
#[tauri::command]
async fn android_create_wiki_file(suggested_name: String) -> Result<Option<String>, String> {
    android::saf::save_wiki_file(&suggested_name).await
}

/// Check if we have persistent permission for a content:// URI.
#[cfg(target_os = "android")]
#[tauri::command]
fn android_has_permission(uri: String) -> bool {
    android::saf::has_permission(&uri)
}

/// Release persistent permission for a content:// URI.
/// Call this when removing a wiki from the list.
#[cfg(target_os = "android")]
#[tauri::command]
fn android_release_permission(uri: String) {
    android::saf::release_permission(&uri);
}

/// Pick a backup directory for Android wikis.
/// Returns the content:// URI of the selected directory, or None if cancelled.
#[cfg(target_os = "android")]
#[tauri::command]
async fn android_pick_backup_directory() -> Result<Option<String>, String> {
    android::saf::pick_backup_directory().await
}

/// Copy an attachment file to the wiki's attachments folder on Android.
/// Returns the relative path to use as _canonical_uri (e.g., "./attachments/image.png").
#[cfg(target_os = "android")]
#[tauri::command]
fn android_copy_attachment(wiki_uri: String, source_uri: String, filename: String) -> Result<String, String> {
    android::saf::copy_attachment_to_wiki(&wiki_uri, &source_uri, &filename)
}

/// Stub for non-Android platforms.
#[cfg(not(target_os = "android"))]
#[tauri::command]
fn android_copy_attachment(_wiki_uri: String, _source_uri: String, _filename: String) -> Result<String, String> {
    Err("Android-only feature".to_string())
}

/// Save attachment content directly to the wiki's attachments folder on Android.
/// Used when we have file content (from file picker) instead of a source URI.
/// Takes base64-encoded content and returns the relative path.
#[cfg(target_os = "android")]
#[tauri::command]
fn android_save_attachment(wiki_uri: String, content_base64: String, filename: String) -> Result<String, String> {
    use base64::Engine;
    let content = base64::engine::general_purpose::STANDARD
        .decode(&content_base64)
        .map_err(|e| format!("Failed to decode base64: {}", e))?;
    android::saf::save_attachment_content(&wiki_uri, &content, &filename)
}

/// Stub for non-Android platforms.
#[cfg(not(target_os = "android"))]
#[tauri::command]
fn android_save_attachment(_wiki_uri: String, _content_base64: String, _filename: String) -> Result<String, String> {
    Err("Android-only feature".to_string())
}

/// Pick a wiki folder (directory containing tiddlywiki.info) on Android.
/// Returns the content:// URI of the selected directory, or None if cancelled.
#[cfg(target_os = "android")]
#[tauri::command]
async fn android_pick_wiki_folder() -> Result<Option<String>, String> {
    let dir_uri = android::saf::pick_directory().await?;

    if let Some(ref uri) = dir_uri {
        // Verify this is a valid wiki folder by checking for tiddlywiki.info
        if let Ok(Some(_)) = android::saf::find_in_directory(uri, "tiddlywiki.info") {
            return Ok(dir_uri);
        } else {
            return Err("Selected folder is not a valid TiddlyWiki folder (missing tiddlywiki.info)".to_string());
        }
    }

    Ok(None)
}

/// Get directory entries with full URIs for folder wiki support on Android.
#[cfg(target_os = "android")]
#[tauri::command]
fn android_list_directory(uri: String) -> Result<Vec<(String, String, bool)>, String> {
    let entries = android::saf::list_directory_entries(&uri)?;
    Ok(entries.into_iter().map(|e| (e.name, e.uri, e.is_dir)).collect())
}

/// Check the status of an Android SAF folder for wiki creation/opening.
/// Returns (is_wiki, is_empty, folder_name)
#[cfg(target_os = "android")]
#[tauri::command]
fn android_check_folder_status(uri: String) -> Result<(bool, bool, String), String> {
    // Get folder name
    let name = android::saf::get_display_name(&uri)
        .unwrap_or_else(|_| "Unknown".to_string());

    eprintln!("[TiddlyDesktop] android_check_folder_status: checking folder '{}'", name);
    eprintln!("[TiddlyDesktop]   uri: {}", uri);

    // List directory contents
    let entries = android::saf::list_directory_entries(&uri)?;

    eprintln!("[TiddlyDesktop]   found {} entries:", entries.len());
    for entry in &entries {
        eprintln!("[TiddlyDesktop]     - '{}' (is_dir: {})", entry.name, entry.is_dir);
    }

    // Check if it's a wiki folder (has tiddlywiki.info)
    let is_wiki = entries.iter().any(|e| e.name == "tiddlywiki.info" && !e.is_dir);

    eprintln!("[TiddlyDesktop]   is_wiki: {}", is_wiki);

    // Check if it's empty
    let is_empty = entries.is_empty();

    Ok((is_wiki, is_empty, name))
}

/// Pick a folder for creating a new wiki (can be empty or non-wiki folder).
/// Returns the content:// URI and folder status, or None if cancelled.
#[cfg(target_os = "android")]
#[tauri::command]
async fn android_pick_folder_for_wiki_creation() -> Result<Option<(String, bool, bool, String)>, String> {
    let dir_uri = android::saf::pick_directory().await?;

    if let Some(ref uri) = dir_uri {
        let (is_wiki, is_empty, name) = android_check_folder_status(uri.clone())?;
        Ok(Some((uri.clone(), is_wiki, is_empty, name)))
    } else {
        Ok(None)
    }
}

// Stub implementations for non-Android platforms to allow compilation
#[cfg(not(target_os = "android"))]
#[tauri::command]
async fn android_pick_wiki_file() -> Result<Option<String>, String> {
    Err("Android-only feature".to_string())
}

#[cfg(not(target_os = "android"))]
#[tauri::command]
async fn android_pick_directory() -> Result<Option<String>, String> {
    Err("Android-only feature".to_string())
}

#[cfg(not(target_os = "android"))]
#[tauri::command]
async fn android_create_wiki_file(_suggested_name: String) -> Result<Option<String>, String> {
    Err("Android-only feature".to_string())
}

#[cfg(not(target_os = "android"))]
#[tauri::command]
fn android_has_permission(_uri: String) -> bool {
    false
}

#[cfg(not(target_os = "android"))]
#[tauri::command]
fn android_release_permission(_uri: String) {
    // No-op on desktop
}

#[cfg(not(target_os = "android"))]
#[tauri::command]
async fn android_pick_backup_directory() -> Result<Option<String>, String> {
    Err("Android-only feature".to_string())
}

#[cfg(not(target_os = "android"))]
#[tauri::command]
async fn android_pick_wiki_folder() -> Result<Option<String>, String> {
    Err("Android-only feature".to_string())
}

#[cfg(not(target_os = "android"))]
#[tauri::command]
fn android_list_directory(_uri: String) -> Result<Vec<(String, String, bool)>, String> {
    Err("Android-only feature".to_string())
}

#[cfg(not(target_os = "android"))]
#[tauri::command]
fn android_check_folder_status(_uri: String) -> Result<(bool, bool, String), String> {
    Err("Android-only feature".to_string())
}

#[cfg(not(target_os = "android"))]
#[tauri::command]
async fn android_pick_folder_for_wiki_creation() -> Result<Option<(String, bool, bool, String)>, String> {
    Err("Android-only feature".to_string())
}

/// Set the Android status bar and navigation bar colors to match the TiddlyWiki palette.
#[cfg(target_os = "android")]
#[tauri::command]
fn android_set_system_bar_colors(status_bar_color: String, nav_bar_color: String, foreground_color: Option<String>) -> Result<(), String> {
    android::wiki_activity::set_system_bar_colors(&status_bar_color, &nav_bar_color, foreground_color.as_deref())
}

#[cfg(not(target_os = "android"))]
#[tauri::command]
fn android_set_system_bar_colors(_status_bar_color: String, _nav_bar_color: String, _foreground_color: Option<String>) -> Result<(), String> {
    // No-op on desktop
    Ok(())
}

/// Read a file and return it as a base64 data URI
/// Used by wiki folders to support _canonical_uri with absolute paths
#[tauri::command]
async fn read_file_as_data_uri(path: String) -> Result<String, String> {
    // Security: Validate path is safe and within user directories
    let validated_path = drag_drop::sanitize::validate_user_file_path(&path)?;

    // Read the file
    let data = tokio::fs::read(&validated_path)
        .await
        .map_err(|e| format!("Failed to read file {}: {}", path, e))?;

    // Get MIME type and encode as base64
    let mime_type = utils::get_mime_type(&validated_path);

    use base64::{engine::general_purpose::STANDARD, Engine};
    let base64_data = STANDARD.encode(&data);

    Ok(format!("data:{};base64,{}", mime_type, base64_data))
}

/// Read a file and return it as raw bytes
/// Used for external attachments drag-drop support
#[tauri::command]
async fn read_file_as_binary(path: String) -> Result<Vec<u8>, String> {
    // Security: Validate path is safe and within user directories
    let validated_path = drag_drop::sanitize::validate_user_file_path(&path)?;

    tokio::fs::read(&validated_path)
        .await
        .map_err(|e| format!("Failed to read file {}: {}", path, e))
}

/// Save base64-encoded binary data to a file on disk.
/// Used for pasting clipboard images as external attachments.
/// Returns the full path of the saved file.
#[tauri::command]
async fn save_binary_file(path: String, data_base64: String) -> Result<(), String> {
    use base64::{engine::general_purpose::STANDARD, Engine};

    // Validate path
    let validated_path = drag_drop::sanitize::validate_user_file_path(&path)?;

    // Create parent directories if needed
    if let Some(parent) = validated_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Failed to create directory {}: {}", parent.display(), e))?;
    }

    // Decode base64
    let bytes = STANDARD
        .decode(&data_base64)
        .map_err(|e| format!("Failed to decode base64: {}", e))?;

    // Write file
    tokio::fs::write(&validated_path, &bytes)
        .await
        .map_err(|e| format!("Failed to write file {}: {}", path, e))?;

    eprintln!(
        "[TiddlyDesktop] Saved binary file: {} ({} bytes)",
        path,
        bytes.len()
    );

    Ok(())
}

/// Open a file picker dialog for importing files
/// Returns the selected file paths (empty if cancelled)
/// Used to replace browser's file input with native dialog that exposes full paths
#[tauri::command]
async fn pick_files_for_import(app: tauri::AppHandle, multiple: bool) -> Result<Vec<String>, String> {
    use tauri_plugin_dialog::DialogExt;

    let title = if multiple { "Import Files" } else { "Import File" };

    let paths: Vec<String> = if multiple {
        app.dialog()
            .file()
            .set_title(title)
            .blocking_pick_files()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| p.into_path().ok())
            .map(|p| p.to_string_lossy().to_string())
            .collect()
    } else {
        app.dialog()
            .file()
            .set_title(title)
            .blocking_pick_file()
            .and_then(|p| p.into_path().ok())
            .map(|p| vec![p.to_string_lossy().to_string()])
            .unwrap_or_default()
    };

    Ok(paths)
}

/// Open an authentication URL in a new window that shares the wiki's session
/// This allows users to log into external services and have cookies stored in the wiki's session
///
/// Security measures:
/// - Only HTTPS URLs are allowed (except localhost for development)
/// - DevTools are disabled to prevent credential inspection
/// - No JavaScript injection - pure browser window
/// - File protocol is blocked
#[tauri::command]
async fn open_auth_window(app: tauri::AppHandle, wiki_path: String, url: String, name: String) -> Result<(), String> {
    use tauri::WebviewWindowBuilder;
    use tauri::WebviewUrl;

    // Security: Validate URL scheme
    let url_lower = url.to_lowercase();

    // Block dangerous protocols
    if url_lower.starts_with("file:") {
        return Err("Security: File URLs are not allowed for authentication".to_string());
    }
    if url_lower.starts_with("javascript:") {
        return Err("Security: JavaScript URLs are not allowed".to_string());
    }
    if url_lower.starts_with("data:") {
        return Err("Security: Data URLs are not allowed for authentication".to_string());
    }

    // Only allow HTTPS (and localhost HTTP for development)
    let is_https = url_lower.starts_with("https://");
    let is_localhost_http = url_lower.starts_with("http://localhost")
        || url_lower.starts_with("http://127.0.0.1")
        || url_lower.starts_with("http://[::1]");

    if !is_https && !is_localhost_http {
        return Err("Security: Only HTTPS URLs are allowed for authentication (except localhost)".to_string());
    }

    // Get the session directory for this wiki (same as the wiki window uses)
    let session_dir = get_wiki_session_dir(&app, &wiki_path);

    // Create a unique label for the auth window
    let label = format!("auth-{}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis());

    // Build the auth window with security settings
    #[cfg(not(target_os = "android"))]
    let mut builder = WebviewWindowBuilder::new(
        &app,
        &label,
        WebviewUrl::External(url.parse().map_err(|e| format!("Invalid URL: {}", e))?)
    )
    .title(format!("Login: {}", name))
    .inner_size(900.0, 700.0)
    .resizable(true)
    .center()
    // Security: Disable devtools in auth windows to prevent credential inspection
    .devtools(false);

    #[cfg(target_os = "android")]
    let _ = &name; // name is only used for window title on desktop
    #[cfg(target_os = "android")]
    let mut builder = WebviewWindowBuilder::new(
        &app,
        &label,
        WebviewUrl::External(url.parse().map_err(|e| format!("Invalid URL: {}", e))?)
    )
    // Security: Disable devtools in auth windows to prevent credential inspection
    .devtools(false);

    // Use the same session directory as the wiki
    if let Some(dir) = session_dir {
        builder = builder.data_directory(dir);
    }

    builder.build()
        .map_err(|e| format!("Failed to create auth window: {}", e))?;

    Ok(())
}

/// Open a wiki file in a separate process (desktop only)
/// Each wiki runs in its own process for true isolation (better drag-drop, crash isolation)
/// Returns WikiEntry so frontend can update its wiki list
#[cfg(not(target_os = "android"))]
#[tauri::command]
async fn open_wiki_window(
    app: tauri::AppHandle,
    path: String,
    _backups_enabled: Option<bool>,
    _backup_count: Option<u32>,
    _tiddler_title: Option<String>,
) -> Result<WikiEntry, String> {
    // Security: Validate path is a user-accessible wiki file
    let path_buf = drag_drop::sanitize::validate_user_file_path(&path)?;

    // Validate that this is a TiddlyWiki file before opening
    validate_tiddlywiki_file_async(&path_buf).await?;

    let state = app.state::<AppState>();

    // Extract filename
    let filename = path_buf
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Unknown")
        .to_string();

    // Check if this wiki is already open in a separate process
    {
        let wiki_processes = state.wiki_processes.lock().unwrap();
        if wiki_processes.contains_key(&path) {
            // Wiki already open - send focus request via IPC
            eprintln!("[TiddlyDesktop] Wiki already open in separate process: {}", path);
            if let Some(server) = GLOBAL_IPC_SERVER.get() {
                if let Err(e) = server.send_focus_window(&path) {
                    eprintln!("[TiddlyDesktop] Failed to send focus request: {}", e);
                }
            }
            // Get existing favicon from storage instead of None
            let existing_favicon = wiki_storage::get_wiki_favicon(&app, &path);
            return Ok(WikiEntry {
                path: path.clone(),
                filename,
                display_path: Some(path.clone()),
                favicon: existing_favicon,
                is_folder: false,
                backups_enabled: true,
                backup_dir: None,
                backup_count: None,
                group: None,
                sync_enabled: false,
                sync_id: None,
                sync_peers: vec![],
        relay_room: None,
            });
        }
    }

    // Extract favicon - first try <head> link, then fall back to $:/favicon.ico tiddler
    let favicon = {
        if let Ok(content) = tokio::fs::read_to_string(&path_buf).await {
            tiddlywiki_html::extract_favicon(&content)
        } else {
            None
        }
    };

    // Get the path to our own executable
    let exe_path = std::env::current_exe()
        .map_err(|e| format!("Failed to get executable path: {}", e))?;

    // Spawn the wiki process
    eprintln!("[TiddlyDesktop] Spawning wiki process: {} --wiki {}", exe_path.display(), path);

    let mut cmd = Command::new(&exe_path);
    cmd.arg("--wiki").arg(&path);

    // Pass IPC auth token to child process via environment variable
    if let Some(token) = ipc::get_auth_token() {
        cmd.env(ipc::AUTH_TOKEN_ENV_VAR, token);
    }

    // Pass collab WS port so child process can connect
    #[cfg(not(target_os = "android"))]
    {
        let collab_port = lan_sync::get_collab_port();
        if collab_port > 0 {
            cmd.env("COLLAB_WS_PORT", collab_port.to_string());
        }
    }

    // Platform-specific process configuration
    #[cfg(target_os = "windows")]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    // Wiki processes run independently - they survive when landing page closes
    // This prevents data loss from unsaved changes in open wikis

    let child = cmd.spawn()
        .map_err(|e| format!("Failed to spawn wiki process: {}", e))?;

    let pid = child.id();
    eprintln!("[TiddlyDesktop] Wiki process spawned with PID: {}", pid);

    // Windows: Assign to job object so it gets killed when parent exits
    #[cfg(target_os = "windows")]
    drag_drop::windows_job::assign_process_to_job(pid);

    // Track the process
    state.wiki_processes.lock().unwrap().insert(path.clone(), WikiProcess {
        pid,
        path: path.clone(),
    });

    // Spawn a thread to wait for the process to exit and clean up
    let app_handle = app.clone();
    let path_clone = path.clone();
    std::thread::spawn(move || {
        let mut child = child;
        match child.wait() {
            Ok(status) => {
                eprintln!("[TiddlyDesktop] Wiki process (PID {}) exited with status: {}", pid, status);
            }
            Err(e) => {
                eprintln!("[TiddlyDesktop] Error waiting for wiki process: {}", e);
            }
        }

        // Clean up tracking
        let state = app_handle.state::<AppState>();
        state.wiki_processes.lock().unwrap().remove(&path_clone);
        eprintln!("[TiddlyDesktop] Removed wiki process from tracking: {}", path_clone);

        // Exit app if no more wikis and no windows
        let wiki_count = state.wiki_processes.lock().unwrap().len();
        let has_windows = app_handle.webview_windows().len() > 0;
        if wiki_count == 0 && !has_windows {
            eprintln!("[TiddlyDesktop] No more wikis or windows, exiting");
            app_handle.exit(0);
        }
    });

    // Pre-request fingerprints from peers so sync data is ready when the wiki JS boots.
    // Look up the sync_id for this wiki and ask peers to send their fingerprints now.
    {
        let sync_id = wiki_storage::get_wiki_sync_id(app.clone(), path.clone());
        if !sync_id.is_empty() {
            tokio::spawn(async move {
                if let Some(mgr) = lan_sync::get_sync_manager() {
                    mgr.pre_request_sync(&sync_id).await;
                }
            });
        }
    }

    // Create the wiki entry
    let entry = WikiEntry {
        path: path.clone(),
        filename,
        display_path: Some(path),
        favicon,
        is_folder: false,
        backups_enabled: true,
        backup_dir: None,
        backup_count: None,
        group: None,
        sync_enabled: false,
        sync_id: None,
        sync_peers: vec![],
        relay_room: None,
    };

    // Add to recent files list
    let _ = wiki_storage::add_to_recent_files(&app, entry.clone());

    // Return the wiki entry so frontend can update its list
    Ok(entry)
}

/// Open a wiki file in a new app instance (Android)
/// On Android, each wiki opens in a separate WikiActivity (visible in recent apps)
/// WikiActivity starts its own HTTP server in the :wiki process, independent of Tauri.
#[cfg(target_os = "android")]
#[tauri::command]
async fn open_wiki_window(
    app: tauri::AppHandle,
    path: String,
    backups_enabled: Option<bool>,
    backup_count: Option<u32>,
    tiddler_title: Option<String>,
) -> Result<WikiEntry, String> {
    // Run on a blocking thread so SAF reads don't block the Tauri async runtime.
    // This allows concurrent wiki opening (e.g. single-file + folder wiki at the same time).
    let app_clone = app.clone();
    let path_clone = path.clone();
    tokio::task::spawn_blocking(move || {
        open_wiki_window_blocking(app_clone, path_clone, backups_enabled, backup_count, tiddler_title)
    })
    .await
    .map_err(|e| format!("Task join error: {}", e))?
}

#[cfg(target_os = "android")]
fn open_wiki_window_blocking(
    app: tauri::AppHandle,
    path: String,
    backups_enabled: Option<bool>,
    backup_count: Option<u32>,
    tiddler_title: Option<String>,
) -> Result<WikiEntry, String> {
    // Path is a content:// URI or JSON-serialized FileUri on Android
    let is_saf_uri = path.starts_with("content://") || path.starts_with("{");

    // Get display name for the filename
    let filename = if is_saf_uri {
        android::saf::get_display_name(&path).unwrap_or_else(|_| "Wiki".to_string())
    } else {
        PathBuf::from(&path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Wiki")
            .to_string()
    };

    // Read wiki content to extract favicon (we don't need to keep the content)
    let favicon = if is_saf_uri {
        match android::saf::read_document_string(&path) {
            Ok(content) => tiddlywiki_html::extract_favicon(&content),
            Err(_) => None,
        }
    } else {
        match std::fs::read_to_string(&path) {
            Ok(content) => tiddlywiki_html::extract_favicon(&content),
            Err(_) => None,
        }
    };

    eprintln!("[TiddlyDesktop] Opening single-file wiki: {}", filename);
    eprintln!("[TiddlyDesktop] WikiActivity will start its own HTTP server in :wiki process");

    // Foreground service is now started from WikiActivity.onCreate() (same :wiki process)

    // Get backup settings - use provided values or defaults
    let use_backups = backups_enabled.unwrap_or(true); // Default: enabled
    let use_backup_count = backup_count.unwrap_or(20); // Default: 20 backups
    let custom_backup_dir = get_wiki_backup_dir(&app, &path);

    // Launch WikiActivity - it will start its own server in the :wiki process
    android::wiki_activity::launch_wiki_activity(
        &path,
        &filename,
        false, // is_folder
        None,  // No pre-rendered HTML for single-file wikis
        use_backups,
        use_backup_count,
        None, // Not a folder wiki
        custom_backup_dir.as_deref(),
        tiddler_title.as_deref(),
    )?;

    // Pre-request fingerprints from peers so sync data is ready when the wiki JS boots.
    {
        let sync_id = wiki_storage::get_wiki_sync_id(app.clone(), path.clone());
        if !sync_id.is_empty() {
            tokio::spawn(async move {
                if let Some(mgr) = lan_sync::get_sync_manager() {
                    mgr.pre_request_sync(&sync_id).await;
                }
            });
        }
    }

    let entry = WikiEntry {
        path: path.clone(),
        filename,
        display_path: Some(android::saf::get_display_path(&path)),
        favicon,
        is_folder: false,
        backups_enabled: use_backups,
        backup_dir: None,
        backup_count: Some(use_backup_count),
        group: None,
        sync_enabled: false,
        sync_id: None,
        sync_peers: vec![],
        relay_room: None,
    };

    // Add to recent files
    let _ = wiki_storage::add_to_recent_files(&app, entry.clone());

    Ok(entry)
}

/// Check for a pending wiki open request from the home screen widget.
/// Returns the wiki info if pending, or null if none.
/// The pending file is consumed (deleted) after reading.
#[cfg(target_os = "android")]
#[tauri::command]
fn get_pending_widget_wiki(app: tauri::AppHandle) -> Option<serde_json::Value> {
    let files_dir = app.path().app_data_dir().ok()?.join("files");
    let pending_file = files_dir.join("pending_widget_wiki.json");
    if !pending_file.exists() {
        return None;
    }
    let content = std::fs::read_to_string(&pending_file).ok()?;
    let _ = std::fs::remove_file(&pending_file);
    eprintln!("[TiddlyDesktop] Consumed pending widget wiki: {}", content);
    serde_json::from_str(&content).ok()
}

#[cfg(not(target_os = "android"))]
#[tauri::command]
fn get_pending_widget_wiki(_app: tauri::AppHandle) -> Option<serde_json::Value> {
    None
}

/// Open a tiddler from a wiki in a new window (single-tiddler view)
/// The new window shares the same wiki and syncs changes via events
#[tauri::command]
async fn open_tiddler_window(
    app: tauri::AppHandle,
    parent_label: String,
    tiddler_title: String,
    template: Option<String>,
    window_title: Option<String>,
    width: Option<f64>,
    height: Option<f64>,
    left: Option<f64>,
    top: Option<f64>,
    variables: Option<String>, // JSON-encoded additional variables
) -> Result<String, String> {
    let state = app.state::<AppState>();

    // Get the wiki path from the parent window
    let wiki_path = {
        let open_wikis = state.open_wikis.lock().unwrap();
        open_wikis.get(&parent_label).cloned()
    }.ok_or_else(|| format!("Parent window '{}' not found", parent_label))?;

    // Create a unique key for this wiki path
    let path_key = utils::base64_url_encode(&wiki_path);

    // Generate a unique window label for this tiddler window
    let safe_title = tiddler_title
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-')
        .take(30)
        .collect::<String>();

    let label = {
        let open_wikis = state.open_wikis.lock().unwrap();
        let mut label = format!("tiddler-{}-{}", safe_title, parent_label);
        let mut counter = 1;
        while open_wikis.contains_key(&label) {
            label = format!("tiddler-{}-{}-{}", safe_title, parent_label, counter);
            counter += 1;
        }
        label
    };

    // Build URL with query parameters for single-tiddler mode
    let encoded_tiddler = urlencoding::encode(&tiddler_title);
    let template_param = template.as_deref().unwrap_or("$:/core/templates/single.tiddler.window");
    let encoded_template = urlencoding::encode(template_param);
    let encoded_parent = urlencoding::encode(&parent_label);

    let mut wiki_url = format!(
        "wikifile://localhost/{}?tiddler={}&template={}&parent={}",
        path_key, encoded_tiddler, encoded_template, encoded_parent
    );

    // Add variables to URL if provided
    if let Some(vars) = &variables {
        let encoded_vars = urlencoding::encode(vars);
        wiki_url.push_str(&format!("&variables={}", encoded_vars));
    }

    // Track this window - map to same wiki path but with special marker
    state.open_wikis.lock().unwrap().insert(label.clone(), format!("{}#tiddler:{}", wiki_path, tiddler_title));

    // Store label for protocol handler
    state.wiki_paths.lock().unwrap().insert(format!("{}_label", path_key), PathBuf::from(&label));

    let title = window_title.unwrap_or_else(|| tiddler_title.clone());
    #[cfg(not(target_os = "android"))]
    let win_width = width.unwrap_or(700.0);
    #[cfg(not(target_os = "android"))]
    let win_height = height.unwrap_or(600.0);
    let _ = (&title, &width, &height); // Silence unused warnings on Android

    #[cfg(not(target_os = "android"))]
    let icon = Image::from_bytes(include_bytes!("../icons/icon.png"))
        .map_err(|e| format!("Failed to load icon: {}", e))?;

    // Get isolated session directory - use the PARENT wiki's path so tiddler windows
    // share session with their parent wiki
    let session_dir = get_wiki_session_dir(&app, &wiki_path);

    // Use full init script for tiddler windows too - they need __WIKI_PATH__ for external attachments
    #[allow(unused_mut)] // mut needed on non-Linux; Linux shadows with user_agent()
    #[cfg(not(target_os = "android"))]
    let mut builder = WebviewWindowBuilder::new(&app, &label, WebviewUrl::External(wiki_url.parse().unwrap()))
        .title(&title)
        .inner_size(win_width, win_height)
        .icon(icon)
        .map_err(|e| format!("Failed to set icon: {}", e))?
        .window_classname("tiddlydesktop-rs")
        .initialization_script(&init_script::get_wiki_init_script(&wiki_path, &label, false))
        .on_document_title_changed(|ww, title| { let _ = ww.set_title(&title); })
        .zoom_hotkeys_enabled(true)
        .devtools(cfg!(debug_assertions)); // Only enable in debug builds

    #[cfg(target_os = "linux")]
    let mut builder = builder.user_agent(LINUX_USER_AGENT);

    // Inject embed proxy port so external iframes can be proxied through HTTP
    #[cfg(not(target_os = "android"))]
    if let Some(state) = app.try_state::<MediaServerState>() {
        let port = state.server.port();
        builder = builder.initialization_script(&format!("window.__TD_EMBED_PORT__ = {};", port));
    }

    #[cfg(target_os = "android")]
    let mut builder = WebviewWindowBuilder::new(&app, &label, WebviewUrl::External(wiki_url.parse().unwrap()))
        .initialization_script(&init_script::get_wiki_init_script(&wiki_path, &label, false))
        .devtools(cfg!(debug_assertions)); // Only enable in debug builds

    // Apply isolated session if available (shares with parent wiki)
    if let Some(dir) = session_dir {
        builder = builder.data_directory(dir);
    }

    // Set window position if specified (desktop only)
    #[cfg(not(target_os = "android"))]
    if let (Some(x), Some(y)) = (left, top) {
        builder = builder.position(x, y);
    }
    #[cfg(target_os = "android")]
    let _ = (left, top); // Silence unused warnings

    // Tauri's drag/drop handler intercepts drops before WebKit/DOM gets them.
    // On macOS, we disable it and use custom handlers.
    // On Windows, we keep it ENABLED - our WRY patch intercepts drops, extracts file paths,
    // emits tauri://drag-* events, then forwards to WebView2's native handler for DOM events.
    // On Linux, vanilla WebKitGTK handles drops natively.
    #[cfg(target_os = "macos")]
    {
        builder = builder.disable_drag_drop_handler();
    }

    let window = builder
        .build()
        .map_err(|e| format!("Failed to create tiddler window: {}", e))?;

    // Note: Drag handlers are set up via the drag_drop plugin's on_webview_ready hook

    // Linux: Set up HeaderBar, enable smooth scrolling, center window (tiddler windows don't save state)
    #[cfg(target_os = "linux")]
    {
        setup_header_bar(&window);
        enable_smooth_scrolling(&window);
        linux_finalize_window_state(&window, &None);
    }

    // Handle window close
    let app_handle = app.clone();
    let label_clone = label.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::Destroyed = event {
            let state = app_handle.state::<AppState>();
            state.open_wikis.lock().unwrap().remove(&label_clone);
        }
    });

    Ok(label)
}

/// Spawn a wiki window as a separate process (sync version for IPC callbacks)
/// This doesn't track the process in AppState - used for IPC-triggered spawns
fn spawn_wiki_process_sync(wiki_path: &str) -> Result<u32, String> {
    let exe_path = std::env::current_exe()
        .map_err(|e| format!("Failed to get executable path: {}", e))?;

    eprintln!("[TiddlyDesktop] Spawning wiki process via IPC: {} --wiki {}", exe_path.display(), wiki_path);

    let mut cmd = Command::new(&exe_path);
    cmd.arg("--wiki").arg(wiki_path);

    // Pass IPC auth token to child process via environment variable
    if let Some(token) = ipc::get_auth_token() {
        cmd.env(ipc::AUTH_TOKEN_ENV_VAR, token);
    }

    // Pass collab WS port so child process can connect
    #[cfg(not(target_os = "android"))]
    {
        let collab_port = lan_sync::get_collab_port();
        if collab_port > 0 {
            cmd.env("COLLAB_WS_PORT", collab_port.to_string());
        }
    }

    // Platform-specific process configuration
    #[cfg(target_os = "windows")]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    // Wiki processes run independently - they survive when landing page closes

    let child = cmd.spawn()
        .map_err(|e| format!("Failed to spawn wiki process: {}", e))?;

    let pid = child.id();
    eprintln!("[TiddlyDesktop] Wiki process spawned with PID: {}", pid);

    // Windows: Assign to job object so it gets killed when parent exits
    #[cfg(target_os = "windows")]
    drag_drop::windows_job::assign_process_to_job(pid);

    // Spawn a thread to wait for the process to exit (cleanup)
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });

    Ok(pid)
}

/// Spawn a tiddler window as a separate process
/// This is used by both the main process and via IPC from wiki processes
fn spawn_tiddler_process(wiki_path: &str, tiddler_title: &str, startup_tiddler: Option<&str>) -> Result<u32, String> {
    let exe_path = std::env::current_exe()
        .map_err(|e| format!("Failed to get executable path: {}", e))?;

    eprintln!("[TiddlyDesktop] Spawning tiddler process: {} --wiki {} --tiddler {}",
        exe_path.display(), wiki_path, tiddler_title);

    let mut cmd = Command::new(&exe_path);
    cmd.arg("--wiki").arg(wiki_path);
    cmd.arg("--tiddler").arg(tiddler_title);

    if let Some(startup) = startup_tiddler {
        cmd.arg("--startup-tiddler").arg(startup);
    }

    // Pass IPC auth token to child process via environment variable
    if let Some(token) = ipc::get_auth_token() {
        cmd.env(ipc::AUTH_TOKEN_ENV_VAR, token);
    }

    // Pass collab WS port so child process can connect
    #[cfg(not(target_os = "android"))]
    {
        let collab_port = lan_sync::get_collab_port();
        if collab_port > 0 {
            cmd.env("COLLAB_WS_PORT", collab_port.to_string());
        }
    }

    // Platform-specific process configuration
    #[cfg(target_os = "windows")]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    // Wiki processes run independently - they survive when landing page closes
    // This prevents data loss from unsaved changes in open wikis

    let child = cmd.spawn()
        .map_err(|e| format!("Failed to spawn tiddler process: {}", e))?;

    let pid = child.id();
    eprintln!("[TiddlyDesktop] Tiddler process spawned with PID: {}", pid);

    // Windows: Assign to job object so it gets killed when parent exits
    #[cfg(target_os = "windows")]
    drag_drop::windows_job::assign_process_to_job(pid);

    // Spawn a thread to wait for the process to exit (cleanup)
    std::thread::spawn(move || {
        let mut child = child;
        match child.wait() {
            Ok(status) => {
                eprintln!("[TiddlyDesktop] Tiddler process (PID {}) exited with status: {}", pid, status);
            }
            Err(e) => {
                eprintln!("[TiddlyDesktop] Error waiting for tiddler process: {}", e);
            }
        }
    });

    Ok(pid)
}

/// IPC command: Notify other windows about a tiddler change
/// Called from JavaScript when a tiddler is modified
#[cfg(not(target_os = "android"))]
#[tauri::command]
fn ipc_notify_tiddler_changed(
    state: tauri::State<WikiModeState>,
    tiddler_title: String,
    tiddler_json: String,
) -> Result<(), String> {
    let mut client_guard = state.ipc_client.lock().unwrap();
    if let Some(ref mut client) = *client_guard {
        client.notify_tiddler_changed(&tiddler_title, &tiddler_json)
            .map_err(|e| format!("IPC error: {}", e))?;
    }
    Ok(())
}

/// IPC command: Notify other windows about a tiddler deletion
#[cfg(not(target_os = "android"))]
#[tauri::command]
fn ipc_notify_tiddler_deleted(
    state: tauri::State<WikiModeState>,
    tiddler_title: String,
) -> Result<(), String> {
    let mut client_guard = state.ipc_client.lock().unwrap();
    if let Some(ref mut client) = *client_guard {
        client.notify_tiddler_deleted(&tiddler_title)
            .map_err(|e| format!("IPC error: {}", e))?;
    }
    Ok(())
}

/// IPC command: Request to open a tiddler in a new window process
/// This sends a message to the main process which spawns the tiddler window
#[cfg(not(target_os = "android"))]
#[tauri::command]
fn ipc_open_tiddler_window(
    state: tauri::State<WikiModeState>,
    tiddler_title: String,
    startup_tiddler: Option<String>,
) -> Result<(), String> {
    let mut client_guard = state.ipc_client.lock().unwrap();
    if let Some(ref mut client) = *client_guard {
        client.request_open_tiddler(&tiddler_title, startup_tiddler.as_deref())
            .map_err(|e| format!("IPC error: {}", e))?;
    } else {
        return Err("Not connected to IPC server".to_string());
    }
    Ok(())
}

/// IPC command: Check if this is a tiddler window
#[cfg(not(target_os = "android"))]
#[tauri::command]
fn ipc_is_tiddler_window(state: tauri::State<WikiModeState>) -> bool {
    state.is_tiddler_window
}

/// IPC command: Get the tiddler title if this is a tiddler window
#[cfg(not(target_os = "android"))]
#[tauri::command]
fn ipc_get_tiddler_title(state: tauri::State<WikiModeState>) -> Option<String> {
    state.tiddler_title.clone()
}

/// IPC command: Request sync from source wiki (for tiddler windows)
#[cfg(not(target_os = "android"))]
#[tauri::command]
fn ipc_request_sync(state: tauri::State<WikiModeState>) -> Result<(), String> {
    let mut client_guard = state.ipc_client.lock().unwrap();
    if let Some(ref mut client) = *client_guard {
        client.request_sync()
            .map_err(|e| format!("IPC error: {}", e))?;
    }
    Ok(())
}

/// IPC command: Send current wiki state (response to sync request from tiddler windows)
#[cfg(not(target_os = "android"))]
#[tauri::command]
fn ipc_send_sync_state(
    state: tauri::State<WikiModeState>,
    tiddlers_json: String,
) -> Result<(), String> {
    let mut client_guard = state.ipc_client.lock().unwrap();
    if let Some(ref mut client) = *client_guard {
        client.send_sync_state(&tiddlers_json)
            .map_err(|e| format!("IPC error: {}", e))?;
    }
    Ok(())
}

/// Response for update check
#[derive(serde::Serialize)]
struct UpdateCheckResult {
    update_available: bool,
    latest_version: Option<String>,
    releases_url: String,
    current_version: String,
}

/// Check for application updates
/// On Android: Checks Google Play Store
/// On Desktop: Checks GitHub releases
#[tauri::command]
async fn check_for_updates() -> Result<UpdateCheckResult, String> {
    #[cfg(target_os = "android")]
    {
        check_for_updates_android().await
    }
    #[cfg(not(target_os = "android"))]
    {
        check_for_updates_desktop().await
    }
}

/// Android version - separate from desktop versioning (must match build.gradle.kts versionName)
#[cfg(target_os = "android")]
const ANDROID_VERSION: &str = "0.0.35";

/// Check for updates on Android via version file on GitHub, linking to Play Store
#[cfg(target_os = "android")]
async fn check_for_updates_android() -> Result<UpdateCheckResult, String> {
    let current_version = ANDROID_VERSION;
    let releases_url = "https://play.google.com/store/apps/details?id=com.burningtreec.tiddlydesktop_rs".to_string();

    let client = reqwest::Client::builder()
        .user_agent("TiddlyDesktop-RS")
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    let response = client
        .get("https://raw.githubusercontent.com/BurningTreeC/tiddlydesktop-rs/main/android-version.txt")
        .send()
        .await
        .map_err(|e| format!("Failed to fetch version file: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("GitHub returned status: {}", response.status()));
    }

    let text = response
        .text()
        .await
        .map_err(|e| format!("Failed to read version file: {}", e))?;

    let latest_version = text.trim().to_string();
    let update_available = version_is_newer(&latest_version, current_version);

    Ok(UpdateCheckResult {
        update_available,
        latest_version: Some(latest_version),
        releases_url,
        current_version: current_version.to_string(),
    })
}

/// Check for updates on Desktop via GitHub releases
#[cfg(not(target_os = "android"))]
async fn check_for_updates_desktop() -> Result<UpdateCheckResult, String> {
    let current_version = env!("CARGO_PKG_VERSION");
    let releases_url = "https://github.com/BurningTreeC/tiddlydesktop-rs/releases".to_string();

    // Fetch latest release from GitHub API
    let client = reqwest::Client::builder()
        .user_agent("TiddlyDesktop-RS")
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    let response = client
        .get("https://api.github.com/repos/BurningTreeC/tiddlydesktop-rs/releases/latest")
        .send()
        .await
        .map_err(|e| format!("Failed to fetch release info: {}", e))?;

    if !response.status().is_success() {
        return Err(format!("GitHub API returned status: {}", response.status()));
    }

    let release: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("Failed to parse release info: {}", e))?;

    let latest_version = release["tag_name"]
        .as_str()
        .map(|s| s.trim_start_matches('v').to_string());

    let update_available = if let Some(ref latest) = latest_version {
        version_is_newer(latest, current_version)
    } else {
        false
    };

    Ok(UpdateCheckResult {
        update_available,
        latest_version,
        releases_url,
        current_version: current_version.to_string(),
    })
}

/// Compare version strings (e.g., "0.3.20" > "0.3.19")
fn version_is_newer(latest: &str, current: &str) -> bool {
    let parse_version = |v: &str| -> Vec<u32> {
        v.split('.')
            .filter_map(|part| part.parse::<u32>().ok())
            .collect()
    };

    let latest_parts = parse_version(latest);
    let current_parts = parse_version(current);

    for i in 0..latest_parts.len().max(current_parts.len()) {
        let latest_part = latest_parts.get(i).copied().unwrap_or(0);
        let current_part = current_parts.get(i).copied().unwrap_or(0);

        if latest_part > current_part {
            return true;
        } else if latest_part < current_part {
            return false;
        }
    }

    false
}

/// IPC command: Update wiki favicon (sends to main process via IPC)
#[cfg(not(target_os = "android"))]
#[tauri::command]
fn ipc_update_favicon(
    state: tauri::State<WikiModeState>,
    favicon: Option<String>,
) -> Result<(), String> {
    let mut client_guard = state.ipc_client.lock().unwrap();
    if let Some(ref mut client) = *client_guard {
        client.send_update_favicon(&state.wiki_path.to_string_lossy(), favicon)
            .map_err(|e| format!("IPC error: {}", e))?;
    }
    Ok(())
}

/// Initialize PDFium from the bundled library in the tdlib resources directory.
/// Uses the same multi-path resolution as tdlib_protocol_handler.
/// Safe to call multiple times — subsequent calls are no-ops.
fn init_pdfium_from_resources(app: &tauri::AppHandle) {
    if pdf_renderer::is_initialized() {
        return;
    }

    #[cfg(target_os = "linux")]
    let lib_name = "libpdfium.so";
    #[cfg(target_os = "windows")]
    let lib_name = "pdfium.dll";
    #[cfg(target_os = "macos")]
    let lib_name = "libpdfium.dylib";
    #[cfg(target_os = "android")]
    let lib_name = "libpdfium.so";

    let resource_dir = match get_resource_dir_path(app) {
        Some(d) => d,
        None => {
            eprintln!("[TiddlyDesktop] PDFium init: no resource directory found");
            return;
        }
    };

    // Try multiple paths: bundled structure, tarball structure, dev-mode fallback
    let candidates = [
        resource_dir.join("resources").join("tdlib").join(lib_name),
        resource_dir.join("tdlib").join(lib_name),
    ];

    // Dev-mode: resolve relative to executable
    let dev_path = std::env::current_exe().ok()
        .and_then(|exe| exe.parent().map(|d| d.join("..").join("..").join("resources").join("tdlib").join(lib_name)))
        .unwrap_or_else(|| PathBuf::from("src-tauri").join("resources").join("tdlib").join(lib_name));

    #[cfg(target_os = "android")]
    let android_path = {
        // On Android, libpdfium.so is in jniLibs and loaded via System.loadLibrary path
        // Try the extracted resources first, then nativeLibraryDir
        let data_dir = resource_dir.clone();
        data_dir.join(lib_name)
    };

    for candidate in candidates.iter().chain(std::iter::once(&dev_path)) {
        if candidate.exists() {
            match pdf_renderer::init_pdfium(&candidate.to_string_lossy()) {
                Ok(()) => {
                    eprintln!("[TiddlyDesktop] PDFium initialized from {:?}", candidate);
                    return;
                }
                Err(e) => {
                    eprintln!("[TiddlyDesktop] PDFium init failed for {:?}: {}", candidate, e);
                }
            }
        }
    }

    #[cfg(target_os = "android")]
    if android_path.exists() {
        match pdf_renderer::init_pdfium(&android_path.to_string_lossy()) {
            Ok(()) => {
                eprintln!("[TiddlyDesktop] PDFium initialized from {:?}", android_path);
                return;
            }
            Err(e) => {
                eprintln!("[TiddlyDesktop] PDFium init failed for {:?}: {}", android_path, e);
            }
        }
    }

    // On Android, try loading from the native lib directory (loaded by JNI)
    #[cfg(target_os = "android")]
    {
        match pdf_renderer::init_pdfium("libpdfium.so") {
            Ok(()) => {
                eprintln!("[TiddlyDesktop] PDFium initialized from system library path");
                return;
            }
            Err(e) => {
                eprintln!("[TiddlyDesktop] PDFium init from system path failed: {}", e);
            }
        }
    }

    eprintln!("[TiddlyDesktop] PDFium: library not found in any expected location");
}

/// Get the resource directory, preferring paths relative to executable for tarball installs
/// This avoids baked-in CI paths like /home/runner/...
/// On Android, returns the app data directory where resources have been extracted.
#[cfg(not(target_os = "android"))]
fn get_resource_dir_path(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            // Tarball structure: bin/tiddlydesktop-rs with resources at ../lib/tiddlydesktop-rs/
            let tarball_resources = exe_dir.join("..").join("lib").join("tiddlydesktop-rs");
            if tarball_resources.exists() {
                if let Ok(canonical) = tarball_resources.canonicalize() {
                    return Some(canonical);
                }
            }

            // AppImage/installed structure: resources might be in ../lib/<app-name>
            // or alongside the binary
            let lib_resources = exe_dir.join("..").join("lib").join("tiddlydesktop-rs");
            if lib_resources.exists() {
                if let Ok(canonical) = lib_resources.canonicalize() {
                    return Some(canonical);
                }
            }
        }
    }

    // Fall back to Tauri's resource_dir (may have baked-in paths from CI)
    app.path().resource_dir().ok()
}

/// Android version: Returns the app data directory where resources have been extracted
#[cfg(target_os = "android")]
fn get_resource_dir_path(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    // On Android, resources are extracted to <app_data>/ directory
    // The tiddlywiki resources are at <app_data>/tiddlywiki/
    app.path().app_data_dir().ok()
}

// get_data_dir is now a pub fn at the top of lib.rs that caches in DATA_DIR.
// It supports portable mode on all desktop platforms.

/// Get an isolated session data directory for a wiki
/// Each wiki gets its own session storage (cookies, localStorage, etc.)
/// This prevents cross-wiki data leakage from plugins/scripts
fn get_wiki_session_dir(app: &tauri::AppHandle, wiki_path: &str) -> Option<std::path::PathBuf> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    // Create a hash of the wiki path for a shorter directory name
    let mut hasher = DefaultHasher::new();
    wiki_path.hash(&mut hasher);
    let hash = hasher.finish();

    // Get data directory (respects portable mode)
    if let Ok(data_dir) = get_data_dir(app) {
        let session_dir = data_dir.join("wiki_sessions").join(format!("{:016x}", hash));
        // Create the directory if it doesn't exist
        if let Err(e) = std::fs::create_dir_all(&session_dir) {
            eprintln!("[TiddlyDesktop] Failed to create session directory: {}", e);
            return None;
        }
        Some(session_dir)
    } else {
        None
    }
}

/// Clear session data for a wiki (cookies, localStorage, etc.)
/// This is useful when users want to log out of authenticated services
#[tauri::command]
fn clear_wiki_session(app: tauri::AppHandle, wiki_path: String) -> Result<(), String> {
    if let Some(session_dir) = get_wiki_session_dir(&app, &wiki_path) {
        eprintln!("[TiddlyDesktop] Clearing session data for wiki: {}", wiki_path);
        eprintln!("[TiddlyDesktop] Session directory: {:?}", session_dir);

        // Remove the entire session directory
        if session_dir.exists() {
            std::fs::remove_dir_all(&session_dir)
                .map_err(|e| format!("Failed to clear session data: {}", e))?;
            eprintln!("[TiddlyDesktop] Session data cleared successfully");
        } else {
            eprintln!("[TiddlyDesktop] Session directory doesn't exist, nothing to clear");
        }

        Ok(())
    } else {
        Err("Failed to determine session directory".to_string())
    }
}

/// Parse query string into a HashMap
fn parse_query_string(query: Option<&str>) -> std::collections::HashMap<String, String> {
    let mut params = std::collections::HashMap::new();
    if let Some(q) = query {
        for pair in q.split('&') {
            if let Some((key, value)) = pair.split_once('=') {
                let key = urlencoding::decode(key).unwrap_or_default().to_string();
                let value = urlencoding::decode(value).unwrap_or_default().to_string();
                params.insert(key, value);
            }
        }
    }
    params
}

/// Build a 416 Range Not Satisfiable response
fn build_416_response(file_size: u64) -> Response<Vec<u8>> {
    Response::builder()
        .status(416)
        .header("Content-Range", format!("bytes */{}", file_size))
        .header("Access-Control-Allow-Origin", "*")
        .body(Vec::new())
        .unwrap()
}

/// Maximum bytes to serve per range response for media streaming.
/// Caps each chunk to prevent loading entire large files into memory.
/// WebKitGTK's URI scheme responses use MemoryInputStream (in-memory buffer),
/// so we must keep response sizes bounded for smooth media playback.
const STREAM_MAX_CHUNK: u64 = 2 * 1024 * 1024; // 2 MB

/// Serve a file with HTTP Range request support for media streaming.
/// Returns 200 for full file, 206 for partial content, or 416 for invalid ranges.
fn serve_file_with_range_support(
    path: &std::path::Path,
    mime_type: &str,
    headers: &tauri::http::HeaderMap,
) -> Response<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            return Response::builder()
                .status(404)
                .header("Access-Control-Allow-Origin", "*")
                .body(format!("File not found: {}", e).as_bytes().to_vec())
                .unwrap();
        }
    };

    let file_size = match file.metadata() {
        Ok(m) => m.len(),
        Err(e) => {
            return Response::builder()
                .status(500)
                .header("Access-Control-Allow-Origin", "*")
                .body(format!("Cannot read file metadata: {}", e).as_bytes().to_vec())
                .unwrap();
        }
    };

    // Check for Range header
    let range_header = headers.get("range").and_then(|v| v.to_str().ok());

    if let Some(range_str) = range_header {
        // Parse "bytes=start-end", "bytes=start-", or "bytes=-suffix"
        let range_str = range_str.trim();
        if !range_str.starts_with("bytes=") {
            return build_416_response(file_size);
        }
        let range_spec = &range_str[6..];

        // Only handle single range (not multipart)
        if range_spec.contains(',') {
            return build_416_response(file_size);
        }

        let (start, mut end) = if let Some(suffix) = range_spec.strip_prefix('-') {
            // bytes=-suffix (last N bytes)
            let suffix_len: u64 = match suffix.parse() {
                Ok(n) if n > 0 => n,
                _ => return build_416_response(file_size),
            };
            let start = file_size.saturating_sub(suffix_len);
            (start, file_size - 1)
        } else if let Some((start_str, end_str)) = range_spec.split_once('-') {
            let start: u64 = match start_str.parse() {
                Ok(n) => n,
                _ => return build_416_response(file_size),
            };
            if start >= file_size {
                return build_416_response(file_size);
            }
            let end = if end_str.is_empty() {
                file_size - 1
            } else {
                match end_str.parse::<u64>() {
                    Ok(n) => n.min(file_size - 1),
                    _ => return build_416_response(file_size),
                }
            };
            if end < start {
                return build_416_response(file_size);
            }
            (start, end)
        } else {
            return build_416_response(file_size);
        };

        // Cap chunk size to avoid loading huge ranges into memory
        let max_end = start + STREAM_MAX_CHUNK - 1;
        if end > max_end {
            end = max_end.min(file_size - 1);
        }

        let length = end - start + 1;
        if file.seek(SeekFrom::Start(start)).is_err() {
            return build_416_response(file_size);
        }
        let mut buf = vec![0u8; length as usize];
        if let Err(e) = file.read_exact(&mut buf) {
            return Response::builder()
                .status(500)
                .header("Access-Control-Allow-Origin", "*")
                .body(format!("Read error: {}", e).as_bytes().to_vec())
                .unwrap();
        }

        Response::builder()
            .status(206)
            .header("Content-Type", mime_type)
            .header("Content-Length", length.to_string())
            .header("Content-Range", format!("bytes {}-{}/{}", start, end, file_size))
            .header("Accept-Ranges", "bytes")
            .header("Access-Control-Allow-Origin", "*")
            // Prevent WebKitGTK from caching partial responses — media players
            // need fresh range requests to stream subsequent chunks
            .header("Cache-Control", "no-cache, no-store, must-revalidate")
            .header("Pragma", "no-cache")
            .body(buf)
            .unwrap()
    } else {
        // No Range header — serve full file
        let mut buf = Vec::with_capacity(file_size as usize);
        if let Err(e) = file.read_to_end(&mut buf) {
            return Response::builder()
                .status(500)
                .header("Access-Control-Allow-Origin", "*")
                .body(format!("Read error: {}", e).as_bytes().to_vec())
                .unwrap();
        }

        Response::builder()
            .status(200)
            .header("Content-Type", mime_type)
            .header("Content-Length", file_size.to_string())
            .header("Accept-Ranges", "bytes")
            .header("Access-Control-Allow-Origin", "*")
            .body(buf)
            .unwrap()
    }
}

/// Handle tdasset:// protocol requests for serving static assets with path validation.
/// This provides a secure alternative to the built-in asset:// protocol by validating
/// that requested files are within user-accessible directories.
/// Called from register_asynchronous_uri_scheme_protocol on a background thread
/// to avoid blocking the main thread during file I/O (critical for media streaming).
fn tdasset_protocol_handler(request: Request<Vec<u8>>) -> Response<Vec<u8>> {
    let uri = request.uri();
    // The path comes URL-encoded from convertFileSrc, decode it
    let raw_path = uri.path();
    let decoded_path = urlencoding::decode(raw_path)
        .map(|s| s.into_owned())
        .unwrap_or_else(|_| raw_path.to_string());

    // Remove leading slash(es) but preserve the path structure
    // For Unix: /home/user/file.jpg -> home/user/file.jpg (we'll add / back)
    // For Windows: /C:/Users/file.jpg -> C:/Users/file.jpg
    let path = decoded_path.trim_start_matches('/');

    // Reconstruct the absolute path
    let file_path = if path.len() >= 2 && path.chars().nth(1) == Some(':') {
        // Windows path like C:/Users/...
        PathBuf::from(path)
    } else {
        // Unix path - add leading /
        PathBuf::from(format!("/{}", path))
    };

    // Security: Validate the path doesn't contain traversal sequences
    let path_str = file_path.to_string_lossy();
    if drag_drop::sanitize::validate_file_path(&path_str).is_none() {
        eprintln!("[TiddlyDesktop] Security: Blocked path traversal in tdasset protocol: {}", path_str);
        return Response::builder()
            .status(403)
            .header("Access-Control-Allow-Origin", "*")
            .body("Access denied: path contains invalid sequences".as_bytes().to_vec())
            .unwrap();
    }

    // Security: Validate the path is user-accessible
    match dunce::canonicalize(&file_path) {
        Ok(canonical) => {
            if !drag_drop::sanitize::is_user_accessible_path(&canonical) {
                eprintln!("[TiddlyDesktop] Security: Blocked access to system path via tdasset: {}", canonical.display());
                return Response::builder()
                    .status(403)
                    .header("Access-Control-Allow-Origin", "*")
                    .body("Access denied: path is outside user-accessible directories".as_bytes().to_vec())
                    .unwrap();
            }

            // Serve the file with range request support for media playback
            let mime_type = utils::get_mime_type(&canonical);
            serve_file_with_range_support(&canonical, mime_type, request.headers())
        }
        Err(e) => {
            Response::builder()
                .status(404)
                .header("Access-Control-Allow-Origin", "*")
                .body(format!("File not found: {}", e).as_bytes().to_vec())
                .unwrap()
        }
    }
}

/// Handle tdlib:// protocol requests - serves bundled library assets (PDF.js)
/// Separate from tdasset:// which validates user-accessible paths only
#[cfg(not(target_os = "android"))]
fn tdlib_protocol_handler(app: &tauri::AppHandle, request: Request<Vec<u8>>) -> Response<Vec<u8>> {
    let uri = request.uri();
    let raw_path = uri.path();
    let decoded_path = urlencoding::decode(raw_path)
        .map(|s| s.into_owned())
        .unwrap_or_else(|_| raw_path.to_string());
    let path = decoded_path.trim_start_matches('/');

    // Security: reject path traversal
    if path.contains("..") || path.contains('\\') {
        return Response::builder()
            .status(403)
            .header("Access-Control-Allow-Origin", "*")
            .body("Access denied: invalid path".as_bytes().to_vec())
            .unwrap();
    }

    let resource_dir = match get_resource_dir_path(app) {
        Some(d) => d,
        None => {
            return Response::builder()
                .status(500)
                .header("Access-Control-Allow-Origin", "*")
                .body("Resource directory not found".as_bytes().to_vec())
                .unwrap();
        }
    };

    // Try multiple paths: bundled structure, tarball structure, and dev-mode fallback
    let bundled_path = resource_dir.join("resources").join("tdlib").join(path);
    let tarball_path = resource_dir.join("tdlib").join(path);
    // Dev-mode: resolve relative to executable (src-tauri/target/debug/ → src-tauri/resources/tdlib/)
    let dev_path = std::env::current_exe().ok()
        .and_then(|exe| exe.parent().map(|d| d.join("..").join("..").join("resources").join("tdlib").join(path)))
        .unwrap_or_else(|| PathBuf::from("src-tauri").join("resources").join("tdlib").join(path));

    let file_path = if bundled_path.exists() {
        bundled_path
    } else if tarball_path.exists() {
        tarball_path
    } else if dev_path.exists() {
        dev_path
    } else {
        return Response::builder()
            .status(404)
            .header("Access-Control-Allow-Origin", "*")
            .body(format!("File not found: {}", path).as_bytes().to_vec())
            .unwrap();
    };

    // Verify canonical path is within a tdlib dir (security check)
    let tdlib_dir = file_path.parent().and_then(|p| {
        // Walk up to find the "tdlib" ancestor
        let mut current = p;
        loop {
            if current.file_name().map(|n| n == "tdlib").unwrap_or(false) {
                return Some(current.to_path_buf());
            }
            current = current.parent()?;
        }
    });
    let tdlib_dir = match tdlib_dir {
        Some(d) => d,
        None => {
            return Response::builder()
                .status(403)
                .header("Access-Control-Allow-Origin", "*")
                .body("Access denied: not in tdlib directory".as_bytes().to_vec())
                .unwrap();
        }
    };
    match (dunce::canonicalize(&file_path), dunce::canonicalize(&tdlib_dir)) {
        (Ok(canonical_file), Ok(canonical_base)) => {
            if !canonical_file.starts_with(&canonical_base) {
                return Response::builder()
                    .status(403)
                    .header("Access-Control-Allow-Origin", "*")
                    .body("Access denied: path outside tdlib directory".as_bytes().to_vec())
                    .unwrap();
            }

            match std::fs::read(&canonical_file) {
                Ok(data) => {
                    let mime_type = utils::get_mime_type(&canonical_file);
                    Response::builder()
                        .status(200)
                        .header("Content-Type", mime_type)
                        .header("Access-Control-Allow-Origin", "*")
                        .body(data)
                        .unwrap()
                }
                Err(e) => {
                    Response::builder()
                        .status(404)
                        .header("Access-Control-Allow-Origin", "*")
                        .body(format!("File not found: {}", e).as_bytes().to_vec())
                        .unwrap()
                }
            }
        }
        _ => {
            Response::builder()
                .status(404)
                .header("Access-Control-Allow-Origin", "*")
                .body("File not found".as_bytes().to_vec())
                .unwrap()
        }
    }
}

/// Handle wiki:// protocol requests
fn wiki_protocol_handler(app: &tauri::AppHandle, request: Request<Vec<u8>>) -> Response<Vec<u8>> {
    let uri = request.uri();
    let full_path = uri.path().trim_start_matches('/');

    // Extract path without query string and parse query params
    let (path, query_params) = {
        let query = uri.query();
        let path = full_path.split('?').next().unwrap_or(full_path);
        (path, parse_query_string(query))
    };

    // Single-tiddler mode params
    let single_tiddler = query_params.get("tiddler").cloned();
    let single_template = query_params.get("template").cloned();
    let parent_window = query_params.get("parent").cloned();
    let single_variables = query_params.get("variables").cloned(); // JSON-encoded extra variables

    // Handle OPTIONS preflight requests for CORS (required for PUT requests on some platforms)
    if request.method() == "OPTIONS" {
        return Response::builder()
            .status(200)
            .header("Access-Control-Allow-Origin", "*")
            .header("Access-Control-Allow-Methods", "GET, PUT, POST, OPTIONS")
            .header("Access-Control-Allow-Headers", "Content-Type")
            .header("Access-Control-Max-Age", "86400")
            .body(Vec::new())
            .unwrap();
    }

    // Handle title-sync requests: wikifile://title-sync/{label}/{title}
    if path.starts_with("title-sync/") {
        let parts: Vec<&str> = path.strip_prefix("title-sync/").unwrap().splitn(2, '/').collect();
        if parts.len() == 2 {
            // Update window title (desktop only - Android has no window titles)
            #[cfg(not(target_os = "android"))]
            {
                let label = urlencoding::decode(parts[0]).unwrap_or_default().to_string();
                let title = urlencoding::decode(parts[1]).unwrap_or_default().to_string();
                let app_clone = app.clone();
                let app_inner = app_clone.clone();
                let _ = app_clone.run_on_main_thread(move || {
                    if let Some(window) = app_inner.get_webview_window(&label) {
                        let _ = window.set_title(&title);
                    }
                });
            }
        }
        return Response::builder()
            .status(200)
            .header("Access-Control-Allow-Origin", "*")
            .body(Vec::new())
            .unwrap();
    }

    // Handle save requests: wikifile://save/{base64-encoded-path}
    // Body contains the wiki content
    if path.starts_with("save/") {
        let path_key = path.strip_prefix("save/").unwrap();
        let decoded = match utils::base64_url_decode(path_key) {
            Some(d) => d,
            None => {
                return Response::builder()
                    .status(400)
                    .body("Invalid path".as_bytes().to_vec())
                    .unwrap();
            }
        };

        // Check if this is an Android SAF URI (content:// or JSON-serialized FileUri)
        let is_saf_uri = decoded.starts_with("content://") || decoded.starts_with('{');

        let wiki_path = if is_saf_uri {
            // Android SAF URI - bypass filesystem path validation
            // SAF permissions are handled by Android's permission system
            PathBuf::from(&decoded)
        } else {
            // Desktop filesystem path - validate for security
            match drag_drop::sanitize::validate_wiki_path_for_write(&decoded) {
                Ok(validated_path) => {
                    // Also check user-accessible
                    if !drag_drop::sanitize::is_user_accessible_path(&validated_path) {
                        return Response::builder()
                            .status(403)
                            .header("Access-Control-Allow-Origin", "*")
                            .body("Access denied: path is outside user-accessible directories".as_bytes().to_vec())
                            .unwrap();
                    }
                    validated_path
                }
                Err(e) => {
                    eprintln!("[TiddlyDesktop] Security: Invalid save path: {}", e);
                    return Response::builder()
                        .status(400)
                        .header("Access-Control-Allow-Origin", "*")
                        .body(format!("Invalid path: {}", e).as_bytes().to_vec())
                        .unwrap();
                }
            }
        };

        let content = String::from_utf8_lossy(request.body()).to_string();

        // Check if backups should be created for this wiki
        let wiki_path_str = wiki_path.to_string_lossy();
        let state = app.state::<AppState>();
        let should_backup = should_create_backup(app, &state, wiki_path_str.as_ref());

        if should_backup {
            #[cfg(target_os = "android")]
            if is_saf_uri {
                // Android SAF backup support
                // Get custom backup directory or try to auto-detect from tree access
                let custom_backup_dir = get_wiki_backup_dir(app, wiki_path_str.as_ref());

                if let Ok(Some(backup_dir_uri)) = android::saf::get_backup_directory(&decoded, custom_backup_dir.as_deref()) {
                    // Get filename stem for backup naming
                    let filename_stem = android::saf::get_display_name(&decoded)
                        .map(|name| {
                            name.strip_suffix(".html")
                                .or_else(|| name.strip_suffix(".htm"))
                                .unwrap_or(&name)
                                .to_string()
                        })
                        .unwrap_or_else(|_| "wiki".to_string());

                    // Create backup
                    if let Err(e) = android::saf::create_backup(&decoded, &backup_dir_uri, &filename_stem) {
                        eprintln!("[TiddlyDesktop] Failed to create Android backup: {}", e);
                    } else {
                        // Clean up old backups
                        let backup_count = wiki_storage::get_wiki_backup_count(app, wiki_path_str.as_ref()).unwrap_or(20);
                        let _ = android::saf::cleanup_old_backups(&backup_dir_uri, &filename_stem, backup_count as usize);
                    }
                } else {
                    eprintln!("[TiddlyDesktop] Android backup skipped: no backup directory available. Set a custom backup directory in wiki settings.");
                }
            }

            #[cfg(not(target_os = "android"))]
            if !is_saf_uri {
                // Desktop filesystem backup
                if wiki_path.exists() {
                    if let Some(parent) = wiki_path.parent() {
                        let filename = wiki_path.file_stem().and_then(|s| s.to_str()).unwrap_or("wiki");

                        // Get custom backup directory if set, otherwise use default
                        // Security: Validate custom backup directory is user-accessible
                        let backup_dir = match get_wiki_backup_dir(app, wiki_path_str.as_ref()) {
                            Some(custom_dir) => {
                                // Validate the custom directory path
                                match drag_drop::sanitize::validate_file_path(&custom_dir) {
                                    Some(_) => {
                                        let dir_path = PathBuf::from(&custom_dir);
                                        // If directory exists, verify it's user-accessible
                                        if dir_path.exists() {
                                            if let Ok(canonical) = dunce::canonicalize(&dir_path) {
                                                if drag_drop::sanitize::is_user_accessible_path(&canonical) {
                                                    canonical
                                                } else {
                                                    eprintln!("[TiddlyDesktop] Security: Custom backup dir not user-accessible, using default");
                                                    parent.join(format!("{}.backups", filename))
                                                }
                                            } else {
                                                parent.join(format!("{}.backups", filename))
                                            }
                                        } else {
                                            // Directory doesn't exist yet, check parent is user-accessible
                                            if let Some(dir_parent) = dir_path.parent() {
                                                if let Ok(canonical_parent) = dunce::canonicalize(dir_parent) {
                                                    if drag_drop::sanitize::is_user_accessible_path(&canonical_parent) {
                                                        dir_path
                                                    } else {
                                                        eprintln!("[TiddlyDesktop] Security: Custom backup dir parent not user-accessible, using default");
                                                        parent.join(format!("{}.backups", filename))
                                                    }
                                                } else {
                                                    parent.join(format!("{}.backups", filename))
                                                }
                                            } else {
                                                parent.join(format!("{}.backups", filename))
                                            }
                                        }
                                    }
                                    None => {
                                        eprintln!("[TiddlyDesktop] Security: Invalid custom backup dir path, using default");
                                        parent.join(format!("{}.backups", filename))
                                    }
                                }
                            }
                            None => parent.join(format!("{}.backups", filename)),
                        };
                        let _ = std::fs::create_dir_all(&backup_dir);

                        let timestamp = Local::now().format("%Y%m%d-%H%M%S");
                        let backup_name = format!("{}.{}.html", filename, timestamp);
                        let backup_path = backup_dir.join(backup_name);
                        let _ = std::fs::copy(&wiki_path, &backup_path);

                        // Clean up old backups (synchronous version)
                        let backup_count = wiki_storage::get_wiki_backup_count(app, wiki_path_str.as_ref()).unwrap_or(20);
                        if backup_count > 0 {
                            if let Ok(entries) = std::fs::read_dir(&backup_dir) {
                                let mut backups: Vec<PathBuf> = entries
                                    .filter_map(|e| e.ok())
                                    .map(|e| e.path())
                                    .filter(|p| p.extension().map(|e| e == "html").unwrap_or(false))
                                    .collect();
                                backups.sort();
                                backups.reverse();
                                for old_backup in backups.into_iter().skip(backup_count as usize) {
                                    let _ = std::fs::remove_file(old_backup);
                                }
                            }
                        }
                    }
                }
            }
        }

        // Write wiki file (uses fs_abstraction for atomic writes and Android SAF support)
        match fs_abstraction::write_wiki_file(&wiki_path, &content) {
            Ok(_) => {
                return Response::builder()
                    .status(200)
                    .header("Access-Control-Allow-Origin", "*")
                    .body(Vec::new())
                    .unwrap();
            }
            Err(e) => {
                return Response::builder()
                    .status(500)
                    .body(format!("Failed to save: {}", e).into_bytes())
                    .unwrap();
            }
        }
    }

    // Handle tdlib asset requests: wikifile://localhost/__tdlib__/{path}
    // WebKitGTK blocks cross-scheme fetch requests (wikifile:// → tdlib://),
    // so we route bundled library assets through wikifile:// for same-origin access.
    #[cfg(not(target_os = "android"))]
    if path.starts_with("__tdlib__/") {
        let tdlib_path = path.strip_prefix("__tdlib__/").unwrap();
        let synthetic_uri = format!("tdlib://localhost/{}", tdlib_path);
        let synthetic_request = Request::builder()
            .uri(&synthetic_uri)
            .body(Vec::new())
            .unwrap();
        return tdlib_protocol_handler(app, synthetic_request);
    }

    // Look up the actual file path
    let state = app.state::<AppState>();
    let paths = state.wiki_paths.lock().unwrap();

    let file_path = match paths.get(path) {
        Some(p) => p.clone(),
        None => {
            match utils::base64_url_decode(path) {
                Some(decoded) => PathBuf::from(decoded),
                None => {
                    // Not a base64-encoded wiki path - this might be a _canonical_uri file request
                    // Get the wiki directory from the Referer header
                    drop(paths); // Release lock before handling file request

                    let referer = request.headers()
                        .get("referer")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");

                    // Extract wiki path from referer: wikifile://localhost/{base64_wiki_path}
                    let wiki_dir = if let Some(ref_path) = referer.strip_prefix("wikifile://localhost/") {
                        // The referer path might have query params or fragments, strip them
                        let ref_path = ref_path.split('?').next().unwrap_or(ref_path);
                        let ref_path = ref_path.split('#').next().unwrap_or(ref_path);

                        if let Some(decoded_wiki_path) = utils::base64_url_decode(ref_path) {
                            PathBuf::from(&decoded_wiki_path)
                                .parent()
                                .map(|p| p.to_path_buf())
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    // Security: Check for path traversal in the raw path
                    if drag_drop::sanitize::validate_file_path(path).is_none() {
                        eprintln!("[TiddlyDesktop] Security: Blocked path traversal in wikifile protocol: {}", path);
                        return Response::builder()
                            .status(403)
                            .header("Access-Control-Allow-Origin", "*")
                            .body("Access denied: path contains invalid sequences".as_bytes().to_vec())
                            .unwrap();
                    }

                    // Resolve the file path
                    let resolved_path = if utils::is_absolute_filesystem_path(path) {
                        // Absolute path - validate it's user-accessible
                        let path_buf = PathBuf::from(path);
                        match dunce::canonicalize(&path_buf) {
                            Ok(canonical) => {
                                if !drag_drop::sanitize::is_user_accessible_path(&canonical) {
                                    eprintln!("[TiddlyDesktop] Security: Blocked access to system path via wikifile: {}", canonical.display());
                                    return Response::builder()
                                        .status(403)
                                        .header("Access-Control-Allow-Origin", "*")
                                        .body("Access denied: path is outside user-accessible directories".as_bytes().to_vec())
                                        .unwrap();
                                }
                                canonical
                            }
                            Err(_) => path_buf, // File might not exist yet; let read() handle error
                        }
                    } else if let Some(ref wiki_dir) = wiki_dir {
                        // Relative path - resolve relative to wiki directory
                        let joined = wiki_dir.join(path);
                        // Canonicalize and verify it's still within wiki_dir (prevent ../ escapes)
                        match dunce::canonicalize(&joined) {
                            Ok(canonical) => {
                                // Canonicalize wiki_dir too for proper comparison
                                let canonical_wiki_dir = dunce::canonicalize(wiki_dir)
                                    .unwrap_or_else(|_| wiki_dir.clone());
                                if !canonical.starts_with(&canonical_wiki_dir) {
                                    eprintln!("[TiddlyDesktop] Security: Blocked path escape from wiki dir: {} -> {}", path, canonical.display());
                                    return Response::builder()
                                        .status(403)
                                        .header("Access-Control-Allow-Origin", "*")
                                        .body("Access denied: path escapes wiki directory".as_bytes().to_vec())
                                        .unwrap();
                                }
                                canonical
                            }
                            Err(_) => joined, // File might not exist; let read() handle error
                        }
                    } else {
                        // No wiki context and not absolute - can't resolve
                        return Response::builder()
                            .status(404)
                            .header("Access-Control-Allow-Origin", "*")
                            .body("File not found: no wiki context for relative path".as_bytes().to_vec())
                            .unwrap();
                    };

                    // Serve the file with range request support for media playback
                    let mime_type = utils::get_mime_type(&resolved_path);
                    return serve_file_with_range_support(&resolved_path, mime_type, request.headers());
                }
            }
        }
    };

    // Note: window_label and is_main_wiki are set by initialization_script(), not needed here
    drop(paths); // Release the lock before file I/O

    // Generate the save URL for this wiki
    let save_url = format!("wikifile://localhost/save/{}", path);

    // Prepare single-tiddler mode params for injection
    // Use serde_json for safe string escaping to prevent injection attacks
    let single_tiddler_js = single_tiddler.as_ref()
        .map(|t| format!("window.__SINGLE_TIDDLER__ = {};", serde_json::to_string(t).unwrap_or_else(|_| "\"\"".to_string())))
        .unwrap_or_default();
    let single_template_js = single_template.as_ref()
        .map(|t| format!("window.__SINGLE_TEMPLATE__ = {};", serde_json::to_string(t).unwrap_or_else(|_| "\"\"".to_string())))
        .unwrap_or_default();
    let parent_window_js = parent_window.as_ref()
        .map(|p| format!("window.__PARENT_WINDOW__ = {};", serde_json::to_string(p).unwrap_or_else(|_| "\"\"".to_string())))
        .unwrap_or_default();
    let single_variables_js = single_variables.as_ref()
        .map(|v| {
            // Security: Validate that the variables parameter is valid JSON before injection
            match serde_json::from_str::<serde_json::Value>(v) {
                Ok(_) => format!(r#"window.__SINGLE_VARIABLES__ = {};"#, v),
                Err(e) => {
                    eprintln!("[TiddlyDesktop] Security: Invalid JSON in variables parameter: {}", e);
                    "window.__SINGLE_VARIABLES__ = {};".to_string()
                }
            }
        })
        .unwrap_or_default();

    // Validate that this is a TiddlyWiki file before loading
    if let Err(e) = validate_tiddlywiki_file(&file_path) {
        eprintln!("[TiddlyDesktop] Refusing to load non-TiddlyWiki file: {} - {}", file_path.display(), e);
        return Response::builder()
            .status(403)
            .header("Content-Type", "text/plain")
            .body(format!("Security error: {}", e).into_bytes())
            .unwrap();
    }

    // Read file content (uses fs_abstraction for Android SAF support)
    let read_result = fs_abstraction::read_wiki_file(&file_path);

    match read_result {
        Ok(content) => {
            // Inject saver and additional functionality for TiddlyWiki
            // Note: __WIKI_PATH__, __WINDOW_LABEL__, __IS_MAIN_WIKI__ are already set by initialization_script()

            // For single-tiddler windows, inject preload tiddlers to use single-tiddler layout
            // This must run BEFORE TiddlyWiki's boot.js to configure the layout
            let single_tiddler_preload = if let Some(ref tiddler) = single_tiddler {
                let template = single_template.as_deref()
                    .unwrap_or("$:/core/templates/single.tiddler.window");
                // Use serde_json for safe JavaScript string escaping
                let tiddler_json = serde_json::to_string(tiddler).unwrap_or_else(|_| "\"\"".to_string());
                let template_json = serde_json::to_string(template).unwrap_or_else(|_| "\"\"".to_string());
                // For wikitext attributes, we need to escape for HTML attribute context
                // Using JSON-encoded strings in the wikitext (which handles quotes, newlines, etc.)
                format!(r##"<script>
// TiddlyDesktop: Configure single-tiddler layout BEFORE boot
(function() {{
    window.$tw = window.$tw || {{}};
    $tw.preloadTiddlers = $tw.preloadTiddlers || [];

    var tiddlerTitle = {tiddler_json};
    var templateTitle = {template_json};

    // Set layout to use single-tiddler wrapper
    $tw.preloadTiddlers.push({{
        title: "$:/layout",
        text: "$:/TiddlyDesktop/SingleTiddlerLayout"
    }});

    // Inject a custom wrapper template that sets currentTiddler
    // Build the wikitext dynamically to avoid escaping issues
    $tw.preloadTiddlers.push({{
        title: "$:/TiddlyDesktop/SingleTiddlerLayout",
        text: '<$set name="currentTiddler" value="' + tiddlerTitle.replace(/"/g, '&quot;') + '"><$transclude tiddler="' + templateTitle.replace(/"/g, '&quot;') + '" mode="block"/></$set>'
    }});

    // Store the tiddler title for reference
    window.__SINGLE_TIDDLER_TITLE__ = tiddlerTitle;
}})();
</script>"##, tiddler_json=tiddler_json, template_json=template_json)
            } else {
                String::new()
            };

            // Media controls CSS is injected inline via the initialization script
            // (WebKitGTK doesn't load CSS from custom URI schemes)
            let media_css_injection = "";

            let script_injection = format!(
                r##"{media_css_injection}{single_tiddler_preload}
<script>
window.__SAVE_URL__ = "{save_url}";
{single_tiddler_js}
{single_template_js}
{parent_window_js}
{single_variables_js}

// TiddlyDesktop initialization - handles both normal and encrypted wikis
(function() {{
    // Prevent double execution if protocol handler script runs multiple times
    if (window.__TD_PROTOCOL_SCRIPT_LOADED__) {{
        console.log('[TiddlyDesktop] Protocol handler script already loaded - skipping duplicate');
        return;
    }}
    window.__TD_PROTOCOL_SCRIPT_LOADED__ = true;

    // Close child tiddler windows (opened via tm-open-window) on refresh or close.
    // Only applies to wiki windows (which have __WIKI_PATH__), not the landing page.
    window.addEventListener('beforeunload', function() {{
        if (!window.__WIKI_PATH__ || !window.__tiddlyDesktopWindows) return;
        var windows = window.__tiddlyDesktopWindows;
        var keys = Object.keys(windows);
        for (var i = 0; i < keys.length; i++) {{
            var info = windows[keys[i]];
            if (info && info.label) {{
                window.__TAURI__.core.invoke('close_window_by_label', {{ label: info.label }}).catch(function() {{}});
            }}
        }}
        window.__tiddlyDesktopWindows = {{}};
    }});

    var SAVE_URL = "{save_url_inner}";

    // Check if this is an encrypted wiki
    function isEncryptedWiki() {{
        return !!document.getElementById('encryptedStoreArea');
    }}

    // Wait for TiddlyWiki to be fully ready (including decryption if needed)
    function waitForTiddlyWiki(callback) {{
        // For encrypted wikis, we must wait for $tw.wiki to exist
        // This means decryption has completed and boot has finished
        if (typeof $tw !== 'undefined' && $tw.wiki) {{
            callback();
        }} else {{
            setTimeout(function() {{ waitForTiddlyWiki(callback); }}, 50);
        }}
    }}

    // Main initialization that runs after TiddlyWiki is ready
    function initializeTiddlyDesktop() {{

    // Define the saver module globally so TiddlyWiki can find it during boot
    window.$TiddlyDesktopSaver = {{
        info: {{
            name: 'tiddlydesktop',
            priority: 5000,
            capabilities: ['save', 'autosave']
        }},
        canSave: function(wiki) {{
            return true;
        }},
        create: function(wiki) {{
            return {{
                wiki: wiki,
                info: {{
                    name: 'tiddlydesktop',
                    priority: 5000,
                    capabilities: ['save', 'autosave']
                }},
                save: function(text, method, callback) {{
                    var self = this;
                    var wikiPath = window.__WIKI_PATH__;

                    // After a successful local save, trigger any configured cloud savers
                    // (GitHub, GitLab, Gitea, Tiddlyhost) as best-effort background operations
                    var cloudSaverNames = ['github', 'gitlab', 'Gitea', 'upload'];
                    function chainCloudSavers() {{
                        if (!$tw || !$tw.saverHandler) return;
                        var savers = $tw.saverHandler.savers;
                        for (var i = savers.length - 1; i >= 0; i--) {{
                            var saver = savers[i];
                            if (cloudSaverNames.indexOf(saver.info.name) === -1) continue;
                            if (saver.info.capabilities.indexOf(method) === -1) continue;
                            (function(s) {{
                                try {{
                                    if (s.save(text, method, function(err) {{
                                        if (err) {{
                                            console.warn('[TiddlyDesktop] Cloud saver \'' + s.info.name + '\' failed: ' + err);
                                        }} else {{
                                            console.log('[TiddlyDesktop] Cloud saver \'' + s.info.name + '\' succeeded');
                                        }}
                                    }})) {{
                                        console.log('[TiddlyDesktop] Triggered cloud saver: ' + s.info.name);
                                    }}
                                }} catch(e) {{
                                    console.warn('[TiddlyDesktop] Cloud saver \'' + s.info.name + '\' threw: ' + e);
                                }}
                            }})(saver);
                        }}
                    }}

                    // Try Tauri IPC first (works reliably on all platforms)
                    if(window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke) {{
                        var savePromise = window.__TAURI__.core.invoke('save_wiki', {{
                            path: wikiPath,
                            content: text
                        }}).then(function() {{
                            window.__TD_SAVE_PROMISE__ = null;
                            callback(null);
                            chainCloudSavers();
                        }}).catch(function(err) {{
                            window.__TD_SAVE_PROMISE__ = null;
                            // IPC failed, try fetch as fallback
                            saveViaFetch(text, callback);
                        }});
                        window.__TD_SAVE_PROMISE__ = savePromise;
                    }} else {{
                        // No Tauri IPC, use fetch
                        saveViaFetch(text, callback);
                    }}

                    function saveViaFetch(content, cb) {{
                        fetch(SAVE_URL, {{
                            method: 'PUT',
                            body: content
                        }}).then(function(response) {{
                            if(response.ok) {{
                                cb(null);
                                chainCloudSavers();
                            }} else {{
                                response.text().then(function(errText) {{
                                    cb('Save failed (HTTP ' + response.status + '): ' + (errText || response.statusText));
                                }}).catch(function() {{
                                    cb('Save failed: HTTP ' + response.status);
                                }});
                            }}
                        }}).catch(function(err) {{
                            cb('Save failed (fetch): ' + err.toString());
                        }});
                    }}

                    return true;
                }}
            }};
        }}
    }};

    // Hook into TiddlyWiki's module registration
    // Supports both modern (5.3.7+) and older TiddlyWiki versions
    function registerWithTiddlyWiki() {{
        if(typeof $tw === 'undefined') {{
            setTimeout(registerWithTiddlyWiki, 50);
            return;
        }}

        var MODULE_TITLE = '$:/plugins/tiddlydesktop/saver';

        // Method 1: Use $tw.modules.define() if available (TiddlyWiki 5.1.3+)
        if($tw.modules && typeof $tw.modules.define === 'function') {{
            $tw.modules.define(MODULE_TITLE, 'saver', window.$TiddlyDesktopSaver);
            console.log('TiddlyDesktop saver: registered via $tw.modules.define()');
        }}

        // Method 2: ALWAYS also register in $tw.modules.types directly
        // In TiddlyWiki < 5.3.7, forEachModuleOfType() only checks $tw.modules.types,
        // NOT $tw.modules.titles (which is where define() stores modules).
        // So define() alone is not enough for older versions.
        if($tw.modules && $tw.modules.types) {{
            $tw.modules.types['saver'] = $tw.modules.types['saver'] || {{}};
            $tw.modules.types['saver'][MODULE_TITLE] = window.$TiddlyDesktopSaver;
            console.log('TiddlyDesktop saver: registered in $tw.modules.types[saver]');
        }}

        // Method 3: Wait for saverHandler and add directly (works on all versions)
        // This is a backup method that ensures the saver is always available
        function addToSaverHandler() {{
            if(!$tw.saverHandler) {{
                setTimeout(addToSaverHandler, 50);
                return;
            }}

            // Check if already added
            var alreadyAdded = false;
            if($tw.saverHandler.savers) {{
                alreadyAdded = $tw.saverHandler.savers.some(function(s) {{
                    return s && s.info && s.info.name === 'tiddlydesktop';
                }});
            }}

            if(!alreadyAdded) {{
                var saver = window.$TiddlyDesktopSaver.create($tw.wiki);
                // Add to array and re-sort (TiddlyWiki iterates backwards, so highest priority must be at the END)
                $tw.saverHandler.savers = $tw.saverHandler.savers || [];
                $tw.saverHandler.savers.push(saver);
                $tw.saverHandler.savers.sort(function(a, b) {{
                    var aPriority = (a && a.info && a.info.priority) || 0;
                    var bPriority = (b && b.info && b.info.priority) || 0;
                    if(aPriority < bPriority) {{
                        return -1;
                    }} else if(aPriority > bPriority) {{
                        return 1;
                    }}
                    return 0;
                }});
                console.log('TiddlyDesktop saver: added to saverHandler directly');
            }}
        }}

        addToSaverHandler();

        // Intercept tm-browser-refresh to wait for any pending save to complete
        // Plugin "save and reload" buttons dispatch tm-save-wiki then tm-browser-refresh
        // sequentially, but the save is async — reload would happen before save finishes.
        $tw.rootWidget.addEventListener("tm-browser-refresh", function(event) {{
            if (window.__TD_SAVE_PROMISE__) {{
                console.log('[TiddlyDesktop] Waiting for save to complete before reload...');
                window.__TD_SAVE_PROMISE__.then(function() {{
                    setTimeout(function() {{ window.location.reload(); }}, 100);
                }}).catch(function() {{
                    setTimeout(function() {{ window.location.reload(); }}, 100);
                }});
            }} else {{
                window.location.reload();
            }}
            return false;
        }});
    }}

    registerWithTiddlyWiki();

    // Title sync and favicon sync are handled by initialization_script
    // (title_sync.js, favicon_sync.js) for all wiki windows

    // Single-tiddler window mode is now handled via preload tiddlers
    // The $:/layout tiddler is set before boot to use $:/TiddlyDesktop/SingleTiddlerLayout

    }} // End of initializeTiddlyDesktop

    // Start initialization based on whether wiki is encrypted
    // We need to wait for DOM to check for encryptedStoreArea
    function startInit() {{
        if (isEncryptedWiki()) {{
            // Encrypted wiki: wait for TiddlyWiki to fully boot (including decryption)
            console.log('TiddlyDesktop: Encrypted wiki detected, waiting for decryption...');
            waitForTiddlyWiki(function() {{
                console.log('TiddlyDesktop: Decryption complete, initializing...');
                initializeTiddlyDesktop();
            }});
        }} else {{
            // Normal wiki: initialize immediately (our code waits for $tw internally)
            initializeTiddlyDesktop();
        }}
    }}

    // Check DOM readiness before looking for encryptedStoreArea
    if (document.readyState === 'loading') {{
        document.addEventListener('DOMContentLoaded', startInit);
    }} else {{
        startInit();
    }}

    // External attachments support is provided by the initialization script (get_dialog_init_script)
}})();
</script>"##,
                media_css_injection = media_css_injection,
                single_tiddler_preload = single_tiddler_preload,
                save_url = save_url,
                single_tiddler_js = single_tiddler_js,
                single_template_js = single_template_js,
                parent_window_js = parent_window_js,
                single_variables_js = single_variables_js,
                save_url_inner = save_url
            );

            // Find <head> tag position - only search first 4KB, don't lowercase the whole file
            let search_area = &content[..content.len().min(4096)];
            let head_pos = search_area.find("<head")
                .or_else(|| search_area.find("<HEAD"))
                .or_else(|| search_area.find("<Head"));

            // Build response efficiently without extra allocations
            let mut response_bytes = Vec::with_capacity(content.len() + script_injection.len() + 100);

            if let Some(head_start) = head_pos {
                if let Some(close_offset) = content[head_start..].find('>') {
                    let insert_pos = head_start + close_offset + 1;
                    response_bytes.extend_from_slice(content[..insert_pos].as_bytes());
                    response_bytes.extend_from_slice(script_injection.as_bytes());
                    response_bytes.extend_from_slice(content[insert_pos..].as_bytes());
                } else {
                    response_bytes.extend_from_slice(script_injection.as_bytes());
                    response_bytes.extend_from_slice(content.as_bytes());
                }
            } else {
                response_bytes.extend_from_slice(script_injection.as_bytes());
                response_bytes.extend_from_slice(content.as_bytes());
            }

            Response::builder()
                .status(200)
                .header("Content-Type", "text/html; charset=utf-8")
                .header("Access-Control-Allow-Origin", "*")
                .body(response_bytes)
                .unwrap()
        }
        Err(e) => Response::builder()
            .status(500)
            .body(format!("Failed to read wiki: {}", e).into_bytes())
            .unwrap(),
    }
}

/// Reveal the main window, or recreate it if it was closed
#[cfg(not(target_os = "android"))]
fn reveal_or_create_main_window(app_handle: &tauri::AppHandle) {
    // Try to get existing window first
    if let Some(window) = app_handle.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }

    // Window was closed - recreate it
    let state = app_handle.state::<AppState>();
    let main_wiki_path = state.main_wiki_path.clone();
    let path_key = utils::base64_url_encode(&main_wiki_path.to_string_lossy());
    let wiki_url = format!("wikifile://localhost/{}", path_key);

    // Load saved window state for landing page
    let saved_state = wiki_storage::get_window_state(app_handle, "__LANDING_PAGE__");
    let (win_width, win_height) = {
        let (w, h) = saved_state.as_ref()
            .map(|s| (s.width as f64, s.height as f64))
            .unwrap_or((800.0, 600.0));

        // On Linux, clamp size to prevent GNOME's auto-maximize (only if not maximized)
        #[cfg(target_os = "linux")]
        let (w, h) = if !saved_state.as_ref().map(|s| s.maximized).unwrap_or(false) {
            linux_clamp_window_size(w, h)
        } else {
            (w, h)
        };

        (w, h)
    };

    // Get effective language (user preference or system-detected)
    let language = wiki_storage::get_effective_language(app_handle);

    if let Ok(icon) = Image::from_bytes(include_bytes!("../icons/icon.png")) {
        // Use full init script with is_main_wiki=true
        #[allow(unused_mut)]  // mut needed for disable_drag_drop_handler()
        let mut builder = WebviewWindowBuilder::new(
            app_handle,
            "main",
            WebviewUrl::External(wiki_url.parse().unwrap())
        )
            .title("TiddlyDesktopRS")
            .inner_size(win_width, win_height)
            .icon(icon)
            .expect("Failed to set icon")
            .initialization_script(&init_script::get_wiki_init_script_with_language(&main_wiki_path.to_string_lossy(), "main", true, Some(&language)))
            .zoom_hotkeys_enabled(true);

        #[cfg(target_os = "linux")]
        let mut builder = builder.user_agent(LINUX_USER_AGENT);

        // Apply saved position if available, with monitor validation
        if let Some(ref state) = saved_state {
            let (x, y) = validate_window_position(app_handle, state);
            builder = builder.position(x, y);
        }

        // Tauri's drag/drop handler: On Windows, our WRY patch intercepts drops,
        // extracts file paths, emits tauri://drag-* events, then forwards to WebView2.
        // On Linux, vanilla WebKitGTK handles drops natively.

        if let Ok(main_window) = builder.build()
        {
            // Note: Drag handlers are set up via the drag_drop plugin's on_webview_ready hook

            // Linux: Set up HeaderBar, enable smooth scrolling, finalize window state
            #[cfg(target_os = "linux")]
            {
                setup_header_bar(&main_window);
                enable_smooth_scrolling(&main_window);
                linux_finalize_window_state(&main_window, &saved_state);
            }

            // Restore maximized state (Windows/macOS only - Linux handled in linux_finalize_window_state)
            #[cfg(not(target_os = "linux"))]
            if saved_state.as_ref().map(|s| s.maximized).unwrap_or(false) {
                let _ = main_window.maximize();
            }

            let _ = main_window.set_focus();
        }
    }
}

// System tray is only available on desktop platforms
#[cfg(not(target_os = "android"))]
fn setup_system_tray(app: &tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let show_window = MenuItemBuilder::with_id("show_window", "Show TiddlyDesktop").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;

    let menu = MenuBuilder::new(app)
        .item(&show_window)
        .separator()
        .item(&quit)
        .build()?;

    let _tray = TrayIconBuilder::new()
        .icon(Image::from_bytes(include_bytes!("../icons/32x32.png"))?)
        .menu(&menu)
        .tooltip("TiddlyDesktopRS")
        .on_menu_event(|app, event| {
            match event.id().as_ref() {
                "show_window" => {
                    reveal_or_create_main_window(app);
                }
                "quit" => {
                    // Close all open windows (wiki windows + landing page) before exiting
                    let windows = app.webview_windows();
                    let window_count = windows.len();
                    if window_count > 0 {
                        eprintln!("[TiddlyDesktop] Quit requested - closing {} window(s)", window_count);
                        for (label, window) in &windows {
                            eprintln!("[TiddlyDesktop] Closing window: {}", label);
                            let _ = window.destroy();
                        }
                    }
                    // Clear wiki processes so ExitRequested handler allows exit
                    let state = app.state::<AppState>();
                    state.wiki_processes.lock().unwrap().clear();
                    app.exit(0);
                }
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            // Handle double-click on tray icon - reveal the main window
            if let tauri::tray::TrayIconEvent::DoubleClick { .. } = event {
                reveal_or_create_main_window(tray.app_handle());
            }
        })
        .build(app)?;

    Ok(())
}

/// Arguments for wiki file mode (single-file wiki in separate process)
#[cfg(not(target_os = "android"))]
struct WikiModeArgs {
    wiki_path: PathBuf,
    tiddler_title: Option<String>,
    startup_tiddler: Option<String>,
}

/// Arguments for wiki folder mode (Node.js server in separate process)
#[cfg(not(target_os = "android"))]
struct WikiFolderModeArgs {
    folder_path: PathBuf,
    port: u16,
}

/// Parse command-line arguments for special modes
#[cfg(not(target_os = "android"))]
enum SpecialModeArgs {
    WikiFile(WikiModeArgs),
    WikiFolder(WikiFolderModeArgs),
}

#[cfg(not(target_os = "android"))]
fn parse_special_mode_args() -> Option<SpecialModeArgs> {
    let args: Vec<String> = std::env::args().collect();

    let mut wiki_path: Option<PathBuf> = None;
    let mut wiki_folder_path: Option<PathBuf> = None;
    let mut tiddler_title: Option<String> = None;
    let mut startup_tiddler: Option<String> = None;
    let mut port: Option<u16> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--wiki" if i + 1 < args.len() => {
                wiki_path = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--wiki-folder" if i + 1 < args.len() => {
                wiki_folder_path = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--tiddler" if i + 1 < args.len() => {
                tiddler_title = Some(args[i + 1].clone());
                i += 2;
            }
            "--startup-tiddler" if i + 1 < args.len() => {
                startup_tiddler = Some(args[i + 1].clone());
                i += 2;
            }
            "--port" if i + 1 < args.len() => {
                port = args[i + 1].parse().ok();
                i += 2;
            }
            _ => {
                i += 1;
            }
        }
    }

    // Wiki folder mode takes precedence
    if let Some(folder_path) = wiki_folder_path {
        return Some(SpecialModeArgs::WikiFolder(WikiFolderModeArgs {
            folder_path,
            port: port.unwrap_or(8080),
        }));
    }

    // Wiki file mode
    wiki_path.map(|path| SpecialModeArgs::WikiFile(WikiModeArgs {
        wiki_path: path,
        tiddler_title,
        startup_tiddler,
    }))
}

/// Simplified app state for wiki-only mode (single wiki process)
#[cfg(not(target_os = "android"))]
#[allow(dead_code)]
struct WikiModeState {
    wiki_path: PathBuf,
    path_key: String,
    is_tiddler_window: bool,
    tiddler_title: Option<String>,
    ipc_client: Arc<Mutex<Option<ipc::IpcClient>>>,
}

/// Run in wiki-only mode - a single wiki window in its own process
/// This is called when the app is started with --wiki <path> [--tiddler <title>]
#[cfg(not(target_os = "android"))]
fn run_wiki_mode(args: WikiModeArgs) {
    // Windows: Initialize RegisterDragDrop hook to extract file paths from drops
    #[cfg(target_os = "windows")]
    drag_drop::windows::init_drop_target_hook();

    let wiki_path = args.wiki_path;
    let is_tiddler_window = args.tiddler_title.is_some();
    let tiddler_title = args.tiddler_title.clone();
    let startup_tiddler = args.startup_tiddler.clone();

    eprintln!("[TiddlyDesktop] Wiki mode: {:?}, tiddler: {:?}", wiki_path, tiddler_title);

    // Validate the wiki file exists
    if !wiki_path.exists() {
        eprintln!("[TiddlyDesktop] Error: Wiki file not found: {:?}", wiki_path);
        std::process::exit(1);
    }

    // Connect to IPC server (main process)
    let wiki_path_str = wiki_path.to_string_lossy().to_string();
    let ipc_client = Arc::new(Mutex::new(
        ipc::try_connect(&wiki_path_str, is_tiddler_window, tiddler_title.clone())
    ));

    if ipc_client.lock().unwrap().is_some() {
        eprintln!("[TiddlyDesktop] Connected to IPC server");
        // Register the IPC client for LAN sync commands to route through
        lan_sync::set_ipc_client_for_sync(ipc_client.clone());
    } else {
        eprintln!("[TiddlyDesktop] Warning: Could not connect to IPC server (main process not running?)");
    }

    // Linux: Configure WebKitGTK hardware acceleration (same as main mode)
    #[cfg(target_os = "linux")]
    {
        fn set_env_if_unset(key: &str, value: &str) {
            if std::env::var(key).is_err() {
                std::env::set_var(key, value);
            }
        }

        if std::env::var("TIDDLYDESKTOP_DISABLE_GPU").map(|v| v == "1" || v.to_lowercase() == "true").unwrap_or(false) {
            set_env_if_unset("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
            set_env_if_unset("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
            set_env_if_unset("LIBGL_ALWAYS_SOFTWARE", "1");
        } else {
            set_env_if_unset("WEBKIT_DISABLE_COMPOSITING_MODE", "0");
            set_env_if_unset("WEBKIT_DISABLE_DMABUF_RENDERER", "0");
        }
    }

    // Create window label from filename + path hash to avoid conflicts
    // when multiple files have the same name in different locations
    let filename = wiki_path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "wiki".to_string());

    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    wiki_path.hash(&mut hasher);
    let path_hash = hasher.finish();

    // For tiddler windows, include tiddler name in label
    let label = if let Some(ref tiddler) = tiddler_title {
        let safe_tiddler = tiddler.replace(|c: char| !c.is_alphanumeric(), "-");
        format!("tiddler-{}-{}-{:x}", filename.replace(|c: char| !c.is_alphanumeric(), "-"), safe_tiddler, path_hash & 0xFFFF)
    } else {
        format!("wiki-{}-{:x}", filename.replace(|c: char| !c.is_alphanumeric(), "-"), path_hash & 0xFFFF)
    };

    // Window title
    let title = if let Some(ref tiddler) = tiddler_title {
        format!("{} - {}", tiddler, filename.trim_end_matches(".html").trim_end_matches(".htm"))
    } else {
        filename.trim_end_matches(".html").trim_end_matches(".htm").to_string()
    };

    // Create path key for protocol handler
    let path_key = utils::base64_url_encode(&wiki_path.to_string_lossy());

    // Move IPC client into the closure
    let ipc_client_for_state = ipc_client.clone();
    let is_tiddler_window_for_state = is_tiddler_window;
    let tiddler_title_for_state = tiddler_title.clone();
    let startup_tiddler_for_state = startup_tiddler.clone();

    let builder = tauri::Builder::default()
        .with_platform_plugins()
        .plugin(drag_drop::init_plugin())
        .plugin(permissions::init_plugin());
    let builder = builder.setup(move |app| {
            // Resolve data directory (portable mode check) for wiki process
            let _ = resolve_data_dir(app.handle()).map(|dir| DATA_DIR.set(dir));

            // Initialize PDFium for native PDF rendering
            init_pdfium_from_resources(&app.handle());

            // Store state for this wiki process
            let wiki_path_clone = wiki_path.clone();
            let path_key_clone = path_key.clone();

            app.manage(WikiModeState {
                wiki_path: wiki_path_clone.clone(),
                path_key: path_key_clone.clone(),
                is_tiddler_window: is_tiddler_window_for_state,
                tiddler_title: tiddler_title_for_state.clone(),
                ipc_client: ipc_client_for_state.clone(),
            });

            // Start localhost HTTP media server (Linux: GStreamer needs HTTP URLs;
            // also used for folder wikis on all platforms)
            match media_server::MediaServer::start() {
                Ok(server) => {
                    app.manage(MediaServerState { server });
                }
                Err(e) => {
                    eprintln!("[TiddlyDesktop] Failed to start media server: {}", e);
                }
            }

            // Also need minimal AppState for commands that expect it


            app.manage(AppState {
                wiki_paths: Mutex::new({
                    let mut m = HashMap::new();
                    m.insert(path_key_clone.clone(), wiki_path_clone.clone());
                    m.insert(format!("{}_label", path_key_clone), PathBuf::from(&label));
                    m
                }),
                open_wikis: Mutex::new({
                    let mut m = HashMap::new();
                    m.insert(label.clone(), wiki_path_clone.to_string_lossy().to_string());
                    m
                }),
                wiki_processes: Mutex::new(HashMap::new()), // Not used in wiki mode
                next_port: Mutex::new(8080),
                main_wiki_path: PathBuf::new(), // Empty sentinel — wiki child process never saves the landing page

                folder_wiki_paths: Mutex::new(HashMap::new()),
                saf_wiki_mappings: Mutex::new(HashMap::new()),
            });

            // Build the wiki URL using our protocol
            // For tiddler windows, include tiddler and template query parameters
            let wiki_url = if let Some(ref tiddler) = tiddler_title_for_state {
                let encoded_tiddler = urlencoding::encode(tiddler);
                let template = startup_tiddler_for_state.as_deref()
                    .unwrap_or("$:/core/templates/single.tiddler.window");
                let encoded_template = urlencoding::encode(template);
                format!("wikifile://localhost/{}?tiddler={}&template={}",
                    path_key_clone, encoded_tiddler, encoded_template)
            } else {
                format!("wikifile://localhost/{}", path_key_clone)
            };

            // Create the wiki window
            let icon = Image::from_bytes(include_bytes!("../icons/icon.png"))?;

            // Load saved window state (main wiki only, not tiddler windows)
            let saved_state = if !is_tiddler_window_for_state {
                wiki_storage::get_window_state(&app.handle(), &wiki_path_clone.to_string_lossy())
            } else {
                None
            };

            // Tiddler windows are smaller than main wiki windows
            let (win_width, win_height) = if is_tiddler_window_for_state {
                (700.0, 600.0)
            } else {
                let (w, h) = saved_state.as_ref()
                    .map(|s| (s.width as f64, s.height as f64))
                    .unwrap_or((1200.0, 800.0));

                // On Linux, clamp size to prevent GNOME's auto-maximize (only if not maximized)
                #[cfg(target_os = "linux")]
                let (w, h) = if !saved_state.as_ref().map(|s| s.maximized).unwrap_or(false) {
                    linux_clamp_window_size(w, h)
                } else {
                    (w, h)
                };

                (w, h)
            };

            #[allow(unused_mut)]
            let mut builder = WebviewWindowBuilder::new(app, &label, WebviewUrl::External(wiki_url.parse().unwrap()))
                .title(&title)
                .inner_size(win_width, win_height)
                .icon(icon)?
                .window_classname("tiddlydesktop-rs-wiki")
                .initialization_script(&init_script::get_wiki_init_script(&wiki_path_clone.to_string_lossy(), &label, false))
                .on_document_title_changed(|ww, title| { let _ = ww.set_title(&title); })
                .zoom_hotkeys_enabled(true)
                .devtools(cfg!(debug_assertions)); // Only enable in debug builds

            // Isolate session data (cookies, localStorage) per wiki
            if let Some(session_dir) = get_wiki_session_dir(app.handle(), &wiki_path_clone.to_string_lossy()) {
                builder = builder.data_directory(session_dir);
            }

            #[cfg(target_os = "linux")]
            let mut builder = builder.user_agent(LINUX_USER_AGENT);

            // Inject embed proxy port so external iframes can be proxied through HTTP
            if let Some(state) = app.try_state::<MediaServerState>() {
                let port = state.server.port();
                builder = builder.initialization_script(&format!("window.__TD_EMBED_PORT__ = {};", port));
            }

            // Apply saved position if available, with monitor validation on Windows/macOS
            if let Some(ref state) = saved_state {
                let (x, y) = validate_window_position(app.handle(), state);
                builder = builder.position(x, y);
            }

            // Tauri's drag/drop handler: On Windows, our WRY patch intercepts drops,
            // extracts file paths, emits tauri://drag-* events, then forwards to WebView2.
            // On Linux, vanilla WebKitGTK handles drops natively.

            let window = builder.build()?;

            // Note: Drag handlers are set up via the drag_drop plugin's on_webview_ready hook

            // Linux: Set up HeaderBar, enable smooth scrolling, finalize window state
            #[cfg(target_os = "linux")]
            {
                setup_header_bar(&window);
                enable_smooth_scrolling(&window);
                linux_finalize_window_state(&window, &saved_state);
            }

            // Restore maximized state (Windows/macOS only - Linux handled in linux_finalize_window_state)
            #[cfg(not(target_os = "linux"))]
            if saved_state.as_ref().map(|s| s.maximized).unwrap_or(false) {
                let _ = window.maximize();
            }

            eprintln!("[TiddlyDesktop] Wiki window created: {}", label);

            // Start IPC listener thread to receive messages from other wiki windows
            let client_guard = ipc_client_for_state.lock().unwrap();
            if let Some(ref client) = *client_guard {
                if let Some(listener_stream) = client.get_listener_stream() {
                    eprintln!("[TiddlyDesktop] IPC listener stream cloned successfully");
                    let app_handle = app.handle().clone();
                    std::thread::spawn(move || {
                        ipc::run_listener(listener_stream, |msg| {
                            match msg {
                                ipc::IpcMessage::TiddlerChanged { tiddler_title, tiddler_json, .. } => {
                                    eprintln!("[IPC Listener] Tiddler changed: {}", tiddler_title);
                                    // Emit event to JavaScript to update the tiddler
                                    let _ = app_handle.emit("ipc-tiddler-changed", serde_json::json!({
                                        "title": tiddler_title,
                                        "tiddler": tiddler_json
                                    }));
                                }
                                ipc::IpcMessage::TiddlerDeleted { tiddler_title, .. } => {
                                    eprintln!("[IPC Listener] Tiddler deleted: {}", tiddler_title);
                                    // Emit event to JavaScript to delete the tiddler
                                    let _ = app_handle.emit("ipc-tiddler-deleted", serde_json::json!({
                                        "title": tiddler_title
                                    }));
                                }
                                ipc::IpcMessage::SyncState { tiddlers_json, .. } => {
                                    eprintln!("[IPC Listener] Received sync state");
                                    // Emit event to JavaScript to sync all tiddlers
                                    let _ = app_handle.emit("ipc-sync-state", serde_json::json!({
                                        "tiddlers": tiddlers_json
                                    }));
                                }
                                ipc::IpcMessage::RequestSync { requester_pid, .. } => {
                                    eprintln!("[IPC Listener] Sync request from pid {}", requester_pid);
                                    // Emit event to JavaScript to send current state
                                    let _ = app_handle.emit("ipc-sync-request", serde_json::json!({
                                        "requester_pid": requester_pid
                                    }));
                                }
                                ipc::IpcMessage::Ack { success, message } => {
                                    if !success {
                                        if let Some(msg) = message {
                                            eprintln!("[IPC Listener] Server error: {}", msg);
                                        }
                                    }
                                }
                                ipc::IpcMessage::FocusWiki { .. } => {
                                    eprintln!("[IPC Listener] Focus window request received");
                                    // Focus this window - must run on main thread for GTK
                                    let handle = app_handle.clone();
                                    let _ = app_handle.run_on_main_thread(move || {
                                        // Get any window in this process (wiki processes have one window)
                                        let windows = handle.webview_windows();
                                        if let Some((label, window)) = windows.into_iter().next() {
                                            eprintln!("[IPC Listener] Found window '{}', attempting to focus", label);
                                            let _ = window.unminimize();
                                            let _ = window.show();
                                            #[cfg(target_os = "linux")]
                                            {
                                                if let Ok(gtk_window) = window.gtk_window() {
                                                    linux_activate_window(&gtk_window);
                                                }
                                            }
                                            #[cfg(not(target_os = "linux"))]
                                            {
                                                let _ = window.set_focus();
                                            }
                                        } else {
                                            eprintln!("[IPC Listener] No windows found in process!");
                                        }
                                    });
                                }
                                // LAN Sync: main process → wiki process
                                ipc::IpcMessage::LanSyncApplyChange { wiki_id, payload_json } => {
                                    // Queue the message for JS to poll via lan_sync_poll_ipc.
                                    // Neither Tauri emit() nor WebView eval() reliably deliver
                                    // messages from IPC listener threads to JS on Linux/WebKitGTK.
                                    if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&payload_json) {
                                        let event_type = payload["type"].as_str().unwrap_or("");
                                        if !event_type.is_empty() {
                                            eprintln!("[IPC Listener] LAN Sync {}: wiki_id={}", event_type, wiki_id);
                                            lan_sync::queue_lan_sync_ipc(payload_json);
                                        }
                                    }
                                }
                                _ => {}
                            }
                        });
                    });
                    eprintln!("[TiddlyDesktop] IPC listener thread started");
                } else {
                    eprintln!("[TiddlyDesktop] WARNING: Failed to clone IPC stream for listener — LAN sync IPC will not work!");
                }
            } else {
                eprintln!("[TiddlyDesktop] WARNING: IPC client not connected — LAN sync IPC will not work!");
            }
            drop(client_guard);

            Ok(())
        })
        .register_uri_scheme_protocol("wikifile", |ctx, request| {
            wiki_protocol_handler(ctx.app_handle(), request)
        })
        .register_asynchronous_uri_scheme_protocol("tdasset", |_ctx, request, responder| {
            // Spawn a thread for file I/O to avoid blocking the main thread.
            // This is critical for media streaming — GStreamer/WebKitGTK needs
            // non-blocking range responses for video/audio playback.
            std::thread::spawn(move || {
                responder.respond(tdasset_protocol_handler(request));
            });
        });
        // tdlib:// protocol is desktop-only (Android serves libraries via WikiHttpServer /_td/)
        #[cfg(not(target_os = "android"))]
        let builder = builder.register_uri_scheme_protocol("tdlib", |ctx, request| {
            tdlib_protocol_handler(ctx.app_handle(), request)
        });
        builder.plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .invoke_handler(tauri::generate_handler![
            // Core wiki commands needed for operation
            load_wiki,
            save_wiki,
            set_window_title,
            set_window_icon,
            set_headerbar_colors,
            get_window_label,
            get_main_wiki_path,
            reveal_in_folder,
            show_alert,
            show_confirm,
            close_window,
            read_file_as_data_uri,
            read_file_as_binary,
            save_binary_file,
            pick_files_for_import,
            wiki_storage::get_external_attachments_config,
            wiki_storage::set_external_attachments_config,
            wiki_storage::js_log,
            clipboard::get_clipboard_content,
            clipboard::set_clipboard_content,

            // Drag-drop commands
            start_native_drag,
            prepare_native_drag,
            cleanup_native_drag,
            get_pending_drag_data,
            get_external_drop_paths,
            update_drag_icon,
            set_pending_drag_icon,
            set_drag_dest_enabled,
            ungrab_seat_for_focus,
            set_over_droppable,
            set_internal_drag_type,
            // Tiddler window commands (same process, shares $tw.wiki)
            open_tiddler_window,
            close_window_by_label,
            toggle_fullscreen,
            print_page,
            set_zoom_level,
            download_file,
            fetch_url,
            fetch_library_plugin,
            http_request,
            is_directory,
            get_window_state_info,
            get_saved_window_state,
            wiki_storage::save_window_state,
            // IPC commands for multi-process wiki sync (between different wiki files)
            ipc_notify_tiddler_changed,
            ipc_notify_tiddler_deleted,
            ipc_open_tiddler_window,
            ipc_is_tiddler_window,
            ipc_get_tiddler_title,
            ipc_request_sync,
            ipc_send_sync_state,
            ipc_update_favicon,
            show_find_in_page,
            extract_video_poster,
            register_media_url,
            // PDF rendering commands
            pdf_open,
            pdf_open_file,
            pdf_render_page,
            pdf_close,
            pdf_char_at_pos,
            pdf_selection_rects,
            pdf_get_text,
            pdf_char_count,
            // LAN sync commands (fall back to IPC when sync manager not in this process)
            wiki_storage::get_wiki_sync_id,
            lan_sync::lan_sync_wiki_opened,
            lan_sync::lan_sync_tiddler_changed,
            lan_sync::lan_sync_tiddler_deleted,
            lan_sync::lan_sync_send_full_sync,
            lan_sync::lan_sync_send_fingerprints,
            lan_sync::lan_sync_broadcast_fingerprints,
            lan_sync::lan_sync_broadcast_manifest,
            lan_sync::lan_sync_poll_ipc,
            lan_sync::lan_sync_load_tombstones,
            lan_sync::lan_sync_save_tombstones,
            lan_sync::lan_sync_collab_editing_started,
            lan_sync::lan_sync_collab_editing_stopped,
            lan_sync::lan_sync_collab_peer_saved,
            lan_sync::lan_sync_collab_update,
            lan_sync::lan_sync_collab_awareness,
            lan_sync::lan_sync_get_remote_editors,
            lan_sync::lan_sync_get_collab_port,
            // Relay sync commands
            lan_sync::relay_sync_get_status,
            lan_sync::relay_sync_add_room,
            lan_sync::relay_sync_remove_room,
            lan_sync::relay_sync_connect_room,
            lan_sync::relay_sync_disconnect_room,
            lan_sync::relay_sync_set_room_auto_connect,
            lan_sync::relay_sync_set_room_password,
            lan_sync::relay_sync_set_room_name,
            lan_sync::relay_sync_get_room_credentials,
            lan_sync::relay_sync_set_url,
            lan_sync::relay_sync_generate_credentials,
            // Per-wiki relay room assignment
            wiki_storage::set_wiki_relay_room,
            get_wiki_installed_plugins,
            install_plugins_to_wiki
        ])
        .build(tauri::generate_context!())
        .expect("error while building wiki-mode application")
        .run(|_app, _event| {
            // Wiki mode doesn't need special event handling
        });
}

/// Run in wiki-folder mode - a Node.js TiddlyWiki server in its own process
/// This is called when the app is started with --wiki-folder <path> --port <port>
#[cfg(not(target_os = "android"))]
fn run_wiki_folder_mode(args: WikiFolderModeArgs) {
    // Windows: Initialize RegisterDragDrop hook to extract file paths from drops
    #[cfg(target_os = "windows")]
    drag_drop::windows::init_drop_target_hook();

    let folder_path = args.folder_path;
    let port = args.port;

    eprintln!("[TiddlyDesktop] Wiki folder mode: {:?}, port: {}", folder_path, port);

    // Validate the folder exists and is a wiki folder
    if !folder_path.exists() {
        eprintln!("[TiddlyDesktop] Error: Wiki folder not found: {:?}", folder_path);
        return;
    }

    if !utils::is_wiki_folder(&folder_path) {
        eprintln!("[TiddlyDesktop] Error: Not a valid wiki folder (missing tiddlywiki.info): {:?}", folder_path);
        return;
    }

    // Get folder name for window title
    let folder_name = folder_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("TiddlyWiki")
        .to_string();

    let folder_path_str = folder_path.to_string_lossy().to_string();

    // We need to find Node.js and TiddlyWiki paths
    // In folder mode, we'll use the same logic as the main process
    let node_path_result = find_node_executable();
    let node_path = match node_path_result {
        Some(p) => p,
        None => {
            eprintln!("[TiddlyDesktop] Error: Node.js not found");
            return;
        }
    };

    // Find TiddlyWiki - it should be in the resources directory
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()));

    let tw_path = exe_dir.as_ref().and_then(|dir| {
        // Try various locations
        let candidates = [
            dir.join("resources").join("tiddlywiki").join("tiddlywiki.js"),
            dir.join("..").join("lib").join("tiddlydesktop-rs").join("resources").join("tiddlywiki").join("tiddlywiki.js"),
            dir.join("..").join("lib").join("tiddlydesktop-rs").join("tiddlywiki").join("tiddlywiki.js"),
            dir.join("..").join("Resources").join("tiddlywiki").join("tiddlywiki.js"),
        ];
        candidates.into_iter().find(|p| p.exists())
    });

    let tw_path = match tw_path {
        Some(p) => p,
        None => {
            eprintln!("[TiddlyDesktop] Error: TiddlyWiki not found in resources");
            if let Some(dir) = &exe_dir {
                let candidates = [
                    dir.join("resources").join("tiddlywiki").join("tiddlywiki.js"),
                    dir.join("..").join("lib").join("tiddlydesktop-rs").join("resources").join("tiddlywiki").join("tiddlywiki.js"),
                    dir.join("..").join("lib").join("tiddlydesktop-rs").join("tiddlywiki").join("tiddlywiki.js"),
                    dir.join("..").join("Resources").join("tiddlywiki").join("tiddlywiki.js"),
                ];
                eprintln!("[TiddlyDesktop]   exe_dir: {:?}", dir);
                for c in &candidates {
                    eprintln!("[TiddlyDesktop]   tried: {:?} (exists={})", c, c.exists());
                }
            } else {
                eprintln!("[TiddlyDesktop]   current_exe() failed");
            }
            return;
        }
    };

    eprintln!("[TiddlyDesktop] Starting wiki folder server:");
    eprintln!("  Node.js: {:?}", node_path);
    eprintln!("  TiddlyWiki: {:?}", tw_path);
    eprintln!("  Wiki folder: {:?}", folder_path);
    eprintln!("  Port: {}", port);

    // Ensure required plugins and autosave are enabled
    ensure_wiki_folder_config(&folder_path);

    // Start the Node.js server
    let mut cmd = Command::new(&node_path);
    cmd.arg(&tw_path)
        .arg(&folder_path)
        .arg("--listen")
        .arg(format!("port={}", port))
        .arg("host=127.0.0.1");

    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let mut server_process = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            eprintln!("[TiddlyDesktop] Error: Failed to start TiddlyWiki server: {}", e);
            return;
        }
    };

    // Windows: Assign to job object so it gets killed when parent exits
    #[cfg(target_os = "windows")]
    drag_drop::windows_job::assign_process_to_job(server_process.id());

    // Wait for server to be ready
    if let Err(e) = wait_for_server_ready(port, &mut server_process, std::time::Duration::from_secs(15)) {
        eprintln!("[TiddlyDesktop] Error: Server failed to start: {}", e);
        let _ = server_process.kill();
        return;
    }

    let server_url = format!("http://127.0.0.1:{}", port);
    eprintln!("[TiddlyDesktop] Wiki folder server ready at {}", server_url);

    // Store server process in a mutex for cleanup
    let server_process = Arc::new(Mutex::new(Some(server_process)));
    let server_process_for_exit = server_process.clone();

    // Connect to IPC server in main process
    let ipc_client: Arc<Mutex<Option<ipc::IpcClient>>> = Arc::new(Mutex::new(None));
    let ipc_client_for_setup = ipc_client.clone();

    // Try to connect to IPC (try_connect handles creation and registration)
    if let Some(client) = ipc::try_connect(&folder_path_str, false, None) {
        eprintln!("[TiddlyDesktop] Registered with IPC server");
        *ipc_client_for_setup.lock().unwrap() = Some(client);
        // Register the IPC client for LAN sync commands to route through
        lan_sync::set_ipc_client_for_sync(ipc_client_for_setup.clone());
    }

    let folder_path_for_state = folder_path.clone();
    let folder_path_for_wiki_state = folder_path.clone();
    let folder_name_for_state = folder_name.clone();
    let ipc_client_for_state = ipc_client.clone();
    let ipc_client_for_wiki_state = ipc_client.clone();

    // Create unique window label from folder name + path hash to avoid conflicts
    // when multiple folders have the same name in different locations
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    folder_path.hash(&mut hasher);
    let path_hash = hasher.finish();
    // Label must start with "folder-" to match Tauri capability pattern "folder-*"
    let label = format!("folder-{}-{:x}", folder_name.replace(|c: char| !c.is_alphanumeric(), "-"), path_hash & 0xFFFF);
    let label_for_state = label.clone();

    // Build the Tauri app for this wiki folder
    tauri::Builder::default()
        .with_platform_plugins()
        .plugin(drag_drop::init_plugin())
        .setup(move |app| {
            // Resolve data directory (portable mode check) for wiki-folder process
            let _ = resolve_data_dir(app.handle()).map(|dir| DATA_DIR.set(dir));

            // Initialize PDFium for native PDF rendering
            init_pdfium_from_resources(&app.handle());

            let icon = Image::from_bytes(include_bytes!("../icons/icon.png"))?;

            // Load saved window state
            let saved_state = wiki_storage::get_window_state(&app.handle(), &folder_path_for_state.to_string_lossy());
            let (win_width, win_height) = {
                let (w, h) = saved_state.as_ref()
                    .map(|s| (s.width as f64, s.height as f64))
                    .unwrap_or((1200.0, 800.0));

                // On Linux, clamp size to prevent GNOME's auto-maximize (only if not maximized)
                #[cfg(target_os = "linux")]
                let (w, h) = if !saved_state.as_ref().map(|s| s.maximized).unwrap_or(false) {
                    linux_clamp_window_size(w, h)
                } else {
                    (w, h)
                };

                (w, h)
            };

            // Start localhost HTTP media server for file serving in folder wikis
            // (must be before window builder so embed proxy port is available for init script)
            match media_server::MediaServer::start() {
                Ok(server) => {
                    app.manage(MediaServerState { server });
                }
                Err(e) => {
                    eprintln!("[TiddlyDesktop] Failed to start media server: {}", e);
                }
            }

            let mut builder = WebviewWindowBuilder::new(
                app,
                &label_for_state,
                WebviewUrl::External(server_url.parse().unwrap())
            )
            .title(&folder_name_for_state)
            .inner_size(win_width, win_height)
            .icon(icon)?
            .window_classname("tiddlydesktop-rs-wiki")
            .initialization_script(&init_script::get_wiki_init_script(
                &folder_path_for_state.to_string_lossy(),
                &label_for_state,
                false
            ))
            .on_document_title_changed(|ww, title| { let _ = ww.set_title(&title); })
            .zoom_hotkeys_enabled(true)
            .devtools(cfg!(debug_assertions)); // Only enable in debug builds

            // Isolate session data (cookies, localStorage) per wiki
            if let Some(session_dir) = get_wiki_session_dir(app.handle(), &folder_path_for_state.to_string_lossy()) {
                builder = builder.data_directory(session_dir);
            }

            // Inject embed proxy port so external iframes can be proxied through HTTP
            if let Some(state) = app.try_state::<MediaServerState>() {
                let port = state.server.port();
                builder = builder.initialization_script(&format!("window.__TD_EMBED_PORT__ = {};", port));
            }

            // Apply saved position, with monitor validation on Windows/macOS
            if let Some(ref state) = saved_state {
                let (x, y) = validate_window_position(app.handle(), state);
                builder = builder.position(x, y);
            }

            let window = builder.build()?;

            // Note: Drag handlers are set up via the drag_drop plugin's on_webview_ready hook

            // Linux: Set up HeaderBar, enable smooth scrolling, finalize window state
            #[cfg(target_os = "linux")]
            {
                setup_header_bar(&window);
                enable_smooth_scrolling(&window);
                linux_finalize_window_state(&window, &saved_state);
            }

            // Restore maximized state (Windows/macOS only - Linux handled in linux_finalize_window_state)
            #[cfg(not(target_os = "linux"))]
            if saved_state.as_ref().map(|s| s.maximized).unwrap_or(false) {
                let _ = window.maximize();
            }

            // Minimal app state for this process


            app.manage(AppState {
                wiki_paths: Mutex::new(HashMap::new()),
                open_wikis: Mutex::new(HashMap::new()),
                wiki_processes: Mutex::new(HashMap::new()),
                next_port: Mutex::new(port + 1),
                main_wiki_path: PathBuf::new(), // Empty sentinel — folder wiki child process never saves the landing page

                folder_wiki_paths: Mutex::new(HashMap::new()),
                saf_wiki_mappings: Mutex::new(HashMap::new()),
            });

            // WikiModeState for IPC commands (favicon sync, etc.)
            app.manage(WikiModeState {
                wiki_path: folder_path_for_wiki_state.clone(),
                path_key: String::new(),
                is_tiddler_window: false,
                tiddler_title: None,
                ipc_client: ipc_client_for_wiki_state.clone(),
            });

            // Start IPC listener for focus requests and LAN sync messages
            let client_guard = ipc_client_for_state.lock().unwrap();
            if let Some(ref client) = *client_guard {
                if let Some(listener_stream) = client.get_listener_stream() {
                    let app_handle = app.handle().clone();
                    std::thread::spawn(move || {
                        ipc::run_listener(listener_stream, |msg| {
                            match msg {
                                ipc::IpcMessage::FocusWiki { .. } => {
                                    eprintln!("[IPC Listener] Focus window request received");
                                    // Focus this window - must run on main thread for GTK
                                    let handle = app_handle.clone();
                                    let _ = app_handle.run_on_main_thread(move || {
                                        // Get any window in this process (wiki processes have one window)
                                        let windows = handle.webview_windows();
                                        if let Some((label, window)) = windows.into_iter().next() {
                                            eprintln!("[IPC Listener] Found window '{}', attempting to focus", label);
                                            let _ = window.unminimize();
                                            let _ = window.show();
                                            #[cfg(target_os = "linux")]
                                            {
                                                if let Ok(gtk_window) = window.gtk_window() {
                                                    linux_activate_window(&gtk_window);
                                                }
                                            }
                                            #[cfg(not(target_os = "linux"))]
                                            {
                                                let _ = window.set_focus();
                                            }
                                        } else {
                                            eprintln!("[IPC Listener] No windows found in process!");
                                        }
                                    });
                                }
                                // LAN Sync: main process → folder wiki process
                                ipc::IpcMessage::LanSyncApplyChange { wiki_id, payload_json } => {
                                    if let Ok(payload) = serde_json::from_str::<serde_json::Value>(&payload_json) {
                                        let event_type = payload["type"].as_str().unwrap_or("");
                                        if !event_type.is_empty() {
                                            eprintln!("[IPC Listener] LAN Sync {}: wiki_id={}", event_type, wiki_id);
                                            lan_sync::queue_lan_sync_ipc(payload_json);
                                        }
                                    }
                                }
                                _ => {}
                            }
                        });
                    });
                }
            }
            drop(client_guard);

            Ok(())
        })
        .on_window_event(move |_window, event| {
            if let tauri::WindowEvent::Destroyed = event {
                // Kill the Node.js server when window closes
                if let Some(mut process) = server_process_for_exit.lock().unwrap().take() {
                    eprintln!("[TiddlyDesktop] Killing wiki folder server");
                    let _ = process.kill();
                }
            }
        })
        .register_uri_scheme_protocol("tdlib", |ctx, request| {
            tdlib_protocol_handler(ctx.app_handle(), request)
        })
        .register_asynchronous_uri_scheme_protocol("tdasset", |_ctx, request, responder| {
            std::thread::spawn(move || {
                responder.respond(tdasset_protocol_handler(request));
            });
        })
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .invoke_handler(tauri::generate_handler![
            load_wiki,
            save_wiki,
            set_window_title,
            set_window_icon,
            set_headerbar_colors,
            get_window_label,
            get_main_wiki_path,
            reveal_in_folder,
            show_alert,
            show_confirm,
            close_window,
            toggle_fullscreen,
            print_page,
            set_zoom_level,
            download_file,
            fetch_url,
            fetch_library_plugin,
            http_request,
            is_directory,
            get_window_state_info,
            get_saved_window_state,
            read_file_as_data_uri,
            read_file_as_binary,
            save_binary_file,
            pick_files_for_import,
            wiki_storage::get_external_attachments_config,
            wiki_storage::set_external_attachments_config,
            wiki_storage::save_window_state,
            wiki_storage::js_log,
            clipboard::get_clipboard_content,
            clipboard::set_clipboard_content,
            show_find_in_page,
            extract_video_poster,
            // PDF rendering commands
            pdf_open,
            pdf_open_file,
            pdf_render_page,
            pdf_close,
            pdf_char_at_pos,
            pdf_selection_rects,
            pdf_get_text,
            pdf_char_count,

            // Drag-drop commands
            start_native_drag,
            prepare_native_drag,
            cleanup_native_drag,
            get_pending_drag_data,
            get_external_drop_paths,
            update_drag_icon,
            set_pending_drag_icon,
            set_drag_dest_enabled,
            ungrab_seat_for_focus,
            set_over_droppable,
            set_internal_drag_type,
            register_media_url,
            // IPC commands for favicon sync
            ipc_update_favicon,
            // LAN sync commands (fall back to IPC when sync manager not in this process)
            wiki_storage::get_wiki_sync_id,
            lan_sync::lan_sync_wiki_opened,
            lan_sync::lan_sync_tiddler_changed,
            lan_sync::lan_sync_tiddler_deleted,
            lan_sync::lan_sync_send_full_sync,
            lan_sync::lan_sync_send_fingerprints,
            lan_sync::lan_sync_broadcast_fingerprints,
            lan_sync::lan_sync_broadcast_manifest,
            lan_sync::lan_sync_poll_ipc,
            lan_sync::lan_sync_load_tombstones,
            lan_sync::lan_sync_save_tombstones,
            lan_sync::lan_sync_collab_editing_started,
            lan_sync::lan_sync_collab_editing_stopped,
            lan_sync::lan_sync_collab_peer_saved,
            lan_sync::lan_sync_collab_update,
            lan_sync::lan_sync_collab_awareness,
            lan_sync::lan_sync_get_remote_editors,
            lan_sync::lan_sync_get_collab_port,
            // Relay sync commands
            lan_sync::relay_sync_get_status,
            lan_sync::relay_sync_add_room,
            lan_sync::relay_sync_remove_room,
            lan_sync::relay_sync_connect_room,
            lan_sync::relay_sync_disconnect_room,
            lan_sync::relay_sync_set_room_auto_connect,
            lan_sync::relay_sync_set_room_password,
            lan_sync::relay_sync_set_room_name,
            lan_sync::relay_sync_get_room_credentials,
            lan_sync::relay_sync_set_url,
            lan_sync::relay_sync_generate_credentials,
            // Per-wiki relay room assignment
            wiki_storage::set_wiki_relay_room,
            get_wiki_installed_plugins,
            install_plugins_to_wiki
        ])
        .build(tauri::generate_context!())
        .expect("error while building wiki-folder-mode application")
        .run(|_app, _event| {});
}

/// Windows: Check if Microsoft Edge version 131+ is installed
/// Required for DragStarting API (SDK 1.0.3719.77)
/// Edge includes the WebView2 runtime - they share the same binaries.
/// Uses registry detection (same method as the NSIS installer).
#[cfg(target_os = "windows")]
fn check_webview2_version() {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::HKEY_LOCAL_MACHINE;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_OK, MB_ICONWARNING};

    const REQUIRED_MAJOR_VERSION: u32 = 131;
    // WebView2 Runtime client GUID (shared by Edge and standalone runtime)
    const WEBVIEW2_GUID: &str = "{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}";

    // Try to read version from registry (same locations as NSIS installer)
    let registry_paths = [
        format!("SOFTWARE\\WOW6432Node\\Microsoft\\EdgeUpdate\\Clients\\{}", WEBVIEW2_GUID),
        format!("SOFTWARE\\Microsoft\\EdgeUpdate\\Clients\\{}", WEBVIEW2_GUID),
    ];

    let mut version_str: Option<String> = None;

    for path in &registry_paths {
        if let Some(ver) = read_registry_string(HKEY_LOCAL_MACHINE, path, "pv") {
            if !ver.is_empty() {
                version_str = Some(ver);
                break;
            }
        }
    }

    let version_str = match version_str {
        Some(v) => v,
        None => {
            eprintln!("[TiddlyDesktop] Microsoft Edge not found in registry");
            let title: Vec<u16> = "TiddlyDesktop - Microsoft Edge Required\0".encode_utf16().collect();
            let message: Vec<u16> = "Microsoft Edge is required to run TiddlyDesktop.\n\n\
                Please install Microsoft Edge from:\n\
                https://www.microsoft.com/edge\0"
                .encode_utf16().collect();
            unsafe {
                MessageBoxW(None, PCWSTR(message.as_ptr()), PCWSTR(title.as_ptr()), MB_OK | MB_ICONWARNING);
            }
            return;
        }
    };

    eprintln!("[TiddlyDesktop] Microsoft Edge version: {}", version_str);

    // Parse major version (format: "131.0.2903.112")
    if let Some(major_str) = version_str.split('.').next() {
        if let Ok(major) = major_str.parse::<u32>() {
            if major < REQUIRED_MAJOR_VERSION {
                eprintln!("[TiddlyDesktop] Edge version {} is below required version {}", major, REQUIRED_MAJOR_VERSION);
                let title: Vec<u16> = "TiddlyDesktop - Edge Update Recommended\0".encode_utf16().collect();
                let message = format!(
                    "Your Microsoft Edge version ({}) is older than recommended.\n\n\
                    TiddlyDesktop works best with Edge {} or newer \
                    (needed for drag-and-drop functionality).\n\n\
                    Please update Microsoft Edge via Settings > About Microsoft Edge.\0",
                    version_str, REQUIRED_MAJOR_VERSION
                );
                let message_wide: Vec<u16> = message.encode_utf16().collect();
                unsafe {
                    MessageBoxW(None, PCWSTR(message_wide.as_ptr()), PCWSTR(title.as_ptr()), MB_OK | MB_ICONWARNING);
                }
            } else {
                eprintln!("[TiddlyDesktop] Edge version {} meets requirement (>= {})", major, REQUIRED_MAJOR_VERSION);
            }
        }
    }
}

/// Helper to read a string value from the Windows registry
#[cfg(target_os = "windows")]
fn read_registry_string(hkey: windows::Win32::System::Registry::HKEY, path: &str, value_name: &str) -> Option<String> {
    use windows::Win32::System::Registry::{
        RegOpenKeyExW, RegQueryValueExW, RegCloseKey, KEY_READ, REG_SZ,
    };

    unsafe {
        let path_wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let value_wide: Vec<u16> = value_name.encode_utf16().chain(std::iter::once(0)).collect();

        let mut key_handle = windows::Win32::System::Registry::HKEY::default();
        let result = RegOpenKeyExW(
            hkey,
            windows::core::PCWSTR(path_wide.as_ptr()),
            Some(0),
            KEY_READ,
            &mut key_handle,
        );

        if result.is_err() {
            return None;
        }

        // First call to get required buffer size
        let mut data_type = REG_SZ;
        let mut data_size: u32 = 0;
        let result = RegQueryValueExW(
            key_handle,
            windows::core::PCWSTR(value_wide.as_ptr()),
            None,
            Some(&mut data_type),
            None,
            Some(&mut data_size),
        );

        if result.is_err() || data_size == 0 {
            let _ = RegCloseKey(key_handle);
            return None;
        }

        // Allocate buffer and read the value
        let mut buffer: Vec<u8> = vec![0u8; data_size as usize];
        let result = RegQueryValueExW(
            key_handle,
            windows::core::PCWSTR(value_wide.as_ptr()),
            None,
            Some(&mut data_type),
            Some(buffer.as_mut_ptr()),
            Some(&mut data_size),
        );

        let _ = RegCloseKey(key_handle);

        if result.is_err() {
            return None;
        }

        // Convert wide string to Rust String
        let wide_chars: Vec<u16> = buffer
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&c| c != 0)
            .collect();

        String::from_utf16(&wide_chars).ok()
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Linux: Disable overlay scrollbars so scrollbars are always visible
    #[cfg(target_os = "linux")]
    std::env::set_var("GTK_OVERLAY_SCROLLING", "0");

    // Windows: Check WebView2 version at startup
    #[cfg(target_os = "windows")]
    check_webview2_version();

    // Windows: Initialize RegisterDragDrop hook to extract file paths from drops
    #[cfg(target_os = "windows")]
    drag_drop::windows::init_drop_target_hook();

    // Check if we're running in a special mode (wiki file or wiki folder)
    // Special modes are desktop-only (Android has a different app structure)
    #[cfg(not(target_os = "android"))]
    if let Some(mode) = parse_special_mode_args() {
        match mode {
            SpecialModeArgs::WikiFile(args) => {
                run_wiki_mode(args);
                return;
            }
            SpecialModeArgs::WikiFolder(args) => {
                run_wiki_folder_mode(args);
                return;
            }
        }
    }

    // Main process: Start the IPC server for wiki process coordination
    // IPC server is desktop-only (Android uses single-process architecture)
    #[cfg(not(target_os = "android"))]
    {
        let server = Arc::new(ipc::IpcServer::new());
        let _ = GLOBAL_IPC_SERVER.set(server.clone());

        std::thread::spawn(move || {
            // Set up callback for opening wikis (from tiddler windows or other sources)
            server.on_open_wiki(|path| {
                eprintln!("[IPC] Open wiki request received: {}", path);
                // Spawn a wiki process for this path
                if let Err(e) = spawn_wiki_process_sync(&path) {
                    eprintln!("[IPC] Failed to open wiki: {}", e);
                }
            });

            // Set up callback for opening tiddler windows
            server.on_open_tiddler(|wiki_path, tiddler_title, startup_tiddler| {
                eprintln!("[IPC] Open tiddler window request: wiki={}, tiddler={}", wiki_path, tiddler_title);
                if let Err(e) = spawn_tiddler_process(&wiki_path, &tiddler_title, startup_tiddler.as_deref()) {
                    eprintln!("[IPC] Failed to spawn tiddler window: {}", e);
                }
            });

            // Set up callback for updating wiki favicon
            server.on_update_favicon(|wiki_path, favicon| {
                eprintln!("[IPC] Update favicon request: wiki={}", wiki_path);
                if let Some(app_handle) = GLOBAL_APP_HANDLE.get() {
                    if let Err(e) = wiki_storage::update_wiki_favicon(app_handle.clone(), wiki_path, favicon) {
                        eprintln!("[IPC] Failed to update favicon: {}", e);
                    }
                } else {
                    eprintln!("[IPC] AppHandle not available yet for favicon update");
                }
            });

            // When a new wiki client registers, send sync-activate if sync is enabled
            // This ensures wiki processes in separate OS processes receive activation
            // (app.emit() only reaches webviews in the same process)
            server.on_client_registered(|wiki_path| {
                eprintln!("[IPC] on_client_registered: wiki_path={:?}", wiki_path);
                if let Some(app) = GLOBAL_APP_HANDLE.get() {
                    let entries = wiki_storage::load_recent_files_from_disk(app);
                    let mut found = false;
                    for entry in &entries {
                        if utils::paths_equal(&entry.path, &wiki_path) && entry.sync_enabled {
                            found = true;
                            if let Some(ref sync_id) = entry.sync_id {
                                if !sync_id.is_empty() {
                                    if let Some(server) = GLOBAL_IPC_SERVER.get() {
                                        let payload = serde_json::json!({
                                            "type": "sync-activate",
                                            "wiki_path": wiki_path,
                                            "sync_id": sync_id,
                                        }).to_string();
                                        server.send_lan_sync_to_all("*", &payload);
                                        eprintln!("[IPC] Sent sync-activate to new client: wiki={}, sync_id={}", wiki_path, sync_id);
                                    }
                                    // Also trigger on_wiki_opened to drain pending
                                    // buffers and request fingerprint sync from peers.
                                    // This ensures buffered changes are delivered even
                                    // if JS hasn't called lan_sync_wiki_opened yet.
                                    let sync_id = sync_id.clone();
                                    tauri::async_runtime::spawn(async move {
                                        if let Some(mgr) = lan_sync::get_sync_manager() {
                                            mgr.on_wiki_opened(&sync_id).await;
                                        }
                                    });
                                }
                            }
                            break;
                        }
                    }
                    if !found {
                        eprintln!("[IPC] on_client_registered: no matching sync-enabled entry for {:?} ({} entries checked)", wiki_path, entries.len());
                    }
                }
            });

            if let Err(e) = server.start() {
                eprintln!("[TiddlyDesktop] IPC server error: {}", e);
            }
        });
    }

    // Normal mode: main browser with wiki list

    // Linux: Configure WebKitGTK hardware acceleration
    // Users can set TIDDLYDESKTOP_DISABLE_GPU=1 to disable hardware acceleration
    // (useful for older nvidia cards with nouveau driver, or other GPU issues)
    #[cfg(target_os = "linux")]
    {
        // Helper to set env var only if not already set by user
        fn set_env_if_unset(key: &str, value: &str) {
            if std::env::var(key).is_err() {
                std::env::set_var(key, value);
            }
        }

        // Check if user has set any WebKit env vars directly
        let user_set_compositing = std::env::var("WEBKIT_DISABLE_COMPOSITING_MODE").is_ok();
        let user_set_dmabuf = std::env::var("WEBKIT_DISABLE_DMABUF_RENDERER").is_ok();
        let user_set_libgl = std::env::var("LIBGL_ALWAYS_SOFTWARE").is_ok();

        if std::env::var("TIDDLYDESKTOP_DISABLE_GPU").map(|v| v == "1" || v.to_lowercase() == "true").unwrap_or(false) {
            // Disable hardware acceleration for problematic GPU drivers
            eprintln!("[TiddlyDesktop] GPU acceleration disabled via TIDDLYDESKTOP_DISABLE_GPU");
            set_env_if_unset("WEBKIT_DISABLE_COMPOSITING_MODE", "1");
            set_env_if_unset("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
            set_env_if_unset("LIBGL_ALWAYS_SOFTWARE", "1");
        } else {
            // Only set defaults if user hasn't specified their own values
            set_env_if_unset("WEBKIT_DISABLE_COMPOSITING_MODE", "0");
            set_env_if_unset("WEBKIT_DISABLE_DMABUF_RENDERER", "0");
        }

        // Log if user has set custom values
        if user_set_compositing || user_set_dmabuf || user_set_libgl {
            eprintln!("[TiddlyDesktop] Using user-provided WebKit environment variables");
        }

        // Print helpful hints for troubleshooting display issues
        eprintln!("[TiddlyDesktop] Linux: If you experience display issues (black artifacts, rendering problems), try:");
        eprintln!("[TiddlyDesktop]   WEBKIT_DISABLE_DMABUF_RENDERER=1 tiddlydesktop-rs");
        eprintln!("[TiddlyDesktop]   WEBKIT_DISABLE_COMPOSITING_MODE=1 tiddlydesktop-rs");
        eprintln!("[TiddlyDesktop]   TIDDLYDESKTOP_DISABLE_GPU=1 tiddlydesktop-rs  (disables all GPU acceleration)");
    }

    let builder = tauri::Builder::default()
        .with_platform_plugins()
        .plugin(drag_drop::init_plugin());
    #[cfg(not(target_os = "android"))]
    let builder = builder.plugin(permissions::init_plugin());
    let builder = builder.setup(|app| {
            // Replace default menu bar with minimal one on macOS (keeps essential shortcuts)
            #[cfg(target_os = "macos")]
            {
                let handle = app.handle();
                let app_menu = Submenu::with_items(handle, "TiddlyDesktopRS", true, &[
                    &PredefinedMenuItem::about(handle, Some("About TiddlyDesktopRS"), None)?,
                    &PredefinedMenuItem::separator(handle)?,
                    &PredefinedMenuItem::hide(handle, None)?,
                    &PredefinedMenuItem::hide_others(handle, None)?,
                    &PredefinedMenuItem::show_all(handle, None)?,
                    &PredefinedMenuItem::separator(handle)?,
                    &PredefinedMenuItem::quit(handle, None)?,
                ])?;
                let edit_menu = Submenu::with_items(handle, "Edit", true, &[
                    &PredefinedMenuItem::undo(handle, None)?,
                    &PredefinedMenuItem::redo(handle, None)?,
                    &PredefinedMenuItem::separator(handle)?,
                    &PredefinedMenuItem::cut(handle, None)?,
                    &PredefinedMenuItem::copy(handle, None)?,
                    &PredefinedMenuItem::paste(handle, None)?,
                    &PredefinedMenuItem::select_all(handle, None)?,
                ])?;
                let window_menu = Submenu::with_items(handle, "Window", true, &[
                    &PredefinedMenuItem::minimize(handle, None)?,
                    &PredefinedMenuItem::close_window(handle, None)?,
                ])?;
                let menu = Menu::with_items(handle, &[&app_menu, &edit_menu, &window_menu])?;
                app.set_menu(menu)?;
            }

            // Store global AppHandle for IPC callbacks
            let _ = GLOBAL_APP_HANDLE.set(app.handle().clone());

            // Initialize PDFium for native PDF rendering
            init_pdfium_from_resources(&app.handle());

            // Ensure main wiki exists (creates from template if needed)
            // This also handles first-run mode selection on macOS/Linux
            let main_wiki_path = ensure_main_wiki_exists(app)
                .map_err(|e| Box::new(std::io::Error::new(std::io::ErrorKind::Other, e)) as Box<dyn std::error::Error>)?;

            println!("Main wiki path: {:?}", main_wiki_path);

            // Initialize app state


            app.manage(AppState {
                wiki_paths: Mutex::new(HashMap::new()),
                open_wikis: Mutex::new(HashMap::new()),
                wiki_processes: Mutex::new(HashMap::new()),
                next_port: Mutex::new(8080),
                main_wiki_path: main_wiki_path.clone(),

                folder_wiki_paths: Mutex::new(HashMap::new()),
                saf_wiki_mappings: Mutex::new(HashMap::new()),
            });

            // Start localhost HTTP media server (Linux: GStreamer needs HTTP URLs;
            // also used for folder wikis on all platforms)
            #[cfg(not(target_os = "android"))]
            match media_server::MediaServer::start() {
                Ok(server) => {
                    app.manage(MediaServerState { server });
                }
                Err(e) => {
                    eprintln!("[TiddlyDesktop] Failed to start media server: {}", e);
                }
            }

            // Initialize LAN Sync manager
            {
                let data_dir = get_data_dir(app.handle())
                    .unwrap_or_else(|_| PathBuf::from("."));
                let sync_data_dir = data_dir.join("lan_sync");
                let _ = std::fs::create_dir_all(&sync_data_dir);

                // Initialize the sync manager (creates device identity, loads paired devices)
                // The actual server starts when the user enables LAN sync from the UI
                let _sync_manager = lan_sync::SyncManager::init(&sync_data_dir);
                eprintln!("[TiddlyDesktop] LAN Sync manager initialized");

                // Start background event loop and auto-connect relay rooms
                // (independent of LAN sync — relay rooms can run without LAN sync)
                tauri::async_runtime::spawn(async {
                    if let Some(mgr) = lan_sync::get_sync_manager() {
                        mgr.start_background().await;
                    }
                });

            }

            // Create a unique key for the main wiki path
            let path_key = utils::base64_url_encode(&main_wiki_path.to_string_lossy());

            // Store the path mapping for the protocol handler
            let state = app.state::<AppState>();
            state.wiki_paths.lock().unwrap().insert(path_key.clone(), main_wiki_path.clone());
            state.wiki_paths.lock().unwrap().insert(format!("{}_label", path_key), PathBuf::from("main"));

            // Track main wiki as open
            state.open_wikis.lock().unwrap().insert("main".to_string(), main_wiki_path.to_string_lossy().to_string());

            // Use wikifile:// protocol to load main wiki
            let wiki_url = format!("wikifile://localhost/{}", path_key);

            // Load saved window state for landing page
            let saved_state = wiki_storage::get_window_state(&app.handle(), "__LANDING_PAGE__");
            let (win_width, win_height) = {
                let (w, h) = saved_state.as_ref()
                    .map(|s| (s.width as f64, s.height as f64))
                    .unwrap_or((800.0, 600.0));

                // On Linux, clamp size to prevent GNOME's auto-maximize (only if not maximized)
                #[cfg(target_os = "linux")]
                let (w, h) = if !saved_state.as_ref().map(|s| s.maximized).unwrap_or(false) {
                    linux_clamp_window_size(w, h)
                } else {
                    (w, h)
                };

                (w, h)
            };
            eprintln!("[TiddlyDesktop] Landing page saved state: {:?}", saved_state);
            eprintln!("[TiddlyDesktop] Using size: {}x{}", win_width, win_height);

            // Get effective language (user preference or system-detected)
            let language = wiki_storage::get_effective_language(&app.handle());
            eprintln!("[TiddlyDesktop] UI language: {}", language);

            // Create the main window programmatically with initialization script
            // Use full init script with is_main_wiki=true so setupExternalAttachments knows to skip
            #[cfg(not(target_os = "android"))]
            let icon = Image::from_bytes(include_bytes!("../icons/icon.png"))?;
            #[allow(unused_mut)]
            #[cfg(not(target_os = "android"))]
            let mut builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(wiki_url.parse().unwrap()))
                .title("TiddlyDesktopRS")
                .inner_size(win_width, win_height)
                .icon(icon)?
                .window_classname("tiddlydesktop-rs")
                .initialization_script(&init_script::get_wiki_init_script_with_language(&main_wiki_path.to_string_lossy(), "main", true, Some(&language)))
                .zoom_hotkeys_enabled(true)
                .devtools(cfg!(debug_assertions)); // Only enable in debug builds

            #[cfg(target_os = "linux")]
            let mut builder = builder.user_agent(LINUX_USER_AGENT);

            // Android: Extract resources synchronously if needed (first run)
            // This takes ~1.5 seconds with ZIP extraction, so we do it before window creation
            #[cfg(target_os = "android")]
            if needs_resource_extraction(app) {
                eprintln!("[TiddlyDesktop] First run detected, extracting resources...");
                if let Err(e) = extract_tiddlywiki_resources(app) {
                    eprintln!("[TiddlyDesktop] Resource extraction failed: {}", e);
                }
            }

            // Android: Verify Node.js binary is ready (extracted via ZIP in extract_tiddlywiki_resources)
            #[cfg(target_os = "android")]
            if let Err(e) = android::node_bridge::ensure_node_binary(app) {
                eprintln!("[TiddlyDesktop] Node.js binary check failed: {}", e);
                // Non-fatal - wiki viewing still works, just not creation/serving
            }

            // Android: Clean up any stale wiki mirror directories from previous sessions
            #[cfg(target_os = "android")]
            android::node_bridge::cleanup_stale_wiki_mirrors();

            // Android: Create window with wiki URL directly (resources are already extracted)
            // Note: Individual wikis open in separate WikiActivity instances (not Tauri-based)
            #[cfg(target_os = "android")]
            let builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(wiki_url.parse().unwrap()))
                .initialization_script(&init_script::get_wiki_init_script_with_language(&main_wiki_path.to_string_lossy(), "main", true, Some(&language)));

            // Apply saved position if available, with monitor validation on Windows/macOS
            // (Android windows are fullscreen - no position needed)
            #[cfg(not(target_os = "android"))]
            if let Some(ref state) = saved_state {
                let (x, y) = validate_window_position(app.handle(), state);
                builder = builder.position(x, y);
            }

            // Tauri's drag/drop handler: On Windows, our WRY patch intercepts drops,
            // extracts file paths, emits tauri://drag-* events, then forwards to WebView2.
            // On Linux, vanilla WebKitGTK handles drops natively.

            let main_window = builder.build()?;

            // Note: Drag handlers are set up via the drag_drop plugin's on_webview_ready hook

            // Linux: Set up HeaderBar, enable smooth scrolling, finalize window state
            #[cfg(target_os = "linux")]
            {
                setup_header_bar(&main_window);
                enable_smooth_scrolling(&main_window);
                linux_finalize_window_state(&main_window, &saved_state);
            }

            // Restore maximized state (Windows/macOS only - Linux handled in linux_finalize_window_state)
            // (Android windows are always fullscreen)
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            if saved_state.as_ref().map(|s| s.maximized).unwrap_or(false) {
                let _ = main_window.maximize();
            }

            // Android: window is already created with wiki URL (resources extracted synchronously above)
            #[cfg(target_os = "android")]
            let _ = &main_window;

            #[cfg(not(target_os = "android"))]
            setup_system_tray(app)?;

            // Handle files passed as command-line arguments
            let args: Vec<String> = std::env::args().skip(1).collect();
            for arg in args {
                let path = PathBuf::from(&arg);
                // Only open files that exist and have .html or .htm extension
                if path.exists() && path.is_file() {
                    if let Some(ext) = path.extension() {
                        let ext_lower = ext.to_string_lossy().to_lowercase();
                        if ext_lower == "html" || ext_lower == "htm" {
                            let app_handle = app.handle().clone();
                            let path_str = arg.clone();
                            tauri::async_runtime::spawn(async move {
                                if let Ok(entry) = open_wiki_window(app_handle.clone(), path_str, None, None, None).await {
                                    // Emit event to refresh wiki list in main window
                                    let _ = app_handle.emit("wiki-list-changed", entry);
                                }
                            });
                        }
                    }
                }
            }

            Ok(())
        })
        .register_uri_scheme_protocol("wikifile", |ctx, request| {
            wiki_protocol_handler(ctx.app_handle(), request)
        })
        .register_asynchronous_uri_scheme_protocol("tdasset", |_ctx, request, responder| {
            std::thread::spawn(move || {
                responder.respond(tdasset_protocol_handler(request));
            });
        });
        // tdlib:// protocol is desktop-only (Android serves libraries via WikiHttpServer /_td/)
        #[cfg(not(target_os = "android"))]
        let builder = builder.register_uri_scheme_protocol("tdlib", |ctx, request| {
            tdlib_protocol_handler(ctx.app_handle(), request)
        });
        builder.plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .invoke_handler(tauri::generate_handler![
            load_wiki,
            save_wiki,
            open_wiki_window,
            open_wiki_folder,
            open_tiddler_window,
            check_is_wiki_folder,
            check_folder_status,
            get_available_editions,
            get_available_plugins,
            init_wiki_folder,
            create_wiki_file,
            convert_wiki,
            set_window_title,
            set_window_icon,
            set_headerbar_colors,
            get_window_label,
            get_main_wiki_path,
            reveal_in_folder,
            show_alert,
            show_confirm,
            close_window,
            close_window_by_label,
            is_directory,
            get_window_state_info,
            get_saved_window_state,
            wiki_storage::save_window_state,
            wiki_storage::get_recent_files,
            wiki_storage::remove_recent_file,
            wiki_storage::set_wiki_backups,
            wiki_storage::set_wiki_backup_dir,
            wiki_storage::set_wiki_backup_count,
            wiki_storage::update_wiki_favicon,
            wiki_storage::get_wiki_backup_dir_setting,
            wiki_storage::set_wiki_sync,
            wiki_storage::get_wiki_sync_id,
            wiki_storage::lan_sync_link_wiki,
            wiki_storage::set_wiki_group,
            wiki_storage::get_wiki_groups,
            wiki_storage::rename_wiki_group,
            wiki_storage::delete_wiki_group,
            read_file_as_data_uri,
            read_file_as_binary,
            save_binary_file,
            pick_files_for_import,
            wiki_storage::get_external_attachments_config,
            wiki_storage::set_external_attachments_config,
            wiki_storage::get_session_auth_config,
            wiki_storage::set_session_auth_config,
            wiki_storage::get_language,
            wiki_storage::set_language,
            wiki_storage::has_custom_language,
            wiki_storage::get_system_language,
            wiki_storage::get_palette,
            wiki_storage::set_palette,
            wiki_storage::get_custom_plugin_path,
            wiki_storage::set_custom_plugin_path,
            wiki_storage::get_custom_edition_path,
            wiki_storage::set_custom_edition_path,
            open_auth_window,
            clear_wiki_session,

            show_find_in_page,
            toggle_fullscreen,
            print_page,
            set_zoom_level,
            download_file,
            wiki_storage::js_log,
            clipboard::get_clipboard_content,
            clipboard::set_clipboard_content,
            start_native_drag,
            prepare_native_drag,
            cleanup_native_drag,
            get_pending_drag_data,
            get_external_drop_paths,
            update_drag_icon,
            set_pending_drag_icon,
            set_drag_dest_enabled,
            ungrab_seat_for_focus,
            set_over_droppable,
            set_internal_drag_type,
            check_for_updates,
            // Android SAF commands (stubs on desktop)
            android_pick_wiki_file,
            android_pick_directory,
            android_create_wiki_file,
            android_has_permission,
            android_pick_backup_directory,
            android_pick_wiki_folder,
            android_list_directory,
            android_release_permission,
            android_check_folder_status,
            android_pick_folder_for_wiki_creation,
            android_set_system_bar_colors,
            android_copy_attachment,
            android_save_attachment,
            get_pending_widget_wiki,
            extract_video_poster,
            register_media_url,
            // PDF rendering commands
            pdf_open,
            pdf_open_file,
            pdf_render_page,
            pdf_close,
            pdf_char_at_pos,
            pdf_selection_rects,
            pdf_get_text,
            pdf_char_count,
            // LAN Sync commands
            lan_sync::lan_sync_start,
            lan_sync::lan_sync_stop,
            lan_sync::lan_sync_get_status,
            lan_sync::lan_sync_announce_username,
            lan_sync::lan_sync_tiddler_changed,
            lan_sync::lan_sync_tiddler_deleted,
            lan_sync::lan_sync_wiki_opened,
            lan_sync::lan_sync_get_available_wikis,
            lan_sync::lan_sync_request_wiki,
            lan_sync::lan_sync_send_full_sync,
            lan_sync::lan_sync_send_fingerprints,
            lan_sync::lan_sync_broadcast_fingerprints,
            lan_sync::lan_sync_broadcast_manifest,
            lan_sync::lan_sync_poll_ipc,
            lan_sync::lan_sync_load_tombstones,
            lan_sync::lan_sync_save_tombstones,
            lan_sync::lan_sync_collab_editing_started,
            lan_sync::lan_sync_collab_editing_stopped,
            lan_sync::lan_sync_collab_peer_saved,
            lan_sync::lan_sync_collab_update,
            lan_sync::lan_sync_collab_awareness,
            lan_sync::lan_sync_get_remote_editors,
            lan_sync::lan_sync_get_collab_port,
            // Relay sync commands
            lan_sync::relay_sync_get_status,
            lan_sync::relay_sync_add_room,
            lan_sync::relay_sync_remove_room,
            lan_sync::relay_sync_connect_room,
            lan_sync::relay_sync_disconnect_room,
            lan_sync::relay_sync_set_room_auto_connect,
            lan_sync::relay_sync_set_room_password,
            lan_sync::relay_sync_set_room_name,
            lan_sync::relay_sync_get_room_credentials,
            lan_sync::relay_sync_set_url,
            lan_sync::relay_sync_generate_credentials,
            // Per-wiki relay room assignment
            wiki_storage::set_wiki_relay_room,
            get_wiki_installed_plugins,
            install_plugins_to_wiki
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            match event {
                // Prevent app exit if wiki windows are still open
                tauri::RunEvent::ExitRequested { api, .. } => {
                    let state = app.state::<AppState>();
                    let wiki_count = state.wiki_processes.lock().unwrap().len();
                    if wiki_count > 0 {
                        eprintln!("[TiddlyDesktop] Preventing exit - {} wiki(s) still open", wiki_count);
                        api.prevent_exit();
                    }
                }
                // Handle files opened via macOS file associations
                #[cfg(target_os = "macos")]
                tauri::RunEvent::Opened { urls } => {
                    for url in urls {
                        if let Ok(path) = url.to_file_path() {
                            if let Some(ext) = path.extension() {
                                let ext_lower = ext.to_string_lossy().to_lowercase();
                                if ext_lower == "html" || ext_lower == "htm" {
                                    let app_handle = app.clone();
                                    let path_str = path.to_string_lossy().to_string();
                                    tauri::async_runtime::spawn(async move {
                                        if let Ok(entry) = open_wiki_window(app_handle.clone(), path_str, None, None, None).await {
                                            // Emit event to refresh wiki list in main window
                                            let _ = app_handle.emit("wiki-list-changed", entry);
                                        }
                                    });
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        });
}

/// JNI_OnLoad - Called when the native library is loaded by Android.
/// Captures the JavaVM for later use by wiki_activity module.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn JNI_OnLoad(
    vm: jni::JavaVM,
    _reserved: *mut std::ffi::c_void,
) -> jni::sys::jint {
    eprintln!("[TiddlyDesktop] JNI_OnLoad called, capturing JavaVM");
    android::wiki_activity::set_java_vm(vm);
    jni::sys::JNI_VERSION_1_6
}

/// JNI function called from WikiActivity.onDestroy() to clean up local wiki copies.
/// Only relevant for folder wikis that use Node.js server with local copies.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_burningtreec_tiddlydesktop_1rs_WikiActivity_cleanupWikiLocalCopy(
    mut env: jni::JNIEnv,
    _class: jni::objects::JClass,
    wiki_path: jni::objects::JString,
    is_folder: jni::sys::jboolean,
) {
    // Only folder wikis have local copies to clean up
    if is_folder == 0 {
        return;
    }

    let wiki_path_str: String = match env.get_string(&wiki_path) {
        Ok(s) => s.into(),
        Err(e) => {
            eprintln!("[TiddlyDesktop] JNI cleanupWikiLocalCopy: Failed to get wiki_path string: {}", e);
            return;
        }
    };

    eprintln!("[TiddlyDesktop] JNI cleanupWikiLocalCopy called for: {}", wiki_path_str);
    android::node_bridge::unregister_running_server(&wiki_path_str);
    android::node_bridge::cleanup_wiki_local_copy(&wiki_path_str);
}

/// JNI function called from WikiActivity to restart a folder wiki's Node.js server.
/// Returns the new server URL on success, or an empty string on failure.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_burningtreec_tiddlydesktop_1rs_WikiActivity_restartFolderWikiServer<'a>(
    mut env: jni::JNIEnv<'a>,
    _class: jni::objects::JClass<'a>,
    wiki_path: jni::objects::JString<'a>,
) -> jni::objects::JString<'a> {
    let wiki_path_str: String = match env.get_string(&wiki_path) {
        Ok(s) => s.into(),
        Err(e) => {
            eprintln!("[TiddlyDesktop] JNI restartFolderWikiServer: Failed to get wiki_path string: {}", e);
            return env.new_string("").unwrap();
        }
    };

    eprintln!("[TiddlyDesktop] JNI restartFolderWikiServer called for: {}", wiki_path_str);

    // Unregister any stale entry
    android::node_bridge::unregister_running_server(&wiki_path_str);

    let is_saf_uri = wiki_path_str.starts_with("content://") || wiki_path_str.starts_with('{');

    let result = if is_saf_uri {
        // SAF URI: clean up old local copy first, then start fresh
        android::node_bridge::cleanup_wiki_local_copy(&wiki_path_str);
        android::node_bridge::start_saf_wiki_server(&wiki_path_str).map(|(url, _)| url)
    } else {
        android::node_bridge::find_available_port()
            .and_then(|port| android::node_bridge::start_wiki_server(&wiki_path_str, port))
    };

    match result {
        Ok(url) => {
            android::node_bridge::register_running_server(&wiki_path_str, &url);
            eprintln!("[TiddlyDesktop] Folder wiki server restarted at: {}", url);
            env.new_string(&url).unwrap()
        }
        Err(e) => {
            eprintln!("[TiddlyDesktop] Failed to restart folder wiki server: {}", e);
            env.new_string("").unwrap()
        }
    }
}

/// JNI function to start a Node.js server from a local filesystem path.
/// Unlike restartFolderWikiServer, this does NOT do any SAF operations —
/// it expects the wiki files to already be at the local path.
/// Returns the server URL on success, or "ERROR:..." on failure.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_burningtreec_tiddlydesktop_1rs_WikiActivity_startFolderWikiServerFromLocal<'a>(
    mut env: jni::JNIEnv<'a>,
    _class: jni::objects::JClass<'a>,
    local_path: jni::objects::JString<'a>,
    wiki_path: jni::objects::JString<'a>,
) -> jni::objects::JString<'a> {
    let local_path_str: String = match env.get_string(&local_path) {
        Ok(s) => s.into(),
        Err(e) => {
            return env.new_string(format!("ERROR:Failed to get local_path: {}", e)).unwrap();
        }
    };
    let wiki_path_str: String = match env.get_string(&wiki_path) {
        Ok(s) => s.into(),
        Err(e) => {
            return env.new_string(format!("ERROR:Failed to get wiki_path: {}", e)).unwrap();
        }
    };

    eprintln!("[TiddlyDesktop] JNI startFolderWikiServerFromLocal: {} (wiki: {})", local_path_str, wiki_path_str);

    // Unregister any stale entry
    android::node_bridge::unregister_running_server(&wiki_path_str);

    let result = android::node_bridge::find_available_port()
        .and_then(|port| android::node_bridge::start_wiki_server(&local_path_str, port));

    match result {
        Ok(url) => {
            android::node_bridge::register_running_server(&wiki_path_str, &url);
            eprintln!("[TiddlyDesktop] Folder wiki server started at: {}", url);
            env.new_string(&url).unwrap()
        }
        Err(e) => {
            eprintln!("[TiddlyDesktop] Failed to start folder wiki server: {}", e);
            env.new_string(format!("ERROR:{}", e)).unwrap()
        }
    }
}

/// JNI: Open a PDF from base64-encoded data. Returns JSON string with handle + page metadata.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_burningtreec_tiddlydesktop_1rs_WikiActivity_pdfOpen<'a>(
    mut env: jni::JNIEnv<'a>,
    _class: jni::objects::JClass<'a>,
    data_base64: jni::objects::JString<'a>,
) -> jni::objects::JString<'a> {
    // Lazy-init PDFium in the :wiki process (main process inits in Tauri setup,
    // but JNI calls come from the :wiki process which has a separate address space)
    if !pdf_renderer::is_initialized() {
        // Try loading from nativeLibraryDir (full path)
        let init_result = match crate::android::node_bridge::get_native_library_dir_pub() {
            Ok(dir) => {
                let lib_path = format!("{}/libpdfium.so", dir);
                pdf_renderer::init_pdfium(&lib_path)
            }
            Err(_) => pdf_renderer::init_pdfium("libpdfium.so"),
        };
        if let Err(e) = init_result {
            let err = format!("{{\"error\":\"Failed to initialize PDFium: {}\"}}", e.replace('"', "'"));
            return env.new_string(&err).unwrap();
        }
    }

    let b64_str: String = match env.get_string(&data_base64) {
        Ok(s) => s.into(),
        Err(e) => {
            let err = format!("{{\"error\":\"Failed to get base64 string: {}\"}}", e);
            return env.new_string(&err).unwrap();
        }
    };

    let bytes = match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &b64_str) {
        Ok(b) => b,
        Err(e) => {
            let err = format!("{{\"error\":\"Invalid base64: {}\"}}", e);
            return env.new_string(&err).unwrap();
        }
    };

    match pdf_renderer::pdf_open(bytes) {
        Ok(result) => {
            let json = serde_json::to_string(&result).unwrap_or_else(|_| "{}".to_string());
            env.new_string(&json).unwrap()
        }
        Err(e) => {
            let err = format!("{{\"error\":\"{}\"}}", e.replace('"', "'"));
            env.new_string(&err).unwrap()
        }
    }
}

/// JNI: Render a single page as PNG + text items. Returns JSON string.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_burningtreec_tiddlydesktop_1rs_WikiActivity_pdfRenderPage<'a>(
    mut env: jni::JNIEnv<'a>,
    _class: jni::objects::JClass<'a>,
    handle: jni::sys::jlong,
    page_num: jni::sys::jint,
    width_px: jni::sys::jint,
) -> jni::objects::JString<'a> {
    match pdf_renderer::pdf_render_page(handle as u64, page_num as u32, width_px as u32) {
        Ok(result) => {
            let json = serde_json::to_string(&result).unwrap_or_else(|_| "{}".to_string());
            env.new_string(&json).unwrap()
        }
        Err(e) => {
            let err = format!("{{\"error\":\"{}\"}}", e.replace('"', "'"));
            env.new_string(&err).unwrap()
        }
    }
}

/// JNI: Close a PDF document and release its handle.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_burningtreec_tiddlydesktop_1rs_WikiActivity_pdfClose(
    _env: jni::JNIEnv,
    _class: jni::objects::JClass,
    handle: jni::sys::jlong,
) {
    pdf_renderer::pdf_close(handle as u64);
}

/// JNI: Hit-test character at pixel position. Returns char index or -1.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_burningtreec_tiddlydesktop_1rs_WikiActivity_pdfCharAtPos(
    _env: jni::JNIEnv,
    _class: jni::objects::JClass,
    handle: jni::sys::jlong,
    page_num: jni::sys::jint,
    pixel_x: jni::sys::jint,
    pixel_y: jni::sys::jint,
    render_width: jni::sys::jint,
) -> jni::sys::jint {
    match pdf_renderer::pdf_char_at_pos(handle as u64, page_num as u32, pixel_x, pixel_y, render_width as u32) {
        Ok(idx) => idx,
        Err(_) => -1,
    }
}

/// JNI: Get selection rectangles for character range. Returns JSON array.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_burningtreec_tiddlydesktop_1rs_WikiActivity_pdfSelectionRects<'a>(
    mut env: jni::JNIEnv<'a>,
    _class: jni::objects::JClass<'a>,
    handle: jni::sys::jlong,
    page_num: jni::sys::jint,
    start_idx: jni::sys::jint,
    end_idx: jni::sys::jint,
    render_width: jni::sys::jint,
) -> jni::objects::JString<'a> {
    match pdf_renderer::pdf_selection_rects(handle as u64, page_num as u32, start_idx as u32, end_idx as u32, render_width as u32) {
        Ok(rects) => {
            let json = serde_json::to_string(&rects).unwrap_or_else(|_| "[]".to_string());
            env.new_string(&json).unwrap()
        }
        Err(_) => {
            env.new_string("[]").unwrap()
        }
    }
}

/// JNI: Extract text for character range.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_burningtreec_tiddlydesktop_1rs_WikiActivity_pdfGetText<'a>(
    mut env: jni::JNIEnv<'a>,
    _class: jni::objects::JClass<'a>,
    handle: jni::sys::jlong,
    page_num: jni::sys::jint,
    start_idx: jni::sys::jint,
    end_idx: jni::sys::jint,
) -> jni::objects::JString<'a> {
    match pdf_renderer::pdf_get_text(handle as u64, page_num as u32, start_idx as u32, end_idx as u32) {
        Ok(text) => env.new_string(&text).unwrap(),
        Err(_) => env.new_string("").unwrap(),
    }
}

/// JNI: Get total character count for a page.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_com_burningtreec_tiddlydesktop_1rs_WikiActivity_pdfCharCount(
    _env: jni::JNIEnv,
    _class: jni::objects::JClass,
    handle: jni::sys::jlong,
    page_num: jni::sys::jint,
) -> jni::sys::jint {
    match pdf_renderer::pdf_char_count(handle as u64, page_num as u32) {
        Ok(count) => count as jni::sys::jint,
        Err(_) => 0,
    }
}
