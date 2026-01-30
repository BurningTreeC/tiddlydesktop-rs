//! Windows drag-drop handling
//!
//! Incoming drops: We hook RegisterDragDrop to wrap WebView2's IDropTarget with our proxy.
//! The proxy captures file paths and stores them via FFI, then forwards to WebView2's
//! original IDropTarget so native HTML5 drop events fire normally.
//! JavaScript retrieves file paths from FFI after handling the native drop event.
//!
//! Outgoing drags: Implemented via OLE DoDragDrop with custom IDataObject.
//! This allows dragging tiddlers to external apps with proper MIME types.

#![cfg(target_os = "windows")]

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::sync::Once;

use tauri::{Emitter, WebviewWindow};

use windows::core::{GUID, HRESULT, BOOL};
use windows::Win32::Foundation::{HWND, LPARAM, E_NOINTERFACE, E_POINTER, S_OK};
use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL};
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{
    OleInitialize, CF_HDROP,
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE,
};
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::{EnumChildWindows, GetClassNameW};

use minhook::MinHook;

// WebView2 DragStarting API (from our forked webview2-com with SDK 1.0.3719.77)
use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2CompositionController,
    ICoreWebView2CompositionController5,
    ICoreWebView2Controller4,
    ICoreWebView2DragStartingEventArgs,
    ICoreWebView2DragStartingEventHandler,
    ICoreWebView2DragStartingEventHandler_Impl,
};
use windows_core::Interface;


/// Clipboard format constants
const CF_UNICODETEXT: u16 = 13;

/// IUnknown interface GUID (used by COM implementations)
const IID_IUNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);

/// Custom clipboard format for HTML
fn get_cf_html() -> u16 {
    use windows::core::w;
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    unsafe { RegisterClipboardFormatW(w!("HTML Format")) as u16 }
}

/// Custom clipboard format for TiddlyWiki tiddler
fn get_cf_tiddler() -> u16 {
    use windows::core::w;
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    unsafe { RegisterClipboardFormatW(w!("text/vnd.tiddler")) as u16 }
}

/// Standard Windows clipboard format for URLs (Unicode)
fn get_cf_url_w() -> u16 {
    use windows::core::w;
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    unsafe { RegisterClipboardFormatW(w!("UniformResourceLocatorW")) as u16 }
}

/// Mozilla URL format
fn get_cf_moz_url() -> u16 {
    use windows::core::w;
    use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
    unsafe { RegisterClipboardFormatW(w!("text/x-moz-url")) as u16 }
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

// NOTE: IDropTarget implementation removed - we now rely on WebView2's native drop handling.
// See setup_drag_handlers() for details.

// ============================================================================
// DragStarting event handler (WebView2 SDK 1.0.3719.77+)
// ============================================================================

/// Handler for WebView2 DragStarting events.
/// This fires when a drag operation starts inside the WebView (e.g., dragging a tiddler).
/// We use this to intercept the drag data and populate OUTGOING_DRAG_STATE for cross-window drags.
#[windows_implement::implement(ICoreWebView2DragStartingEventHandler)]
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
        _sender: windows_core::Ref<'_, ICoreWebView2CompositionController>,
        args: windows_core::Ref<'_, ICoreWebView2DragStartingEventArgs>,
    ) -> windows_core::Result<()> {
        eprintln!(
            "[TiddlyDesktop] Windows DragStarting: Drag started in window '{}'",
            self.window_label
        );

        // Get the IDataObject from the drag event
        // Use cloned() to get the underlying interface from the Ref wrapper
        let data_object = match args.cloned() {
            Some(args_inner) => unsafe { args_inner.Data() },
            None => {
                eprintln!("[TiddlyDesktop] Windows DragStarting: args was null");
                return Ok(());
            }
        };
        match data_object {
            Ok(data_obj) => {
                // Extract drag data from the IDataObject
                let drag_data = extract_drag_data_from_idataobject(&data_obj);

                eprintln!(
                    "[TiddlyDesktop] Windows DragStarting: Extracted data - text_plain: {:?}, has_tiddler: {}",
                    drag_data.text_plain.as_ref().map(|s| s.chars().take(50).collect::<String>()),
                    drag_data.text_vnd_tiddler.is_some()
                );

                // Store the drag data for potential cross-window drops
                if let Ok(mut guard) = OUTGOING_DRAG_STATE.lock() {
                    *guard = Some(OutgoingDragState {
                        data: drag_data,
                        source_window_label: self.window_label.clone(),
                        data_was_requested: false,
                    });
                }
            }
            Err(e) => {
                eprintln!(
                    "[TiddlyDesktop] Windows DragStarting: Failed to get IDataObject: {:?}",
                    e
                );
            }
        }

        // Don't set Handled = true - let WebView2 continue with its default drag behavior.
        // We've captured the data above for cross-window scenarios - target windows
        // query OUTGOING_DRAG_STATE via get_pending_drag_data IPC.
        //
        // Since we no longer register IDropTarget (see WRY patch), WebView2's native
        // drop handling remains active and HTML5 drag events fire normally.

        Ok(())
    }
}

