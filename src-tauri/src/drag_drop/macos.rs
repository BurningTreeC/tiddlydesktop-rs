//! macOS drag-drop handling using objc2/AppKit for content extraction
//!
//! WKWebView's native drag-drop handling doesn't reliably expose content (text, HTML, URLs)
//! from external apps to JavaScript via dataTransfer. We use method swizzling to intercept
//! NSDraggingDestination protocol methods and:
//! 1. Extract content from the drag pasteboard (NSPasteboard)
//! 2. Emit td-drag-* events to JavaScript
//! 3. Let JavaScript create synthetic DOM events for TiddlyWiki
//!
//! Internal drags (within the webview) are handled by JavaScript:
//! - internal_drag.js intercepts dragstart for draggable elements and text selections
//! - td-drag-* handlers check TD.isInternalDragActive() and skip if true
//! - internal_drag.js creates synthetic drag events using mouse tracking

#![cfg(target_os = "macos")]

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::{Arc, Mutex, Once};
use std::sync::OnceLock;

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject, Bool, Sel};
use objc2::sel;
use objc2_app_kit::{NSPasteboard, NSView, NSWindow, NSEvent, NSPasteboardItem};
use objc2_foundation::{NSArray, NSData, NSPoint, NSRect, NSSize, NSString};
use tauri::{Emitter, WebviewWindow};

use super::sanitize::{sanitize_html, is_dangerous_url};

/// Data captured from a drag operation
#[derive(Clone, Debug, serde::Serialize)]
pub struct DragContentData {
    pub types: Vec<String>,
    pub data: HashMap<String, String>,
    /// True if this is a text-selection drag (for filtering text/html in JS)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_text_selection_drag: Option<bool>,
    /// True if this is a same-window drag (for Issue 4b handling in JS)
    #[serde(rename = "isSameWindow")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_same_window: Option<bool>,
}

/// Pasteboard type constants (UTI format)
const NS_PASTEBOARD_TYPE_STRING: &str = "public.utf8-plain-text";
const NS_PASTEBOARD_TYPE_HTML: &str = "public.html";
const NS_PASTEBOARD_TYPE_URL: &str = "public.url";
const NS_PASTEBOARD_TYPE_FILE_URL: &str = "public.file-url";
/// TiddlyWiki tiddler JSON format
const NS_PASTEBOARD_TYPE_TIDDLER: &str = "text/vnd.tiddler";
const NS_PASTEBOARD_TYPE_JSON: &str = "public.json";
/// Browser custom data formats (may contain text/vnd.tiddler)
/// Note: Safari exposes content through standard pasteboard types, no custom format needed
const NS_PASTEBOARD_TYPE_MOZ_CUSTOM: &str = "org.mozilla.custom-clipdata";
const NS_PASTEBOARD_TYPE_CHROME_CUSTOM: &str = "org.chromium.web-custom-data";

/// NSDragOperation constants
const NS_DRAG_OPERATION_NONE: usize = 0;
const NS_DRAG_OPERATION_COPY: usize = 1;
const NS_DRAG_OPERATION_GENERIC: usize = 4;

/// State for tracking drag operations per window
struct DragState {
    window: WebviewWindow,
    drag_active: bool,
    last_position: Option<(f64, f64)>,
}

lazy_static::lazy_static! {
    static ref DRAG_STATES: Mutex<HashMap<String, Arc<Mutex<DragState>>>> = Mutex::new(HashMap::new());
    /// Maps webview pointer to window label for lookup in swizzled methods
    static ref WEBVIEW_TO_LABEL: Mutex<HashMap<usize, String>> = Mutex::new(HashMap::new());
}

/// Original method implementations (stored after first swizzle)
static mut ORIGINAL_DRAGGING_ENTERED: Option<unsafe extern "C" fn(*mut AnyObject, Sel, *mut AnyObject) -> usize> = None;
static mut ORIGINAL_DRAGGING_UPDATED: Option<unsafe extern "C" fn(*mut AnyObject, Sel, *mut AnyObject) -> usize> = None;
static mut ORIGINAL_DRAGGING_EXITED: Option<unsafe extern "C" fn(*mut AnyObject, Sel, *mut AnyObject)> = None;
static mut ORIGINAL_PERFORM_DRAG: Option<unsafe extern "C" fn(*mut AnyObject, Sel, *mut AnyObject) -> Bool> = None;

static SWIZZLE_ONCE: Once = Once::new();

/// Set up drag-drop handling for a webview window
pub fn setup_drag_handlers(window: &WebviewWindow) {
    eprintln!(
        "[TiddlyDesktop] macOS: setup_drag_handlers called for window '{}'",
        window.label()
    );

    let label = window.label().to_string();
    let state = Arc::new(Mutex::new(DragState {
        window: window.clone(),
        drag_active: false,
        last_position: None,
    }));

    DRAG_STATES
        .lock()
        .unwrap()
        .insert(label.clone(), state.clone());

    let window_clone = window.clone();
    let label_clone = label.clone();

    // Use run_on_main_thread to safely access Cocoa APIs
    let _ = window.run_on_main_thread(move || {
        setup_drag_destination(&window_clone, &label_clone);
    });
}

fn setup_drag_destination(window: &WebviewWindow, label: &str) {
    // Get the NSWindow
    if let Ok(ns_window) = window.ns_window() {
        let ns_window_ptr = ns_window as *mut c_void as *mut AnyObject;

        unsafe {
            let ns_window: &NSWindow = &*(ns_window_ptr as *const NSWindow);

            if let Some(content_view) = ns_window.contentView() {
                if let Some(webview) = find_wkwebview(&content_view) {
                    eprintln!("[TiddlyDesktop] macOS: Found WKWebView, setting up drag handling");

                    // Store mapping from webview pointer to window label
                    let webview_ptr = &*webview as *const NSView as usize;
                    WEBVIEW_TO_LABEL.lock().unwrap().insert(webview_ptr, label.to_string());

                    // Register for drag types
                    let drag_types = create_drag_types_array();
                    let _: () = msg_send![&webview, registerForDraggedTypes: &*drag_types];

                    // Swizzle methods (only once globally)
                    swizzle_drag_methods(&webview);

                    eprintln!(
                        "[TiddlyDesktop] macOS: Drag handling set up for window '{}'",
                        label
                    );
                } else {
                    eprintln!("[TiddlyDesktop] macOS: WKWebView not found in view hierarchy");
                }
            } else {
                eprintln!("[TiddlyDesktop] macOS: Failed to get content view");
            }
        }
    } else {
        eprintln!("[TiddlyDesktop] macOS: Failed to get NSWindow");
    }
}

