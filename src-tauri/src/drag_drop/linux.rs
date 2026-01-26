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
use glib::prelude::*;
use gtk::prelude::*;
use gtk::TargetList;
use tauri::{Emitter, Manager, WebviewWindow};

use super::native_dnd;

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
    /// True if this is a text-selection drag (not a draggable element)
    pub is_text_selection_drag: bool,
}

/// Registry mapping GDK window raw pointers to window labels + dimensions
/// The raw pointer (usize) is used both as the key and to reconstruct the GdkWindow on demand
/// SAFETY: This is only accessed from the GTK main thread
fn gdk_window_registry() -> &'static Mutex<HashMap<usize, (String, tauri::AppHandle, i32, i32)>> {
    static INSTANCE: OnceLock<Mutex<HashMap<usize, (String, tauri::AppHandle, i32, i32)>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Track which window label currently has the pointer (from enter/leave events)
/// This is used for Wayland cross-wiki drag detection since GDK pointer queries don't work
fn pointer_inside_window() -> &'static Mutex<Option<String>> {
    static INSTANCE: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(None))
}

/// Set which window has the pointer
fn set_pointer_inside_window(label: Option<String>) {
    if let Ok(mut guard) = pointer_inside_window().lock() {
        *guard = label;
    }
}

/// Get which window has the pointer
fn get_pointer_inside_window() -> Option<String> {
    pointer_inside_window()
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
}

/// Track which window is currently the drag target (from drag-motion/drag-leave events)
/// This is more reliable during drags than pointer tracking since GTK drag events
/// fire correctly even when pointer enter/leave events don't
fn current_drag_target() -> &'static Mutex<Option<String>> {
    static INSTANCE: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(None))
}

/// Set the current drag target window
fn set_current_drag_target(label: Option<String>) {
    if let Ok(mut guard) = current_drag_target().lock() {
        if *guard != label {
            eprintln!("[TiddlyDesktop] Linux: Current drag target changed: {:?} -> {:?}", *guard, label);
        }
        *guard = label;
    }
}

/// Get the current drag target window
fn get_current_drag_target() -> Option<String> {
    current_drag_target()
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
}

// NOTE: get_dest_window_label was removed because context.dest_window() returns
// the window receiving the signal, NOT the actual pointer location. This made it
// useless for determining which window the pointer is actually over during cross-wiki
// drags. We now use tracking-based filtering via current_drag_target() instead.

/// Registry mapping window labels to WebKitWebView widget pointers for drag destination toggling
/// Stores raw pointers as usize for Send+Sync compatibility
/// SAFETY: The pointers must only be dereferenced on the GTK main thread
fn webkit_widget_registry() -> &'static Mutex<HashMap<String, usize>> {
    static INSTANCE: OnceLock<Mutex<HashMap<String, usize>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a WebKitWebView widget for later drag destination toggling
fn register_webkit_widget(label: &str, widget: &gtk::Widget) {
    use glib::translate::ToGlibPtr;
    // Store the raw pointer - the widget is kept alive by GTK
    let stash: glib::translate::Stash<'_, *mut gtk::ffi::GtkWidget, _> = widget.to_glib_none();
    let ptr = stash.0 as usize;
    if let Ok(mut registry) = webkit_widget_registry().lock() {
        registry.insert(label.to_string(), ptr);
        eprintln!("[TiddlyDesktop] Linux: Registered WebKit widget {:?} for '{}' for drag dest toggling", ptr, label);
    }
}

/// Check if the drag source widget is one of our registered WebKit widgets
/// Returns the source window label if it is, None if external source
fn get_source_window_label(context: &gdk::DragContext) -> Option<String> {
    use glib::translate::ToGlibPtr;
    if let Some(source_widget) = context.drag_get_source_widget() {
        let stash: glib::translate::Stash<'_, *mut gtk::ffi::GtkWidget, _> = source_widget.to_glib_none();
        let source_ptr = stash.0 as usize;
        if let Ok(registry) = webkit_widget_registry().lock() {
            for (label, ptr) in registry.iter() {
                if *ptr == source_ptr {
                    return Some(label.clone());
                }
            }
        }
    }
    None
}

/// Toggle drag destination on WebKitWebView for a window
/// NOTE: This is now a no-op! We should NEVER call drag_dest_set() or drag_dest_unset()
/// on a WebKitWebView because:
/// 1. WebKitWebView is already a fully configured drag destination
/// 2. drag_dest_set() replaces WebKit's internal target list and breaks caret updates
/// 3. drag_dest_unset() removes the destination entirely
/// Instead, we just connect to signals and return false to let WebKit handle everything.
pub fn set_drag_dest_enabled(label: &str, enabled: bool) {
    eprintln!("[TiddlyDesktop] Linux: set_drag_dest_enabled('{}', {}) - NO-OP (WebKit handles drag dest)", label, enabled);
    // Intentionally do nothing - WebKit's drag destination must remain intact
}

/// Temporarily ungrab the seat to allow focus changes during drag
/// This is called from JavaScript when hovering over an editable element
pub fn ungrab_seat_for_focus(label: &str) {
    eprintln!("[TiddlyDesktop] Linux: ungrab_seat_for_focus('{}')", label);

    let label = label.to_string();
    glib::MainContext::default().invoke(move || {
        // Get the display and default seat
        if let Some(display) = gdk::Display::default() {
            if let Some(seat) = display.default_seat() {
                eprintln!("[TiddlyDesktop] Linux: Ungrabbing seat for '{}'", label);
                seat.ungrab();
                eprintln!("[TiddlyDesktop] Linux: Seat ungrabbed");
            } else {
                eprintln!("[TiddlyDesktop] Linux: No default seat found");
            }
        } else {
            eprintln!("[TiddlyDesktop] Linux: No default display found");
        }
    });
}

