//! Windows drag-drop handling using custom IDropTarget for content extraction
//!
//! WebView2's native drag-drop (SetAllowExternalDrop) only works for files, not content
//! (text, HTML, URLs) from external apps. We use a custom IDropTarget to:
//! 1. Extract content from IDataObject (the OLE drag data)
//! 2. Emit td-drag-* events to JavaScript
//! 3. Let JavaScript create synthetic DOM events for TiddlyWiki
//!
//! Internal drags (within the webview) are handled by JavaScript:
//! - internal_drag.js intercepts dragstart for draggable elements and text selections
//! - td-drag-* handlers check TD.isInternalDragActive() and skip if true
//! - internal_drag.js creates synthetic drag events using mouse tracking

#![cfg(target_os = "windows")]

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use tauri::{Emitter, WebviewWindow};

use super::sanitize::{sanitize_html, sanitize_uri_list, sanitize_file_paths, is_dangerous_url};
use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Controller4;
use windows::core::{GUID, HRESULT};
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, POINTL, E_NOINTERFACE, E_POINTER, S_OK};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::Globalization::{MultiByteToWideChar, CP_ACP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS};
use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{
    IDropTarget, OleInitialize, RegisterDragDrop, RevokeDragDrop, DROPEFFECT, DROPEFFECT_COPY,
    DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{EnumChildWindows, GetClassNameW, GetPropW};

/// Thread-safe wrapper for our drop target
#[allow(dead_code)]
struct SendDropTarget(*mut DropTargetImpl);
unsafe impl Send for SendDropTarget {}
unsafe impl Sync for SendDropTarget {}

// Global state for registered drop targets (keyed by HWND as isize)
lazy_static::lazy_static! {
    static ref DROP_TARGET_MAP: Mutex<HashMap<isize, SendDropTarget>> = Mutex::new(HashMap::new());
}

/// Clipboard format constants
const CF_TEXT: u16 = 1;
const CF_UNICODETEXT: u16 = 13;
const CF_HDROP: u16 = 15;

/// IDropTarget interface GUID
const IID_IDROPTARGET: GUID = GUID::from_u128(0x00000122_0000_0000_c000_000000000046);
const IID_IUNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);

/// Custom clipboard format for HTML
fn get_cf_html() -> u16 {
    use windows::core::w;
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    unsafe { RegisterClipboardFormatW(w!("HTML Format")) as u16 }
}

/// Custom clipboard format for URI list
fn get_cf_uri_list() -> u16 {
    use windows::core::w;
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    unsafe { RegisterClipboardFormatW(w!("text/uri-list")) as u16 }
}

/// Custom clipboard format for TiddlyWiki tiddler
fn get_cf_tiddler() -> u16 {
    use windows::core::w;
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    unsafe { RegisterClipboardFormatW(w!("text/vnd.tiddler")) as u16 }
}

/// Standard Windows clipboard format for URLs (ANSI)
fn get_cf_url() -> u16 {
    use windows::core::w;
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    unsafe { RegisterClipboardFormatW(w!("UniformResourceLocator")) as u16 }
}

/// Standard Windows clipboard format for URLs (Unicode)
fn get_cf_url_w() -> u16 {
    use windows::core::w;
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    unsafe { RegisterClipboardFormatW(w!("UniformResourceLocatorW")) as u16 }
}

use super::encoding::decode_string;

/// Mozilla URL format
fn get_cf_moz_url() -> u16 {
    use windows::core::w;
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    unsafe { RegisterClipboardFormatW(w!("text/x-moz-url")) as u16 }
}

/// Mozilla custom clipdata format (contains custom MIME types like text/vnd.tiddler)
fn get_cf_moz_custom_clipdata() -> u16 {
    use windows::core::w;
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    unsafe { RegisterClipboardFormatW(w!("application/x-moz-custom-clipdata")) as u16 }
}

/// Chrome custom data format (Pickle format with custom MIME types)
fn get_cf_chromium_custom_data() -> u16 {
    use windows::core::w;
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    unsafe { RegisterClipboardFormatW(w!("Chromium Web Custom MIME Data Format")) as u16 }
}

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

/// IDropTarget vtable - must match COM layout exactly
#[repr(C)]
#[allow(non_snake_case)]
struct IDropTargetVtbl {
    QueryInterface: unsafe extern "system" fn(
        this: *mut DropTargetImpl,
        riid: *const GUID,
        ppv_object: *mut *mut std::ffi::c_void,
    ) -> HRESULT,
    AddRef: unsafe extern "system" fn(this: *mut DropTargetImpl) -> u32,
    Release: unsafe extern "system" fn(this: *mut DropTargetImpl) -> u32,
    DragEnter: unsafe extern "system" fn(
        this: *mut DropTargetImpl,
        pDataObj: *mut std::ffi::c_void,
        grfKeyState: u32,
        pt: POINTL,
        pdwEffect: *mut u32,
    ) -> HRESULT,
    DragOver: unsafe extern "system" fn(
        this: *mut DropTargetImpl,
        grfKeyState: u32,
        pt: POINTL,
        pdwEffect: *mut u32,
    ) -> HRESULT,
    DragLeave: unsafe extern "system" fn(this: *mut DropTargetImpl) -> HRESULT,
    Drop: unsafe extern "system" fn(
        this: *mut DropTargetImpl,
        pDataObj: *mut std::ffi::c_void,
        grfKeyState: u32,
        pt: POINTL,
        pdwEffect: *mut u32,
    ) -> HRESULT,
}

/// Our IDropTarget implementation
#[repr(C)]
struct DropTargetImpl {
    vtbl: *const IDropTargetVtbl,
    ref_count: AtomicU32,
    window: WebviewWindow,
    drag_active: Mutex<bool>,
    hwnd: isize,
    /// Original IDropTarget from WebView2 (if any) - called for same-window drops on editable elements
    original_drop_target: Option<IDropTarget>,
}

/// Static vtable instance
static DROPTARGET_VTBL: IDropTargetVtbl = IDropTargetVtbl {
    QueryInterface: DropTargetImpl::query_interface,
    AddRef: DropTargetImpl::add_ref,
    Release: DropTargetImpl::release,
    DragEnter: DropTargetImpl::drag_enter,
    DragOver: DropTargetImpl::drag_over,
    DragLeave: DropTargetImpl::drag_leave,
    Drop: DropTargetImpl::drop_impl,
};

impl DropTargetImpl {
    fn new(window: WebviewWindow, hwnd: HWND, original_drop_target: Option<IDropTarget>) -> *mut Self {
        let obj = Box::new(Self {
            vtbl: &DROPTARGET_VTBL,
            ref_count: AtomicU32::new(1),
            window,
            drag_active: Mutex::new(false),
            hwnd: hwnd.0 as isize,
            original_drop_target,
        });
        Box::into_raw(obj)
    }

    unsafe fn as_idroptarget(ptr: *mut Self) -> IDropTarget {
        std::mem::transmute(ptr)
    }

    /// Convert screen coordinates (from POINTL) to client coordinates relative to our HWND
    fn screen_to_client_coords(&self, pt: POINTL) -> (i32, i32) {
        let mut client_point = POINT { x: pt.x, y: pt.y };
        unsafe {
            let hwnd = HWND(self.hwnd as *mut _);
            // ScreenToClient converts screen coordinates to client coordinates
            // relative to the specified window's client area
            if ScreenToClient(hwnd, &mut client_point).as_bool() {
                (client_point.x, client_point.y)
            } else {
                // Fallback to original coordinates if conversion fails
                eprintln!("[TiddlyDesktop] Windows: ScreenToClient failed, using raw coordinates");
                (pt.x, pt.y)
            }
        }
    }

    unsafe extern "system" fn query_interface(
        this: *mut Self,
        riid: *const GUID,
        ppv_object: *mut *mut std::ffi::c_void,
    ) -> HRESULT {
        if ppv_object.is_null() {
            return E_POINTER;
        }
        let iid = &*riid;
        if *iid == IID_IUNKNOWN || *iid == IID_IDROPTARGET {
            Self::add_ref(this);
            *ppv_object = this as *mut std::ffi::c_void;
            S_OK
        } else {
            *ppv_object = std::ptr::null_mut();
            E_NOINTERFACE
        }
    }

    unsafe extern "system" fn add_ref(this: *mut Self) -> u32 {
        let obj = &*this;
        obj.ref_count.fetch_add(1, Ordering::SeqCst) + 1
    }

    unsafe extern "system" fn release(this: *mut Self) -> u32 {
        let obj = &*this;
        let count = obj.ref_count.fetch_sub(1, Ordering::SeqCst) - 1;
        if count == 0 {
            drop(Box::from_raw(this));
        }
        count
    }

    /// Check if there's an active outgoing drag from this specific window (same-window drag)
    fn is_same_window_drag(&self) -> bool {
        if let Ok(guard) = OUTGOING_DRAG_STATE.lock() {
            if let Some(state) = guard.as_ref() {
                return state.source_window_label == self.window.label();
            }
        }
        false
    }

    /// Check if there's any active outgoing drag from our app (cross-wiki or same-window)
    fn is_any_outgoing_drag() -> bool {
        if let Ok(guard) = OUTGOING_DRAG_STATE.lock() {
            return guard.is_some();
        }
        false
    }

    /// Get the source window label if there's an active outgoing drag
    fn get_source_window_label() -> Option<String> {
        if let Ok(guard) = OUTGOING_DRAG_STATE.lock() {
            if let Some(state) = guard.as_ref() {
                return Some(state.source_window_label.clone());
            }
        }
        None
    }

    /// Check if the outgoing drag has tiddler data
    fn has_tiddler_data() -> bool {
        if let Ok(guard) = OUTGOING_DRAG_STATE.lock() {
            if let Some(state) = guard.as_ref() {
                return state.data.text_vnd_tiddler.is_some();
            }
        }
        false
    }

    /// Check if the outgoing drag is a text selection drag
    fn is_text_selection_drag() -> bool {
        if let Ok(guard) = OUTGOING_DRAG_STATE.lock() {
            if let Some(state) = guard.as_ref() {
                return state.data.is_text_selection_drag;
            }
        }
        false
    }