/// Swizzle NSDraggingDestination methods on the WKWebView class
unsafe fn swizzle_drag_methods(webview: &NSView) {
    SWIZZLE_ONCE.call_once(|| {
        let class: *const AnyClass = msg_send![webview, class];
        if class.is_null() {
            eprintln!("[TiddlyDesktop] macOS: Failed to get WKWebView class");
            return;
        }

        eprintln!("[TiddlyDesktop] macOS: Swizzling drag methods on class");

        // Swizzle draggingEntered:
        swizzle_method(
            class as *mut AnyClass,
            sel!(draggingEntered:),
            swizzled_dragging_entered as *mut c_void,
            &raw mut ORIGINAL_DRAGGING_ENTERED,
        );

        // Swizzle draggingUpdated:
        swizzle_method(
            class as *mut AnyClass,
            sel!(draggingUpdated:),
            swizzled_dragging_updated as *mut c_void,
            &raw mut ORIGINAL_DRAGGING_UPDATED,
        );

        // Swizzle draggingExited:
        swizzle_method(
            class as *mut AnyClass,
            sel!(draggingExited:),
            swizzled_dragging_exited as *mut c_void,
            &raw mut ORIGINAL_DRAGGING_EXITED,
        );

        // Swizzle performDragOperation:
        swizzle_method(
            class as *mut AnyClass,
            sel!(performDragOperation:),
            swizzled_perform_drag_operation as *mut c_void,
            &raw mut ORIGINAL_PERFORM_DRAG,
        );

        eprintln!("[TiddlyDesktop] macOS: Method swizzling complete");
    });
}

/// Helper to swizzle a single method
unsafe fn swizzle_method<F>(
    class: *mut AnyClass,
    selector: Sel,
    new_impl: *mut c_void,
    original_storage: *mut Option<F>,
) {
    // Use libc to call the Objective-C runtime functions directly
    extern "C" {
        fn class_getInstanceMethod(cls: *const AnyClass, sel: Sel) -> *mut c_void;
        fn method_setImplementation(method: *mut c_void, imp: *mut c_void) -> *mut c_void;
    }

    let method = class_getInstanceMethod(class as *const AnyClass, selector);
    if method.is_null() {
        eprintln!("[TiddlyDesktop] macOS: Method {:?} not found, skipping swizzle", selector);
        return;
    }

    let original_impl = method_setImplementation(method, new_impl);
    if !original_impl.is_null() {
        *original_storage = Some(std::mem::transmute_copy(&original_impl));
        eprintln!("[TiddlyDesktop] macOS: Swizzled {:?}", selector);
    }
}

/// Get window label from webview pointer
fn get_window_label(webview: *mut AnyObject) -> Option<String> {
    let ptr = webview as usize;
    WEBVIEW_TO_LABEL.lock().unwrap().get(&ptr).cloned()
}

/// Get dragging location from NSDraggingInfo
unsafe fn get_dragging_location(dragging_info: *mut AnyObject, webview: *mut AnyObject) -> (f64, f64) {
    // Get location in window coordinates
    let location: NSPoint = msg_send![dragging_info, draggingLocation];

    // Convert to view coordinates
    let view_location: NSPoint = msg_send![webview, convertPoint: location, fromView: std::ptr::null::<AnyObject>()];

    // Get view bounds to flip Y coordinate (Cocoa uses bottom-left origin, web uses top-left)
    let bounds: objc2_foundation::NSRect = msg_send![webview, bounds];
    let flipped_y = bounds.size.height - view_location.y;

    (view_location.x, flipped_y)
}

/// Get pasteboard from NSDraggingInfo
unsafe fn get_dragging_pasteboard(dragging_info: *mut AnyObject) -> Option<Retained<NSPasteboard>> {
    let pasteboard: *mut AnyObject = msg_send![dragging_info, draggingPasteboard];
    if pasteboard.is_null() {
        return None;
    }
    Some(Retained::retain(pasteboard as *mut NSPasteboard).unwrap())
}

// ============================================================================
// Swizzled method implementations
// ============================================================================

/// Check if there's an active outgoing drag from this specific window (same-window drag)
fn is_same_window_drag(window_label: &str) -> bool {
    if let Ok(guard) = outgoing_drag_state().lock() {
        if let Some(state) = guard.as_ref() {
            return state.source_window_label == window_label;
        }
    }
    false
}

/// Check if there's any active outgoing drag from our app (cross-wiki or same-window)
fn is_any_outgoing_drag() -> bool {
    if let Ok(guard) = outgoing_drag_state().lock() {
        return guard.is_some();
    }
    false
}

/// Get the source window label if there's an active outgoing drag
fn get_source_window_label() -> Option<String> {
    if let Ok(guard) = outgoing_drag_state().lock() {
        if let Some(state) = guard.as_ref() {
            return Some(state.source_window_label.clone());
        }
    }
    None
}

/// Check if the outgoing drag has tiddler data
fn has_tiddler_data() -> bool {
    if let Ok(guard) = outgoing_drag_state().lock() {
        if let Some(state) = guard.as_ref() {
            return state.data.text_vnd_tiddler.is_some();
        }
    }
    false
}

/// Check if the outgoing drag is a text selection drag
fn is_text_selection_drag_flag() -> bool {
    if let Ok(guard) = outgoing_drag_state().lock() {
        if let Some(state) = guard.as_ref() {
            return state.data.is_text_selection_drag;
        }
    }
    false
}

/// Swizzled draggingEntered: - native event when drag enters the view
extern "C" fn swizzled_dragging_entered(this: *mut AnyObject, _sel: Sel, dragging_info: *mut AnyObject) -> usize {
    unsafe {
        if let Some(label) = get_window_label(this) {
            let (x, y) = get_dragging_location(dragging_info, this);

            // Check if this drag is from our app (any window) - for cross-wiki detection
            let is_our_drag = is_any_outgoing_drag();
            let source_window_label = get_source_window_label();
            let is_same_window = is_same_window_drag(&label);
            let has_tiddler = has_tiddler_data();
            let is_text_sel = is_text_selection_drag_flag();

            eprintln!(
                "[TiddlyDesktop] macOS: draggingEntered, isOurDrag={}, sourceWindow={:?}, isSameWindow={}",
                is_our_drag, source_window_label, is_same_window
            );

            if let Some(state) = DRAG_STATES.lock().unwrap().get(&label) {
                let mut state = state.lock().unwrap();
                state.drag_active = true;
                state.last_position = Some((x, y));

                // Emit td-drag-motion with full context for cross-wiki support
                // Include hasTiddlerData and isTextSelectionDrag for Issue 4b handling in JS
                let _ = state.window.emit(
                    "td-drag-motion",
                    serde_json::json!({
                        "x": x,
                        "y": y,
                        "screenCoords": false,
                        "isOurDrag": is_our_drag,
                        "isSameWindow": is_same_window,
                        "sourceWindowLabel": source_window_label,
                        "windowLabel": label,
                        "hasTiddlerData": has_tiddler,
                        "isTextSelectionDrag": is_text_sel
                    }),
                );
            }
        }

        // Call original or return copy operation
        if let Some(original) = ORIGINAL_DRAGGING_ENTERED {
            original(this, _sel, dragging_info)
        } else {
            NS_DRAG_OPERATION_COPY | NS_DRAG_OPERATION_GENERIC
        }
    }
}

