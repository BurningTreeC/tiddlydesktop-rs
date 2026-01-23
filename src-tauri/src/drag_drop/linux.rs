//! Linux drag-drop handling using GTK3 signals for content extraction
//!
//! WebKitGTK's native drag-drop handling doesn't reliably expose content (text, HTML, URLs)
//! from external apps to JavaScript. We use GTK3 drag signals to:
//! 1. Extract content from the drag selection data
//! 2. Emit td-drag-* events to JavaScript
//! 3. Let JavaScript create synthetic DOM events for TiddlyWiki
//!
//! Internal drags (within the webview) are handled by JavaScript:
//! - internal_drag.js intercepts dragstart for draggable elements and text selections
//! - td-drag-* handlers check TD.isInternalDragActive() and skip if true
//! - internal_drag.js creates synthetic drag events using mouse tracking
//!
//! Thread safety: GTK must only be used from the main thread. We use
//! glib::MainContext::default().invoke() to schedule GTK operations on the main thread
//! when called from other threads.

#![cfg(target_os = "linux")]

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gdk::DragAction;
use gtk::prelude::*;
use gtk::{DestDefaults, TargetEntry, TargetFlags};
use tauri::{Emitter, Manager, WebviewWindow};

use super::encoding::decode_string;

/// Data captured from a drag operation
#[derive(Clone, Debug, serde::Serialize)]
pub struct DragContentData {
    pub types: Vec<String>,
    pub data: HashMap<String, String>,
}

/// State for tracking drag operations
struct DragState {
    window: WebviewWindow,
    drag_active: bool,
    /// Set to true when drag-drop signal fires (user released mouse button)
    drop_requested: bool,
    /// Set to true while processing drop data
    drop_in_progress: bool,
    last_position: Option<(i32, i32)>,
}

/// Set up drag-drop handling for a webview window
/// This schedules the setup on the GTK main thread to avoid thread safety issues
pub fn setup_drag_handlers(window: &WebviewWindow) {
    let label = window.label().to_string();
    eprintln!(
        "[TiddlyDesktop] Linux: setup_drag_handlers called for window '{}'",
        label
    );

    // Check if we're on the GTK main thread
    let main_context = glib::MainContext::default();
    let is_main_thread = main_context.is_owner();

    if is_main_thread {
        // We're on the main thread, set up directly
        eprintln!(
            "[TiddlyDesktop] Linux: On main thread, setting up directly for '{}'",
            label
        );
        if let Ok(gtk_window) = window.gtk_window() {
            setup_gtk_drag_handlers(&gtk_window, window.clone());
        } else {
            eprintln!("[TiddlyDesktop] Linux: Failed to get GTK window for '{}'", label);
        }
    } else {
        // We're not on the main thread, schedule setup via glib main context
        // We need to pass Send-safe data and get the window on the main thread
        eprintln!(
            "[TiddlyDesktop] Linux: Not on main thread, scheduling for '{}'",
            label
        );

        let app_handle = window.app_handle().clone();
        let label_clone = label.clone();

        // Use invoke() which can be called from any thread
        main_context.invoke(move || {
            eprintln!(
                "[TiddlyDesktop] Linux: Running setup on main thread for '{}'",
                label_clone
            );

            // Get the window from the app handle
            if let Some(window) = app_handle.get_webview_window(&label_clone) {
                if let Ok(gtk_window) = window.gtk_window() {
                    setup_gtk_drag_handlers(&gtk_window, window);
                } else {
                    eprintln!(
                        "[TiddlyDesktop] Linux: Failed to get GTK window for '{}' (from invoke)",
                        label_clone
                    );
                }
            } else {
                eprintln!(
                    "[TiddlyDesktop] Linux: Window '{}' not found (from invoke)",
                    label_clone
                );
            }
        });
    }
}

