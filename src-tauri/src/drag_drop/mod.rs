//! Platform-specific drag-drop handling for external content (text, HTML, URLs)
//!
//! This module provides drag-drop support that extracts content from external applications
//! and emits `td-*` events to JavaScript.
//!
//! ## Shared utilities
//! - `encoding` - Text encoding detection and conversion (UTF-8, UTF-16 LE/BE)
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

mod encoding;

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
pub mod windows_job;

#[cfg(target_os = "linux")]
mod linux;


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
    };
    linux::prepare_native_drag(window, outgoing_data)
}

#[cfg(target_os = "windows")]
pub fn prepare_native_drag_impl(_window: &WebviewWindow, _data: NativeDragData) -> Result<(), String> {
    Ok(()) // No-op for Windows currently
}

#[cfg(target_os = "macos")]
pub fn prepare_native_drag_impl(_window: &WebviewWindow, _data: NativeDragData) -> Result<(), String> {
    Ok(()) // No-op for macOS currently
}

/// Clean up native drag preparation (called when internal drag ends normally)
#[cfg(target_os = "linux")]
pub fn cleanup_native_drag_impl() -> Result<(), String> {
    linux::cleanup_native_drag()
}

#[cfg(target_os = "windows")]
pub fn cleanup_native_drag_impl() -> Result<(), String> {
    Ok(()) // No-op for Windows currently
}

#[cfg(target_os = "macos")]
pub fn cleanup_native_drag_impl() -> Result<(), String> {
    Ok(()) // No-op for macOS currently
}
