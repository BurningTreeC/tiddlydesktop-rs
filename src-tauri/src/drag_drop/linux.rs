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
use std::sync::Mutex;
use std::sync::OnceLock;

use gdk::DragAction;
use gtk::prelude::*;
use gtk::{DestDefaults, TargetEntry, TargetFlags, TargetList};
use tauri::{Emitter, Manager, WebviewWindow};

/// Data to be provided during an outgoing drag operation
/// Matches MIME types used by TiddlyWiki5's drag-drop system
#[derive(Clone, Debug, Default)]
pub struct OutgoingDragData {
    pub text_plain: Option<String>,
    pub text_html: Option<String>,
    pub text_vnd_tiddler: Option<String>,
    pub text_uri_list: Option<String>,
    /// Mozilla URL format: data:text/vnd.tiddler,<url-encoded-json>
    pub text_x_moz_url: Option<String>,
    /// Standard URL type: data:text/vnd.tiddler,<url-encoded-json>
    pub url: Option<String>,
}

/// Outgoing drag data with source window identification
struct OutgoingDragState {
    data: OutgoingDragData,
    source_window_label: String,
    /// Set to true when drag-data-get is called (data was actually transferred)
    data_was_requested: bool,
}

/// Global storage for outgoing drag data (needed because GTK callbacks can't capture owned data easily)
fn outgoing_drag_state() -> &'static Mutex<Option<OutgoingDragState>> {
    static INSTANCE: OnceLock<Mutex<Option<OutgoingDragState>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(None))
}

/// Mark that data was requested for the current drag
fn mark_data_requested() {
    if let Ok(mut guard) = outgoing_drag_state().lock() {
        if let Some(state) = guard.as_mut() {
            state.data_was_requested = true;
        }
    }
}

/// Check if data was requested for the current drag
fn was_data_requested() -> bool {
    outgoing_drag_state()
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|state| state.data_was_requested))
        .unwrap_or(false)
}