/// Swizzled draggingUpdated: - native event when drag moves over the view
extern "C" fn swizzled_dragging_updated(this: *mut AnyObject, _sel: Sel, dragging_info: *mut AnyObject) -> usize {
    unsafe {
        if let Some(label) = get_window_label(this) {
            let (x, y) = get_dragging_location(dragging_info, this);

            // Check if this drag is from our app (any window) - for cross-wiki detection
            let is_our_drag = is_any_outgoing_drag();
            let source_window_label = get_source_window_label();
            let is_same_window = is_same_window_drag(&label);
            let has_tiddler = has_tiddler_data();
            let is_text_sel = is_text_selection_drag_flag();

            if let Some(state) = DRAG_STATES.lock().unwrap().get(&label) {
                let mut state = state.lock().unwrap();
                state.last_position = Some((x, y));

                // Emit td-drag-motion with full context for cross-wiki support
                // Include hasTiddlerData and isTextSelectionDrag for Issue 4b handling in JS
                let _ = state.window.emit(
                    "td-drag-motion",
                    serde_json::json!({
                        "x": x,
                        "y": y,
                        "screenCoords": false,
                        "isOurDrag": is_our_drag,
                        "isSameWindow": is_same_window,
                        "sourceWindowLabel": source_window_label,
                        "windowLabel": label,
                        "hasTiddlerData": has_tiddler,
                        "isTextSelectionDrag": is_text_sel
                    }),
                );
            }
        }

        // Call original or return copy operation
        if let Some(original) = ORIGINAL_DRAGGING_UPDATED {
            original(this, _sel, dragging_info)
        } else {
            NS_DRAG_OPERATION_COPY | NS_DRAG_OPERATION_GENERIC
        }
    }
}

/// Swizzled draggingExited: - native event when drag leaves the view
extern "C" fn swizzled_dragging_exited(this: *mut AnyObject, _sel: Sel, dragging_info: *mut AnyObject) {
    unsafe {
        if let Some(label) = get_window_label(this) {
            // Check if this drag is from our app (any window) - for cross-wiki detection
            let is_our_drag = is_any_outgoing_drag();
            let source_window_label = get_source_window_label();
            let is_same_window = is_same_window_drag(&label);

            eprintln!(
                "[TiddlyDesktop] macOS: draggingExited, isOurDrag={}, sourceWindow={:?}, isSameWindow={}",
                is_our_drag, source_window_label, is_same_window
            );

            if let Some(state) = DRAG_STATES.lock().unwrap().get(&label) {
                let mut state = state.lock().unwrap();
                state.drag_active = false;
                state.last_position = None;

                let _ = state.window.emit("td-drag-leave", serde_json::json!({
                    "isOurDrag": is_our_drag,
                    "isSameWindow": is_same_window,
                    "sourceWindowLabel": source_window_label,
                    "windowLabel": label
                }));
            }
        }

        // Call original
        if let Some(original) = ORIGINAL_DRAGGING_EXITED {
            original(this, _sel, dragging_info);
        }
    }
}

/// Swizzled performDragOperation: - native event when drop occurs
extern "C" fn swizzled_perform_drag_operation(this: *mut AnyObject, _sel: Sel, dragging_info: *mut AnyObject) -> Bool {
    unsafe {
        let mut handled = false;

        if let Some(label) = get_window_label(this) {
            let (x, y) = get_dragging_location(dragging_info, this);

            // Check if this drag is from our app (any window) - for cross-wiki detection
            let is_our_drag = is_any_outgoing_drag();
            let source_window_label = get_source_window_label();
            let is_same_window = is_same_window_drag(&label);

            eprintln!(
                "[TiddlyDesktop] macOS: performDragOperation, isOurDrag={}, sourceWindow={:?}, isSameWindow={}",
                is_our_drag, source_window_label, is_same_window
            );

            // For same-window drags, first let the browser try to handle it natively.
            // This is critical for editable elements (input/textarea/contenteditable) which
            // need browser-trusted drop events to work correctly.
            if is_same_window {
                eprintln!("[TiddlyDesktop] macOS: performDragOperation - same-window drag, trying native handler first");

                // Call the original browser handler first
                let browser_handled = if let Some(original) = ORIGINAL_PERFORM_DRAG {
                    let result = original(this, _sel, dragging_info);
                    result == Bool::YES
                } else {
                    false
                };

                if browser_handled {
                    // Browser handled it (e.g., drop on editable element)
                    eprintln!("[TiddlyDesktop] macOS: performDragOperation - browser handled same-window drop natively");
                    handled = true;
                } else {
                    // Browser didn't handle it - emit td-drag-content for $droppable handlers etc.
                    eprintln!("[TiddlyDesktop] macOS: performDragOperation - browser didn't handle, emitting td-drag-content");
                    if let Some(state) = DRAG_STATES.lock().unwrap().get(&label) {
                        let state = state.lock().unwrap();
                        let _ = state.window.emit(
                            "td-drag-drop-position",
                            serde_json::json!({
                                "x": x,
                                "y": y,
                                "screenCoords": false,
                                "isOurDrag": true,
                                "isSameWindow": true,
                                "sourceWindowLabel": source_window_label,
                                "windowLabel": label
                            }),
                        );

                        // Get the stored drag data and emit td-drag-content so JS can process the drop
                        if let Ok(guard) = outgoing_drag_state().lock() {
                            if let Some(outgoing) = guard.as_ref() {
                                let mut types = Vec::new();
                                let mut data = std::collections::HashMap::new();

                                // Include text/vnd.tiddler for $droppable handlers
                                if let Some(ref tiddler) = outgoing.data.text_vnd_tiddler {
                                    types.push("text/vnd.tiddler".to_string());
                                    data.insert("text/vnd.tiddler".to_string(), tiddler.clone());
                                }
                                if let Some(ref text) = outgoing.data.text_plain {
                                    types.push("text/plain".to_string());
                                    data.insert("text/plain".to_string(), text.clone());
                                }

                                if !types.is_empty() {
                                    eprintln!(
                                        "[TiddlyDesktop] macOS: performDragOperation - emitting td-drag-content for same-window drag, types: {:?}",
                                        types
                                    );
                                    let content_data = DragContentData {
                                        types,
                                        data,
                                        is_text_selection_drag: if outgoing.data.is_text_selection_drag { Some(true) } else { None },
                                        is_same_window: Some(true)
                                    };
                                    let _ = state.window.emit("td-drag-content", &content_data);
                                }
                            }
                        }
                    }
                    handled = true;
                }
            } else if let Some(pasteboard) = get_dragging_pasteboard(dragging_info) {
                // Check for file drops first
                let file_paths = extract_file_paths(&pasteboard);

                if !file_paths.is_empty() {
                    // External file drop - WRY patch stores paths via FFI
                    // Let native WKWebView handling fire HTML5 drop events
                    // JavaScript retrieves paths from FFI after native drop event fires
                    eprintln!("[TiddlyDesktop] macOS: External file drop detected, delegating to WRY patch");
                    // DON'T set handled = true - let WRY patch and native handling fire
                    // handled remains false, so we call original handler below
                } else if let Some(mut content) = extract_pasteboard_content(&pasteboard) {
                    // For cross-wiki drags from our app, propagate the is_text_selection_drag flag
                    // This allows JS to filter text/html for text-selection drags (Issue 3)
                    if is_our_drag {
                        if let Ok(guard) = outgoing_drag_state().lock() {
                            if let Some(outgoing) = guard.as_ref() {
                                if outgoing.data.is_text_selection_drag {
                                    content.is_text_selection_drag = Some(true);
                                }
                            }
                        }
                    }
                    eprintln!("[TiddlyDesktop] macOS: Content drop with types: {:?}", content.types);
                    emit_drop_with_content(&label, x, y, content);
                    handled = true;
                }
            }

            // Reset state
            if let Some(state) = DRAG_STATES.lock().unwrap().get(&label) {
                let mut state = state.lock().unwrap();
                state.drag_active = false;
                state.last_position = None;
            }
        }

        // If we handled it, return true; otherwise call original
        if handled {
            Bool::YES
        } else if let Some(original) = ORIGINAL_PERFORM_DRAG {
            original(this, _sel, dragging_info)
        } else {
            Bool::YES
        }
    }
}

