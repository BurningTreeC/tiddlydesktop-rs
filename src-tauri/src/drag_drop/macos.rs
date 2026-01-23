//! macOS drag-drop handling using objc2/AppKit for content extraction
//!
//! WKWebView's native drag-drop handling doesn't reliably expose content (text, HTML, URLs)
//! from external apps to JavaScript via dataTransfer. We use Cocoa drag pasteboard APIs to:
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
use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{msg_send_id, ClassType};
use objc2_app_kit::{NSPasteboard, NSView, NSWindow};
use objc2_foundation::{NSArray, NSString};
use tauri::{Emitter, WebviewWindow};

/// Data captured from a drag operation
#[derive(Clone, Debug, serde::Serialize)]
pub struct DragContentData {
    pub types: Vec<String>,
    pub data: HashMap<String, String>,
}

/// Pasteboard type constants (UTI format)
const NS_PASTEBOARD_TYPE_STRING: &str = "public.utf8-plain-text";
const NS_PASTEBOARD_TYPE_HTML: &str = "public.html";
const NS_PASTEBOARD_TYPE_URL: &str = "public.url";
const NS_PASTEBOARD_TYPE_FILE_URL: &str = "public.file-url";

/// State for tracking drag operations per window
struct DragState {
    window: WebviewWindow,
    drag_active: bool,
    drop_in_progress: bool,
}

lazy_static::lazy_static! {
    static ref DRAG_STATES: Mutex<HashMap<String, Arc<Mutex<DragState>>>> = Mutex::new(HashMap::new());
}

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
        drop_in_progress: false,
    }));

    DRAG_STATES
        .lock()
        .unwrap()
        .insert(label.clone(), state.clone());

    // Register for drag types on the webview
    // Note: The actual drag interception happens through Wry's drag-drop handler
    // which Tauri exposes via tauri://drag-* events. Our implementation here
    // supplements that by providing helper functions for content extraction.

    let window_clone = window.clone();

    // Use run_on_main_thread to safely access Cocoa APIs
    let _ = window.run_on_main_thread(move || {
        setup_drag_destination(&window_clone);
    });
}

