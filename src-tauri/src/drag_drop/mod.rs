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