/// Extract drag data from an IDataObject (used by DragStarting handler)
fn extract_drag_data_from_idataobject(data_object: &IDataObject) -> OutgoingDragData {
    let mut data = OutgoingDragData::default();

    // Try to get text/plain (CF_UNICODETEXT)
    data.text_plain = get_unicode_text_from_idataobject(data_object, CF_UNICODETEXT);

    // Try to get text/vnd.tiddler
    let cf_tiddler = get_cf_tiddler();
    data.text_vnd_tiddler = get_string_from_idataobject(data_object, cf_tiddler);

    // Try to get HTML
    let cf_html = get_cf_html();
    data.text_html = get_string_from_idataobject(data_object, cf_html);

    // Try to get URL
    let cf_url_w = get_cf_url_w();
    data.url = get_unicode_text_from_idataobject(data_object, cf_url_w);

    // Try to get text/x-moz-url
    let cf_moz_url = get_cf_moz_url();
    data.text_x_moz_url = get_unicode_text_from_idataobject(data_object, cf_moz_url);

    data
}

/// Get Unicode text from an IDataObject for a specific clipboard format
fn get_unicode_text_from_idataobject(data_object: &IDataObject, cf: u16) -> Option<String> {
    use windows::Win32::System::Memory::GlobalSize;

    let format = FORMATETC {
        cfFormat: cf,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0 as u32,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };

    unsafe {
        match data_object.GetData(&format) {
            Ok(medium) => {
                let hglobal = medium.u.hGlobal;
                if hglobal.0.is_null() {
                    return None;
                }

                let ptr = GlobalLock(hglobal);
                if ptr.is_null() {
                    return None;
                }

                let size = GlobalSize(hglobal);
                if size == 0 {
                    let _ = GlobalUnlock(hglobal);
                    return None;
                }

                // Read as UTF-16
                let wide_chars = std::slice::from_raw_parts(ptr as *const u16, size / 2);
                // Find null terminator
                let len = wide_chars.iter().position(|&c| c == 0).unwrap_or(wide_chars.len());
                let text = String::from_utf16_lossy(&wide_chars[..len]);

                let _ = GlobalUnlock(hglobal);

                if text.is_empty() {
                    None
                } else {
                    Some(text)
                }
            }
            Err(_) => None,
        }
    }
}

/// Get a string (UTF-8) from an IDataObject for a specific clipboard format
fn get_string_from_idataobject(data_object: &IDataObject, cf: u16) -> Option<String> {
    use windows::Win32::System::Memory::GlobalSize;

    let format = FORMATETC {
        cfFormat: cf,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0 as u32,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };

    unsafe {
        match data_object.GetData(&format) {
            Ok(medium) => {
                let hglobal = medium.u.hGlobal;
                if hglobal.0.is_null() {
                    return None;
                }

                let ptr = GlobalLock(hglobal);
                if ptr.is_null() {
                    return None;
                }

                let size = GlobalSize(hglobal);
                if size == 0 {
                    let _ = GlobalUnlock(hglobal);
                    return None;
                }

                // Read as UTF-8 bytes
                let bytes = std::slice::from_raw_parts(ptr as *const u8, size);
                // Find null terminator
                let len = bytes.iter().position(|&c| c == 0).unwrap_or(bytes.len());
                let text = String::from_utf8_lossy(&bytes[..len]).to_string();

                let _ = GlobalUnlock(hglobal);

                if text.is_empty() {
                    None
                } else {
                    Some(text)
                }
            }
            Err(_) => None,
        }
    }
}