/// Register a GDK window with its label for cross-wiki drag detection
fn register_gdk_window(gdk_window: &gdk::Window, label: &str, app_handle: &tauri::AppHandle, width: i32, height: i32) {
    let ptr = gdk_window.as_ptr() as usize;
    if let Ok(mut registry) = gdk_window_registry().lock() {
        registry.insert(ptr, (label.to_string(), app_handle.clone(), width, height));
        eprintln!("[TiddlyDesktop] Linux: Registered GDK window {:?} for '{}' ({}x{})", ptr, label, width, height);
    }

    // Also register with native DnD system for proper cross-wiki detection
    // Get the underlying X11 window ID or Wayland surface ID
    match native_dnd::get_display_server() {
        native_dnd::DisplayServer::X11 => {
            // Get X11 window ID using FFI
            // gdk_x11_window_get_xid is available when running on X11
            extern "C" {
                fn gdk_x11_window_get_xid(window: *mut gdk::ffi::GdkWindow) -> u32;
            }
            let xid = unsafe { gdk_x11_window_get_xid(gdk_window.as_ptr()) };
            if xid != 0 {
                native_dnd::register_surface(xid, label);
                eprintln!("[TiddlyDesktop] Linux: Registered X11 window {} for '{}' with native DnD", xid, label);
            }
        }
        native_dnd::DisplayServer::Wayland => {
            // On Wayland, try to get the wl_surface from GDK
            // Note: Our separate Wayland connection won't receive drag events for GTK's surfaces,
            // but we register anyway for potential future use with GTK Wayland integration
            extern "C" {
                fn gdk_wayland_window_get_wl_surface(window: *mut gdk::ffi::GdkWindow) -> *mut std::ffi::c_void;
            }
            let surface_ptr = unsafe { gdk_wayland_window_get_wl_surface(gdk_window.as_ptr()) };
            if !surface_ptr.is_null() {
                // Use pointer address as a pseudo-ID for tracking
                // This won't match protocol IDs but can be used for GDK-based tracking
                let surface_id = (surface_ptr as usize & 0xFFFFFFFF) as u32;
                native_dnd::register_surface(surface_id, label);
                eprintln!("[TiddlyDesktop] Linux: Registered Wayland surface {:?} (pseudo-id {}) for '{}'",
                    surface_ptr, surface_id, label);
            } else {
                eprintln!("[TiddlyDesktop] Linux: Could not get Wayland surface for '{}'", label);
            }
        }
        native_dnd::DisplayServer::Unknown => {}
    }
}

/// Update the dimensions for a registered GDK window
fn update_gdk_window_dimensions(gdk_window: &gdk::Window, width: i32, height: i32) {
    let ptr = gdk_window.as_ptr() as usize;
    if let Ok(mut registry) = gdk_window_registry().lock() {
        if let Some(entry) = registry.get_mut(&ptr) {
            entry.2 = width;
            entry.3 = height;
        }
    }
}

/// Info needed to reset WebKitGTK pointer state after drag operations
struct ActiveDragWidgetInfo {
    window_label: String,
    app_handle: tauri::AppHandle,
}

/// Global storage for the active drag's widget info (for pointer reset on Escape/cleanup)
fn active_drag_widget_info() -> &'static Mutex<Option<ActiveDragWidgetInfo>> {
    static INSTANCE: OnceLock<Mutex<Option<ActiveDragWidgetInfo>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(None))
}

/// Set the active drag widget info (called when starting a native drag)
fn set_active_drag_widget_info(window_label: String, app_handle: tauri::AppHandle) {
    if let Ok(mut guard) = active_drag_widget_info().lock() {
        *guard = Some(ActiveDragWidgetInfo { window_label, app_handle });
    }
}

/// Clear the active drag widget info
fn clear_active_drag_widget_info() {
    if let Ok(mut guard) = active_drag_widget_info().lock() {
        *guard = None;
    }
}

/// Get the active drag widget info if available
fn get_active_drag_widget_info() -> Option<(String, tauri::AppHandle)> {
    active_drag_widget_info()
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|info| (info.window_label.clone(), info.app_handle.clone())))
}

/// Outgoing drag data with source window identification
struct OutgoingDragState {
    data: OutgoingDragData,
    source_window_label: String,
    /// Set to true when drag-data-get is called (data was actually transferred)
    data_was_requested: bool,
    /// True if this is a text-selection drag (needs special handling)
    is_text_selection_drag: bool,
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
    let result = outgoing_drag_state()
        .lock()
        .map(|guard| {
            match guard.as_ref() {
                Some(state) => {
                    let matches = state.source_window_label == window_label;
                    if !matches {
                        eprintln!("[TiddlyDesktop] Linux: has_outgoing_data_for_window('{}') - state exists but label='{}' doesn't match",
                            window_label, state.source_window_label);
                    }
                    matches
                }
                None => {
                    eprintln!("[TiddlyDesktop] Linux: has_outgoing_data_for_window('{}') - no state stored", window_label);
                    false
                }
            }
        })
        .unwrap_or_else(|_| {
            eprintln!("[TiddlyDesktop] Linux: has_outgoing_data_for_window('{}') - lock failed", window_label);
            false
        });
    result
}

/// Check if we have any outgoing drag data (from any window of our app)
fn has_any_outgoing_data() -> bool {
    outgoing_drag_state()
        .lock()
        .map(|guard| guard.is_some())
        .unwrap_or(false)
}

use super::encoding::decode_string;
use super::sanitize::{sanitize_html, sanitize_uri_list, sanitize_file_paths, is_dangerous_url};

/// Data captured from a drag operation
#[derive(Clone, Debug, serde::Serialize)]
pub struct DragContentData {
    pub types: Vec<String>,
    pub data: HashMap<String, String>,
    #[serde(rename = "targetWindow")]
    pub target_window: String,
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

    // Initialize native DnD protocol handler and GTK settings (only happens once)
    static NATIVE_DND_INIT: std::sync::Once = std::sync::Once::new();
    NATIVE_DND_INIT.call_once(|| {
        match native_dnd::init() {
            Ok(true) => eprintln!("[TiddlyDesktop] Linux: Native DnD protocol initialized"),
            Ok(false) => eprintln!("[TiddlyDesktop] Linux: Native DnD protocol not available"),
            Err(e) => eprintln!("[TiddlyDesktop] Linux: Native DnD init error: {}", e),
        }

        // Reduce GTK drag threshold for more responsive drag start
        // Default is 8 pixels, we reduce to 4 for snappier feel
        if let Some(settings) = gtk::Settings::default() {
            settings.set_property("gtk-dnd-drag-threshold", 4i32);
            eprintln!("[TiddlyDesktop] Linux: Set GTK drag threshold to 4 pixels");
        }
    });

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

    // NOTE: Do NOT call drag_source_unset() - WebKit must remain the drag source

