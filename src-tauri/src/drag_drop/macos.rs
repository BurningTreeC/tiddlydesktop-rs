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

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSPasteboard, NSView, NSWindow};
use objc2_foundation::{NSArray, NSData, NSString};
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
/// TiddlyWiki tiddler JSON format
const NS_PASTEBOARD_TYPE_TIDDLER: &str = "text/vnd.tiddler";
const NS_PASTEBOARD_TYPE_JSON: &str = "public.json";
/// Browser custom data formats (may contain text/vnd.tiddler)
const NS_PASTEBOARD_TYPE_MOZ_CUSTOM: &str = "org.mozilla.custom-clipdata";
const NS_PASTEBOARD_TYPE_CHROME_CUSTOM: &str = "org.chromium.web-custom-data";

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
        // Tauri returns a raw pointer - cast it to NSWindow
        let ns_window_ptr = ns_window as *mut std::ffi::c_void as *mut AnyObject;

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
                    let _: () = msg_send![&webview, registerForDraggedTypes: &*drag_types];

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
        let class: *const AnyObject = msg_send![obj, class];
        let name: Retained<NSString> = msg_send![class, className];
        name.to_string()
    }
}

/// Create an NSArray of pasteboard types we accept
fn create_drag_types_array() -> Retained<NSArray<NSString>> {
    let types = vec![
        // Browser custom data formats - highest priority (may contain text/vnd.tiddler)
        NSString::from_str(NS_PASTEBOARD_TYPE_MOZ_CUSTOM),
        NSString::from_str(NS_PASTEBOARD_TYPE_CHROME_CUSTOM),
        // TiddlyWiki tiddler JSON - highest priority for cross-wiki drag
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
        // Cast to AnyObject to use msg_send
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
pub fn extract_pasteboard_content(pasteboard: &NSPasteboard) -> Option<DragContentData> {
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

    // 1. Try to get TiddlyWiki tiddler JSON (highest priority for cross-wiki drag)
    let tiddler_type = NSString::from_str(NS_PASTEBOARD_TYPE_TIDDLER);
    if let Some(tiddler) = pasteboard.stringForType(&tiddler_type) {
        let tiddler_str = tiddler.to_string();
        if !tiddler_str.is_empty() {
            eprintln!("[TiddlyDesktop] macOS: Got tiddler data!");
            types.push("text/vnd.tiddler".to_string());
            data.insert("text/vnd.tiddler".to_string(), tiddler_str.clone());
            // Also include as text/plain for fallback
            types.push("text/plain".to_string());
            data.insert("text/plain".to_string(), tiddler_str);
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
    let text_type = NSString::from_str(NS_PASTEBOARD_TYPE_STRING);
    if let Some(text) = pasteboard.stringForType(&text_type) {
        let text_str = text.to_string();
        if !text_str.is_empty() && !data.contains_key("text/plain") {
            // Check if plain text looks like tiddler JSON (browser may not expose text/vnd.tiddler)
            let looks_like_tiddler_json = text_str.trim_start().starts_with('[')
                && text_str.contains("\"title\"")
                && (text_str.contains("\"text\"") || text_str.contains("\"fields\""));

            if looks_like_tiddler_json && !data.contains_key("text/vnd.tiddler") {
                eprintln!("[TiddlyDesktop] macOS: Detected tiddler JSON in plain text!");
                types.push("text/vnd.tiddler".to_string());
                data.insert("text/vnd.tiddler".to_string(), text_str.clone());
            }

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

    // 4. Try to get HTML
    let html_type = NSString::from_str(NS_PASTEBOARD_TYPE_HTML);
    if let Some(html) = pasteboard.stringForType(&html_type) {
        let html_str = html.to_string();
        if !html_str.is_empty() {
            types.push("text/html".to_string());
            data.insert("text/html".to_string(), html_str);
        }
    }

    // 5. Try to get URL
    let url_type = NSString::from_str(NS_PASTEBOARD_TYPE_URL);
    if let Some(url) = pasteboard.stringForType(&url_type) {
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
            // filenames is an NSArray of NSStrings
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