// Global storage for the DragStarting event token (to remove handler on cleanup)
lazy_static::lazy_static! {
    static ref DRAG_STARTING_TOKENS: Mutex<std::collections::HashMap<String, i64>> = Mutex::new(std::collections::HashMap::new());
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

/// Set up drag-drop handling for a webview window.
///
/// On Windows, we set up:
/// 1. OLE initialization for DoDragDrop (outgoing drags)
/// 2. DragStarting event handler to intercept drags starting in WebView2
/// 3. Store the composition controller so our IDropTarget wrapper can forward to it
///
/// This enables full native OLE drag-and-drop:
/// - Inside → Inside: Native DOM events via composition controller
/// - Inside → Outside: DragStarting captures data, OLE DoDragDrop handles transfer
/// - Outside → Inside: Our IDropTarget wrapper extracts paths AND forwards to composition controller
pub fn setup_drag_handlers(window: &WebviewWindow) {
    let window_label = window.label().to_string();
    let window_label_clone = window_label.clone();

    // Get the HWND for storing the composition controller (convert to usize to be Send)
    let hwnd_key = match window.hwnd() {
        Ok(h) => h.0 as usize,
        Err(e) => {
            eprintln!("[TiddlyDesktop] Windows: Failed to get HWND: {:?}", e);
            return;
        }
    };

    eprintln!(
        "[TiddlyDesktop] Windows: setup_drag_handlers called for window '{}' (hwnd: {:#x})",
        window_label, hwnd_key
    );

    let _ = window.with_webview(move |webview| {
        #[cfg(windows)]
        unsafe {
            // Initialize OLE (required for DoDragDrop when starting outgoing drags)
            match OleInitialize(None) {
                Ok(()) => eprintln!("[TiddlyDesktop] Windows: OleInitialize succeeded"),
                Err(e) => eprintln!("[TiddlyDesktop] Windows: OleInitialize: {:?}", e),
            }

            // Get the WebView2 controller
            let controller = webview.controller();

            // Enable external drops - required for composition hosting mode
            // Without this, WebView2 won't accept any drops (shows "not allowed" cursor)
            match controller.cast::<ICoreWebView2Controller4>() {
                Ok(controller4) => {
                    match controller4.SetAllowExternalDrop(true) {
                        Ok(()) => {
                            eprintln!("[TiddlyDesktop] Windows: SetAllowExternalDrop(true) succeeded");
                        }
                        Err(e) => {
                            eprintln!("[TiddlyDesktop] Windows: SetAllowExternalDrop failed: {:?}", e);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[TiddlyDesktop] Windows: Failed to get ICoreWebView2Controller4: {:?}", e);
                }
            }

            // Get ICoreWebView2CompositionController5 for both drag forwarding and DragStarting
            // Controller5 inherits from Controller3, so it has DragEnter/DragOver/DragLeave/Drop
            // as well as the newer DragStarting event
            match controller.cast::<ICoreWebView2CompositionController5>() {
                Ok(controller5) => {
                    eprintln!("[TiddlyDesktop] Windows: Got ICoreWebView2CompositionController5 for drag forwarding");
                    // Store it so our IDropTarget wrapper can find it
                    store_composition_controller(hwnd_key, controller5.clone());

                    // Create the DragStarting handler
                    let handler: ICoreWebView2DragStartingEventHandler =
                        DragStartingHandler::new(window_label_clone.clone()).into();

                    // Register the handler
                    let mut token: i64 = 0;
                    match controller5.add_DragStarting(&handler, &mut token) {
                        Ok(()) => {
                            eprintln!(
                                "[TiddlyDesktop] Windows: DragStarting handler registered (token: {})",
                                token
                            );
                            // Store the token for potential cleanup later
                            if let Ok(mut tokens) = DRAG_STARTING_TOKENS.lock() {
                                tokens.insert(window_label_clone.clone(), token);
                            }
                        }
                        Err(e) => {
                            eprintln!(
                                "[TiddlyDesktop] Windows: Failed to register DragStarting handler: {:?}",
                                e
                            );
                        }
                    }
                }
                Err(e) => {
                    eprintln!(
                        "[TiddlyDesktop] Windows: Failed to get ICoreWebView2CompositionController5: {:?}",
                        e
                    );
                    eprintln!(
                        "[TiddlyDesktop] Windows: DragStarting API not available (need WebView2 Runtime 131+)"
                    );
                }
            }

            eprintln!("[TiddlyDesktop] Windows: Drag-drop setup complete - wrapper will extract paths and forward to WebView2");
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

// ============================================================================
// FFI functions for WRY patch: External file drop path extraction
// ============================================================================

lazy_static::lazy_static! {
    /// Global storage for file paths from external drops (populated by WRY patch via FFI)
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
                eprintln!("[TiddlyDesktop] Windows FFI: Storing {} drop paths", paths.len());
                for path in &paths {
                    eprintln!("[TiddlyDesktop] Windows FFI:   - {}", path);
                }
                if let Ok(mut guard) = EXTERNAL_DROP_PATHS.lock() {
                    *guard = Some(paths);
                }
            }
        }
    }
}

/// FFI function called by WRY patch to clear stored file paths (e.g., on drag leave).
#[no_mangle]
pub extern "C" fn tiddlydesktop_clear_drop_paths() {
    eprintln!("[TiddlyDesktop] Windows FFI: Clearing drop paths");
    if let Ok(mut guard) = EXTERNAL_DROP_PATHS.lock() {
        *guard = None;
    }
}

/// Get the stored external drop paths (called from Tauri command).
/// Returns the paths and clears the storage.
pub fn take_external_drop_paths() -> Option<Vec<String>> {
    if let Ok(mut guard) = EXTERNAL_DROP_PATHS.lock() {
        guard.take()
    } else {
        None
    }
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
    let guard = OUTGOING_DRAG_STATE.lock().ok()?;
    let state = guard.as_ref()?;

    // Only return data if it's a cross-wiki drag (different window)
    if state.source_window_label == target_window {
        eprintln!(
            "[TiddlyDesktop] Windows: get_pending_drag_data - same window '{}', returning None",
            target_window
        );
        return None;
    }

    eprintln!(
        "[TiddlyDesktop] Windows: get_pending_drag_data - cross-wiki from '{}' to '{}', returning data",
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
// RegisterDragDrop Hook: Capture file paths while preserving WebView2's
// native HTML5 drop events via ICoreWebView2CompositionController5
// ============================================================================
//
// Strategy (based on Microsoft WebView2 documentation):
// 1. WebView2 does NOT register its own IDropTarget - the host app is supposed to do it
// 2. WRY registers its IDropTarget on WebView2's child window
// 3. We hook RegisterDragDrop to wrap WRY's IDropTarget
// 4. Our wrapper:
//    a) Extracts file paths from IDataObject → stores via FFI for JavaScript
//    b) Forwards to ICoreWebView2CompositionController5.DragEnter/DragOver/DragLeave/Drop
// 5. The composition controller fires native HTML5 drag events
//
// This gives us: file path extraction + native HTML5 events
//
// Note: Controller5 inherits from Controller3, so it has all the drag methods
// plus the newer DragStarting event for outgoing drags.

// Type alias for the RegisterDragDrop function signature
type FnRegisterDragDrop = unsafe extern "system" fn(HWND, *mut std::ffi::c_void) -> HRESULT;

static HOOK_INIT: Once = Once::new();

/// Global storage for the original RegisterDragDrop function pointer (trampoline)
static mut ORIGINAL_REGISTER_DRAG_DROP: Option<FnRegisterDragDrop> = None;

/// Stored composition controller with its host window HWND
/// Wrapped for Send+Sync safety (COM objects are thread-safe on Windows)
struct ControllerWithHost {
    controller: ICoreWebView2CompositionController5,
    host_hwnd: usize,  // Main window HWND for coordinate conversion
}

// Safety: COM objects are thread-safe on Windows when accessed through proper COM mechanisms
unsafe impl Send for ControllerWithHost {}
unsafe impl Sync for ControllerWithHost {}

lazy_static::lazy_static! {
    /// Composition controller - used to forward drag events to WebView2
    /// so that native HTML5 drag events fire
    /// We store just one controller (Tauri typically has one WebView per window)
    static ref COMPOSITION_CONTROLLER: Mutex<Option<ControllerWithHost>> =
        Mutex::new(None);

    /// Track which HWNDs have our wrapper registered (to avoid double-wrapping)
    static ref WRAPPED_HWNDS: Mutex<std::collections::HashSet<usize>> =
        Mutex::new(std::collections::HashSet::new());
}

/// Store a composition controller with its host window HWND (called from setup_drag_handlers)
pub fn store_composition_controller(host_hwnd: usize, controller: ICoreWebView2CompositionController5) {
    if let Ok(mut guard) = COMPOSITION_CONTROLLER.lock() {
        eprintln!("[TiddlyDesktop] Windows: Storing composition controller for host hwnd {:#x}", host_hwnd);
        *guard = Some(ControllerWithHost { controller, host_hwnd });
    }
}

/// Get the composition controller and host HWND
fn get_composition_controller_with_host() -> Option<(ICoreWebView2CompositionController5, HWND)> {
    COMPOSITION_CONTROLLER.lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(|c| (c.controller.clone(), HWND(c.host_hwnd as *mut _))))
}

/// Initialize the RegisterDragDrop hook. Must be called before WebView2 initializes.
pub fn init_drop_target_hook() {
    HOOK_INIT.call_once(|| {
        unsafe {
            // Get the address of RegisterDragDrop from ole32.dll
            let module = windows::Win32::System::LibraryLoader::GetModuleHandleW(
                windows::core::w!("ole32.dll")
            );

            if let Ok(module) = module {
                let proc_addr = windows::Win32::System::LibraryLoader::GetProcAddress(
                    module,
                    windows::core::s!("RegisterDragDrop")
                );

                if let Some(addr) = proc_addr {
                    let target_fn: FnRegisterDragDrop = std::mem::transmute(addr);
                    let detour_fn: FnRegisterDragDrop = hooked_register_drag_drop;

                    match MinHook::create_hook(target_fn as *mut _, detour_fn as *mut _) {
                        Ok(trampoline) => {
                            ORIGINAL_REGISTER_DRAG_DROP = Some(std::mem::transmute(trampoline));

                            match MinHook::enable_hook(target_fn as *mut _) {
                                Ok(()) => {
                                    eprintln!("[TiddlyDesktop] Windows: RegisterDragDrop hook installed successfully");
                                }
                                Err(e) => {
                                    eprintln!("[TiddlyDesktop] Windows: Failed to enable RegisterDragDrop hook: {:?}", e);
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("[TiddlyDesktop] Windows: Failed to create RegisterDragDrop hook: {:?}", e);
                        }
                    }
                } else {
                    eprintln!("[TiddlyDesktop] Windows: GetProcAddress failed for RegisterDragDrop");
                }
            } else {
                eprintln!("[TiddlyDesktop] Windows: GetModuleHandleW failed for ole32.dll");
            }
        }
    });
}

/// Hooked RegisterDragDrop - wraps the IDropTarget with our proxy that forwards to composition controller
unsafe extern "system" fn hooked_register_drag_drop(hwnd: HWND, drop_target: *mut std::ffi::c_void) -> HRESULT {
    let hwnd_key = hwnd.0 as usize;
    eprintln!("[TiddlyDesktop] Windows: RegisterDragDrop hook called for hwnd {:?}", hwnd.0);

    // Get the original function
    let original_fn = match ORIGINAL_REGISTER_DRAG_DROP {
        Some(f) => f,
        None => {
            eprintln!("[TiddlyDesktop] Windows: No original RegisterDragDrop function stored, returning error");
            return HRESULT::from_win32(0x80004005); // E_FAIL
        }
    };

    if drop_target.is_null() {
        return original_fn(hwnd, drop_target);
    }

    // Check if we already wrapped this HWND (avoid double-wrapping)
    let already_wrapped = WRAPPED_HWNDS.lock()
        .map(|guard| guard.contains(&hwnd_key))
        .unwrap_or(false);

    if already_wrapped {
        eprintln!("[TiddlyDesktop] Windows: HWND {:?} already wrapped, passing through", hwnd.0);
        return original_fn(hwnd, drop_target);
    }

    // Always wrap IDropTarget - we'll do lazy lookup of the composition controller
    // when drag events happen. This handles the timing issue where RegisterDragDrop
    // is called before setup_drag_handlers runs.
    eprintln!("[TiddlyDesktop] Windows: Wrapping IDropTarget for hwnd {:?}", hwnd.0);

    // Create our wrapper - it will forward to the composition controller
    // We don't need the original IDropTarget (WRY's) since we forward to WebView2 directly
    let wrapper = DropTargetWrapper::new(hwnd);
    let wrapper_ptr = Box::into_raw(Box::new(wrapper));

    // Register the wrapper instead of WRY's IDropTarget
    let result = original_fn(hwnd, wrapper_ptr as *mut std::ffi::c_void);

    if result.is_ok() {
        eprintln!("[TiddlyDesktop] Windows: Wrapped IDropTarget registered successfully for hwnd {:?}", hwnd.0);
        // Track that this hwnd has our wrapper
        if let Ok(mut guard) = WRAPPED_HWNDS.lock() {
            guard.insert(hwnd_key);
        }
    } else {
        eprintln!("[TiddlyDesktop] Windows: RegisterDragDrop failed: {:?}", result);
        // Clean up the wrapper if registration failed
        let _ = Box::from_raw(wrapper_ptr);
    }

    result
}

// IDropTarget interface GUID
const IID_IDROPTARGET: GUID = GUID::from_u128(0x00000122_0000_0000_c000_000000000046);

/// IDropTarget vtable
#[repr(C)]
#[allow(non_snake_case)]
struct IDropTargetVtbl {
    // IUnknown
    QueryInterface: unsafe extern "system" fn(*mut DropTargetWrapper, *const GUID, *mut *mut std::ffi::c_void) -> HRESULT,
    AddRef: unsafe extern "system" fn(*mut DropTargetWrapper) -> u32,
    Release: unsafe extern "system" fn(*mut DropTargetWrapper) -> u32,
    // IDropTarget
    DragEnter: unsafe extern "system" fn(*mut DropTargetWrapper, *mut std::ffi::c_void, u32, i64, *mut u32) -> HRESULT,
    DragOver: unsafe extern "system" fn(*mut DropTargetWrapper, u32, i64, *mut u32) -> HRESULT,
    DragLeave: unsafe extern "system" fn(*mut DropTargetWrapper) -> HRESULT,
    Drop: unsafe extern "system" fn(*mut DropTargetWrapper, *mut std::ffi::c_void, u32, i64, *mut u32) -> HRESULT,
}

static DROP_TARGET_WRAPPER_VTBL: IDropTargetVtbl = IDropTargetVtbl {
    QueryInterface: DropTargetWrapper::query_interface,
    AddRef: DropTargetWrapper::add_ref,
    Release: DropTargetWrapper::release,
    DragEnter: DropTargetWrapper::drag_enter,
    DragOver: DropTargetWrapper::drag_over,
    DragLeave: DropTargetWrapper::drag_leave,
    Drop: DropTargetWrapper::drop,
};

/// Our IDropTarget wrapper that captures file paths and forwards to
/// ICoreWebView2CompositionController5 so native HTML5 drag events fire.
#[repr(C)]
struct DropTargetWrapper {
    vtbl: *const IDropTargetVtbl,
    ref_count: AtomicU32,
    /// The HWND this wrapper is registered for (used to find the composition controller)
    hwnd: HWND,
}

// Safety: COM objects need to be Send+Sync for cross-thread access
unsafe impl Send for DropTargetWrapper {}
unsafe impl Sync for DropTargetWrapper {}

impl DropTargetWrapper {
    fn new(hwnd: HWND) -> Self {
        Self {
            vtbl: &DROP_TARGET_WRAPPER_VTBL,
            ref_count: AtomicU32::new(1),
            hwnd,
        }
    }

    /// Extract file paths from IDataObject
    unsafe fn extract_file_paths(data_obj: *mut std::ffi::c_void) -> Vec<PathBuf> {
        let mut paths = Vec::new();

        if data_obj.is_null() {
            return paths;
        }

        // Cast to IDataObject
        let data_object: &IDataObject = match (data_obj as *const IDataObject).as_ref() {
            Some(obj) => obj,
            None => return paths,
        };

        // Set up FORMATETC for CF_HDROP
        let format = FORMATETC {
            cfFormat: CF_HDROP.0,
            ptd: ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0 as u32,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };

        // Try to get the data
        match data_object.GetData(&format) {
            Ok(medium) => {
                let hglobal = medium.u.hGlobal;
                if !hglobal.0.is_null() {
                    let hdrop = HDROP(hglobal.0 as _);

                    // Get the number of files
                    let file_count = DragQueryFileW(hdrop, 0xFFFFFFFF, None);

                    for i in 0..file_count {
                        // Get the length of the file path
                        let len = DragQueryFileW(hdrop, i, None) as usize;
                        if len > 0 {
                            let mut buffer = vec![0u16; len + 1];
                            DragQueryFileW(hdrop, i, Some(&mut buffer));
                            let path_str = OsString::from_wide(&buffer[..len]);
                            paths.push(PathBuf::from(path_str));
                        }
                    }
                }
            }
            Err(_) => {
                // Not a file drop, that's OK
            }
        }

        paths
    }

    /// Find the composition controller and its host HWND for coordinate conversion
    /// Returns (controller, host_hwnd) - use host_hwnd for ScreenToClient
    fn find_composition_controller_with_host(&self) -> Option<(ICoreWebView2CompositionController5, HWND)> {
        get_composition_controller_with_host()
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
        if *iid == IID_IUNKNOWN || *iid == IID_IDROPTARGET {
            Self::add_ref(this);
            *ppv = this as *mut std::ffi::c_void;
            S_OK
        } else {
            *ppv = ptr::null_mut();
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

    unsafe extern "system" fn drag_enter(
        this: *mut Self,
        data_obj: *mut std::ffi::c_void,
        key_state: u32,
        pt: i64,
        effect: *mut u32,
    ) -> HRESULT {
        let wrapper = &*this;

        // Check if this is an internal drag (from our app)
        let is_internal = tiddlydesktop_has_internal_drag() != 0;

        if !is_internal {
            // Extract file paths and store them for JavaScript to retrieve
            let paths = Self::extract_file_paths(data_obj);
            if !paths.is_empty() {
                eprintln!("[TiddlyDesktop] Windows Wrapper: DragEnter with {} file paths", paths.len());

                // Store paths for later retrieval by JavaScript
                let json_parts: Vec<String> = paths.iter().map(|p| {
                    let s = p.to_string_lossy();
                    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
                    format!("\"{}\"", escaped)
                }).collect();
                let json = format!("[{}]", json_parts.join(","));

                if let Ok(mut guard) = EXTERNAL_DROP_PATHS.lock() {
                    if let Ok(parsed) = serde_json::from_str::<Vec<String>>(&json) {
                        *guard = Some(parsed);
                        eprintln!("[TiddlyDesktop] Windows Wrapper: Stored {} paths for JS retrieval", paths.len());
                    }
                }
            }
        } else {
            eprintln!("[TiddlyDesktop] Windows Wrapper: DragEnter - internal drag, skipping path extraction");
        }

        // Forward to WebView2's composition controller to fire HTML5 events
        if let Some((controller, host_hwnd)) = wrapper.find_composition_controller_with_host() {
            // IDropTarget receives screen coordinates, but CompositionController expects client coordinates
            let pt_x = (pt & 0xFFFFFFFF) as i32;
            let pt_y = (pt >> 32) as i32;
            let mut point = windows::Win32::Foundation::POINT { x: pt_x, y: pt_y };

            // Convert screen coords to HOST window's client coords
            // In Tauri, WebView fills the window, so no additional offset needed
            let _ = windows::Win32::Graphics::Gdi::ScreenToClient(host_hwnd, &mut point);

            // Convert raw pointer to IDataObject
            let data_object: Option<IDataObject> = if data_obj.is_null() {
                None
            } else {
                Some(std::mem::transmute(data_obj))
            };

            let result = controller.DragEnter(
                data_object.as_ref(),
                key_state,
                point,
                effect as *mut u32,
            );

            // Don't drop the IDataObject - we don't own it
            if let Some(d) = data_object {
                std::mem::forget(d);
            }

            match result {
                Ok(()) => {
                    eprintln!("[TiddlyDesktop] Windows Wrapper: DragEnter forwarded to composition controller");
                    S_OK
                }
                Err(e) => {
                    eprintln!("[TiddlyDesktop] Windows Wrapper: DragEnter forward failed: {:?}", e);
                    e.code()
                }
            }
        } else {
            eprintln!("[TiddlyDesktop] Windows Wrapper: No composition controller found for DragEnter");
            // Return DROPEFFECT_COPY to allow the drop
            if !effect.is_null() {
                *effect = DROPEFFECT_COPY.0;
            }
            S_OK
        }
    }

    unsafe extern "system" fn drag_over(
        this: *mut Self,
        key_state: u32,
        pt: i64,
        effect: *mut u32,
    ) -> HRESULT {
        let wrapper = &*this;

        // Forward to WebView2's composition controller
        if let Some((controller, host_hwnd)) = wrapper.find_composition_controller_with_host() {
            // Convert screen coords to HOST window's client coords
            let pt_x = (pt & 0xFFFFFFFF) as i32;
            let pt_y = (pt >> 32) as i32;
            let mut point = windows::Win32::Foundation::POINT { x: pt_x, y: pt_y };
            let _ = windows::Win32::Graphics::Gdi::ScreenToClient(host_hwnd, &mut point);

            match controller.DragOver(key_state, point, effect as *mut u32) {
                Ok(()) => S_OK,
                Err(e) => e.code(),
            }
        } else {
            // Return DROPEFFECT_COPY to allow the drop
            if !effect.is_null() {
                *effect = DROPEFFECT_COPY.0;
            }
            S_OK
        }
    }

    unsafe extern "system" fn drag_leave(this: *mut Self) -> HRESULT {
        let wrapper = &*this;

        // Clear stored paths on leave
        if let Ok(mut guard) = EXTERNAL_DROP_PATHS.lock() {
            if guard.is_some() {
                eprintln!("[TiddlyDesktop] Windows Wrapper: DragLeave - clearing stored paths");
                *guard = None;
            }
        }

        // Forward to WebView2's composition controller
        if let Some((controller, _host_hwnd)) = wrapper.find_composition_controller_with_host() {
            match controller.DragLeave() {
                Ok(()) => S_OK,
                Err(e) => e.code(),
            }
        } else {
            S_OK
        }
    }

    unsafe extern "system" fn drop(
        this: *mut Self,
        data_obj: *mut std::ffi::c_void,
        key_state: u32,
        pt: i64,
        effect: *mut u32,
    ) -> HRESULT {
        let wrapper = &*this;

        eprintln!("[TiddlyDesktop] Windows Wrapper: Drop - forwarding to composition controller (HTML5 events will fire)");

        // Forward to WebView2's composition controller - this triggers HTML5 drop events!
        if let Some((controller, host_hwnd)) = wrapper.find_composition_controller_with_host() {
            // Convert screen coords to HOST window's client coords
            let pt_x = (pt & 0xFFFFFFFF) as i32;
            let pt_y = (pt >> 32) as i32;
            let mut point = windows::Win32::Foundation::POINT { x: pt_x, y: pt_y };
            let _ = windows::Win32::Graphics::Gdi::ScreenToClient(host_hwnd, &mut point);

            // Convert raw pointer to IDataObject
            let data_object: Option<IDataObject> = if data_obj.is_null() {
                None
            } else {
                Some(std::mem::transmute(data_obj))
            };

            let result = controller.Drop(
                data_object.as_ref(),
                key_state,
                point,
                effect as *mut u32,
            );

            // Don't drop the IDataObject - we don't own it
            if let Some(d) = data_object {
                std::mem::forget(d);
            }

            match result {
                Ok(()) => {
                    eprintln!("[TiddlyDesktop] Windows Wrapper: Drop forwarded successfully - HTML5 events should fire");
                    S_OK
                }
                Err(e) => {
                    eprintln!("[TiddlyDesktop] Windows Wrapper: Drop forward failed: {:?}", e);
                    e.code()
                }
            }
        } else {
            eprintln!("[TiddlyDesktop] Windows Wrapper: No composition controller found for Drop");
            S_OK
        }
    }
}
