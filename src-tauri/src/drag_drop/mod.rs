//! Platform-specific drag-drop handling for external content (text, HTML, URLs)
//!
//! This module provides drag-drop support that extracts content from external applications
//! and emits `td-*` events to JavaScript.
//!
//! ## Shared utilities
//! - `encoding` - Text encoding detection and conversion (UTF-8, UTF-16 LE/BE)
//! - `sanitize` - Security sanitization for incoming drag content
//!
//! ## Platform implementations
//!
//! - **Windows**: Custom IDropTarget COM implementation extracts content from IDataObject
//!   because WebView2's native handling doesn't expose content (text/html) to JavaScript
//! - **macOS**: Uses objc2/AppKit to register for drag types and extract content from
//!   NSPasteboard, because WKWebView's dataTransfer doesn't reliably expose external content
//! - **Linux**: Uses GTK3 drag-and-drop signals to extract content from SelectionData,
//!   because WebKitGTK's dataTransfer doesn't reliably expose external content
//!
//! All platforms emit the same events for JavaScript to handle:
//! - `td-drag-motion` - during drag over (with coordinates)
//! - `td-drag-leave` - when drag leaves the window
//! - `td-drag-drop-start` - at the start of a drop operation
//! - `td-drag-drop-position` - drop position coordinates
//! - `td-drag-content` - extracted content (text/html/urls)
//! - `td-file-drop` - file paths for file drops
//!
//! Internal drags (within the webview) are handled by JavaScript:
//! - `internal_drag.js` intercepts dragstart for draggable elements and text selections
//! - `td-drag-*` handlers check `TD.isInternalDragActive()` and skip if true
//! - `internal_drag.js` creates synthetic drag events using mouse tracking
//!
//! ## Security
//!
//! Incoming drag content from external applications is sanitized:
//! - URLs: Dangerous schemes (javascript:, vbscript:, data:text/html) are blocked
//! - HTML: Script tags and event handlers are stripped
//! - File paths: Path traversal sequences are rejected

// Encoding utilities - only needed on Windows and Linux
#[cfg(any(target_os = "windows", target_os = "linux"))]
mod encoding;

// Sanitization utilities - needed on all platforms with drag-drop
// Made pub(crate) so lib.rs can use validate_wiki_path for Tauri commands
#[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
pub(crate) mod sanitize;

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
pub mod windows_job;

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
mod input_inject;

#[cfg(target_os = "linux")]
pub(crate) mod native_dnd;


#[cfg(target_os = "macos")]
mod macos;

use tauri::WebviewWindow;

/// Set up platform-specific drag-drop handling for a webview window
pub fn setup_drag_handlers(window: &WebviewWindow) {
    #[cfg(target_os = "windows")]
    windows::setup_drag_handlers(window);

    #[cfg(target_os = "linux")]
    linux::setup_drag_handlers(window);

    #[cfg(target_os = "macos")]
    macos::setup_drag_handlers(window);
}

/// Data for starting a native drag operation (cross-platform structure)
/// Matches MIME types used by TiddlyWiki5's drag-drop system
#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct NativeDragData {
    pub text_plain: Option<String>,
    pub text_html: Option<String>,
    pub text_vnd_tiddler: Option<String>,
    pub text_uri_list: Option<String>,
    /// Mozilla URL format: data:text/vnd.tiddler,<url-encoded-json>
    pub text_x_moz_url: Option<String>,
    /// Standard URL type: data:text/vnd.tiddler,<url-encoded-json>
    pub url: Option<String>,
    /// True if this is a text-selection drag (not a draggable element)
    /// Text-selection drags need special handling because WebKit's DataTransfer is broken
    #[serde(default)]
    pub is_text_selection_drag: bool,
}

/// Start a native drag operation (called from JavaScript when pointer leaves window during internal drag)
#[cfg(target_os = "linux")]
pub fn start_native_drag_impl(window: &WebviewWindow, data: NativeDragData, x: i32, y: i32, image_data: Option<Vec<u8>>, image_offset_x: Option<i32>, image_offset_y: Option<i32>) -> Result<(), String> {
    let outgoing_data = linux::OutgoingDragData {
        text_plain: data.text_plain,
        text_html: data.text_html,
        text_vnd_tiddler: data.text_vnd_tiddler,
        text_uri_list: data.text_uri_list,
        text_x_moz_url: data.text_x_moz_url,
        url: data.url,
        is_text_selection_drag: data.is_text_selection_drag,
    };
    linux::start_native_drag(window, outgoing_data, x, y, image_data, image_offset_x, image_offset_y)
}

#[cfg(target_os = "windows")]
pub fn start_native_drag_impl(window: &WebviewWindow, data: NativeDragData, x: i32, y: i32, image_data: Option<Vec<u8>>, image_offset_x: Option<i32>, image_offset_y: Option<i32>) -> Result<(), String> {
    let outgoing_data = windows::OutgoingDragData {
        text_plain: data.text_plain,
        text_html: data.text_html,
        text_vnd_tiddler: data.text_vnd_tiddler,
        text_uri_list: data.text_uri_list,
        text_x_moz_url: data.text_x_moz_url,
        url: data.url,
        is_text_selection_drag: data.is_text_selection_drag,
    };
    windows::start_native_drag(window, outgoing_data, x, y, image_data, image_offset_x, image_offset_y)
}