// ============================================================================
// Helper functions
// ============================================================================

/// Find WKWebView in the view hierarchy
fn find_wkwebview(view: &NSView) -> Option<Retained<NSView>> {
    unsafe {
        let class_name = get_class_name(view);
        if class_name.contains("WKWebView") || class_name.contains("WryWebView") {
            return Some(Retained::retain(view as *const NSView as *mut NSView).unwrap());
        }

        let subviews = view.subviews();
        for subview in subviews.iter() {
            if let Some(found) = find_wkwebview(&subview) {
                return Some(found);
            }
        }

        None
    }
}

/// Get the class name of an Objective-C object
fn get_class_name(obj: &NSView) -> String {
    unsafe {
        let class: *const AnyObject = msg_send![obj, class];
        let name: Retained<NSString> = msg_send![class, className];
        name.to_string()
    }
}

/// Create an NSArray of pasteboard types we accept
fn create_drag_types_array() -> Retained<NSArray<NSString>> {
    let types = vec![
        // Browser custom data formats - highest priority (may contain text/vnd.tiddler)
        // Note: Safari uses standard pasteboard types, no custom format needed
        NSString::from_str(NS_PASTEBOARD_TYPE_MOZ_CUSTOM),
        NSString::from_str(NS_PASTEBOARD_TYPE_CHROME_CUSTOM),
        // TiddlyWiki tiddler JSON
        NSString::from_str(NS_PASTEBOARD_TYPE_TIDDLER),
        NSString::from_str(NS_PASTEBOARD_TYPE_JSON),
        NSString::from_str(NS_PASTEBOARD_TYPE_STRING),
        NSString::from_str(NS_PASTEBOARD_TYPE_HTML),
        NSString::from_str(NS_PASTEBOARD_TYPE_URL),
        NSString::from_str(NS_PASTEBOARD_TYPE_FILE_URL),
        NSString::from_str("public.text"),
        NSString::from_str("NSStringPboardType"),
        NSString::from_str("NSHTMLPboardType"),
        NSString::from_str("NSURLPboardType"),
        NSString::from_str("NSFilenamesPboardType"),
    ];

    NSArray::from_retained_slice(&types)
}

/// Extract bytes from NSData
fn nsdata_to_vec(data: &Retained<NSData>) -> Vec<u8> {
    unsafe {
        let obj = &**data as *const NSData as *const AnyObject;
        let len: usize = msg_send![obj, length];
        if len == 0 {
            return Vec::new();
        }
        let ptr: *const u8 = msg_send![obj, bytes];
        if ptr.is_null() {
            return Vec::new();
        }
        std::slice::from_raw_parts(ptr, len).to_vec()
    }
}

/// Decode UTF-16LE bytes to a String
fn decode_utf16le(data: &[u8]) -> String {
    if data.len() < 2 {
        return String::new();
    }
    let u16_vec: Vec<u16> = data
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();
    String::from_utf16_lossy(&u16_vec)
}

/// Parse Mozilla's custom clipdata format
fn parse_moz_custom_clipdata(data: &[u8]) -> Option<HashMap<String, String>> {
    if data.len() < 8 {
        return None;
    }

    let mut result = HashMap::new();
    let mut offset = 0;

    let num_entries = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    offset += 4;

    for _ in 0..num_entries {
        if offset + 4 > data.len() { break; }

        let mime_len = u32::from_be_bytes([
            data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
        ]) as usize;
        offset += 4;

        if offset + mime_len > data.len() { break; }
        let mime_type = decode_utf16le(&data[offset..offset + mime_len]);
        offset += mime_len;

        if offset + 4 > data.len() { break; }
        let content_len = u32::from_be_bytes([
            data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
        ]) as usize;
        offset += 4;

        if offset + content_len > data.len() {
            let content = decode_utf16le(&data[offset..]);
            if !mime_type.is_empty() && !content.is_empty() {
                result.insert(mime_type, content);
            }
            break;
        }

        let content = decode_utf16le(&data[offset..offset + content_len]);
        offset += content_len;

        if !mime_type.is_empty() {
            result.insert(mime_type, content);
        }
    }

    if result.is_empty() { None } else { Some(result) }
}

/// Parse Chrome's custom data format (Pickle)
fn parse_chromium_custom_data(data: &[u8]) -> Option<HashMap<String, String>> {
    if data.len() < 12 {
        return None;
    }

    let mut result = HashMap::new();
    let mut offset = 4; // Skip payload size

    let num_entries = u64::from_le_bytes([
        data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
        data[offset + 4], data[offset + 5], data[offset + 6], data[offset + 7],
    ]) as usize;
    offset += 8;

    for _ in 0..num_entries {
        if offset + 4 > data.len() { break; }

        let mime_char_len = u32::from_le_bytes([
            data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
        ]) as usize;
        offset += 4;

        let mime_byte_len = mime_char_len * 2;
        if offset + mime_byte_len > data.len() { break; }

        let mime_type = decode_utf16le(&data[offset..offset + mime_byte_len]);
        offset += mime_byte_len;
        offset += (4 - (mime_byte_len % 4)) % 4;

        if offset + 4 > data.len() { break; }

        let content_char_len = u32::from_le_bytes([
            data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
        ]) as usize;
        offset += 4;

        let content_byte_len = content_char_len * 2;
        if offset + content_byte_len > data.len() {
            let content = decode_utf16le(&data[offset..]);
            if !mime_type.is_empty() && !content.is_empty() {
                result.insert(mime_type, content);
            }
            break;
        }

        let content = decode_utf16le(&data[offset..offset + content_byte_len]);
        offset += content_byte_len;
        offset += (4 - (content_byte_len % 4)) % 4;

        if !mime_type.is_empty() {
            result.insert(mime_type, content);
        }
    }

    if result.is_empty() { None } else { Some(result) }
}

