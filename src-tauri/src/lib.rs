use std::{collections::HashMap, path::PathBuf, process::{Child, Command}, sync::{Arc, Mutex, OnceLock}};
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

/// Global AppHandle for IPC callbacks that need Tauri access
static GLOBAL_APP_HANDLE: OnceLock<tauri::AppHandle> = OnceLock::new();

/// Global IPC server for sending messages to wiki processes
static GLOBAL_IPC_SERVER: OnceLock<Arc<ipc::IpcServer>> = OnceLock::new();

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

/// Linux: Set up a GtkHeaderBar on a window for reliable title display
/// This works around WebKitGTK's broken title propagation
/// Title starts empty - JavaScript will set the real title once TiddlyWiki loads
#[cfg(target_os = "linux")]
fn setup_header_bar(window: &tauri::WebviewWindow) {
    use gtk::prelude::{ButtonExt, ContainerExt, EventBoxExt, GtkSettingsExt, GtkWindowExt, HeaderBarExt, LabelExt, OverlayExt, StyleContextExt, WidgetExt, WidgetExtManual};
    use gtk::glib;

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

        // Close button overlaid on the right
        let close_button = gtk::Button::from_icon_name(Some("window-close-symbolic"), gtk::IconSize::Menu);
        close_button.set_halign(gtk::Align::End);
        close_button.set_valign(gtk::Align::Center);
        close_button.set_margin_end(4);
        close_button.style_context().add_class("titlebutton");
        close_button.style_context().add_class("close");
        let win_weak_close = glib::object::ObjectExt::downgrade(&gtk_window);
        close_button.connect_clicked(move |_| {
            if let Some(win) = win_weak_close.upgrade() {
                win.close();
            }
        });
        overlay.add_overlay(&close_button);

        event_box.add(&overlay);

        // Enable events on the event box for dragging
        event_box.add_events(
            gdk::EventMask::BUTTON_PRESS_MASK
            | gdk::EventMask::BUTTON_RELEASE_MASK
            | gdk::EventMask::POINTER_MOTION_MASK
        );

        // Get drag threshold from GTK settings (typically 8 pixels)
        let drag_threshold = gtk::Settings::default()
            .and_then(|s| Some(s.gtk_dnd_drag_threshold()))
            .unwrap_or(8);

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

/// Inter-process communication for multi-process wiki architecture
mod ipc;

/// JavaScript initialization scripts for wiki windows
mod init_script;

/// Core data types
mod types;
pub use types::{WikiEntry, ExternalAttachmentsConfig, AuthUrlEntry, SessionAuthConfig, WikiConfigs, EditionInfo, PluginInfo, FolderStatus, CommandResult};

/// Clipboard operations
mod clipboard;

/// Utility functions
mod utils;

/// Wiki storage and recent files management
mod wiki_storage;

/// TiddlyWiki HTML manipulation
mod tiddlywiki_html;

use chrono::Local;
use tauri::{
    image::Image,
    http::{Request, Response},
    menu::{MenuBuilder, MenuItemBuilder},
    tray::TrayIconBuilder,
    Emitter, Manager, WebviewUrl, WebviewWindowBuilder,
};
/// Determine storage mode for macOS/Linux
/// Always uses the app data directory (portable mode only available on Windows)
#[cfg(not(target_os = "windows"))]
fn determine_storage_mode(app: &tauri::App) -> Result<PathBuf, String> {
    app.path().app_data_dir().map_err(|e| e.to_string())
}

/// Windows: determine storage mode based on marker file
#[cfg(target_os = "windows")]
fn determine_storage_mode(app: &tauri::App) -> Result<PathBuf, String> {
    let exe_path = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe_dir = exe_path.parent().ok_or("No exe directory")?;

    // Check for portable marker
    if exe_dir.join("portable").exists() || exe_dir.join("portable.txt").exists() {
        return Ok(exe_dir.to_path_buf());
    }

    // Check if portable data file already exists (user chose portable mode previously)
    if exe_dir.join("tiddlydesktop.html").exists() {
        return Ok(exe_dir.to_path_buf());
    }

    // Installed mode: app data directory
    app.path().app_data_dir().map_err(|e| e.to_string())
}

/// Get the user editions directory path
/// Location: ~/.local/share/tiddlydesktop-rs/editions/ (Linux) or equivalent on other platforms
fn get_user_editions_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
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
}

/// Get the bundled index.html path
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

/// Ensure main wiki file exists, extracting from resources if needed
/// Also handles migration when bundled version is newer than existing
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

/// Create a backup of the wiki file before saving
/// If custom_backup_dir is Some, backups go there; otherwise to .backups folder next to wiki
async fn create_backup(path: &PathBuf, custom_backup_dir: Option<&str>) -> Result<(), String> {
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

    // Clean up old backups (keep last 20)
    cleanup_old_backups(&backup_dir, 20).await;

    Ok(())
}

/// Remove old backups, keeping only the most recent ones
async fn cleanup_old_backups(backup_dir: &PathBuf, keep: usize) {
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
    // Security: Validate the path before reading
    let validated_path = drag_drop::sanitize::validate_wiki_path(&path)?;

    tokio::fs::read_to_string(&validated_path)
        .await
        .map_err(|e| format!("Failed to read wiki: {}", e))
}