fn setup_gtk_drag_handlers(gtk_window: &gtk::ApplicationWindow, window: WebviewWindow) {
    let label = window.label().to_string();
    eprintln!(
        "[TiddlyDesktop] Linux: Setting up GTK drag handlers for '{}'",
        label
    );

    let state = Rc::new(RefCell::new(DragState {
        window: window.clone(),
        drag_active: false,
        drop_requested: false,
        drop_in_progress: false,
        last_position: None,
    }));

    // Set up handlers on the GTK window itself to intercept before WebKitGTK
    // This gives us first crack at the drag events
    setup_widget_drag_handlers(gtk_window.upcast_ref::<gtk::Widget>(), state.clone(), &label);

    // Also try to find and set up on WebKit widget for redundancy
    if let Some(webview_widget) = find_webkit_widget(gtk_window) {
        let widget_type = webview_widget.type_().name();
        eprintln!(
            "[TiddlyDesktop] Linux: Also setting up handlers on WebKit widget: {}",
            widget_type
        );
        setup_webkit_drag_handlers(&webview_widget, state);
    }
}

/// Find the WebKitWebView widget in the widget hierarchy
fn find_webkit_widget(container: &impl IsA<gtk::Widget>) -> Option<gtk::Widget> {
    let widget = container.upcast_ref::<gtk::Widget>();
    let widget_type = widget.type_().name();

    if widget_type.contains("WebKit") || widget_type.contains("webview") {
        return Some(widget.clone());
    }

    if let Some(container) = widget.downcast_ref::<gtk::Container>() {
        for child in container.children() {
            if let Some(found) = find_webkit_widget(&child) {
                return Some(found);
            }
        }
    }

    None
}

/// Set up drag handlers on the window widget
fn setup_widget_drag_handlers(widget: &gtk::Widget, state: Rc<RefCell<DragState>>, label: &str) {
    let widget_type = widget.type_().name();
    eprintln!(
        "[TiddlyDesktop] Linux: Setting up drag handlers on widget type: {} for window '{}'",
        widget_type, label
    );

    // Define target types we accept - include both OTHER_APP and SAME_WIDGET
    let targets = vec![
        TargetEntry::new("text/plain", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 0),
        TargetEntry::new("text/html", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 1),
        TargetEntry::new("text/uri-list", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 2),
        TargetEntry::new("STRING", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 3),
        TargetEntry::new("UTF8_STRING", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 4),
        TargetEntry::new("TEXT", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 5),
    ];

    // Set up the widget as a drop destination
    // Use DestDefaults::MOTION | DestDefaults::HIGHLIGHT to handle motion/highlighting
    // but not DROP, so we can handle the drop ourselves
    widget.drag_dest_set(
        DestDefaults::MOTION | DestDefaults::HIGHLIGHT | DestDefaults::DROP,
        &targets,
        DragAction::COPY | DragAction::MOVE | DragAction::LINK,
    );

    // Connect drag-motion signal
    let state_motion = state.clone();
    widget.connect_drag_motion(move |_widget, context, x, y, time| {
        let mut s = state_motion.borrow_mut();
        s.last_position = Some((x, y));

        if !s.drag_active {
            s.drag_active = true;
            eprintln!("[TiddlyDesktop] Linux: drag-motion enter at ({}, {})", x, y);
        }

        // Rate-limited logging
        static LAST_LOG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let last = LAST_LOG.load(std::sync::atomic::Ordering::Relaxed);
        if now - last > 500 {
            LAST_LOG.store(now, std::sync::atomic::Ordering::Relaxed);
            eprintln!("[TiddlyDesktop] Linux: drag-motion at ({}, {})", x, y);
        }

        // Emit td-drag-motion event
        let _ = s.window.emit(
            "td-drag-motion",
            serde_json::json!({
                "x": x,
                "y": y,
                "screenCoords": false
            }),
        );

        // Set the drag status to indicate we accept
        context.drag_status(DragAction::COPY, time);
        true
    });

    // Connect drag-leave signal
    let state_leave = state.clone();
    widget.connect_drag_leave(move |_widget, _context, _time| {
        let mut s = state_leave.borrow_mut();

        if s.drop_in_progress {
            return;
        }

        eprintln!("[TiddlyDesktop] Linux: drag-leave");
        s.drag_active = false;

        let _ = s.window.emit("td-drag-leave", ());
    });

    // Connect drag-drop signal to request data
    let state_drop_signal = state.clone();
    let widget_clone = widget.clone();
    widget.connect_drag_drop(move |_widget, context, x, y, time| {
        eprintln!("[TiddlyDesktop] Linux: drag-drop signal at ({}, {})", x, y);

        // Mark that a real drop was requested (user released mouse button)
        {
            let mut s = state_drop_signal.borrow_mut();
            s.drop_requested = true;
            s.last_position = Some((x, y));
        }

        // Request data for the drop - try text/html first, then text/plain
        let targets = context.list_targets();
        eprintln!(
            "[TiddlyDesktop] Linux: Available targets: {:?}",
            targets.iter().map(|a| a.name()).collect::<Vec<_>>()
        );

        // Find the best target to request
        let preferred_targets = ["text/html", "text/uri-list", "UTF8_STRING", "text/plain", "STRING"];
        let mut requested = false;

        for pref in &preferred_targets {
            for target in &targets {
                if target.name() == *pref {
                    eprintln!("[TiddlyDesktop] Linux: Requesting data for target: {}", pref);
                    widget_clone.drag_get_data(context, target, time);
                    requested = true;
                    break;
                }
            }
            if requested {
                break;
            }
        }

        if !requested && !targets.is_empty() {
            // Request the first available target
            eprintln!(
                "[TiddlyDesktop] Linux: Requesting data for first target: {}",
                targets[0].name()
            );
            widget_clone.drag_get_data(context, &targets[0], time);
        }

        true
    });

    // Connect drag-data-received signal (for the actual drop)
    let state_drop = state.clone();
    widget.connect_drag_data_received(
        move |_widget, context, x, y, selection_data, _info, time| {
            handle_drag_data_received(&state_drop, context, x, y, selection_data, time);
        },
    );

    eprintln!("[TiddlyDesktop] Linux: GTK3 drag-drop handlers connected on window");
}