    // Find the WebKitWebView widget
    if let Some(webview_widget) = find_webkit_widget(gtk_window) {
        let widget_type = webview_widget.type_().name();
        eprintln!(
            "[TiddlyDesktop] Linux: Found WebKit widget: {}",
            widget_type
        );

        // Register the widget for later use
        register_webkit_widget(&label, &webview_widget);

        // IMPORTANT: Do NOT call drag_source_unset() on the WebView!
        // WebKit must remain the drag source so it can update the caret during drags.
        // We just hook into drag-begin/drag-data-get to observe and provide custom data.
        eprintln!("[TiddlyDesktop] Linux: Letting WebKit remain drag source (preserves caret)");

        // Set up drag destination handlers on WebKitWebView
        setup_webkit_drag_handlers(&webview_widget, state);

        // Set up outgoing drag handlers (drag SOURCE) for when we drag TO external apps
        setup_outgoing_drag_handlers(&webview_widget, window.clone());

        // Register the WebKit widget's GDK window for cross-wiki drag detection
        // This allows polling to identify when the pointer enters another wiki window
        if let Some(gdk_window) = webview_widget.window() {
            // Get initial window dimensions
            let alloc = webview_widget.allocation();
            register_gdk_window(&gdk_window, &label, window.app_handle(), alloc.width(), alloc.height());

            // Update dimensions when window is resized
            let gdk_window_for_resize = gdk_window.clone();
            webview_widget.connect_size_allocate(move |_widget, alloc| {
                update_gdk_window_dimensions(&gdk_window_for_resize, alloc.width(), alloc.height());
            });
        }
    }

    // Set up enter/leave event tracking for Wayland cross-wiki detection
    // GTK's pointer queries don't work on Wayland during drags, but enter/leave events do
    // We track which window the pointer is over, which works even during drags
    setup_pointer_tracking(gtk_window, &label);
}

/// Set up pointer enter/leave tracking for a window
/// This is crucial for Wayland where GDK pointer queries don't work during drags
fn setup_pointer_tracking(gtk_window: &gtk::ApplicationWindow, label: &str) {
    // Enable necessary events
    gtk_window.add_events(
        gdk::EventMask::ENTER_NOTIFY_MASK | gdk::EventMask::LEAVE_NOTIFY_MASK
    );

    let label_for_enter = label.to_string();
    gtk_window.connect_enter_notify_event(move |_window, event| {
        // Only track if this is a normal crossing (not from grab)
        let crossing_mode = event.mode();
        if crossing_mode == gdk::CrossingMode::Normal || crossing_mode == gdk::CrossingMode::Ungrab {
            eprintln!("[TiddlyDesktop] Linux: Pointer entered window '{}' (mode: {:?})", label_for_enter, crossing_mode);
            set_pointer_inside_window(Some(label_for_enter.clone()));
        }
        glib::Propagation::Proceed
    });

    let label_for_leave = label.to_string();
    gtk_window.connect_leave_notify_event(move |_window, event| {
        // Only track if this is a normal crossing and pointer is leaving to outside
        let crossing_mode = event.mode();
        let detail = event.detail();
        // NotifyInferior means we entered a child widget, not left the window
        if detail != gdk::NotifyType::Inferior {
            if crossing_mode == gdk::CrossingMode::Normal || crossing_mode == gdk::CrossingMode::Ungrab {
                eprintln!("[TiddlyDesktop] Linux: Pointer left window '{}' (mode: {:?}, detail: {:?})", label_for_leave, crossing_mode, detail);
                // Only clear if we were the one inside
                if get_pointer_inside_window().as_ref() == Some(&label_for_leave) {
                    set_pointer_inside_window(None);
                }
            }
        }
        glib::Propagation::Proceed
    });

    eprintln!("[TiddlyDesktop] Linux: Set up pointer tracking for window '{}'", label);
}