    // IDropTarget::DragEnter - native event when drag enters this window
    unsafe extern "system" fn drag_enter(
        this: *mut Self,
        p_data_obj: *mut std::ffi::c_void,
        _grf_key_state: u32,
        pt: POINTL,
        pdw_effect: *mut u32,
    ) -> HRESULT {
        let obj = &*this;
        *obj.drag_active.lock().unwrap() = true;

        // Convert screen coordinates to client coordinates
        let (client_x, client_y) = obj.screen_to_client_coords(pt);

        // Check if this drag is from our app (any window) - for cross-wiki detection
        let is_our_drag = Self::is_any_outgoing_drag();
        let source_window_label = Self::get_source_window_label();
        let is_same_window = obj.is_same_window_drag();
        let has_tiddler = Self::has_tiddler_data();
        let is_text_sel = Self::is_text_selection_drag();

        eprintln!(
            "[TiddlyDesktop] Windows IDropTarget::DragEnter at ({}, {}), isOurDrag={}, sourceWindow={:?}, isSameWindow={}",
            client_x, client_y, is_our_drag, source_window_label, is_same_window
        );

        // Log available formats for debugging
        if !p_data_obj.is_null() {
            // Borrow the IDataObject without taking ownership (ManuallyDrop prevents Release)
            let data_object: std::mem::ManuallyDrop<IDataObject> = std::mem::ManuallyDrop::new(
                std::mem::transmute(p_data_obj)
            );
            obj.log_available_formats(&data_object);
        }

        // Emit td-drag-motion with full context for cross-wiki support
        // Include hasTiddlerData and isTextSelectionDrag for Issue 4b handling in JS
        let _ = obj.window.emit(
            "td-drag-motion",
            serde_json::json!({
                "x": client_x,
                "y": client_y,
                "screenCoords": false,
                "physicalPixels": true,
                "isOurDrag": is_our_drag,
                "isSameWindow": is_same_window,
                "sourceWindowLabel": source_window_label,
                "windowLabel": obj.window.label(),
                "hasTiddlerData": has_tiddler,
                "isTextSelectionDrag": is_text_sel
            }),
        );

        // Accept the drag
        if !pdw_effect.is_null() {
            let allowed = DROPEFFECT(*pdw_effect);
            *pdw_effect = choose_drop_effect(allowed).0 as u32;
        }

        S_OK
    }

    // IDropTarget::DragOver - native event when drag moves over this window
    unsafe extern "system" fn drag_over(
        this: *mut Self,
        _grf_key_state: u32,
        pt: POINTL,
        pdw_effect: *mut u32,
    ) -> HRESULT {
        let obj = &*this;
        {
            let mut active = obj.drag_active.lock().unwrap();
            if !*active {
                *active = true;
            }
        }

        // Convert screen coordinates to client coordinates
        let (client_x, client_y) = obj.screen_to_client_coords(pt);

        // Check if this drag is from our app (any window) - for cross-wiki detection
        let is_our_drag = Self::is_any_outgoing_drag();
        let source_window_label = Self::get_source_window_label();
        let is_same_window = obj.is_same_window_drag();
        let has_tiddler = Self::has_tiddler_data();
        let is_text_sel = Self::is_text_selection_drag();

        // Rate-limited logging
        static LAST_LOG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let last = LAST_LOG.load(std::sync::atomic::Ordering::Relaxed);
        if now - last > 500 {
            LAST_LOG.store(now, std::sync::atomic::Ordering::Relaxed);
            eprintln!(
                "[TiddlyDesktop] Windows IDropTarget::DragOver at ({}, {}), isOurDrag={}, isSameWindow={}",
                client_x, client_y, is_our_drag, is_same_window
            );
        }

        // Emit td-drag-motion with full context for cross-wiki support
        // Include hasTiddlerData and isTextSelectionDrag for Issue 4b handling in JS
        let _ = obj.window.emit(
            "td-drag-motion",
            serde_json::json!({
                "x": client_x,
                "y": client_y,
                "screenCoords": false,
                "physicalPixels": true,
                "isOurDrag": is_our_drag,
                "isSameWindow": is_same_window,
                "sourceWindowLabel": source_window_label,
                "windowLabel": obj.window.label(),
                "hasTiddlerData": has_tiddler,
                "isTextSelectionDrag": is_text_sel
            }),
        );

        if !pdw_effect.is_null() {
            let allowed = DROPEFFECT(*pdw_effect);
            *pdw_effect = choose_drop_effect(allowed).0 as u32;
        }

        S_OK
    }

    // IDropTarget::DragLeave - native event when drag leaves this window
    unsafe extern "system" fn drag_leave(this: *mut Self) -> HRESULT {
        let obj = &*this;

        // Check if this drag is from our app (any window) - for cross-wiki detection
        let is_our_drag = Self::is_any_outgoing_drag();
        let source_window_label = Self::get_source_window_label();
        let is_same_window = obj.is_same_window_drag();

        eprintln!(
            "[TiddlyDesktop] Windows IDropTarget::DragLeave, isOurDrag={}, sourceWindow={:?}, isSameWindow={}",
            is_our_drag, source_window_label, is_same_window
        );

        let was_active = {
            let mut active = obj.drag_active.lock().unwrap();
            let was = *active;
            *active = false;
            was
        };

        if was_active {
            let _ = obj.window.emit("td-drag-leave", serde_json::json!({
                "isOurDrag": is_our_drag,
                "isSameWindow": is_same_window,
                "sourceWindowLabel": source_window_label,
                "windowLabel": obj.window.label()
            }));
        }

        S_OK
    }

    // IDropTarget::Drop - native event when drop occurs on this window
    unsafe extern "system" fn drop_impl(
        this: *mut Self,
        p_data_obj: *mut std::ffi::c_void,
        _grf_key_state: u32,
        pt: POINTL,
        pdw_effect: *mut u32,
    ) -> HRESULT {
        let obj = &*this;
        *obj.drag_active.lock().unwrap() = false;

        // Convert screen coordinates to client coordinates
        let (client_x, client_y) = obj.screen_to_client_coords(pt);

        // Check if this drag is from our app (any window) - for cross-wiki detection
        let is_our_drag = Self::is_any_outgoing_drag();
        let source_window_label = Self::get_source_window_label();
        let is_same_window = obj.is_same_window_drag();

        eprintln!(
            "[TiddlyDesktop] Windows IDropTarget::Drop at ({}, {}), isOurDrag={}, sourceWindow={:?}, isSameWindow={}",
            client_x, client_y, is_our_drag, source_window_label, is_same_window
        );

        // For same-window drags, first let the browser try to handle it natively.
        // This is critical for editable elements (input/textarea/contenteditable) which
        // need browser-trusted drop events to work correctly.
        if is_same_window {
            eprintln!("[TiddlyDesktop] Windows IDropTarget::Drop - same-window drag, trying native handler first");

            // Call the original IDropTarget if we have one
            let browser_handled = if let Some(ref original) = obj.original_drop_target {
                eprintln!("[TiddlyDesktop] Windows IDropTarget::Drop - calling original IDropTarget");
                // Borrow the IDataObject without taking ownership (ManuallyDrop prevents Release)
                let data_obj_borrowed: std::mem::ManuallyDrop<IDataObject> = std::mem::ManuallyDrop::new(
                    std::mem::transmute(p_data_obj)
                );
                let result = original.Drop(
                    &*data_obj_borrowed,
                    MODIFIERKEYS_FLAGS(_grf_key_state),
                    pt,
                    pdw_effect as *mut DROPEFFECT
                );
                result.is_ok()
            } else {
                eprintln!("[TiddlyDesktop] Windows IDropTarget::Drop - no original IDropTarget available");
                false
            };

            if browser_handled {
                // Browser handled it (e.g., drop on editable element)
                eprintln!("[TiddlyDesktop] Windows IDropTarget::Drop - browser handled same-window drop natively");
                return S_OK;
            }

            // Browser didn't handle it - emit td-drag-content for $droppable handlers etc.
            eprintln!("[TiddlyDesktop] Windows IDropTarget::Drop - browser didn't handle, emitting td-drag-content");
            let _ = obj.window.emit(
                "td-drag-drop-position",
                serde_json::json!({
                    "x": client_x,
                    "y": client_y,
                    "screenCoords": false,
                    "physicalPixels": true,
                    "isOurDrag": true,
                    "isSameWindow": true,
                    "sourceWindowLabel": source_window_label,
                    "windowLabel": obj.window.label()
                }),
            );

            // Get the stored drag data and emit td-drag-content so JS can process the drop.
            if let Ok(guard) = OUTGOING_DRAG_STATE.lock() {
                if let Some(state) = guard.as_ref() {
                    let mut types = Vec::new();
                    let mut data = HashMap::new();

                    // Include text/vnd.tiddler for $droppable handlers
                    if let Some(ref tiddler) = state.data.text_vnd_tiddler {
                        types.push("text/vnd.tiddler".to_string());
                        data.insert("text/vnd.tiddler".to_string(), tiddler.clone());
                    }
                    if let Some(ref text) = state.data.text_plain {
                        types.push("text/plain".to_string());
                        data.insert("text/plain".to_string(), text.clone());
                    }

                    if !types.is_empty() {
                        eprintln!(
                            "[TiddlyDesktop] Windows IDropTarget::Drop - emitting td-drag-content for same-window drag, types: {:?}",
                            types
                        );
                        let content_data = DragContentData {
                            types,
                            data,
                            is_text_selection_drag: if state.data.is_text_selection_drag { Some(true) } else { None },
                            is_same_window: Some(true)
                        };
                        let _ = obj.window.emit("td-drag-content", &content_data);
                    }
                }
            }

            if !pdw_effect.is_null() {
                *pdw_effect = DROPEFFECT_COPY.0 as u32;
            }
            return S_OK;
        }

        if !p_data_obj.is_null() {
            // Borrow the IDataObject without taking ownership (ManuallyDrop prevents Release)
            let data_object: std::mem::ManuallyDrop<IDataObject> = std::mem::ManuallyDrop::new(
                std::mem::transmute(p_data_obj)
            );
            obj.log_available_formats(&data_object);

            // Emit drop-start
            let _ = obj.window.emit(
                "td-drag-drop-start",
                serde_json::json!({
                    "x": client_x,
                    "y": client_y,
                    "screenCoords": false,
                    "physicalPixels": true
                }),
            );

            // Check for file paths first
            let file_paths = obj.get_file_paths(&data_object);
            // Security: Sanitize file paths to prevent path traversal
            let file_paths = sanitize_file_paths(file_paths);
            if !file_paths.is_empty() {
                eprintln!(
                    "[TiddlyDesktop] Windows IDropTarget::Drop - {} files",
                    file_paths.len()
                );
                let _ = obj.window.emit(
                    "td-drag-drop-position",
                    serde_json::json!({
                        "x": client_x,
                        "y": client_y,
                        "screenCoords": false,
                        "physicalPixels": true
                    }),
                );
                let _ = obj.window.emit(
                    "td-file-drop",
                    serde_json::json!({
                        "paths": file_paths
                    }),
                );
                if !pdw_effect.is_null() {
                    *pdw_effect = DROPEFFECT_COPY.0 as u32;
                }
                return S_OK;
            }

            // Content drop - extract text/html/urls
            if let Some(mut content_data) = obj.extract_data(&data_object) {
                // For cross-wiki drags from our app, propagate the is_text_selection_drag flag
                // This allows JS to filter text/html for text-selection drags (Issue 3)
                if is_our_drag {
                    if let Ok(guard) = OUTGOING_DRAG_STATE.lock() {
                        if let Some(state) = guard.as_ref() {
                            if state.data.is_text_selection_drag {
                                content_data.is_text_selection_drag = Some(true);
                            }
                        }
                    }
                }
                eprintln!(
                    "[TiddlyDesktop] Windows IDropTarget::Drop - content types: {:?}",
                    content_data.types
                );
                let _ = obj.window.emit(
                    "td-drag-drop-position",
                    serde_json::json!({
                        "x": client_x,
                        "y": client_y,
                        "screenCoords": false,
                        "physicalPixels": true
                    }),
                );
                let _ = obj.window.emit("td-drag-content", &content_data);
                if !pdw_effect.is_null() {
                    *pdw_effect = DROPEFFECT_COPY.0 as u32;
                }
            } else {
                eprintln!("[TiddlyDesktop] Windows IDropTarget::Drop - no content extracted");
                if !pdw_effect.is_null() {
                    *pdw_effect = DROPEFFECT_NONE.0 as u32;
                }
            }
        } else {
            if !pdw_effect.is_null() {
                *pdw_effect = DROPEFFECT_NONE.0 as u32;
            }
        }

        S_OK
    }

