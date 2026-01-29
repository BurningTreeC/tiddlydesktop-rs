//! Windows drag-drop handling
//!
//! Incoming drops: Handled natively by WebView2 (SetAllowExternalDrop=true).
//! JavaScript patches DataTransfer.getData/setData for custom format handling.
//! Cross-wiki drag data is shared via Tauri IPC.
//!
//! Outgoing drags: Implemented via OLE DoDragDrop with custom IDataObject.
//! This allows dragging tiddlers to external apps with proper MIME types.

#![cfg(target_os = "windows")]

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;

use tauri::{Emitter, WebviewWindow};

use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Controller4;
use windows::core::{GUID, HRESULT, BOOL};
use windows::Win32::Foundation::{HWND, LPARAM, E_NOINTERFACE, E_POINTER, S_OK};
use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL};
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{
    OleInitialize, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE,
};
use windows::Win32::UI::WindowsAndMessaging::{EnumChildWindows, GetClassNameW};

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
/// On Windows, we let WebView2 handle drops natively (no custom IDropTarget).
/// This ensures native DOM drop events fire correctly on all elements including inputs.
/// Custom format handling (text/vnd.tiddler etc.) is done via JavaScript DataTransfer
/// patching and Tauri IPC for cross-wiki drag data sharing.
///
/// This function also sets up a window event listener to inject file paths into
/// JavaScript synchronously when drag events occur, ensuring __pendingExternalFiles
/// is populated BEFORE the native WebView2 drop fires.
pub fn setup_drag_handlers(window: &WebviewWindow) {
    eprintln!(
        "[TiddlyDesktop] Windows: setup_drag_handlers called for window '{}'",
        window.label()
    );

    // Set up drag event listener to inject file paths synchronously
    // This runs BEFORE the native DOM drop event fires in WebView2
    let window_clone = window.clone();
    window.on_window_event(move |event| {
        use tauri::DragDropEvent;

        if let tauri::WindowEvent::DragDrop(drag_event) = event {
            let paths = match drag_event {
                DragDropEvent::Enter { paths, .. } => Some(paths),
                DragDropEvent::Drop { paths, .. } => Some(paths),
                _ => None,
            };

            if let Some(paths) = paths {
                if !paths.is_empty() {
                    inject_drag_paths(&window_clone, paths);
                }
            }
        }
    });

    let _ = window.with_webview(move |webview| {
        #[cfg(windows)]
        unsafe {
            use windows::core::Interface;

            let controller = webview.controller();

            // Ensure WebView2's native external drop is enabled (should be default, but be explicit)
            // This allows native DOM drop events to fire on all elements including inputs.
            if let Ok(controller4) = controller.cast::<ICoreWebView2Controller4>() {
                let result = controller4.SetAllowExternalDrop(true);
                eprintln!(
                    "[TiddlyDesktop] Windows: SetAllowExternalDrop(true) result: {:?}",
                    result
                );
            }

            // Initialize OLE for outgoing drags (DoDragDrop)
            match OleInitialize(None) {
                Ok(()) => eprintln!("[TiddlyDesktop] Windows: OleInitialize succeeded"),
                Err(e) => eprintln!("[TiddlyDesktop] Windows: OleInitialize: {:?}", e),
            }

            // No custom IDropTarget registration - let WebView2 handle drops natively.
            // JavaScript patches DataTransfer.getData/setData to handle custom formats.
            eprintln!("[TiddlyDesktop] Windows: Native drop handling enabled (no custom IDropTarget)");
        }
    });
}

/// Inject file paths into JavaScript's window.__pendingExternalFiles
/// This is called synchronously when drag events occur, ensuring paths are
/// available before the native DOM drop event fires.
fn inject_drag_paths(window: &WebviewWindow, paths: &[std::path::PathBuf]) {
    if paths.is_empty() {
        return;
    }

    // Build JavaScript to inject paths into __pendingExternalFiles
    let mut js_parts = Vec::new();
    js_parts.push("(function() { window.__pendingExternalFiles = window.__pendingExternalFiles || {};".to_string());

    for path in paths {
        let path_str = path.to_string_lossy();
        // Skip non-file paths (data: URIs, etc.)
        if path_str.starts_with("data:") {
            continue;
        }

        // Extract filename from path
        if let Some(filename) = path.file_name().and_then(|n| n.to_str()) {
            // Escape for JavaScript string
            let escaped_path = path_str.replace('\\', "\\\\").replace('\'', "\\'");
            let escaped_filename = filename.replace('\\', "\\\\").replace('\'', "\\'");
            js_parts.push(format!(
                "window.__pendingExternalFiles['{}'] = '{}';",
                escaped_filename, escaped_path
            ));
        }
    }

    js_parts.push("})();".to_string());
    let js = js_parts.join(" ");

    // Execute synchronously - this runs before the native drop event fires
    if let Err(e) = window.eval(&js) {
        eprintln!("[TiddlyDesktop] Windows: Failed to inject drag paths: {}", e);
    } else {
        eprintln!("[TiddlyDesktop] Windows: Injected {} file paths into __pendingExternalFiles", paths.len());
    }
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