/// Extract content from a drag pasteboard
fn extract_pasteboard_content(pasteboard: &NSPasteboard) -> Option<DragContentData> {
    let mut types = Vec::new();
    let mut data = HashMap::new();

    // 0a. Try Mozilla custom clipdata format first
    let moz_type = NSString::from_str(NS_PASTEBOARD_TYPE_MOZ_CUSTOM);
    if let Some(moz_data) = pasteboard.dataForType(&moz_type) {
        let bytes = nsdata_to_vec(&moz_data);
        if let Some(moz_entries) = parse_moz_custom_clipdata(&bytes) {
            eprintln!("[TiddlyDesktop] macOS: Parsed Mozilla custom clipdata, found {} entries", moz_entries.len());
            if let Some(tiddler_json) = moz_entries.get("text/vnd.tiddler") {
                eprintln!("[TiddlyDesktop] macOS: Found text/vnd.tiddler in Mozilla clipdata!");
                types.push("text/vnd.tiddler".to_string());
                data.insert("text/vnd.tiddler".to_string(), tiddler_json.clone());
            }
            for (mime_type, content) in moz_entries {
                if !data.contains_key(&mime_type) {
                    types.push(mime_type.clone());
                    data.insert(mime_type, content);
                }
            }
        }
    }

    // 0b. Try Chrome custom data format
    if !data.contains_key("text/vnd.tiddler") {
        let chrome_type = NSString::from_str(NS_PASTEBOARD_TYPE_CHROME_CUSTOM);
        if let Some(chrome_data) = pasteboard.dataForType(&chrome_type) {
            let bytes = nsdata_to_vec(&chrome_data);
            if let Some(chrome_entries) = parse_chromium_custom_data(&bytes) {
                eprintln!("[TiddlyDesktop] macOS: Parsed Chrome custom clipdata, found {} entries", chrome_entries.len());
                if let Some(tiddler_json) = chrome_entries.get("text/vnd.tiddler") {
                    eprintln!("[TiddlyDesktop] macOS: Found text/vnd.tiddler in Chrome clipdata!");
                    types.push("text/vnd.tiddler".to_string());
                    data.insert("text/vnd.tiddler".to_string(), tiddler_json.clone());
                }
                for (mime_type, content) in chrome_entries {
                    if !data.contains_key(&mime_type) {
                        types.push(mime_type.clone());
                        data.insert(mime_type, content);
                    }
                }
            }
        }
    }

    // 1. Try to get TiddlyWiki tiddler JSON
    if !data.contains_key("text/vnd.tiddler") {
        let tiddler_type = NSString::from_str(NS_PASTEBOARD_TYPE_TIDDLER);
        if let Some(tiddler) = pasteboard.stringForType(&tiddler_type) {
            let tiddler_str = tiddler.to_string();
            if !tiddler_str.is_empty() {
                eprintln!("[TiddlyDesktop] macOS: Got tiddler data!");
                types.push("text/vnd.tiddler".to_string());
                data.insert("text/vnd.tiddler".to_string(), tiddler_str);
                // Don't also add as text/plain - that would cause duplicate imports
            }
        }
    }

    // 2. Try to get JSON
    let json_type = NSString::from_str(NS_PASTEBOARD_TYPE_JSON);
    if let Some(json) = pasteboard.stringForType(&json_type) {
        let json_str = json.to_string();
        if !json_str.is_empty() && !data.contains_key("application/json") {
            types.push("application/json".to_string());
            data.insert("application/json".to_string(), json_str);
        }
    }

    // 3. Try to get plain text
    // Skip if text/vnd.tiddler is already present to avoid duplicate data
    // This fixes Issue 1 & 2: external drops from browsers include both tiddler and plain text
    if !data.contains_key("text/vnd.tiddler") {
        let text_type = NSString::from_str(NS_PASTEBOARD_TYPE_STRING);
        if let Some(text) = pasteboard.stringForType(&text_type) {
            let text_str = text.to_string();
            if !text_str.is_empty() && !data.contains_key("text/plain") {
                // Check if plain text looks like tiddler JSON
                let looks_like_tiddler_json = text_str.trim_start().starts_with('[')
                    && text_str.contains("\"title\"")
                    && (text_str.contains("\"text\"") || text_str.contains("\"fields\""));

                if looks_like_tiddler_json {
                    eprintln!("[TiddlyDesktop] macOS: Detected tiddler JSON in plain text!");
                    types.push("text/vnd.tiddler".to_string());
                    data.insert("text/vnd.tiddler".to_string(), text_str.clone());
                    // Don't also add as text/plain - that would cause duplicate imports
                } else {
                    types.push("text/plain".to_string());
                    data.insert("text/plain".to_string(), text_str.clone());

                    // Check if it's a URL
                    if text_str.starts_with("http://") || text_str.starts_with("https://") {
                        // Security: Block dangerous URL schemes
                        if !is_dangerous_url(&text_str) {
                            types.push("text/uri-list".to_string());
                            data.insert("text/uri-list".to_string(), text_str.clone());
                            types.push("URL".to_string());
                            data.insert("URL".to_string(), text_str);
                        }
                    }
                }
            }
        }
    }

    // 4. Try to get HTML
    let html_type = NSString::from_str(NS_PASTEBOARD_TYPE_HTML);
    if let Some(html) = pasteboard.stringForType(&html_type) {
        let html_str = html.to_string();
        if !html_str.is_empty() && !data.contains_key("text/html") {
            // Security: Sanitize HTML content
            let sanitized_html = sanitize_html(&html_str);
            types.push("text/html".to_string());
            data.insert("text/html".to_string(), sanitized_html);
        }
    }

    // 5. Try to get URL
    let url_type = NSString::from_str(NS_PASTEBOARD_TYPE_URL);
    if let Some(url) = pasteboard.stringForType(&url_type) {
        let url_str = url.to_string();
        // Security: Block dangerous URL schemes
        if !url_str.is_empty() && !data.contains_key("URL") && !is_dangerous_url(&url_str) {
            types.push("URL".to_string());
            data.insert("URL".to_string(), url_str);
        }
    }

    if types.is_empty() {
        None
    } else {
        Some(DragContentData { types, data, is_text_selection_drag: None, is_same_window: None })
    }
}

/// Extract file paths from a drag pasteboard
fn extract_file_paths(pasteboard: &NSPasteboard) -> Vec<String> {
    let mut paths = Vec::new();

    // Try public.file-url
    let file_url_type = NSString::from_str(NS_PASTEBOARD_TYPE_FILE_URL);
    if let Some(file_url) = pasteboard.stringForType(&file_url_type) {
        let url_str = file_url.to_string();
        if url_str.starts_with("file://") {
            if let Some(path) = url_str.strip_prefix("file://") {
                if let Ok(decoded) = urlencoding::decode(path) {
                    paths.push(decoded.into_owned());
                }
            }
        }
    }

    // Also try NSFilenamesPboardType
    if paths.is_empty() {
        let filenames_type = NSString::from_str("NSFilenamesPboardType");
        if let Some(filenames) = pasteboard.propertyListForType(&filenames_type) {
            unsafe {
                let filenames_ptr: *const AnyObject = &*filenames;
                let array: &NSArray<NSString> = &*(filenames_ptr as *const NSArray<NSString>);
                for item in array.iter() {
                    let path_str: String = item.to_string();
                    if !path_str.is_empty() {
                        paths.push(path_str);
                    }
                }
            }
        }
    }

    paths
}

/// Emit drop events with content
fn emit_drop_with_content(window_label: &str, x: f64, y: f64, content: DragContentData) {
    if let Some(state) = DRAG_STATES.lock().unwrap().get(window_label) {
        let state = state.lock().unwrap();

        let _ = state.window.emit(
            "td-drag-drop-start",
            serde_json::json!({
                "x": x,
                "y": y,
                "screenCoords": false
            }),
        );

        let _ = state.window.emit(
            "td-drag-drop-position",
            serde_json::json!({
                "x": x,
                "y": y,
                "screenCoords": false
            }),
        );

        let _ = state.window.emit("td-drag-content", &content);
    }
}

// ============================================================================
// Outgoing drag support (TiddlyWiki → external apps)
// ============================================================================

/// Data to be provided during an outgoing drag operation
#[derive(Clone, Debug, Default)]
#[allow(dead_code)]
pub struct OutgoingDragData {
    pub text_plain: Option<String>,
    pub text_html: Option<String>,
    pub text_vnd_tiddler: Option<String>,
    pub text_uri_list: Option<String>,
    pub text_x_moz_url: Option<String>,
    pub url: Option<String>,
    /// True if this is a text-selection drag (not used on macOS currently)
    pub is_text_selection_drag: bool,
}