    /// Log available clipboard formats for debugging
    fn log_available_formats(&self, data_object: &IDataObject) {
        use windows::Win32::System::DataExchange::GetClipboardFormatNameW;

        unsafe {
            if let Ok(enum_fmt) = data_object.EnumFormatEtc(1) {
                let mut formats: [FORMATETC; 1] = [std::mem::zeroed()];
                let mut fetched: u32 = 0;
                let mut format_names = Vec::new();
                while enum_fmt.Next(&mut formats, Some(&mut fetched)).is_ok() && fetched > 0 {
                    let cf = formats[0].cfFormat;
                    let mut name_buf = [0u16; 256];
                    let name_len = GetClipboardFormatNameW(cf as u32, &mut name_buf);
                    let name = if name_len > 0 {
                        OsString::from_wide(&name_buf[..name_len as usize])
                            .to_string_lossy()
                            .to_string()
                    } else {
                        match cf {
                            1 => "CF_TEXT".to_string(),
                            13 => "CF_UNICODETEXT".to_string(),
                            15 => "CF_HDROP".to_string(),
                            _ => format!("CF_{}", cf),
                        }
                    };
                    format_names.push(name);
                    fetched = 0;
                }
                eprintln!(
                    "[TiddlyDesktop] Windows: Available formats: {:?}",
                    format_names
                );
            }
        }
    }

    /// Extract data following TiddlyWiki5's importDataTypes priority
    fn extract_data(&self, data_object: &IDataObject) -> Option<DragContentData> {
        let mut types = Vec::new();
        let mut data = HashMap::new();

        // 0a. Try Mozilla custom clipdata format first (contains custom MIME types)
        let cf_moz_custom = get_cf_moz_custom_clipdata();
        if let Some(raw_data) = self.get_raw_data(data_object, cf_moz_custom) {
            if let Some(moz_data) = parse_moz_custom_clipdata(&raw_data) {
                eprintln!(
                    "[TiddlyDesktop] Windows: Parsed Mozilla custom clipdata, found {} entries",
                    moz_data.len()
                );
                if let Some(tiddler_json) = moz_data.get("text/vnd.tiddler") {
                    eprintln!("[TiddlyDesktop] Windows: Found text/vnd.tiddler in Mozilla clipdata!");
                    types.push("text/vnd.tiddler".to_string());
                    data.insert("text/vnd.tiddler".to_string(), tiddler_json.clone());
                }
                for (mime_type, content) in moz_data {
                    if !data.contains_key(&mime_type) {
                        // Security: Sanitize content based on MIME type
                        let sanitized = if mime_type == "text/html" {
                            sanitize_html(&content)
                        } else if mime_type == "text/uri-list" {
                            sanitize_uri_list(&content)
                        } else {
                            content
                        };
                        types.push(mime_type.clone());
                        data.insert(mime_type, sanitized);
                    }
                }
            }
        }

        // 0b. Try Chrome custom data format (Pickle format)
        if !data.contains_key("text/vnd.tiddler") {
            let cf_chrome_custom = get_cf_chromium_custom_data();
            if let Some(raw_data) = self.get_raw_data(data_object, cf_chrome_custom) {
                if let Some(chrome_data) = parse_chromium_custom_data(&raw_data) {
                    eprintln!(
                        "[TiddlyDesktop] Windows: Parsed Chrome custom clipdata, found {} entries",
                        chrome_data.len()
                    );
                    if let Some(tiddler_json) = chrome_data.get("text/vnd.tiddler") {
                        eprintln!("[TiddlyDesktop] Windows: Found text/vnd.tiddler in Chrome clipdata!");
                        types.push("text/vnd.tiddler".to_string());
                        data.insert("text/vnd.tiddler".to_string(), tiddler_json.clone());
                    }
                    for (mime_type, content) in chrome_data {
                        if !data.contains_key(&mime_type) {
                            // Security: Sanitize content based on MIME type
                            let sanitized = if mime_type == "text/html" {
                                sanitize_html(&content)
                            } else if mime_type == "text/uri-list" {
                                sanitize_uri_list(&content)
                            } else {
                                content
                            };
                            types.push(mime_type.clone());
                            data.insert(mime_type, sanitized);
                        }
                    }
                }
            }
        }

        // 1. text/vnd.tiddler (direct format, if not already found)
        if !data.contains_key("text/vnd.tiddler") {
            let cf_tiddler = get_cf_tiddler();
            if let Some(tiddler) = self.get_string_data(data_object, cf_tiddler) {
                types.push("text/vnd.tiddler".to_string());
                data.insert("text/vnd.tiddler".to_string(), tiddler);
            }
        }

        // 2. URL (UniformResourceLocator)
        let cf_url_w = get_cf_url_w();
        let cf_url = get_cf_url();
        if let Some(url) = self.get_unicode_text_format(data_object, cf_url_w) {
            // Security: Block dangerous URL schemes
            if !is_dangerous_url(&url) {
                types.push("URL".to_string());
                data.insert("URL".to_string(), url);
            }
        } else if let Some(url) = self.get_string_data(data_object, cf_url) {
            // Security: Block dangerous URL schemes
            if !is_dangerous_url(&url) {
                types.push("URL".to_string());
                data.insert("URL".to_string(), url);
            }
        }

        // 3. text/x-moz-url
        let cf_moz_url = get_cf_moz_url();
        if let Some(moz_url) = self.get_unicode_text_format(data_object, cf_moz_url) {
            let url = moz_url.lines().next().unwrap_or(&moz_url);
            // Security: Block dangerous URL schemes
            if !is_dangerous_url(url) {
                types.push("text/x-moz-url".to_string());
                data.insert("text/x-moz-url".to_string(), url.to_string());
            }
        }

        // 4. text/html (HTML Format)
        let cf_html = get_cf_html();
        if let Some(html) = self.get_string_data(data_object, cf_html) {
            // Extract content from Windows HTML Format markers
            let html_content = if let Some(start) = html.find("<!--StartFragment-->") {
                if let Some(end) = html.find("<!--EndFragment-->") {
                    html[start + 20..end].to_string()
                } else {
                    html
                }
            } else {
                html
            };
            // Security: Sanitize HTML content
            let sanitized_html = sanitize_html(&html_content);
            types.push("text/html".to_string());
            data.insert("text/html".to_string(), sanitized_html);
        }

        // 5. text/plain (CF_UNICODETEXT)
        // Skip if text/vnd.tiddler is already present to avoid duplicate data
        // This fixes Issue 1 & 2: external drops from browsers include both tiddler and plain text
        if !data.contains_key("text/vnd.tiddler") {
            if let Some(text) = self.get_unicode_text(data_object) {
                // Check if plain text looks like tiddler JSON (browser may not expose text/vnd.tiddler)
                let looks_like_tiddler_json = text.trim_start().starts_with('[')
                    && text.contains("\"title\"")
                    && (text.contains("\"text\"") || text.contains("\"fields\""));

                if looks_like_tiddler_json {
                    eprintln!("[TiddlyDesktop] Windows: Detected tiddler JSON in plain text!");
                    types.push("text/vnd.tiddler".to_string());
                    data.insert("text/vnd.tiddler".to_string(), text);
                    // Don't also add as text/plain - that would cause duplicate imports
                } else {
                    types.push("text/plain".to_string());
                    data.insert("text/plain".to_string(), text);
                }
            }
        }

        // 6. Text (CF_TEXT fallback)
        // Skip if text/vnd.tiddler is already present to avoid duplicate data
        if !data.contains_key("text/vnd.tiddler") && !data.contains_key("text/plain") {
            if let Some(text) = self.get_ansi_text(data_object) {
                // Check if ANSI text looks like tiddler JSON
                let looks_like_tiddler_json = text.trim_start().starts_with('[')
                    && text.contains("\"title\"")
                    && (text.contains("\"text\"") || text.contains("\"fields\""));

                if looks_like_tiddler_json {
                    eprintln!("[TiddlyDesktop] Windows: Detected tiddler JSON in ANSI text!");
                    types.push("text/vnd.tiddler".to_string());
                    data.insert("text/vnd.tiddler".to_string(), text);
                    // Don't also add as Text - that would cause duplicate imports
                } else {
                    types.push("Text".to_string());
                    data.insert("Text".to_string(), text);
                }
            }
        }

        // 7. text/uri-list
        let cf_uri = get_cf_uri_list();
        if let Some(uri_list) = self.get_string_data(data_object, cf_uri) {
            // Security: Sanitize URI list
            let sanitized_uri_list = sanitize_uri_list(&uri_list);
            types.push("text/uri-list".to_string());
            data.insert("text/uri-list".to_string(), sanitized_uri_list);
        }

        if types.is_empty() {
            None
        } else {
            Some(DragContentData { types, data, is_text_selection_drag: None, is_same_window: None })
        }
    }