/// Set up handlers for outgoing drags (when we drag TO external applications)
///
/// NEW STRATEGY: Let WebKit remain the drag source and just hook into signals.
/// This preserves WebKit's internal drag handling including caret updates.
/// We just observe drag-begin (to extend targets and set icon) and
/// provide data in drag-data-get when requested.
fn setup_outgoing_drag_handlers(widget: &gtk::Widget, window: WebviewWindow) {
    eprintln!("[TiddlyDesktop] Linux: Setting up outgoing drag handlers (WebKit-compatible)");

    let window_label_for_begin = window.label().to_string();

    // Connect drag-begin signal - fires when WebKit starts a drag
    // We observe the drag start and set a custom icon if we have one
    // NOTE: We can't extend WebKit's target list (it returns null) because WebKit
    // manages targets internally through the DOM dataTransfer API.
    // For inter-wiki drops, we rely on:
    // 1. The destination requesting text/vnd.tiddler (even if not advertised)
    // 2. Our drag-data-get providing the data if we have it
    widget.connect_drag_begin(move |_widget, context| {
        eprintln!("[TiddlyDesktop] Linux: drag-begin signal (WebKit started drag)");

        // Check if we have prepared data
        let has_our_data = has_outgoing_data_for_window(&window_label_for_begin) || has_any_outgoing_data();

        if has_our_data {
            eprintln!("[TiddlyDesktop] Linux: We have prepared drag data");
        } else {
            eprintln!("[TiddlyDesktop] Linux: No prepared data yet (will be set in dragstart)");
        }

        // IMPORTANT: WebKit sets its own drag icon AFTER drag-begin returns.
        // We MUST use an idle callback to override WebKit's icon.
        // The idle callback runs after WebKit's setup is complete.
        let context_clone = context.clone();
        let context_clone2 = context.clone();
        eprintln!("[TiddlyDesktop] Linux: Scheduling idle callback to set drag icon (to override WebKit)");
        glib::idle_add_local_once(move || {
            // Check if we have a pre-rendered PNG from pointerdown
            if let Ok(guard) = outgoing_drag_image().lock() {
                if let Some((img_data, offset_x, offset_y)) = guard.as_ref() {
                    eprintln!("[TiddlyDesktop] Linux: Setting drag icon from PNG in idle callback");
                    if set_drag_icon_from_png(&context_clone, img_data, *offset_x, *offset_y) {
                        return;
                    }
                }
            }

            // No PNG yet - set a transparent icon to hide WebKit's default
            eprintln!("[TiddlyDesktop] Linux: No PNG yet, setting transparent icon while waiting");
            if let Some(transparent_pixbuf) = create_transparent_pixbuf(1, 1) {
                context_clone.drag_set_icon_pixbuf(&transparent_pixbuf, 0, 0);
            }

            // Schedule delayed retry for late-arriving PNG from dragstart backup
            glib::timeout_add_local_once(std::time::Duration::from_millis(100), move || {
                if let Ok(guard) = outgoing_drag_image().lock() {
                    if let Some((img_data, offset_x, offset_y)) = guard.as_ref() {
                        eprintln!("[TiddlyDesktop] Linux: Setting drag icon from PNG in delayed retry");
                        set_drag_icon_from_png(&context_clone2, img_data, *offset_x, *offset_y);
                    }
                }
            });
        });
    });

    // Handle drag-data-get: provide ALL stored data and block WebKit's internal format.
    //
    // JS calls prepare_native_drag during dragstart for ALL drags (including text selections).
    // We provide the stored data for all requested formats, and block WebKit's internal
    // binary format that causes "Chinese characters" in Firefox.
    //
    // We use raw signal connection to run BEFORE WebKit and stop emission when needed.
    unsafe {
        use glib::translate::ToGlibPtr;
        use std::ffi::CStr;

        extern "C" fn drag_data_get_handler(
            widget: *mut gtk::ffi::GtkWidget,
            _context: *mut gdk::ffi::GdkDragContext,
            selection_data: *mut gtk::ffi::GtkSelectionData,
            _info: u32,
            _time: u32,
            _user_data: glib::ffi::gpointer,
        ) {
            unsafe {
                extern "C" {
                    fn gtk_selection_data_get_target(data: *mut gtk::ffi::GtkSelectionData) -> gdk::ffi::GdkAtom;
                    fn gdk_atom_name(atom: gdk::ffi::GdkAtom) -> *mut std::ffi::c_char;
                    fn g_free(ptr: glib::ffi::gpointer);
                    fn g_signal_stop_emission_by_name(instance: *mut glib::gobject_ffi::GObject, name: *const std::ffi::c_char);
                    fn gtk_selection_data_set(
                        data: *mut gtk::ffi::GtkSelectionData,
                        type_: gdk::ffi::GdkAtom,
                        format: i32,
                        data_ptr: *const u8,
                        length: i32,
                    );
                    fn gdk_atom_intern(name: *const std::ffi::c_char, only_if_exists: i32) -> gdk::ffi::GdkAtom;
                }

                let target_atom = gtk_selection_data_get_target(selection_data);
                let target_name_ptr = gdk_atom_name(target_atom);
                if target_name_ptr.is_null() {
                    return;
                }
                let target_name = CStr::from_ptr(target_name_ptr).to_string_lossy().to_string();
                g_free(target_name_ptr as glib::ffi::gpointer);

                // Get stored drag data, source window label, and text-selection flag
                let (stored_data, source_window, is_text_selection) = outgoing_drag_state().lock().ok()
                    .and_then(|guard| {
                        guard.as_ref().map(|state| (
                            Some(state.data.clone()),
                            Some(state.source_window_label.clone()),
                            state.is_text_selection_drag
                        ))
                    })
                    .unwrap_or((None, None, false));

                // Check if this is a same-window drop
                let current_target = current_drag_target().lock().ok().and_then(|g| g.clone());
                let is_same_window = source_window.is_some() && source_window == current_target;

                // For same-window TIDDLER drags, let WebKit handle everything natively.
                // But for TEXT-SELECTION drags, we must provide data because WebKit's native
                // handling is broken (DataTransfer doesn't preserve data across events).
                // We only block the webkit internal format (which causes Chinese characters in Firefox).
                if is_same_window && !is_text_selection {
                    if target_name == "org.webkitgtk.WebKit.custom-pasteboard-data" {
                        // Block the internal format
                        let atom_name = b"org.webkitgtk.WebKit.custom-pasteboard-data\0".as_ptr() as *const std::ffi::c_char;
                        let atom = gdk_atom_intern(atom_name, 0);
                        gtk_selection_data_set(selection_data, atom, 8, std::ptr::null(), 0);
                        let signal_name = b"drag-data-get\0".as_ptr() as *const std::ffi::c_char;
                        g_signal_stop_emission_by_name(widget as *mut glib::gobject_ffi::GObject, signal_name);
                    }
                    // For all other formats, let WebKit handle natively
                    return;
                }

                // If no stored data, let WebKit handle everything EXCEPT its internal format
                let data = match stored_data {
                    Some(d) => d,
                    None => {
                        // Still block the internal format even without data
                        if target_name == "org.webkitgtk.WebKit.custom-pasteboard-data" {
                            // Set empty data
                            let atom_name = b"org.webkitgtk.WebKit.custom-pasteboard-data\0".as_ptr() as *const std::ffi::c_char;
                            let atom = gdk_atom_intern(atom_name, 0);
                            gtk_selection_data_set(selection_data, atom, 8, std::ptr::null(), 0);
                            let signal_name = b"drag-data-get\0".as_ptr() as *const std::ffi::c_char;
                            g_signal_stop_emission_by_name(widget as *mut glib::gobject_ffi::GObject, signal_name);
                        }
                        return;
                    }
                };

                // Helper to set data (without stopping emission - let WebKit also provide data)
                let set_data = |atom_name: &[u8], content: &str| {
                    let atom = gdk_atom_intern(atom_name.as_ptr() as *const std::ffi::c_char, 0);
                    let bytes = content.as_bytes();
                    gtk_selection_data_set(selection_data, atom, 8, bytes.as_ptr(), bytes.len() as i32);
                    mark_data_requested();
                    // Don't stop emission - let WebKit's handler also run
                };

                match target_name.as_str() {
                    // Block WebKit's internal format - set empty data and stop signal
                    "org.webkitgtk.WebKit.custom-pasteboard-data" => {
                        gtk_selection_data_set(selection_data, target_atom, 8, std::ptr::null(), 0);
                        let signal_name = b"drag-data-get\0".as_ptr() as *const std::ffi::c_char;
                        g_signal_stop_emission_by_name(widget as *mut glib::gobject_ffi::GObject, signal_name);
                    }
                    // Our custom TiddlyWiki type
                    "text/vnd.tiddler" => {
                        if let Some(ref tiddler) = data.text_vnd_tiddler {
                            set_data(b"text/vnd.tiddler\0", tiddler);
                        }
                    }
                    // Plain text (including charset variants)
                    s if s == "text/plain" || s.starts_with("text/plain;") || s == "TEXT" => {
                        if let Some(ref text) = data.text_plain {
                            set_data(b"text/plain\0", text);
                        }
                    }
                    "UTF8_STRING" => {
                        if let Some(ref text) = data.text_plain {
                            set_data(b"UTF8_STRING\0", text);
                        }
                    }
                    "STRING" => {
                        if let Some(ref text) = data.text_plain {
                            set_data(b"STRING\0", text);
                        }
                    }
                    // HTML - DON'T provide it to avoid Firefox/Chrome encoding incompatibility
                    // Firefox expects UTF-16LE, Chrome expects UTF-8 - can't satisfy both
                    // Apps will fall back to text/plain which works universally
                    // Block the signal so WebKit doesn't provide its version either
                    s if s == "text/html" || s.starts_with("text/html;") => {
                        let signal_name = b"drag-data-get\0".as_ptr() as *const std::ffi::c_char;
                        g_signal_stop_emission_by_name(widget as *mut glib::gobject_ffi::GObject, signal_name);
                    }
                    // URI list
                    "text/uri-list" => {
                        if let Some(ref uri) = data.url {
                            set_data(b"text/uri-list\0", uri);
                        } else if let Some(ref uris) = data.text_uri_list {
                            set_data(b"text/uri-list\0", uris);
                        }
                    }
                    // Mozilla URL format (needs UTF-16LE)
                    "text/x-moz-url" => {
                        if let Some(ref moz_url) = data.text_x_moz_url {
                            let title = data.text_plain.as_deref().unwrap_or("");
                            let full_moz_url = format!("{}\n{}", moz_url, title);
                            let utf16_bytes: Vec<u8> = full_moz_url
                                .encode_utf16()
                                .flat_map(|c| c.to_le_bytes())
                                .collect();
                            let atom = gdk_atom_intern(b"text/x-moz-url\0".as_ptr() as *const std::ffi::c_char, 0);
                            gtk_selection_data_set(selection_data, atom, 8, utf16_bytes.as_ptr(), utf16_bytes.len() as i32);
                            mark_data_requested();
                        }
                    }
                    // URL type
                    "URL" => {
                        if let Some(ref url) = data.url {
                            set_data(b"URL\0", url);
                        }
                    }
                    _ => {
                        // Unknown type - don't provide data, let WebKit handle if it can
                    }
                }
            }
        }

        // Connect with G_CONNECT_FIRST (value 0) - this doesn't exist in glib-rs,
        // but we can use g_signal_connect_data directly with connect_flags = 0
        extern "C" {
            fn g_signal_connect_data(
                instance: *mut glib::gobject_ffi::GObject,
                detailed_signal: *const std::ffi::c_char,
                c_handler: Option<extern "C" fn()>,
                data: glib::ffi::gpointer,
                destroy_data: Option<extern "C" fn(glib::ffi::gpointer, *mut glib::gobject_ffi::GClosure)>,
                connect_flags: u32,
            ) -> std::ffi::c_ulong;
        }

        let signal_name = b"drag-data-get\0".as_ptr() as *const std::ffi::c_char;
        let widget_ptr: *mut gtk::ffi::GtkWidget = widget.to_glib_none().0;
        g_signal_connect_data(
            widget_ptr as *mut glib::gobject_ffi::GObject,
            signal_name,
            Some(std::mem::transmute(drag_data_get_handler as *const ())),
            std::ptr::null_mut(),
            None,
            0, // G_CONNECT_DEFAULT - runs before handlers connected with connect()
        );
    }

    // Clone window for later handlers
    let window_for_failed = window.clone();
    let window_for_end = window.clone();

    // Connect drag-end signal to notify JavaScript and clean up state
    // NOTE: We do NOT call drag_source_unset() - WebKit remains in control
    widget.connect_drag_end(move |_widget, _context| {
        let data_was_requested = was_data_requested();
        eprintln!("[TiddlyDesktop] Linux: drag-end signal, data_was_requested={}", data_was_requested);

        // Clean up our drag state
        if let Ok(mut guard) = outgoing_drag_state().lock() {
            *guard = None;
        }
        // Clear the drag image so we know to wait for fresh data on next drag
        if let Ok(mut guard) = outgoing_drag_image().lock() {
            *guard = None;
        }
        clear_active_drag_widget_info();
        if let Ok(mut ready) = outgoing_drag_source_ready().lock() {
            *ready = false;
        }
        // Clear the current drag target tracking
        set_current_drag_target(None);

        // Notify JavaScript
        #[derive(serde::Serialize, Clone)]
        struct DragEndPayload {
            data_was_requested: bool,
            #[serde(rename = "targetWindow")]
            target_window: String,
        }
        let _ = window_for_end.emit("td-drag-end", DragEndPayload {
            data_was_requested,
            target_window: window_for_end.label().to_string(),
        });
        eprintln!("[TiddlyDesktop] Linux: Emitted td-drag-end");
    });

    // Connect drag-failed signal to detect cancelled drags
    // NOTE: We do NOT call drag_source_unset() - WebKit remains in control
    widget.connect_drag_failed(move |_widget, _context, result| {
        eprintln!("[TiddlyDesktop] Linux: drag-failed signal, result={:?}", result);

        #[derive(serde::Serialize, Clone)]
        struct DragCancelPayload {
            reason: String,
            #[serde(rename = "targetWindow")]
            target_window: String,
        }

        let reason = match result {
            gtk::DragResult::Success => "success",
            gtk::DragResult::NoTarget => "no_target",
            gtk::DragResult::UserCancelled => "user_cancelled",
            gtk::DragResult::TimeoutExpired => "timeout",
            gtk::DragResult::GrabBroken => "grab_broken",
            gtk::DragResult::Error => "error",
            _ => "unknown",
        };

        // Only emit cancel for actual failures, not success
        if !matches!(result, gtk::DragResult::Success) {
            let _ = window_for_failed.emit("td-drag-cancel", DragCancelPayload {
                reason: reason.to_string(),
                target_window: window_for_failed.label().to_string(),
            });
        }

        // Clean up our drag state
        if let Ok(mut guard) = outgoing_drag_state().lock() {
            *guard = None;
        }
        // Clear the drag image so we know to wait for fresh data on next drag
        if let Ok(mut guard) = outgoing_drag_image().lock() {
            *guard = None;
        }
        // Clear the current drag target tracking
        set_current_drag_target(None);
        eprintln!("[TiddlyDesktop] Linux: Cleared drag state in drag-failed");

        glib::Propagation::Proceed
    });

    eprintln!("[TiddlyDesktop] Linux: Outgoing drag handlers connected (WebKit-compatible)");
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
fn setup_widget_drag_handlers(_widget: &gtk::Widget, _state: Rc<RefCell<DragState>>, label: &str) {
    // NOTE: We do NOT set up any drop handling on the GtkWindow!
    //
    // WebKitWebView is already a fully configured drag destination and handles:
    //   - Caret positioning during drags
    //   - Text insertion into inputs/textareas/contenteditables
    //   - Native DOM drop events for TiddlyWiki's dropzones
    //
    // Any drag handling on the parent GtkWindow interferes with WebKit's native
    // drop handling. We only observe drags on the WebKitWebView for visual feedback
    // (td-drag-motion, td-drag-leave events) but let WebKit handle all drops.
    eprintln!(
        "[TiddlyDesktop] Linux: Skipping GtkWindow drag handlers for '{}' - letting WebKit handle all drops natively",
        label
    );
}

/// Set up drag handlers on WebKit widget
fn setup_webkit_drag_handlers(_widget: &gtk::Widget, _state: Rc<RefCell<DragState>>) {
    // EXPERIMENT: Don't connect any handlers, let vanilla WebKitGTK handle everything
    // like Epiphany does. Testing if this allows external drops to work natively.
    eprintln!("[TiddlyDesktop] Linux: setup_webkit_drag_handlers - NO-OP (testing vanilla WebKitGTK)");
}

#[allow(dead_code)]
/// Set up drag handlers on WebKit widget (DISABLED for vanilla WebKitGTK test)
fn setup_webkit_drag_handlers_disabled(widget: &gtk::Widget, state: Rc<RefCell<DragState>>) {
    // NOTE: We do NOT call drag_dest_set() on the WebView!
    // WebKitWebView is already a fully configured drag destination.
    // Calling drag_dest_set() would:
    //   1. Replace WebKit's internal target list
    //   2. Break caret updates during drags
    //   3. Interfere with WebKit's internal drop handling
    //
    // We connect to signals to:
    //   - Emit td-drag-motion/leave for JS dropzone highlighting
    //   - Intercept external drops (WebKitGTK bug: doesn't transfer cross-process data to JS)
    //   - Let internal drops through to WebKit for native text insertion

    // Connect drag-motion signal
    let state_motion = state.clone();
    widget.connect_drag_motion(move |_widget, context, x, y, time| {
        // Get window label first (need to borrow state)
        let window_label = {
            let s = state_motion.borrow();
            s.window.label().to_string()
        };

        // Check if the drag source is one of our registered WebKit widgets
        // This is reliable regardless of async prepare_native_drag timing
        let source_window_label = get_source_window_label(context);
        let is_our_drag = source_window_label.is_some();

        // Check if this is an internal drag (source is same widget/window)
        let is_internal = source_window_label.as_ref() == Some(&window_label);

        // Rate-limited logging
        static LAST_LOG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let last = LAST_LOG.load(std::sync::atomic::Ordering::Relaxed);
        let should_log = now - last > 500;
        if should_log {
            LAST_LOG.store(now, std::sync::atomic::Ordering::Relaxed);
        }

        // For non-internal drags (external or cross-wiki), emit td-drag-motion for JS
        // JS uses this for TiddlyWiki dropzone highlighting (tc-dragover)
        //
        // GTK sends drag-motion to ALL windows that registered as drag destinations,
        // not just the one under the pointer. On Wayland we can't query pointer position,
        // so we use tracking: the first window to receive drag-motion after the previous
        // target received drag-leave becomes the new target.
        //
        // NOTE: context.dest_window() is NOT useful here - it returns the window
        // receiving THIS signal, not the actual pointer location.
        let should_emit = if is_internal {
            false // Internal drags are handled by WebKit natively
        } else if is_our_drag {
            // Cross-wiki drag: use tracking to determine if we're the actual target
            let current_target = get_current_drag_target();
            match current_target {
                Some(ref target) if target == &window_label => {
                    // We're the current target, emit
                    true
                }
                Some(ref target) => {
                    // Different window is the target, don't emit
                    if should_log {
                        eprintln!("[TiddlyDesktop] Linux: drag-motion to {} but target is {}, skipping",
                            window_label, target);
                    }
                    false
                }
                None => {
                    // No target set, claim it
                    if should_log {
                        eprintln!("[TiddlyDesktop] Linux: {} claiming drag target (was None)", window_label);
                    }
                    set_current_drag_target(Some(window_label.clone()));
                    true
                }
            }
        } else {
            // External drag: always emit (external apps don't have this issue)
            true
        };

        if should_log {
            eprintln!("[TiddlyDesktop] Linux: WebKit drag-motion at ({}, {}), source={:?}, target={}, is_our_drag={}, should_emit={}",
                x, y, source_window_label, window_label, is_our_drag, should_emit);
        }

        // Only update drag_active and emit events if this window should handle the drag
        if should_emit {
            {
                let mut s = state_motion.borrow_mut();
                s.last_position = Some((x, y));
                s.drag_active = true;
            }
            let s = state_motion.borrow();
            let _ = s.window.emit(
                "td-drag-motion",
                serde_json::json!({
                    "x": x,
                    "y": y,
                    "screenCoords": false,
                    "isOurDrag": is_our_drag,
                    "sourceWindow": source_window_label,
                    "targetWindow": window_label
                }),
            );
        }

        // Tell GTK we accept this drag
        context.drag_status(DragAction::COPY, time);

        // Return false to let WebKit handle caret positioning
        // WebKit's internal drag handling will update the caret over editable elements
        false
    });

    // Connect drag-leave signal
    let state_leave = state.clone();
    widget.connect_drag_leave(move |_widget, context, _time| {
        // Get window label and drag_active state
        let (window_label, was_drag_active) = {
            let s = state_leave.borrow();
            (s.window.label().to_string(), s.drag_active)
        };

        // Check if the drag source is one of our registered WebKit widgets
        let source_window_label = get_source_window_label(context);
        let is_our_drag = source_window_label.is_some();

        // For cross-wiki drags, clear the current target if we were it
        // This allows the next window to claim the drag on its drag-motion
        if is_our_drag {
            let current_target = get_current_drag_target();
            if current_target.as_ref() == Some(&window_label) {
                eprintln!("[TiddlyDesktop] Linux: drag-leave clearing target (was {})", window_label);
                set_current_drag_target(None);
            } else {
                eprintln!("[TiddlyDesktop] Linux: drag-leave from {} but target is {:?}, not clearing",
                    window_label, current_target);
            }
        }

        // Only emit drag-leave if we had an active drag in this window
        // This prevents spurious drag-leave events to windows that never had the drag
        if was_drag_active {
            eprintln!("[TiddlyDesktop] Linux: drag-leave emitting td-drag-leave for {}", window_label);
            let s = state_leave.borrow();
            let _ = s.window.emit("td-drag-leave", serde_json::json!({
                "isOurDrag": is_our_drag,
                "sourceWindow": source_window_label,
                "targetWindow": window_label
            }));
        }

        // Update state
        {
            let mut s = state_leave.borrow_mut();
            if !s.drop_in_progress {
                s.drag_active = false;
            }
        }
    });

    // Connect drag-drop signal
    // WebKitGTK bug: It doesn't transfer cross-process drag data to JavaScript's DataTransfer.
    // Strategy:
    //   - Internal drops (source_widget exists): return false → WebKit handles natively
    //     (preserves text insertion into inputs/textareas/contenteditables)
    //   - External drops (source_widget is None): we do GTK data transfer → emit to JS
    let state_drop_signal = state.clone();
    widget.connect_drag_drop(move |widget, context, x, y, time| {
        let window_label = {
            let s = state_drop_signal.borrow();
            s.window.label().to_string()
        };

        let source_widget = context.drag_get_source_widget();
        let is_external = source_widget.is_none();

        eprintln!("[TiddlyDesktop] Linux: drag-drop at ({}, {}) is_external={}, target={}",
            x, y, is_external, window_label);

        if is_external {
            // External drop - do GTK data transfer ourselves
            {
                let mut s = state_drop_signal.borrow_mut();
                s.drop_requested = true;
                s.last_position = Some((x, y));
            }

            let targets = context.list_targets();
            let priority = ["text/uri-list", "text/html", "text/plain", "UTF8_STRING", "STRING"];

            let target = priority.iter()
                .find_map(|&p| targets.iter().find(|t| t.name() == p).cloned())
                .or_else(|| targets.first().cloned());

            if let Some(t) = target {
                eprintln!("[TiddlyDesktop] Linux: Requesting external data: {}", t.name());
                widget.drag_get_data(context, &t, time);
                true
            } else {
                false
            }
        } else {
            // Internal drop - let WebKit handle natively
            eprintln!("[TiddlyDesktop] Linux: Internal drop - WebKit handles");
            false
        }
    });

    // Handle data from external drops
    let state_data_received = state.clone();
    widget.connect_drag_data_received(move |_widget, context, x, y, selection_data, _info, time| {
        handle_drag_data_received(&state_data_received, context, x, y, selection_data, time);
    });

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

    let window_label = s.window.label().to_string();
    let source_window_label = get_source_window_label(context);

    // Check if this drop is from our app (same-window OR cross-wiki)
    // GTK's data transfer returns NONE for intra-process drops, so we use our stored data directly
    let is_our_drag = source_window_label.is_some();

    if is_our_drag {
        let source_label = source_window_label.clone().unwrap_or_default();
        let is_same_window = source_label == window_label;
        eprintln!(
            "[TiddlyDesktop] Linux: Drop from our app detected: {} -> {} (same_window={})",
            source_label, window_label, is_same_window
        );

        // Let WebKit handle all internal drops natively
        // This allows dropping into inputs/textareas to work correctly
        // WebKit will receive the data via the standard drag-data-get mechanism
        eprintln!("[TiddlyDesktop] Linux: Internal drop - letting WebKit handle natively");
        s.drop_in_progress = false;
        // Don't call drag_finish - let the native handling continue
        return;
    }

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
                    // Convert screen coords to window-local coords using root_coords
                    // root_coords(0,0) gives us where window origin is in screen coords
                    let (win_screen_x, win_screen_y) = dest_window.root_coords(0, 0);
                    let rel_x = px - win_screen_x;
                    let rel_y = py - win_screen_y;
                    eprintln!(
                        "[TiddlyDesktop] Linux: Got pointer position: screen({}, {}), window screen origin({}, {}), relative({}, {})",
                        px, py, win_screen_x, win_screen_y, rel_x, rel_y
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
                        "screenCoords": false,
                        "targetWindow": window_label
                    }),
                );
                let _ = s.window.emit(
                    "td-file-drop",
                    serde_json::json!({
                        "paths": paths,
                        "targetWindow": window_label
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
                "screenCoords": false,
                "targetWindow": window_label
            }),
        );

        let content_data = DragContentData { types, data, target_window: window_label.clone() };
        let _ = s.window.emit("td-drag-content", &content_data);
    }

    // Note: Source window cleanup happens in its drag-end handler
    // No special cross-wiki handling needed with native GTK DnD

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
            is_text_selection_drag: data.is_text_selection_drag,
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