/// Save wiki content to disk with backup
#[tauri::command]
async fn save_wiki(app: tauri::AppHandle, path: String, content: String) -> Result<(), String> {
    // Security: Validate the path before writing
    let validated_path = drag_drop::sanitize::validate_wiki_path_for_write(&path)?;

    // Check if backups are enabled for this wiki
    let state = app.state::<AppState>();
    if should_create_backup(&app, &state, &path) {
        let backup_dir = get_wiki_backup_dir(&app, &path);
        create_backup(&validated_path, backup_dir.as_deref()).await?;
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

        #[cfg(not(target_os = "linux"))]
        {
            window.set_title(&title).map_err(|e| e.to_string())?;
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
                                #td-headerbar button.titlebutton:hover {{
                                    background: alpha({}, 0.15);
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

    window.set_icon(icon)
        .map_err(|e| format!("Failed to set icon: {}", e))?;

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
    if let (Ok(canonical_path), Ok(canonical_main)) = (
        dunce::canonicalize(&path_buf),
        dunce::canonicalize(&state.main_wiki_path)
    ) {
        if canonical_path == canonical_main {
            return false;
        }
    } else {
        // Fallback to string comparison if canonicalization fails
        let main_wiki = state.main_wiki_path.to_string_lossy();
        if utils::paths_equal(path, &main_wiki) {
            return false;
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
    let _ = window.destroy();
}

/// Close a window by its label (used by tm-close-window)
#[tauri::command]
fn close_window_by_label(app: tauri::AppHandle, label: String) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(&label) {
        window.destroy().map_err(|e| e.to_string())?;
        Ok(())
    } else {
        Err(format!("Window '{}' not found", label))
    }
}

/// Toggle fullscreen mode for the current window (used by tm-full-screen)
#[tauri::command]
fn toggle_fullscreen(window: tauri::WebviewWindow) -> Result<bool, String> {
    let is_fullscreen = window.is_fullscreen().map_err(|e| e.to_string())?;
    window
        .set_fullscreen(!is_fullscreen)
        .map_err(|e| e.to_string())?;
    Ok(!is_fullscreen)
}

/// Print the current page (used by tm-print)
#[tauri::command]
fn print_page(window: tauri::WebviewWindow) -> Result<(), String> {
    window.print().map_err(|e| e.to_string())
}

/// Show a save file dialog and write content to the selected file (used by tm-download-file)
#[tauri::command]
async fn download_file(
    app: tauri::AppHandle,
    filename: String,
    content: String,
    content_type: Option<String>,
) -> Result<String, String> {
    use tauri_plugin_dialog::DialogExt;

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

    // Show save dialog
    let file_path = app
        .dialog()
        .file()
        .set_file_name(&filename)
        .add_filter(filter_name, extensions)
        .blocking_save_file();

    match file_path {
        Some(path) => {
            // Write the content to the file
            let path_str = path.to_string();
            tokio::fs::write(&path_str, &content)
                .await
                .map_err(|e| format!("Failed to write file: {}", e))?;
            Ok(path_str)
        }
        None => Err("Save cancelled".to_string()),
    }
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
        } else if ((e.key === 'f' || e.key === 'F') && (e.ctrlKey || e.metaKey)) {
            e.preventDefault();
            e.stopPropagation();
            input.focus();
            input.select();
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

/// Run a shell command with optional confirmation dialog
/// Security: Shows a confirmation dialog by default to prevent unauthorized execution
#[tauri::command]
async fn run_command(
    app: tauri::AppHandle,
    command: String,
    args: Option<Vec<String>>,
    working_dir: Option<String>,
    wait: Option<bool>,
    confirm: Option<bool>,
) -> Result<Option<CommandResult>, String> {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

    let should_confirm = confirm.unwrap_or(true); // Default to confirming
    let should_wait = wait.unwrap_or(false);
    let args_vec = args.unwrap_or_default();

    // Build the command string for display
    let display_cmd = if args_vec.is_empty() {
        command.clone()
    } else {
        format!("{} {}", command, args_vec.join(" "))
    };

    // Show confirmation dialog if required
    if should_confirm {
        let message = format!(
            "A wiki wants to run the following command:\n\n{}\n\nDo you want to allow this?",
            display_cmd
        );

        let confirmed = app.dialog()
            .message(message)
            .kind(MessageDialogKind::Warning)
            .title("Execute Command")
            .buttons(MessageDialogButtons::OkCancel)
            .blocking_show();

        if !confirmed {
            return Err("Command execution cancelled by user".to_string());
        }
    }

    // Build the command
    let mut cmd = std::process::Command::new(&command);

    if !args_vec.is_empty() {
        cmd.args(&args_vec);
    }

    if let Some(dir) = working_dir {
        // Security: Validate the working directory is user-accessible
        let validated_dir = drag_drop::sanitize::validate_user_directory_path(&dir)
            .map_err(|e| format!("Invalid working directory: {}", e))?;
        cmd.current_dir(validated_dir);
    }

    // On Windows, hide the console window
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    if should_wait {
        // Run and wait for output
        let output = cmd.output()
            .map_err(|e| format!("Failed to execute command: {}", e))?;

        Ok(Some(CommandResult {
            success: output.status.success(),
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        }))
    } else {
        // Fire and forget
        #[allow(unused_variables)]
        let child = cmd.spawn()
            .map_err(|e| format!("Failed to spawn command: {}", e))?;

        // Windows: Assign to job object so it gets killed when parent exits
        #[cfg(target_os = "windows")]
        drag_drop::windows_job::assign_process_to_job(child.id());

        Ok(None)
    }
}

/// Check if a file is a valid TiddlyWiki HTML file
/// Returns Ok(()) if valid, Err with reason if not
fn validate_tiddlywiki_file(path: &std::path::Path) -> Result<(), String> {
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

    // Read the first 100KB of the file to check for TiddlyWiki markers
    // TiddlyWiki headers and meta tags are always near the top
    let mut file = std::fs::File::open(path)
        .map_err(|e| format!("Failed to open file: {}", e))?;

    let mut buffer = vec![0u8; 100_000]; // 100KB should be enough for headers
    use std::io::Read;
    let bytes_read = file.read(&mut buffer)
        .map_err(|e| format!("Failed to read file: {}", e))?;
    buffer.truncate(bytes_read);

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
    // Read the first 100KB of the file to check for TiddlyWiki markers
    let path_buf = path.to_path_buf();

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

    // Read first 100KB
    let mut file = tokio::fs::File::open(&path_buf).await
        .map_err(|e| format!("Failed to open file: {}", e))?;

    let mut buffer = vec![0u8; 100_000];
    use tokio::io::AsyncReadExt;
    let bytes_read = file.read(&mut buffer).await
        .map_err(|e| format!("Failed to read file: {}", e))?;
    buffer.truncate(bytes_read);

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

/// Get the next available port for a wiki folder server
fn allocate_port(state: &AppState) -> u16 {
    let mut port = state.next_port.lock().unwrap();
    let allocated = *port;
    *port += 1;
    allocated
}

/// Check if system Node.js is available and compatible (v18+)
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

    // Tarball structure has tiddlywiki directly in lib/tiddlydesktop-rs/tiddlywiki/
    let tw_path = resource_path.join("tiddlywiki").join("tiddlywiki.js");
    // Also check Tauri bundle structure with resources/ prefix
    let tw_path_bundled = resource_path.join("resources").join("tiddlywiki").join("tiddlywiki.js");

    // Also check in the development path
    let dev_path = PathBuf::from("src-tauri/resources/tiddlywiki/tiddlywiki.js");

    if tw_path.exists() {
        Ok(tw_path)
    } else if tw_path_bundled.exists() {
        Ok(tw_path_bundled)
    } else if dev_path.exists() {
        let canonical = dev_path.canonicalize().map_err(|e| e.to_string())?;
        Ok(utils::normalize_path(canonical))
    } else {
        Err(format!("TiddlyWiki not found at {:?}, {:?}, or {:?}", tw_path, tw_path_bundled, dev_path))
    }
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
#[tauri::command]
async fn open_wiki_folder(app: tauri::AppHandle, path: String) -> Result<WikiEntry, String> {
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
                favicon: existing_favicon,
                is_folder: true,
                backups_enabled: false,
                backup_dir: None,
                group: None,
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
        path,
        filename: folder_name,
        favicon,
        is_folder: true,
        backups_enabled: false, // Not applicable for folder wikis (they use autosave)
        backup_dir: None,
        group: None,
    };

    // Add to recent files list
    let _ = wiki_storage::add_to_recent_files(&app, entry.clone());

    // Return the wiki entry so frontend can update its list
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
                            let id = path.file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("")
                                .to_string();

                            // Skip internal/core plugins
                            if id == "tiddlyweb" || id == "filesystem" || id == "tiddlydesktop-rs" || id.starts_with("test") {
                                continue;
                            }

                            let name = info.get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or(&id)
                                .to_string();

                            let description = info.get("description")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();

                            // Determine category
                            let category = if editor_plugins.iter().any(|p| id.starts_with(p)) {
                                "Editor"
                            } else if utility_plugins.contains(&id.as_str()) {
                                "Utility"
                            } else if storage_plugins.contains(&id.as_str()) {
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

    // Sort by category, then by name
    plugins.sort_by(|a, b| {
        let cat_order = |c: &str| match c {
            "Editor" => 0,
            "Utility" => 1,
            "Storage" => 2,
            _ => 3,
        };
        cat_order(&a.category).cmp(&cat_order(&b.category))
            .then_with(|| a.name.cmp(&b.name))
    });

    Ok(plugins)
}

/// Initialize a new wiki folder with the specified edition and plugins
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
    let user_editions_dir = get_user_editions_dir(&app)?;
    cmd.env("TIDDLYWIKI_EDITION_PATH", &user_editions_dir);
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

/// Create a single-file wiki with the specified edition and plugins
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
    let user_editions_dir = get_user_editions_dir(&app)?;
    init_cmd.env("TIDDLYWIKI_EDITION_PATH", &user_editions_dir);
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

/// Convert a wiki between single-file and folder formats
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
        if !dest.join("tiddlywiki.info").exists() {
            return Err("Conversion failed - tiddlywiki.info not created".to_string());
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

    // Use the same session directory as the wiki
    if let Some(dir) = session_dir {
        builder = builder.data_directory(dir);
    }

    builder.build()
        .map_err(|e| format!("Failed to create auth window: {}", e))?;

    Ok(())
}

/// Open a wiki file in a separate process
/// Each wiki runs in its own process for true isolation (better drag-drop, crash isolation)
/// Returns WikiEntry so frontend can update its wiki list
#[tauri::command]
async fn open_wiki_window(app: tauri::AppHandle, path: String) -> Result<WikiEntry, String> {
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
                favicon: existing_favicon,
                is_folder: false,
                backups_enabled: true,
                backup_dir: None,
                group: None,
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

    // Create the wiki entry
    let entry = WikiEntry {
        path,
        filename,
        favicon,
        is_folder: false,
        backups_enabled: true,
        backup_dir: None,
        group: None,
    };

    // Add to recent files list
    let _ = wiki_storage::add_to_recent_files(&app, entry.clone());

    // Return the wiki entry so frontend can update its list
    Ok(entry)
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
    let win_width = width.unwrap_or(700.0);
    let win_height = height.unwrap_or(600.0);

    let icon = Image::from_bytes(include_bytes!("../icons/icon.png"))
        .map_err(|e| format!("Failed to load icon: {}", e))?;

    // Get isolated session directory - use the PARENT wiki's path so tiddler windows
    // share session with their parent wiki
    let session_dir = get_wiki_session_dir(&app, &wiki_path);

    // Use full init script for tiddler windows too - they need __WIKI_PATH__ for external attachments
    let mut builder = WebviewWindowBuilder::new(&app, &label, WebviewUrl::External(wiki_url.parse().unwrap()))
        .title(&title)
        .inner_size(win_width, win_height)
        .icon(icon)
        .map_err(|e| format!("Failed to set icon: {}", e))?
        .window_classname("tiddlydesktop-rs")
        .initialization_script(&init_script::get_wiki_init_script(&wiki_path, &label, false))
        .devtools(cfg!(debug_assertions)); // Only enable in debug builds

    // Apply isolated session if available (shares with parent wiki)
    if let Some(dir) = session_dir {
        builder = builder.data_directory(dir);
    }

    // Set window position if specified
    if let (Some(x), Some(y)) = (left, top) {
        builder = builder.position(x, y);
    }

    // Tauri's drag/drop handler intercepts drops before WebKit/DOM gets them.
    // On Windows/macOS, we disable it and use custom handlers.
    // On Linux, we're testing if vanilla WebKitGTK handles drops like Epiphany.
    #[cfg(not(target_os = "linux"))]
    {
        builder = builder.disable_drag_drop_handler();
    }

    let window = builder
        .build()
        .map_err(|e| format!("Failed to create tiddler window: {}", e))?;

    // Note: Drag handlers are set up via the drag_drop plugin's on_webview_ready hook

    // Linux: Set up HeaderBar and center window (tiddler windows don't save state)
    #[cfg(target_os = "linux")]
    {
        setup_header_bar(&window);
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
#[tauri::command]
fn ipc_is_tiddler_window(state: tauri::State<WikiModeState>) -> bool {
    state.is_tiddler_window
}

/// IPC command: Get the tiddler title if this is a tiddler window
#[tauri::command]
fn ipc_get_tiddler_title(state: tauri::State<WikiModeState>) -> Option<String> {
    state.tiddler_title.clone()
}

/// IPC command: Request sync from source wiki (for tiddler windows)
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

/// IPC command: Update wiki favicon (sends to main process via IPC)
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

/// Get the resource directory, preferring paths relative to executable for tarball installs
/// This avoids baked-in CI paths like /home/runner/...
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

/// Get the base data directory, respecting portable mode
/// Checks for portable marker files in exe directory on all platforms
/// Falls back to app_data_dir for installed mode
fn get_data_dir(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            // Check for portable marker files
            if exe_dir.join("portable").exists() || exe_dir.join("portable.txt").exists() {
                return Some(exe_dir.to_path_buf());
            }
            // Check if portable data file already exists (user chose portable mode previously)
            if exe_dir.join("tiddlydesktop.html").exists() {
                return Some(exe_dir.to_path_buf());
            }
        }
    }

    // Default: use app data directory (installed mode)
    app.path().app_data_dir().ok()
}

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
    if let Some(data_dir) = get_data_dir(app) {
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
            let label = urlencoding::decode(parts[0]).unwrap_or_default().to_string();
            let title = urlencoding::decode(parts[1]).unwrap_or_default().to_string();

            // Update window title
            let app_clone = app.clone();
            let app_inner = app_clone.clone();
            let _ = app_clone.run_on_main_thread(move || {
                if let Some(window) = app_inner.get_webview_window(&label) {
                    let _ = window.set_title(&title);
                }
            });
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
        let wiki_path = match utils::base64_url_decode(path_key) {
            Some(decoded) => {
                // Security: Validate the wiki path before saving
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
            }
            None => {
                return Response::builder()
                    .status(400)
                    .body("Invalid path".as_bytes().to_vec())
                    .unwrap();
            }
        };

        let content = String::from_utf8_lossy(request.body()).to_string();

        // Check if backups should be created for this wiki
        let state = app.state::<AppState>();
        let wiki_path_str = wiki_path.to_string_lossy();
        let should_backup = should_create_backup(app, &state, wiki_path_str.as_ref());

        // Create backup if appropriate (synchronous since protocol handlers can't be async)
        if should_backup && wiki_path.exists() {
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
            }
        }

        // Write to temp file then rename for atomic operation
        let temp_path = wiki_path.with_extension("tmp");
        match std::fs::write(&temp_path, &content) {
            Ok(_) => {
                match std::fs::rename(&temp_path, &wiki_path) {
                    Ok(_) => {
                        return Response::builder()
                            .status(200)
                            .header("Access-Control-Allow-Origin", "*")
                            .body(Vec::new())
                            .unwrap();
                    }
                    Err(_rename_err) => {
                        // On Windows, rename can fail if file is locked
                        // Fall back to direct write after removing temp file
                        let _ = std::fs::remove_file(&temp_path);
                        match std::fs::write(&wiki_path, &content) {
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
                }
            }
            Err(e) => {
                return Response::builder()
                    .status(500)
                    .body(format!("Failed to write: {}", e).into_bytes())
                    .unwrap();
            }
        }
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

                    // Serve the file
                    match std::fs::read(&resolved_path) {
                        Ok(content) => {
                            let mime_type = utils::get_mime_type(&resolved_path);
                            return Response::builder()
                                .status(200)
                                .header("Content-Type", mime_type)
                                .header("Access-Control-Allow-Origin", "*")
                                .body(content)
                                .unwrap();
                        }
                        Err(e) => {
                            return Response::builder()
                                .status(404)
                                .header("Access-Control-Allow-Origin", "*")
                                .body(format!("File not found: {} ({})", resolved_path.display(), e).as_bytes().to_vec())
                                .unwrap();
                        }
                    }
                }
            }
        }
    };

    // Note: window_label and is_main_wiki are set by initialization_script(), not needed here
    drop(paths); // Release the lock before file I/O

    // Generate the save URL for this wiki
    let save_url = format!("wikifile://localhost/save/{}", path);

    // Prepare single-tiddler mode params for injection
    let single_tiddler_js = single_tiddler.as_ref()
        .map(|t| format!(r#"window.__SINGLE_TIDDLER__ = "{}";"#, t.replace('\\', "\\\\").replace('"', "\\\"")))
        .unwrap_or_default();
    let single_template_js = single_template.as_ref()
        .map(|t| format!(r#"window.__SINGLE_TEMPLATE__ = "{}";"#, t.replace('\\', "\\\\").replace('"', "\\\"")))
        .unwrap_or_default();
    let parent_window_js = parent_window.as_ref()
        .map(|p| format!(r#"window.__PARENT_WINDOW__ = "{}";"#, p.replace('\\', "\\\\").replace('"', "\\\"")))
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

    // Read file content
    let read_result = std::fs::read_to_string(&file_path);

    match read_result {
        Ok(content) => {
            // Inject saver and additional functionality for TiddlyWiki
            // Note: __WIKI_PATH__, __WINDOW_LABEL__, __IS_MAIN_WIKI__ are already set by initialization_script()

            // For single-tiddler windows, inject preload tiddlers to use single-tiddler layout
            // This must run BEFORE TiddlyWiki's boot.js to configure the layout
            let single_tiddler_preload = if let Some(ref tiddler) = single_tiddler {
                let template = single_template.as_deref()
                    .unwrap_or("$:/core/templates/single.tiddler.window");
                let escaped_tiddler = tiddler.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n");
                let escaped_template = template.replace('\\', "\\\\").replace('"', "\\\"");
                format!(r##"<script>
// TiddlyDesktop: Configure single-tiddler layout BEFORE boot
(function() {{
    window.$tw = window.$tw || {{}};
    $tw.preloadTiddlers = $tw.preloadTiddlers || [];

    // Set layout to use single-tiddler wrapper
    $tw.preloadTiddlers.push({{
        title: "$:/layout",
        text: "$:/TiddlyDesktop/SingleTiddlerLayout"
    }});

    // Inject a custom wrapper template that sets currentTiddler
    $tw.preloadTiddlers.push({{
        title: "$:/TiddlyDesktop/SingleTiddlerLayout",
        text: '<$set name="currentTiddler" value="{escaped_tiddler}"><$transclude tiddler="{escaped_template}" mode="block"/></$set>'
    }});

    // Store the tiddler title for reference
    window.__SINGLE_TIDDLER_TITLE__ = "{escaped_tiddler}";
}})();
</script>"##, escaped_tiddler=escaped_tiddler, escaped_template=escaped_template)
            } else {
                String::new()
            };

            let script_injection = format!(
                r##"{single_tiddler_preload}
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
    // Check if user wants to use a different saver (GitHub, GitLab, Gitea, etc.)
    function shouldUseLocalSaver(wiki) {{
        // Check if local saving is explicitly disabled
        var localSaverEnabled = wiki.getTiddlerText('$:/config/TiddlyDesktop/LocalSaver', 'yes');
        if (localSaverEnabled === 'no') {{
            return false;
        }}
        // Check if GitHub saver is configured (username + repo = user wants to use it)
        // Token is stored in localStorage, but if username/repo are set, user intends to use GitHub
        var githubUser = wiki.getTiddlerText('$:/GitHub/Username', '');
        var githubRepo = wiki.getTiddlerText('$:/GitHub/Repo', '');
        if (githubUser && githubRepo) {{
            return false; // Let GitHub saver handle it
        }}
        // Check if GitLab saver is configured
        var gitlabUser = wiki.getTiddlerText('$:/GitLab/Username', '');
        var gitlabRepo = wiki.getTiddlerText('$:/GitLab/Repo', '');
        if (gitlabUser && gitlabRepo) {{
            return false; // Let GitLab saver handle it
        }}
        // Check if Gitea saver is configured
        var giteaUser = wiki.getTiddlerText('$:/Gitea/Username', '');
        var giteaRepo = wiki.getTiddlerText('$:/Gitea/Repo', '');
        if (giteaUser && giteaRepo) {{
            return false; // Let Gitea saver handle it
        }}
        return true;
    }}

    window.$TiddlyDesktopSaver = {{
        info: {{
            name: 'tiddlydesktop',
            priority: 5000,
            capabilities: ['save', 'autosave']
        }},
        canSave: function(wiki) {{
            return shouldUseLocalSaver(wiki);
        }},
        create: function(wiki) {{
            return {{
                wiki: wiki,
                info: {{
                    name: 'tiddlydesktop',
                    priority: 5000,
                    capabilities: ['save', 'autosave']
                }},
                canSave: function(wiki) {{
                    return shouldUseLocalSaver(wiki);
                }},
                save: function(text, method, callback) {{
                    var wikiPath = window.__WIKI_PATH__;

                    // Try Tauri IPC first (works reliably on all platforms)
                    if(window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke) {{
                        window.__TAURI__.core.invoke('save_wiki', {{
                            path: wikiPath,
                            content: text
                        }}).then(function() {{
                            callback(null);
                        }}).catch(function(err) {{
                            // IPC failed, try fetch as fallback
                            saveViaFetch(text, callback);
                        }});
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
    function registerWithTiddlyWiki() {{
        if(typeof $tw === 'undefined') {{
            setTimeout(registerWithTiddlyWiki, 50);
            return;
        }}

        // Register as a module if modules system exists
        if($tw.modules && $tw.modules.types) {{
            $tw.modules.types['saver'] = $tw.modules.types['saver'] || {{}};
            $tw.modules.types['saver']['$:/plugins/tiddlydesktop/saver'] = window.$TiddlyDesktopSaver;
            console.log('TiddlyDesktop saver: registered as module');
        }}

        // Wait for saverHandler and add directly
        function addToSaverHandler() {{
            if(!$tw.saverHandler) {{
                setTimeout(addToSaverHandler, 50);
                return;
            }}

            // Check if already added
            var alreadyAdded = $tw.saverHandler.savers.some(function(s) {{
                return s.info && s.info.name === 'tiddlydesktop';
            }});

            if(!alreadyAdded) {{
                var saver = window.$TiddlyDesktopSaver.create($tw.wiki);
                // Add to array and re-sort (TiddlyWiki iterates backwards, so highest priority must be at the END)
                $tw.saverHandler.savers.push(saver);
                $tw.saverHandler.savers.sort(function(a, b) {{
                    if(a.info.priority < b.info.priority) {{
                        return -1;
                    }} else if(a.info.priority > b.info.priority) {{
                        return 1;
                    }}
                    return 0;
                }});
            }}
        }}

        addToSaverHandler();
    }}

    registerWithTiddlyWiki();

    // Title sync - mirror document.title to native window titlebar (GtkHeaderBar on Linux)
    // Uses MutationObserver on <title> element, like original TiddlyDesktop
    // TiddlyWiki5's render.js updates document.title from $:/core/wiki/title template
    (function() {{
        var windowLabel = window.__WINDOW_LABEL__;
        var lastTitle = '';

        function syncTitle() {{
            var title = document.title || '';

            // Skip if title hasn't changed or is empty/generic
            if (!title || title === lastTitle || title === 'Loading...') {{
                return;
            }}

            lastTitle = title;

            // Update native window titlebar (HeaderBar on Linux) via Tauri
            if (window.__TAURI__ && window.__TAURI__.core) {{
                window.__TAURI__.core.invoke('set_window_title', {{
                    label: windowLabel,
                    title: title
                }}).catch(function(e) {{
                    console.error('TiddlyDesktop: Failed to set window title:', e);
                }});
            }}
        }}

        // Set up MutationObserver on <title> element
        function setupTitleObserver() {{
            var titleElement = document.querySelector('title');
            if (!titleElement) {{
                // Title element not in DOM yet, retry
                setTimeout(setupTitleObserver, 100);
                return;
            }}

            // Initial sync
            syncTitle();

            // Observe changes to the title element (like original TiddlyDesktop)
            var observer = new MutationObserver(function() {{
                syncTitle();
            }});

            observer.observe(titleElement, {{
                childList: true,      // Text node added/removed
                characterData: true,  // Text content changes
                subtree: true         // Descendants (the text node)
            }});
        }}

        setupTitleObserver();
    }})();

    // Favicon sync - extract from $:/favicon.ico and update landing page
    // Also watches for changes so favicon updates are reflected instantly
    (function() {{
        var wikiPath = window.__WIKI_PATH__;
        var lastFavicon = '';

        function sendFaviconUpdate(dataUri) {{
            // Skip if favicon hasn't changed
            if (dataUri === lastFavicon) {{
                return;
            }}
            lastFavicon = dataUri;

            // Send to Rust to update the wiki list entry and window icon
            if (window.__TAURI__ && window.__TAURI__.core) {{
                // Update window icon (titlebar/taskbar)
                window.__TAURI__.core.invoke('set_window_icon', {{
                    label: window.__WINDOW_LABEL__,
                    faviconDataUri: dataUri
                }}).catch(function(err) {{
                    console.error('TiddlyDesktop: Failed to set window icon:', err);
                }});

                // Update wiki list entry favicon
                // In main wiki mode (main process), use update_wiki_favicon directly
                // In wiki mode (child process), use IPC to send to main process
                if (window.__IS_MAIN_WIKI__) {{
                    // Main process - direct command
                    window.__TAURI__.core.invoke('update_wiki_favicon', {{
                        path: wikiPath,
                        favicon: dataUri
                    }}).catch(function(err) {{
                        console.error('TiddlyDesktop: Failed to update favicon:', err);
                    }});
                }} else {{
                    // Wiki child process - use IPC
                    window.__TAURI__.core.invoke('ipc_update_favicon', {{
                        favicon: dataUri
                    }}).catch(function(err) {{
                        console.error('TiddlyDesktop: Failed to update favicon via IPC:', err);
                    }});
                }}
            }}
        }}

        function extractAndUpdateFavicon() {{
            if (typeof $tw === 'undefined' || !$tw.wiki) {{
                return; // TiddlyWiki not ready
            }}

            // Get the favicon tiddler
            var faviconTiddler = $tw.wiki.getTiddler('$:/favicon.ico');
            if (!faviconTiddler || !faviconTiddler.fields.text) {{
                return; // No favicon tiddler
            }}

            var text = faviconTiddler.fields.text;
            var type = faviconTiddler.fields.type || 'image/x-icon';

            // Build data URI
            var dataUri;
            if (text.startsWith('data:')) {{
                dataUri = text; // Already a data URI
            }} else {{
                // Assume base64 encoded
                dataUri = 'data:' + type + ';base64,' + text;
            }}

            sendFaviconUpdate(dataUri);
        }}

        function setupFaviconSync() {{
            if (typeof $tw === 'undefined' || !$tw.wiki || !$tw.wiki.addEventListener) {{
                setTimeout(setupFaviconSync, 100);
                return;
            }}

            // Initial extraction
            extractAndUpdateFavicon();

            // Watch for changes to $:/favicon.ico
            $tw.wiki.addEventListener('change', function(changes) {{
                if (changes['$:/favicon.ico']) {{
                    extractAndUpdateFavicon();
                }}
            }});
        }}

        setupFaviconSync();
    }})();

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
            .initialization_script(&init_script::get_wiki_init_script_with_language(&main_wiki_path.to_string_lossy(), "main", true, Some(&language)));

        // Apply saved position if available, with monitor validation
        if let Some(ref state) = saved_state {
            let (x, y) = validate_window_position(app_handle, state);
            builder = builder.position(x, y);
        }

        // Tauri's drag/drop handler intercepts drops before WebKit/DOM gets them.
        // On Windows/macOS, we disable it. On Linux, testing vanilla WebKitGTK.
        #[cfg(target_os = "windows")]
        {
            builder = builder.disable_drag_drop_handler();
        }

        if let Ok(main_window) = builder.build()
        {
            // Note: Drag handlers are set up via the drag_drop plugin's on_webview_ready hook

            // Linux: Set up HeaderBar and finalize window state (centering, unmaximize workaround)
            #[cfg(target_os = "linux")]
            {
                setup_header_bar(&main_window);
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
        .tooltip("TiddlyDesktop")
        .on_menu_event(|app, event| {
            match event.id().as_ref() {
                "show_window" => {
                    reveal_or_create_main_window(app);
                }
                "quit" => {
                    // Check if wikis are open before quitting
                    let state = app.state::<AppState>();
                    let wiki_count = state.wiki_processes.lock().unwrap().len();
                    if wiki_count > 0 {
                        eprintln!("[TiddlyDesktop] Quit requested with {} wiki(s) open - closing all", wiki_count);
                        // Clear wiki processes so ExitRequested handler allows exit
                        state.wiki_processes.lock().unwrap().clear();
                    }
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
struct WikiModeArgs {
    wiki_path: PathBuf,
    tiddler_title: Option<String>,
    startup_tiddler: Option<String>,
}

/// Arguments for wiki folder mode (Node.js server in separate process)
struct WikiFolderModeArgs {
    folder_path: PathBuf,
    port: u16,
}

/// Parse command-line arguments for special modes
enum SpecialModeArgs {
    WikiFile(WikiModeArgs),
    WikiFolder(WikiFolderModeArgs),
}

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
fn run_wiki_mode(args: WikiModeArgs) {
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

    tauri::Builder::default()
        .plugin(drag_drop::init_plugin())
        .setup(move |app| {
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
                main_wiki_path: wiki_path_clone.clone(), // Use wiki path as "main" for this process
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
                .devtools(cfg!(debug_assertions)); // Only enable in debug builds

            // Apply saved position if available, with monitor validation on Windows/macOS
            if let Some(ref state) = saved_state {
                let (x, y) = validate_window_position(app.handle(), state);
                builder = builder.position(x, y);
            }

            // Tauri's drag/drop handler intercepts drops before WebKit/DOM gets them.
            // On Windows, we disable it. On Linux, testing vanilla WebKitGTK.
            #[cfg(target_os = "windows")]
            {
                builder = builder.disable_drag_drop_handler();
            }

            let window = builder.build()?;

            // Note: Drag handlers are set up via the drag_drop plugin's on_webview_ready hook

            // Linux: Set up HeaderBar and finalize window state (centering, unmaximize workaround)
            #[cfg(target_os = "linux")]
            {
                setup_header_bar(&window);
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
                                _ => {}
                            }
                        });
                    });
                    eprintln!("[TiddlyDesktop] IPC listener thread started");
                }
            }
            drop(client_guard);

            Ok(())
        })
        .register_uri_scheme_protocol("wikifile", |ctx, request| {
            wiki_protocol_handler(ctx.app_handle(), request)
        })
        .plugin(tauri_plugin_opener::init())
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
            pick_files_for_import,
            wiki_storage::get_external_attachments_config,
            wiki_storage::set_external_attachments_config,
            wiki_storage::js_log,
            clipboard::get_clipboard_content,
            clipboard::set_clipboard_content,
            run_command,
            // Drag-drop commands
            start_native_drag,
            prepare_native_drag,
            cleanup_native_drag,
            get_pending_drag_data,
            update_drag_icon,
            set_pending_drag_icon,
            set_drag_dest_enabled,
            ungrab_seat_for_focus,
            // Tiddler window commands (same process, shares $tw.wiki)
            open_tiddler_window,
            close_window_by_label,
            toggle_fullscreen,
            print_page,
            download_file,
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
            show_find_in_page
        ])
        .build(tauri::generate_context!())
        .expect("error while building wiki-mode application")
        .run(|_app, _event| {
            // Wiki mode doesn't need special event handling
        });
}

/// Run in wiki-folder mode - a Node.js TiddlyWiki server in its own process
/// This is called when the app is started with --wiki-folder <path> --port <port>
fn run_wiki_folder_mode(args: WikiFolderModeArgs) {
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
            dir.join("..").join("Resources").join("tiddlywiki").join("tiddlywiki.js"),
        ];
        candidates.into_iter().find(|p| p.exists())
    });

    let tw_path = match tw_path {
        Some(p) => p,
        None => {
            eprintln!("[TiddlyDesktop] Error: TiddlyWiki not found in resources");
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
    }

    let folder_path_for_state = folder_path.clone();
    let folder_name_for_state = folder_name.clone();
    let ipc_client_for_state = ipc_client.clone();

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
        .plugin(drag_drop::init_plugin())
        .setup(move |app| {
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
            .devtools(cfg!(debug_assertions)); // Only enable in debug builds

            // Apply saved position, with monitor validation on Windows/macOS
            if let Some(ref state) = saved_state {
                let (x, y) = validate_window_position(app.handle(), state);
                builder = builder.position(x, y);
            }

            let window = builder.build()?;

            // Note: Drag handlers are set up via the drag_drop plugin's on_webview_ready hook

            // Linux: Set up HeaderBar and finalize window state (centering, unmaximize workaround)
            #[cfg(target_os = "linux")]
            {
                setup_header_bar(&window);
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
                main_wiki_path: folder_path_for_state.clone(),
            });

            // Start IPC listener for focus requests
            let client_guard = ipc_client_for_state.lock().unwrap();
            if let Some(ref client) = *client_guard {
                if let Some(listener_stream) = client.get_listener_stream() {
                    let app_handle = app.handle().clone();
                    std::thread::spawn(move || {
                        ipc::run_listener(listener_stream, |msg| {
                            if let ipc::IpcMessage::FocusWiki { .. } = msg {
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
        .invoke_handler(tauri::generate_handler![
            load_wiki,
            save_wiki,
            set_window_title,
            set_window_icon,
            set_headerbar_colors,
            get_window_label,
            show_alert,
            show_confirm,
            close_window,
            toggle_fullscreen,
            print_page,
            download_file,
            is_directory,
            get_window_state_info,
            get_saved_window_state,
            wiki_storage::save_window_state,
            wiki_storage::js_log,
            clipboard::get_clipboard_content,
            clipboard::set_clipboard_content,
            show_find_in_page,
        ])
        .build(tauri::generate_context!())
        .expect("error while building wiki-folder-mode application")
        .run(|_app, _event| {});
}

pub fn run() {
    // Check if we're running in a special mode (wiki file or wiki folder)
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

        if let Err(e) = server.start() {
            eprintln!("[TiddlyDesktop] IPC server error: {}", e);
        }
    });

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

    tauri::Builder::default()
        .plugin(drag_drop::init_plugin())
        .setup(|app| {
            // Store global AppHandle for IPC callbacks
            let _ = GLOBAL_APP_HANDLE.set(app.handle().clone());

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
            });

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
            let icon = Image::from_bytes(include_bytes!("../icons/icon.png"))?;
            #[allow(unused_mut)]
            let mut builder = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(wiki_url.parse().unwrap()))
                .title("TiddlyDesktopRS")
                .inner_size(win_width, win_height)
                .icon(icon)?
                .window_classname("tiddlydesktop-rs")
                .initialization_script(&init_script::get_wiki_init_script_with_language(&main_wiki_path.to_string_lossy(), "main", true, Some(&language)))
                .devtools(cfg!(debug_assertions)); // Only enable in debug builds

            // Apply saved position if available, with monitor validation on Windows/macOS
            if let Some(ref state) = saved_state {
                let (x, y) = validate_window_position(app.handle(), state);
                builder = builder.position(x, y);
            }

            // Tauri's drag/drop handler intercepts drops before WebKit/DOM gets them.
            // On Windows, we disable it. On Linux, testing vanilla WebKitGTK.
            #[cfg(target_os = "windows")]
            {
                builder = builder.disable_drag_drop_handler();
            }

            let main_window = builder.build()?;

            // Note: Drag handlers are set up via the drag_drop plugin's on_webview_ready hook

            // Linux: Set up HeaderBar and finalize window state (centering, unmaximize workaround)
            #[cfg(target_os = "linux")]
            {
                setup_header_bar(&main_window);
                linux_finalize_window_state(&main_window, &saved_state);
            }

            // Restore maximized state (Windows/macOS only - Linux handled in linux_finalize_window_state)
            #[cfg(not(target_os = "linux"))]
            if saved_state.as_ref().map(|s| s.maximized).unwrap_or(false) {
                let _ = main_window.maximize();
            }

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
                                if let Ok(entry) = open_wiki_window(app_handle.clone(), path_str).await {
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
        .plugin(tauri_plugin_opener::init())
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
            wiki_storage::update_wiki_favicon,
            wiki_storage::get_wiki_backup_dir_setting,
            wiki_storage::set_wiki_group,
            wiki_storage::get_wiki_groups,
            wiki_storage::rename_wiki_group,
            wiki_storage::delete_wiki_group,
            read_file_as_data_uri,
            read_file_as_binary,
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
            open_auth_window,
            run_command,
            show_find_in_page,
            toggle_fullscreen,
            print_page,
            download_file,
            wiki_storage::js_log,
            clipboard::get_clipboard_content,
            clipboard::set_clipboard_content,
            start_native_drag,
            prepare_native_drag,
            cleanup_native_drag,
            get_pending_drag_data,
            update_drag_icon,
            set_pending_drag_icon,
            set_drag_dest_enabled,
            ungrab_seat_for_focus
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
                                        if let Ok(entry) = open_wiki_window(app_handle.clone(), path_str).await {
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