    fn get_unicode_text(&self, data_object: &IDataObject) -> Option<String> {
        let format = FORMATETC {
            cfFormat: CF_UNICODETEXT,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0 as u32,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };

        unsafe {
            if let Ok(medium) = data_object.GetData(&format) {
                if !medium.u.hGlobal.0.is_null() {
                    let ptr = GlobalLock(medium.u.hGlobal) as *const u16;
                    if !ptr.is_null() {
                        let size = GlobalSize(medium.u.hGlobal) / 2;
                        let slice = std::slice::from_raw_parts(ptr, size);
                        let len = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
                        let text = OsString::from_wide(&slice[..len])
                            .to_string_lossy()
                            .to_string();
                        let _ = GlobalUnlock(medium.u.hGlobal);
                        return Some(text);
                    }
                }
            }
        }
        None
    }

    fn get_ansi_text(&self, data_object: &IDataObject) -> Option<String> {
        let format = FORMATETC {
            cfFormat: CF_TEXT,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0 as u32,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };

        unsafe {
            if let Ok(medium) = data_object.GetData(&format) {
                if !medium.u.hGlobal.0.is_null() {
                    let ptr = GlobalLock(medium.u.hGlobal) as *const u8;
                    if !ptr.is_null() {
                        let size = GlobalSize(medium.u.hGlobal);
                        let slice = std::slice::from_raw_parts(ptr, size);
                        let len = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
                        let ansi_bytes = &slice[..len];

                        let wide_len = MultiByteToWideChar(
                            CP_ACP,
                            MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0),
                            ansi_bytes,
                            None,
                        );

                        if wide_len > 0 {
                            let mut wide_buf: Vec<u16> = vec![0; wide_len as usize];
                            let result = MultiByteToWideChar(
                                CP_ACP,
                                MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0),
                                ansi_bytes,
                                Some(&mut wide_buf),
                            );
                            let _ = GlobalUnlock(medium.u.hGlobal);
                            if result > 0 {
                                return String::from_utf16(&wide_buf[..result as usize]).ok();
                            }
                        } else {
                            let _ = GlobalUnlock(medium.u.hGlobal);
                        }
                    }
                }
            }
        }
        None
    }

    fn get_unicode_text_format(&self, data_object: &IDataObject, cf: u16) -> Option<String> {
        let format = FORMATETC {
            cfFormat: cf,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0 as u32,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };

        unsafe {
            if let Ok(medium) = data_object.GetData(&format) {
                if !medium.u.hGlobal.0.is_null() {
                    let ptr = GlobalLock(medium.u.hGlobal) as *const u16;
                    if !ptr.is_null() {
                        let size = GlobalSize(medium.u.hGlobal) / 2;
                        let slice = std::slice::from_raw_parts(ptr, size);
                        let len = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
                        let text = OsString::from_wide(&slice[..len])
                            .to_string_lossy()
                            .to_string();
                        let _ = GlobalUnlock(medium.u.hGlobal);
                        return Some(text);
                    }
                }
            }
        }
        None
    }

    fn get_string_data(&self, data_object: &IDataObject, cf: u16) -> Option<String> {
        let format = FORMATETC {
            cfFormat: cf,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0 as u32,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };

        unsafe {
            if let Ok(medium) = data_object.GetData(&format) {
                if !medium.u.hGlobal.0.is_null() {
                    let ptr = GlobalLock(medium.u.hGlobal) as *const u8;
                    if !ptr.is_null() {
                        let size = GlobalSize(medium.u.hGlobal);
                        let slice = std::slice::from_raw_parts(ptr, size);

                        // Try to decode with proper encoding detection
                        let text = decode_string(slice);
                        let _ = GlobalUnlock(medium.u.hGlobal);
                        return Some(text);
                    }
                }
            }
        }
        None
    }

    fn get_raw_data(&self, data_object: &IDataObject, cf: u16) -> Option<Vec<u8>> {
        let format = FORMATETC {
            cfFormat: cf,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0 as u32,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };

        unsafe {
            if let Ok(medium) = data_object.GetData(&format) {
                if !medium.u.hGlobal.0.is_null() {
                    let ptr = GlobalLock(medium.u.hGlobal) as *const u8;
                    if !ptr.is_null() {
                        let size = GlobalSize(medium.u.hGlobal);
                        let slice = std::slice::from_raw_parts(ptr, size);
                        let data = slice.to_vec();
                        let _ = GlobalUnlock(medium.u.hGlobal);
                        return Some(data);
                    }
                }
            }
        }
        None
    }

    fn get_file_paths(&self, data_object: &IDataObject) -> Vec<String> {
        let mut paths = Vec::new();
        let format = FORMATETC {
            cfFormat: CF_HDROP,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0 as u32,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };

        unsafe {
            if let Ok(medium) = data_object.GetData(&format) {
                let hdrop = HDROP(medium.u.hGlobal.0 as *mut _);
                let count = DragQueryFileW(hdrop, 0xFFFFFFFF, None);
                for i in 0..count {
                    let len = DragQueryFileW(hdrop, i, None);
                    let mut buffer = vec![0u16; (len + 1) as usize];
                    DragQueryFileW(hdrop, i, Some(&mut buffer));
                    let path = OsString::from_wide(&buffer[..len as usize])
                        .to_string_lossy()
                        .to_string();
                    paths.push(path);
                }
            }
        }
        paths
    }
}