/// Clean up native drag preparation (called when internal drag ends normally or on Escape)
pub fn cleanup_native_drag() -> Result<(), String> {
    eprintln!("[TiddlyDesktop] Linux: cleanup_native_drag called");

    // Get the active drag widget info for pointer reset BEFORE clearing it
    let widget_info = get_active_drag_widget_info();

    // Clear the stored drag data
    if let Ok(mut guard) = outgoing_drag_state().lock() {
        *guard = None;
    }

    // Clear the ready flag
    if let Ok(mut ready) = outgoing_drag_source_ready().lock() {
        *ready = false;
    }

    // Clear the pending drag image (so next drag gets fresh data)
    if let Ok(mut guard) = outgoing_drag_image().lock() {
        *guard = None;
    }

    // Clear the active drag widget info
    clear_active_drag_widget_info();

    // Reset WebKitGTK pointer state to fix the bug where pointer events stop working
    // after GTK drag operations (including Escape cancellation)
    if let Some((window_label, app_handle)) = widget_info {
        eprintln!(
            "[TiddlyDesktop] Linux: Resetting pointer state for window '{}' after cleanup",
            window_label
        );

        // Schedule pointer reset on main thread
        glib::MainContext::default().invoke(move || {
            // Look up the GDK window for this window
            if let Ok(registry) = gdk_window_registry().lock() {
                for (gdk_ptr, (label, _, _, _)) in registry.iter() {
                    if label == &window_label {
                        // Found the window - reconstruct GDK window and reset
                        let gdk_window: gdk::Window = unsafe {
                            glib::translate::from_glib_none(*gdk_ptr as *mut _)
                        };

                        // Get the WebKit widget from the Tauri window
                        if let Some(webview_window) = app_handle.get_webview_window(&window_label) {
                            if let Ok(gtk_window) = webview_window.gtk_window() {
                                if let Some(webkit_widget) = find_webkit_widget(&gtk_window) {
                                    // Reset pointer state
                                    let injection_succeeded = reset_webkit_pointer_state(&webkit_widget, &gdk_window, 0, 0);

                                    // Emit event to JavaScript so it can enable mousedown fallback if needed
                                    // Use global emit since emit_to doesn't reach JS listeners
                                    let _ = webview_window.emit(
                                        "td-reset-pointer-state",
                                        serde_json::json!({
                                            "x": 0,
                                            "y": 0,
                                            "needsFallback": !injection_succeeded,
                                            "fromCleanup": true,
                                            "windowLabel": window_label
                                        })
                                    );
                                    eprintln!(
                                        "[TiddlyDesktop] Linux: Emitted td-reset-pointer-state from cleanup, needsFallback={}",
                                        !injection_succeeded
                                    );
                                }
                            }
                        }
                        break;
                    }
                }
            }
        });
    }

    Ok(())
}