/// State for tracking outgoing drag operations
#[allow(dead_code)]
struct OutgoingDragState {
    data: OutgoingDragData,
    source_window_label: String,
    data_was_requested: bool,
}

fn outgoing_drag_state() -> &'static Mutex<Option<OutgoingDragState>> {
    static INSTANCE: OnceLock<Mutex<Option<OutgoingDragState>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(None))
}

/// Original NSDraggingSource methods (stored after first swizzle)
static mut ORIGINAL_SOURCE_OPERATION_MASK: Option<unsafe extern "C" fn(*mut AnyObject, Sel, *mut AnyObject, isize) -> usize> = None;
static mut ORIGINAL_DRAGGING_SESSION_MOVED: Option<unsafe extern "C" fn(*mut AnyObject, Sel, *mut AnyObject, NSPoint)> = None;
static mut ORIGINAL_DRAGGING_SESSION_ENDED: Option<unsafe extern "C" fn(*mut AnyObject, Sel, *mut AnyObject, NSPoint, usize)> = None;

static SOURCE_SWIZZLE_ONCE: Once = Once::new();

/// Swizzle NSDraggingSource methods for outgoing drag tracking
unsafe fn swizzle_drag_source_methods(webview: &NSView) {
    SOURCE_SWIZZLE_ONCE.call_once(|| {
        let class: *const AnyClass = msg_send![webview, class];
        if class.is_null() {
            eprintln!("[TiddlyDesktop] macOS: Failed to get WKWebView class for source swizzle");
            return;
        }

        eprintln!("[TiddlyDesktop] macOS: Swizzling NSDraggingSource methods");

        // Swizzle draggingSession:sourceOperationMaskForDraggingContext:
        swizzle_method(
            class as *mut AnyClass,
            sel!(draggingSession:sourceOperationMaskForDraggingContext:),
            swizzled_source_operation_mask as *mut c_void,
            &raw mut ORIGINAL_SOURCE_OPERATION_MASK,
        );

        // Swizzle draggingSession:movedToPoint:
        swizzle_method(
            class as *mut AnyClass,
            sel!(draggingSession:movedToPoint:),
            swizzled_dragging_session_moved as *mut c_void,
            &raw mut ORIGINAL_DRAGGING_SESSION_MOVED,
        );

        // Swizzle draggingSession:endedAtPoint:operation:
        swizzle_method(
            class as *mut AnyClass,
            sel!(draggingSession:endedAtPoint:operation:),
            swizzled_dragging_session_ended as *mut c_void,
            &raw mut ORIGINAL_DRAGGING_SESSION_ENDED,
        );

        eprintln!("[TiddlyDesktop] macOS: NSDraggingSource swizzling complete");
    });
}

/// NSDragOperation constants for source
const NS_DRAG_OPERATION_COPY_MOVE_LINK: usize = 1 | 16 | 2; // Copy | Move | Link

/// Swizzled draggingSession:sourceOperationMaskForDraggingContext:
extern "C" fn swizzled_source_operation_mask(
    this: *mut AnyObject,
    _sel: Sel,
    session: *mut AnyObject,
    context: isize,
) -> usize {
    unsafe {
        // Check if we have outgoing drag data
        let has_outgoing = outgoing_drag_state()
            .lock()
            .map(|g| g.is_some())
            .unwrap_or(false);

        if has_outgoing {
            // For our drags, allow all operations
            return NS_DRAG_OPERATION_COPY_MOVE_LINK;
        }

        // Call original for other drags
        if let Some(original) = ORIGINAL_SOURCE_OPERATION_MASK {
            original(this, _sel, session, context)
        } else {
            NS_DRAG_OPERATION_COPY_MOVE_LINK
        }
    }
}

/// Swizzled draggingSession:movedToPoint:
extern "C" fn swizzled_dragging_session_moved(
    this: *mut AnyObject,
    _sel: Sel,
    session: *mut AnyObject,
    screen_point: NSPoint,
) {
    unsafe {
        // No tracking needed here - NSDraggingDestination on each window handles enter/leave/motion natively
        // This matches the Linux approach where GTK signals handle all drag events

        // Call original
        if let Some(original) = ORIGINAL_DRAGGING_SESSION_MOVED {
            original(this, _sel, session, screen_point);
        }
    }
}

/// Swizzled draggingSession:endedAtPoint:operation:
extern "C" fn swizzled_dragging_session_ended(
    this: *mut AnyObject,
    _sel: Sel,
    session: *mut AnyObject,
    screen_point: NSPoint,
    operation: usize,
) {
    unsafe {
        // Check if this is our outgoing drag
        let state_info = outgoing_drag_state().lock().ok().and_then(|guard| {
            guard.as_ref().map(|s| (s.source_window_label.clone(), s.data_was_requested))
        });

        if let Some((window_label, data_was_requested)) = state_info {
            if let Some(label) = get_window_label(this) {
                if label == window_label {
                    eprintln!(
                        "[TiddlyDesktop] macOS: Drag ended at ({}, {}), operation: {}, data_was_requested: {}",
                        screen_point.x, screen_point.y, operation, data_was_requested
                    );

                    // Emit drag end event
                    if let Some(drag_state) = DRAG_STATES.lock().unwrap().get(&label) {
                        let ds = drag_state.lock().unwrap();
                        let _ = ds.window.emit(
                            "td-drag-end",
                            serde_json::json!({
                                "data_was_requested": operation != NS_DRAG_OPERATION_NONE || data_was_requested
                            }),
                        );
                    }

                    // Clear outgoing drag state
                    if let Ok(mut guard) = outgoing_drag_state().lock() {
                        *guard = None;
                    }
                }
            }
        }

        // Call original
        if let Some(original) = ORIGINAL_DRAGGING_SESSION_ENDED {
            original(this, _sel, session, screen_point, operation);
        }
    }
}

/// Apply opacity to an NSImage by drawing it into a new image with reduced alpha
/// Returns a new NSImage with the opacity applied, or null on failure
unsafe fn apply_opacity_to_nsimage(image: *mut AnyObject, size: NSSize, opacity: f64) -> *mut AnyObject {
    // Create a new NSImage with the same size
    let new_image_class: *const AnyObject = msg_send![objc2::class!(NSImage), alloc];
    if new_image_class.is_null() {
        return std::ptr::null_mut();
    }

    let new_image: *mut AnyObject = msg_send![new_image_class, initWithSize: size];
    if new_image.is_null() {
        return std::ptr::null_mut();
    }

    // Lock focus on the new image to draw into it
    let locked: Bool = msg_send![new_image, lockFocus];
    if !locked.as_bool() {
        eprintln!("[TiddlyDesktop] macOS: Failed to lock focus on new image");
        return std::ptr::null_mut();
    }

    // Draw the original image with reduced opacity
    // drawInRect:fromRect:operation:fraction: draws with the specified alpha (fraction)
    let dest_rect = NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size,
    };
    let zero_rect = NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size: NSSize { width: 0.0, height: 0.0 },
    };

    // NSCompositingOperationSourceOver = 2
    let _: () = msg_send![
        image,
        drawInRect: dest_rect,
        fromRect: zero_rect,
        operation: 2u64,  // NSCompositingOperationSourceOver
        fraction: opacity
    ];

    // Unlock focus
    let _: () = msg_send![new_image, unlockFocus];

    new_image
}