fn setup_drag_destination(window: &WebviewWindow) {
    // Get the NSWindow
    if let Ok(ns_window) = window.ns_window() {
        // Tauri returns a raw pointer wrapped in a type
        let ns_window_ptr = ns_window.0 as *mut AnyObject;

        unsafe {
            // Convert to NSWindow reference
            let ns_window: &NSWindow = &*(ns_window_ptr as *const NSWindow);

            // Get the content view
            if let Some(content_view) = ns_window.contentView() {
                // Find the WKWebView in the view hierarchy
                if let Some(webview) = find_wkwebview(&content_view) {
                    eprintln!("[TiddlyDesktop] macOS: Found WKWebView, registering drag types");

                    // Register for drag types we're interested in
                    let drag_types = create_drag_types_array();
                    let _: () = msg_send_id![&webview, registerForDraggedTypes: &*drag_types];

                    eprintln!(
                        "[TiddlyDesktop] macOS: Drag types registered for window '{}'",
                        window.label()
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

/// Find WKWebView in the view hierarchy
fn find_wkwebview(view: &NSView) -> Option<Retained<NSView>> {
    unsafe {
        // Check if this view is a WKWebView
        let class_name = get_class_name(view);
        if class_name.contains("WKWebView") || class_name.contains("WryWebView") {
            return Some(Retained::retain(view as *const NSView as *mut NSView).unwrap());
        }

        // Recursively search subviews
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
        let class: *const AnyObject = msg_send_id![obj, class];
        let name: Retained<NSString> = msg_send_id![class, className];
        name.to_string()
    }
}

/// Create an NSArray of pasteboard types we accept
fn create_drag_types_array() -> Retained<NSArray<NSString>> {
    let types = vec![
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

/// Extract content from a drag pasteboard
pub fn extract_pasteboard_content(pasteboard: &NSPasteboard) -> Option<DragContentData> {
    let mut types = Vec::new();
    let mut data = HashMap::new();

    // Try to get plain text
    let text_type = NSString::from_str(NS_PASTEBOARD_TYPE_STRING);
    if let Some(text) = unsafe { pasteboard.stringForType(&text_type) } {
        let text_str = text.to_string();
        if !text_str.is_empty() {
            types.push("text/plain".to_string());
            data.insert("text/plain".to_string(), text_str.clone());

            // Check if it's a URL
            if text_str.starts_with("http://") || text_str.starts_with("https://") {
                types.push("text/uri-list".to_string());
                data.insert("text/uri-list".to_string(), text_str.clone());
                types.push("URL".to_string());
                data.insert("URL".to_string(), text_str);
            }
        }
    }

    // Try to get HTML
    let html_type = NSString::from_str(NS_PASTEBOARD_TYPE_HTML);
    if let Some(html) = unsafe { pasteboard.stringForType(&html_type) } {
        let html_str = html.to_string();
        if !html_str.is_empty() {
            types.push("text/html".to_string());
            data.insert("text/html".to_string(), html_str);
        }
    }

    // Try to get URL
    let url_type = NSString::from_str(NS_PASTEBOARD_TYPE_URL);
    if let Some(url) = unsafe { pasteboard.stringForType(&url_type) } {
        let url_str = url.to_string();
        if !url_str.is_empty() && !data.contains_key("URL") {
            types.push("URL".to_string());
            data.insert("URL".to_string(), url_str);
        }
    }

    if types.is_empty() {
        None
    } else {
        Some(DragContentData { types, data })
    }
}

/// Extract file paths from a drag pasteboard
pub fn extract_file_paths(pasteboard: &NSPasteboard) -> Vec<String> {
    let mut paths = Vec::new();

    // Try public.file-url
    let file_url_type = NSString::from_str(NS_PASTEBOARD_TYPE_FILE_URL);
    if let Some(file_url) = unsafe { pasteboard.stringForType(&file_url_type) } {
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
        if let Some(filenames) = unsafe { pasteboard.propertyListForType(&filenames_type) } {
            // filenames is an NSArray of NSStrings
            unsafe {
                let array: &NSArray<NSString> = &*(filenames.as_ref() as *const AnyObject
                    as *const NSArray<NSString>);
                for item in array.iter() {
                    let path_str = item.to_string();
                    if !path_str.is_empty() {
                        paths.push(path_str);
                    }
                }
            }
        }
    }

    paths
}

/// Emit drag motion event
pub fn emit_drag_motion(window_label: &str, x: f64, y: f64) {
    if let Some(state) = DRAG_STATES.lock().unwrap().get(window_label) {
        let state = state.lock().unwrap();
        let _ = state.window.emit(
            "td-drag-motion",
            serde_json::json!({
                "x": x,
                "y": y,
                "screenCoords": false
            }),
        );
    }
}

/// Emit drag leave event
pub fn emit_drag_leave(window_label: &str) {
    if let Some(state) = DRAG_STATES.lock().unwrap().get(window_label) {
        let state = state.lock().unwrap();
        let _ = state.window.emit("td-drag-leave", ());
    }
}

/// Emit drop events with content
pub fn emit_drop_with_content(window_label: &str, x: f64, y: f64, content: DragContentData) {
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

        eprintln!(
            "[TiddlyDesktop] macOS: Emitted content drop with types: {:?}",
            content.types
        );
    }
}

/// Emit drop events with file paths
pub fn emit_drop_with_files(window_label: &str, x: f64, y: f64, paths: Vec<String>) {
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

        let _ = state.window.emit(
            "td-file-drop",
            serde_json::json!({
                "paths": paths
            }),
        );

        eprintln!(
            "[TiddlyDesktop] macOS: Emitted file drop with {} paths",
            paths.len()
        );
    }
}