/// Check if we have outgoing drag data for a specific window
fn has_outgoing_data_for_window(window_label: &str) -> bool {
    outgoing_drag_state()
        .lock()
        .map(|guard| {
            guard.as_ref()
                .map(|state| state.source_window_label == window_label)
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

use super::encoding::decode_string;
use super::sanitize::{sanitize_html, sanitize_uri_list, sanitize_file_paths, is_dangerous_url};

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

        // Set up outgoing drag handlers (drag SOURCE) for when we drag TO external apps
        setup_outgoing_drag_handlers(&webview_widget, window.clone());
    }
}

/// Set up handlers for outgoing drags (when we drag TO external applications)
fn setup_outgoing_drag_handlers(widget: &gtk::Widget, window: WebviewWindow) {
    eprintln!("[TiddlyDesktop] Linux: Setting up outgoing drag handlers");

    // Connect drag-data-get signal to provide data when requested by external apps
    widget.connect_drag_data_get(move |_widget, _context, selection_data, info, _time| {
        eprintln!(
            "[TiddlyDesktop] Linux: drag-data-get called, info: {}",
            info
        );

        // Mark that data was requested - this means an external app picked up the drag
        mark_data_requested();

        // Get the stored drag data (regardless of window - we're providing data for an active drag)
        let drag_data = if let Ok(guard) = outgoing_drag_state().lock() {
            guard.as_ref().map(|state| state.data.clone())
        } else {
            None
        };

        if let Some(data) = drag_data {
            let target_name = selection_data.target().name();
            eprintln!(
                "[TiddlyDesktop] Linux: Providing data for target: {}",
                target_name
            );

            // Provide data based on requested target
            // MIME type encoding requirements:
            // - text/x-moz-url: UTF-16LE (Mozilla specific)
            // - text/vnd.tiddler: UTF-8 (custom TiddlyWiki type)
            // - URL: UTF-8
            // - text/html: UTF-8
            // - text/uri-list: UTF-8 (ASCII subset per RFC 2483)
            // - text/plain, UTF8_STRING: UTF-8
            // - STRING: Latin-1, but GTK's set_text() handles conversion
            // - TEXT: Compound text, GTK handles conversion
            eprintln!(
                "[TiddlyDesktop] Linux: Requested target '{}', available: vnd.tiddler={}, moz_url={}, url={}, plain={}",
                target_name,
                data.text_vnd_tiddler.is_some(),
                data.text_x_moz_url.is_some(),
                data.url.is_some(),
                data.text_plain.is_some()
            );

            if target_name == "text/vnd.tiddler" {
                // TiddlyWiki custom type - UTF-8 JSON
                if let Some(tiddler) = data.text_vnd_tiddler.as_ref() {
                    eprintln!("[TiddlyDesktop] Linux: Providing text/vnd.tiddler ({} bytes UTF-8)", tiddler.len());
                    selection_data.set_text(tiddler);
                }
            } else if target_name == "text/x-moz-url" {
                // Mozilla URL format: URL\nTitle (two lines) - MUST be UTF-16LE encoded!
                if let Some(moz_url) = data.text_x_moz_url.as_ref() {
                    let title = data.text_plain.as_deref().unwrap_or("");
                    let full_moz_url = format!("{}\n{}", moz_url, title);

                    // Convert to UTF-16LE bytes (Mozilla's required format)
                    let utf16_bytes: Vec<u8> = full_moz_url
                        .encode_utf16()
                        .flat_map(|c| c.to_le_bytes())
                        .collect();

                    eprintln!("[TiddlyDesktop] Linux: Providing text/x-moz-url ({} chars -> {} bytes UTF-16LE)",
                        full_moz_url.len(), utf16_bytes.len());

                    let atom = gdk::Atom::intern("text/x-moz-url");
                    selection_data.set(&atom, 8, &utf16_bytes);
                }
            } else if target_name == "URL" {
                // Standard URL type - UTF-8 data URI
                if let Some(url) = data.url.as_ref() {
                    eprintln!("[TiddlyDesktop] Linux: Providing URL ({} bytes UTF-8)", url.len());
                    selection_data.set_text(url);
                }
            } else if target_name == "text/html" {
                // HTML content - UTF-8
                if let Some(html) = data.text_html.as_ref() {
                    eprintln!("[TiddlyDesktop] Linux: Providing text/html ({} bytes UTF-8)", html.len());
                    selection_data.set_text(html);
                }
            } else if target_name == "text/uri-list" {
                // URI list - UTF-8 (RFC 2483)
                // Prefer data URI for tiddler data, fall back to regular uri-list
                if let Some(data_uri) = data.url.as_ref() {
                    eprintln!("[TiddlyDesktop] Linux: Providing text/uri-list with data URI ({} bytes UTF-8)", data_uri.len());
                    selection_data.set_text(data_uri);
                } else if let Some(uris) = data.text_uri_list.as_ref() {
                    eprintln!("[TiddlyDesktop] Linux: Providing text/uri-list ({} bytes UTF-8)", uris.len());
                    let uri_list: Vec<&str> = uris.lines().collect();
                    let _ = selection_data.set_uris(&uri_list);
                }
            } else if target_name == "UTF8_STRING" || target_name == "text/plain" {
                // UTF-8 text - provide tiddler title or fall back to tiddler JSON
                if let Some(text) = data.text_plain.as_ref() {
                    eprintln!("[TiddlyDesktop] Linux: Providing {} ({} bytes UTF-8)", target_name, text.len());
                    selection_data.set_text(text);
                } else if let Some(tiddler) = data.text_vnd_tiddler.as_ref() {
                    eprintln!("[TiddlyDesktop] Linux: Providing {} (fallback tiddler: {} bytes UTF-8)", target_name, tiddler.len());
                    selection_data.set_text(tiddler);
                }
            } else if target_name == "STRING" || target_name == "TEXT" || target_name == "COMPOUND_TEXT" {
                // Legacy X11 text types - GTK's set_text() handles encoding conversion
                if let Some(text) = data.text_plain.as_ref() {
                    eprintln!("[TiddlyDesktop] Linux: Providing {} ({} bytes, GTK converts)", target_name, text.len());
                    selection_data.set_text(text);
                } else if let Some(tiddler) = data.text_vnd_tiddler.as_ref() {
                    eprintln!("[TiddlyDesktop] Linux: Providing {} (fallback: {} bytes, GTK converts)", target_name, tiddler.len());
                    selection_data.set_text(tiddler);
                }
            } else {
                eprintln!("[TiddlyDesktop] Linux: Unknown target '{}' - no data provided", target_name);
            }
        } else {
            eprintln!("[TiddlyDesktop] Linux: No outgoing drag data available");
        }
    });

    // Connect drag-end signal to notify JavaScript
    // NOTE: We do NOT clear outgoing_drag_data here because GTK may fire drag-end
    // immediately if there's no valid GDK event (e.g., when starting drag from JS).
    // The data is cleared by cleanup_native_drag() called from JavaScript.
    widget.connect_drag_end(move |_widget, _context| {
        let data_was_requested = was_data_requested();
        eprintln!("[TiddlyDesktop] Linux: Outgoing drag-end signal received, data_was_requested={}", data_was_requested);

        // Notify JavaScript that GTK thinks the drag ended
        // Include whether data was actually requested - if true, it's a real drop to external app
        // If false, GTK fired drag-end prematurely (no valid GDK event)
        #[derive(serde::Serialize, Clone)]
        struct DragEndPayload {
            /// True if drag-data-get was called (external app received the data)
            data_was_requested: bool,
        }
        let _ = window.emit("td-drag-end", DragEndPayload { data_was_requested });
    });

    eprintln!("[TiddlyDesktop] Linux: Outgoing drag handlers connected");
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
    // Custom data containers (may contain text/vnd.tiddler):
    // - Firefox: application/x-moz-custom-clipdata
    // - Chrome: chromium/x-web-custom-data
    let targets = vec![
        TargetEntry::new("application/x-moz-custom-clipdata", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 0),
        TargetEntry::new("chromium/x-web-custom-data", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 1),
        TargetEntry::new("text/vnd.tiddler", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 2),
        TargetEntry::new("application/json", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 3),
        TargetEntry::new("text/x-moz-url", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 4),
        TargetEntry::new("text/plain", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 5),
        TargetEntry::new("text/html", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 6),
        TargetEntry::new("text/uri-list", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 7),
        TargetEntry::new("STRING", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 8),
        TargetEntry::new("UTF8_STRING", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 9),
        TargetEntry::new("TEXT", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 10),
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
    let label_for_motion = label.to_string();
    widget.connect_drag_motion(move |_widget, context, x, y, time| {
        // Check if we have outgoing drag data FOR THIS WINDOW
        // If so, let WebKit handle the native drag events (return false to propagate)
        // GDK polling handles our own drag for position tracking
        let has_outgoing_data = has_outgoing_data_for_window(&label_for_motion);
        if has_outgoing_data {
            // Return false to let WebKit's native handlers process the drag
            // This allows TiddlyWiki's droppable widgets to respond
            eprintln!("[TiddlyDesktop] Linux: GtkWindow drag-motion for our own drag at ({}, {}) - letting WebKit handle it", x, y);
            context.drag_status(DragAction::COPY, time);
            return false;
        }

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

        // Emit td-drag-motion event for external drags only
        let _ = s.window.emit(
            "td-drag-motion",
            serde_json::json!({
                "x": x,
                "y": y,
                "screenCoords": false,
                "isOurDrag": false
            }),
        );

        // Set the drag status to indicate we accept
        context.drag_status(DragAction::COPY, time);
        true
    });

    // Connect drag-leave signal
    let state_leave = state.clone();
    let label_for_leave = label.to_string();
    widget.connect_drag_leave(move |_widget, _context, _time| {
        // Check if we have outgoing drag data FOR THIS WINDOW
        // If so, skip emitting - GDK polling handles our own drag exclusively
        let has_outgoing_data = has_outgoing_data_for_window(&label_for_leave);
        if has_outgoing_data {
            return;
        }

        let mut s = state_leave.borrow_mut();

        if s.drop_in_progress {
            return;
        }

        eprintln!("[TiddlyDesktop] Linux: drag-leave");
        s.drag_active = false;

        let _ = s.window.emit("td-drag-leave", serde_json::json!({
            "isOurDrag": false
        }));
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
        // Browser custom data containers that may contain text/vnd.tiddler:
        // - Firefox: application/x-moz-custom-clipdata
        // - Chrome: chromium/x-web-custom-data (Pickle format)
        let preferred_targets = ["application/x-moz-custom-clipdata", "chromium/x-web-custom-data", "text/vnd.tiddler", "application/json", "text/x-moz-url", "text/html", "text/uri-list", "UTF8_STRING", "text/plain", "STRING"];
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
    // Custom data containers (may contain text/vnd.tiddler):
    // - Firefox: application/x-moz-custom-clipdata
    // - Chrome: chromium/x-web-custom-data
    let targets = vec![
        TargetEntry::new("application/x-moz-custom-clipdata", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 0),
        TargetEntry::new("chromium/x-web-custom-data", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 1),
        TargetEntry::new("text/vnd.tiddler", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 2),
        TargetEntry::new("application/json", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 3),
        TargetEntry::new("text/x-moz-url", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 4),
        TargetEntry::new("text/plain", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 5),
        TargetEntry::new("text/html", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 6),
        TargetEntry::new("text/uri-list", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 7),
        TargetEntry::new("STRING", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 8),
        TargetEntry::new("UTF8_STRING", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 9),
        TargetEntry::new("TEXT", TargetFlags::OTHER_APP | TargetFlags::OTHER_WIDGET, 10),
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
        // Get window label first (need to borrow state)
        let window_label = {
            let s = state_motion.borrow();
            s.window.label().to_string()
        };

        // Check if this is an internal drag (source is same widget)
        let is_internal = context.drag_get_source_widget()
            .map(|source| source == *widget)
            .unwrap_or(false);

        // Check if we have outgoing drag data FOR THIS WINDOW (not just any window)
        let has_outgoing_data = has_outgoing_data_for_window(&window_label);

        if is_internal && !has_outgoing_data {
            // Internal drag without outgoing data for this window - let WebKitGTK + TiddlyWiki handle natively
            // Return false to let the event propagate
            return false;
        }

        // If we have outgoing data for this window, this is our drag re-entering
        // Return false to let WebKit's native handlers process the drag
        // GDK polling handles position tracking, but WebKit needs to see the drag
        // for TiddlyWiki's droppable widgets to respond
        if has_outgoing_data {
            eprintln!("[TiddlyDesktop] Linux: GTK drag-motion for our own drag at ({}, {}) - letting WebKit handle it", x, y);
            context.drag_status(DragAction::COPY, time);
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

        // Emit td-drag-motion event for external drags only
        let _ = s.window.emit(
            "td-drag-motion",
            serde_json::json!({
                "x": x,
                "y": y,
                "screenCoords": false,
                "isOurDrag": false
            }),
        );

        context.drag_status(DragAction::COPY, time);
        true
    });

    // Connect drag-leave signal
    let state_leave = state.clone();
    widget.connect_drag_leave(move |_widget, _context, _time| {
        // Get window label first
        let window_label = {
            let s = state_leave.borrow();
            s.window.label().to_string()
        };

        // Check if we have outgoing drag data FOR THIS WINDOW
        let has_outgoing_data = has_outgoing_data_for_window(&window_label);

        // If we have outgoing data for this window, this is our drag leaving
        // Don't emit td-drag-leave here - GDK polling handles our own drag exclusively
        if has_outgoing_data {
            eprintln!("[TiddlyDesktop] Linux: GTK drag-leave for our own drag - skipping (polling handles it)");
            return;
        }

        let mut s = state_leave.borrow_mut();

        if s.drop_in_progress {
            return;
        }

        eprintln!("[TiddlyDesktop] Linux: WebKit drag-leave for window '{}'", window_label);
        s.drag_active = false;

        // Emit for external drags only
        let _ = s.window.emit("td-drag-leave", serde_json::json!({
            "isOurDrag": false
        }));
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
        eprintln!(
            "[TiddlyDesktop] Linux: WebKit available targets: {:?}",
            targets.iter().map(|a| a.name()).collect::<Vec<_>>()
        );
        // application/x-moz-custom-clipdata may contain custom MIME types like text/vnd.tiddler
        // text/x-moz-url contains URL + title in Mozilla format
        let preferred_targets = ["application/x-moz-custom-clipdata", "text/vnd.tiddler", "application/json", "text/x-moz-url", "text/html", "text/uri-list", "UTF8_STRING", "text/plain", "STRING"];
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

/// Parse Mozilla's application/x-moz-custom-clipdata format
/// Format:
/// - 4 bytes big-endian: number of entries
/// - For each entry:
///   - 4 bytes big-endian: length of MIME type in bytes (UTF-16LE)
///   - MIME type as UTF-16LE
///   - 4 bytes big-endian: length of data in bytes (UTF-16LE)
///   - Data as UTF-16LE
fn parse_moz_custom_clipdata(data: &[u8]) -> Option<HashMap<String, String>> {
    if data.len() < 8 {
        return None;
    }

    let mut result = HashMap::new();
    let mut offset = 0;

    // Read number of entries (4 bytes big-endian)
    let num_entries = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    offset += 4;

    eprintln!(
        "[TiddlyDesktop] Linux: Mozilla clipdata: {} entries",
        num_entries
    );

    for i in 0..num_entries {
        if offset + 4 > data.len() {
            eprintln!(
                "[TiddlyDesktop] Linux: Mozilla clipdata: truncated at entry {} (mime type length)",
                i
            );
            break;
        }

        // Read MIME type length (4 bytes big-endian, in bytes)
        let mime_len = u32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        offset += 4;

        if offset + mime_len > data.len() {
            eprintln!(
                "[TiddlyDesktop] Linux: Mozilla clipdata: truncated at entry {} (mime type data)",
                i
            );
            break;
        }

        // Read MIME type as UTF-16LE
        let mime_bytes = &data[offset..offset + mime_len];
        let mime_type = decode_utf16le(mime_bytes);
        offset += mime_len;

        if offset + 4 > data.len() {
            eprintln!(
                "[TiddlyDesktop] Linux: Mozilla clipdata: truncated at entry {} (content length)",
                i
            );
            break;
        }

        // Read content length (4 bytes big-endian, in bytes)
        let content_len = u32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        offset += 4;

        if offset + content_len > data.len() {
            eprintln!(
                "[TiddlyDesktop] Linux: Mozilla clipdata: truncated at entry {} (content data, need {} have {})",
                i, content_len, data.len() - offset
            );
            // Try to read what we can
            let available = data.len() - offset;
            let content_bytes = &data[offset..offset + available];
            let content = decode_utf16le(content_bytes);
            if !mime_type.is_empty() && !content.is_empty() {
                result.insert(mime_type, content);
            }
            break;
        }

        // Read content as UTF-16LE
        let content_bytes = &data[offset..offset + content_len];
        let content = decode_utf16le(content_bytes);
        offset += content_len;

        eprintln!(
            "[TiddlyDesktop] Linux: Mozilla clipdata entry {}: {} = {} bytes -> {} chars",
            i,
            mime_type,
            content_len,
            content.len()
        );

        if !mime_type.is_empty() {
            result.insert(mime_type, content);
        }
    }

    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

/// Decode UTF-16LE bytes to a String
fn decode_utf16le(data: &[u8]) -> String {
    if data.len() < 2 {
        return String::new();
    }

    // Convert bytes to u16 array (little-endian)
    let u16_vec: Vec<u16> = data
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();

    // Decode UTF-16
    String::from_utf16_lossy(&u16_vec)
}

/// Parse Chrome's chromium/x-web-custom-data format (Pickle)
/// Format (all little-endian):
/// - 4 bytes: payload size
/// - 8 bytes: number of entries (64-bit)
/// - For each entry:
///   - 4 bytes: MIME type length (in chars, not bytes)
///   - MIME type as UTF-16LE (padded to 4-byte boundary)
///   - 4 bytes: data length (in chars, not bytes)
///   - Data as UTF-16LE (padded to 4-byte boundary)
fn parse_chromium_custom_data(data: &[u8]) -> Option<HashMap<String, String>> {
    if data.len() < 12 {
        return None;
    }

    let mut result = HashMap::new();
    let mut offset = 0;

    // Skip payload size (4 bytes)
    offset += 4;

    // Read number of entries (8 bytes little-endian, but usually small)
    if offset + 8 > data.len() {
        return None;
    }
    let num_entries = u64::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
        data[offset + 4],
        data[offset + 5],
        data[offset + 6],
        data[offset + 7],
    ]) as usize;
    offset += 8;

    eprintln!(
        "[TiddlyDesktop] Linux: Chrome clipdata: {} entries",
        num_entries
    );

    for i in 0..num_entries {
        if offset + 4 > data.len() {
            break;
        }

        // Read MIME type length (in UTF-16 chars)
        let mime_char_len = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        offset += 4;

        let mime_byte_len = mime_char_len * 2;
        if offset + mime_byte_len > data.len() {
            break;
        }

        // Read MIME type as UTF-16LE
        let mime_bytes = &data[offset..offset + mime_byte_len];
        let mime_type = decode_utf16le(mime_bytes);
        offset += mime_byte_len;

        // Align to 4-byte boundary
        let padding = (4 - (mime_byte_len % 4)) % 4;
        offset += padding;

        if offset + 4 > data.len() {
            break;
        }

        // Read content length (in UTF-16 chars)
        let content_char_len = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        offset += 4;

        let content_byte_len = content_char_len * 2;
        if offset + content_byte_len > data.len() {
            // Try to read what we can
            let available = data.len() - offset;
            let content_bytes = &data[offset..offset + available];
            let content = decode_utf16le(content_bytes);
            if !mime_type.is_empty() && !content.is_empty() {
                result.insert(mime_type, content);
            }
            break;
        }

        // Read content as UTF-16LE
        let content_bytes = &data[offset..offset + content_byte_len];
        let content = decode_utf16le(content_bytes);
        offset += content_byte_len;

        // Align to 4-byte boundary
        let padding = (4 - (content_byte_len % 4)) % 4;
        offset += padding;

        eprintln!(
            "[TiddlyDesktop] Linux: Chrome clipdata entry {}: {} = {} chars",
            i,
            mime_type,
            content.len()
        );

        if !mime_type.is_empty() {
            result.insert(mime_type, content);
        }
    }

    if result.is_empty() {
        None
    } else {
        Some(result)
    }
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

    // Variable to track if we found tiddler data
    let mut tiddler_json: Option<String> = None;
    let mut other_content: HashMap<String, String> = HashMap::new();

    // 1. Check browser custom clipdata formats for tiddler data
    if data_type == "application/x-moz-custom-clipdata" && raw_data.len() >= 8 {
        if let Some(moz_data) = parse_moz_custom_clipdata(&raw_data) {
            eprintln!(
                "[TiddlyDesktop] Linux: Parsed Mozilla custom clipdata, found {} entries",
                moz_data.len()
            );
            for (mime_type, content) in &moz_data {
                eprintln!(
                    "[TiddlyDesktop] Linux: Mozilla clipdata entry: {} ({} chars)",
                    mime_type,
                    content.len()
                );
                if mime_type == "text/vnd.tiddler" {
                    tiddler_json = Some(content.clone());
                } else if mime_type == "text/html" {
                    // Security: Sanitize HTML from external sources
                    other_content.insert(mime_type.clone(), sanitize_html(content));
                } else if mime_type == "text/uri-list" {
                    // Security: Sanitize URI list
                    other_content.insert(mime_type.clone(), sanitize_uri_list(content));
                } else {
                    other_content.insert(mime_type.clone(), content.clone());
                }
            }
        }
    } else if data_type == "chromium/x-web-custom-data" && raw_data.len() >= 12 {
        if let Some(chrome_data) = parse_chromium_custom_data(&raw_data) {
            eprintln!(
                "[TiddlyDesktop] Linux: Parsed Chrome custom clipdata, found {} entries",
                chrome_data.len()
            );
            for (mime_type, content) in &chrome_data {
                eprintln!(
                    "[TiddlyDesktop] Linux: Chrome clipdata entry: {} ({} chars)",
                    mime_type,
                    content.len()
                );
                if mime_type == "text/vnd.tiddler" {
                    tiddler_json = Some(content.clone());
                } else if mime_type == "text/html" {
                    // Security: Sanitize HTML from external sources
                    other_content.insert(mime_type.clone(), sanitize_html(content));
                } else if mime_type == "text/uri-list" {
                    // Security: Sanitize URI list
                    other_content.insert(mime_type.clone(), sanitize_uri_list(content));
                } else {
                    other_content.insert(mime_type.clone(), content.clone());
                }
            }
        }
    }

    // 2. Try to decode the raw data as text (for non-browser-custom types)
    let text = if tiddler_json.is_none() && !raw_data.is_empty() {
        let decoded = decode_string(&raw_data);
        if !decoded.is_empty() && !decoded.contains('\u{FFFD}') {
            Some(decoded)
        } else {
            selection_data.text().map(|t| t.to_string())
        }
    } else {
        None
    };

    // 3. Check if the received data IS tiddler data (direct type or content detection)
    if let Some(ref text_content) = text {
        eprintln!(
            "[TiddlyDesktop] Linux: Got text content: {} chars, preview: {:?}",
            text_content.len(),
            &text_content[..std::cmp::min(200, text_content.len())]
        );

        // Check for file URIs first (not tiddler data)
        if text_content.starts_with("file://") || data_type == "text/uri-list" {
            let paths: Vec<String> = text_content
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

            // Security: Sanitize file paths to prevent path traversal
            let paths = sanitize_file_paths(paths);

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

                context.drag_finish(true, false, time);
                s.drag_active = false;
                s.drop_in_progress = false;
                return;
            }
        }

        // Check if this is tiddler data (by type or content)
        if data_type == "text/vnd.tiddler" {
            tiddler_json = Some(text_content.clone());
        } else if tiddler_json.is_none() {
            // Content-based detection: looks like tiddler JSON array?
            let looks_like_tiddler = text_content.trim_start().starts_with('[')
                && text_content.contains("\"title\"")
                && (text_content.contains("\"text\"") || text_content.contains("\"fields\""));
            if looks_like_tiddler {
                eprintln!("[TiddlyDesktop] Linux: Detected tiddler JSON by content!");
                tiddler_json = Some(text_content.clone());
            }
        }

        // Store other content types
        if tiddler_json.is_none() {
            if text_content.starts_with("http://") || text_content.starts_with("https://") {
                // Security: Check for dangerous URL schemes
                if !is_dangerous_url(text_content) {
                    other_content.insert("text/uri-list".to_string(), text_content.clone());
                    other_content.insert("URL".to_string(), text_content.clone());
                }
            } else if text_content.trim_start().starts_with('<') || data_type == "text/html" {
                // Security: Sanitize HTML content
                let sanitized_html = sanitize_html(text_content);
                other_content.insert("text/html".to_string(), sanitized_html);
            }
            other_content.insert("text/plain".to_string(), text_content.clone());
        }
    }

    // 4. Build final types/data - prioritize tiddler data if found
    if let Some(ref tiddler) = tiddler_json {
        eprintln!("[TiddlyDesktop] Linux: Using tiddler data ({} chars)", tiddler.len());
        types.push("text/vnd.tiddler".to_string());
        data.insert("text/vnd.tiddler".to_string(), tiddler.clone());
        // Also add as text/plain for fallback
        types.push("text/plain".to_string());
        data.insert("text/plain".to_string(), tiddler.clone());
    } else {
        // No tiddler data - use other content
        for (mime_type, content) in other_content {
            if !data.contains_key(&mime_type) {
                types.push(mime_type.clone());
                data.insert(mime_type, content);
            }
        }
    }

    // 5. Emit the final content
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

    context.drag_finish(has_content, false, time);
    s.drag_active = false;
    s.drop_in_progress = false;
}

/// Global flag to track if we have a pending outgoing drag source setup
fn outgoing_drag_source_ready() -> &'static Mutex<bool> {
    static INSTANCE: OnceLock<Mutex<bool>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(false))
}

/// Prepare a native drag operation - sets up the widget as a drag source
/// This should be called when an internal drag STARTS, not when it leaves
/// GTK will then handle the transition to external drag naturally when pointer leaves
pub fn prepare_native_drag(window: &WebviewWindow, data: OutgoingDragData) -> Result<(), String> {
    let label = window.label().to_string();
    eprintln!(
        "[TiddlyDesktop] Linux: prepare_native_drag called for window '{}'",
        label
    );

    // Store the drag data with window label for the drag-data-get callback
    {
        let mut guard = outgoing_drag_state().lock().map_err(|e| e.to_string())?;
        *guard = Some(OutgoingDragState {
            data: data.clone(),
            source_window_label: label.clone(),
            data_was_requested: false, // Reset - will be set true when drag-data-get is called
        });
    }

    // Mark that we have a drag source ready
    {
        let mut ready = outgoing_drag_source_ready().lock().map_err(|e| e.to_string())?;
        *ready = true;
    }

    eprintln!("[TiddlyDesktop] Linux: Native drag data stored, ready for transition");
    Ok(())
}

/// Clean up native drag preparation (called when internal drag ends normally)
pub fn cleanup_native_drag() -> Result<(), String> {
    eprintln!("[TiddlyDesktop] Linux: cleanup_native_drag called");

    // Stop pointer polling
    stop_pointer_polling();

    // Clear the stored drag data
    if let Ok(mut guard) = outgoing_drag_state().lock() {
        *guard = None;
    }

    // Clear the ready flag
    if let Ok(mut ready) = outgoing_drag_source_ready().lock() {
        *ready = false;
    }

    // Clear the drag context and icon state
    ACTIVE_DRAG_CONTEXT.with(|ctx| {
        *ctx.borrow_mut() = None;
    });
    DRAG_ICON_STATE.with(|state| {
        *state.borrow_mut() = None;
    });

    Ok(())
}

/// Global storage for the drag image as PNG data
fn outgoing_drag_image() -> &'static Mutex<Option<Vec<u8>>> {
    static INSTANCE: OnceLock<Mutex<Option<Vec<u8>>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(None))
}

/// State for pointer polling during outgoing drag (stored thread-locally)
struct PointerPollState {
    window: WebviewWindow,
    window_label: String,
    /// The WebKit widget's GDK window - for getting coordinates relative to web content
    webkit_gdk_window: gdk::Window,
    window_width: i32,
    window_height: i32,
    last_inside: bool,
}

/// Stored drag icon state for show/hide toggling
struct DragIconState {
    pixbuf: gdk::gdk_pixbuf::Pixbuf,
    hot_x: i32,
    hot_y: i32,
}

thread_local! {
    /// Thread-local storage for the polling source ID (so we can cancel it)
    static POLLING_SOURCE_ID: RefCell<Option<glib::SourceId>> = const { RefCell::new(None) };
    /// Thread-local storage for the active drag context (to hide/show icon)
    static ACTIVE_DRAG_CONTEXT: RefCell<Option<gdk::DragContext>> = const { RefCell::new(None) };
    /// Thread-local storage for the drag icon (to restore when leaving window again)
    static DRAG_ICON_STATE: RefCell<Option<DragIconState>> = const { RefCell::new(None) };
}

/// Start polling the pointer position for outgoing drag re-entry detection
fn start_pointer_polling(window: &WebviewWindow) {
    let label = window.label().to_string();
    eprintln!("[TiddlyDesktop] Linux: Starting pointer polling for window '{}'", label);

    // Stop any existing polling first
    stop_pointer_polling();

    // Get the GTK window to find the WebKit widget
    let gtk_window = match window.gtk_window() {
        Ok(w) => w,
        Err(e) => {
            eprintln!("[TiddlyDesktop] Linux: Failed to get GTK window for polling: {}", e);
            return;
        }
    };

    // Find the WebKit widget and get its GDK window
    // This gives us coordinates relative to the web content area (not including title bar)
    let webkit_widget = match find_webkit_widget(&gtk_window) {
        Some(w) => w,
        None => {
            eprintln!("[TiddlyDesktop] Linux: Failed to find WebKit widget for polling");
            return;
        }
    };

    let webkit_gdk_window = match webkit_widget.window() {
        Some(w) => w,
        None => {
            eprintln!("[TiddlyDesktop] Linux: Failed to get WebKit GDK window for polling");
            return;
        }
    };

    // Get WebKit widget size for bounds checking
    let allocation = webkit_widget.allocation();
    let window_width = allocation.width();
    let window_height = allocation.height();

    eprintln!(
        "[TiddlyDesktop] Linux: WebKit widget bounds for polling: {}x{}",
        window_width, window_height
    );

    let window_clone = window.clone();
    let state = Rc::new(RefCell::new(PointerPollState {
        window: window_clone,
        window_label: label.clone(),
        webkit_gdk_window,
        window_width,
        window_height,
        last_inside: false,
    }));

    let source_id = glib::timeout_add_local(std::time::Duration::from_millis(16), move || {
        poll_pointer_position(&state)
    });

    // Store the source ID so we can cancel it later
    POLLING_SOURCE_ID.with(|id| {
        *id.borrow_mut() = Some(source_id);
    });
}

/// Stop polling the pointer position
fn stop_pointer_polling() {
    eprintln!("[TiddlyDesktop] Linux: Stopping pointer polling");
    POLLING_SOURCE_ID.with(|id| {
        if let Some(source_id) = id.borrow_mut().take() {
            source_id.remove();
        }
    });
}

/// Poll the pointer position and emit events
fn poll_pointer_position(state: &Rc<RefCell<PointerPollState>>) -> glib::ControlFlow {
    let state_ref = match state.try_borrow() {
        Ok(s) => s,
        Err(_) => return glib::ControlFlow::Continue,
    };

    // Check if we still have outgoing drag data and if data was requested (dropped outside)
    let (has_drag_data, data_was_requested) = outgoing_drag_state()
        .lock()
        .map(|g| {
            if let Some(drag_state) = g.as_ref() {
                (true, drag_state.data_was_requested)
            } else {
                (false, false)
            }
        })
        .unwrap_or((false, false));

    if !has_drag_data {
        eprintln!("[TiddlyDesktop] Linux: No drag data, stopping pointer polling");
        POLLING_SOURCE_ID.with(|id| {
            *id.borrow_mut() = None;
        });
        return glib::ControlFlow::Break;
    }

    // If data was requested, it means an external app received the drop - stop polling
    if data_was_requested {
        eprintln!("[TiddlyDesktop] Linux: Data was requested (dropped outside), stopping pointer polling");
        POLLING_SOURCE_ID.with(|id| {
            *id.borrow_mut() = None;
        });
        return glib::ControlFlow::Break;
    }

    // Get the default display and seat
    let display = match gdk::Display::default() {
        Some(d) => d,
        None => return glib::ControlFlow::Continue,
    };

    let seat = match display.default_seat() {
        Some(s) => s,
        None => return glib::ControlFlow::Continue,
    };

    let pointer = match seat.pointer() {
        Some(p) => p,
        None => return glib::ControlFlow::Continue,
    };

    // Get button state and pointer position relative to the WebKit widget
    // Using device_position() on the WebKit's GDK window gives us coordinates
    // relative to the web content area, matching what JavaScript expects
    let (_win, local_x, local_y, mask) = state_ref.webkit_gdk_window.device_position(&pointer);
    let button_pressed = mask.contains(gdk::ModifierType::BUTTON1_MASK);

    // Check if pointer is inside window bounds using local coordinates
    let inside = local_x >= 0
        && local_x < state_ref.window_width
        && local_y >= 0
        && local_y < state_ref.window_height;

    let window = state_ref.window.clone();
    let window_label = state_ref.window_label.clone();
    let was_inside = state_ref.last_inside;

    drop(state_ref); // Release borrow before mutating

    // Update last_inside state
    if let Ok(mut s) = state.try_borrow_mut() {
        s.last_inside = inside;
    }

    if !button_pressed {
        // Button released - stop polling and emit drag end
        eprintln!(
            "[TiddlyDesktop] Linux: Button released at local=({}, {}), inside={}",
            local_x, local_y, inside
        );

        // Emit td-pointer-up so JavaScript knows the button was released
        // Include window label so JS can verify it's for this window
        #[derive(serde::Serialize, Clone)]
        struct PointerUpPayload {
            x: i32,
            y: i32,
            inside: bool,
            #[serde(rename = "windowLabel")]
            window_label: String,
        }
        let _ = window.emit("td-pointer-up", PointerUpPayload {
            x: local_x,
            y: local_y,
            inside,
            window_label: window_label.clone(),
        });

        POLLING_SOURCE_ID.with(|id| {
            *id.borrow_mut() = None;
        });
        return glib::ControlFlow::Break;
    }

    // Button is still pressed
    if inside {
        // Pointer is inside the window with button pressed
        if !was_inside {
            // Just re-entered the window - hide GTK's drag icon since JS will show its own
            eprintln!(
                "[TiddlyDesktop] Linux: Pointer re-entered window at local=({}, {}), hiding GTK drag icon",
                local_x, local_y
            );
            hide_gtk_drag_icon();
        }

        // Emit motion event - keep polling to track drag
        // Include window label so JS can verify it's for this window
        eprintln!("[TiddlyDesktop] Linux: POLLING position: local=({}, {})", local_x, local_y);
        #[derive(serde::Serialize, Clone)]
        struct MotionPayload {
            x: i32,
            y: i32,
            #[serde(rename = "isOurDrag")]
            is_our_drag: bool,
            #[serde(rename = "fromPolling")]
            from_polling: bool,
            #[serde(rename = "windowLabel")]
            window_label: String,
        }
        let _ = window.emit("td-drag-motion", MotionPayload {
            x: local_x,
            y: local_y,
            is_our_drag: true,
            from_polling: true,
            window_label: window_label.clone(),
        });
    } else {
        // Pointer is outside the window
        if was_inside {
            // Just left the window - emit leave event
            eprintln!(
                "[TiddlyDesktop] Linux: Pointer left window at local=({}, {}), restoring GTK drag icon",
                local_x, local_y
            );

            // Restore the GTK drag icon since we're leaving the window
            restore_gtk_drag_icon();

            #[derive(serde::Serialize, Clone)]
            struct LeavePayload {
                #[serde(rename = "isOurDrag")]
                is_our_drag: bool,
                #[serde(rename = "windowLabel")]
                window_label: String,
            }
            let _ = window.emit("td-drag-leave", LeavePayload {
                is_our_drag: true,
                window_label: window_label.clone(),
            });
        }
    }

    glib::ControlFlow::Continue
}

/// Start a native drag operation with the provided data
/// This is called from JavaScript when the pointer leaves the window during an internal drag
pub fn start_native_drag(window: &WebviewWindow, data: OutgoingDragData, x: i32, y: i32, image_data: Option<Vec<u8>>, image_offset_x: Option<i32>, image_offset_y: Option<i32>) -> Result<(), String> {
    let label = window.label().to_string();
    eprintln!(
        "[TiddlyDesktop] Linux: start_native_drag called for window '{}' at ({}, {}), has image: {}, offset: ({:?}, {:?})",
        label, x, y, image_data.is_some(), image_offset_x, image_offset_y
    );

    // Store the drag data with window label for the drag-data-get callback
    {
        let mut guard = outgoing_drag_state().lock().map_err(|e| e.to_string())?;
        *guard = Some(OutgoingDragState {
            data: data.clone(),
            source_window_label: label.clone(),
            data_was_requested: false, // Reset - will be set true when drag-data-get is called
        });
    }

    // Store the drag image if provided
    if let Some(img) = image_data.as_ref() {
        if let Ok(mut guard) = outgoing_drag_image().lock() {
            *guard = Some(img.clone());
        }
    }

    // Get the GTK window
    let gtk_window = window.gtk_window().map_err(|e| format!("Failed to get GTK window: {}", e))?;

    // Find the WebKit widget (or use the window itself)
    let widget = find_webkit_widget(&gtk_window)
        .unwrap_or_else(|| gtk_window.upcast::<gtk::Widget>());

    let widget_type = widget.type_().name();
    eprintln!(
        "[TiddlyDesktop] Linux: Starting drag on widget type: {}",
        widget_type
    );

    // Build target list based on what data we have
    // Order matches TiddlyWiki's import priority
    let target_list = TargetList::new(&[]);

    if data.text_vnd_tiddler.is_some() {
        // Primary TiddlyWiki tiddler format
        let atom = gdk::Atom::intern("text/vnd.tiddler");
        target_list.add(&atom, 0, 1);
    }
    if data.url.is_some() {
        // Standard URL type (used by Chrome-like browsers)
        let atom = gdk::Atom::intern("URL");
        target_list.add(&atom, 0, 2);
    }
    if data.text_x_moz_url.is_some() {
        // Mozilla URL format
        let atom = gdk::Atom::intern("text/x-moz-url");
        target_list.add(&atom, 0, 3);
    }
    if data.text_html.is_some() {
        let atom = gdk::Atom::intern("text/html");
        target_list.add(&atom, 0, 4);
    }
    if data.text_uri_list.is_some() {
        target_list.add_uri_targets(0);
    }
    if data.text_plain.is_some() {
        target_list.add_text_targets(0);
    }

    // Ensure we have at least text targets
    target_list.add_text_targets(0);

    // Note: drag-data-get and drag-end handlers are connected once in setup_outgoing_drag_handlers

    // Try to get current GDK event for better drag initiation (GTK3)
    let current_event = gtk::current_event();

    eprintln!(
        "[TiddlyDesktop] Linux: current_event available: {}",
        current_event.is_some()
    );

    // If no current event, the drag may not survive long
    // but we'll still try - the data can be provided while it lasts
    let event_for_drag: Option<gdk::Event> = current_event;

    // Start the drag operation
    let drag_context = widget.drag_begin_with_coordinates(
        &target_list,
        DragAction::COPY | DragAction::MOVE,
        1, // button 1 (left mouse button)
        event_for_drag.as_ref(), // Use event if available
        x,
        y,
    );

    if let Some(context) = drag_context {
        eprintln!(
            "[TiddlyDesktop] Linux: Native drag started successfully"
        );

        // Store the drag context so we can hide the icon on re-entry
        ACTIVE_DRAG_CONTEXT.with(|ctx| {
            *ctx.borrow_mut() = Some(context.clone());
        });

        // Try to set drag icon from the captured image
        // Use the offset from JS to position the icon identically
        let offset_x = image_offset_x.unwrap_or(0);
        let offset_y = image_offset_y.unwrap_or(0);
        let icon_set = if let Some(img_data) = image_data {
            set_drag_icon_from_png(&context, &img_data, offset_x, offset_y)
        } else {
            false
        };

        // Fall back to stock icon if custom icon failed
        if !icon_set {
            context.drag_set_icon_name("text-x-generic", 0, 0);
        }

        // Start polling pointer position to detect re-entry while dragging
        // This works around the limitation that WebKit doesn't receive pointer events
        // when the pointer is outside the window with button held
        start_pointer_polling(window);

        Ok(())
    } else {
        // Clean up on failure
        if let Ok(mut guard) = outgoing_drag_state().lock() {
            *guard = None;
        }
        if let Ok(mut guard) = outgoing_drag_image().lock() {
            *guard = None;
        }
        Err("Failed to start native drag - drag_begin_with_coordinates returned None".to_string())
    }
}

/// Hide the GTK drag icon by setting a 1x1 transparent pixbuf
/// Called when the pointer re-enters the window and JS takes over drag visualization
fn hide_gtk_drag_icon() {
    ACTIVE_DRAG_CONTEXT.with(|ctx| {
        if let Some(context) = ctx.borrow().as_ref() {
            use gdk::gdk_pixbuf::Pixbuf;
            // Create a 1x1 transparent pixbuf
            if let Some(pixbuf) = Pixbuf::new(gdk::gdk_pixbuf::Colorspace::Rgb, true, 8, 1, 1) {
                // Fill with transparent pixels (RGBA = 0,0,0,0)
                pixbuf.fill(0x00000000);
                context.drag_set_icon_pixbuf(&pixbuf, 0, 0);
                eprintln!("[TiddlyDesktop] Linux: GTK drag icon hidden (set to 1x1 transparent)");
            }
        }
    });
}

/// Restore the GTK drag icon from stored state
/// Called when the pointer leaves the window after re-entry
fn restore_gtk_drag_icon() {
    ACTIVE_DRAG_CONTEXT.with(|ctx| {
        if let Some(context) = ctx.borrow().as_ref() {
            DRAG_ICON_STATE.with(|state| {
                if let Some(icon_state) = state.borrow().as_ref() {
                    context.drag_set_icon_pixbuf(&icon_state.pixbuf, icon_state.hot_x, icon_state.hot_y);
                    eprintln!("[TiddlyDesktop] Linux: GTK drag icon restored");
                }
            });
        }
    });
}

/// Set drag icon from PNG data with the specified hot spot offset
/// Applies 0.7 opacity to match JS drag image styling
fn set_drag_icon_from_png(context: &gdk::DragContext, png_data: &[u8], hot_x: i32, hot_y: i32) -> bool {
    use gdk::gdk_pixbuf::Pixbuf;
    use gtk::gio::MemoryInputStream;
    use glib::Bytes;

    eprintln!("[TiddlyDesktop] Linux: Setting drag icon from PNG ({} bytes), hot spot: ({}, {})", png_data.len(), hot_x, hot_y);

    // Create a memory input stream from the PNG data
    let bytes = Bytes::from(png_data);
    let stream = MemoryInputStream::from_bytes(&bytes);

    // Load pixbuf from stream
    match Pixbuf::from_stream(&stream, None::<&gtk::gio::Cancellable>) {
        Ok(pixbuf) => {
            let width = pixbuf.width();
            let height = pixbuf.height();

            // Apply 0.7 opacity to match JS drag image styling
            let pixbuf_with_alpha = apply_opacity_to_pixbuf(&pixbuf, 0.7);

            eprintln!(
                "[TiddlyDesktop] Linux: Loaded pixbuf {}x{} with 0.7 opacity, using hot spot ({}, {})",
                width, height, hot_x, hot_y
            );

            // Store the pixbuf and offsets so we can restore after hiding
            DRAG_ICON_STATE.with(|state| {
                *state.borrow_mut() = Some(DragIconState {
                    pixbuf: pixbuf_with_alpha.clone(),
                    hot_x,
                    hot_y,
                });
            });

            // Set the pixbuf as drag icon with the same offset that JS uses
            // This ensures both drag images overlap perfectly
            context.drag_set_icon_pixbuf(&pixbuf_with_alpha, hot_x, hot_y);
            true
        }
        Err(e) => {
            eprintln!("[TiddlyDesktop] Linux: Failed to load pixbuf: {}", e);
            false
        }
    }
}

/// Apply opacity to a pixbuf by multiplying the alpha channel
fn apply_opacity_to_pixbuf(pixbuf: &gdk::gdk_pixbuf::Pixbuf, opacity: f64) -> gdk::gdk_pixbuf::Pixbuf {
    use gdk::gdk_pixbuf::{Colorspace, Pixbuf};

    let width = pixbuf.width();
    let height = pixbuf.height();
    let has_alpha = pixbuf.has_alpha();

    // Create a new pixbuf with alpha channel
    let result = Pixbuf::new(Colorspace::Rgb, true, 8, width, height)
        .expect("Failed to create pixbuf for opacity");

    // Copy and apply opacity
    if has_alpha {
        // Source has alpha - copy with modified alpha
        pixbuf.composite(
            &result,
            0, 0,           // dest x, y
            width, height,  // dest width, height
            0.0, 0.0,       // offset x, y
            1.0, 1.0,       // scale x, y
            gdk::gdk_pixbuf::InterpType::Nearest,
            (opacity * 255.0) as i32,  // overall_alpha (0-255)
        );
    } else {
        // Source doesn't have alpha - fill with transparent first, then composite
        result.fill(0x00000000);
        pixbuf.composite(
            &result,
            0, 0,
            width, height,
            0.0, 0.0,
            1.0, 1.0,
            gdk::gdk_pixbuf::InterpType::Nearest,
            (opacity * 255.0) as i32,
        );
    }

    result
}

