//! Windows drag-drop handling via composition hosting
//!
//! This module uses WebView2 composition hosting mode for full drag-drop control:
//! 1. Registers IDropTarget on parent HWND (composition mode - we control it)
//! 2. Extracts file paths and emits Tauri events
//! 3. Forwards drag events to WebView2 via ICoreWebView2CompositionController3
//! 4. Provides DragStarting handler for cross-wiki drag detection
//! 5. Supports outgoing drags via OLE DoDragDrop

#![cfg(target_os = "windows")]

use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};
use std::sync::Mutex;

use tauri::{Emitter, Manager, WebviewWindow};

use windows::core::{implement, HRESULT, w};
use windows::Win32::Foundation::{HWND, POINTL, S_OK, POINT};
use windows::Win32::System::Com::{
    IDataObject, IDataObject_Impl, IEnumFORMATETC, DVASPECT_CONTENT, FORMATETC,
    STGMEDIUM, TYMED_HGLOBAL, IAdviseSink, IEnumSTATDATA,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{
    DoDragDrop, IDropSource, IDropSource_Impl, IDropTarget, IDropTarget_Impl,
    OleInitialize,
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::UI::Shell::DragQueryFileW;
use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
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
/// - WRY already registers IDropTarget via CompositionDragDropTarget
/// - WRY extracts file paths and calls drag_drop_handler → Tauri events
/// - We only need to register DragStarting handler for outgoing drag detection
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
            // Initialize OLE (required for DoDragDrop)
            let _ = OleInitialize(None);

            let controller = webview.controller();

            // Get the WRY container HWND (for logging)
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

            // Get composition controller for DragStarting handler
            // In composition hosting mode, the controller IS a composition controller
            let composition_controller3 = controller.cast::<ICoreWebView2CompositionController3>().ok();

            if let Some(comp_ctrl) = composition_controller3 {
                eprintln!("[TiddlyDesktop] Windows: Got ICoreWebView2CompositionController3 - composition mode active");

                // Register DragStarting handler for cross-wiki drag detection
                // NOTE: WRY already registers IDropTarget via CompositionDragDropTarget,
                // so we do NOT call register_drop_target_composition here.
                // File paths from external drops come through Tauri's onDragDropEvent.
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

/// Register IDropTarget on container HWND for composition hosting mode
/// NOTE: Not currently used - WRY's CompositionDragDropTarget handles this now
#[allow(dead_code)]
unsafe fn register_drop_target_composition(
    container_hwnd: HWND,
    composition_controller: ICoreWebView2CompositionController3,
    app: tauri::AppHandle,
) {
    use windows::Win32::System::Ole::RegisterDragDrop;

    eprintln!("[TiddlyDesktop] Windows: Registering IDropTarget on container HWND {:?} (composition mode)", container_hwnd);

    // Create our drop target that forwards to WebView2
    let drop_target = ForwardingDropTarget::new(container_hwnd, composition_controller, app);
    let drop_target_interface: IDropTarget = drop_target.into();

    // Register on the container HWND
    match RegisterDragDrop(container_hwnd, &drop_target_interface) {
        Ok(()) => {
            eprintln!("[TiddlyDesktop] Windows: Registered ForwardingDropTarget on container HWND");
            // Keep it alive
            std::mem::forget(drop_target_interface);
        }
        Err(e) => {
            eprintln!("[TiddlyDesktop] Windows: RegisterDragDrop on container HWND failed: {:?}", e);
        }
    }
}

/// Register IDropTarget for windowed mode (fallback - finds Chrome_WidgetWin_* windows)
/// NOTE: Not currently used - WRY's CompositionDragDropTarget handles this now
#[allow(dead_code)]
unsafe fn register_drop_target_windowed(
    parent_hwnd: HWND,
    composition_controller: ICoreWebView2CompositionController3,
    app: tauri::AppHandle,
) {
    use windows::Win32::Foundation::LPARAM;
    use windows::Win32::UI::WindowsAndMessaging::EnumChildWindows;
    use windows::Win32::System::Ole::{RegisterDragDrop, RevokeDragDrop};

    struct EnumContext {
        chrome_widget_0: Option<HWND>,
        chrome_widget_1: Option<HWND>,
    }

    unsafe extern "system" fn enum_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let ctx = &mut *(lparam.0 as *mut EnumContext);

        let mut class_name_buf = [0u16; 256];
        let len = GetClassNameW(hwnd, &mut class_name_buf);
        if len > 0 {
            let class_name = String::from_utf16_lossy(&class_name_buf[..len as usize]);
            if class_name == "Chrome_WidgetWin_0" && ctx.chrome_widget_0.is_none() {
                ctx.chrome_widget_0 = Some(hwnd);
            } else if class_name == "Chrome_WidgetWin_1" && ctx.chrome_widget_1.is_none() {
                ctx.chrome_widget_1 = Some(hwnd);
            }
        }
        BOOL::from(true)
    }

    let mut ctx = EnumContext { chrome_widget_0: None, chrome_widget_1: None };
    let _ = EnumChildWindows(Some(parent_hwnd), Some(enum_callback), LPARAM(&mut ctx as *mut _ as isize));

    // Also check children of Chrome_WidgetWin_0 for Chrome_WidgetWin_1
    if let Some(chrome_0) = ctx.chrome_widget_0 {
        let mut ctx2 = EnumContext { chrome_widget_0: None, chrome_widget_1: None };
        let _ = EnumChildWindows(Some(chrome_0), Some(enum_callback), LPARAM(&mut ctx2 as *mut _ as isize));
        if ctx.chrome_widget_1.is_none() {
            ctx.chrome_widget_1 = ctx2.chrome_widget_1;
        }
    }

    if let Some(chrome_0) = ctx.chrome_widget_0 {
        eprintln!("[TiddlyDesktop] Windows: Found Chrome_WidgetWin_0 at {:?}", chrome_0);
    }

    if let Some(chrome_1) = ctx.chrome_widget_1 {
        eprintln!("[TiddlyDesktop] Windows: Found Chrome_WidgetWin_1 at {:?}", chrome_1);

        // Revoke the browser process's drop target
        match RevokeDragDrop(chrome_1) {
            Ok(()) => eprintln!("[TiddlyDesktop] Windows: RevokeDragDrop succeeded on Chrome_WidgetWin_1"),
            Err(e) => eprintln!("[TiddlyDesktop] Windows: RevokeDragDrop failed: {:?}", e),
        }

        // Create our drop target that forwards to WebView2
        let drop_target = ForwardingDropTarget::new(chrome_1, composition_controller, app);
        let drop_target_interface: IDropTarget = drop_target.into();

        // Register it on Chrome_WidgetWin_1
        match RegisterDragDrop(chrome_1, &drop_target_interface) {
            Ok(()) => {
                eprintln!("[TiddlyDesktop] Windows: Registered ForwardingDropTarget on Chrome_WidgetWin_1");
                std::mem::forget(drop_target_interface);
            }
            Err(e) => {
                eprintln!("[TiddlyDesktop] Windows: RegisterDragDrop on Chrome_WidgetWin_1 failed: {:?}", e);
            }
        }
    } else {
        eprintln!("[TiddlyDesktop] Windows: Chrome_WidgetWin_1 not found - drag-drop may not work");
    }
}

// ============================================================================
// ForwardingDropTarget - extracts file paths and forwards to WebView2
// NOTE: Not currently used - WRY's CompositionDragDropTarget handles this now
// ============================================================================

#[allow(dead_code)]
#[implement(IDropTarget)]
struct ForwardingDropTarget {
    hwnd: HWND,
    composition_controller: ICoreWebView2CompositionController3,
    app: tauri::AppHandle,
    current_paths: std::cell::UnsafeCell<Vec<String>>,
}

unsafe impl Send for ForwardingDropTarget {}
unsafe impl Sync for ForwardingDropTarget {}

impl ForwardingDropTarget {
    fn new(hwnd: HWND, composition_controller: ICoreWebView2CompositionController3, app: tauri::AppHandle) -> Self {
        Self {
            hwnd,
            composition_controller,
            app,
            current_paths: std::cell::UnsafeCell::new(Vec::new()),
        }
    }

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

    unsafe fn to_client_coords(&self, pt: &POINTL) -> (i32, i32) {
        let mut client_pt = POINT { x: pt.x, y: pt.y };
        let _ = ScreenToClient(self.hwnd, &mut client_pt);
        (client_pt.x, client_pt.y)
    }
}

impl IDropTarget_Impl for ForwardingDropTarget_Impl {
    fn DragEnter(
        &self,
        pdataobj: Ref<'_, IDataObject>,
        grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows_core::Result<()> {
        eprintln!("[TiddlyDesktop] ForwardingDropTarget::DragEnter at ({}, {})", pt.x, pt.y);

        // Extract file paths
        if let Some(data_obj) = pdataobj.as_ref() {
            let paths = unsafe { ForwardingDropTarget::extract_file_paths(data_obj) };
            if !paths.is_empty() {
                eprintln!("[TiddlyDesktop] ForwardingDropTarget DragEnter: {} files", paths.len());
                unsafe { *self.current_paths.get() = paths.clone(); }

                let (x, y) = unsafe { self.to_client_coords(pt) };
                let _ = self.app.emit("tauri://drag-enter", serde_json::json!({
                    "paths": paths,
                    "position": { "x": x, "y": y }
                }));
            }
        }

        // Forward to WebView2
        unsafe {
            let point = POINT { x: pt.x, y: pt.y };
            if let Err(e) = self.composition_controller.DragEnter(pdataobj.as_ref(), grfkeystate.0, point, pdweffect as *mut u32) {
                eprintln!("[TiddlyDesktop] ForwardingDropTarget: DragEnter forward failed: {:?}", e);
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
        let _ = self.app.emit("tauri://drag-over", serde_json::json!({
            "position": { "x": x, "y": y }
        }));

        // Forward to WebView2
        unsafe {
            let point = POINT { x: pt.x, y: pt.y };
            if let Err(e) = self.composition_controller.DragOver(grfkeystate.0, point, pdweffect as *mut u32) {
                eprintln!("[TiddlyDesktop] ForwardingDropTarget: DragOver forward failed: {:?}", e);
            }
        }

        Ok(())
    }

    fn DragLeave(&self) -> windows_core::Result<()> {
        unsafe { (*self.current_paths.get()).clear(); }
        let _ = self.app.emit("tauri://drag-leave", serde_json::json!({}));

        // Forward to WebView2
        unsafe {
            if let Err(e) = self.composition_controller.DragLeave() {
                eprintln!("[TiddlyDesktop] ForwardingDropTarget: DragLeave forward failed: {:?}", e);
            }
        }

        Ok(())
    }

    fn Drop(
        &self,
        pdataobj: Ref<'_, IDataObject>,
        grfkeystate: MODIFIERKEYS_FLAGS,
        pt: &POINTL,
        pdweffect: *mut DROPEFFECT,
    ) -> windows_core::Result<()> {
        eprintln!("[TiddlyDesktop] ForwardingDropTarget::Drop at ({}, {})", pt.x, pt.y);

        // Get file paths
        let paths = if let Some(data_obj) = pdataobj.as_ref() {
            unsafe { ForwardingDropTarget::extract_file_paths(data_obj) }
        } else {
            unsafe { (*self.current_paths.get()).clone() }
        };

        if !paths.is_empty() {
            eprintln!("[TiddlyDesktop] ForwardingDropTarget Drop: {} files: {:?}", paths.len(), paths);

            let (x, y) = unsafe { self.to_client_coords(pt) };
            let _ = self.app.emit("tauri://drag-drop", serde_json::json!({
                "paths": paths,
                "position": { "x": x, "y": y }
            }));
        }

        unsafe { (*self.current_paths.get()).clear(); }

        // Forward to WebView2
        unsafe {
            let point = POINT { x: pt.x, y: pt.y };
            if let Err(e) = self.composition_controller.Drop(pdataobj.as_ref(), grfkeystate.0, point, pdweffect as *mut u32) {
                eprintln!("[TiddlyDesktop] ForwardingDropTarget: Drop forward failed: {:?}", e);
            }
        }

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