/// Prepare for a potential native drag (called when internal drag starts)
/// This sets the outgoing drag state so that performDragOperation can detect same-window drags
/// and avoid emitting td-drag-content events that would trigger imports.
pub fn prepare_native_drag(window: &WebviewWindow, data: OutgoingDragData) -> Result<(), String> {
    let label = window.label().to_string();
    eprintln!(
        "[TiddlyDesktop] macOS: prepare_native_drag called for window '{}'",
        label
    );

    // Store drag state so performDragOperation can detect same-window drags
    let mut guard = outgoing_drag_state().lock().map_err(|e| e.to_string())?;
    *guard = Some(OutgoingDragState {
        data,
        source_window_label: label,
        data_was_requested: false,
    });

    Ok(())
}

/// Clean up native drag preparation (called when internal drag ends normally)
pub fn cleanup_native_drag() -> Result<(), String> {
    eprintln!("[TiddlyDesktop] macOS: cleanup_native_drag called");

    if let Ok(mut guard) = outgoing_drag_state().lock() {
        *guard = None;
    }

    Ok(())
}

/// Start a native drag operation
pub fn start_native_drag(
    window: &WebviewWindow,
    data: OutgoingDragData,
    x: i32,
    y: i32,
    image_data: Option<Vec<u8>>,
    image_offset_x: Option<i32>,
    image_offset_y: Option<i32>,
) -> Result<(), String> {
    let label = window.label().to_string();
    eprintln!(
        "[TiddlyDesktop] macOS: start_native_drag called for window '{}' at ({}, {})",
        label, x, y
    );

    // Store outgoing drag state
    {
        let mut guard = outgoing_drag_state().lock().map_err(|e| e.to_string())?;
        *guard = Some(OutgoingDragState {
            data: data.clone(),
            source_window_label: label.clone(),
            data_was_requested: false,
        });
    }

    // Clone window for use in closure
    let window_clone = window.clone();
    let data_clone = data.clone();

    // Run on main thread since we need to access Cocoa APIs
    let _ = window.run_on_main_thread(move || {
        if let Err(e) = start_native_drag_on_main_thread(&window_clone, data_clone, x, y, image_data, image_offset_x, image_offset_y) {
            eprintln!("[TiddlyDesktop] macOS: start_native_drag failed: {}", e);
            // Clear state on failure
            if let Ok(mut guard) = outgoing_drag_state().lock() {
                *guard = None;
            }
        }
    });

    Ok(())
}

fn start_native_drag_on_main_thread(
    window: &WebviewWindow,
    data: OutgoingDragData,
    x: i32,
    y: i32,
    image_data: Option<Vec<u8>>,
    image_offset_x: Option<i32>,
    image_offset_y: Option<i32>,
) -> Result<(), String> {
    let label = window.label().to_string();

    // Get NSWindow
    let ns_window = window.ns_window().map_err(|e| format!("Failed to get NSWindow: {}", e))?;
    let ns_window_ptr = ns_window as *mut c_void as *mut AnyObject;

    unsafe {
        let ns_window: &NSWindow = &*(ns_window_ptr as *const NSWindow);

        let content_view = ns_window.contentView()
            .ok_or("Failed to get content view")?;

        let webview = find_wkwebview(&content_view)
            .ok_or("Failed to find WKWebView")?;

        // Swizzle source methods if not already done
        swizzle_drag_source_methods(&webview);

        // Create pasteboard item with our data
        let pasteboard_item: Retained<NSPasteboardItem> = msg_send![objc2::class!(NSPasteboardItem), new];

        // Add data to pasteboard item
        if let Some(ref text) = data.text_plain {
            let type_str = NSString::from_str(NS_PASTEBOARD_TYPE_STRING);
            let text_str = NSString::from_str(text);
            let _: Bool = msg_send![&pasteboard_item, setString: &*text_str, forType: &*type_str];
        }

        if let Some(ref html) = data.text_html {
            let type_str = NSString::from_str(NS_PASTEBOARD_TYPE_HTML);
            let html_str = NSString::from_str(html);
            let _: Bool = msg_send![&pasteboard_item, setString: &*html_str, forType: &*type_str];
        }

        if let Some(ref tiddler) = data.text_vnd_tiddler {
            let type_str = NSString::from_str(NS_PASTEBOARD_TYPE_TIDDLER);
            let tiddler_str = NSString::from_str(tiddler);
            let _: Bool = msg_send![&pasteboard_item, setString: &*tiddler_str, forType: &*type_str];
        }

        if let Some(ref url) = data.url {
            let type_str = NSString::from_str(NS_PASTEBOARD_TYPE_URL);
            let url_str = NSString::from_str(url);
            let _: Bool = msg_send![&pasteboard_item, setString: &*url_str, forType: &*type_str];
        }

        // Create dragging item using raw pointers to avoid objc2 type issues
        let dragging_item_alloc: *mut AnyObject = msg_send![objc2::class!(NSDraggingItem), alloc];
        if dragging_item_alloc.is_null() {
            return Err("Failed to allocate NSDraggingItem".to_string());
        }
        let dragging_item: *mut AnyObject = msg_send![dragging_item_alloc, initWithPasteboardWriter: &*pasteboard_item];
        if dragging_item.is_null() {
            return Err("Failed to init NSDraggingItem".to_string());
        }

        // Set dragging frame (required)
        let offset_x = image_offset_x.unwrap_or(0);
        let offset_y = image_offset_y.unwrap_or(0);
        let frame = NSRect {
            origin: NSPoint {
                x: x as f64 - offset_x as f64,
                y: y as f64 - offset_y as f64,
            },
            size: NSSize { width: 100.0, height: 50.0 },
        };

        // Try to create drag image from PNG data with 0.7 opacity (matching Linux/Windows)
        if let Some(img_bytes) = image_data {
            let ns_data = NSData::from_vec(img_bytes);
            // Use raw pointers to avoid objc2 type issues with alloc/init pattern
            let image_class: *const AnyObject = msg_send![objc2::class!(NSImage), alloc];
            if !image_class.is_null() {
                let image: *mut AnyObject = msg_send![image_class, initWithData: &*ns_data];
                if !image.is_null() {
                    let size: NSSize = msg_send![image, size];

                    // Apply 0.7 opacity to match JS drag image styling
                    // Create a new image and draw the original with reduced alpha
                    let faded_image = apply_opacity_to_nsimage(image, size, 0.7);
                    let final_image = if !faded_image.is_null() { faded_image } else { image };

                    let image_frame = NSRect {
                        origin: NSPoint {
                            x: x as f64 - offset_x as f64,
                            y: y as f64 - offset_y as f64,
                        },
                        size,
                    };
                    let _: () = msg_send![dragging_item, setDraggingFrame: image_frame, contents: final_image];
                    eprintln!("[TiddlyDesktop] macOS: Set drag image {}x{} with 0.7 opacity", size.width, size.height);
                } else {
                    let _: () = msg_send![dragging_item, setDraggingFrame: frame, contents: std::ptr::null::<AnyObject>()];
                }
            } else {
                let _: () = msg_send![dragging_item, setDraggingFrame: frame, contents: std::ptr::null::<AnyObject>()];
            }
        } else {
            let _: () = msg_send![dragging_item, setDraggingFrame: frame, contents: std::ptr::null::<AnyObject>()];
        }

        // Create items array using raw pointer
        let items: *mut AnyObject = msg_send![objc2::class!(NSArray), arrayWithObject: dragging_item];

        // Get current event for drag initiation (required by beginDraggingSession)
        let current_event: Option<Retained<NSEvent>> = msg_send![objc2::class!(NSApp), currentEvent];
        let event = current_event.ok_or("No current event available for drag")?;

        // Start dragging session
        // Note: beginDraggingSession:fromPoint:event: is on NSView
        let start_point = NSPoint { x: x as f64, y: y as f64 };

        eprintln!(
            "[TiddlyDesktop] macOS: Calling beginDraggingSession at ({}, {})",
            start_point.x, start_point.y
        );

        let session: *mut AnyObject = msg_send![
            &webview,
            beginDraggingSessionWithItems: items,
            event: &*event,
            source: &*webview
        ];

        if !session.is_null() {
            eprintln!("[TiddlyDesktop] macOS: Native drag session started for window '{}'", label);
            Ok(())
        } else {
            // Clear state on failure
            if let Ok(mut guard) = outgoing_drag_state().lock() {
                *guard = None;
            }
            Err("beginDraggingSession returned nil".to_string())
        }
    }
}