/// Set up drag handlers on WebKit widget (overrides WebKitGTK's internal handling)
fn setup_webkit_drag_handlers(widget: &gtk::Widget, state: Rc<RefCell<DragState>>) {
    // Define target types we accept
    let targets = vec![
        TargetEntry::new("text/plain", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 0),
        TargetEntry::new("text/html", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 1),
        TargetEntry::new("text/uri-list", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 2),
        TargetEntry::new("STRING", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 3),
        TargetEntry::new("UTF8_STRING", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 4),
        TargetEntry::new("TEXT", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 5),
    ];

    // Override WebKitGTK's drag handling by setting our own destination
    widget.drag_dest_set(
        DestDefaults::MOTION | DestDefaults::HIGHLIGHT | DestDefaults::DROP,
        &targets,
        DragAction::COPY | DragAction::MOVE | DragAction::LINK,
    );

    // Connect drag-motion signal
    let state_motion = state.clone();
    widget.connect_drag_motion(move |widget, context, x, y, time| {
        // Check if this is an internal drag (source is same widget)
        // For internal drags, let WebKitGTK + TiddlyWiki handle them natively
        let is_internal = context.drag_get_source_widget()
            .map(|source| source == *widget)
            .unwrap_or(false);

        if is_internal {
            // Internal drag - don't intercept, let native TiddlyWiki handling work
            // Return false to let the event propagate
            return false;
        }

        let mut s = state_motion.borrow_mut();
        s.last_position = Some((x, y));

        if !s.drag_active {
            s.drag_active = true;
            eprintln!(
                "[TiddlyDesktop] Linux: WebKit drag-motion enter at ({}, {})",
                x, y
            );
        }

        // Rate-limited logging
        static LAST_LOG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let last = LAST_LOG.load(std::sync::atomic::Ordering::Relaxed);
        if now - last > 500 {
            LAST_LOG.store(now, std::sync::atomic::Ordering::Relaxed);
            eprintln!("[TiddlyDesktop] Linux: WebKit drag-motion at ({}, {})", x, y);
        }

        // Emit td-drag-motion event
        let _ = s.window.emit(
            "td-drag-motion",
            serde_json::json!({
                "x": x,
                "y": y,
                "screenCoords": false
            }),
        );

        context.drag_status(DragAction::COPY, time);
        true
    });

    // Connect drag-leave signal
    let state_leave = state.clone();
    widget.connect_drag_leave(move |widget, context, _time| {
        // Skip internal drags
        let is_internal = context.drag_get_source_widget()
            .map(|source| source == *widget)
            .unwrap_or(false);
        if is_internal {
            return;
        }

        let mut s = state_leave.borrow_mut();

        if s.drop_in_progress {
            return;
        }

        eprintln!("[TiddlyDesktop] Linux: WebKit drag-leave");
        s.drag_active = false;

        let _ = s.window.emit("td-drag-leave", ());
    });

    // Connect drag-drop signal
    let state_drop_signal = state.clone();
    let widget_clone = widget.clone();
    widget.connect_drag_drop(move |widget, context, x, y, time| {
        // Skip internal drags - let WebKitGTK + TiddlyWiki handle them
        let is_internal = context.drag_get_source_widget()
            .map(|source| source == *widget)
            .unwrap_or(false);

        if is_internal {
            eprintln!("[TiddlyDesktop] Linux: Internal drop - letting native handling work");
            return false;
        }

        eprintln!(
            "[TiddlyDesktop] Linux: WebKit drag-drop signal at ({}, {})",
            x, y
        );

        {
            let mut s = state_drop_signal.borrow_mut();
            s.drop_requested = true;
            s.last_position = Some((x, y));
        }

        // Request data
        let targets = context.list_targets();
        let preferred_targets = ["text/html", "text/uri-list", "UTF8_STRING", "text/plain", "STRING"];
        let mut requested = false;

        for pref in &preferred_targets {
            for target in &targets {
                if target.name() == *pref {
                    widget_clone.drag_get_data(context, target, time);
                    requested = true;
                    break;
                }
            }
            if requested {
                break;
            }
        }

        if !requested && !targets.is_empty() {
            widget_clone.drag_get_data(context, &targets[0], time);
        }

        true
    });

    // Connect drag-data-received signal
    let state_data = state.clone();
    widget.connect_drag_data_received(
        move |widget, context, x, y, selection_data, _info, time| {
            // Skip internal drags
            let is_internal = context.drag_get_source_widget()
                .map(|source| source == *widget)
                .unwrap_or(false);
            if is_internal {
                return;
            }
            handle_drag_data_received(&state_data, context, x, y, selection_data, time);
        },
    );

    eprintln!("[TiddlyDesktop] Linux: WebKit drag handlers set up");
}

