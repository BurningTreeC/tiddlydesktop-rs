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
use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Controller4;
use windows::core::{GUID, HRESULT};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, POINTL, E_NOINTERFACE, E_POINTER, S_OK};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::Globalization::{MultiByteToWideChar, CP_ACP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS};
use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL};
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{
    IDropTarget, OleInitialize, RegisterDragDrop, RevokeDragDrop, DROPEFFECT, DROPEFFECT_COPY,
    DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE,
};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{EnumChildWindows, GetClassNameW};

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

/// Data captured from a drag operation
#[derive(Clone, Debug, serde::Serialize)]
pub struct DragContentData {
    pub types: Vec<String>,
    pub data: HashMap<String, String>,
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
    fn new(window: WebviewWindow, hwnd: HWND) -> *mut Self {
        let obj = Box::new(Self {
            vtbl: &DROPTARGET_VTBL,
            ref_count: AtomicU32::new(1),
            window,
            drag_active: Mutex::new(false),
            hwnd: hwnd.0 as isize,
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

    // IDropTarget::DragEnter - always emit events, JS checks TD.isInternalDragActive()
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

        eprintln!(
            "[TiddlyDesktop] Windows IDropTarget::DragEnter at screen({}, {}) -> client({}, {})",
            pt.x, pt.y, client_x, client_y
        );

        // Log available formats for debugging
        if !p_data_obj.is_null() {
            let data_object: &IDataObject = std::mem::transmute(&p_data_obj);
            obj.log_available_formats(data_object);
        }

        // Always emit td-drag-motion - JavaScript will filter internal drags
        // using TD.isInternalDragActive() check
        let _ = obj.window.emit(
            "td-drag-motion",
            serde_json::json!({
                "x": client_x,
                "y": client_y,
                "screenCoords": false
            }),
        );

        // Accept the drag
        if !pdw_effect.is_null() {
            let allowed = DROPEFFECT(*pdw_effect);
            *pdw_effect = choose_drop_effect(allowed).0 as u32;
        }

        S_OK
    }

    // IDropTarget::DragOver - always emit events
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
                "[TiddlyDesktop] Windows IDropTarget::DragOver at screen({}, {}) -> client({}, {})",
                pt.x, pt.y, client_x, client_y
            );
        }

        // Always emit - JS checks TD.isInternalDragActive()
        let _ = obj.window.emit(
            "td-drag-motion",
            serde_json::json!({
                "x": client_x,
                "y": client_y,
                "screenCoords": false
            }),
        );

        if !pdw_effect.is_null() {
            let allowed = DROPEFFECT(*pdw_effect);
            *pdw_effect = choose_drop_effect(allowed).0 as u32;
        }

        S_OK
    }

    // IDropTarget::DragLeave
    unsafe extern "system" fn drag_leave(this: *mut Self) -> HRESULT {
        let obj = &*this;
        eprintln!("[TiddlyDesktop] Windows IDropTarget::DragLeave");

        let was_active = {
            let mut active = obj.drag_active.lock().unwrap();
            let was = *active;
            *active = false;
            was
        };

        if was_active {
            let _ = obj.window.emit("td-drag-leave", ());
        }

        S_OK
    }

    // IDropTarget::Drop - extract content and emit events
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

        eprintln!(
            "[TiddlyDesktop] Windows IDropTarget::Drop at screen({}, {}) -> client({}, {})",
            pt.x, pt.y, client_x, client_y
        );

        if !p_data_obj.is_null() {
            let data_object: &IDataObject = std::mem::transmute(&p_data_obj);
            obj.log_available_formats(data_object);

            // Emit drop-start
            let _ = obj.window.emit(
                "td-drag-drop-start",
                serde_json::json!({
                    "x": client_x,
                    "y": client_y,
                    "screenCoords": false
                }),
            );

            // Check for file paths first
            let file_paths = obj.get_file_paths(data_object);
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
                        "screenCoords": false
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
            if let Some(content_data) = obj.extract_data(data_object) {
                eprintln!(
                    "[TiddlyDesktop] Windows IDropTarget::Drop - content types: {:?}",
                    content_data.types
                );
                let _ = obj.window.emit(
                    "td-drag-drop-position",
                    serde_json::json!({
                        "x": client_x,
                        "y": client_y,
                        "screenCoords": false
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

        // 1. text/vnd.tiddler
        let cf_tiddler = get_cf_tiddler();
        if let Some(tiddler) = self.get_string_data(data_object, cf_tiddler) {
            types.push("text/vnd.tiddler".to_string());
            data.insert("text/vnd.tiddler".to_string(), tiddler);
        }

        // 2. URL (UniformResourceLocator)
        let cf_url_w = get_cf_url_w();
        let cf_url = get_cf_url();
        if let Some(url) = self.get_unicode_text_format(data_object, cf_url_w) {
            types.push("URL".to_string());
            data.insert("URL".to_string(), url);
        } else if let Some(url) = self.get_string_data(data_object, cf_url) {
            types.push("URL".to_string());
            data.insert("URL".to_string(), url);
        }

        // 3. text/x-moz-url
        let cf_moz_url = get_cf_moz_url();
        if let Some(moz_url) = self.get_unicode_text_format(data_object, cf_moz_url) {
            let url = moz_url.lines().next().unwrap_or(&moz_url);
            types.push("text/x-moz-url".to_string());
            data.insert("text/x-moz-url".to_string(), url.to_string());
        }

        // 4. text/html (HTML Format)
        let cf_html = get_cf_html();
        if let Some(html) = self.get_string_data(data_object, cf_html) {
            // Extract content from Windows HTML Format markers
            if let Some(start) = html.find("<!--StartFragment-->") {
                if let Some(end) = html.find("<!--EndFragment-->") {
                    let content = &html[start + 20..end];
                    types.push("text/html".to_string());
                    data.insert("text/html".to_string(), content.to_string());
                }
            } else {
                types.push("text/html".to_string());
                data.insert("text/html".to_string(), html);
            }
        }

        // 5. text/plain (CF_UNICODETEXT)
        if let Some(text) = self.get_unicode_text(data_object) {
            types.push("text/plain".to_string());
            data.insert("text/plain".to_string(), text);
        }

        // 6. Text (CF_TEXT fallback)
        if let Some(text) = self.get_ansi_text(data_object) {
            types.push("Text".to_string());
            data.insert("Text".to_string(), text);
        }

        // 7. text/uri-list
        let cf_uri = get_cf_uri_list();
        if let Some(uri_list) = self.get_string_data(data_object, cf_uri) {
            types.push("text/uri-list".to_string());
            data.insert("text/uri-list".to_string(), uri_list);
        }

        if types.is_empty() {
            None
        } else {
            Some(DragContentData { types, data })
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

            // DISABLE WebView2's native external drop - we handle it via IDropTarget
            // WebView2's native handling doesn't expose content (text/html) to JavaScript
            if let Ok(controller4) = controller.cast::<ICoreWebView2Controller4>() {
                let result = controller4.SetAllowExternalDrop(false);
                eprintln!(
                    "[TiddlyDesktop] Windows: SetAllowExternalDrop(false) result: {:?}",
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

                let drop_target_ptr = DropTargetImpl::new(window_for_drop.clone(), target_hwnd);
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