// ============================================================================
// FFI functions for WRY patch to access stored drag data
// ============================================================================

/// FFI function called by WRY patch to get stored text/plain data for internal drags.
/// Returns a pointer to a null-terminated C string, or null if no data is available.
/// The caller must NOT free this memory - it's managed by Rust.
///
/// This allows WRY to fix the pasteboard before calling native drop handling,
/// ensuring that inputs receive the correct text (tiddler title) instead of
/// the resolved URL (wikifile://...#TiddlerTitle).
#[no_mangle]
pub extern "C" fn tiddlydesktop_get_internal_drag_text_plain() -> *const std::ffi::c_char {
    // Use a thread-local to store the CString so it outlives the function call
    thread_local! {
        static CACHED_STRING: std::cell::RefCell<Option<std::ffi::CString>> = const { std::cell::RefCell::new(None) };
    }

    let text = outgoing_drag_state()
        .lock()
        .ok()
        .and_then(|guard| {
            guard.as_ref().and_then(|state| state.data.text_plain.clone())
        });

    match text {
        Some(s) => {
            CACHED_STRING.with(|cached| {
                if let Ok(cstring) = std::ffi::CString::new(s) {
                    let ptr = cstring.as_ptr();
                    *cached.borrow_mut() = Some(cstring);
                    ptr
                } else {
                    std::ptr::null()
                }
            })
        }
        None => std::ptr::null(),
    }
}

/// FFI function called by WRY patch to get stored text/vnd.tiddler data for internal drags.
/// Returns a pointer to a null-terminated C string containing the tiddler JSON,
/// or null if no data is available.
/// The caller must NOT free this memory - it's managed by Rust.
///
/// This allows TiddlyWiki's dropzone handlers to receive the full tiddler data
/// instead of just plain text.
#[no_mangle]
pub extern "C" fn tiddlydesktop_get_internal_drag_tiddler_json() -> *const std::ffi::c_char {
    // Use a thread-local to store the CString so it outlives the function call
    thread_local! {
        static CACHED_STRING: std::cell::RefCell<Option<std::ffi::CString>> = const { std::cell::RefCell::new(None) };
    }

    let tiddler = outgoing_drag_state()
        .lock()
        .ok()
        .and_then(|guard| {
            guard.as_ref().and_then(|state| state.data.text_vnd_tiddler.clone())
        });

    match tiddler {
        Some(s) => {
            CACHED_STRING.with(|cached| {
                if let Ok(cstring) = std::ffi::CString::new(s) {
                    let ptr = cstring.as_ptr();
                    *cached.borrow_mut() = Some(cstring);
                    ptr
                } else {
                    std::ptr::null()
                }
            })
        }
        None => std::ptr::null(),
    }
}

/// FFI function to check if there's an active internal drag from our app.
/// Returns 1 if there's an active internal drag, 0 otherwise.
#[no_mangle]
pub extern "C" fn tiddlydesktop_has_internal_drag() -> i32 {
    outgoing_drag_state()
        .lock()
        .ok()
        .map(|guard| if guard.is_some() { 1 } else { 0 })
        .unwrap_or(0)
}

/// Response structure for get_pending_drag_data command
#[derive(Clone, Debug, serde::Serialize)]
pub struct PendingDragDataResponse {
    pub text_plain: Option<String>,
    pub text_html: Option<String>,
    pub text_vnd_tiddler: Option<String>,
    pub text_uri_list: Option<String>,
    pub source_window: String,
    pub is_text_selection_drag: bool,
}

/// Get pending drag data for cross-wiki drops.
/// Returns the drag data if there's an active drag from a DIFFERENT window,
/// otherwise returns None (same-window drags are handled natively).
pub fn get_pending_drag_data(target_window: &str) -> Option<PendingDragDataResponse> {
    let guard = outgoing_drag_state().lock().ok()?;
    let state = guard.as_ref()?;

    // Only return data if it's a cross-wiki drag (different window)
    if state.source_window_label == target_window {
        eprintln!(
            "[TiddlyDesktop] macOS: get_pending_drag_data - same window '{}', returning None",
            target_window
        );
        return None;
    }

    eprintln!(
        "[TiddlyDesktop] macOS: get_pending_drag_data - cross-wiki from '{}' to '{}', returning data",
        state.source_window_label, target_window
    );

    Some(PendingDragDataResponse {
        text_plain: state.data.text_plain.clone(),
        text_html: state.data.text_html.clone(),
        text_vnd_tiddler: state.data.text_vnd_tiddler.clone(),
        text_uri_list: state.data.text_uri_list.clone(),
        source_window: state.source_window_label.clone(),
        is_text_selection_drag: state.data.is_text_selection_drag,
    })
}

// ============================================================================
// FFI functions for external file drop path extraction (matching Windows API)
// ============================================================================

lazy_static::lazy_static! {
    /// Global storage for file paths from external drops (populated by swizzled drop handler)
    static ref EXTERNAL_DROP_PATHS: Mutex<Option<Vec<String>>> = Mutex::new(None);
}

/// FFI function called by WRY patch to store file paths when a drop occurs.
/// The paths are stored as a JSON array string.
/// This allows JavaScript to access the original file paths when the native DOM drop fires.
#[no_mangle]
pub extern "C" fn tiddlydesktop_store_drop_paths(paths_json: *const std::ffi::c_char) {
    if paths_json.is_null() {
        return;
    }

    unsafe {
        let cstr = std::ffi::CStr::from_ptr(paths_json);
        if let Ok(json_str) = cstr.to_str() {
            if let Ok(paths) = serde_json::from_str::<Vec<String>>(json_str) {
                eprintln!("[TiddlyDesktop] macOS FFI: Storing {} drop paths", paths.len());
                for path in &paths {
                    eprintln!("[TiddlyDesktop] macOS FFI:   - {}", path);
                }
                if let Ok(mut guard) = EXTERNAL_DROP_PATHS.lock() {
                    *guard = Some(paths);
                }
            }
        }
    }
}

/// FFI function called to clear stored file paths (e.g., on drag leave).
#[no_mangle]
pub extern "C" fn tiddlydesktop_clear_drop_paths() {
    eprintln!("[TiddlyDesktop] macOS FFI: Clearing drop paths");
    if let Ok(mut guard) = EXTERNAL_DROP_PATHS.lock() {
        *guard = None;
    }
}