/// Handle received drag data
fn handle_drag_data_received(
    state: &Rc<RefCell<DragState>>,
    context: &gdk::DragContext,
    x: i32,
    y: i32,
    selection_data: &gtk::SelectionData,
    time: u32,
) {
    let mut s = state.borrow_mut();

    // Only process as a drop if drag-drop signal has fired (user released mouse)
    // Otherwise this is just a preview/validation request
    if !s.drop_requested {
        eprintln!(
            "[TiddlyDesktop] Linux: drag-data-received (preview, ignoring) type: {}",
            selection_data.data_type().name()
        );
        return;
    }

    s.drop_requested = false; // Reset for next drop
    s.drop_in_progress = true;

    // If coordinates are (0, 0), try to get the current pointer position
    let (final_x, final_y) = if x == 0 && y == 0 {
        // Try to get pointer position from the display's default seat
        let fallback = s.last_position.unwrap_or((x, y));

        if let Some(display) = gdk::Display::default() {
            if let Some(seat) = display.default_seat() {
                if let Some(pointer) = seat.pointer() {
                    // Get the dest window to calculate relative coordinates
                    let dest_window = context.dest_window();
                    let (_screen, px, py) = pointer.position();
                    // Get the window origin to convert screen to window coords
                    let (_, wx, wy) = dest_window.origin();
                    let rel_x = px - wx;
                    let rel_y = py - wy;
                    eprintln!(
                        "[TiddlyDesktop] Linux: Got pointer position: screen({}, {}), window origin({}, {}), relative({}, {})",
                        px, py, wx, wy, rel_x, rel_y
                    );
                    (rel_x, rel_y)
                } else {
                    fallback
                }
            } else {
                fallback
            }
        } else {
            fallback
        }
    } else {
        (x, y)
    };

    s.last_position = Some((final_x, final_y));

    eprintln!("[TiddlyDesktop] Linux: drag-data-received at ({}, {}) [original: ({}, {})]", final_x, final_y, x, y);

    // Emit drop-start
    let _ = s.window.emit(
        "td-drag-drop-start",
        serde_json::json!({
            "x": final_x,
            "y": final_y,
            "screenCoords": false
        }),
    );

    // Try to extract content from selection data
    let mut types = Vec::new();
    let mut data = HashMap::new();

    // Get the data type that was received
    let data_type = selection_data.data_type().name();
    eprintln!("[TiddlyDesktop] Linux: Received data type: {}", data_type);

    // Get raw data first for debugging and proper encoding detection
    let raw_data = selection_data.data();
    if !raw_data.is_empty() {
        eprintln!(
            "[TiddlyDesktop] Linux: Raw data size: {} bytes, first 100 bytes: {:?}",
            raw_data.len(),
            &raw_data[..std::cmp::min(100, raw_data.len())]
        );
    }

    // Try to decode raw data with proper encoding detection
    let text = if !raw_data.is_empty() {
        let decoded = decode_string(&raw_data);
        if !decoded.is_empty() && !decoded.contains('\u{FFFD}') {
            eprintln!(
                "[TiddlyDesktop] Linux: Decoded raw data: {} chars",
                decoded.len()
            );
            Some(decoded)
        } else {
            selection_data.text().map(|t| t.to_string())
        }
    } else {
        selection_data.text().map(|t| t.to_string())
    };

    if let Some(text) = text {
        eprintln!(
            "[TiddlyDesktop] Linux: Got text content: {} chars, preview: {:?}",
            text.len(),
            &text[..std::cmp::min(200, text.len())]
        );

        // Check if it's a file URI list
        if text.starts_with("file://") || data_type == "text/uri-list" {
            let paths: Vec<String> = text
                .lines()
                .filter(|line| !line.is_empty() && !line.starts_with('#'))
                .filter_map(|line| {
                    let uri = line.trim();
                    if uri.starts_with("file://") {
                        let path = uri.strip_prefix("file://").unwrap_or(uri);
                        urlencoding::decode(path).map(|p| p.into_owned()).ok()
                    } else {
                        None
                    }
                })
                .collect();

            if !paths.is_empty() {
                eprintln!(
                    "[TiddlyDesktop] Linux: File drop with {} paths",
                    paths.len()
                );

                let _ = s.window.emit(
                    "td-drag-drop-position",
                    serde_json::json!({
                        "x": final_x,
                        "y": final_y,
                        "screenCoords": false
                    }),
                );
                let _ = s.window.emit(
                    "td-file-drop",
                    serde_json::json!({
                        "paths": paths
                    }),
                );

                // Finish the drag
                context.drag_finish(true, false, time);
                s.drag_active = false;
                s.drop_in_progress = false;
                return;
            }
        }

        // Check if it looks like a URL
        if text.starts_with("http://") || text.starts_with("https://") {
            types.push("text/uri-list".to_string());
            data.insert("text/uri-list".to_string(), text.clone());
            types.push("URL".to_string());
            data.insert("URL".to_string(), text.clone());
        }

        // Check if it looks like HTML
        let trimmed = text.trim_start();
        if trimmed.starts_with('<') || trimmed.starts_with("&lt;") || data_type == "text/html" {
            types.push("text/html".to_string());
            data.insert("text/html".to_string(), text.clone());
        }

        // Always include as text/plain
        types.push("text/plain".to_string());
        data.insert("text/plain".to_string(), text);
    }

    let has_content = !types.is_empty();

    if has_content {
        eprintln!(
            "[TiddlyDesktop] Linux: Content drop with types: {:?}",
            types
        );

        let _ = s.window.emit(
            "td-drag-drop-position",
            serde_json::json!({
                "x": final_x,
                "y": final_y,
                "screenCoords": false
            }),
        );

        let content_data = DragContentData { types, data };
        let _ = s.window.emit("td-drag-content", &content_data);
    }

    // Finish the drag operation
    context.drag_finish(has_content, false, time);

    s.drag_active = false;
    s.drop_in_progress = false;
}