/// Global storage for the drag image as PNG data with hotspot offset
fn outgoing_drag_image() -> &'static Mutex<Option<(Vec<u8>, i32, i32)>> {
    static INSTANCE: OnceLock<Mutex<Option<(Vec<u8>, i32, i32)>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(None))
}

/// Set the pending drag icon (called from JS before drag starts)
pub fn set_pending_drag_icon(image_data: Vec<u8>, offset_x: i32, offset_y: i32) -> Result<(), String> {
    eprintln!(
        "[TiddlyDesktop] Linux: set_pending_drag_icon called with {} bytes, offset ({}, {})",
        image_data.len(), offset_x, offset_y
    );
    if let Ok(mut guard) = outgoing_drag_image().lock() {
        *guard = Some((image_data, offset_x, offset_y));
        Ok(())
    } else {
        Err("Failed to lock outgoing_drag_image".to_string())
    }
}

/// Aggressively reset WebKitGTK's pointer event state after a re-entry + drop
/// This is called when the user drags out, re-enters, and drops inside the window.
/// WebKitGTK has a bug where pointer events stop being generated after GTK drag operations.
/// This version works on both X11 and Wayland.
/// Reset WebKitGTK's pointer state after a re-entry + drop scenario.
/// Returns true if reset succeeded, false if JS mousedown fallback is needed.
fn reset_webkit_pointer_state(widget: &gtk::Widget, gdk_window: &gdk::Window, local_x: i32, local_y: i32) -> bool {
    eprintln!("[TiddlyDesktop] Linux: Resetting WebKitGTK pointer state at local ({}, {})", local_x, local_y);

    // Get display and seat
    let display = match gdk::Display::default() {
        Some(d) => d,
        None => {
            eprintln!("[TiddlyDesktop] Linux: No display for pointer reset");
            return false;
        }
    };
    let seat = match display.default_seat() {
        Some(s) => s,
        None => {
            eprintln!("[TiddlyDesktop] Linux: No seat for pointer reset");
            return false;
        }
    };

    // Ungrab the seat first
    seat.ungrab();
    eprintln!("[TiddlyDesktop] Linux: Ungrabbed seat");

    // Check for any active GTK grab and release it
    if let Some(grab_widget) = gtk::grab_get_current() {
        eprintln!("[TiddlyDesktop] Linux: Found active GTK grab on {:?}, removing", grab_widget.type_().name());
        grab_widget.grab_remove();
    }

    // Tell GTK the drag is completely done
    widget.drag_unhighlight();
    eprintln!("[TiddlyDesktop] Linux: Called drag_unhighlight");

    // Convert local coordinates to screen coordinates for input injection
    // local_x, local_y are relative to the WebKit WIDGET, so we need the widget's
    // screen position, not the toplevel's. Use get_root_coords to convert directly.
    let (screen_x, screen_y) = gdk_window.root_coords(local_x, local_y);
    eprintln!("[TiddlyDesktop] Linux: Converted local ({}, {}) to screen ({}, {}) via root_coords",
        local_x, local_y, screen_x, screen_y);
    eprintln!("[TiddlyDesktop] Linux: Screen coordinates for injection: ({}, {})", screen_x, screen_y);

    // Detect if we're on Wayland
    let is_wayland = std::env::var("WAYLAND_DISPLAY").is_ok()
        || std::env::var("XDG_SESSION_TYPE").map(|v| v == "wayland").unwrap_or(false);

    let injection_succeeded = if is_wayland {
        // On Wayland, try using webkit2gtk's run_javascript to directly execute
        // synthetic pointer events in WebKit's JS engine. This might trigger
        // different code paths than dispatching through Tauri events.
        eprintln!("[TiddlyDesktop] Linux: Wayland detected, trying webkit2gtk run_javascript approach");
        try_webkit_js_reset(widget, local_x, local_y)
    } else {
        // On X11, use XTest to inject real input events
        eprintln!("[TiddlyDesktop] Linux: X11 detected, trying XTest input injection");
        !super::input_inject::inject_click_or_need_fallback(screen_x, screen_y)
    };

    // Also do the standard GTK cleanup
    let widget_clone = widget.clone();
    glib::idle_add_local_once(move || {
        // Ungrab seat again
        if let Some(display) = gdk::Display::default() {
            if let Some(seat) = display.default_seat() {
                seat.ungrab();
            }
        }

        // Remove any GTK grab
        if let Some(grab_widget) = gtk::grab_get_current() {
            grab_widget.grab_remove();
        }

        // Queue redraw and grab focus
        widget_clone.queue_draw();
        if widget_clone.can_focus() {
            widget_clone.grab_focus();
        }

        eprintln!("[TiddlyDesktop] Linux: GTK cleanup complete");
    });

    injection_succeeded
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
            is_text_selection_drag: data.is_text_selection_drag,
        });
    }

    // Store widget info for pointer reset on Escape/cleanup
    set_active_drag_widget_info(label.clone(), window.app_handle().clone());

    // Store the drag image if provided (with offsets, defaulting to center if not specified)
    if let Some(img) = image_data.as_ref() {
        if let Ok(mut guard) = outgoing_drag_image().lock() {
            let offset_x = image_offset_x.unwrap_or(10);
            let offset_y = image_offset_y.unwrap_or(10);
            *guard = Some((img.clone(), offset_x, offset_y));
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

/// Update the drag icon during an active drag operation
/// This can be called from JS to change the drag image mid-drag
/// Note: Currently not implemented - drag icon must be set at drag start
pub fn update_drag_icon(_image_data: Vec<u8>, _offset_x: i32, _offset_y: i32) -> Result<(), String> {
    // Not storing drag context to avoid potential crashes with external drags
    eprintln!("[TiddlyDesktop] Linux: update_drag_icon not supported - set icon at drag start");
    Ok(())
}

/// Create a transparent pixbuf of the given size
fn create_transparent_pixbuf(width: i32, height: i32) -> Option<gdk::gdk_pixbuf::Pixbuf> {
    use gdk::gdk_pixbuf::{Colorspace, Pixbuf};

    let pixbuf = Pixbuf::new(Colorspace::Rgb, true, 8, width, height)?;
    // Fill with transparent pixels (RGBA = 0,0,0,0)
    pixbuf.fill(0x00000000);
    Some(pixbuf)
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

/// On Wayland, we can't inject real input events without libei permissions.
/// Return false to signal that JavaScript should handle the reset via synthetic events.
/// The JS code will dispatch a full click cycle at off-screen coordinates (-1000, -1000).
fn try_webkit_js_reset(_widget: &gtk::Widget, local_x: i32, local_y: i32) -> bool {
    eprintln!("[TiddlyDesktop] Linux: Wayland - delegating pointer reset to JavaScript at ({}, {})", local_x, local_y);
    // Return false so needsFallback=true is sent to JS
    // JS will then dispatch synthetic pointer events at off-screen coordinates
    false
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