#[cfg(target_os = "macos")]
pub fn start_native_drag_impl(window: &WebviewWindow, data: NativeDragData, x: i32, y: i32, image_data: Option<Vec<u8>>, image_offset_x: Option<i32>, image_offset_y: Option<i32>) -> Result<(), String> {
    let outgoing_data = macos::OutgoingDragData {
        text_plain: data.text_plain,
        text_html: data.text_html,
        text_vnd_tiddler: data.text_vnd_tiddler,
        text_uri_list: data.text_uri_list,
        text_x_moz_url: data.text_x_moz_url,
        url: data.url,
        is_text_selection_drag: data.is_text_selection_drag,
    };
    macos::start_native_drag(window, outgoing_data, x, y, image_data, image_offset_x, image_offset_y)
}

/// Prepare for a potential native drag (called when internal drag starts)
#[cfg(target_os = "linux")]
pub fn prepare_native_drag_impl(window: &WebviewWindow, data: NativeDragData) -> Result<(), String> {
    let outgoing_data = linux::OutgoingDragData {
        text_plain: data.text_plain,
        text_html: data.text_html,
        text_vnd_tiddler: data.text_vnd_tiddler,
        text_uri_list: data.text_uri_list,
        text_x_moz_url: data.text_x_moz_url,
        url: data.url,
        is_text_selection_drag: data.is_text_selection_drag,
    };
    linux::prepare_native_drag(window, outgoing_data)
}

#[cfg(target_os = "windows")]
pub fn prepare_native_drag_impl(window: &WebviewWindow, data: NativeDragData) -> Result<(), String> {
    let outgoing_data = windows::OutgoingDragData {
        text_plain: data.text_plain,
        text_html: data.text_html,
        text_vnd_tiddler: data.text_vnd_tiddler,
        text_uri_list: data.text_uri_list,
        text_x_moz_url: data.text_x_moz_url,
        url: data.url,
        is_text_selection_drag: data.is_text_selection_drag,
    };
    windows::prepare_native_drag(window, outgoing_data)
}

#[cfg(target_os = "macos")]
pub fn prepare_native_drag_impl(window: &WebviewWindow, data: NativeDragData) -> Result<(), String> {
    let outgoing_data = macos::OutgoingDragData {
        text_plain: data.text_plain,
        text_html: data.text_html,
        text_vnd_tiddler: data.text_vnd_tiddler,
        text_uri_list: data.text_uri_list,
        text_x_moz_url: data.text_x_moz_url,
        url: data.url,
        is_text_selection_drag: data.is_text_selection_drag,
    };
    macos::prepare_native_drag(window, outgoing_data)
}

/// Clean up native drag preparation (called when internal drag ends normally)
#[cfg(target_os = "linux")]
pub fn cleanup_native_drag_impl() -> Result<(), String> {
    linux::cleanup_native_drag()
}

#[cfg(target_os = "windows")]
pub fn cleanup_native_drag_impl() -> Result<(), String> {
    windows::cleanup_native_drag()
}

#[cfg(target_os = "macos")]
pub fn cleanup_native_drag_impl() -> Result<(), String> {
    macos::cleanup_native_drag()
}

/// Update the drag icon during an active drag operation
#[cfg(target_os = "linux")]
pub fn update_drag_icon_impl(image_data: Vec<u8>, offset_x: i32, offset_y: i32) -> Result<(), String> {
    linux::update_drag_icon(image_data, offset_x, offset_y)
}

#[cfg(target_os = "windows")]
pub fn update_drag_icon_impl(_image_data: Vec<u8>, _offset_x: i32, _offset_y: i32) -> Result<(), String> {
    Ok(()) // No-op for Windows currently
}

#[cfg(target_os = "macos")]
pub fn update_drag_icon_impl(_image_data: Vec<u8>, _offset_x: i32, _offset_y: i32) -> Result<(), String> {
    Ok(()) // No-op for macOS currently
}

/// Set the pending drag icon before a drag starts
#[cfg(target_os = "linux")]
pub fn set_pending_drag_icon_impl(image_data: Vec<u8>, offset_x: i32, offset_y: i32) -> Result<(), String> {
    linux::set_pending_drag_icon(image_data, offset_x, offset_y)
}

/// Toggle drag destination on WebKitWebView for a window
/// When disabled, WebKitGTK's native handling takes over (shows caret in editables)
/// When enabled, our custom handling intercepts drags
#[cfg(target_os = "linux")]
pub fn set_drag_dest_enabled_impl(label: &str, enabled: bool) {
    linux::set_drag_dest_enabled(label, enabled)
}

#[cfg(target_os = "windows")]
pub fn set_drag_dest_enabled_impl(_label: &str, _enabled: bool) {
    // No-op for Windows currently
}

#[cfg(target_os = "macos")]
pub fn set_drag_dest_enabled_impl(_label: &str, _enabled: bool) {
    // No-op for macOS currently
}

/// Temporarily ungrab the seat to allow focus changes during drag
#[cfg(target_os = "linux")]
pub fn ungrab_seat_for_focus_impl(label: &str) {
    linux::ungrab_seat_for_focus(label)
}

#[cfg(target_os = "windows")]
pub fn ungrab_seat_for_focus_impl(_label: &str) {
    // No-op for Windows currently
}

#[cfg(target_os = "macos")]
pub fn ungrab_seat_for_focus_impl(_label: &str) {
    // No-op for macOS currently
}
