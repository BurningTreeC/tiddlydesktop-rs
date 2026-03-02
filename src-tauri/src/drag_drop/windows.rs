//! Windows drag-drop handling via composition hosting
//!
//! This module uses WebView2 composition hosting mode for full drag-drop control:
//! 1. Registers IDropTarget on parent HWND (composition mode - we control it)
//! 2. Extracts file paths and emits Tauri events
//! 3. Forwards drag events to WebView2 via ICoreWebView2CompositionController3
//! 4. Provides DragStarting handler for cross-wiki drag detection
//! 5. Supports outgoing drags via OLE DoDragDrop

#![cfg(target_os = "windows")]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::sync::Mutex;

use super::encoding;
use super::sanitize;

use tauri::{Emitter, Manager, WebviewWindow};
// Note: wry::WebViewExtWindows has composition_controller() but Tauri's PlatformWebview
// doesn't expose it. Instead we cast ICoreWebView2Controller to get the composition controller.

use windows::core::{implement, HRESULT, w};
use windows::Win32::Foundation::{HWND, POINTL, S_OK, POINT};
use windows::Win32::System::Com::{
    IDataObject, IDataObject_Impl, IEnumFORMATETC, DVASPECT_CONTENT, FORMATETC,
    STGMEDIUM, TYMED_HGLOBAL, IAdviseSink, IEnumSTATDATA,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{
    DoDragDrop, IDropSource, IDropSource_Impl, IDropTarget, IDropTarget_Impl,
    OleInitialize, RegisterDragDrop, RevokeDragDrop,
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::UI::Shell::DragQueryFileW;
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows_core::{BOOL, Interface, Ref};

// WebView2 APIs
use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2CompositionController, ICoreWebView2CompositionController3,
    ICoreWebView2CompositionController5, ICoreWebView2Controller4,
    ICoreWebView2DragStartingEventArgs, ICoreWebView2DragStartingEventHandler,
    ICoreWebView2DragStartingEventHandler_Impl,
};

/// CF_HDROP for file drops
const CF_HDROP: u16 = 15;

/// Clipboard format constants
const CF_UNICODETEXT: u16 = 13;

/// Mouse button masks
const MK_LBUTTON: u32 = 0x0001;
const MK_RBUTTON: u32 = 0x0002;

/// DRAGDROP return codes (HRESULT values)
const DRAGDROP_S_DROP: HRESULT = HRESULT(0x00040100u32 as i32);
const DRAGDROP_S_CANCEL: HRESULT = HRESULT(0x00040101u32 as i32);
const DRAGDROP_S_USEDEFAULTCURSORS: HRESULT = HRESULT(0x00040102u32 as i32);

/// Custom clipboard formats
fn cf_html() -> u16 {
    unsafe { RegisterClipboardFormatW(w!("HTML Format")) as u16 }
}

fn cf_tiddler() -> u16 {
    unsafe { RegisterClipboardFormatW(w!("text/vnd.tiddler")) as u16 }
}

fn cf_url_w() -> u16 {
    unsafe { RegisterClipboardFormatW(w!("UniformResourceLocatorW")) as u16 }
}

fn cf_moz_url() -> u16 {
    unsafe { RegisterClipboardFormatW(w!("text/x-moz-url")) as u16 }
}

fn cf_chromium_custom() -> u16 {
    unsafe { RegisterClipboardFormatW(w!("chromium/x-web-custom-data")) as u16 }
}

fn cf_moz_custom() -> u16 {
    unsafe { RegisterClipboardFormatW(w!("application/x-moz-custom-clipdata")) as u16 }
}

// ============================================================================
// Global App Handle
// ============================================================================

lazy_static::lazy_static! {
    /// Global app handle for emitting events (set during init)
    static ref APP_HANDLE: Mutex<Option<tauri::AppHandle>> = Mutex::new(None);
}

/// Initialize drag-drop support (called early in app startup)
/// With composition hosting, this just stores the app handle - no hooks needed
pub fn init_drop_target_hook() {
    eprintln!("[TiddlyDesktop] Windows: init_drop_target_hook (composition mode - no hooks needed)");
}

// ============================================================================
// Internal Drag State (for FFI with wry fork)
// ============================================================================

/// Combined internal drag state to prevent race conditions when reading.
/// Values: 0 = no drag, 1 = tiddler/$draggable drag, 2 = link drag, 3 = text selection drag
/// Using a single atomic ensures consistent reads - either the drag is active
/// with its type, or it's not active at all.
static INTERNAL_DRAG_STATE: AtomicU8 = AtomicU8::new(0);

/// State values for INTERNAL_DRAG_STATE
const DRAG_STATE_NONE: u8 = 0;
const DRAG_STATE_TIDDLER: u8 = 1;  // Tiddler/$draggable drag - skip listener AND forwarding
const DRAG_STATE_LINK: u8 = 2;     // Link drag - skip listener, but DO forward (needs native handling)
const DRAG_STATE_TEXT_SELECTION: u8 = 3;  // Text selection drag - DO listener, DO forward

/// FFI function for wry fork to check if there's an active internal drag.
/// Returns 1 if there's an active internal drag from this WebView2, 0 otherwise.
#[no_mangle]
pub extern "C" fn tiddlydesktop_has_internal_drag() -> i32 {
    if INTERNAL_DRAG_STATE.load(Ordering::Acquire) != DRAG_STATE_NONE { 1 } else { 0 }
}

/// FFI function for wry fork to check if the internal drag is a text selection drag.
/// Returns 1 if it's a text selection drag (should activate dropzone), 0 otherwise.
/// Text selection drags SHOULD activate the dropzone for pasting.
/// Tiddler and link drags should NOT activate the dropzone.
#[no_mangle]
pub extern "C" fn tiddlydesktop_is_text_selection_drag() -> i32 {
    if INTERNAL_DRAG_STATE.load(Ordering::Acquire) == DRAG_STATE_TEXT_SELECTION { 1 } else { 0 }
}

/// FFI function for wry fork to check if the internal drag is a tiddler drag.
/// Returns 1 if it's a tiddler/$draggable drag, 0 otherwise.
/// Tiddler drags are fully handled by internal_drag.js and don't need composition
/// controller forwarding. Link drags DO need forwarding for native handling.
#[no_mangle]
pub extern "C" fn tiddlydesktop_is_tiddler_drag() -> i32 {
    if INTERNAL_DRAG_STATE.load(Ordering::Acquire) == DRAG_STATE_TIDDLER { 1 } else { 0 }
}

/// Drag type for internal drag state
#[derive(Debug, Clone, Copy, PartialEq)]
enum InternalDragType {
    Tiddler,      // $draggable with text/vnd.tiddler - handled by internal_drag.js
    Link,         // Link drag with URL data - needs native forwarding
    TextSelection, // Plain text selection - needs dropzone activation
}

/// Called when an internal drag starts (from DragStarting event)
fn set_internal_drag_active(drag_type: InternalDragType) {
    let state = match drag_type {
        InternalDragType::Tiddler => DRAG_STATE_TIDDLER,
        InternalDragType::Link => DRAG_STATE_LINK,
        InternalDragType::TextSelection => DRAG_STATE_TEXT_SELECTION,
    };
    eprintln!("[TiddlyDesktop] Windows: set_internal_drag_active({:?}) -> state={}", drag_type, state);
    INTERNAL_DRAG_STATE.store(state, Ordering::Release);
}

/// FFI function for wry fork to call when the drag ends (Drop or DragLeave).
/// This clears the internal drag state.
#[no_mangle]
pub extern "C" fn tiddlydesktop_clear_internal_drag() {
    eprintln!("[TiddlyDesktop] Windows: tiddlydesktop_clear_internal_drag");
    INTERNAL_DRAG_STATE.store(DRAG_STATE_NONE, Ordering::Release);
    // Also clear droppable state when drag ends
    OVER_DROPPABLE.store(false, Ordering::Release);
}

// ============================================================================
// Droppable State (for cursor effect control)
// ============================================================================

/// Tracks whether the cursor is currently over a $droppable widget.
/// This is set by JavaScript via a Tauri command during dragenter/dragleave.
/// Used by wry to determine the cursor effect for internal drags.
static OVER_DROPPABLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// FFI function for wry fork to check if cursor is over a droppable widget.
/// Returns 1 if over a droppable, 0 otherwise.
#[no_mangle]
pub extern "C" fn tiddlydesktop_is_over_droppable() -> i32 {
    if OVER_DROPPABLE.load(Ordering::Acquire) { 1 } else { 0 }
}

/// Set whether the cursor is over a droppable widget.
/// Called from JavaScript via Tauri command.
pub fn set_over_droppable(over: bool) {
    OVER_DROPPABLE.store(over, Ordering::Release);
}

/// Set the internal drag type from JavaScript.
/// This is called from the JS dragstart handler and is more reliable than
/// the WebView2 DragStarting event because JS events fire before IDropTarget::DragEnter.
pub fn set_internal_drag_type_from_js(drag_type: &str) {
    let state = match drag_type {
        "tiddler" => DRAG_STATE_TIDDLER,
        "link" => DRAG_STATE_LINK,
        "text" => DRAG_STATE_TEXT_SELECTION,
        "none" => DRAG_STATE_NONE,
        _ => {
            eprintln!("[TiddlyDesktop] Windows: Unknown drag type from JS: {}", drag_type);
            DRAG_STATE_NONE
        }
    };
    eprintln!("[TiddlyDesktop] Windows: set_internal_drag_type_from_js({}) -> state={}", drag_type, state);
    INTERNAL_DRAG_STATE.store(state, Ordering::Release);
}

// ============================================================================
// Public API
// ============================================================================

/// Data for outgoing drag operations (TiddlyWiki → external apps)
#[derive(Clone, Debug, Default)]
#[allow(dead_code)]
pub struct OutgoingDragData {
    pub text_plain: Option<String>,
    pub text_html: Option<String>,
    pub text_vnd_tiddler: Option<String>,
    pub text_uri_list: Option<String>,
    pub text_x_moz_url: Option<String>,
    pub url: Option<String>,
    pub is_text_selection_drag: bool,
    pub source_window: Option<String>,
}

/// Set up drag handlers for a window (composition hosting mode)
///
/// In composition hosting mode:
/// - WebView2 renders to a DirectComposition visual
/// - No Chrome_WidgetWin_* child windows are created in our process
/// - We replace WRY's CompositionDragDropTarget with our ContentAwareDropTarget
///   that extracts content (text, HTML, tiddler data) from external drags
/// - We register DragStarting handler for outgoing drag detection
pub fn setup_drag_handlers(window: &WebviewWindow) {
    let window_label = window.label().to_string();
    let app_handle = window.app_handle().clone();

    eprintln!("[TiddlyDesktop] Windows: setup_drag_handlers for '{}' (composition mode)", window_label);

    // Store app handle globally
    if let Ok(mut handle) = APP_HANDLE.lock() {
        *handle = Some(app_handle.clone());
    }

    let _ = window.with_webview(move |webview| {
        #[cfg(windows)]
        unsafe {
            // Initialize OLE (required for DoDragDrop and RegisterDragDrop)
            let _ = OleInitialize(None);

            let controller = webview.controller();

            // Get the WRY container HWND
            let mut container_hwnd = HWND::default();
            let _ = controller.ParentWindow(&mut container_hwnd);
            eprintln!("[TiddlyDesktop] Windows: Container HWND = {:?}", container_hwnd);

            // Enable external drops (should already be done by WRY patch, but ensure it)
            if let Ok(controller4) = controller.cast::<ICoreWebView2Controller4>() {
                match controller4.SetAllowExternalDrop(true) {
                    Ok(()) => eprintln!("[TiddlyDesktop] Windows: SetAllowExternalDrop(true) confirmed"),
                    Err(e) => eprintln!("[TiddlyDesktop] Windows: SetAllowExternalDrop failed: {:?}", e),
                }
            }

            // Get composition controller for forwarding drag events and DragStarting handler
            let composition_controller3 = controller.cast::<ICoreWebView2CompositionController>()
                .ok()
                .and_then(|c| c.cast::<ICoreWebView2CompositionController3>().ok());

            if let Some(comp_ctrl) = composition_controller3 {
                eprintln!("[TiddlyDesktop] Windows: Got ICoreWebView2CompositionController3 - composition mode active");

                // Replace WRY's CompositionDragDropTarget with our ContentAwareDropTarget
                // that extracts content from external drags and emits td-drag-* events
                match RevokeDragDrop(container_hwnd) {
                    Ok(()) => eprintln!("[TiddlyDesktop] Windows: RevokeDragDrop succeeded on container HWND"),
                    Err(e) => eprintln!("[TiddlyDesktop] Windows: RevokeDragDrop failed (may not have been registered): {:?}", e),
                }

                let drop_target = ContentAwareDropTarget::new(
                    container_hwnd,
                    comp_ctrl.clone(),
                    app_handle.clone(),
                    window_label.clone(),
                );
                let drop_target_interface: IDropTarget = drop_target.into();

                match RegisterDragDrop(container_hwnd, &drop_target_interface) {
                    Ok(()) => {
                        eprintln!("[TiddlyDesktop] Windows: Registered ContentAwareDropTarget on container HWND");
                        // Keep the drop target alive for the lifetime of the window
                        std::mem::forget(drop_target_interface);
                    }
                    Err(e) => {
                        eprintln!("[TiddlyDesktop] Windows: RegisterDragDrop failed: {:?}", e);
                    }
                }

                // Register DragStarting handler for outgoing drag detection
                if let Ok(controller5) = comp_ctrl.cast::<ICoreWebView2CompositionController5>() {
                    let handler: ICoreWebView2DragStartingEventHandler =
                        DragStartingHandler::new(window_label.clone()).into();
                    let mut token: i64 = 0;
                    match controller5.add_DragStarting(&handler, &mut token) {
                        Ok(()) => eprintln!("[TiddlyDesktop] Windows: DragStarting handler registered"),
                        Err(e) => eprintln!("[TiddlyDesktop] Windows: DragStarting registration failed: {:?}", e),
                    }
                }
            } else {
                eprintln!("[TiddlyDesktop] Windows: WARNING - Could not get composition controller");
            }

            eprintln!("[TiddlyDesktop] Windows: Drag-drop setup complete for '{}'", window_label);
        }
    });
}

// ============================================================================
// Content extraction helpers for incoming drags
// ============================================================================

/// Get raw bytes from a clipboard format in an IDataObject
unsafe fn get_format_bytes(data_object: &IDataObject, cf: u16) -> Option<Vec<u8>> {
    let format = FORMATETC {
        cfFormat: cf,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0 as u32,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };

    let medium = data_object.GetData(&format).ok()?;
    if medium.tymed != TYMED_HGLOBAL.0 as u32 || medium.u.hGlobal.0.is_null() {
        return None;
    }

    let ptr = GlobalLock(medium.u.hGlobal);
    if ptr.is_null() {
        return None;
    }

    let size = GlobalSize(medium.u.hGlobal);
    if size == 0 {
        let _ = GlobalUnlock(medium.u.hGlobal);
        return None;
    }

    let bytes = std::slice::from_raw_parts(ptr as *const u8, size).to_vec();
    let _ = GlobalUnlock(medium.u.hGlobal);
    Some(bytes)
}

/// Get a string from a clipboard format, using encoding::decode_string for auto-detection
unsafe fn get_format_string(data_object: &IDataObject, cf: u16) -> Option<String> {
    let bytes = get_format_bytes(data_object, cf)?;
    let s = encoding::decode_string(&bytes);
    if s.is_empty() { None } else { Some(s) }
}

/// Check if a clipboard format is available in an IDataObject
unsafe fn has_format(data_object: &IDataObject, cf: u16) -> bool {
    let format = FORMATETC {
        cfFormat: cf,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0 as u32,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    data_object.QueryGetData(&format) == S_OK
}

/// Check if the IDataObject has any content formats (not just files)
unsafe fn has_content_formats(data_object: &IDataObject) -> bool {
    has_format(data_object, CF_UNICODETEXT)
        || has_format(data_object, cf_html())
        || has_format(data_object, cf_tiddler())
        || has_format(data_object, cf_url_w())
        || has_format(data_object, cf_chromium_custom())
        || has_format(data_object, cf_moz_custom())
}

/// Extract file paths from CF_HDROP in an IDataObject
unsafe fn extract_file_paths(data_object: &IDataObject) -> Vec<String> {
    let mut paths = Vec::new();

    let format = FORMATETC {
        cfFormat: CF_HDROP,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0 as u32,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };

    if let Ok(medium) = data_object.GetData(&format) {
        if medium.tymed == TYMED_HGLOBAL.0 as u32 && !medium.u.hGlobal.0.is_null() {
            let hdrop = windows::Win32::UI::Shell::HDROP(medium.u.hGlobal.0);
            let count = DragQueryFileW(hdrop, 0xFFFFFFFF, None);

            for i in 0..count {
                let len = DragQueryFileW(hdrop, i, None);
                if len > 0 {
                    let mut buf = vec![0u16; (len + 1) as usize];
                    DragQueryFileW(hdrop, i, Some(&mut buf));
                    let path = String::from_utf16_lossy(&buf[..len as usize]);
                    paths.push(path);
                }
            }
        }
    }

    paths
}

/// Extract content from an IDataObject into a map of MIME type → content string.
/// Tries browser custom formats first (Chrome, Firefox), then falls back to individual formats.
/// Applies sanitization to HTML and URL content.
unsafe fn extract_content(data_object: &IDataObject) -> HashMap<String, String> {
    let mut result = HashMap::new();

    // Try Chrome custom data first (richest source - contains all MIME types)
    if let Some(bytes) = get_format_bytes(data_object, cf_chromium_custom()) {
        if let Some(custom_data) = encoding::parse_chromium_custom_data(&bytes) {
            eprintln!("[TiddlyDesktop] Windows: Extracted Chrome custom data with {} types", custom_data.len());
            for (mime, content) in custom_data {
                // Sanitize HTML content
                let content = if mime == "text/html" {
                    sanitize::sanitize_html(&content)
                } else {
                    content
                };
                result.insert(mime, content);
            }
            if !result.is_empty() {
                return result;
            }
        }
    }

    // Try Firefox custom data
    if let Some(bytes) = get_format_bytes(data_object, cf_moz_custom()) {
        if let Some(custom_data) = encoding::parse_moz_custom_clipdata(&bytes) {
            eprintln!("[TiddlyDesktop] Windows: Extracted Mozilla custom data with {} types", custom_data.len());
            for (mime, content) in custom_data {
                let content = if mime == "text/html" {
                    sanitize::sanitize_html(&content)
                } else {
                    content
                };
                result.insert(mime, content);
            }
            if !result.is_empty() {
                return result;
            }
        }
    }

    // Fall back to individual clipboard formats
    if let Some(text) = get_format_string(data_object, CF_UNICODETEXT) {
        result.insert("text/plain".to_string(), text);
    }

    if let Some(html_raw) = get_format_string(data_object, cf_html()) {
        let html = strip_cf_html_header(&html_raw);
        let html = sanitize::sanitize_html(&html);
        result.insert("text/html".to_string(), html);
    }

    if let Some(tiddler) = get_format_string(data_object, cf_tiddler()) {
        result.insert("text/vnd.tiddler".to_string(), tiddler);
    }

    if let Some(url) = get_format_string(data_object, cf_url_w()) {
        if !sanitize::is_dangerous_url(&url) {
            result.insert("text/uri-list".to_string(), url);
        } else {
            eprintln!("[TiddlyDesktop] Windows: Blocked dangerous URL from drag data");
        }
    }

    eprintln!("[TiddlyDesktop] Windows: Extracted {} individual format(s)", result.len());
    result
}

/// Strip the CF_HTML header to extract just the HTML fragment.
/// CF_HTML format wraps the actual HTML with header lines like:
/// ```
/// Version:0.9
/// StartHTML:0000000105
/// EndHTML:0000000199
/// StartFragment:0000000141
/// EndFragment:0000000163
/// ```
/// We extract the fragment between `<!--StartFragment-->` and `<!--EndFragment-->` markers.
fn strip_cf_html_header(html: &str) -> String {
    const START_MARKER: &str = "<!--StartFragment-->";
    const END_MARKER: &str = "<!--EndFragment-->";

    if let Some(start) = html.find(START_MARKER) {
        let content_start = start + START_MARKER.len();
        if let Some(end) = html[content_start..].find(END_MARKER) {
            return html[content_start..content_start + end].to_string();
        }
    }

    // If no markers found, try to skip the header lines and return everything after the blank line
    if let Some(pos) = html.find("\r\n\r\n") {
        return html[pos + 4..].to_string();
    }
    if let Some(pos) = html.find("\n\n") {
        return html[pos + 2..].to_string();
    }

    html.to_string()
}

/// Query OUTGOING_DRAG_STATE for info about the current outgoing drag.
/// Returns (is_our_drag, is_same_window, source_window, has_tiddler_data, is_text_selection_drag)
fn get_outgoing_drag_info(target_window: &str) -> (bool, bool, Option<String>, bool, bool) {
    if let Ok(state) = OUTGOING_DRAG_STATE.lock() {
        if let Some(ref s) = *state {
            let is_our_drag = true;
            let is_same_window = s.source_window_label == target_window;
            let source_window = Some(s.source_window_label.clone());
            let has_tiddler = s.data.text_vnd_tiddler.is_some();
            let is_text_sel = s.data.is_text_selection_drag;
            return (is_our_drag, is_same_window, source_window, has_tiddler, is_text_sel);
        }
    }
    (false, false, None, false, false)
}

// ============================================================================
// ContentAwareDropTarget - extracts content and file paths, forwards to WebView2
// ============================================================================

/// Serializable drag content data for td-drag-content events
#[derive(Clone, Debug, serde::Serialize)]
struct DragContentData {
    types: Vec<String>,
    data: HashMap<String, String>,
    #[serde(rename = "isSameWindow")]
    #[serde(skip_serializing_if = "Option::is_none")]
    is_same_window: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_text_selection_drag: Option<bool>,
}

#[implement(IDropTarget)]
struct ContentAwareDropTarget {
    hwnd: HWND,
    composition_controller: ICoreWebView2CompositionController3,
    app: tauri::AppHandle,
    window_label: String,
    /// Whether we're tracking an active content drag (non-file)
    drag_active: std::cell::UnsafeCell<bool>,
    /// Whether we're tracking a file drag
    file_drag_active: std::cell::UnsafeCell<bool>,
    /// File paths extracted during DragEnter
    current_paths: std::cell::UnsafeCell<Vec<String>>,
}

unsafe impl Send for ContentAwareDropTarget {}
unsafe impl Sync for ContentAwareDropTarget {}

impl ContentAwareDropTarget {
    fn new(
        hwnd: HWND,
        composition_controller: ICoreWebView2CompositionController3,
        app: tauri::AppHandle,
        window_label: String,
    ) -> Self {
        Self {
            hwnd,
            composition_controller,
            app,
            window_label,
            drag_active: std::cell::UnsafeCell::new(false),
            file_drag_active: std::cell::UnsafeCell::new(false),
            current_paths: std::cell::UnsafeCell::new(Vec::new()),
        }
    }

    unsafe fn to_client_coords(&self, pt: &POINTL) -> (i32, i32) {
        let mut client_pt = POINT { x: pt.x, y: pt.y };
        let _ = ScreenToClient(self.hwnd, &mut client_pt);
        (client_pt.x, client_pt.y)
    }
}

impl IDropTarget_Impl for ContentAwareDropTarget_Impl {
    fn DragEnter(
        &self,
        pdataobj: Ref<'_, IDataObject>,
        grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows_core::Result<()> {
        let (x, y) = unsafe { self.to_client_coords(pt) };
        eprintln!("[TiddlyDesktop] ContentAwareDropTarget::DragEnter at ({}, {}) client=({}, {})", pt.x, pt.y, x, y);

        let internal_state = INTERNAL_DRAG_STATE.load(Ordering::Acquire);
        let is_internal_tiddler = internal_state == DRAG_STATE_TIDDLER;

        if let Some(data_obj) = pdataobj.as_ref() {
            // Check for file drops first
            let paths = unsafe { extract_file_paths(data_obj) };

            if !paths.is_empty() && !is_internal_tiddler {
                // File drag - emit tauri://drag-enter and track as file drag
                eprintln!("[TiddlyDesktop] Windows: DragEnter: {} file(s)", paths.len());
                unsafe {
                    *self.current_paths.get() = paths.clone();
                    *self.file_drag_active.get() = true;
                }

                let _ = self.app.emit("tauri://drag-enter", serde_json::json!({
                    "paths": paths,
                    "position": { "x": x, "y": y }
                }));
            } else if !is_internal_tiddler {
                // Check for content formats (text, HTML, tiddler data, URLs, browser custom)
                let has_content = unsafe { has_content_formats(data_obj) };

                if has_content {
                    eprintln!("[TiddlyDesktop] Windows: DragEnter: content drag detected");
                    unsafe { *self.drag_active.get() = true; }

                    // Get outgoing drag info for cross-wiki detection
                    let (is_our_drag, is_same_window, source_window, has_tiddler, is_text_sel) =
                        get_outgoing_drag_info(&self.window_label);

                    let _ = self.app.emit("td-drag-motion", serde_json::json!({
                        "x": x,
                        "y": y,
                        "physicalPixels": true,
                        "isOurDrag": is_our_drag,
                        "isSameWindow": is_same_window,
                        "sourceWindow": source_window,
                        "targetWindow": self.window_label,
                        "hasTiddlerData": has_tiddler,
                        "isTextSelectionDrag": is_text_sel
                    }));

                    // Force COPY effect for content drags
                    unsafe { *pdweffect = DROPEFFECT_COPY; }
                }
            }
        }

        // Forward to WebView2 composition controller
        unsafe {
            let point = POINT { x: pt.x, y: pt.y };
            if let Err(e) = self.composition_controller.DragEnter(pdataobj.as_ref(), grfkeystate.0, point, pdweffect as *mut u32) {
                eprintln!("[TiddlyDesktop] Windows: DragEnter forward failed: {:?}", e);
            }

            // Override effect back to COPY for content drags (WebView2 may have changed it)
            if *self.drag_active.get() {
                *pdweffect = DROPEFFECT_COPY;
            }
        }

        Ok(())
    }

    fn DragOver(
        &self,
        grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows_core::Result<()> {
        let (x, y) = unsafe { self.to_client_coords(pt) };

        let is_content_drag = unsafe { *self.drag_active.get() };
        let is_file_drag = unsafe { *self.file_drag_active.get() };

        if is_content_drag {
            let (is_our_drag, is_same_window, source_window, has_tiddler, is_text_sel) =
                get_outgoing_drag_info(&self.window_label);

            let _ = self.app.emit("td-drag-motion", serde_json::json!({
                "x": x,
                "y": y,
                "physicalPixels": true,
                "isOurDrag": is_our_drag,
                "isSameWindow": is_same_window,
                "sourceWindow": source_window,
                "targetWindow": self.window_label,
                "hasTiddlerData": has_tiddler,
                "isTextSelectionDrag": is_text_sel
            }));
        } else if is_file_drag {
            let _ = self.app.emit("tauri://drag-over", serde_json::json!({
                "position": { "x": x, "y": y }
            }));
        }

        // Forward to WebView2
        unsafe {
            let point = POINT { x: pt.x, y: pt.y };
            if let Err(e) = self.composition_controller.DragOver(grfkeystate.0, point, pdweffect as *mut u32) {
                eprintln!("[TiddlyDesktop] Windows: DragOver forward failed: {:?}", e);
            }

            // Override effect for content drags
            if is_content_drag {
                *pdweffect = DROPEFFECT_COPY;
            }
        }

        Ok(())
    }

    fn DragLeave(&self) -> windows_core::Result<()> {
        let was_content_drag = unsafe { *self.drag_active.get() };
        let was_file_drag = unsafe { *self.file_drag_active.get() };

        eprintln!("[TiddlyDesktop] Windows: DragLeave (content={}, file={})", was_content_drag, was_file_drag);

        if was_content_drag {
            let (is_our_drag, _, _, _, _) = get_outgoing_drag_info(&self.window_label);
            let _ = self.app.emit("td-drag-leave", serde_json::json!({
                "isOurDrag": is_our_drag,
                "targetWindow": self.window_label
            }));
        }

        if was_file_drag {
            let _ = self.app.emit("tauri://drag-leave", serde_json::json!({}));
        }

        // Clear state
        unsafe {
            *self.drag_active.get() = false;
            *self.file_drag_active.get() = false;
            (*self.current_paths.get()).clear();
        }

        // Forward to WebView2
        unsafe {
            if let Err(e) = self.composition_controller.DragLeave() {
                eprintln!("[TiddlyDesktop] Windows: DragLeave forward failed: {:?}", e);
            }
        }

        // Clear internal drag state (safety net)
        INTERNAL_DRAG_STATE.store(DRAG_STATE_NONE, Ordering::Release);

        Ok(())
    }

    fn Drop(
        &self,
        pdataobj: Ref<'_, IDataObject>,
        grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows_core::Result<()> {
        let (x, y) = unsafe { self.to_client_coords(pt) };
        let was_content_drag = unsafe { *self.drag_active.get() };
        let was_file_drag = unsafe { *self.file_drag_active.get() };

        eprintln!("[TiddlyDesktop] Windows: Drop at ({}, {}) content={}, file={}", x, y, was_content_drag, was_file_drag);

        if let Some(data_obj) = pdataobj.as_ref() {
            if was_file_drag {
                // File drop
                let paths = unsafe { extract_file_paths(data_obj) };
                if !paths.is_empty() {
                    eprintln!("[TiddlyDesktop] Windows: File drop: {} files", paths.len());

                    let _ = self.app.emit("td-file-drop", serde_json::json!({
                        "paths": paths,
                        "targetWindow": self.window_label
                    }));

                    let _ = self.app.emit("tauri://drag-drop", serde_json::json!({
                        "paths": paths,
                        "position": { "x": x, "y": y }
                    }));
                }
            } else if was_content_drag {
                // Content drop - extract all content from IDataObject
                let content = unsafe { extract_content(data_obj) };

                if !content.is_empty() {
                    let (_, is_same_window, _, _, is_text_sel) = get_outgoing_drag_info(&self.window_label);

                    // Emit td-drag-drop-start
                    let _ = self.app.emit("td-drag-drop-start", serde_json::json!({
                        "x": x,
                        "y": y,
                        "physicalPixels": true
                    }));

                    // Emit td-drag-drop-position
                    let _ = self.app.emit("td-drag-drop-position", serde_json::json!({
                        "x": x,
                        "y": y,
                        "physicalPixels": true,
                        "targetWindow": self.window_label
                    }));

                    // Emit td-drag-content with extracted data
                    let types: Vec<String> = content.keys().cloned().collect();
                    eprintln!("[TiddlyDesktop] Windows: Content drop - emitting td-drag-content with {} types: {:?}",
                        types.len(), types);

                    let content_data = DragContentData {
                        types,
                        data: content,
                        is_same_window: if is_same_window { Some(true) } else { None },
                        is_text_selection_drag: if is_text_sel { Some(true) } else { None },
                    };
                    let _ = self.app.emit("td-drag-content", &content_data);
                }
            }
        }

        // Clear state
        unsafe {
            *self.drag_active.get() = false;
            *self.file_drag_active.get() = false;
            (*self.current_paths.get()).clear();
        }

        // Forward to WebView2
        unsafe {
            let point = POINT { x: pt.x, y: pt.y };
            if let Err(e) = self.composition_controller.Drop(pdataobj.as_ref(), grfkeystate.0, point, pdweffect as *mut u32) {
                eprintln!("[TiddlyDesktop] Windows: Drop forward failed: {:?}", e);
            }
        }

        // Clear internal drag state
        INTERNAL_DRAG_STATE.store(DRAG_STATE_NONE, Ordering::Release);

        Ok(())
    }
}

// ============================================================================
// Outgoing drag support
// ============================================================================

struct OutgoingDragState {
    data: OutgoingDragData,
    source_window_label: String,
}

lazy_static::lazy_static! {
    static ref OUTGOING_DRAG_STATE: Mutex<Option<OutgoingDragState>> = Mutex::new(None);
}

/// Prepare data for a native drag operation
pub fn prepare_native_drag(window: &WebviewWindow, data: OutgoingDragData) -> Result<(), String> {
    let label = window.label().to_string();
    eprintln!("[TiddlyDesktop] Windows: prepare_native_drag for '{}'", label);

    if let Ok(mut state) = OUTGOING_DRAG_STATE.lock() {
        *state = Some(OutgoingDragState {
            data,
            source_window_label: label,
        });
    }
    Ok(())
}

/// Start a native drag operation with OLE DoDragDrop
pub fn start_native_drag(
    window: &WebviewWindow,
    data: OutgoingDragData,
    _x: i32,
    _y: i32,
    _image_data: Option<Vec<u8>>,
    _image_offset_x: Option<i32>,
    _image_offset_y: Option<i32>,
) -> Result<(), String> {
    let label = window.label().to_string();
    eprintln!("[TiddlyDesktop] Windows: start_native_drag for '{}'", label);

    // Store the data
    if let Ok(mut state) = OUTGOING_DRAG_STATE.lock() {
        *state = Some(OutgoingDragState {
            data: data.clone(),
            source_window_label: label,
        });
    }

    // Start the OLE drag operation on a separate thread
    std::thread::spawn(move || {
        unsafe {
            let _ = OleInitialize(None);

            let data_object: IDataObject = DataObjectImpl::new(data).into();
            let drop_source: IDropSource = DropSourceImpl::new().into();

            let mut effect = DROPEFFECT_NONE;
            let allowed = DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK;

            let result = DoDragDrop(&data_object, &drop_source, allowed, &mut effect);
            eprintln!("[TiddlyDesktop] Windows: DoDragDrop result: {:?}, effect: {:?}", result, effect);
        }
    });

    Ok(())
}

/// Clean up after a drag operation
pub fn cleanup_native_drag() -> Result<(), String> {
    eprintln!("[TiddlyDesktop] Windows: cleanup_native_drag");

    // Clear outgoing drag state
    if let Ok(mut state) = OUTGOING_DRAG_STATE.lock() {
        *state = None;
    }

    // Also clear internal drag state to handle cases where:
    // 1. User pressed Escape to cancel
    // 2. Drag ended outside our windows
    // 3. Any other abnormal termination
    // This is a safety net - normally wry calls tiddlydesktop_clear_internal_drag()
    INTERNAL_DRAG_STATE.store(DRAG_STATE_NONE, Ordering::Release);

    Ok(())
}

/// Get pending drag data for cross-wiki drops
pub fn get_pending_drag_data(_target_window: &str) -> Option<OutgoingDragData> {
    if let Ok(state) = OUTGOING_DRAG_STATE.lock() {
        state.as_ref().map(|s| {
            let mut data = s.data.clone();
            data.source_window = Some(s.source_window_label.clone());
            data
        })
    } else {
        None
    }
}

/// Take pending file paths (not used - handled by hook)
pub fn take_pending_file_paths() -> Option<Vec<String>> {
    None
}

// ============================================================================
// DragStarting handler for cross-wiki drag detection
// ============================================================================

#[implement(ICoreWebView2DragStartingEventHandler)]
struct DragStartingHandler {
    window_label: String,
}

impl DragStartingHandler {
    fn new(window_label: String) -> Self {
        Self { window_label }
    }
}

impl ICoreWebView2DragStartingEventHandler_Impl for DragStartingHandler_Impl {
    fn Invoke(
        &self,
        _sender: Ref<'_, ICoreWebView2CompositionController>,
        args: Ref<'_, ICoreWebView2DragStartingEventArgs>,
    ) -> windows_core::Result<()> {
        eprintln!("[TiddlyDesktop] Windows: DragStarting event from '{}'", self.window_label);

        // Determine the type of internal drag:
        // 1. Tiddler/$draggable drag (has text/vnd.tiddler) - skip dropzone AND skip forwarding
        // 2. Link drag (has URL data but no tiddler) - skip dropzone, but DO forward
        // 3. Text selection drag (has neither) - activate dropzone, DO forward
        let drag_type = if let Some(args) = args.as_ref() {
            unsafe {
                if let Ok(data_object) = args.Data() {
                    // Check for text/vnd.tiddler format (tiddler/$draggable drag)
                    let tiddler_format = FORMATETC {
                        cfFormat: cf_tiddler(),
                        ptd: std::ptr::null_mut(),
                        dwAspect: DVASPECT_CONTENT.0 as u32,
                        lindex: -1,
                        tymed: TYMED_HGLOBAL.0 as u32,
                    };
                    let has_tiddler_data = data_object.QueryGetData(&tiddler_format) == S_OK;

                    // Check for URL format (link drag)
                    let url_format = FORMATETC {
                        cfFormat: cf_url_w(),
                        ptd: std::ptr::null_mut(),
                        dwAspect: DVASPECT_CONTENT.0 as u32,
                        lindex: -1,
                        tymed: TYMED_HGLOBAL.0 as u32,
                    };
                    let has_url_data = data_object.QueryGetData(&url_format) == S_OK;

                    // Also check text/x-moz-url (Firefox-style URL format)
                    let moz_url_format = FORMATETC {
                        cfFormat: cf_moz_url(),
                        ptd: std::ptr::null_mut(),
                        dwAspect: DVASPECT_CONTENT.0 as u32,
                        lindex: -1,
                        tymed: TYMED_HGLOBAL.0 as u32,
                    };
                    let has_moz_url = data_object.QueryGetData(&moz_url_format) == S_OK;

                    eprintln!("[TiddlyDesktop] Windows: DragStarting - has_tiddler_data={}, has_url_data={}, has_moz_url={}",
                        has_tiddler_data, has_url_data, has_moz_url);

                    // Determine drag type based on data formats
                    if has_tiddler_data {
                        InternalDragType::Tiddler
                    } else if has_url_data || has_moz_url {
                        InternalDragType::Link
                    } else {
                        InternalDragType::TextSelection
                    }
                } else {
                    eprintln!("[TiddlyDesktop] Windows: DragStarting - could not get data object");
                    InternalDragType::TextSelection // Assume text selection if we can't check
                }
            }
        } else {
            eprintln!("[TiddlyDesktop] Windows: DragStarting - no args");
            InternalDragType::TextSelection // Assume text selection if no args
        };

        // Set the internal drag state
        // This is read by the wry fork's CompositionDragDropTarget via FFI
        set_internal_drag_active(drag_type);

        // CRITICAL: For non-text-selection drags, set $tw.dragInProgress via JavaScript
        // BEFORE the HTML5 dragenter event reaches the dropzone. This prevents the
        // dropzone from activating for internal drags (tiddler links, $draggables).
        // Text selection drags SHOULD activate the dropzone so users can drop text.
        if drag_type != InternalDragType::TextSelection {
            if let Ok(handle) = APP_HANDLE.lock() {
                if let Some(app) = handle.as_ref() {
                    if let Some(window) = app.get_webview_window(&self.window_label) {
                        // Set $tw.dragInProgress to true - dropzone.js checks this and ignores the drag
                        let _ = window.eval("if(typeof $tw !== 'undefined') { $tw.dragInProgress = true; }");
                        eprintln!("[TiddlyDesktop] Windows: Set $tw.dragInProgress = true via eval");
                    }
                }
            }
        }

        Ok(())
    }
}

// ============================================================================
// IDataObject implementation for outgoing drags
// ============================================================================

#[implement(IDataObject)]
struct DataObjectImpl {
    data: OutgoingDragData,
}

impl DataObjectImpl {
    fn new(data: OutgoingDragData) -> Self {
        Self { data }
    }
}

impl IDataObject_Impl for DataObjectImpl_Impl {
    fn GetData(&self, pformatetc: *const FORMATETC) -> windows_core::Result<STGMEDIUM> {
        unsafe {
            if pformatetc.is_null() {
                return Err(windows_core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG));
            }

            let format = &*pformatetc;
            let cf = format.cfFormat;

            let (content, is_unicode) = if cf == CF_UNICODETEXT {
                (self.data.text_plain.as_ref(), true)
            } else if cf == cf_html() {
                (self.data.text_html.as_ref(), false)
            } else if cf == cf_tiddler() {
                (self.data.text_vnd_tiddler.as_ref(), true)
            } else if cf == cf_url_w() || cf == cf_moz_url() {
                (self.data.text_uri_list.as_ref().or(self.data.url.as_ref()), true)
            } else {
                return Err(windows_core::Error::from_hresult(windows::Win32::Foundation::DV_E_FORMATETC));
            };

            let Some(content) = content else {
                return Err(windows_core::Error::from_hresult(windows::Win32::Foundation::DV_E_FORMATETC));
            };

            let hglobal = if is_unicode {
                let utf16: Vec<u16> = content.encode_utf16().chain(std::iter::once(0)).collect();
                let size = utf16.len() * 2;
                let hglobal = GlobalAlloc(GMEM_MOVEABLE, size)
                    .map_err(|_| windows_core::Error::from_hresult(windows::Win32::Foundation::E_OUTOFMEMORY))?;
                let ptr = GlobalLock(hglobal);
                std::ptr::copy_nonoverlapping(utf16.as_ptr(), ptr as *mut u16, utf16.len());
                let _ = GlobalUnlock(hglobal);
                hglobal
            } else {
                let bytes = content.as_bytes();
                let size = bytes.len() + 1;
                let hglobal = GlobalAlloc(GMEM_MOVEABLE, size)
                    .map_err(|_| windows_core::Error::from_hresult(windows::Win32::Foundation::E_OUTOFMEMORY))?;
                let ptr = GlobalLock(hglobal);
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
                *(ptr as *mut u8).add(bytes.len()) = 0;
                let _ = GlobalUnlock(hglobal);
                hglobal
            };

            let mut medium = STGMEDIUM::default();
            medium.tymed = TYMED_HGLOBAL.0 as u32;
            medium.u.hGlobal = hglobal;
            medium.pUnkForRelease = std::mem::ManuallyDrop::new(None);

            Ok(medium)
        }
    }

    fn GetDataHere(&self, _pformatetc: *const FORMATETC, _pmedium: *mut STGMEDIUM) -> windows_core::Result<()> {
        Err(windows_core::Error::from_hresult(windows::Win32::Foundation::E_NOTIMPL))
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        unsafe {
            if pformatetc.is_null() {
                return windows::Win32::Foundation::E_INVALIDARG;
            }
            let cf = (*pformatetc).cfFormat;

            let supported = match () {
                _ if cf == CF_UNICODETEXT => self.data.text_plain.is_some(),
                _ if cf == cf_html() => self.data.text_html.is_some(),
                _ if cf == cf_tiddler() => self.data.text_vnd_tiddler.is_some(),
                _ if cf == cf_url_w() || cf == cf_moz_url() => {
                    self.data.text_uri_list.is_some() || self.data.url.is_some()
                }
                _ => false,
            };

            if supported { S_OK } else { windows::Win32::Foundation::DV_E_FORMATETC }
        }
    }

    fn GetCanonicalFormatEtc(&self, _pformatectin: *const FORMATETC, pformatetcout: *mut FORMATETC) -> HRESULT {
        unsafe {
            if !pformatetcout.is_null() {
                (*pformatetcout).ptd = std::ptr::null_mut();
            }
        }
        windows::Win32::Foundation::DATA_S_SAMEFORMATETC
    }

    fn SetData(&self, _pformatetc: *const FORMATETC, _pmedium: *const STGMEDIUM, _frelease: BOOL) -> windows_core::Result<()> {
        Err(windows_core::Error::from_hresult(windows::Win32::Foundation::E_NOTIMPL))
    }

    fn EnumFormatEtc(&self, _dwdirection: u32) -> windows_core::Result<IEnumFORMATETC> {
        Err(windows_core::Error::from_hresult(windows::Win32::Foundation::E_NOTIMPL))
    }

    fn DAdvise(&self, _pformatetc: *const FORMATETC, _advf: u32, _padvsink: Ref<'_, IAdviseSink>) -> windows_core::Result<u32> {
        Err(windows_core::Error::from_hresult(windows::Win32::Foundation::OLE_E_ADVISENOTSUPPORTED))
    }

    fn DUnadvise(&self, _dwconnection: u32) -> windows_core::Result<()> {
        Err(windows_core::Error::from_hresult(windows::Win32::Foundation::OLE_E_ADVISENOTSUPPORTED))
    }

    fn EnumDAdvise(&self) -> windows_core::Result<IEnumSTATDATA> {
        Err(windows_core::Error::from_hresult(windows::Win32::Foundation::OLE_E_ADVISENOTSUPPORTED))
    }
}

// ============================================================================
// IDropSource implementation for outgoing drags
// ============================================================================

#[implement(IDropSource)]
struct DropSourceImpl {
    _button_pressed: AtomicU32,
}

impl DropSourceImpl {
    fn new() -> Self {
        Self {
            _button_pressed: AtomicU32::new(1),
        }
    }
}

impl IDropSource_Impl for DropSourceImpl_Impl {
    fn QueryContinueDrag(&self, fescapepressed: BOOL, grfkeystate: MODIFIERKEYS_FLAGS) -> HRESULT {
        if fescapepressed.as_bool() {
            return DRAGDROP_S_CANCEL;
        }

        let left = (grfkeystate.0 & MK_LBUTTON) != 0;
        let right = (grfkeystate.0 & MK_RBUTTON) != 0;

        if !left && !right {
            return DRAGDROP_S_DROP;
        }

        S_OK
    }

    fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}