/// Choose a drop effect
fn choose_drop_effect(allowed: DROPEFFECT) -> DROPEFFECT {
    if (allowed.0 & DROPEFFECT_COPY.0) != 0 {
        DROPEFFECT_COPY
    } else if (allowed.0 & DROPEFFECT_MOVE.0) != 0 {
        DROPEFFECT_MOVE
    } else if (allowed.0 & DROPEFFECT_LINK.0) != 0 {
        DROPEFFECT_LINK
    } else {
        DROPEFFECT_COPY
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

/// Parse Mozilla's application/x-moz-custom-clipdata format
fn parse_moz_custom_clipdata(data: &[u8]) -> Option<HashMap<String, String>> {
    if data.len() < 8 {
        return None;
    }

    let mut result = HashMap::new();
    let mut offset = 0;

    let num_entries = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    offset += 4;

    for _ in 0..num_entries {
        if offset + 4 > data.len() {
            break;
        }

        let mime_len = u32::from_be_bytes([
            data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
        ]) as usize;
        offset += 4;

        if offset + mime_len > data.len() {
            break;
        }

        let mime_type = decode_utf16le(&data[offset..offset + mime_len]);
        offset += mime_len;

        if offset + 4 > data.len() {
            break;
        }

        let content_len = u32::from_be_bytes([
            data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
        ]) as usize;
        offset += 4;

        if offset + content_len > data.len() {
            let available = data.len() - offset;
            let content = decode_utf16le(&data[offset..offset + available]);
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
        if offset + 4 > data.len() {
            break;
        }

        let mime_char_len = u32::from_le_bytes([
            data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
        ]) as usize;
        offset += 4;

        let mime_byte_len = mime_char_len * 2;
        if offset + mime_byte_len > data.len() {
            break;
        }

        let mime_type = decode_utf16le(&data[offset..offset + mime_byte_len]);
        offset += mime_byte_len;
        offset += (4 - (mime_byte_len % 4)) % 4; // Align to 4-byte boundary

        if offset + 4 > data.len() {
            break;
        }

        let content_char_len = u32::from_le_bytes([
            data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
        ]) as usize;
        offset += 4;

        let content_byte_len = content_char_len * 2;
        if offset + content_byte_len > data.len() {
            let available = data.len() - offset;
            let content = decode_utf16le(&data[offset..offset + available]);
            if !mime_type.is_empty() && !content.is_empty() {
                result.insert(mime_type, content);
            }
            break;
        }

        let content = decode_utf16le(&data[offset..offset + content_byte_len]);
        offset += content_byte_len;
        offset += (4 - (content_byte_len % 4)) % 4; // Align to 4-byte boundary

        if !mime_type.is_empty() {
            result.insert(mime_type, content);
        }
    }

    if result.is_empty() { None } else { Some(result) }
}

/// Find the WebView2 content window (deepest Chrome_WidgetWin_*)
fn find_webview2_content_hwnd(parent: HWND) -> Option<HWND> {
    fn find_deepest_chrome_window(hwnd: HWND, depth: usize) -> Option<(HWND, usize)> {
        struct EnumData {
            chrome_windows: Vec<HWND>,
        }

        unsafe extern "system" fn enum_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let data = &mut *(lparam.0 as *mut EnumData);
            let mut class_name = [0u16; 256];
            let len = GetClassNameW(hwnd, &mut class_name);
            if len > 0 {
                let class_str = OsString::from_wide(&class_name[..len as usize])
                    .to_string_lossy()
                    .to_string();
                if class_str.starts_with("Chrome_WidgetWin") {
                    data.chrome_windows.push(hwnd);
                }
            }
            BOOL(1)
        }

        let mut data = EnumData {
            chrome_windows: Vec::new(),
        };
        unsafe {
            let _ = EnumChildWindows(
                Some(hwnd),
                Some(enum_callback),
                LPARAM(&mut data as *mut _ as isize),
            );
        }

        let mut deepest: Option<(HWND, usize)> = None;
        for chrome_hwnd in &data.chrome_windows {
            if let Some((child_hwnd, child_depth)) =
                find_deepest_chrome_window(*chrome_hwnd, depth + 1)
            {
                if deepest.is_none() || child_depth > deepest.unwrap().1 {
                    deepest = Some((child_hwnd, child_depth));
                }
            } else {
                if deepest.is_none() || depth > deepest.unwrap().1 {
                    deepest = Some((*chrome_hwnd, depth));
                }
            }
        }
        deepest
    }

    find_deepest_chrome_window(parent, 0).map(|(hwnd, _)| hwnd)
}

// ============================================================================
// Outgoing drag support (TiddlyWiki → external apps)
// ============================================================================

/// Data to be provided during an outgoing drag operation
/// Matches MIME types used by TiddlyWiki5's drag-drop system
#[derive(Clone, Debug, Default)]
#[allow(dead_code)]
pub struct OutgoingDragData {
    pub text_plain: Option<String>,
    pub text_html: Option<String>,
    pub text_vnd_tiddler: Option<String>,
    pub text_uri_list: Option<String>,
    pub text_x_moz_url: Option<String>,
    pub url: Option<String>,
    /// True if this is a text-selection drag (not used on Windows currently)
    pub is_text_selection_drag: bool,
}

/// Global state for outgoing drag operation
#[allow(dead_code)]
struct OutgoingDragState {
    data: OutgoingDragData,
    source_window_label: String,
    data_was_requested: bool,
}

lazy_static::lazy_static! {
    static ref OUTGOING_DRAG_STATE: Mutex<Option<OutgoingDragState>> = Mutex::new(None);
}

/// IDataObject vtable
#[repr(C)]
#[allow(non_snake_case)]
struct IDataObjectVtbl {
    // IUnknown
    QueryInterface: unsafe extern "system" fn(*mut DataObjectImpl, *const GUID, *mut *mut std::ffi::c_void) -> HRESULT,
    AddRef: unsafe extern "system" fn(*mut DataObjectImpl) -> u32,
    Release: unsafe extern "system" fn(*mut DataObjectImpl) -> u32,
    // IDataObject
    GetData: unsafe extern "system" fn(*mut DataObjectImpl, *const FORMATETC, *mut STGMEDIUM) -> HRESULT,
    GetDataHere: unsafe extern "system" fn(*mut DataObjectImpl, *const FORMATETC, *mut STGMEDIUM) -> HRESULT,
    QueryGetData: unsafe extern "system" fn(*mut DataObjectImpl, *const FORMATETC) -> HRESULT,
    GetCanonicalFormatEtc: unsafe extern "system" fn(*mut DataObjectImpl, *const FORMATETC, *mut FORMATETC) -> HRESULT,
    SetData: unsafe extern "system" fn(*mut DataObjectImpl, *const FORMATETC, *const STGMEDIUM, i32) -> HRESULT,
    EnumFormatEtc: unsafe extern "system" fn(*mut DataObjectImpl, u32, *mut *mut std::ffi::c_void) -> HRESULT,
    DAdvise: unsafe extern "system" fn(*mut DataObjectImpl, *const FORMATETC, u32, *mut std::ffi::c_void, *mut u32) -> HRESULT,
    DUnadvise: unsafe extern "system" fn(*mut DataObjectImpl, u32) -> HRESULT,
    EnumDAdvise: unsafe extern "system" fn(*mut DataObjectImpl, *mut *mut std::ffi::c_void) -> HRESULT,
}

/// STGMEDIUM for data transfer
#[repr(C)]
#[allow(non_snake_case)]
struct STGMEDIUM {
    tymed: u32,
    u: STGMEDIUM_u,
    pUnkForRelease: *mut std::ffi::c_void,
}

#[repr(C)]
#[derive(Copy, Clone)]
#[allow(non_snake_case)]
union STGMEDIUM_u {
    hBitmap: *mut std::ffi::c_void,
    hMetaFilePict: *mut std::ffi::c_void,
    hEnhMetaFile: *mut std::ffi::c_void,
    hGlobal: *mut std::ffi::c_void,
    lpszFileName: *mut u16,
    pstm: *mut std::ffi::c_void,
    pstg: *mut std::ffi::c_void,
}

impl Default for STGMEDIUM_u {
    fn default() -> Self {
        Self { hGlobal: std::ptr::null_mut() }
    }
}

const IID_IDATAOBJECT: GUID = GUID::from_u128(0x0000010e_0000_0000_c000_000000000046);

/// Our IDataObject implementation
#[repr(C)]
struct DataObjectImpl {
    vtbl: *const IDataObjectVtbl,
    ref_count: AtomicU32,
    data: OutgoingDragData,
    cf_tiddler: u16,
    cf_html: u16,
    cf_url_w: u16,
    cf_moz_url: u16,
}

static DATAOBJECT_VTBL: IDataObjectVtbl = IDataObjectVtbl {
    QueryInterface: DataObjectImpl::query_interface,
    AddRef: DataObjectImpl::add_ref,
    Release: DataObjectImpl::release,
    GetData: DataObjectImpl::get_data,
    GetDataHere: DataObjectImpl::get_data_here,
    QueryGetData: DataObjectImpl::query_get_data,
    GetCanonicalFormatEtc: DataObjectImpl::get_canonical_format_etc,
    SetData: DataObjectImpl::set_data,
    EnumFormatEtc: DataObjectImpl::enum_format_etc,
    DAdvise: DataObjectImpl::d_advise,
    DUnadvise: DataObjectImpl::d_unadvise,
    EnumDAdvise: DataObjectImpl::enum_d_advise,
};

impl DataObjectImpl {
    fn new(data: OutgoingDragData) -> *mut Self {
        let obj = Box::new(Self {
            vtbl: &DATAOBJECT_VTBL,
            ref_count: AtomicU32::new(1),
            data,
            cf_tiddler: get_cf_tiddler(),
            cf_html: get_cf_html(),
            cf_url_w: get_cf_url_w(),
            cf_moz_url: get_cf_moz_url(),
        });
        Box::into_raw(obj)
    }

    unsafe extern "system" fn query_interface(
        this: *mut Self,
        riid: *const GUID,
        ppv: *mut *mut std::ffi::c_void,
    ) -> HRESULT {
        if ppv.is_null() {
            return E_POINTER;
        }
        let iid = &*riid;
        if *iid == IID_IUNKNOWN || *iid == IID_IDATAOBJECT {
            Self::add_ref(this);
            *ppv = this as *mut std::ffi::c_void;
            S_OK
        } else {
            *ppv = std::ptr::null_mut();
            E_NOINTERFACE
        }
    }

    unsafe extern "system" fn add_ref(this: *mut Self) -> u32 {
        (*this).ref_count.fetch_add(1, Ordering::SeqCst) + 1
    }

    unsafe extern "system" fn release(this: *mut Self) -> u32 {
        let count = (*this).ref_count.fetch_sub(1, Ordering::SeqCst) - 1;
        if count == 0 {
            drop(Box::from_raw(this));
        }
        count
    }

    unsafe extern "system" fn get_data(
        this: *mut Self,
        pformatetc: *const FORMATETC,
        pmedium: *mut STGMEDIUM,
    ) -> HRESULT {
        use windows::Win32::System::Memory::{GlobalAlloc, GLOBAL_ALLOC_FLAGS};

        if pformatetc.is_null() || pmedium.is_null() {
            return E_POINTER;
        }

        let obj = &*this;
        let format = &*pformatetc;

        // Mark data as requested
        if let Ok(mut guard) = OUTGOING_DRAG_STATE.lock() {
            if let Some(state) = guard.as_mut() {
                state.data_was_requested = true;
            }
        }

        let cf = format.cfFormat;
        eprintln!("[TiddlyDesktop] Windows IDataObject::GetData called for format {}", cf);

        // Determine which data to provide based on clipboard format
        let data_bytes: Option<Vec<u8>> = if cf == CF_UNICODETEXT {
            // Plain text as UTF-16LE with null terminator
            obj.data.text_plain.as_ref().map(|s| {
                let mut bytes: Vec<u8> = s.encode_utf16()
                    .flat_map(|c| c.to_le_bytes())
                    .collect();
                bytes.extend_from_slice(&[0, 0]); // null terminator
                bytes
            })
        } else if cf == obj.cf_tiddler {
            // TiddlyWiki tiddler JSON as UTF-8
            obj.data.text_vnd_tiddler.as_ref().map(|s| {
                let mut bytes = s.as_bytes().to_vec();
                bytes.push(0); // null terminator
                bytes
            })
        } else if cf == obj.cf_html {
            // HTML Format (Windows-specific with headers)
            obj.data.text_html.as_ref().map(|html| {
                create_html_format(html)
            })
        } else if cf == obj.cf_url_w {
            // URL as UTF-16LE
            obj.data.url.as_ref().map(|s| {
                let mut bytes: Vec<u8> = s.encode_utf16()
                    .flat_map(|c| c.to_le_bytes())
                    .collect();
                bytes.extend_from_slice(&[0, 0]);
                bytes
            })
        } else if cf == obj.cf_moz_url {
            // Mozilla URL format: URL\nTitle as UTF-16LE
            obj.data.text_x_moz_url.as_ref().map(|url| {
                let title = obj.data.text_plain.as_deref().unwrap_or("");
                let full = format!("{}\n{}", url, title);
                let mut bytes: Vec<u8> = full.encode_utf16()
                    .flat_map(|c| c.to_le_bytes())
                    .collect();
                bytes.extend_from_slice(&[0, 0]);
                bytes
            })
        } else {
            None
        };

        if let Some(bytes) = data_bytes {
            // Allocate global memory and copy data
            let hglobal = GlobalAlloc(GLOBAL_ALLOC_FLAGS(0x0042), bytes.len()); // GMEM_MOVEABLE | GMEM_ZEROINIT
            if hglobal.is_err() {
                return HRESULT::from_win32(0x8007000E); // E_OUTOFMEMORY
            }
            let hglobal = hglobal.unwrap();

            let ptr = GlobalLock(hglobal);
            if ptr.is_null() {
                return HRESULT::from_win32(0x8007000E);
            }

            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
            let _ = GlobalUnlock(hglobal);

            (*pmedium).tymed = TYMED_HGLOBAL.0 as u32;
            (*pmedium).u.hGlobal = hglobal.0 as *mut std::ffi::c_void;
            (*pmedium).pUnkForRelease = std::ptr::null_mut();

            eprintln!("[TiddlyDesktop] Windows IDataObject::GetData provided {} bytes for format {}", bytes.len(), cf);
            S_OK
        } else {
            HRESULT::from_win32(0x80040064) // DV_E_FORMATETC
        }
    }

    unsafe extern "system" fn get_data_here(
        _this: *mut Self,
        _pformatetc: *const FORMATETC,
        _pmedium: *mut STGMEDIUM,
    ) -> HRESULT {
        HRESULT::from_win32(0x80040069) // DV_E_FORMATETC - not implemented
    }

    unsafe extern "system" fn query_get_data(
        this: *mut Self,
        pformatetc: *const FORMATETC,
    ) -> HRESULT {
        if pformatetc.is_null() {
            return E_POINTER;
        }

        let obj = &*this;
        let format = &*pformatetc;
        let cf = format.cfFormat;

        // Check if we support this format
        let supported = (cf == CF_UNICODETEXT && obj.data.text_plain.is_some())
            || (cf == obj.cf_tiddler && obj.data.text_vnd_tiddler.is_some())
            || (cf == obj.cf_html && obj.data.text_html.is_some())
            || (cf == obj.cf_url_w && obj.data.url.is_some())
            || (cf == obj.cf_moz_url && obj.data.text_x_moz_url.is_some());

        if supported {
            S_OK
        } else {
            HRESULT::from_win32(0x80040064) // DV_E_FORMATETC
        }
    }

    unsafe extern "system" fn get_canonical_format_etc(
        _this: *mut Self,
        _pformatetcin: *const FORMATETC,
        pformatetcout: *mut FORMATETC,
    ) -> HRESULT {
        if !pformatetcout.is_null() {
            (*pformatetcout).ptd = std::ptr::null_mut();
        }
        HRESULT::from_win32(0x00040003) // DATA_S_SAMEFORMATETC
    }

    unsafe extern "system" fn set_data(
        _this: *mut Self,
        _pformatetc: *const FORMATETC,
        _pmedium: *const STGMEDIUM,
        _frelease: i32,
    ) -> HRESULT {
        HRESULT::from_win32(0x80004001) // E_NOTIMPL
    }

    unsafe extern "system" fn enum_format_etc(
        this: *mut Self,
        dwdirection: u32,
        ppenumformatetc: *mut *mut std::ffi::c_void,
    ) -> HRESULT {
        if ppenumformatetc.is_null() {
            return E_POINTER;
        }

        if dwdirection != 1 {
            // Only DATADIR_GET supported
            return HRESULT::from_win32(0x80004001); // E_NOTIMPL
        }

        let obj = &*this;

        // Build list of supported formats
        let mut formats = Vec::new();

        if obj.data.text_vnd_tiddler.is_some() {
            formats.push(FORMATETC {
                cfFormat: obj.cf_tiddler,
                ptd: std::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT.0 as u32,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
            });
        }
        if obj.data.url.is_some() {
            formats.push(FORMATETC {
                cfFormat: obj.cf_url_w,
                ptd: std::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT.0 as u32,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
            });
        }
        if obj.data.text_x_moz_url.is_some() {
            formats.push(FORMATETC {
                cfFormat: obj.cf_moz_url,
                ptd: std::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT.0 as u32,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
            });
        }
        if obj.data.text_html.is_some() {
            formats.push(FORMATETC {
                cfFormat: obj.cf_html,
                ptd: std::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT.0 as u32,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
            });
        }
        if obj.data.text_plain.is_some() {
            formats.push(FORMATETC {
                cfFormat: CF_UNICODETEXT,
                ptd: std::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT.0 as u32,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
            });
        }

        // Create enumerator
        let enum_ptr = FormatEnumerator::new(formats);
        *ppenumformatetc = enum_ptr as *mut std::ffi::c_void;

        S_OK
    }

    unsafe extern "system" fn d_advise(
        _this: *mut Self,
        _pformatetc: *const FORMATETC,
        _advf: u32,
        _padvsink: *mut std::ffi::c_void,
        _pdwconnection: *mut u32,
    ) -> HRESULT {
        HRESULT::from_win32(0x80040003) // OLE_E_ADVISENOTSUPPORTED
    }

    unsafe extern "system" fn d_unadvise(_this: *mut Self, _dwconnection: u32) -> HRESULT {
        HRESULT::from_win32(0x80040003) // OLE_E_ADVISENOTSUPPORTED
    }

    unsafe extern "system" fn enum_d_advise(
        _this: *mut Self,
        _ppenumadvise: *mut *mut std::ffi::c_void,
    ) -> HRESULT {
        HRESULT::from_win32(0x80040003) // OLE_E_ADVISENOTSUPPORTED
    }
}

/// Create Windows "HTML Format" clipboard data with required headers
fn create_html_format(html: &str) -> Vec<u8> {
    let header = "Version:0.9\r\nStartHTML:00000000\r\nEndHTML:00000000\r\nStartFragment:00000000\r\nEndFragment:00000000\r\n";
    let prefix = "<!DOCTYPE html>\r\n<html>\r\n<body>\r\n<!--StartFragment-->";
    let suffix = "<!--EndFragment-->\r\n</body>\r\n</html>";

    let start_html = header.len();
    let start_fragment = start_html + prefix.len();
    let end_fragment = start_fragment + html.len();
    let end_html = end_fragment + suffix.len();

    let formatted = format!(
        "Version:0.9\r\nStartHTML:{:08}\r\nEndHTML:{:08}\r\nStartFragment:{:08}\r\nEndFragment:{:08}\r\n{}{}{}",
        start_html, end_html, start_fragment, end_fragment, prefix, html, suffix
    );

    let mut bytes = formatted.into_bytes();
    bytes.push(0);
    bytes
}

/// IEnumFORMATETC implementation
#[repr(C)]
#[allow(non_snake_case)]
struct IEnumFORMATETCVtbl {
    QueryInterface: unsafe extern "system" fn(*mut FormatEnumerator, *const GUID, *mut *mut std::ffi::c_void) -> HRESULT,
    AddRef: unsafe extern "system" fn(*mut FormatEnumerator) -> u32,
    Release: unsafe extern "system" fn(*mut FormatEnumerator) -> u32,
    Next: unsafe extern "system" fn(*mut FormatEnumerator, u32, *mut FORMATETC, *mut u32) -> HRESULT,
    Skip: unsafe extern "system" fn(*mut FormatEnumerator, u32) -> HRESULT,
    Reset: unsafe extern "system" fn(*mut FormatEnumerator) -> HRESULT,
    Clone: unsafe extern "system" fn(*mut FormatEnumerator, *mut *mut std::ffi::c_void) -> HRESULT,
}

const IID_IENUMFORMATETC: GUID = GUID::from_u128(0x00000103_0000_0000_c000_000000000046);

#[repr(C)]
struct FormatEnumerator {
    vtbl: *const IEnumFORMATETCVtbl,
    ref_count: AtomicU32,
    formats: Vec<FORMATETC>,
    index: AtomicU32,
}

static ENUM_FORMATETC_VTBL: IEnumFORMATETCVtbl = IEnumFORMATETCVtbl {
    QueryInterface: FormatEnumerator::query_interface,
    AddRef: FormatEnumerator::add_ref,
    Release: FormatEnumerator::release,
    Next: FormatEnumerator::next,
    Skip: FormatEnumerator::skip,
    Reset: FormatEnumerator::reset,
    Clone: FormatEnumerator::clone_enum,
};

impl FormatEnumerator {
    fn new(formats: Vec<FORMATETC>) -> *mut Self {
        let obj = Box::new(Self {
            vtbl: &ENUM_FORMATETC_VTBL,
            ref_count: AtomicU32::new(1),
            formats,
            index: AtomicU32::new(0),
        });
        Box::into_raw(obj)
    }

    unsafe extern "system" fn query_interface(
        this: *mut Self,
        riid: *const GUID,
        ppv: *mut *mut std::ffi::c_void,
    ) -> HRESULT {
        if ppv.is_null() {
            return E_POINTER;
        }
        let iid = &*riid;
        if *iid == IID_IUNKNOWN || *iid == IID_IENUMFORMATETC {
            Self::add_ref(this);
            *ppv = this as *mut std::ffi::c_void;
            S_OK
        } else {
            *ppv = std::ptr::null_mut();
            E_NOINTERFACE
        }
    }

    unsafe extern "system" fn add_ref(this: *mut Self) -> u32 {
        (*this).ref_count.fetch_add(1, Ordering::SeqCst) + 1
    }

    unsafe extern "system" fn release(this: *mut Self) -> u32 {
        let count = (*this).ref_count.fetch_sub(1, Ordering::SeqCst) - 1;
        if count == 0 {
            drop(Box::from_raw(this));
        }
        count
    }

    unsafe extern "system" fn next(
        this: *mut Self,
        celt: u32,
        rgelt: *mut FORMATETC,
        pcelt_fetched: *mut u32,
    ) -> HRESULT {
        let obj = &*this;
        let mut fetched = 0u32;
        let mut index = obj.index.load(Ordering::SeqCst);

        for i in 0..celt {
            if index >= obj.formats.len() as u32 {
                break;
            }
            if !rgelt.is_null() {
                *rgelt.add(i as usize) = obj.formats[index as usize];
            }
            index += 1;
            fetched += 1;
        }

        obj.index.store(index, Ordering::SeqCst);

        if !pcelt_fetched.is_null() {
            *pcelt_fetched = fetched;
        }

        if fetched == celt {
            S_OK
        } else {
            HRESULT(1) // S_FALSE
        }
    }

    unsafe extern "system" fn skip(this: *mut Self, celt: u32) -> HRESULT {
        let obj = &*this;
        let index = obj.index.fetch_add(celt, Ordering::SeqCst);
        if index + celt <= obj.formats.len() as u32 {
            S_OK
        } else {
            HRESULT(1) // S_FALSE
        }
    }

    unsafe extern "system" fn reset(this: *mut Self) -> HRESULT {
        (*this).index.store(0, Ordering::SeqCst);
        S_OK
    }

    unsafe extern "system" fn clone_enum(
        this: *mut Self,
        ppenum: *mut *mut std::ffi::c_void,
    ) -> HRESULT {
        if ppenum.is_null() {
            return E_POINTER;
        }
        let obj = &*this;
        let clone = FormatEnumerator::new(obj.formats.clone());
        (*clone).index.store(obj.index.load(Ordering::SeqCst), Ordering::SeqCst);
        *ppenum = clone as *mut std::ffi::c_void;
        S_OK
    }
}

/// IDropSource implementation
#[repr(C)]
#[allow(non_snake_case)]
struct IDropSourceVtbl {
    QueryInterface: unsafe extern "system" fn(*mut DropSourceImpl, *const GUID, *mut *mut std::ffi::c_void) -> HRESULT,
    AddRef: unsafe extern "system" fn(*mut DropSourceImpl) -> u32,
    Release: unsafe extern "system" fn(*mut DropSourceImpl) -> u32,
    QueryContinueDrag: unsafe extern "system" fn(*mut DropSourceImpl, i32, u32) -> HRESULT,
    GiveFeedback: unsafe extern "system" fn(*mut DropSourceImpl, u32) -> HRESULT,
}

const IID_IDROPSOURCE: GUID = GUID::from_u128(0x00000121_0000_0000_c000_000000000046);

const DRAGDROP_S_DROP: HRESULT = HRESULT(0x00040100u32 as i32);
const DRAGDROP_S_CANCEL: HRESULT = HRESULT(0x00040101u32 as i32);
const DRAGDROP_S_USEDEFAULTCURSORS: HRESULT = HRESULT(0x00040102u32 as i32);

/// MK_LBUTTON constant
const MK_LBUTTON: u32 = 0x0001;

#[repr(C)]
struct DropSourceImpl {
    vtbl: *const IDropSourceVtbl,
    ref_count: AtomicU32,
    #[allow(dead_code)]
    window: WebviewWindow,
    #[allow(dead_code)]
    window_hwnd: HWND,
}

static DROPSOURCE_VTBL: IDropSourceVtbl = IDropSourceVtbl {
    QueryInterface: DropSourceImpl::query_interface,
    AddRef: DropSourceImpl::add_ref,
    Release: DropSourceImpl::release,
    QueryContinueDrag: DropSourceImpl::query_continue_drag,
    GiveFeedback: DropSourceImpl::give_feedback,
};

impl DropSourceImpl {
    fn new(window: WebviewWindow, window_hwnd: HWND) -> *mut Self {
        let obj = Box::new(Self {
            vtbl: &DROPSOURCE_VTBL,
            ref_count: AtomicU32::new(1),
            window,
            window_hwnd,
        });
        Box::into_raw(obj)
    }

    unsafe extern "system" fn query_interface(
        this: *mut Self,
        riid: *const GUID,
        ppv: *mut *mut std::ffi::c_void,
    ) -> HRESULT {
        if ppv.is_null() {
            return E_POINTER;
        }
        let iid = &*riid;
        if *iid == IID_IUNKNOWN || *iid == IID_IDROPSOURCE {
            Self::add_ref(this);
            *ppv = this as *mut std::ffi::c_void;
            S_OK
        } else {
            *ppv = std::ptr::null_mut();
            E_NOINTERFACE
        }
    }

    unsafe extern "system" fn add_ref(this: *mut Self) -> u32 {
        (*this).ref_count.fetch_add(1, Ordering::SeqCst) + 1
    }

    unsafe extern "system" fn release(this: *mut Self) -> u32 {
        let count = (*this).ref_count.fetch_sub(1, Ordering::SeqCst) - 1;
        if count == 0 {
            drop(Box::from_raw(this));
        }
        count
    }

    unsafe extern "system" fn query_continue_drag(
        _this: *mut Self,
        f_escape_pressed: i32,
        grf_key_state: u32,
    ) -> HRESULT {
        // Cancel on Escape
        if f_escape_pressed != 0 {
            eprintln!("[TiddlyDesktop] Windows IDropSource: Escape pressed, canceling drag");
            return DRAGDROP_S_CANCEL;
        }

        // Drop when button released
        if (grf_key_state & MK_LBUTTON) == 0 {
            eprintln!("[TiddlyDesktop] Windows IDropSource: Button released, dropping");
            return DRAGDROP_S_DROP;
        }

        S_OK
    }

    unsafe extern "system" fn give_feedback(
        _this: *mut Self,
        _dw_effect: u32,
    ) -> HRESULT {
        // No polling needed - IDropTarget on each window handles enter/leave/motion natively
        // This matches the Linux approach where GTK signals handle all drag events
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// Create an HBITMAP from PNG data with premultiplied alpha (required for drag images)
/// Applies 0.7 opacity to match the JS drag image styling
fn create_hbitmap_from_png(png_data: &[u8]) -> Option<(windows::Win32::Graphics::Gdi::HBITMAP, i32, i32)> {
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, SelectObject,
        BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    };

    // Decode PNG using the image crate
    let img = match image::load_from_memory(png_data) {
        Ok(img) => img.to_rgba8(),
        Err(e) => {
            eprintln!("[TiddlyDesktop] Windows: Failed to decode PNG: {}", e);
            return None;
        }
    };

    let width = img.width() as i32;
    let height = img.height() as i32;

    unsafe {
        // Create a DIB section for the bitmap
        let mut bmi: BITMAPINFO = std::mem::zeroed();
        bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = width;
        bmi.bmiHeader.biHeight = -height; // Negative for top-down DIB
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        bmi.bmiHeader.biCompression = BI_RGB.0;

        let hdc = CreateCompatibleDC(None);
        if hdc.is_invalid() {
            eprintln!("[TiddlyDesktop] Windows: CreateCompatibleDC failed");
            return None;
        }

        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let hbitmap = CreateDIBSection(
            Some(hdc),
            &bmi,
            DIB_RGB_COLORS,
            &mut bits,
            None,
            0,
        );

        if hbitmap.is_err() || bits.is_null() {
            eprintln!("[TiddlyDesktop] Windows: CreateDIBSection failed");
            let _ = DeleteDC(hdc);
            return None;
        }
        let hbitmap = hbitmap.unwrap();

        // Copy pixels with premultiplied alpha and 0.7 opacity
        let pixel_count = (width * height) as usize;
        let dst = std::slice::from_raw_parts_mut(bits as *mut u8, pixel_count * 4);

        for y in 0..height as usize {
            for x in 0..width as usize {
                let src_idx = (y * width as usize + x) * 4;
                let dst_idx = src_idx;

                let r = img.as_raw()[src_idx];
                let g = img.as_raw()[src_idx + 1];
                let b = img.as_raw()[src_idx + 2];
                let a = img.as_raw()[src_idx + 3];

                // Apply 0.7 opacity
                let a = ((a as f32) * 0.7) as u8;

                // Premultiply alpha (required for Windows drag images)
                let af = a as f32 / 255.0;
                let pr = ((r as f32) * af) as u8;
                let pg = ((g as f32) * af) as u8;
                let pb = ((b as f32) * af) as u8;

                // Windows uses BGRA format
                dst[dst_idx] = pb;
                dst[dst_idx + 1] = pg;
                dst[dst_idx + 2] = pr;
                dst[dst_idx + 3] = a;
            }
        }

        let _ = SelectObject(hdc, hbitmap.into());
        let _ = DeleteDC(hdc);

        eprintln!("[TiddlyDesktop] Windows: Created HBITMAP {}x{} from PNG", width, height);
        Some((hbitmap, width, height))
    }
}

/// Set up the drag image using IDragSourceHelper
fn setup_drag_image(
    data_object: &IDataObject,
    png_data: &[u8],
    offset_x: i32,
    offset_y: i32,
) -> bool {
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
    use windows::Win32::UI::Shell::{IDragSourceHelper, SHDRAGIMAGE, CLSID_DragDropHelper};

    // Create the bitmap from PNG
    let (hbitmap, width, height) = match create_hbitmap_from_png(png_data) {
        Some(result) => result,
        None => return false,
    };

    unsafe {
        // Create IDragSourceHelper
        let helper: Result<IDragSourceHelper, _> = CoCreateInstance(
            &CLSID_DragDropHelper,
            None,
            CLSCTX_INPROC_SERVER,
        );

        let helper = match helper {
            Ok(h) => h,
            Err(e) => {
                eprintln!("[TiddlyDesktop] Windows: Failed to create IDragSourceHelper: {:?}", e);
                return false;
            }
        };

        // Set up the SHDRAGIMAGE structure
        let mut drag_image: SHDRAGIMAGE = std::mem::zeroed();
        drag_image.sizeDragImage.cx = width;
        drag_image.sizeDragImage.cy = height;
        drag_image.ptOffset.x = offset_x;
        drag_image.ptOffset.y = offset_y;
        drag_image.hbmpDragImage = hbitmap;
        drag_image.crColorKey = windows::Win32::Foundation::COLORREF(0xFFFFFFFF); // No color key (we use alpha)

        // Initialize from bitmap
        match helper.InitializeFromBitmap(&drag_image, data_object) {
            Ok(()) => {
                eprintln!("[TiddlyDesktop] Windows: Drag image set successfully ({}x{}, offset {}, {})",
                    width, height, offset_x, offset_y);
                true
            }
            Err(e) => {
                eprintln!("[TiddlyDesktop] Windows: InitializeFromBitmap failed: {:?}", e);
                false
            }
        }
    }
}

/// Prepare for a potential native drag (called when internal drag starts)
/// This sets the outgoing drag state so that IDropTarget can detect same-window drags
/// and avoid emitting td-drag-content events that would trigger imports.
pub fn prepare_native_drag(window: &WebviewWindow, data: OutgoingDragData) -> Result<(), String> {
    let label = window.label().to_string();
    eprintln!(
        "[TiddlyDesktop] Windows: prepare_native_drag called for window '{}'",
        label
    );

    // Store drag state so IDropTarget can detect same-window drags
    let mut guard = OUTGOING_DRAG_STATE.lock().map_err(|e| e.to_string())?;
    *guard = Some(OutgoingDragState {
        data,
        source_window_label: label,
        data_was_requested: false,
    });

    Ok(())
}

/// Clean up native drag preparation (called when internal drag ends normally)
pub fn cleanup_native_drag() -> Result<(), String> {
    eprintln!("[TiddlyDesktop] Windows: cleanup_native_drag called");

    if let Ok(mut guard) = OUTGOING_DRAG_STATE.lock() {
        *guard = None;
    }

    Ok(())
}

/// Start a native drag operation (called from JavaScript when pointer leaves window during internal drag)
pub fn start_native_drag(window: &WebviewWindow, data: OutgoingDragData, _x: i32, _y: i32, image_data: Option<Vec<u8>>, image_offset_x: Option<i32>, image_offset_y: Option<i32>) -> Result<(), String> {
    use windows::Win32::System::Ole::DoDragDrop;

    let label = window.label().to_string();
    eprintln!(
        "[TiddlyDesktop] Windows: start_native_drag called for window '{}', has image: {}, offset: ({:?}, {:?})",
        label, image_data.is_some(), image_offset_x, image_offset_y
    );

    // Store drag state
    {
        let mut guard = OUTGOING_DRAG_STATE.lock().map_err(|e| e.to_string())?;
        *guard = Some(OutgoingDragState {
            data: data.clone(),
            source_window_label: label.clone(),
            data_was_requested: false,
        });
    }

    // Get HWND
    let hwnd = window.hwnd().map_err(|e| format!("Failed to get HWND: {}", e))?;
    let hwnd = HWND(hwnd.0 as *mut _);

    // Find the Chrome_WidgetWin for better drag source
    let target_hwnd = find_webview2_content_hwnd(hwnd).unwrap_or(hwnd);

    // Create COM objects
    let data_object_ptr = DataObjectImpl::new(data);
    let drop_source_ptr = DropSourceImpl::new(window.clone(), target_hwnd);

    eprintln!("[TiddlyDesktop] Windows: Starting DoDragDrop");

    // DoDragDrop is blocking - it runs its own message loop
    let result = unsafe {
        let data_object: IDataObject = std::mem::transmute(data_object_ptr);
        let drop_source: windows::Win32::System::Ole::IDropSource = std::mem::transmute(drop_source_ptr);

        // Set up the drag image if provided
        if let Some(ref img_data) = image_data {
            let offset_x = image_offset_x.unwrap_or(0);
            let offset_y = image_offset_y.unwrap_or(0);
            setup_drag_image(&data_object, img_data, offset_x, offset_y);
        }

        let mut effect: DROPEFFECT = DROPEFFECT_NONE;
        let hr = DoDragDrop(
            &data_object,
            &drop_source,
            DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK,
            &mut effect,
        );

        eprintln!("[TiddlyDesktop] Windows: DoDragDrop returned {:?}, effect {:?}", hr, effect);
        hr
    };

    // Get whether data was requested and emit end event
    let data_was_requested = OUTGOING_DRAG_STATE
        .lock()
        .ok()
        .and_then(|g| g.as_ref().map(|s| s.data_was_requested))
        .unwrap_or(false);

    let _ = window.emit(
        "td-drag-end",
        serde_json::json!({
            "data_was_requested": data_was_requested
        }),
    );

    // Clean up state
    if let Ok(mut guard) = OUTGOING_DRAG_STATE.lock() {
        *guard = None;
    }

    if result.is_ok() {
        Ok(())
    } else {
        Err(format!("DoDragDrop failed: {:?}", result))
    }
}

/// Set up drag-drop handling for a webview window
pub fn setup_drag_handlers(window: &WebviewWindow) {
    eprintln!(
        "[TiddlyDesktop] Windows: setup_drag_handlers called for window '{}'",
        window.label()
    );
    let window_for_drop = window.clone();

    let _ = window.with_webview(move |webview| {
        #[cfg(windows)]
        unsafe {
            use windows::core::Interface;

            let controller = webview.controller();

            // ENABLE WebView2's native external drop setting - this allows OLE drag-drop
            // to reach our custom IDropTarget. We then intercept via RegisterDragDrop.
            // Setting this to false would block external drops at the WebView2 level entirely.
            if let Ok(controller4) = controller.cast::<ICoreWebView2Controller4>() {
                let result = controller4.SetAllowExternalDrop(true);
                eprintln!(
                    "[TiddlyDesktop] Windows: SetAllowExternalDrop(true) result: {:?}",
                    result
                );
            }

            // Initialize OLE
            match OleInitialize(None) {
                Ok(()) => eprintln!("[TiddlyDesktop] Windows: OleInitialize succeeded"),
                Err(e) => eprintln!("[TiddlyDesktop] Windows: OleInitialize: {:?}", e),
            }

            // Register IDropTarget on the Chrome_WidgetWin content window
            if let Ok(parent_hwnd) = window_for_drop.hwnd() {
                let parent = HWND(parent_hwnd.0 as *mut _);

                // Find Chrome_WidgetWin with retry (created async)
                let mut target_hwnd = None;
                for attempt in 0..10 {
                    if let Some(hwnd) = find_webview2_content_hwnd(parent) {
                        target_hwnd = Some(hwnd);
                        eprintln!(
                            "[TiddlyDesktop] Windows: Found Chrome_WidgetWin on attempt {}",
                            attempt + 1
                        );
                        break;
                    }
                    if attempt < 9 {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                }
                let target_hwnd = target_hwnd.unwrap_or_else(|| {
                    eprintln!(
                        "[TiddlyDesktop] Windows: Chrome_WidgetWin not found, using parent"
                    );
                    parent
                });

                eprintln!(
                    "[TiddlyDesktop] Windows: Registering IDropTarget on HWND 0x{:x}",
                    target_hwnd.0 as isize
                );

                // Get the original IDropTarget before we revoke it.
                // WebView2 registers its IDropTarget asynchronously, so we retry with delays.
                // The property "OleDropTargetInterface" is set by OLE's RegisterDragDrop().
                let original_drop_target: Option<IDropTarget> = {
                    use windows::core::w;

                    let mut result: Option<IDropTarget> = None;

                    // Retry up to 20 times with 50ms delay (total 1 second max wait)
                    for attempt in 0..20 {
                        let prop = GetPropW(target_hwnd, w!("OleDropTargetInterface"));

                        if prop.0.is_null() {
                            // Property not set yet - WebView2 hasn't registered its IDropTarget
                            if attempt < 19 {
                                std::thread::sleep(std::time::Duration::from_millis(50));
                                continue;
                            }
                            eprintln!("[TiddlyDesktop] Windows: No OleDropTargetInterface property found after {} attempts", attempt + 1);
                            break;
                        }

                        // OLE stores the raw IDropTarget* directly in this property.
                        // GetPropW returns it without AddRef, so we wrap in ManuallyDrop
                        // and clone to get our own properly ref-counted reference.
                        let raw_ptr = prop.0 as *mut std::ffi::c_void;
                        eprintln!(
                            "[TiddlyDesktop] Windows: Found OleDropTargetInterface property: {:p}",
                            raw_ptr
                        );

                        let temp = std::mem::ManuallyDrop::new(
                            IDropTarget::from_raw(raw_ptr)
                        );

                        // Clone calls AddRef, giving us our own reference
                        result = Some((*temp).clone());
                        eprintln!(
                            "[TiddlyDesktop] Windows: Got original IDropTarget on attempt {}",
                            attempt + 1
                        );
                        break;
                    }

                    result
                };

                eprintln!("[TiddlyDesktop] Windows: original_drop_target acquired: {}", original_drop_target.is_some());

                let drop_target_ptr = DropTargetImpl::new(window_for_drop.clone(), target_hwnd, original_drop_target);
                let drop_target = DropTargetImpl::as_idroptarget(drop_target_ptr);
                DROP_TARGET_MAP
                    .lock()
                    .unwrap()
                    .insert(target_hwnd.0 as isize, SendDropTarget(drop_target_ptr));

                let _ = RevokeDragDrop(target_hwnd);
                match RegisterDragDrop(target_hwnd, &drop_target) {
                    Ok(()) => eprintln!("[TiddlyDesktop] Windows: RegisterDragDrop succeeded"),
                    Err(e) => eprintln!("[TiddlyDesktop] Windows: RegisterDragDrop failed: {:?}", e),
                }
            }
        }
    });
}

// ============================================================================
// FFI functions for potential WRY patch integration (matching macOS API)
// ============================================================================

/// FFI function to get stored text/plain data for internal drags.
/// Returns a pointer to a null-terminated C string, or null if no data is available.
/// The caller must NOT free this memory - it's managed by Rust.
#[no_mangle]
pub extern "C" fn tiddlydesktop_get_internal_drag_text_plain() -> *const std::ffi::c_char {
    // Use a thread-local to store the CString so it outlives the function call
    thread_local! {
        static CACHED_STRING: std::cell::RefCell<Option<std::ffi::CString>> = const { std::cell::RefCell::new(None) };
    }

    let text = OUTGOING_DRAG_STATE
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

/// FFI function to get stored text/vnd.tiddler data for internal drags.
/// Returns a pointer to a null-terminated C string containing the tiddler JSON,
/// or null if no data is available.
/// The caller must NOT free this memory - it's managed by Rust.
#[no_mangle]
pub extern "C" fn tiddlydesktop_get_internal_drag_tiddler_json() -> *const std::ffi::c_char {
    // Use a thread-local to store the CString so it outlives the function call
    thread_local! {
        static CACHED_STRING: std::cell::RefCell<Option<std::ffi::CString>> = const { std::cell::RefCell::new(None) };
    }

    let tiddler = OUTGOING_DRAG_STATE
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
    OUTGOING_DRAG_STATE
        .lock()
        .ok()
        .map(|guard| if guard.is_some() { 1 } else { 0 })
        .unwrap_or(0)
}
