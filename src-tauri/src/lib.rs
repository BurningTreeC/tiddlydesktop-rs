use std::{collections::HashMap, path::PathBuf, process::{Child, Command}, sync::Mutex};
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt as UnixCommandExt;

/// Windows flag to prevent console window from appearing
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Windows Job Object for killing child processes when parent dies
#[cfg(target_os = "windows")]
mod windows_job {
    use std::ptr;
    use std::sync::OnceLock;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateJobObjectW(lpJobAttributes: *mut std::ffi::c_void, lpName: *const u16) -> *mut std::ffi::c_void;
        fn SetInformationJobObject(hJob: *mut std::ffi::c_void, JobObjectInformationClass: u32, lpJobObjectInformation: *const std::ffi::c_void, cbJobObjectInformationLength: u32) -> i32;
        fn AssignProcessToJobObject(hJob: *mut std::ffi::c_void, hProcess: *mut std::ffi::c_void) -> i32;
        fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: i32, dwProcessId: u32) -> *mut std::ffi::c_void;
        fn CloseHandle(hObject: *mut std::ffi::c_void) -> i32;
    }

    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
    const JOBOBJECT_EXTENDED_LIMIT_INFORMATION: u32 = 9;
    const PROCESS_ALL_ACCESS: u32 = 0x1F0FFF;

    #[repr(C)]
    struct JOBOBJECT_BASIC_LIMIT_INFORMATION {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[repr(C)]
    struct IO_COUNTERS {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    struct JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
        basic_limit_information: JOBOBJECT_BASIC_LIMIT_INFORMATION,
        io_info: IO_COUNTERS,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    // Wrapper to make the handle Send+Sync (safe because Job Objects are thread-safe Windows handles)
    struct JobHandle(*mut std::ffi::c_void);
    unsafe impl Send for JobHandle {}
    unsafe impl Sync for JobHandle {}

    static JOB_HANDLE: OnceLock<JobHandle> = OnceLock::new();

    pub fn get_job_handle() -> *mut std::ffi::c_void {
        JOB_HANDLE.get_or_init(|| {
            unsafe {
                let job = CreateJobObjectW(ptr::null_mut(), ptr::null());
                if job.is_null() {
                    return JobHandle(ptr::null_mut());
                }

                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

                SetInformationJobObject(
                    job,
                    JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                    &info as *const _ as *const std::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );

                JobHandle(job)
            }
        }).0
    }

    pub fn assign_process_to_job(pid: u32) {
        let job = get_job_handle();
        if job.is_null() {
            return;
        }

        unsafe {
            let process = OpenProcess(PROCESS_ALL_ACCESS, 0, pid);
            if !process.is_null() {
                AssignProcessToJobObject(job, process);
                CloseHandle(process);
            }
        }
    }
}

/// Linux drag-drop handling - captures content from external drags
#[cfg(target_os = "linux")]
mod linux_drag {
    use gtk::prelude::*;
    use gdk::DragAction;
    use glib;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;
    use tauri::{Emitter, WebviewWindow};

    /// Data captured from a drag operation
    #[derive(Clone, Debug, serde::Serialize)]
    pub struct DragContentData {
        pub types: Vec<String>,
        pub data: HashMap<String, String>,
    }

    /// State for multi-format drag data collection
    /// GTK only allows requesting one format at a time, so we need to track state
    struct DragState {
        pending_targets: Vec<gdk::Atom>,
        collected_data: HashMap<String, String>,
        drop_position: (i32, i32),
    }

    /// Set up drag-drop handling for a webview window
    /// This schedules the GTK setup on the main thread since GTK is not thread-safe
    pub fn setup_drag_handlers(window: &WebviewWindow) {
        let window_clone = window.clone();

        // Schedule GTK work on the main thread
        glib::MainContext::default().invoke(move || {
            setup_drag_handlers_impl(&window_clone);
        });
    }

    /// Internal implementation that must run on the main GTK thread
    fn setup_drag_handlers_impl(window: &WebviewWindow) {
        // gtk_window() is available directly on WebviewWindow on Linux
        let gtk_window = match window.gtk_window() {
            Ok(w) => w,
            Err(e) => {
                eprintln!("[TiddlyDesktop] Failed to get GTK window: {}", e);
                return;
            }
        };

        // We need to find the WebKitWebView widget inside the GTK window
        // It's usually nested inside containers
        let webview = find_webview_widget(&gtk_window);
        if webview.is_none() {
            eprintln!("[TiddlyDesktop] Could not find WebKitWebView widget");
            return;
        }
        let webview = webview.unwrap();

        // Set up drag destination with content types matching TiddlyWiki5's importDataTypes
        // Order matches TW5 priority: text/vnd.tiddler, URL, text/x-moz-url, text/html, text/plain, Text, text/uri-list
        let targets = vec![
            gtk::TargetEntry::new("text/vnd.tiddler", gtk::TargetFlags::OTHER_APP, 0),
            gtk::TargetEntry::new("URL", gtk::TargetFlags::OTHER_APP, 1),
            gtk::TargetEntry::new("text/x-moz-url", gtk::TargetFlags::OTHER_APP, 2),
            gtk::TargetEntry::new("text/html", gtk::TargetFlags::OTHER_APP, 3),
            gtk::TargetEntry::new("text/plain", gtk::TargetFlags::OTHER_APP, 4),
            gtk::TargetEntry::new("Text", gtk::TargetFlags::OTHER_APP, 5),
            gtk::TargetEntry::new("text/uri-list", gtk::TargetFlags::OTHER_APP, 6),
            // Additional X11/GTK-specific formats for encoding compatibility
            gtk::TargetEntry::new("UTF8_STRING", gtk::TargetFlags::OTHER_APP, 7),
            gtk::TargetEntry::new("STRING", gtk::TargetFlags::OTHER_APP, 8),
            gtk::TargetEntry::new("TEXT", gtk::TargetFlags::OTHER_APP, 9),
            // Chrome-specific format that may contain custom MIME data
            gtk::TargetEntry::new("chromium/x-web-custom-data", gtk::TargetFlags::OTHER_APP, 10),
        ];

        webview.drag_dest_set(
            gtk::DestDefaults::empty(), // We handle everything manually
            &targets,
            DragAction::COPY | DragAction::MOVE | DragAction::LINK,
        );

        // Clone window for closures
        let window_clone = window.clone();

        // Handle drag-motion - emit position so JS can show dropzone highlights
        let window_for_motion = window_clone.clone();
        webview.connect_drag_motion(move |_widget, context, x, y, time| {
            // Accept the drag by setting the suggested action
            context.drag_status(DragAction::COPY, time);

            // Emit motion event so JavaScript can dispatch synthetic dragenter/dragover
            let _ = window_for_motion.emit("td-drag-motion", serde_json::json!({
                "x": x,
                "y": y
            }));

            true // We can accept this drag
        });

        // Handle drag-leave at GTK level
        let window_for_leave = window_clone.clone();
        webview.connect_drag_leave(move |_widget, _context, _time| {
            let _ = window_for_leave.emit("td-drag-leave", ());
        });

        // Shared state for collecting data from multiple formats
        let drag_state: Rc<RefCell<Option<DragState>>> = Rc::new(RefCell::new(None));

        // Handle drag-drop at GTK level - request ALL relevant formats
        let window_for_drop = window_clone.clone();
        let drag_state_for_drop = drag_state.clone();
        webview.connect_drag_drop(move |widget, context, x, y, time| {
            // Emit drop-start immediately so JS knows a drop is in progress
            let _ = window_for_drop.emit("td-drag-drop-start", serde_json::json!({
                "x": x,
                "y": y
            }));

            let available_targets = context.list_targets();

            // Debug: log all available targets
            eprintln!("[TiddlyDesktop] Linux: Available drag targets:");
            for target in &available_targets {
                eprintln!("[TiddlyDesktop]   - {}", target.name());
            }

            // Formats we want to collect (in TW5 priority order)
            // We'll request ALL of these that are available, then let JS pick the best
            let wanted_formats = [
                "text/vnd.tiddler",
                "URL",
                "text/x-moz-url",
                "text/html",
                "text/plain",
                "Text",
                "text/uri-list",
                "UTF8_STRING",
            ];

            // Find which wanted formats are available
            let mut targets_to_request: Vec<gdk::Atom> = Vec::new();
            for wanted in &wanted_formats {
                for target in &available_targets {
                    if target.name() == *wanted {
                        targets_to_request.push(target.clone());
                        break;
                    }
                }
            }

            // If none of our preferred formats, try any non-DELETE format
            if targets_to_request.is_empty() {
                for target in &available_targets {
                    if target.name() != "DELETE" {
                        targets_to_request.push(target.clone());
                        break;
                    }
                }
            }

            if !targets_to_request.is_empty() {
                eprintln!("[TiddlyDesktop] Linux: Will request {} formats:", targets_to_request.len());
                for t in &targets_to_request {
                    eprintln!("[TiddlyDesktop]   - {}", t.name());
                }

                // Store state for the data received handler
                *drag_state_for_drop.borrow_mut() = Some(DragState {
                    pending_targets: targets_to_request.clone(),
                    collected_data: HashMap::new(),
                    drop_position: (x, y),
                });

                // Request the first format
                widget.drag_get_data(context, &targets_to_request[0], time);
            }

            true // We handled it
        });

        // Handle receiving the actual data - collects multiple formats
        let window_for_data = window_clone.clone();
        let drag_state_for_data = drag_state.clone();
        webview.connect_drag_data_received(move |widget, context, _x, _y, selection_data, _info, time| {
            // Get the data type
            let data_type = selection_data.data_type();
            let type_name = data_type.name().to_string();

            // Helper to decode text with proper encoding detection
            fn decode_text(raw_data: &[u8]) -> Option<String> {
                if raw_data.is_empty() {
                    return None;
                }

                // Check for BOM (Byte Order Mark)
                if raw_data.len() >= 3 && raw_data[0] == 0xEF && raw_data[1] == 0xBB && raw_data[2] == 0xBF {
                    return String::from_utf8(raw_data[3..].to_vec()).ok();
                }
                if raw_data.len() >= 2 && raw_data[0] == 0xFF && raw_data[1] == 0xFE {
                    if raw_data.len() % 2 == 0 {
                        let u16_data: Vec<u16> = raw_data[2..]
                            .chunks_exact(2)
                            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                            .collect();
                        return String::from_utf16(&u16_data).ok();
                    }
                }
                if raw_data.len() >= 2 && raw_data[0] == 0xFE && raw_data[1] == 0xFF {
                    if raw_data.len() % 2 == 0 {
                        let u16_data: Vec<u16> = raw_data[2..]
                            .chunks_exact(2)
                            .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
                            .collect();
                        return String::from_utf16(&u16_data).ok();
                    }
                }

                // Check for UTF-16LE/BE pattern BEFORE trying UTF-8
                // (UTF-8 will "succeed" with embedded nulls but produce garbage)
                if raw_data.len() >= 4 && raw_data.len() % 2 == 0 {
                    let looks_like_utf16le = raw_data[1] == 0 && raw_data[3] == 0
                        && raw_data[0] != 0 && raw_data[2] != 0;
                    if looks_like_utf16le {
                        eprintln!("[TiddlyDesktop] Linux: Detected UTF-16LE encoding");
                        let u16_data: Vec<u16> = raw_data
                            .chunks_exact(2)
                            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                            .collect();
                        if let Ok(s) = String::from_utf16(&u16_data) {
                            return Some(s);
                        }
                    }

                    let looks_like_utf16be = raw_data[0] == 0 && raw_data[2] == 0
                        && raw_data[1] != 0 && raw_data[3] != 0;
                    if looks_like_utf16be {
                        eprintln!("[TiddlyDesktop] Linux: Detected UTF-16BE encoding");
                        let u16_data: Vec<u16> = raw_data
                            .chunks_exact(2)
                            .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
                            .collect();
                        if let Ok(s) = String::from_utf16(&u16_data) {
                            return Some(s);
                        }
                    }
                }

                // Try UTF-8
                if let Ok(s) = String::from_utf8(raw_data.to_vec()) {
                    return Some(s);
                }

                None
            }

            // Decode the received data
            // Note: selection_data.text() may misinterpret UTF-16LE as UTF-8, embedding null bytes
            // So we check for null bytes and use decode_text which properly handles UTF-16
            let text_data = selection_data.text().map(|s| s.to_string()).and_then(|s| {
                // If the string contains null bytes, it's likely misinterpreted UTF-16
                if s.contains('\0') {
                    eprintln!("[TiddlyDesktop] Linux: text() returned string with null bytes, trying decode_text");
                    decode_text(&selection_data.data())
                } else {
                    Some(s)
                }
            }).or_else(|| {
                decode_text(&selection_data.data())
            });

            // Get mutable access to drag state
            let mut state_borrow = drag_state_for_data.borrow_mut();

            if let Some(ref mut state) = *state_borrow {
                // Store the received data
                if let Some(text) = text_data {
                    // For text/x-moz-url, extract just the URL (first line)
                    let value = if type_name == "text/x-moz-url" {
                        text.lines().next().unwrap_or(&text).to_string()
                    } else {
                        text
                    };
                    let preview = if value.len() > 100 { &value[..100] } else { &value[..] };
                    eprintln!("[TiddlyDesktop] Linux: Received {}: {}...", type_name, preview);
                    state.collected_data.insert(type_name.clone(), value);
                } else {
                    eprintln!("[TiddlyDesktop] Linux: Received {} but no text data", type_name);
                }

                // Remove this target from pending
                state.pending_targets.retain(|t| t.name() != type_name);

                // If more targets to request, request the next one
                if !state.pending_targets.is_empty() {
                    let next_target = state.pending_targets[0].clone();
                    drop(state_borrow); // Release borrow before calling drag_get_data
                    widget.drag_get_data(context, &next_target, time);
                    return;
                }

                // All targets received - emit the collected data
                let (x, y) = state.drop_position;
                let collected = std::mem::take(&mut state.collected_data);
                drop(state_borrow);

                // Check for file drops in text/uri-list
                if let Some(uri_list) = collected.get("text/uri-list") {
                    let file_paths: Vec<String> = uri_list.lines()
                        .filter(|line| !line.starts_with('#') && !line.is_empty())
                        .filter_map(|line| {
                            let trimmed = line.trim();
                            if trimmed.starts_with("file://") {
                                let path = trimmed.strip_prefix("file://").unwrap_or(trimmed);
                                urlencoding::decode(path).ok().map(|s| s.into_owned())
                            } else {
                                None
                            }
                        })
                        .collect();

                    if !file_paths.is_empty() {
                        // File drop
                        let _ = window_for_data.emit("td-drag-drop-position", serde_json::json!({
                            "x": x, "y": y
                        }));
                        let _ = window_for_data.emit("td-file-drop", serde_json::json!({
                            "paths": file_paths
                        }));
                        context.drag_finish(true, false, time);
                        return;
                    }
                }

                // Content drop - emit ALL collected data, let JS pick the best format
                if !collected.is_empty() {
                    let types: Vec<String> = collected.keys().cloned().collect();
                    let window_label = window_for_data.label();
                    eprintln!("[TiddlyDesktop] Linux: Emitting td-drag-content to window '{}' with types: {:?}", window_label, types);
                    let drag_data = DragContentData { types, data: collected };

                    match window_for_data.emit("td-drag-drop-position", serde_json::json!({
                        "x": x, "y": y
                    })) {
                        Ok(_) => eprintln!("[TiddlyDesktop] Linux: td-drag-drop-position emitted OK to '{}'", window_label),
                        Err(e) => eprintln!("[TiddlyDesktop] Linux: td-drag-drop-position emit FAILED: {:?}", e),
                    }

                    match window_for_data.emit("td-drag-content", &drag_data) {
                        Ok(_) => eprintln!("[TiddlyDesktop] Linux: td-drag-content emitted OK to '{}'", window_label),
                        Err(e) => eprintln!("[TiddlyDesktop] Linux: td-drag-content emit FAILED: {:?}", e),
                    }
                }

                context.drag_finish(true, false, time);
            } else {
                // No state - shouldn't happen, but handle gracefully
                context.drag_finish(false, false, time);
            }
        });
    }

    /// Find the WebKitWebView widget inside a GTK ApplicationWindow
    fn find_webview_widget(window: &gtk::ApplicationWindow) -> Option<gtk::Widget> {
        // The webview is usually inside a container hierarchy
        // Let's traverse the widget tree to find it
        fn find_webkit_recursive(widget: &gtk::Widget) -> Option<gtk::Widget> {
            // Check if this widget's type name contains "WebKit"
            let type_name = widget.type_().name();
            if type_name.contains("WebKit") || type_name.contains("webview") {
                return Some(widget.clone());
            }

            // If it's a container, check children
            if let Some(container) = widget.downcast_ref::<gtk::Container>() {
                for child in container.children() {
                    if let Some(found) = find_webkit_recursive(&child) {
                        return Some(found);
                    }
                }
            }

            None
        }

        if let Some(child) = window.child() {
            find_webkit_recursive(&child)
        } else {
            None
        }
    }
}

/// Windows drag-drop handling using WebView2's native APIs
///
/// This approach works WITH WebView2 rather than against it, similar to how
/// the Linux implementation works with GTK's drag_dest_set.
///
/// On Windows:
/// 1. We use Tauri's with_webview to access the WebView2 controller
/// 2. We DISABLE AllowExternalDrop so WebView2 doesn't intercept external drags
/// 3. We register an IDropTarget on the PARENT window to handle all external drags
/// 4. We emit events to JavaScript which handles the actual drop processing
///
/// The key insight is that WebView2's internal drag handling intercepts drags
/// before they reach our IDropTarget on the parent window. By disabling
/// AllowExternalDrop, we force WebView2 to ignore external drags, allowing
/// them to be handled by our IDropTarget instead.
#[cfg(target_os = "windows")]
mod windows_drag {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tauri::{Emitter, WebviewWindow};
    use windows::core::{GUID, HRESULT, BOOL};
    use windows::Win32::Foundation::{HWND, LPARAM, POINTL, S_OK, E_NOINTERFACE, E_POINTER};
    use windows::Win32::System::Com::{
        CoInitializeEx, IDataObject, COINIT_APARTMENTTHREADED, TYMED_HGLOBAL,
        FORMATETC, DVASPECT_CONTENT,
    };
    use windows::Win32::System::Ole::{
        IDropTarget, RegisterDragDrop, RevokeDragDrop,
        DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_MOVE, DROPEFFECT_LINK, DROPEFFECT_NONE,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock, GlobalSize};
    use windows::Win32::Globalization::{MultiByteToWideChar, CP_ACP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS};
    use windows::Win32::UI::Shell::DragQueryFileW;
    use windows::Win32::UI::Shell::HDROP;
    use windows::Win32::UI::WindowsAndMessaging::{EnumChildWindows, GetClassNameW};
    use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2Controller4;

    /// Thread-safe wrapper for our drop target (stored for cleanup, pointer kept alive)
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
        use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
        use windows::core::w;
        unsafe { RegisterClipboardFormatW(w!("HTML Format")) as u16 }
    }

    /// Custom clipboard format for URI list
    fn get_cf_uri_list() -> u16 {
        use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
        use windows::core::w;
        unsafe { RegisterClipboardFormatW(w!("text/uri-list")) as u16 }
    }

    /// Custom clipboard format for TiddlyWiki tiddler
    fn get_cf_tiddler() -> u16 {
        use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
        use windows::core::w;
        unsafe { RegisterClipboardFormatW(w!("text/vnd.tiddler")) as u16 }
    }

    /// Standard Windows clipboard format for URLs (ANSI) - used by browsers
    fn get_cf_url() -> u16 {
        use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
        use windows::core::w;
        unsafe { RegisterClipboardFormatW(w!("UniformResourceLocator")) as u16 }
    }

    /// Standard Windows clipboard format for URLs (Unicode) - used by browsers
    fn get_cf_url_w() -> u16 {
        use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
        use windows::core::w;
        unsafe { RegisterClipboardFormatW(w!("UniformResourceLocatorW")) as u16 }
    }

    /// Mozilla URL format - contains data URI with tiddler content
    fn get_cf_moz_url() -> u16 {
        use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
        use windows::core::w;
        unsafe { RegisterClipboardFormatW(w!("text/x-moz-url")) as u16 }
    }

    /// Data captured from a drag operation
    #[derive(Clone, Debug, serde::Serialize)]
    pub struct DragContentData {
        pub types: Vec<String>,
        pub data: HashMap<String, String>,
    }

    /// IDropTarget vtable - must match COM layout exactly
    /// Field names use PascalCase to match Windows COM conventions
    #[repr(C)]
    #[allow(non_snake_case)]
    struct IDropTargetVtbl {
        // IUnknown methods
        QueryInterface: unsafe extern "system" fn(
            this: *mut DropTargetImpl,
            riid: *const GUID,
            ppv_object: *mut *mut std::ffi::c_void,
        ) -> HRESULT,
        AddRef: unsafe extern "system" fn(this: *mut DropTargetImpl) -> u32,
        Release: unsafe extern "system" fn(this: *mut DropTargetImpl) -> u32,
        // IDropTarget methods
        DragEnter: unsafe extern "system" fn(
            this: *mut DropTargetImpl,
            pDataObj: *mut std::ffi::c_void, // IDataObject*
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
            pDataObj: *mut std::ffi::c_void, // IDataObject*
            grfKeyState: u32,
            pt: POINTL,
            pdwEffect: *mut u32,
        ) -> HRESULT,
    }

    /// Our IDropTarget implementation struct
    /// The vtable pointer MUST be the first field for COM compatibility
    #[repr(C)]
    struct DropTargetImpl {
        vtbl: *const IDropTargetVtbl,
        ref_count: AtomicU32,
        window: WebviewWindow,
        drag_active: Mutex<bool>,
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
        /// Create a new DropTarget and return a raw pointer (caller owns the reference)
        fn new(window: WebviewWindow) -> *mut Self {
            let obj = Box::new(Self {
                vtbl: &DROPTARGET_VTBL,
                ref_count: AtomicU32::new(1),
                window,
                drag_active: Mutex::new(false),
            });
            Box::into_raw(obj)
        }

        /// Convert to IDropTarget interface
        unsafe fn as_idroptarget(ptr: *mut Self) -> IDropTarget {
            std::mem::transmute(ptr)
        }

        // IUnknown::QueryInterface
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

        // IUnknown::AddRef
        unsafe extern "system" fn add_ref(this: *mut Self) -> u32 {
            let obj = &*this;
            obj.ref_count.fetch_add(1, Ordering::SeqCst) + 1
        }

        // IUnknown::Release
        unsafe extern "system" fn release(this: *mut Self) -> u32 {
            let obj = &*this;
            let count = obj.ref_count.fetch_sub(1, Ordering::SeqCst) - 1;
            if count == 0 {
                // Drop the box
                drop(Box::from_raw(this));
            }
            count
        }

        // IDropTarget::DragEnter
        unsafe extern "system" fn drag_enter(
            this: *mut Self,
            p_data_obj: *mut std::ffi::c_void,
            _grf_key_state: u32,
            pt: POINTL,
            pdw_effect: *mut u32,
        ) -> HRESULT {
            let obj = &*this;
            *obj.drag_active.lock().unwrap() = true;

            eprintln!("[TiddlyDesktop] Windows IDropTarget::DragEnter called at ({}, {})", pt.x, pt.y);

            // Log available clipboard formats for debugging
            if !p_data_obj.is_null() {
                let data_object: &IDataObject = std::mem::transmute(&p_data_obj);
                obj.log_available_formats(data_object);
            }

            // Convert to client coordinates
            let (x, y) = obj.screen_to_client(pt.x, pt.y);
            eprintln!("[TiddlyDesktop] Windows IDropTarget::DragEnter client coords: ({}, {})", x, y);

            // Emit event with client coordinates
            let _ = obj.window.emit("td-drag-motion", serde_json::json!({
                "x": x,
                "y": y
            }));

            // Accept the drag
            if !pdw_effect.is_null() {
                let allowed = DROPEFFECT(*pdw_effect);
                *pdw_effect = choose_drop_effect(allowed).0 as u32;
            }

            S_OK
        }

        // IDropTarget::DragOver
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

            // Convert to client coordinates
            let (x, y) = obj.screen_to_client(pt.x, pt.y);

            // Emit continuous motion events
            let _ = obj.window.emit("td-drag-motion", serde_json::json!({
                "x": x,
                "y": y
            }));

            // Accept the drag
            if !pdw_effect.is_null() {
                let allowed = DROPEFFECT(*pdw_effect);
                *pdw_effect = choose_drop_effect(allowed).0 as u32;
            }

            S_OK
        }

        // IDropTarget::DragLeave
        unsafe extern "system" fn drag_leave(this: *mut Self) -> HRESULT {
            let obj = &*this;
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

        // IDropTarget::Drop
        unsafe extern "system" fn drop_impl(
            this: *mut Self,
            p_data_obj: *mut std::ffi::c_void,
            _grf_key_state: u32,
            pt: POINTL,
            pdw_effect: *mut u32,
        ) -> HRESULT {
            let obj = &*this;
            *obj.drag_active.lock().unwrap() = false;

            eprintln!("[TiddlyDesktop] Windows IDropTarget::Drop called at ({}, {})", pt.x, pt.y);

            if !p_data_obj.is_null() {
                // Convert raw pointer to IDataObject reference
                let data_object: &IDataObject = std::mem::transmute(&p_data_obj);

                // Log available formats for debugging
                obj.log_available_formats(data_object);

                // Convert to client coordinates
                let (x, y) = obj.screen_to_client(pt.x, pt.y);
                eprintln!("[TiddlyDesktop] Windows IDropTarget::Drop client coords: ({}, {})", x, y);

                // Emit drop start
                let _ = obj.window.emit("td-drag-drop-start", serde_json::json!({
                    "x": x,
                    "y": y
                }));

                // Check for file paths first
                let file_paths = obj.get_file_paths(data_object);
                eprintln!("[TiddlyDesktop] Windows IDropTarget::Drop file_paths count: {}", file_paths.len());
                if !file_paths.is_empty() {
                    eprintln!("[TiddlyDesktop] Windows IDropTarget::Drop emitting td-file-drop");
                    let _ = obj.window.emit("td-drag-drop-position", serde_json::json!({
                        "x": x,
                        "y": y
                    }));
                    let _ = obj.window.emit("td-file-drop", serde_json::json!({
                        "paths": file_paths
                    }));
                    if !pdw_effect.is_null() {
                        *pdw_effect = DROPEFFECT_COPY.0 as u32;
                    }
                    return S_OK;
                }

                // Content drop - extract and emit data
                eprintln!("[TiddlyDesktop] Windows IDropTarget::Drop attempting content extraction");
                if let Some(content_data) = obj.extract_data(data_object) {
                    eprintln!("[TiddlyDesktop] Windows IDropTarget::Drop extracted {} types: {:?}",
                             content_data.types.len(), content_data.types);
                    let _ = obj.window.emit("td-drag-drop-position", serde_json::json!({
                        "x": x,
                        "y": y
                    }));
                    let _ = obj.window.emit("td-drag-content", &content_data);
                    eprintln!("[TiddlyDesktop] Windows IDropTarget::Drop emitted td-drag-content");
                    if !pdw_effect.is_null() {
                        *pdw_effect = DROPEFFECT_COPY.0 as u32;
                    }
                } else {
                    eprintln!("[TiddlyDesktop] Windows IDropTarget::Drop no content extracted!");
                    if !pdw_effect.is_null() {
                        *pdw_effect = DROPEFFECT_NONE.0 as u32;
                    }
                }
            } else {
                eprintln!("[TiddlyDesktop] Windows IDropTarget::Drop - p_data_obj is null!");
                if !pdw_effect.is_null() {
                    *pdw_effect = DROPEFFECT_NONE.0 as u32;
                }
            }

            S_OK
        }

        /// Log available clipboard formats from the IDataObject for debugging
        fn log_available_formats(&self, data_object: &IDataObject) {
            use windows::Win32::System::DataExchange::GetClipboardFormatNameW;

            unsafe {
                eprintln!("[TiddlyDesktop] Windows: Enumerating available clipboard formats...");
                if let Ok(enum_fmt) = data_object.EnumFormatEtc(1) { // 1 = DATADIR_GET
                    let mut formats: [FORMATETC; 1] = [std::mem::zeroed()];
                    let mut fetched: u32 = 0;
                    let mut count = 0;
                    while enum_fmt.Next(&mut formats, Some(&mut fetched)).is_ok() && fetched > 0 {
                        let cf = formats[0].cfFormat;
                        // Get format name
                        let mut name_buf = [0u16; 256];
                        let name_len = GetClipboardFormatNameW(cf as u32, &mut name_buf);
                        let format_name = if name_len > 0 {
                            OsString::from_wide(&name_buf[..name_len as usize]).to_string_lossy().to_string()
                        } else {
                            // Standard formats don't have registered names
                            match cf {
                                1 => "CF_TEXT".to_string(),
                                2 => "CF_BITMAP".to_string(),
                                7 => "CF_OEMTEXT".to_string(),
                                8 => "CF_DIB".to_string(),
                                13 => "CF_UNICODETEXT".to_string(),
                                15 => "CF_HDROP".to_string(),
                                16 => "CF_LOCALE".to_string(),
                                17 => "CF_DIBV5".to_string(),
                                _ => format!("Unknown({})", cf),
                            }
                        };
                        eprintln!("[TiddlyDesktop] Windows:   Format {}: {} (cfFormat={})", count, format_name, cf);
                        count += 1;
                        fetched = 0;
                    }
                    eprintln!("[TiddlyDesktop] Windows: Total {} formats available", count);
                } else {
                    eprintln!("[TiddlyDesktop] Windows: Failed to enumerate formats");
                }
            }
        }

        // Helper methods - extract data following TiddlyWiki5's importDataTypes priority:
        // 1. text/vnd.tiddler  2. URL  3. text/x-moz-url  4. text/html  5. text/plain  6. Text  7. text/uri-list
        fn extract_data(&self, data_object: &IDataObject) -> Option<DragContentData> {
            let mut types = Vec::new();
            let mut data = HashMap::new();

            eprintln!("[TiddlyDesktop] Windows extract_data: starting extraction...");

            // 1. text/vnd.tiddler - Primary TW format (JSON tiddler data)
            let cf_tiddler = get_cf_tiddler();
            eprintln!("[TiddlyDesktop] Windows extract_data: trying text/vnd.tiddler (cf={})", cf_tiddler);
            if let Some(tiddler) = self.get_string_data(data_object, cf_tiddler) {
                eprintln!("[TiddlyDesktop] Windows extract_data: got text/vnd.tiddler ({} bytes)", tiddler.len());
                types.push("text/vnd.tiddler".to_string());
                data.insert("text/vnd.tiddler".to_string(), tiddler);
            }

            // 2. URL - Windows UniformResourceLocator (may contain data URI)
            let cf_url_w = get_cf_url_w();
            let cf_url = get_cf_url();
            eprintln!("[TiddlyDesktop] Windows extract_data: trying URL formats (cf_url_w={}, cf_url={})", cf_url_w, cf_url);
            if let Some(url) = self.get_unicode_url(data_object, cf_url_w) {
                eprintln!("[TiddlyDesktop] Windows extract_data: got UniformResourceLocatorW ({} bytes)", url.len());
                types.push("URL".to_string());
                data.insert("URL".to_string(), url);
            } else if let Some(url) = self.get_string_data(data_object, cf_url) {
                eprintln!("[TiddlyDesktop] Windows extract_data: got UniformResourceLocator ({} bytes)", url.len());
                types.push("URL".to_string());
                data.insert("URL".to_string(), url);
            }

            // 3. text/x-moz-url - Mozilla format (UTF-16, URL on first line, title on second)
            let cf_moz_url = get_cf_moz_url();
            eprintln!("[TiddlyDesktop] Windows extract_data: trying text/x-moz-url (cf={})", cf_moz_url);
            if let Some(moz_url) = self.get_unicode_text_format(data_object, cf_moz_url) {
                // Extract just the URL (first line)
                let url = moz_url.lines().next().unwrap_or(&moz_url);
                eprintln!("[TiddlyDesktop] Windows extract_data: got text/x-moz-url ({} bytes)", url.len());
                types.push("text/x-moz-url".to_string());
                data.insert("text/x-moz-url".to_string(), url.to_string());
            }

            // 4. text/html - HTML content
            let cf_html = get_cf_html();
            eprintln!("[TiddlyDesktop] Windows extract_data: trying HTML Format (cf={})", cf_html);
            if let Some(html) = self.get_string_data(data_object, cf_html) {
                eprintln!("[TiddlyDesktop] Windows extract_data: got HTML Format ({} bytes)", html.len());
                // Windows HTML Format has markers we need to extract content from
                if let Some(start) = html.find("<!--StartFragment-->") {
                    if let Some(end) = html.find("<!--EndFragment-->") {
                        let content = &html[start + 20..end];
                        eprintln!("[TiddlyDesktop] Windows extract_data: extracted fragment ({} bytes)", content.len());
                        types.push("text/html".to_string());
                        data.insert("text/html".to_string(), content.to_string());
                    }
                } else {
                    types.push("text/html".to_string());
                    data.insert("text/html".to_string(), html);
                }
            }

            // 5. text/plain - Unicode text (CF_UNICODETEXT)
            eprintln!("[TiddlyDesktop] Windows extract_data: trying CF_UNICODETEXT");
            if let Some(text) = self.get_unicode_text(data_object) {
                eprintln!("[TiddlyDesktop] Windows extract_data: got CF_UNICODETEXT ({} bytes)", text.len());
                types.push("text/plain".to_string());
                data.insert("text/plain".to_string(), text);
            }

            // 6. Text - ANSI text (CF_TEXT) - IE compatible fallback
            eprintln!("[TiddlyDesktop] Windows extract_data: trying CF_TEXT");
            if let Some(text) = self.get_ansi_text(data_object) {
                eprintln!("[TiddlyDesktop] Windows extract_data: got CF_TEXT ({} bytes)", text.len());
                types.push("Text".to_string());
                data.insert("Text".to_string(), text);
            }

            // 7. text/uri-list - URI list format (lowest priority in TW5)
            let cf_uri = get_cf_uri_list();
            eprintln!("[TiddlyDesktop] Windows extract_data: trying text/uri-list (cf={})", cf_uri);
            if let Some(uri_list) = self.get_string_data(data_object, cf_uri) {
                eprintln!("[TiddlyDesktop] Windows extract_data: got text/uri-list ({} bytes)", uri_list.len());
                types.push("text/uri-list".to_string());
                data.insert("text/uri-list".to_string(), uri_list);
            }

            eprintln!("[TiddlyDesktop] Windows extract_data: finished, got {} types", types.len());

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
                            let text = OsString::from_wide(&slice[..len]).to_string_lossy().to_string();
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

                            // Convert from system ANSI code page (CP_ACP) to UTF-16, then to UTF-8
                            // First, get required buffer size
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
                                    // Convert UTF-16 to UTF-8 String
                                    return String::from_utf16(&wide_buf[..result as usize]).ok();
                                }
                            } else {
                                let _ = GlobalUnlock(medium.u.hGlobal);
                            }
                            return None;
                        }
                    }
                }
            }
            None
        }

        /// Get UTF-16 encoded text from a custom clipboard format (like text/x-moz-url)
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
                            let text = OsString::from_wide(&slice[..len]).to_string_lossy().to_string();
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
                            let len = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
                            let text = String::from_utf8_lossy(&slice[..len]).to_string();
                            let _ = GlobalUnlock(medium.u.hGlobal);
                            return Some(text);
                        }
                    }
                }
            }
            None
        }

        /// Get Unicode URL data from a custom clipboard format (e.g., UniformResourceLocatorW)
        fn get_unicode_url(&self, data_object: &IDataObject, cf: u16) -> Option<String> {
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
                            let text = OsString::from_wide(&slice[..len]).to_string_lossy().to_string();
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
                        let path = OsString::from_wide(&buffer[..len as usize]).to_string_lossy().to_string();
                        paths.push(path);
                    }
                }
            }
            paths
        }

        fn screen_to_client(&self, x: i32, y: i32) -> (i32, i32) {
            // Convert screen coordinates (physical pixels) to CSS client coordinates (logical pixels)
            // IDropTarget receives physical pixel coordinates, but JavaScript's elementFromPoint
            // expects CSS pixels (logical), so we need to account for DPI scaling
            if let Ok(scale_factor) = self.window.scale_factor() {
                if let Ok(inner_pos) = self.window.inner_position() {
                    // inner_pos is in physical pixels, so subtract first, then divide by scale
                    let client_x = ((x - inner_pos.x) as f64 / scale_factor).round() as i32;
                    let client_y = ((y - inner_pos.y) as f64 / scale_factor).round() as i32;
                    return (client_x, client_y);
                }
            }
            (x, y)
        }
    }

    /// Choose a drop effect that the source allows
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

    /// Find the WebView2 content window (Chrome-based child window)
    /// WebView2 uses Chromium which creates child windows with class names containing "Chrome"
    fn find_webview2_content_hwnd(parent: HWND) -> Option<HWND> {
        // Structure to pass data to the callback
        struct EnumData {
            found: Option<HWND>,
        }

        unsafe extern "system" fn enum_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
            let data = &mut *(lparam.0 as *mut EnumData);

            // Get the class name
            let mut class_name = [0u16; 256];
            let len = GetClassNameW(hwnd, &mut class_name);
            if len > 0 {
                let class_str = OsString::from_wide(&class_name[..len as usize])
                    .to_string_lossy()
                    .to_string();

                // WebView2 content window has class name like "Chrome_WidgetWin_0" or "Chrome_WidgetWin_1"
                if class_str.starts_with("Chrome_WidgetWin") {
                    data.found = Some(hwnd);
                    return BOOL(0); // Stop enumeration
                }
            }
            BOOL(1) // Continue enumeration
        }

        let mut data = EnumData { found: None };
        unsafe {
            let _ = EnumChildWindows(
                Some(parent),
                Some(enum_callback),
                LPARAM(&mut data as *mut _ as isize),
            );
        }
        data.found
    }

    /// Set up drag-drop handling for a webview window
    pub fn setup_drag_handlers(window: &WebviewWindow) {
        let window_for_drop = window.clone();

        // CRITICAL: Both AllowExternalDrop AND RegisterDragDrop must be called from the
        // same thread that owns the window (the webview's thread). OLE drag-drop uses COM
        // and the IDropTarget must be registered from the window's owning thread for
        // events to be properly delivered. Registering from a different thread causes
        // the drop target to never receive events.
        let _ = window.with_webview(move |webview| {
            #[cfg(windows)]
            unsafe {
                use windows::core::Interface;

                // First, disable WebView2's AllowExternalDrop so our IDropTarget receives events
                let controller = webview.controller();
                if let Ok(controller4) = controller.cast::<ICoreWebView2Controller4>() {
                    match controller4.SetAllowExternalDrop(false) {
                        Ok(()) => {
                            eprintln!("[TiddlyDesktop] Windows: Disabled WebView2 AllowExternalDrop");
                        }
                        Err(e) => {
                            eprintln!("[TiddlyDesktop] Windows: Failed to disable AllowExternalDrop: {:?}", e);
                        }
                    }
                } else {
                    eprintln!("[TiddlyDesktop] Windows: Could not get ICoreWebView2Controller4 (older WebView2?)");
                }

                // Now register our IDropTarget on the WebView2 content window
                // This MUST be done on the same thread that processes the window's messages
                if let Ok(parent_hwnd) = window_for_drop.hwnd() {
                    let parent_hwnd = HWND(parent_hwnd.0 as *mut _);

                    // Find the WebView2 content window (Chrome_WidgetWin_*)
                    // This is where we need to register to receive drag events
                    let target_hwnd = if let Some(webview_hwnd) = find_webview2_content_hwnd(parent_hwnd) {
                        eprintln!("[TiddlyDesktop] Windows: Found WebView2 content window");
                        webview_hwnd
                    } else {
                        // Fallback to parent window if WebView2 content not found
                        eprintln!("[TiddlyDesktop] Windows: WebView2 content window not found, using parent");
                        parent_hwnd
                    };

                    // Create our drop target
                    let drop_target_ptr = DropTargetImpl::new(window_for_drop.clone());
                    let drop_target = DropTargetImpl::as_idroptarget(drop_target_ptr);

                    // Store for later cleanup
                    DROP_TARGET_MAP.lock().unwrap().insert(
                        target_hwnd.0 as isize,
                        SendDropTarget(drop_target_ptr)
                    );

                    // Revoke any existing drop target and register ours
                    let _ = RevokeDragDrop(target_hwnd);
                    match RegisterDragDrop(target_hwnd, &drop_target) {
                        Ok(()) => {
                            eprintln!("[TiddlyDesktop] Windows: Registered IDropTarget on WebView2 content window");
                        }
                        Err(e) => {
                            eprintln!("[TiddlyDesktop] Windows: Failed to register IDropTarget: {:?}", e);
                        }
                    }
                }
            }
        });
    }

    /// Clean up drop target for a window
    #[allow(dead_code)]
    pub fn cleanup_drag_handlers(window: &WebviewWindow) {
        if let Ok(parent_hwnd) = window.hwnd() {
            let parent_hwnd = HWND(parent_hwnd.0 as *mut _);

            // Find the WebView2 content window (same logic as setup)
            let target_hwnd = find_webview2_content_hwnd(parent_hwnd).unwrap_or(parent_hwnd);

            unsafe {
                let _ = RevokeDragDrop(target_hwnd);
                DROP_TARGET_MAP.lock().unwrap().remove(&(target_hwnd.0 as isize));
            }
        }
    }
}

/// macOS drag-drop handling - captures content from external drags via objc2
#[cfg(target_os = "macos")]
mod macos_drag {
    use std::collections::HashMap;
    use std::ffi::CStr;
    use std::sync::Mutex;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Sel};
    use objc2::{class, msg_send, sel, ClassType};
    use objc2_foundation::{NSArray, NSPoint, NSString, NSURL};
    use tauri::{Emitter, WebviewWindow};

    // Store window references for drag callbacks
    use lazy_static::lazy_static;
    lazy_static! {
        static ref WINDOW_MAP: Mutex<HashMap<usize, WebviewWindow>> = Mutex::new(HashMap::new());
    }

    /// Data captured from a drag operation
    #[derive(Clone, Debug, serde::Serialize)]
    pub struct DragContentData {
        pub types: Vec<String>,
        pub data: HashMap<String, String>,
    }

    /// NSDragOperation constants
    const NS_DRAG_OPERATION_NONE: usize = 0;
    const NS_DRAG_OPERATION_COPY: usize = 1;

    /// Create an NSString from a Rust string slice
    fn nsstring_from_str(s: &str) -> Retained<NSString> {
        NSString::from_str(s)
    }

    /// Get string from NSString
    fn nsstring_to_string(ns_string: &NSString) -> String {
        ns_string.to_string()
    }

    /// Extract drag data from dragging info pasteboard
    unsafe fn extract_drag_data(dragging_info: *mut AnyObject) -> Option<DragContentData> {
        if dragging_info.is_null() {
            return None;
        }

        let pasteboard: *mut AnyObject = msg_send![dragging_info, draggingPasteboard];
        if pasteboard.is_null() {
            return None;
        }

        let mut types = Vec::new();
        let mut data = HashMap::new();

        // Request types matching TiddlyWiki5's importDataTypes priority:
        // 1. text/vnd.tiddler  2. URL  3. text/x-moz-url  4. text/html  5. text/plain  6. Text  7. text/uri-list
        // macOS uses UTIs (Uniform Type Identifiers) which we map to MIME types
        let type_mappings: &[(&str, &str)] = &[
            // 1. text/vnd.tiddler - TiddlyWiki native format (custom UTI)
            ("text/vnd.tiddler", "text/vnd.tiddler"),
            // 2. URL - Standard URL format (contains data URI on macOS)
            ("public.url", "URL"),
            // 3. text/x-moz-url - Mozilla URL format (browsers may register this)
            ("text/x-moz-url", "text/x-moz-url"),
            // 4. text/html - HTML content
            ("public.html", "text/html"),
            ("Apple HTML pasteboard type", "text/html"),
            // 5. text/plain - Plain text (UTF-8)
            ("public.utf8-plain-text", "text/plain"),
            ("NSStringPboardType", "text/plain"),
            // 6. Text - Plain text fallback (same as text/plain on macOS)
            ("public.plain-text", "Text"),
            // 7. text/uri-list - URI list format
            ("public.url", "text/uri-list"),
        ];

        for (pb_type_name, mime_type) in type_mappings {
            let pb_type = nsstring_from_str(pb_type_name);
            let value: *mut AnyObject = msg_send![pasteboard, stringForType: &*pb_type];
            if !value.is_null() {
                // Cast to NSString and get the value
                let ns_str = value as *const NSString;
                let value_str = nsstring_to_string(&*ns_str);
                if !types.contains(&mime_type.to_string()) {
                    types.push(mime_type.to_string());
                }
                data.insert(mime_type.to_string(), value_str);
            }
        }

        if types.is_empty() {
            None
        } else {
            Some(DragContentData { types, data })
        }
    }

    /// Extract file paths from dragging info
    unsafe fn extract_file_paths(dragging_info: *mut AnyObject) -> Vec<String> {
        let mut paths = Vec::new();
        if dragging_info.is_null() {
            return paths;
        }

        let pasteboard: *mut AnyObject = msg_send![dragging_info, draggingPasteboard];
        if pasteboard.is_null() {
            return paths;
        }

        let file_url_type = nsstring_from_str("public.file-url");
        let pb_types: *mut AnyObject = msg_send![pasteboard, types];

        if !pb_types.is_null() {
            let contains: Bool = msg_send![pb_types, containsObject: &*file_url_type];
            if contains.as_bool() {
                let url_class = NSURL::class();
                let classes: *mut AnyObject = msg_send![class!(NSArray), arrayWithObject: url_class];
                let options: *mut AnyObject = msg_send![class!(NSDictionary), dictionary];
                let urls: *mut AnyObject = msg_send![pasteboard, readObjectsForClasses: classes, options: options];

                if !urls.is_null() {
                    let count: usize = msg_send![urls, count];
                    for i in 0..count {
                        let url: *mut AnyObject = msg_send![urls, objectAtIndex: i];
                        let is_file: Bool = msg_send![url, isFileURL];
                        if is_file.as_bool() {
                            let path: *mut AnyObject = msg_send![url, path];
                            if !path.is_null() {
                                let ns_str = path as *const NSString;
                                let path_str = nsstring_to_string(&*ns_str);
                                paths.push(path_str);
                            }
                        }
                    }
                }
            }
        }
        paths
    }

    /// Get window from view
    unsafe fn get_window_for_view(view: *mut AnyObject) -> Option<WebviewWindow> {
        if view.is_null() {
            return None;
        }
        let ns_window: *mut AnyObject = msg_send![view, window];
        if ns_window.is_null() {
            return None;
        }
        let window_id = ns_window as usize;
        WINDOW_MAP.lock().ok()?.get(&window_id).cloned()
    }

    /// Convert window coordinates to view (client) coordinates
    /// draggingLocation returns coordinates in window's coordinate system (origin at bottom-left)
    /// We need to convert to the view's coordinate system for elementFromPoint()
    unsafe fn convert_to_view_coords(view: *mut AnyObject, window_point: NSPoint) -> (i32, i32) {
        // Convert from window coordinates to view coordinates
        // nil as fromView means "from window coordinates"
        let view_point: NSPoint = msg_send![view, convertPoint: window_point, fromView: std::ptr::null::<AnyObject>()];

        // The view uses flipped coordinates (origin at top-left for WebKit)
        // But convertPoint handles this automatically for flipped views
        // Just cast to integers for JS
        (view_point.x as i32, view_point.y as i32)
    }

    /// draggingEntered: callback - called when drag enters the view
    extern "C" fn dragging_entered(this: *mut AnyObject, _sel: Sel, dragging_info: *mut AnyObject) -> usize {
        unsafe {
            if let Some(window) = get_window_for_view(this) {
                // Accept all drags and emit motion event
                let point: NSPoint = msg_send![dragging_info, draggingLocation];
                let (x, y) = convert_to_view_coords(this, point);
                let _ = window.emit("td-drag-motion", serde_json::json!({
                    "x": x,
                    "y": y
                }));
                return NS_DRAG_OPERATION_COPY;
            }
        }
        NS_DRAG_OPERATION_NONE
    }

    /// draggingUpdated: callback - called continuously during drag
    extern "C" fn dragging_updated(this: *mut AnyObject, _sel: Sel, dragging_info: *mut AnyObject) -> usize {
        unsafe {
            if let Some(window) = get_window_for_view(this) {
                let point: NSPoint = msg_send![dragging_info, draggingLocation];
                let (x, y) = convert_to_view_coords(this, point);
                let _ = window.emit("td-drag-motion", serde_json::json!({
                    "x": x,
                    "y": y
                }));
                return NS_DRAG_OPERATION_COPY;
            }
        }
        NS_DRAG_OPERATION_NONE
    }

    /// draggingExited: callback - called when drag leaves the view
    extern "C" fn dragging_exited(this: *mut AnyObject, _sel: Sel, _dragging_info: *mut AnyObject) {
        unsafe {
            if let Some(window) = get_window_for_view(this) {
                let _ = window.emit("td-drag-leave", ());
            }
        }
    }

    /// prepareForDragOperation: callback - called before performDragOperation to confirm we'll handle it
    extern "C" fn prepare_for_drag_operation(_this: *mut AnyObject, _sel: Sel, _dragging_info: *mut AnyObject) -> Bool {
        // Always return YES to indicate we'll handle the drop
        Bool::YES
    }

    /// performDragOperation: callback - called when drop occurs
    extern "C" fn perform_drag_operation(this: *mut AnyObject, _sel: Sel, dragging_info: *mut AnyObject) -> Bool {
        unsafe {
            if let Some(window) = get_window_for_view(this) {
                let point: NSPoint = msg_send![dragging_info, draggingLocation];
                let (x, y) = convert_to_view_coords(this, point);

                // Emit drop-start
                let _ = window.emit("td-drag-drop-start", serde_json::json!({
                    "x": x,
                    "y": y
                }));

                // Check for file paths first
                let file_paths = extract_file_paths(dragging_info);
                if !file_paths.is_empty() {
                    let _ = window.emit("td-file-drop", serde_json::json!({
                        "paths": file_paths
                    }));
                    return Bool::YES;
                }

                // Content drop
                if let Some(content_data) = extract_drag_data(dragging_info) {
                    let _ = window.emit("td-drag-content", &content_data);
                    return Bool::YES;
                }
            }
        }
        // Return YES even if we didn't emit anything - we handled the drop, just had no content
        Bool::YES
    }

    /// concludeDragOperation: callback - cleanup after drop
    extern "C" fn conclude_drag_operation(_this: *mut AnyObject, _sel: Sel, _dragging_info: *mut AnyObject) {
        // No cleanup needed, but implementing this prevents superclass from doing its own cleanup
    }

    /// wantsPeriodicDraggingUpdates - return YES for continuous draggingUpdated: calls
    extern "C" fn wants_periodic_dragging_updates(_this: *mut AnyObject, _sel: Sel) -> Bool {
        Bool::YES
    }

    // Declare object_setClass from Objective-C runtime
    extern "C" {
        fn object_setClass(obj: *mut AnyObject, cls: *const AnyClass) -> *const AnyClass;
        fn class_getName(cls: *const AnyClass) -> *const std::ffi::c_char;
        fn objc_getClass(name: *const std::ffi::c_char) -> *const AnyClass;
    }

    /// Add drag handling methods to a specific view instance using method swizzling
    unsafe fn setup_drag_methods_on_view(view: *mut AnyObject) {
        if view.is_null() {
            return;
        }

        // Get the view's class
        let view_class: *const AnyClass = msg_send![view, class];

        // Check if we've already added methods to this class
        let class_name_ptr = class_getName(view_class);
        let class_name = CStr::from_ptr(class_name_ptr).to_string_lossy();

        // Create a dynamic subclass for this specific view
        let subclass_name = format!("TD_{}", class_name);
        let subclass_cstr = std::ffi::CString::new(subclass_name.clone()).unwrap();

        // Check if subclass already exists
        let existing_class = objc_getClass(subclass_cstr.as_ptr());
        let subclass: *const AnyClass = if !existing_class.is_null() {
            existing_class
        } else {
            // Create new subclass
            let superclass = &*view_class;
            if let Some(mut builder) = ClassBuilder::new(&subclass_cstr, superclass) {
                builder.add_method(
                    sel!(draggingEntered:),
                    dragging_entered as extern "C" fn(*mut AnyObject, Sel, *mut AnyObject) -> usize,
                );
                builder.add_method(
                    sel!(draggingUpdated:),
                    dragging_updated as extern "C" fn(*mut AnyObject, Sel, *mut AnyObject) -> usize,
                );
                builder.add_method(
                    sel!(draggingExited:),
                    dragging_exited as extern "C" fn(*mut AnyObject, Sel, *mut AnyObject),
                );
                builder.add_method(
                    sel!(prepareForDragOperation:),
                    prepare_for_drag_operation as extern "C" fn(*mut AnyObject, Sel, *mut AnyObject) -> Bool,
                );
                builder.add_method(
                    sel!(performDragOperation:),
                    perform_drag_operation as extern "C" fn(*mut AnyObject, Sel, *mut AnyObject) -> Bool,
                );
                builder.add_method(
                    sel!(concludeDragOperation:),
                    conclude_drag_operation as extern "C" fn(*mut AnyObject, Sel, *mut AnyObject),
                );
                builder.add_method(
                    sel!(wantsPeriodicDraggingUpdates),
                    wants_periodic_dragging_updates as extern "C" fn(*mut AnyObject, Sel) -> Bool,
                );
                let registered = builder.register();
                registered as *const AnyClass
            } else {
                return;
            }
        };

        // Change the view's class to our subclass (isa-swizzling)
        object_setClass(view, subclass);
    }

    /// Set up drag-drop handling for a webview window
    pub fn setup_drag_handlers(window: &WebviewWindow) {
        let window_clone = window.clone();

        // Get the NSWindow
        if let Ok(ns_window) = window.ns_window() {
            let ns_window_ptr = ns_window as *mut AnyObject;

            unsafe {
                // Store window reference for callbacks
                let window_id = ns_window_ptr as usize;
                WINDOW_MAP.lock().unwrap().insert(window_id, window_clone);

                // Build array of drag types (in order of preference)
                let type_strings: Vec<Retained<NSString>> = vec![
                    nsstring_from_str("text/vnd.tiddler"),
                    nsstring_from_str("public.url"),
                    nsstring_from_str("public.utf8-plain-text"),
                    nsstring_from_str("NSStringPboardType"),
                    nsstring_from_str("public.html"),
                    nsstring_from_str("public.file-url"),
                ];
                let type_refs: Vec<&NSString> = type_strings.iter().map(|s| &**s).collect();
                let types = NSArray::from_slice(&type_refs);

                let content_view: *mut AnyObject = msg_send![ns_window_ptr, contentView];
                if !content_view.is_null() {
                    // Find the WKWebView
                    unsafe fn find_webview(view: *mut AnyObject) -> Option<*mut AnyObject> {
                        if view.is_null() {
                            return None;
                        }
                        let view_class: *const AnyClass = msg_send![view, class];
                        let class_name_ptr = class_getName(view_class);
                        let name = CStr::from_ptr(class_name_ptr).to_string_lossy();
                        if name.contains("WKWebView") {
                            return Some(view);
                        }
                        let subviews: *mut AnyObject = msg_send![view, subviews];
                        if !subviews.is_null() {
                            let count: usize = msg_send![subviews, count];
                            for i in 0..count {
                                let subview: *mut AnyObject = msg_send![subviews, objectAtIndex: i];
                                if let Some(wv) = find_webview(subview) {
                                    return Some(wv);
                                }
                            }
                        }
                        None
                    }

                    if let Some(webview) = find_webview(content_view) {
                        // Register drag types on the webview
                        let _: () = msg_send![webview, registerForDraggedTypes: &*types];

                        // Swizzle the webview's class to add our drag handlers
                        setup_drag_methods_on_view(webview);
                    }
                }
            }
        }
    }
}

use chrono::Local;
use serde::{Deserialize, Serialize};
use tauri::{
    image::Image,
    http::{Request, Response},
    menu::{MenuBuilder, MenuItemBuilder},
    tray::TrayIconBuilder,
    Emitter, Manager, WebviewUrl, WebviewWindowBuilder,
};

/// A wiki entry in the recent files list
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WikiEntry {
    pub path: String,
    pub filename: String,
    #[serde(default)]
    pub favicon: Option<String>, // Data URI for favicon
    #[serde(default)]
    pub is_folder: bool, // true if this is a wiki folder
    #[serde(default = "default_backups_enabled")]
    pub backups_enabled: bool, // whether to create backups on save (single-file only)
    #[serde(default)]
    pub backup_dir: Option<String>, // custom backup directory (if None, uses .backups folder next to wiki)
    #[serde(default)]
    pub group: Option<String>, // group name for organizing wikis (None = "Ungrouped")
}

fn default_backups_enabled() -> bool {
    true
}

/// Get the path to the recent files JSON
fn get_recent_files_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    Ok(data_dir.join("recent_wikis.json"))
}

/// Configuration for external attachments per wiki
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExternalAttachmentsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub use_absolute_for_descendents: bool,
    #[serde(default)]
    pub use_absolute_for_non_descendents: bool,
}

impl Default for ExternalAttachmentsConfig {
    fn default() -> Self {
        Self {
            enabled: true,  // Enable by default
            use_absolute_for_descendents: false,
            use_absolute_for_non_descendents: false,
        }
    }
}

fn default_true() -> bool {
    true
}

/// A single authentication URL entry
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthUrlEntry {
    pub name: String,
    pub url: String,
}

/// Configuration for session authentication URLs per wiki
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SessionAuthConfig {
    #[serde(default)]
    pub auth_urls: Vec<AuthUrlEntry>,
}

/// All wiki configs stored in a single file, keyed by wiki path
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
struct WikiConfigs {
    #[serde(default)]
    external_attachments: HashMap<String, ExternalAttachmentsConfig>,
    #[serde(default)]
    session_auth: HashMap<String, SessionAuthConfig>,
}

/// Get the path to the wiki configs JSON
fn get_wiki_configs_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    Ok(data_dir.join("wiki_configs.json"))
}

/// Load all wiki configs from disk
fn load_wiki_configs(app: &tauri::AppHandle) -> Result<WikiConfigs, String> {
    let path = get_wiki_configs_path(app)?;
    if path.exists() {
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read wiki configs: {}", e))?;
        serde_json::from_str(&content)
            .map_err(|e| format!("Failed to parse wiki configs: {}", e))
    } else {
        Ok(WikiConfigs::default())
    }
}

/// Save all wiki configs to disk
fn save_wiki_configs(app: &tauri::AppHandle, configs: &WikiConfigs) -> Result<(), String> {
    let path = get_wiki_configs_path(app)?;
    let content = serde_json::to_string_pretty(configs)
        .map_err(|e| format!("Failed to serialize wiki configs: {}", e))?;
    std::fs::write(&path, content)
        .map_err(|e| format!("Failed to write wiki configs: {}", e))
}

/// Load recent files from disk
fn load_recent_files_from_disk(app: &tauri::AppHandle) -> Vec<WikiEntry> {
    let path = match get_recent_files_path(app) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };

    if !path.exists() {
        return Vec::new();
    }

    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Save recent files to disk
fn save_recent_files_to_disk(app: &tauri::AppHandle, entries: &[WikiEntry]) -> Result<(), String> {
    let path = get_recent_files_path(app)?;

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let json = serde_json::to_string_pretty(entries).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())
}

/// Add or update a wiki in the recent files list
fn add_to_recent_files(app: &tauri::AppHandle, mut entry: WikiEntry) -> Result<(), String> {
    let mut entries = load_recent_files_from_disk(app);

    // Preserve backup settings from existing entry (if any)
    if let Some(existing) = entries.iter().find(|e| paths_equal(&e.path, &entry.path)) {
        entry.backups_enabled = existing.backups_enabled;
        entry.backup_dir = existing.backup_dir.clone();
    }

    // Remove existing entry with same path (if any)
    entries.retain(|e| !paths_equal(&e.path, &entry.path));

    // Add new entry at the beginning
    entries.insert(0, entry);

    // Keep only the most recent 50 entries
    entries.truncate(50);

    save_recent_files_to_disk(app, &entries)
}

/// Get recent files list
/// Debug logging from JavaScript - prints to terminal
#[tauri::command]
fn js_log(message: String) {
    eprintln!("[TiddlyDesktop] JS: {}", message);
}

/// Clipboard content data structure (same format as drag-drop)
#[derive(serde::Serialize)]
struct ClipboardContentData {
    types: Vec<String>,
    data: std::collections::HashMap<String, String>,
}

/// Get clipboard content for paste handling
/// Returns content in the same format as drag-drop for consistent processing
#[tauri::command]
fn get_clipboard_content() -> Result<ClipboardContentData, String> {
    #[cfg(target_os = "linux")]
    {
        get_clipboard_content_linux()
    }
    #[cfg(target_os = "windows")]
    {
        get_clipboard_content_windows()
    }
    #[cfg(target_os = "macos")]
    {
        get_clipboard_content_macos()
    }
}

#[cfg(target_os = "linux")]
fn get_clipboard_content_linux() -> Result<ClipboardContentData, String> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    // Helper to decode text with proper encoding detection (same as drag-drop)
    fn decode_text(raw_data: &[u8]) -> Option<String> {
        if raw_data.is_empty() {
            return None;
        }

        // Check for BOM
        if raw_data.len() >= 3 && raw_data[0] == 0xEF && raw_data[1] == 0xBB && raw_data[2] == 0xBF {
            return String::from_utf8(raw_data[3..].to_vec()).ok();
        }
        if raw_data.len() >= 2 && raw_data[0] == 0xFF && raw_data[1] == 0xFE {
            if raw_data.len() % 2 == 0 {
                let u16_data: Vec<u16> = raw_data[2..]
                    .chunks_exact(2)
                    .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                    .collect();
                return String::from_utf16(&u16_data).ok();
            }
        }
        if raw_data.len() >= 2 && raw_data[0] == 0xFE && raw_data[1] == 0xFF {
            if raw_data.len() % 2 == 0 {
                let u16_data: Vec<u16> = raw_data[2..]
                    .chunks_exact(2)
                    .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
                    .collect();
                return String::from_utf16(&u16_data).ok();
            }
        }

        // Check for UTF-16LE/BE pattern BEFORE trying UTF-8
        if raw_data.len() >= 4 && raw_data.len() % 2 == 0 {
            let looks_like_utf16le = raw_data[1] == 0 && raw_data[3] == 0
                && raw_data[0] != 0 && raw_data[2] != 0;
            if looks_like_utf16le {
                eprintln!("[TiddlyDesktop] Clipboard: Detected UTF-16LE encoding");
                let u16_data: Vec<u16> = raw_data
                    .chunks_exact(2)
                    .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                    .collect();
                if let Ok(s) = String::from_utf16(&u16_data) {
                    return Some(s);
                }
            }

            let looks_like_utf16be = raw_data[0] == 0 && raw_data[2] == 0
                && raw_data[1] != 0 && raw_data[3] != 0;
            if looks_like_utf16be {
                eprintln!("[TiddlyDesktop] Clipboard: Detected UTF-16BE encoding");
                let u16_data: Vec<u16> = raw_data
                    .chunks_exact(2)
                    .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
                    .collect();
                if let Ok(s) = String::from_utf16(&u16_data) {
                    return Some(s);
                }
            }
        }

        // Try UTF-8
        if let Ok(s) = String::from_utf8(raw_data.to_vec()) {
            return Some(s);
        }

        None
    }

    let mut types = Vec::new();
    let mut data = HashMap::new();

    // GTK3 clipboard API
    let display = gdk::Display::default().ok_or("No display")?;
    let clipboard = gtk::Clipboard::default(&display).ok_or("No clipboard")?;

    // Request formats in TiddlyWiki priority order
    let formats_to_try = [
        "text/vnd.tiddler",
        "text/html",
        "text/plain",
        "UTF8_STRING",
        "STRING",
    ];

    for clipboard_type in formats_to_try {
        let target = gdk::Atom::intern(clipboard_type);

        // Use request_contents to get raw data with proper encoding
        let result: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
        let result_clone = result.clone();

        clipboard.request_contents(&target, move |_clipboard, selection_data| {
            let raw_data = selection_data.data().to_vec();
            if let Ok(mut guard) = result_clone.lock() {
                *guard = Some(raw_data);
            }
        });

        // Process pending GTK events to complete the async request
        while gtk::events_pending() {
            gtk::main_iteration();
        }

        // Small delay to ensure callback completes
        std::thread::sleep(std::time::Duration::from_millis(10));
        while gtk::events_pending() {
            gtk::main_iteration();
        }

        if let Ok(guard) = result.lock() {
            if let Some(raw_data) = guard.as_ref() {
                if !raw_data.is_empty() {
                    // Check for null bytes indicating misinterpreted UTF-16
                    let text = if raw_data.contains(&0) {
                        decode_text(raw_data)
                    } else {
                        String::from_utf8(raw_data.clone()).ok()
                    };

                    if let Some(text) = text {
                        if !text.is_empty() {
                            let mime_type = if clipboard_type == "UTF8_STRING" || clipboard_type == "STRING" {
                                "text/plain"
                            } else {
                                clipboard_type
                            };

                            if !types.contains(&mime_type.to_string()) {
                                types.push(mime_type.to_string());
                                data.insert(mime_type.to_string(), text);
                                eprintln!("[TiddlyDesktop] Clipboard: Got {} ({} chars)", mime_type, data.get(mime_type).map(|s| s.len()).unwrap_or(0));
                            }
                        }
                    }
                }
            }
        };
    }

    // Fallback: try wait_for_text (simpler but may have encoding issues)
    if types.is_empty() {
        if let Some(text) = clipboard.wait_for_text() {
            let text_str = text.to_string();
            if !text_str.is_empty() {
                types.push("text/plain".to_string());
                data.insert("text/plain".to_string(), text_str);
                eprintln!("[TiddlyDesktop] Clipboard: Fallback got text/plain");
            }
        }
    }

    eprintln!("[TiddlyDesktop] Clipboard: Returning {} types", types.len());
    Ok(ClipboardContentData { types, data })
}

#[cfg(target_os = "windows")]
fn get_clipboard_content_windows() -> Result<ClipboardContentData, String> {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::{
        OpenClipboard, CloseClipboard, GetClipboardData, RegisterClipboardFormatA,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock, GlobalSize};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    let mut types = Vec::new();
    let mut data = HashMap::new();

    unsafe {
        if OpenClipboard(None).is_err() {
            return Err("Failed to open clipboard".to_string());
        }

        // Get HTML format - RegisterClipboardFormatA returns 0 on failure, format ID on success
        let cf_html = RegisterClipboardFormatA(windows::core::s!("HTML Format"));

        if cf_html != 0 {
            if let Ok(h) = GetClipboardData(cf_html) {
                if !h.0.is_null() {
                    let ptr = GlobalLock(HGLOBAL(h.0)) as *const u8;
                    if !ptr.is_null() {
                        let size = GlobalSize(HGLOBAL(h.0));
                        let slice = std::slice::from_raw_parts(ptr, size);
                        let len = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
                        let html = String::from_utf8_lossy(&slice[..len]).to_string();

                        // Extract content from Windows HTML Format markers
                        if let Some(start) = html.find("<!--StartFragment-->") {
                            if let Some(end) = html.find("<!--EndFragment-->") {
                                let content = &html[start + 20..end];
                                types.push("text/html".to_string());
                                data.insert("text/html".to_string(), content.to_string());
                                eprintln!("[TiddlyDesktop] Clipboard: Got text/html ({} chars)", content.len());
                            }
                        } else if !html.is_empty() {
                            types.push("text/html".to_string());
                            data.insert("text/html".to_string(), html.clone());
                            eprintln!("[TiddlyDesktop] Clipboard: Got text/html ({} chars)", html.len());
                        }

                        let _ = GlobalUnlock(HGLOBAL(h.0));
                    }
                }
            }
        }

        // Get Unicode text
        if let Ok(h) = GetClipboardData(CF_UNICODETEXT.0 as u32) {
            if !h.0.is_null() {
                let ptr = GlobalLock(HGLOBAL(h.0)) as *const u16;
                if !ptr.is_null() {
                    let size = GlobalSize(HGLOBAL(h.0)) / 2;
                    let slice = std::slice::from_raw_parts(ptr, size);
                    let len = slice.iter().position(|&c| c == 0).unwrap_or(slice.len());
                    let text = OsString::from_wide(&slice[..len]).to_string_lossy().to_string();

                    if !text.is_empty() {
                        types.push("text/plain".to_string());
                        data.insert("text/plain".to_string(), text.clone());
                        eprintln!("[TiddlyDesktop] Clipboard: Got text/plain ({} chars)", text.len());
                    }

                    let _ = GlobalUnlock(HGLOBAL(h.0));
                }
            }
        }

        let _ = CloseClipboard();
    }

    eprintln!("[TiddlyDesktop] Clipboard: Returning {} types", types.len());
    Ok(ClipboardContentData { types, data })
}

#[cfg(target_os = "macos")]
fn get_clipboard_content_macos() -> Result<ClipboardContentData, String> {
    use std::collections::HashMap;
    use objc2_foundation::NSString;
    use objc2_app_kit::NSPasteboard;

    let mut types = Vec::new();
    let mut data = HashMap::new();

    let pasteboard = NSPasteboard::generalPasteboard();

    // Request types matching TiddlyWiki5's importDataTypes priority
    let type_mappings: &[(&str, &str)] = &[
        ("public.html", "text/html"),
        ("Apple HTML pasteboard type", "text/html"),
        ("public.utf8-plain-text", "text/plain"),
        ("NSStringPboardType", "text/plain"),
        ("public.plain-text", "text/plain"),
    ];

    for (pb_type_name, mime_type) in type_mappings {
        let pb_type = NSString::from_str(pb_type_name);
        if let Some(ns_str) = pasteboard.stringForType(&pb_type) {
            let value_str = ns_str.to_string();
            if !value_str.is_empty() && !types.contains(&mime_type.to_string()) {
                let len = value_str.len();
                types.push(mime_type.to_string());
                data.insert(mime_type.to_string(), value_str);
                eprintln!("[TiddlyDesktop] Clipboard: Got {} ({} chars)", mime_type, len);
            }
        }
    }

    eprintln!("[TiddlyDesktop] Clipboard: Returning {} types", types.len());
    Ok(ClipboardContentData { types, data })
}

#[tauri::command]
fn get_recent_files(app: tauri::AppHandle) -> Vec<WikiEntry> {
    load_recent_files_from_disk(&app)
}

/// Remove a wiki from the recent files list
#[tauri::command]
fn remove_recent_file(app: tauri::AppHandle, path: String) -> Result<(), String> {
    let mut entries = load_recent_files_from_disk(&app);
    entries.retain(|e| !paths_equal(&e.path, &path));
    save_recent_files_to_disk(&app, &entries)
}

/// Set backups enabled/disabled for a wiki
#[tauri::command]
fn set_wiki_backups(app: tauri::AppHandle, path: String, enabled: bool) -> Result<(), String> {
    let mut entries = load_recent_files_from_disk(&app);

    for entry in entries.iter_mut() {
        if paths_equal(&entry.path, &path) {
            entry.backups_enabled = enabled;
            break;
        }
    }

    save_recent_files_to_disk(&app, &entries)
}

/// Set custom backup directory for a wiki (None to use default .backups folder)
#[tauri::command]
fn set_wiki_backup_dir(app: tauri::AppHandle, path: String, backup_dir: Option<String>) -> Result<(), String> {
    let mut entries = load_recent_files_from_disk(&app);

    for entry in entries.iter_mut() {
        if paths_equal(&entry.path, &path) {
            entry.backup_dir = backup_dir;
            break;
        }
    }

    save_recent_files_to_disk(&app, &entries)
}

/// Update favicon for a wiki (used after decryption when favicon wasn't available initially)
#[tauri::command]
fn update_wiki_favicon(app: tauri::AppHandle, path: String, favicon: Option<String>) -> Result<(), String> {
    let mut entries = load_recent_files_from_disk(&app);

    for entry in entries.iter_mut() {
        if paths_equal(&entry.path, &path) {
            entry.favicon = favicon;
            break;
        }
    }

    save_recent_files_to_disk(&app, &entries)
}

/// Set group for a wiki (None to move to "Ungrouped")
#[tauri::command]
fn set_wiki_group(app: tauri::AppHandle, path: String, group: Option<String>) -> Result<(), String> {
    let mut entries = load_recent_files_from_disk(&app);

    for entry in entries.iter_mut() {
        if paths_equal(&entry.path, &path) {
            entry.group = group;
            break;
        }
    }

    save_recent_files_to_disk(&app, &entries)
}

/// Get all unique group names from the wiki list
#[tauri::command]
fn get_wiki_groups(app: tauri::AppHandle) -> Vec<String> {
    let entries = load_recent_files_from_disk(&app);
    let mut groups: Vec<String> = entries
        .iter()
        .filter_map(|e| e.group.clone())
        .collect();
    groups.sort();
    groups.dedup();
    groups
}

/// Rename a group (updates all wikis in that group)
#[tauri::command]
fn rename_wiki_group(app: tauri::AppHandle, old_name: String, new_name: String) -> Result<(), String> {
    let mut entries = load_recent_files_from_disk(&app);

    for entry in entries.iter_mut() {
        if entry.group.as_ref() == Some(&old_name) {
            entry.group = Some(new_name.clone());
        }
    }

    save_recent_files_to_disk(&app, &entries)
}

/// Delete a group (moves all wikis to Ungrouped)
#[tauri::command]
fn delete_wiki_group(app: tauri::AppHandle, group_name: String) -> Result<(), String> {
    let mut entries = load_recent_files_from_disk(&app);

    for entry in entries.iter_mut() {
        if entry.group.as_ref() == Some(&group_name) {
            entry.group = None;
        }
    }

    save_recent_files_to_disk(&app, &entries)
}

/// Get current backup directory setting for a wiki (None means default .backups folder)
#[tauri::command]
fn get_wiki_backup_dir_setting(app: tauri::AppHandle, path: String) -> Option<String> {
    let entries = load_recent_files_from_disk(&app);

    for entry in entries {
        if paths_equal(&entry.path, &path) {
            return entry.backup_dir;
        }
    }

    None
}

/// Determine storage mode for macOS/Linux
/// Always uses the app data directory (portable mode only available on Windows)
#[cfg(not(target_os = "windows"))]
fn determine_storage_mode(app: &tauri::App) -> Result<PathBuf, String> {
    app.path().app_data_dir().map_err(|e| e.to_string())
}

/// Windows: determine storage mode based on marker file
#[cfg(target_os = "windows")]
fn determine_storage_mode(app: &tauri::App) -> Result<PathBuf, String> {
    let exe_path = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe_dir = exe_path.parent().ok_or("No exe directory")?;

    // Check for portable marker
    if exe_dir.join("portable").exists() || exe_dir.join("portable.txt").exists() {
        return Ok(exe_dir.to_path_buf());
    }

    // Check if portable data file already exists (user chose portable mode previously)
    if exe_dir.join("tiddlydesktop.html").exists() {
        return Ok(exe_dir.to_path_buf());
    }

    // Installed mode: app data directory
    app.path().app_data_dir().map_err(|e| e.to_string())
}

/// Get the user editions directory path
/// Location: ~/.local/share/tiddlydesktop-rs/editions/ (Linux) or equivalent on other platforms
fn get_user_editions_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    Ok(data_dir.join("editions"))
}

/// Extract a tiddler's text content from TiddlyWiki HTML
/// Supports both JSON format (TW 5.2+) and div format (older)
fn extract_tiddler_from_html(html: &str, tiddler_title: &str) -> Option<String> {
    // TiddlyWiki stores tiddlers in multiple formats. Saved/modified tiddlers appear at the
    // END of the tiddler store as single-escaped JSON. Plugin-embedded tiddlers appear
    // earlier as double-escaped JSON. We need to find the LAST occurrence (most recent save).

    // First try single-escaped JSON format (saved tiddlers at end of file)
    // Format: {"title":"$:/TiddlyDesktop/WikiList","type":"application/json","text":"[...]"}
    let single_escaped_search = format!(r#"{{"title":"{}""#, tiddler_title);

    // Find the LAST occurrence (most recently saved version)
    if let Some(start_idx) = html.rfind(&single_escaped_search) {
        let after_title = &html[start_idx..std::cmp::min(start_idx + 2_000_000, html.len())];
        // Look for "text":" pattern (single-escaped)
        let text_pattern = r#""text":""#;
        if let Some(text_start) = after_title.find(text_pattern) {
            let text_content_start = text_start + 8; // length of "text":" (8 chars)
            if text_content_start < after_title.len() {
                let remaining = &after_title[text_content_start..];
                // Find closing " that's not escaped with backslash
                let mut end_pos = 0;
                let bytes = remaining.as_bytes();
                while end_pos < bytes.len() {
                    if bytes[end_pos] == b'"' {
                        // Check if escaped
                        let mut backslash_count = 0;
                        let mut check_pos = end_pos;
                        while check_pos > 0 && bytes[check_pos - 1] == b'\\' {
                            backslash_count += 1;
                            check_pos -= 1;
                        }
                        // If even number of backslashes, quote is not escaped
                        if backslash_count % 2 == 0 {
                            break;
                        }
                    }
                    end_pos += 1;
                }
                if end_pos < remaining.len() {
                    let text = &remaining[..end_pos];
                    // Unescape single-escaped JSON
                    let unescaped = text
                        .replace("\\n", "\n")
                        .replace("\\t", "\t")
                        .replace("\\r", "\r")
                        .replace("\\\"", "\"")
                        .replace("\\\\", "\\");
                    return Some(unescaped);
                }
            }
        }
    }

    // Try double-escaped JSON format (inside plugin bundles)
    // Format: \"$:/Title\":{\"title\":\"...\",\"text\":\"value\",...}
    let escaped_search = format!(r#"\"{}\":{{"#, tiddler_title);

    // Search from end to find the last (most recent) occurrence
    if let Some(start_idx) = html.rfind(&escaped_search) {
        let after_title = &html[start_idx..std::cmp::min(start_idx + 2_000_000, html.len())];
        let text_pattern = r#"\"text\":\""#;
        if let Some(text_start) = after_title.find(text_pattern) {
            let text_content_start = text_start + 11; // length of \"text\":\" (11 chars)
            if text_content_start < after_title.len() {
                let remaining = &after_title[text_content_start..];
                // Find the closing \" - need to skip escaped backslashes
                let mut end_pos = 0;
                let bytes = remaining.as_bytes();
                while end_pos < bytes.len().saturating_sub(1) {
                    if bytes[end_pos] == b'\\' && bytes[end_pos + 1] == b'"' {
                        // Check if this backslash is escaped (preceded by \\)
                        if end_pos >= 2 && bytes[end_pos - 1] == b'\\' && bytes[end_pos - 2] == b'\\' {
                            // This is \\\\" - the backslash is escaped, so \" is the real end
                            break;
                        } else if end_pos >= 1 && bytes[end_pos - 1] == b'\\' {
                            // This is \\" - skip it (escaped quote inside string)
                            end_pos += 2;
                            continue;
                        }
                        // Found unescaped \"
                        break;
                    }
                    end_pos += 1;
                }
                if end_pos < remaining.len() {
                    let text = &remaining[..end_pos];
                    // Unescape double-escaped JSON (embedded in JS string)
                    let unescaped = text
                        .replace("\\\\n", "\n")
                        .replace("\\\\t", "\t")
                        .replace("\\\\r", "\r")
                        .replace("\\\\\\\\", "\\");
                    return Some(unescaped);
                }
            }
        }
    }

    // Fallback to div format (older TiddlyWiki)
    let escaped_title = regex::escape(tiddler_title);
    let pattern = format!(
        r#"<div[^>]*\stitle="{}"[^>]*>([\s\S]*?)</div>"#,
        escaped_title
    );
    let re = regex::Regex::new(&pattern).ok()?;
    let caps = re.captures(html)?;
    let content = caps.get(1)?.as_str();
    // Decode HTML entities
    Some(html_decode(content))
}

/// Decode basic HTML entities
fn html_decode(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#34;", "\"")
}

/// Encode basic HTML entities
fn html_encode(s: &str) -> String {
    s.replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace("\"", "&quot;")
}

/// Inject or replace a tiddler in TiddlyWiki HTML
/// Works with modern TiddlyWiki JSON store format
fn inject_tiddler_into_html(html: &str, tiddler_title: &str, tiddler_type: &str, content: &str) -> String {
    // Modern TiddlyWiki (5.2+) uses JSON store in a script tag
    // Format: <script class="tiddlywiki-tiddler-store" type="application/json">[{...}]</script>

    // Escape content for JSON string
    let json_escaped_content = content
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");

    // Create the new tiddler JSON object
    let new_tiddler = format!(
        r#"{{"title":"{}","type":"{}","text":"{}"}}"#,
        tiddler_title, tiddler_type, json_escaped_content
    );

    // Find the tiddler store - look for the LAST one (TW can have multiple stores)
    // The store ends with ]</script>
    let store_end = r#"]</script>"#;

    if let Some(end_pos) = html.rfind(store_end) {
        // Insert the new tiddler before the closing ]
        let mut result = String::with_capacity(html.len() + new_tiddler.len() + 10);
        result.push_str(&html[..end_pos]);
        result.push(',');
        result.push_str(&new_tiddler);
        result.push_str(&html[end_pos..]);
        return result;
    }

    // Fallback to div format for older TiddlyWiki
    let encoded_content = html_encode(content);
    let new_div = format!(
        r#"<div title="{}" type="{}">{}</div>"#,
        tiddler_title, tiddler_type, encoded_content
    );

    let store_end_markers = [
        "</div><!--~~ Library modules ~~-->",
        r#"</div><script"#,
    ];

    for marker in &store_end_markers {
        if let Some(pos) = html.find(marker) {
            let mut result = String::with_capacity(html.len() + new_div.len() + 1);
            result.push_str(&html[..pos]);
            result.push_str(&new_div);
            result.push('\n');
            result.push_str(&html[pos..]);
            return result;
        }
    }

    // Fallback: return unchanged
    html.to_string()
}

/// Get the bundled index.html path
fn get_bundled_index_path(app: &tauri::App) -> Result<PathBuf, String> {
    // Use our helper that prefers exe-relative paths (avoids baked-in CI paths)
    let resource_path = get_resource_dir_path(app.handle())
        .ok_or_else(|| "Failed to get resource directory".to_string())?;
    let resource_path = normalize_path(resource_path);

    let possible_sources = [
        resource_path.join("resources").join("index.html"),
        resource_path.join("index.html"),
    ];

    for source in &possible_sources {
        if source.exists() {
            return Ok(source.clone());
        }
    }

    // Development fallback (cargo runs from src-tauri directory)
    let dev_sources = [
        PathBuf::from("../src/index.html"),
        PathBuf::from("src/index.html"),
    ];
    for dev_source in &dev_sources {
        if dev_source.exists() {
            return Ok(dev_source.clone());
        }
    }

    Err(format!("Could not find source index.html. Tried: {:?}", possible_sources))
}

/// Ensure main wiki file exists, extracting from resources if needed
/// Also handles migration when bundled version is newer than existing
fn ensure_main_wiki_exists(app: &tauri::App) -> Result<PathBuf, String> {
    let wiki_dir = determine_storage_mode(app)?;
    std::fs::create_dir_all(&wiki_dir).map_err(|e| format!("Failed to create wiki dir: {}", e))?;

    let main_wiki_path = wiki_dir.join("tiddlydesktop.html");
    let bundled_path = get_bundled_index_path(app)?;

    if !main_wiki_path.exists() {
        // First run: copy from bundled resources
        std::fs::copy(&bundled_path, &main_wiki_path)
            .map_err(|e| format!("Failed to copy wiki: {}", e))?;
        println!("Created main wiki from {:?}", bundled_path);
    } else {
        // Check if we need to migrate to a newer version
        let existing_html = std::fs::read_to_string(&main_wiki_path)
            .map_err(|e| format!("Failed to read existing wiki: {}", e))?;
        let bundled_html = std::fs::read_to_string(&bundled_path)
            .map_err(|e| format!("Failed to read bundled wiki: {}", e))?;

        let existing_version = extract_tiddler_from_html(&existing_html, "$:/TiddlyDesktop/AppVersion")
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(0);
        let bundled_version = extract_tiddler_from_html(&bundled_html, "$:/TiddlyDesktop/AppVersion")
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(0);

        if bundled_version > existing_version {
            println!("Migrating to newer version...");

            // Extract user data from existing wiki
            let wiki_list = extract_tiddler_from_html(&existing_html, "$:/TiddlyDesktop/WikiList");

            // Start with bundled HTML
            let mut new_html = bundled_html;

            // Inject user data into new HTML
            if let Some(list) = wiki_list {
                println!("Preserving wiki list during migration");
                new_html = inject_tiddler_into_html(&new_html, "$:/TiddlyDesktop/WikiList", "application/json", &list);
            }

            // Write the migrated wiki
            std::fs::write(&main_wiki_path, new_html)
                .map_err(|e| format!("Failed to write migrated wiki: {}", e))?;
            println!("Migration complete");
        }
    }

    Ok(main_wiki_path)
}

/// A running wiki folder server
#[allow(dead_code)] // Fields may be used for status display in future
struct WikiFolderServer {
    process: Child,
    port: u16,
    path: String,
}

/// App state
struct AppState {
    /// Mapping of encoded paths to actual file paths
    wiki_paths: Mutex<HashMap<String, PathBuf>>,
    /// Mapping of window labels to wiki paths (for duplicate detection)
    open_wikis: Mutex<HashMap<String, String>>,
    /// Running wiki folder servers (keyed by window label)
    wiki_servers: Mutex<HashMap<String, WikiFolderServer>>,
    /// Next available port for wiki folder servers
    next_port: Mutex<u16>,
    /// Path to the main wiki file (tiddlydesktop.html)
    main_wiki_path: PathBuf,
}

/// Extract favicon from the $:/favicon.ico tiddler in TiddlyWiki HTML
/// The tiddler contains base64-encoded image data with a type field
fn extract_favicon_from_tiddler(html: &str) -> Option<String> {
    // Try single-escaped format first (saved tiddlers at end of store)
    // Format: "$:/favicon.ico","text":"base64data"
    let single_pattern = r#""$:/favicon.ico","#;
    if let Some(start_idx) = html.rfind(single_pattern) {
        let after_start = &html[start_idx..std::cmp::min(start_idx + 500_000, html.len())];

        // Extract the text field (base64 content)
        if let Some(text_start) = after_start.find(r#""text":""#) {
            let after_text = &after_start[text_start + 8..];
            // Find closing quote (not escaped)
            let mut end_pos = 0;
            let bytes = after_text.as_bytes();
            while end_pos < bytes.len() {
                if bytes[end_pos] == b'"' {
                    let mut backslash_count = 0;
                    let mut check_pos = end_pos;
                    while check_pos > 0 && bytes[check_pos - 1] == b'\\' {
                        backslash_count += 1;
                        check_pos -= 1;
                    }
                    if backslash_count % 2 == 0 {
                        break;
                    }
                }
                end_pos += 1;
            }
            if end_pos > 0 && end_pos < after_text.len() {
                let base64_content = &after_text[..end_pos];
                if !base64_content.is_empty() && !base64_content.starts_with('[') {
                    // Try to extract type field
                    let mime_type = if let Some(type_start) = after_start.find(r#""type":""#) {
                        let after_type = &after_start[type_start + 8..];
                        if let Some(type_end) = after_type.find('"') {
                            &after_type[..type_end]
                        } else {
                            "image/png"
                        }
                    } else {
                        "image/png"
                    };
                    return Some(format!("data:{};base64,{}", mime_type, base64_content));
                }
            }
        }
    }

    // Try double-escaped format (inside plugin bundles)
    // Pattern: \"$:/favicon.ico\":{\"title\":\"$:/favicon.ico\",\"type\":\"image/...\",\"text\":\"base64data\",...}
    let tiddler_pattern = r#"\"$:/favicon.ico\":{"#;

    if let Some(start_idx) = html.rfind(tiddler_pattern) {
        // Find the end of this tiddler object - need to track brace depth
        let after_start = &html[start_idx + tiddler_pattern.len()..];
        let mut brace_depth = 1;
        let mut end_idx = 0;
        for (i, c) in after_start.char_indices() {
            match c {
                '{' => brace_depth += 1,
                '}' => {
                    brace_depth -= 1;
                    if brace_depth == 0 {
                        end_idx = i;
                        break;
                    }
                }
                _ => {}
            }
            // Safety limit - favicon tiddlers shouldn't be huge
            if i > 1_000_000 {
                break;
            }
        }

        if end_idx > 0 {
            let tiddler_content = &after_start[..end_idx];

            // Extract the type field
            let mime_type = if let Some(type_start) = tiddler_content.find(r#"\"type\":\""#) {
                let after_type = &tiddler_content[type_start + 10..];
                if let Some(type_end) = after_type.find(r#"\""#) {
                    Some(&after_type[..type_end])
                } else {
                    None
                }
            } else {
                // Default to image/png if no type specified
                Some("image/png")
            };

            // Extract the text field (base64 content)
            if let Some(text_start) = tiddler_content.find(r#"\"text\":\""#) {
                let after_text = &tiddler_content[text_start + 10..];
                if let Some(text_end) = after_text.find(r#"\""#) {
                    let base64_content = &after_text[..text_end];
                    if !base64_content.is_empty() {
                        // Construct data URI
                        let mime = mime_type.unwrap_or("image/png");
                        return Some(format!("data:{};base64,{}", mime, base64_content));
                    }
                }
            }
        }
    }

    None
}

/// Extract favicon from wiki HTML content
/// First tries the <link> tag in <head>, then falls back to $:/favicon.ico tiddler
fn extract_favicon(content: &str) -> Option<String> {
    // First try: Look for favicon link with data URI in the head section
    // Search up to </head> since large <style> sections can push it past 64KB
    let head_end = content.find("</head>")
        .or_else(|| content.find("</HEAD>"))
        .unwrap_or(content.len().min(500_000)); // Fallback to 500KB max
    let search_content = &content[..head_end];

    // Look for favicon link with data URI
    // Common patterns:
    // <link id="faviconLink" rel="shortcut icon" href="data:image/...">
    // <link rel="icon" href="data:image/...">

    // Find favicon link elements
    for pattern in &["<link", "<LINK"] {
        let mut search_pos = 0;
        while let Some(link_start) = search_content[search_pos..].find(pattern) {
            let abs_start = search_pos + link_start;
            if let Some(link_end) = search_content[abs_start..].find('>') {
                let link_tag = &search_content[abs_start..abs_start + link_end + 1];
                let link_tag_lower = link_tag.to_lowercase();

                // Check if this is a favicon link
                if (link_tag_lower.contains("icon") || link_tag_lower.contains("faviconlink"))
                    && link_tag_lower.contains("href=")
                {
                    // Extract href value
                    if let Some(href_start) = link_tag.to_lowercase().find("href=") {
                        let after_href = &link_tag[href_start + 5..];
                        let quote_char = after_href.chars().next();
                        if let Some(q) = quote_char {
                            if q == '"' || q == '\'' {
                                if let Some(href_end) = after_href[1..].find(q) {
                                    let href = &after_href[1..href_end + 1];
                                    if href.starts_with("data:image") {
                                        return Some(href.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
                search_pos = abs_start + link_end + 1;
            } else {
                break;
            }
        }
    }

    // Second try: Extract from $:/favicon.ico tiddler
    // This requires searching the full content since tiddlers are later in the file
    extract_favicon_from_tiddler(content)
}

/// Extract favicon from a wiki folder by reading the favicon file
async fn extract_favicon_from_folder(wiki_path: &PathBuf) -> Option<String> {
    use base64::{engine::general_purpose::STANDARD, Engine};

    let tiddlers_path = wiki_path.join("tiddlers");

    // TiddlyWiki stores $:/favicon.ico as $__favicon.ico.EXT ($ and : and / escaped)
    // Common patterns: $__favicon.ico.png, $__favicon.ico.ico, $__favicon.ico
    let favicon_patterns = [
        ("$__favicon.ico.png", "image/png"),
        ("$__favicon.ico.jpg", "image/jpeg"),
        ("$__favicon.ico.jpeg", "image/jpeg"),
        ("$__favicon.ico.gif", "image/gif"),
        ("$__favicon.ico.ico", "image/x-icon"),
        ("$__favicon.ico", "image/x-icon"),
        ("favicon.ico", "image/x-icon"),
        ("favicon.png", "image/png"),
    ];

    for (filename, mime_type) in &favicon_patterns {
        let favicon_path = tiddlers_path.join(filename);
        if let Ok(data) = tokio::fs::read(&favicon_path).await {
            // Convert to base64 data URI
            let base64_data = STANDARD.encode(&data);
            return Some(format!("data:{};base64,{}", mime_type, base64_data));
        }
    }

    // Also check for .tid file format (base64 content in text field)
    let tid_patterns = [
        "$__favicon.ico.png.tid",
        "$__favicon.ico.tid",
    ];

    for tid_filename in &tid_patterns {
        let tid_path = tiddlers_path.join(tid_filename);
        if let Ok(content) = tokio::fs::read_to_string(&tid_path).await {
            // Parse .tid file - look for text field after blank line
            if let Some(blank_pos) = content.find("\n\n") {
                let text_content = content[blank_pos + 2..].trim();
                if !text_content.is_empty() {
                    // Get type from header
                    let mime_type = if content.contains("type: image/png") {
                        "image/png"
                    } else if content.contains("type: image/jpeg") {
                        "image/jpeg"
                    } else {
                        "image/png"
                    };
                    return Some(format!("data:{};base64,{}", mime_type, text_content));
                }
            }
        }
    }

    None
}

/// Get MIME type from file extension
fn get_mime_type(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()).as_deref() {
        // Images
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("bmp") => "image/bmp",
        Some("tiff") | Some("tif") => "image/tiff",
        // Audio
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("ogg") => "audio/ogg",
        Some("m4a") => "audio/mp4",
        Some("flac") => "audio/flac",
        // Video
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("ogv") => "video/ogg",
        Some("avi") => "video/x-msvideo",
        Some("mov") => "video/quicktime",
        // Documents
        Some("pdf") => "application/pdf",
        Some("json") => "application/json",
        Some("xml") => "application/xml",
        // Text
        Some("txt") => "text/plain",
        Some("html") | Some("htm") => "text/html",
        Some("css") => "text/css",
        Some("js") => "application/javascript",
        Some("csv") => "text/csv",
        // Fonts
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        // Default
        _ => "application/octet-stream",
    }
}

/// Check if a path string looks like an absolute filesystem path
fn is_absolute_filesystem_path(path: &str) -> bool {
    // Unix absolute path
    if path.starts_with('/') {
        return true;
    }
    // Windows absolute path (e.g., C:\, D:\, etc.)
    if path.len() >= 3 {
        let bytes = path.as_bytes();
        if bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && (bytes[2] == b'\\' || bytes[2] == b'/') {
            return true;
        }
    }
    false
}

/// Create a backup of the wiki file before saving
/// If custom_backup_dir is Some, backups go there; otherwise to .backups folder next to wiki
async fn create_backup(path: &PathBuf, custom_backup_dir: Option<&str>) -> Result<(), String> {
    if !path.exists() {
        return Ok(()); // No backup needed for new files
    }

    let parent = path.parent().ok_or("No parent directory")?;
    let filename = path.file_stem().and_then(|s| s.to_str()).unwrap_or("wiki");

    // Determine backup directory
    let backup_dir = if let Some(custom_dir) = custom_backup_dir {
        PathBuf::from(custom_dir)
    } else {
        // Default: .backups folder next to the wiki
        parent.join(format!("{}.backups", filename))
    };

    tokio::fs::create_dir_all(&backup_dir)
        .await
        .map_err(|e| format!("Failed to create backup dir: {}", e))?;

    // Create timestamped backup filename
    let timestamp = Local::now().format("%Y%m%d-%H%M%S");
    let backup_name = format!("{}.{}.html", filename, timestamp);
    let backup_path = backup_dir.join(backup_name);

    // Copy current file to backup
    tokio::fs::copy(path, &backup_path)
        .await
        .map_err(|e| format!("Failed to create backup: {}", e))?;

    // Clean up old backups (keep last 20)
    cleanup_old_backups(&backup_dir, 20).await;

    Ok(())
}

/// Remove old backups, keeping only the most recent ones
async fn cleanup_old_backups(backup_dir: &PathBuf, keep: usize) {
    if let Ok(mut entries) = tokio::fs::read_dir(backup_dir).await {
        let mut backups: Vec<PathBuf> = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().map(|e| e == "html").unwrap_or(false) {
                backups.push(path);
            }
        }

        // Sort by name (which includes timestamp) descending
        backups.sort();
        backups.reverse();

        // Remove old backups
        for old_backup in backups.into_iter().skip(keep) {
            let _ = tokio::fs::remove_file(old_backup).await;
        }
    }
}

/// Load wiki content from disk
#[tauri::command]
async fn load_wiki(_app: tauri::AppHandle, path: String) -> Result<String, String> {
    tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| format!("Failed to read wiki: {}", e))
}

/// Save wiki content to disk with backup
#[tauri::command]
async fn save_wiki(app: tauri::AppHandle, path: String, content: String) -> Result<(), String> {
    let path_buf = PathBuf::from(&path);

    // Check if backups are enabled for this wiki
    let state = app.state::<AppState>();
    if should_create_backup(&app, &state, &path) {
        let backup_dir = get_wiki_backup_dir(&app, &path);
        create_backup(&path_buf, backup_dir.as_deref()).await?;
    }

    // Write to a temp file first, then rename for atomic operation
    let temp_path = path_buf.with_extension("tmp");

    tokio::fs::write(&temp_path, &content)
        .await
        .map_err(|e| format!("Failed to write temp file: {}", e))?;

    // Try rename first, fall back to direct write if it fails (Windows file locking)
    if let Err(_) = tokio::fs::rename(&temp_path, &path_buf).await {
        let _ = tokio::fs::remove_file(&temp_path).await;
        tokio::fs::write(&path_buf, &content)
            .await
            .map_err(|e| format!("Failed to save file: {}", e))?;
    }

    Ok(())
}

/// Set window title (works on Windows/macOS, not Linux due to WebKitGTK limitations)
#[tauri::command]
async fn set_window_title(app: tauri::AppHandle, label: String, title: String) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(&label) {
        window.set_title(&title).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Get current window label
#[tauri::command]
fn get_window_label(window: tauri::Window) -> String {
    window.label().to_string()
}

/// Compare two paths for equality (case-insensitive on Windows)
fn paths_equal(path1: &str, path2: &str) -> bool {
    #[cfg(target_os = "windows")]
    {
        path1.eq_ignore_ascii_case(path2)
    }
    #[cfg(not(target_os = "windows"))]
    {
        path1 == path2
    }
}

/// Check if backups should be created for a wiki path
/// Checks both if it's the main wiki (always no backup) and the user's backups_enabled setting
fn should_create_backup(app: &tauri::AppHandle, state: &AppState, path: &str) -> bool {
    // Don't backup the main TiddlyDesktop wiki
    // Use canonicalized paths for robust comparison (handles symlinks, relative paths, etc.)
    let path_buf = PathBuf::from(path);
    if let (Ok(canonical_path), Ok(canonical_main)) = (
        dunce::canonicalize(&path_buf),
        dunce::canonicalize(&state.main_wiki_path)
    ) {
        if canonical_path == canonical_main {
            return false;
        }
    } else {
        // Fallback to string comparison if canonicalization fails
        let main_wiki = state.main_wiki_path.to_string_lossy();
        if paths_equal(path, &main_wiki) {
            return false;
        }
    }
    // Check if backups are enabled for this wiki in the recent files list
    let entries = load_recent_files_from_disk(app);
    for entry in entries {
        if paths_equal(&entry.path, path) {
            return entry.backups_enabled;
        }
    }
    // Default to enabled for wikis not in the list
    true
}

/// Get custom backup directory for a wiki path (if set)
fn get_wiki_backup_dir(app: &tauri::AppHandle, path: &str) -> Option<String> {
    let entries = load_recent_files_from_disk(app);
    for entry in entries {
        if paths_equal(&entry.path, path) {
            return entry.backup_dir.clone();
        }
    }
    None
}

/// Get path to main wiki file
#[tauri::command]
fn get_main_wiki_path(state: tauri::State<AppState>) -> String {
    state.main_wiki_path.to_string_lossy().to_string()
}

/// Show an alert dialog
#[tauri::command]
async fn show_alert(app: tauri::AppHandle, message: String) -> Result<(), String> {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
    app.dialog()
        .message(message)
        .kind(MessageDialogKind::Info)
        .title("TiddlyWiki")
        .buttons(MessageDialogButtons::Ok)
        .blocking_show();
    Ok(())
}

/// Show a confirm dialog
#[tauri::command]
async fn show_confirm(app: tauri::AppHandle, message: String) -> Result<bool, String> {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
    let result = app.dialog()
        .message(message)
        .kind(MessageDialogKind::Warning)
        .title("TiddlyWiki")
        .buttons(MessageDialogButtons::OkCancel)
        .blocking_show();
    Ok(result)
}

/// Close the current window (used after confirming unsaved changes)
#[tauri::command]
fn close_window(window: tauri::Window) {
    let _ = window.destroy();
}

/// Close a window by its label (used by tm-close-window)
#[tauri::command]
fn close_window_by_label(app: tauri::AppHandle, label: String) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(&label) {
        window.destroy().map_err(|e| e.to_string())?;
        Ok(())
    } else {
        Err(format!("Window '{}' not found", label))
    }
}

/// JavaScript for injecting a custom find bar UI
/// This is used on platforms without native find-in-page UI (Linux, Windows)
const FIND_BAR_JS: &str = r#"
(function() {
    var HIGHLIGHT_CLASS = 'td-find-highlight';
    var CURRENT_CLASS = 'td-find-current';

    // Add highlight styles if not present
    if (!document.getElementById('td-find-styles')) {
        var style = document.createElement('style');
        style.id = 'td-find-styles';
        style.textContent = '.' + HIGHLIGHT_CLASS + '{background:#ffeb3b;color:#000;border-radius:2px;}' +
            '.' + CURRENT_CLASS + '{background:#ff9800;color:#000;box-shadow:0 0 0 2px #ff9800;}';
        document.head.appendChild(style);
    }

    // Check if find bar already exists
    var existingBar = document.getElementById('td-find-bar');
    if (existingBar) {
        existingBar.style.display = 'flex';
        var input = existingBar.querySelector('input');
        if (input) {
            input.focus();
            input.select();
        }
        return;
    }

    // Create find bar
    var bar = document.createElement('div');
    bar.id = 'td-find-bar';
    bar.style.cssText = 'position:fixed;top:0;left:0;right:0;display:flex;align-items:center;gap:8px;padding:8px 12px;background:#f0f0f0;border-bottom:1px solid #ccc;z-index:999999;font-family:system-ui,sans-serif;font-size:14px;box-shadow:0 2px 8px rgba(0,0,0,0.15);';

    var input = document.createElement('input');
    input.type = 'text';
    input.placeholder = 'Find in page...';
    input.style.cssText = 'flex:1;max-width:300px;padding:6px 10px;border:1px solid #ccc;border-radius:4px;font-size:14px;outline:none;';

    var info = document.createElement('span');
    info.style.cssText = 'color:#666;min-width:100px;text-align:center;';
    info.textContent = '';

    var prevBtn = document.createElement('button');
    prevBtn.textContent = '▲';
    prevBtn.title = 'Previous (Shift+F3, Shift+Enter, Ctrl/Cmd+Shift+G)';
    prevBtn.style.cssText = 'padding:4px 10px;border:1px solid #ccc;border-radius:4px;background:#fff;cursor:pointer;font-size:12px;';

    var nextBtn = document.createElement('button');
    nextBtn.textContent = '▼';
    nextBtn.title = 'Next (F3, Enter, Ctrl/Cmd+G)';
    nextBtn.style.cssText = 'padding:4px 10px;border:1px solid #ccc;border-radius:4px;background:#fff;cursor:pointer;font-size:12px;';

    var closeBtn = document.createElement('button');
    closeBtn.textContent = '✕';
    closeBtn.title = 'Close (Escape)';
    closeBtn.style.cssText = 'padding:4px 10px;border:none;background:transparent;cursor:pointer;font-size:16px;color:#666;';

    bar.appendChild(input);
    bar.appendChild(info);
    bar.appendChild(prevBtn);
    bar.appendChild(nextBtn);
    bar.appendChild(closeBtn);
    document.body.appendChild(bar);

    var highlights = [];
    var currentIndex = -1;
    var lastSearch = '';
    var searchTimeout = null;

    function clearHighlights() {
        highlights.forEach(function(span) {
            var parent = span.parentNode;
            if (parent) {
                parent.replaceChild(document.createTextNode(span.textContent), span);
                parent.normalize();
            }
        });
        highlights = [];
        currentIndex = -1;
    }

    function highlightMatches(term) {
        clearHighlights();
        if (!term) {
            info.textContent = '';
            return;
        }

        var termLower = term.toLowerCase();
        var walker = document.createTreeWalker(
            document.body,
            NodeFilter.SHOW_TEXT,
            {
                acceptNode: function(node) {
                    // Skip the find bar itself and script/style elements
                    var parent = node.parentElement;
                    if (!parent) return NodeFilter.FILTER_REJECT;
                    if (parent.closest('#td-find-bar')) return NodeFilter.FILTER_REJECT;
                    var tag = parent.tagName;
                    if (tag === 'SCRIPT' || tag === 'STYLE' || tag === 'NOSCRIPT') {
                        return NodeFilter.FILTER_REJECT;
                    }
                    if (node.textContent.toLowerCase().indexOf(termLower) !== -1) {
                        return NodeFilter.FILTER_ACCEPT;
                    }
                    return NodeFilter.FILTER_REJECT;
                }
            }
        );

        var nodesToProcess = [];
        var textNode;
        while (textNode = walker.nextNode()) {
            nodesToProcess.push(textNode);
        }

        nodesToProcess.forEach(function(node) {
            var text = node.textContent;
            var textLower = text.toLowerCase();
            var idx = 0;
            var lastIdx = 0;
            var frag = document.createDocumentFragment();

            while ((idx = textLower.indexOf(termLower, lastIdx)) !== -1) {
                // Add text before match
                if (idx > lastIdx) {
                    frag.appendChild(document.createTextNode(text.substring(lastIdx, idx)));
                }
                // Add highlighted match
                var span = document.createElement('span');
                span.className = HIGHLIGHT_CLASS;
                span.textContent = text.substring(idx, idx + term.length);
                frag.appendChild(span);
                highlights.push(span);
                lastIdx = idx + term.length;
            }

            // Add remaining text
            if (lastIdx < text.length) {
                frag.appendChild(document.createTextNode(text.substring(lastIdx)));
            }

            node.parentNode.replaceChild(frag, node);
        });

        if (highlights.length > 0) {
            currentIndex = 0;
            updateCurrent();
            info.textContent = '1 of ' + highlights.length;
            info.style.color = '#666';
        } else {
            info.textContent = 'No matches';
            info.style.color = '#c00';
        }
    }

    function updateCurrent() {
        highlights.forEach(function(span, i) {
            if (i === currentIndex) {
                span.classList.add(CURRENT_CLASS);
                span.scrollIntoView({ behavior: 'smooth', block: 'center' });
            } else {
                span.classList.remove(CURRENT_CLASS);
            }
        });
    }

    function goToMatch(delta) {
        if (highlights.length === 0) return;
        currentIndex = (currentIndex + delta + highlights.length) % highlights.length;
        updateCurrent();
        info.textContent = (currentIndex + 1) + ' of ' + highlights.length;
    }

    function doSearch() {
        var term = input.value;
        if (term === lastSearch) return;
        lastSearch = term;
        highlightMatches(term);
    }

    function closeBar() {
        bar.style.display = 'none';
        clearHighlights();
        lastSearch = '';
        info.textContent = '';
        document.removeEventListener('keydown', globalKeyHandler, true);
    }

    function globalKeyHandler(e) {
        if (bar.style.display === 'none') return;

        if (e.key === 'F3') {
            e.preventDefault();
            e.stopPropagation();
            goToMatch(e.shiftKey ? -1 : 1);
            input.focus();
        } else if ((e.key === 'g' || e.key === 'G') && (e.ctrlKey || e.metaKey)) {
            // Ctrl+G / Cmd+G - Find next, Ctrl+Shift+G / Cmd+Shift+G - Find previous
            e.preventDefault();
            e.stopPropagation();
            goToMatch(e.shiftKey ? -1 : 1);
            input.focus();
        } else if (e.key === 'Escape') {
            e.preventDefault();
            e.stopPropagation();
            closeBar();
        } else if ((e.key === 'f' || e.key === 'F') && (e.ctrlKey || e.metaKey)) {
            e.preventDefault();
            e.stopPropagation();
            input.focus();
            input.select();
        }
    }

    document.addEventListener('keydown', globalKeyHandler, true);

    input.addEventListener('input', function() {
        if (searchTimeout) clearTimeout(searchTimeout);
        searchTimeout = setTimeout(doSearch, 200);
    });

    input.addEventListener('keydown', function(e) {
        if (e.key === 'Enter' || e.key === 'F3') {
            e.preventDefault();
            if (searchTimeout) {
                clearTimeout(searchTimeout);
                doSearch();
            }
            goToMatch(e.shiftKey ? -1 : 1);
        } else if (e.key === 'Escape') {
            e.preventDefault();
            closeBar();
        }
    });

    prevBtn.addEventListener('click', function(e) {
        e.preventDefault();
        goToMatch(-1);
        input.focus();
    });

    nextBtn.addEventListener('click', function(e) {
        e.preventDefault();
        goToMatch(1);
        input.focus();
    });

    closeBtn.addEventListener('click', function(e) {
        e.preventDefault();
        closeBar();
    });

    input.focus();
})();
"#;

/// Show the find-in-page UI for the webview
/// Platform-specific implementations:
/// - Windows (WebView2): Injects custom find bar (no native UI)
/// - macOS (WKWebView): Uses performTextFinderAction for native find bar
/// - Linux (WebKitGTK): Injects custom find bar (no native UI)
#[tauri::command]
fn show_find_in_page(window: tauri::WebviewWindow) -> Result<(), String> {
    show_find_in_page_impl(&window)
}

#[cfg(target_os = "windows")]
fn show_find_in_page_impl(window: &tauri::WebviewWindow) -> Result<(), String> {
    // WebView2 doesn't have a built-in find bar UI
    // Inject a custom find bar that uses window.find() API
    let _ = window.eval(FIND_BAR_JS);
    Ok(())
}

#[cfg(target_os = "macos")]
fn show_find_in_page_impl(window: &tauri::WebviewWindow) -> Result<(), String> {
    // Use the same JavaScript find bar as Linux/Windows for consistency
    let _ = window.eval(FIND_BAR_JS);
    Ok(())
}

#[cfg(target_os = "linux")]
fn show_find_in_page_impl(window: &tauri::WebviewWindow) -> Result<(), String> {
    // WebKitGTK doesn't have a built-in find bar UI
    // Inject a custom find bar that uses window.find() API
    let _ = window.eval(FIND_BAR_JS);
    Ok(())
}

/// Result of running a command
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandResult {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// Run a shell command with optional confirmation dialog
/// Security: Shows a confirmation dialog by default to prevent unauthorized execution
#[tauri::command]
async fn run_command(
    app: tauri::AppHandle,
    command: String,
    args: Option<Vec<String>>,
    working_dir: Option<String>,
    wait: Option<bool>,
    confirm: Option<bool>,
) -> Result<Option<CommandResult>, String> {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

    let should_confirm = confirm.unwrap_or(true); // Default to confirming
    let should_wait = wait.unwrap_or(false);
    let args_vec = args.unwrap_or_default();

    // Build the command string for display
    let display_cmd = if args_vec.is_empty() {
        command.clone()
    } else {
        format!("{} {}", command, args_vec.join(" "))
    };

    // Show confirmation dialog if required
    if should_confirm {
        let message = format!(
            "A wiki wants to run the following command:\n\n{}\n\nDo you want to allow this?",
            display_cmd
        );

        let confirmed = app.dialog()
            .message(message)
            .kind(MessageDialogKind::Warning)
            .title("Execute Command")
            .buttons(MessageDialogButtons::OkCancel)
            .blocking_show();

        if !confirmed {
            return Err("Command execution cancelled by user".to_string());
        }
    }

    // Build the command
    let mut cmd = std::process::Command::new(&command);

    if !args_vec.is_empty() {
        cmd.args(&args_vec);
    }

    if let Some(dir) = working_dir {
        cmd.current_dir(dir);
    }

    // On Windows, hide the console window
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }

    if should_wait {
        // Run and wait for output
        let output = cmd.output()
            .map_err(|e| format!("Failed to execute command: {}", e))?;

        Ok(Some(CommandResult {
            success: output.status.success(),
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        }))
    } else {
        // Fire and forget
        cmd.spawn()
            .map_err(|e| format!("Failed to spawn command: {}", e))?;

        Ok(None)
    }
}

// Note: show_prompt is not implemented as a Tauri command because Tauri's dialog plugin
// doesn't have a native text input prompt. The browser's native window.prompt() is used
// instead, which works in the webview. For a better UX, consider implementing a custom
// TiddlyWiki-based modal dialog for text input.

/// JavaScript initialization script - provides confirm modal and close handling for wiki windows
fn get_init_script_with_path(wiki_path: &str) -> String {
    format!(r#"
    window.__WIKI_PATH__ = "{}";
    "#, wiki_path.replace('\\', "\\\\").replace('"', "\\\"")) + get_dialog_init_script()
}

fn get_dialog_init_script() -> &'static str {
    r#"
    (function() {
        console.log('[TiddlyDesktop] Initialization script loaded');
        var promptWrapper = null;
        var confirmationBypassed = false;

        function ensureWrapper() {
            if(!promptWrapper && document.body) {
                promptWrapper = document.createElement('div');
                promptWrapper.className = 'td-confirm-wrapper';
                promptWrapper.style.cssText = 'display:none;position:fixed;top:0;left:0;right:0;bottom:0;background:rgba(0,0,0,0.5);z-index:10000;align-items:center;justify-content:center;';
                document.body.appendChild(promptWrapper);
            }
            return promptWrapper;
        }

        function showConfirmModal(message, callback) {
            var wrapper = ensureWrapper();
            if(!wrapper) {
                if(callback) callback(true);
                return;
            }

            var modal = document.createElement('div');
            modal.style.cssText = 'background:white;padding:20px;border-radius:8px;box-shadow:0 4px 20px rgba(0,0,0,0.3);max-width:400px;text-align:center;';

            var msgP = document.createElement('p');
            msgP.textContent = message;
            msgP.style.cssText = 'margin:0 0 20px 0;font-size:16px;';

            var btnContainer = document.createElement('div');
            btnContainer.style.cssText = 'display:flex;gap:10px;justify-content:center;';

            var cancelBtn = document.createElement('button');
            cancelBtn.textContent = 'Cancel';
            cancelBtn.style.cssText = 'padding:8px 20px;background:#e0e0e0;border:none;border-radius:4px;cursor:pointer;font-size:14px;';
            cancelBtn.onclick = function() {
                wrapper.style.display = 'none';
                wrapper.innerHTML = '';
                if(callback) callback(false);
            };

            var okBtn = document.createElement('button');
            okBtn.textContent = 'OK';
            okBtn.style.cssText = 'padding:8px 20px;background:#4a90d9;color:white;border:none;border-radius:4px;cursor:pointer;font-size:14px;';
            okBtn.onclick = function() {
                wrapper.style.display = 'none';
                wrapper.innerHTML = '';
                if(callback) callback(true);
            };

            btnContainer.appendChild(cancelBtn);
            btnContainer.appendChild(okBtn);
            modal.appendChild(msgP);
            modal.appendChild(btnContainer);
            wrapper.innerHTML = '';
            wrapper.appendChild(modal);
            wrapper.style.display = 'flex';
            okBtn.focus();
        }

        // Our custom confirm function
        var customConfirm = function(message) {
            if(confirmationBypassed) {
                return true;
            }

            var currentEvent = window.event;

            showConfirmModal(message, function(confirmed) {
                if(confirmed && currentEvent && currentEvent.target) {
                    confirmationBypassed = true;
                    try {
                        var target = currentEvent.target;
                        if(typeof target.click === 'function') {
                            target.click();
                        } else {
                            var newEvent = new MouseEvent('click', {
                                bubbles: true,
                                cancelable: true,
                                view: window
                            });
                            target.dispatchEvent(newEvent);
                        }
                    } finally {
                        confirmationBypassed = false;
                    }
                }
            });

            return false;
        };

        // Install the override using Object.defineProperty to prevent it being replaced
        function installConfirmOverride() {
            try {
                Object.defineProperty(window, 'confirm', {
                    value: customConfirm,
                    writable: false,
                    configurable: true
                });
            } catch(e) {
                window.confirm = customConfirm;
            }
        }

        // Install immediately and reinstall after DOM events in case something overwrites it
        installConfirmOverride();
        if(document.readyState === 'loading') {
            document.addEventListener('DOMContentLoaded', installConfirmOverride);
        }
        window.addEventListener('load', installConfirmOverride);

        // Handle window close with unsaved changes check
        function setupCloseHandler() {
            if(typeof window.__TAURI__ === 'undefined' || !window.__TAURI__.event) {
                setTimeout(setupCloseHandler, 100);
                return;
            }

            var getCurrentWindow = window.__TAURI__.window.getCurrentWindow;
            var invoke = window.__TAURI__.core.invoke;
            var appWindow = getCurrentWindow();

            appWindow.onCloseRequested(function(event) {
                // Always prevent close first, then decide what to do
                event.preventDefault();

                // Check if TiddlyWiki has unsaved changes
                var isDirty = false;
                if(typeof $tw !== 'undefined' && $tw.wiki) {
                    if(typeof $tw.wiki.isDirty === 'function') {
                        isDirty = $tw.wiki.isDirty();
                    } else if($tw.saverHandler && typeof $tw.saverHandler.isDirty === 'function') {
                        isDirty = $tw.saverHandler.isDirty();
                    } else if($tw.saverHandler && typeof $tw.saverHandler.numChanges === 'function') {
                        isDirty = $tw.saverHandler.numChanges() > 0;
                    } else if(document.title && document.title.startsWith('*')) {
                        isDirty = true;
                    } else if($tw.syncer && typeof $tw.syncer.isDirty === 'function') {
                        isDirty = $tw.syncer.isDirty();
                    }
                }

                if(isDirty) {
                    showConfirmModal('You have unsaved changes. Are you sure you want to close?', function(confirmed) {
                        if(confirmed) {
                            invoke('close_window');
                        }
                    });
                } else {
                    invoke('close_window');
                }
            });
        }

        setupCloseHandler();

        // Handle absolute filesystem paths via Tauri IPC
        function setupFilesystemSupport() {
            if(typeof window.__TAURI__ === 'undefined' || !window.__TAURI__.core) {
                setTimeout(setupFilesystemSupport, 100);
                return;
            }

            function waitForTiddlyWiki() {
                if(typeof $tw === 'undefined' || !$tw.wiki || !$tw.utils || !$tw.utils.httpRequest) {
                    setTimeout(waitForTiddlyWiki, 100);
                    return;
                }

                var invoke = window.__TAURI__.core.invoke;
                var wikiPath = window.__WIKI_PATH__ || '';

                function isUrl(path) {
                    if(!path || typeof path !== 'string') return false;
                    return path.startsWith('http:') || path.startsWith('https:') ||
                           path.startsWith('data:') || path.startsWith('blob:') ||
                           path.startsWith('file:');
                }

                function isAbsolutePath(path) {
                    if(!path || typeof path !== 'string') return false;
                    // Unix absolute path
                    if(path.startsWith('/')) return true;
                    // Windows absolute path (C:\, D:\, etc.)
                    if(path.length >= 3 && path[1] === ':' && (path[2] === '\\' || path[2] === '/')) return true;
                    return false;
                }

                function isFilesystemPath(path) {
                    if(!path || typeof path !== 'string') return false;
                    if(isUrl(path)) return false;
                    return true; // Either absolute or relative filesystem path
                }

                function normalizePath(path) {
                    // Normalize path by resolving . and .. segments
                    var separator = path.indexOf('\\') >= 0 ? '\\' : '/';
                    var parts = path.split(/[/\\]/);
                    var result = [];
                    for (var i = 0; i < parts.length; i++) {
                        var part = parts[i];
                        if (part === '..') {
                            if (result.length > 0 && result[result.length - 1] !== '') {
                                result.pop();
                            }
                        } else if (part !== '.' && part !== '') {
                            result.push(part);
                        } else if (part === '' && i === 0) {
                            // Keep leading empty string for absolute paths (e.g., /home/...)
                            result.push('');
                        }
                    }
                    return result.join(separator);
                }

                function resolveFilesystemPath(path) {
                    if(isAbsolutePath(path)) {
                        return normalizePath(path);
                    }
                    // Relative path - resolve against wiki path
                    if(!wikiPath) {
                        console.warn('[TiddlyDesktop] Cannot resolve relative path without __WIKI_PATH__:', path);
                        return null;
                    }
                    // Get the directory containing the wiki (for single-file wikis) or the wiki folder itself
                    var basePath = wikiPath;
                    // For single-file wikis, get the parent directory
                    if(basePath.endsWith('.html') || basePath.endsWith('.htm')) {
                        var lastSlash = Math.max(basePath.lastIndexOf('/'), basePath.lastIndexOf('\\'));
                        if(lastSlash > 0) {
                            basePath = basePath.substring(0, lastSlash);
                        }
                    }
                    // Join paths (handle both / and \ separators)
                    var separator = basePath.indexOf('\\') >= 0 ? '\\' : '/';
                    var fullPath = basePath + separator + path.replace(/[/\\]/g, separator);
                    return normalizePath(fullPath);
                }

                // Override httpRequest to support filesystem paths
                var originalHttpRequest = $tw.utils.httpRequest;
                $tw.utils.httpRequest = function(options) {
                    var url = options.url;

                    if(isFilesystemPath(url)) {
                        var resolvedPath = resolveFilesystemPath(url);
                        if(!resolvedPath) {
                            if(options.callback) {
                                options.callback('Cannot resolve path: ' + url, null, {
                                    status: 400, statusText: 'Bad Request',
                                    responseText: '', response: '',
                                    getAllResponseHeaders: function() { return ''; }
                                });
                            }
                            return { abort: function() {} };
                        }

                        invoke('read_file_as_data_uri', { path: resolvedPath })
                            .then(function(dataUri) {
                                var mockXhr = {
                                    status: 200,
                                    statusText: 'OK',
                                    responseText: dataUri,
                                    response: dataUri,
                                    getAllResponseHeaders: function() { return ''; }
                                };
                                if(options.callback) {
                                    options.callback(null, dataUri, mockXhr);
                                }
                            })
                            .catch(function(err) {
                                var mockXhr = {
                                    status: 404,
                                    statusText: 'Not Found',
                                    responseText: '',
                                    response: '',
                                    getAllResponseHeaders: function() { return ''; }
                                };
                                if(options.callback) {
                                    options.callback(err, null, mockXhr);
                                }
                            });
                        return { abort: function() {} };
                    }

                    return originalHttpRequest.call($tw.utils, options);
                };
                console.log('[TiddlyDesktop] httpRequest override installed');

                // Intercept media loading to convert filesystem paths to asset:// URLs
                // TiddlyWiki's parsers set src to _canonical_uri directly for:
                // - <img> (images), <iframe> (PDFs), <audio>, <video>
                // We need to convert those paths to URLs the browser can load
                function setupMediaInterceptor() {
                    if (!window.__TAURI__ || !window.__TAURI__.core || !window.__TAURI__.core.convertFileSrc) {
                        setTimeout(setupMediaInterceptor, 100);
                        return;
                    }

                    var convertFileSrc = window.__TAURI__.core.convertFileSrc;

                    function convertElementSrc(element) {
                        var src = element.getAttribute('src');
                        if (!src) return;

                        // Skip if already converted or is a data URI or web URL
                        if (src.startsWith('asset://') || src.startsWith('data:') ||
                            src.startsWith('http://') || src.startsWith('https://') ||
                            src.startsWith('blob:') || src.startsWith('wikifile://')) {
                            return;
                        }

                        // Check if it's a filesystem path (relative or absolute)
                        var resolvedPath = resolveFilesystemPath(src);
                        if (resolvedPath) {
                            var assetUrl = convertFileSrc(resolvedPath);
                            element.setAttribute('src', assetUrl);
                        }
                    }

                    // Elements that can have src pointing to _canonical_uri
                    var mediaSelectors = 'img, iframe, audio, video, embed, source';

                    // Process existing elements
                    document.querySelectorAll(mediaSelectors).forEach(convertElementSrc);

                    // Watch for new elements being added or src changes
                    var observer = new MutationObserver(function(mutations) {
                        mutations.forEach(function(mutation) {
                            // Handle added nodes
                            mutation.addedNodes.forEach(function(node) {
                                if (node.nodeType === 1) { // Element node
                                    if (node.matches && node.matches(mediaSelectors)) {
                                        convertElementSrc(node);
                                    }
                                    if (node.querySelectorAll) {
                                        node.querySelectorAll(mediaSelectors).forEach(convertElementSrc);
                                    }
                                }
                            });
                            // Handle src attribute changes
                            if (mutation.type === 'attributes' && mutation.attributeName === 'src') {
                                convertElementSrc(mutation.target);
                            }
                        });
                    });

                    observer.observe(document.body, {
                        childList: true,
                        subtree: true,
                        attributes: true,
                        attributeFilter: ['src']
                    });

                    console.log('[TiddlyDesktop] Media interceptor installed');
                }

                setupMediaInterceptor();

                console.log('[TiddlyDesktop] Filesystem support installed');
            }

            waitForTiddlyWiki();
        }

        setupFilesystemSupport();

        // ========================================
        // External Attachments Support
        // ========================================

        // Detect if running on Windows by checking path format
        function isWindowsPath(path) {
            // Windows path patterns: C:\, D:/, \\share
            return /^[A-Za-z]:[\\\/]/.test(path) || path.startsWith("\\\\");
        }

        // Get the native path separator
        function getNativeSeparator(originalPath) {
            // Use the separator from the original path, defaulting to what looks native
            if (originalPath.indexOf("\\") >= 0) return "\\";
            if (isWindowsPath(originalPath)) return "\\";
            return "/";
        }

        // Normalize path to forward slashes for comparison
        function normalizeForComparison(filepath) {
            var path = filepath.replace(/\\/g, "/");
            // For Windows paths like C:/..., don't add leading slash
            // Only add leading slash for Unix paths
            if (path.charAt(0) !== "/" && !isWindowsPath(filepath)) {
                path = "/" + path;
            }
            // Handle network shares (\\share -> /share after backslash conversion)
            if (path.substring(0, 2) === "//") {
                path = path.substring(1);
            }
            return path;
        }

        // Convert normalized path back to native format
        function toNativePath(normalizedPath, useBackslashes) {
            if (useBackslashes) {
                return normalizedPath.replace(/\//g, "\\");
            }
            return normalizedPath;
        }

        function makePathRelative(sourcepath, rootpath, options) {
            options = options || {};

            // Detect if we're dealing with Windows paths
            var isWindows = isWindowsPath(sourcepath) || isWindowsPath(rootpath);
            var nativeSep = isWindows ? "\\" : "/";

            // Normalize paths for comparison (using forward slashes)
            var normalizedSource = normalizeForComparison(sourcepath);
            var normalizedRoot = normalizeForComparison(rootpath);

            var sourceParts = normalizedSource.split("/");
            var rootParts = normalizedRoot.split("/");

            // Don't URL-encode paths - we're dealing with local filesystem paths
            // that our filesystem support handles directly

            var c = 0;
            while (c < sourceParts.length && c < rootParts.length && sourceParts[c] === rootParts[c]) {
                c += 1;
            }

            if (c === 1 ||
                (options.useAbsoluteForNonDescendents && c < rootParts.length) ||
                (options.useAbsoluteForDescendents && c === rootParts.length)) {
                // Return absolute path in native format
                return toNativePath(normalizedSource, isWindows);
            }

            // Build relative path
            var outputParts = [];
            for (var p = c; p < rootParts.length - 1; p++) {
                outputParts.push("..");
            }
            for (p = c; p < sourceParts.length; p++) {
                outputParts.push(sourceParts[p]);
            }
            // Return relative path with native separators
            return outputParts.join(nativeSep);
        }

        function getMimeType(filename) {
            var ext = filename.split(".").pop().toLowerCase();
            var mimeTypes = {
                "png": "image/png", "jpg": "image/jpeg", "jpeg": "image/jpeg",
                "gif": "image/gif", "webp": "image/webp", "svg": "image/svg+xml",
                "ico": "image/x-icon", "bmp": "image/bmp", "pdf": "application/pdf",
                "mp3": "audio/mpeg", "mp4": "video/mp4", "webm": "video/webm",
                "ogg": "audio/ogg", "wav": "audio/wav", "zip": "application/zip"
            };
            return mimeTypes[ext] || "application/octet-stream";
        }

        // Store file paths during drag for the drop event
        var pendingFilePaths = [];

        function createSyntheticDragEvent(type, position, dataTransfer, relatedTarget) {
            // Don't pass dataTransfer to constructor - it may reject non-native DataTransfer objects
            var event = new DragEvent(type, {
                bubbles: true,
                cancelable: true,
                clientX: position ? position.x : 0,
                clientY: position ? position.y : 0,
                relatedTarget: relatedTarget !== undefined ? relatedTarget : null
            });

            // Always set dataTransfer via defineProperty - this works with mock objects
            if (dataTransfer) {
                try {
                    Object.defineProperty(event, 'dataTransfer', {
                        value: dataTransfer,
                        writable: false,
                        configurable: true
                    });
                } catch (e) {
                    console.error("[TiddlyDesktop] Could not set dataTransfer:", e);
                }
            }

            // Mark as synthetic so native handlers can skip it
            event.__tiddlyDesktopSynthetic = true;

            return event;
        }

        var extAttachRetryCount = 0;
        function setupExternalAttachments() {
            extAttachRetryCount++;

            // Log progress: frequently at first, then less often while waiting for encrypted wikis
            var shouldLog = extAttachRetryCount === 1 ||
                (extAttachRetryCount <= 100 && extAttachRetryCount % 10 === 0) ||
                (extAttachRetryCount > 100 && extAttachRetryCount % 60 === 0);  // Every ~60 seconds after initial wait
            if (shouldLog) {
                var msg = "setupExternalAttachments attempt " + extAttachRetryCount +
                    " __TAURI__:" + !!window.__TAURI__ +
                    " __IS_MAIN_WIKI__:" + window.__IS_MAIN_WIKI__ +
                    " __WIKI_PATH__:" + window.__WIKI_PATH__ +
                    " $tw:" + (typeof $tw !== 'undefined' && $tw.wiki ? "ready" : "not ready");
                if (window.__TAURI__ && window.__TAURI__.core) {
                    window.__TAURI__.core.invoke("js_log", { message: msg });
                }
            }

            if (!window.__TAURI__ || !window.__TAURI__.event) {
                // Keep retrying until Tauri is available (should be quick)
                setTimeout(setupExternalAttachments, 100);
                return;
            }

            // Skip main wiki - no file imports there
            if (window.__IS_MAIN_WIKI__) {
                window.__TAURI__.core.invoke("js_log", { message: "Main wiki - external attachments disabled" });
                return;
            }

            // Wait for __WIKI_PATH__ to be set (by protocol handler script)
            if (!window.__WIKI_PATH__) {
                // Keep retrying - wiki path should be set soon after page load
                setTimeout(setupExternalAttachments, 100);
                return;
            }

            // Wait for TiddlyWiki to be ready (no timeout - encrypted wikis may take arbitrarily long)
            // User might not decrypt for minutes or even hours
            if (typeof $tw === 'undefined' || !$tw.wiki) {
                // Use longer interval after initial attempts to reduce CPU usage while waiting
                var interval = extAttachRetryCount < 100 ? 100 : 1000;
                setTimeout(setupExternalAttachments, interval);
                return;
            }

            var listen = window.__TAURI__.event.listen;
            var invoke = window.__TAURI__.core.invoke;
            var wikiPath = window.__WIKI_PATH__;

            var windowLabel = window.__WINDOW_LABEL__ || 'unknown';
            invoke("js_log", { message: "Setting up drag-drop listeners for: " + wikiPath + " window: " + windowLabel });

            // Get element at position - universal, works for any element
            function getTargetElement(position) {
                if (position && position.x !== undefined && position.y !== undefined) {
                    var el = document.elementFromPoint(position.x, position.y);
                    if (el) return el;
                }
                // Fallback to document body
                return document.body;
            }

            // Track state for drag operations
            var enteredTarget = null;
            var currentTarget = null;
            var isDragging = false;

            // Native drag state tracking (Linux GTK, Windows IDropTarget) - declared early so Tauri handlers can check it
            var nativeDragActive = false;
            var nativeDragTarget = null;
            var pendingGtkFileDrop = null;
            var nativeDropInProgress = false;  // Set when drop starts, prevents drag-leave cancellation

            // Helper to create DataTransfer with pending files
            function createDataTransferWithFiles() {
                var dt = new DataTransfer();
                pendingFilePaths.forEach(function(path) {
                    var filename = path.split(/[/\\]/).pop();
                    dt.items.add(new File([""], filename, { type: getMimeType(filename) }));
                });
                return dt;
            }

            // Listen to drag-enter events - start of drag over window
            listen("tauri://drag-enter", function(event) {
                // Skip if native handler is active (Linux GTK, Windows IDropTarget)
                if (nativeDragActive) return;

                var paths = event.payload.paths || [];

                // Skip if this is an internal drag (Tauri detects internal drags as external)
                // Check: all paths are data URLs, or $tw.dragInProgress is set
                var isInternalDrag = (typeof $tw !== "undefined" && $tw.dragInProgress) ||
                    (paths.length > 0 && paths.every(function(p) { return p.startsWith("data:"); }));

                if (isInternalDrag) {
                    return;
                }

                var target = getTargetElement(event.payload.position);
                enteredTarget = target;
                currentTarget = target;

                if (paths.length > 0) {
                    // File drag - we have file paths
                    pendingFilePaths = paths;
                    isDragging = true;

                    var dt = createDataTransferWithFiles();
                    var enterEvent = createSyntheticDragEvent("dragenter", event.payload.position, dt);
                    target.dispatchEvent(enterEvent);
                } else {
                    // Content drag (text, HTML, etc.) - no paths, data captured on drop
                    contentDragActive = true;
                    contentDragTarget = target;
                    contentDragEnterCount = 1; // Reset counter for native dragleave detection

                    // Create DataTransfer with common types as placeholders
                    var dt = createContentDataTransfer();
                    var enterEvent = createSyntheticDragEvent("dragenter", event.payload.position, dt);
                    target.dispatchEvent(enterEvent);
                }
            });

            // Listen to drag-over events - continuous drag over window
            listen("tauri://drag-over", function(event) {
                // Skip if native handler is active (Linux GTK, Windows IDropTarget)
                if (nativeDragActive) return;

                // Skip if not in any external drag mode
                if (!isDragging && !contentDragActive) return;

                // Also skip if $tw.dragInProgress is set (internal drag took over)
                if (typeof $tw !== "undefined" && $tw.dragInProgress) return;

                var target = getTargetElement(event.payload.position);
                var dt = isDragging ? createDataTransferWithFiles() : createContentDataTransfer();

                // If target changed, fire dragleave on old target and dragenter on new target
                if (currentTarget && currentTarget !== target) {
                    // relatedTarget for dragleave is the element being entered
                    var leaveEvent = createSyntheticDragEvent("dragleave", event.payload.position, dt, target);
                    currentTarget.dispatchEvent(leaveEvent);

                    // relatedTarget for dragenter is the element being left
                    var enterEvent = createSyntheticDragEvent("dragenter", event.payload.position, dt, currentTarget);
                    target.dispatchEvent(enterEvent);
                }

                currentTarget = target;
                if (contentDragActive) {
                    contentDragTarget = target;
                }

                // Fire dragover (must be fired continuously and preventDefault called to allow drop)
                var overEvent = createSyntheticDragEvent("dragover", event.payload.position, dt);
                target.dispatchEvent(overEvent);
            });

            // Helper to cancel external drag and clear all dropzone highlights
            function cancelExternalDrag(reason) {
                if (!isDragging) return;

                var dt = createDataTransferWithFiles();

                // Fire dragleave on current target with relatedTarget=null to signal leaving window
                if (currentTarget) {
                    var leaveEvent = createSyntheticDragEvent("dragleave", null, dt, null);
                    currentTarget.dispatchEvent(leaveEvent);
                }

                // Fire dragleave and dragend on ALL elements with tc-dragover class
                // The dragend triggers TiddlyWiki's dropzone resetState() method
                document.querySelectorAll(".tc-dragover").forEach(function(el) {
                    var droppableLeaveEvent = createSyntheticDragEvent("dragleave", null, dt, null);
                    el.dispatchEvent(droppableLeaveEvent);
                    var dropzoneEndEvent = createSyntheticDragEvent("dragend", null, dt, null);
                    el.dispatchEvent(dropzoneEndEvent);
                    el.classList.remove("tc-dragover");
                });

                // Also fire dragend on dropzone elements that might not have tc-dragover yet
                document.querySelectorAll(".tc-dropzone").forEach(function(el) {
                    var dropzoneEndEvent = createSyntheticDragEvent("dragend", null, dt, null);
                    el.dispatchEvent(dropzoneEndEvent);
                });

                // Remove tc-dragging class from any elements that have it
                document.querySelectorAll(".tc-dragging").forEach(function(el) {
                    el.classList.remove("tc-dragging");
                });

                // Fire dragend on body as well
                var endEvent = createSyntheticDragEvent("dragend", null, dt, null);
                document.body.dispatchEvent(endEvent);

                // Reset TiddlyWiki's internal drag state
                if (typeof $tw !== "undefined") {
                    $tw.dragInProgress = null;
                }

                pendingFilePaths = [];
                enteredTarget = null;
                currentTarget = null;
                isDragging = false;
            }

            // Listen to drag-leave events - drag left the window without dropping
            listen("tauri://drag-leave", function(event) {
                // Skip if native handler is active (Linux GTK, Windows IDropTarget)
                if (nativeDragActive) return;

                if (isDragging) {
                    cancelExternalDrag("drag left window");
                } else if (contentDragActive) {
                    cancelContentDrag("drag left window");
                }
            });

            // Helper to convert screen coordinates to client coordinates
            // Used when native handlers send screen coordinates (with screenCoords: true)
            function screenToClient(x, y) {
                // On Windows with DPI scaling, coordinates need adjustment
                var dpr = window.devicePixelRatio || 1;
                return {
                    x: x / dpr - window.screenX,
                    y: y / dpr - window.screenY
                };
            }

            // Native drag-motion event (Linux GTK, Windows IDropTarget)
            // Fires continuously during drag - use to dispatch synthetic dragenter/dragover
            // Handles both file and content drags
            listen("td-drag-motion", function(event) {
                invoke("js_log", { message: "td-drag-motion received at " + (event.payload ? event.payload.x + "," + event.payload.y : "null") });
                if (!event.payload) return;

                var pos;
                if (event.payload.screenCoords) {
                    pos = screenToClient(event.payload.x, event.payload.y);
                } else {
                    pos = { x: event.payload.x, y: event.payload.y };
                }
                var target = getTargetElement(pos);

                // Create DataTransfer with content types (empty values - actual data comes at drop time)
                var dt = new DataTransfer();
                ["text/plain", "text/html", "text/uri-list", "text/vnd.tiddler"].forEach(function(type) {
                    try { dt.setData(type, ""); } catch(e) {}
                });

                if (!nativeDragActive) {
                    // First motion event - dispatch dragenter
                    nativeDragActive = true;
                    nativeDragTarget = target;
                    currentTarget = target;

                    var enterEvent = createSyntheticDragEvent("dragenter", pos, dt);
                    enterEvent.__tiddlyDesktopSynthetic = true;
                    target.dispatchEvent(enterEvent);
                } else {
                    // Subsequent motion - dispatch dragover, and dragenter/dragleave if target changed
                    if (nativeDragTarget && nativeDragTarget !== target) {
                        // Target changed - fire dragleave on old, dragenter on new
                        var leaveEvent = createSyntheticDragEvent("dragleave", pos, dt, target);
                        leaveEvent.__tiddlyDesktopSynthetic = true;
                        nativeDragTarget.dispatchEvent(leaveEvent);

                        var enterEvent = createSyntheticDragEvent("dragenter", pos, dt, nativeDragTarget);
                        enterEvent.__tiddlyDesktopSynthetic = true;
                        target.dispatchEvent(enterEvent);
                    }
                    nativeDragTarget = target;
                    currentTarget = target;
                }

                // Always dispatch dragover to allow drop
                var overEvent = createSyntheticDragEvent("dragover", pos, dt);
                overEvent.__tiddlyDesktopSynthetic = true;
                target.dispatchEvent(overEvent);
            });

            // Native drag-drop-start event (Linux GTK, Windows IDropTarget)
            // Fires immediately when user releases mouse to drop, BEFORE drag-leave
            // This allows us to distinguish "leaving window" from "dropping"
            listen("td-drag-drop-start", function(event) {
                nativeDropInProgress = true;
                if (event.payload) {
                    if (event.payload.screenCoords) {
                        pendingContentDropPos = screenToClient(event.payload.x, event.payload.y);
                    } else {
                        pendingContentDropPos = {
                            x: event.payload.x,
                            y: event.payload.y
                        };
                    }
                }
            });

            // Native drag-leave event (Linux GTK, Windows IDropTarget)
            // Note: Native handlers may fire drag-leave during drop operations too, so we check nativeDropInProgress
            // to avoid canceling when a drop is actually happening.
            listen("td-drag-leave", function(event) {
                // Skip if a drop is in progress
                if (nativeDropInProgress) return;

                // Only cancel if we were tracking a native drag
                if (nativeDragActive) {
                    // Small delay to make sure no drop event is coming
                    setTimeout(function() {
                        // Double-check drop isn't in progress
                        if (nativeDropInProgress) return;
                        if (!nativeDragActive) return;

                        // No drop came - cancel the drag
                        var dt = new DataTransfer();
                        if (nativeDragTarget) {
                            var leaveEvent = createSyntheticDragEvent("dragleave", null, dt, null);
                            leaveEvent.__tiddlyDesktopSynthetic = true;
                            nativeDragTarget.dispatchEvent(leaveEvent);
                        }
                        // Clear dropzone highlights
                        document.querySelectorAll(".tc-dragover").forEach(function(el) {
                            el.classList.remove("tc-dragover");
                            var endEvent = createSyntheticDragEvent("dragend", null, dt, null);
                            endEvent.__tiddlyDesktopSynthetic = true;
                            el.dispatchEvent(endEvent);
                        });
                        nativeDragActive = false;
                        nativeDragTarget = null;
                        nativeDropInProgress = false;
                        currentTarget = null;
                    }, 100);
                }
            });

            // Native drag content received (Linux GTK, Windows IDropTarget)
            // This provides the actual content data from the drag
            listen("td-drag-content", function(event) {
                invoke("js_log", { message: "td-drag-content received! types=" + JSON.stringify(event.payload?.types) });
                // Use document.title to show we received the event (visible indicator)
                var origTitle = document.title;
                document.title = "[TD-CONTENT] " + origTitle;
                setTimeout(function() { document.title = origTitle; }, 2000);

                if (event.payload) {
                    // Store the content data for processing
                    pendingContentDropData = {
                        types: event.payload.types || [],
                        data: event.payload.data || {},
                        files: []
                    };

                    // If we already have position, process the drop now
                    if (pendingContentDropPos) {
                        processContentDrop();
                    }
                }
            }).then(function() {
                invoke("js_log", { message: "td-drag-content listener REGISTERED for window: " + windowLabel });
            }).catch(function(e) {
                invoke("js_log", { message: "td-drag-content listener FAILED: " + e });
            });

            // Native drag drop position (Linux GTK, Windows IDropTarget)
            // Note: Don't check contentDragActive - native events are authoritative
            listen("td-drag-drop-position", function(event) {
                invoke("js_log", { message: "td-drag-drop-position received! x=" + event.payload?.x + " y=" + event.payload?.y });
                // Use document.title to show we received the event (visible indicator)
                var origTitle = document.title;
                document.title = "[TD-POS] " + origTitle;
                setTimeout(function() { document.title = origTitle; }, 2000);

                if (event.payload) {
                    var pos;
                    if (event.payload.screenCoords) {
                        // Windows IDropTarget sends screen coordinates - convert to client
                        pos = screenToClient(event.payload.x, event.payload.y);
                    } else {
                        pos = { x: event.payload.x, y: event.payload.y };
                    }
                    pendingContentDropPos = pos;
                    // Process the content drop now that we have position
                    if (pendingContentDropData) {
                        processContentDrop();
                    }
                }
            }).then(function() {
                invoke("js_log", { message: "td-drag-drop-position listener REGISTERED for window: " + windowLabel });
            }).catch(function(e) {
                invoke("js_log", { message: "td-drag-drop-position listener FAILED: " + e });
            });

            // Native file drop (Linux GTK, Windows IDropTarget)
            // This handles file drags from file managers
            listen("td-file-drop", function(event) {
                if (!event.payload || !event.payload.paths || event.payload.paths.length === 0) return;

                var paths = event.payload.paths;
                pendingGtkFileDrop = paths;

                // Wait for position from td-drag-drop-position
                // Use a small timeout in case position already arrived
                setTimeout(function() {
                    if (!pendingGtkFileDrop) return;
                    processGtkFileDrop();
                }, 10);
            });

            // Process native file drop when we have both paths and position
            function processGtkFileDrop() {
                if (!pendingGtkFileDrop) return;

                var paths = pendingGtkFileDrop;
                var pos = pendingContentDropPos || { x: 100, y: 100 };
                var dropTarget = nativeDragTarget || getTargetElement(pos);

                // Clear pending state
                pendingGtkFileDrop = null;
                pendingContentDropPos = null;

                // Read all files and create File objects for the drop event
                var filePromises = paths.map(function(filepath) {
                    // Skip data URLs and non-file paths
                    if (filepath.startsWith("data:") || (!filepath.startsWith("/") && !filepath.match(/^[A-Za-z]:\\/))) {
                        return Promise.resolve(null);
                    }

                    var filename = filepath.split(/[/\\]/).pop();
                    var mimeType = getMimeType(filename);

                    // Skip wiki files
                    if (filename.toLowerCase().endsWith(".html") || filename.toLowerCase().endsWith(".htm")) {
                        return Promise.resolve(null);
                    }

                    return invoke("read_file_as_binary", { path: filepath }).then(function(bytes) {
                        // Store the path in global map for the import hook to find
                        window.__pendingExternalFiles[filename] = filepath;

                        var file = new File([new Uint8Array(bytes)], filename, { type: mimeType });
                        return file;
                    }).catch(function(err) {
                        console.error("[TiddlyDesktop] Failed to read file:", filepath, err);
                        return null;
                    });
                });

                Promise.all(filePromises).then(function(files) {
                    var validFiles = files.filter(function(f) { return f !== null; });
                    if (validFiles.length === 0) {
                        resetGtkDragState();
                        return;
                    }

                    // Create DataTransfer with actual file content
                    var dt = new DataTransfer();
                    validFiles.forEach(function(file) {
                        dt.items.add(file);
                    });

                    // Fire dragleave on current target first (to unhighlight dropzone)
                    if (nativeDragTarget) {
                        var leaveEvent = createSyntheticDragEvent("dragleave", pos, dt, null);
                        leaveEvent.__tiddlyDesktopSynthetic = true;
                        nativeDragTarget.dispatchEvent(leaveEvent);
                    }

                    // Fire drop event
                    var dropEvent = createSyntheticDragEvent("drop", pos, dt);
                    dropEvent.__tiddlyDesktopSynthetic = true;
                    dropTarget.dispatchEvent(dropEvent);

                    // Fire dragend to signal drag operation completed
                    var endEvent = createSyntheticDragEvent("dragend", pos, dt);
                    endEvent.__tiddlyDesktopSynthetic = true;
                    document.body.dispatchEvent(endEvent);

                    // Clear pending files after a delay (import should be done by then)
                    setTimeout(function() {
                        window.__pendingExternalFiles = {};
                    }, 5000);

                    resetGtkDragState();
                });
            }

            // Reset native drag state
            function resetGtkDragState() {
                nativeDragActive = false;
                nativeDragTarget = null;
                nativeDropInProgress = false;
                currentTarget = null;
                pendingContentDropPos = null;
            }

            // Handle Escape key during external drag (file or browser)
            document.addEventListener("keydown", function(event) {
                if (event.key === "Escape") {
                    if (isDragging) {
                        cancelExternalDrag("escape pressed");
                    } else if (contentDragActive) {
                        cancelContentDrag("escape pressed");
                    }
                }

                // Handle Ctrl+F / Cmd+F for find-in-page
                // Block on main/landing page
                if ((event.key === "f" || event.key === "F") && (event.ctrlKey || event.metaKey)) {
                    if (window.__IS_MAIN_WIKI__) {
                        // Block find on the landing page
                        event.preventDefault();
                        event.stopPropagation();
                    }
                }
            }, true);

            // Bubble-phase handler for find-in-page on wiki windows
            // This runs AFTER editors have had a chance to handle Ctrl+F
            document.addEventListener("keydown", function(event) {
                if ((event.key === "f" || event.key === "F") && (event.ctrlKey || event.metaKey)) {
                    // Skip if this is the main wiki (already blocked in capture phase)
                    if (window.__IS_MAIN_WIKI__) return;

                    // Skip if an editor or other handler already handled this
                    if (event.defaultPrevented) return;

                    // Show native find-in-page UI
                    if (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke) {
                        event.preventDefault();
                        window.__TAURI__.core.invoke('show_find_in_page').catch(function(err) {
                            console.log('[TiddlyDesktop] Find in page error:', err);
                        });
                    }
                }
            }, false); // false = bubble phase

            // Handle window blur during external drag (file or browser)
            window.addEventListener("blur", function(event) {
                if (isDragging) {
                    cancelExternalDrag("window lost focus");
                } else if (contentDragActive) {
                    cancelContentDrag("window lost focus");
                }
            }, true);

            // Native dragenter detection for content drags (Windows/macOS fallback)
            // Tauri drag events may not fire for content drags on these platforms,
            // so we detect content drags directly via native dragenter
            document.addEventListener("dragenter", function(event) {
                // Skip synthetic events and already tracked drags
                if (event.__tiddlyDesktopSynthetic) return;
                if (nativeDragActive || isDragging) return;

                // Check if this is already being tracked
                if (contentDragActive) {
                    contentDragEnterCount++;
                    return;
                }

                // Check if this is a content drag (not file drag)
                var dt = event.dataTransfer;
                if (!dt || !dt.types || dt.types.length === 0) return;

                // Skip internal TiddlyWiki drags
                if (typeof $tw !== "undefined" && $tw.dragInProgress) return;

                // Detect if this is a file drag or content drag
                // File drags have "Files" type, but we need to check if there are actual file items
                var hasFiles = false;
                var hasContent = false;
                var types = [];

                for (var i = 0; i < dt.types.length; i++) {
                    var type = dt.types[i];
                    types.push(type);
                    if (type === "Files") {
                        // Check if there are actual file items
                        if (dt.items && dt.items.length > 0) {
                            for (var j = 0; j < dt.items.length; j++) {
                                if (dt.items[j].kind === "file") {
                                    hasFiles = true;
                                    break;
                                }
                            }
                        }
                    } else if (type === "text/plain" || type === "text/html" || type === "text/uri-list" ||
                               type === "TEXT" || type === "STRING" || type === "UTF8_STRING") {
                        hasContent = true;
                    }
                }

                // If this is a file drag with actual files, let Tauri handle it
                if (hasFiles && !hasContent) return;

                // This is a content drag (or mixed content+file from external source)
                // Start tracking it for Windows/macOS where Tauri events may not fire
                contentDragActive = true;
                contentDragTarget = document.elementFromPoint(event.clientX, event.clientY) || document.body;
                contentDragTypes = types;
                contentDragEnterCount = 1;
                currentTarget = contentDragTarget;

                // IMPORTANT: Call preventDefault on dragenter to indicate we accept the drop
                // Without this, the browser shows "no drop allowed" cursor
                event.preventDefault();

                // Dispatch synthetic dragenter to light up dropzone
                var enterDt = createContentDataTransfer();
                var enterEvent = createSyntheticDragEvent("dragenter", {
                    x: event.clientX,
                    y: event.clientY
                }, enterDt, null);
                enterEvent.__tiddlyDesktopSynthetic = true;
                contentDragTarget.dispatchEvent(enterEvent);

            }, true);

            // Native dragover handler for content drags (Windows/macOS fallback)
            // Dispatches synthetic dragover events to keep dropzone highlighted
            document.addEventListener("dragover", function(event) {
                // Skip synthetic events
                if (event.__tiddlyDesktopSynthetic) return;

                // Only handle for content drags we're tracking natively (not GTK or Tauri)
                if (!contentDragActive || nativeDragActive || isDragging) return;

                // Skip internal TiddlyWiki drags
                if (typeof $tw !== "undefined" && $tw.dragInProgress) return;

                // Prevent default to allow drop
                event.preventDefault();

                // Get the element under the cursor
                var target = document.elementFromPoint(event.clientX, event.clientY) || document.body;

                // Update content drag target
                var oldTarget = contentDragTarget;
                contentDragTarget = target;
                currentTarget = target;

                // Create synthetic dragover event with content types
                var dt = createContentDataTransfer();
                var overEvent = createSyntheticDragEvent("dragover", {
                    x: event.clientX,
                    y: event.clientY
                }, dt, null);
                overEvent.__tiddlyDesktopSynthetic = true;
                target.dispatchEvent(overEvent);

                // If target changed, fire dragleave on old and dragenter on new
                if (oldTarget && oldTarget !== target) {
                    var leaveEvent = createSyntheticDragEvent("dragleave", {
                        x: event.clientX,
                        y: event.clientY
                    }, dt, target);
                    leaveEvent.__tiddlyDesktopSynthetic = true;
                    oldTarget.dispatchEvent(leaveEvent);

                    var enterEvent = createSyntheticDragEvent("dragenter", {
                        x: event.clientX,
                        y: event.clientY
                    }, dt, oldTarget);
                    enterEvent.__tiddlyDesktopSynthetic = true;
                    target.dispatchEvent(enterEvent);
                }
            }, true);

            // Native dragleave detection for content drags
            // Uses enter/leave counter since dragleave fires for every element boundary
            document.addEventListener("dragleave", function(event) {
                // Only track for content drags, skip synthetic events and file drags
                if (!contentDragActive || event.__tiddlyDesktopSynthetic || isDragging) return;
                contentDragEnterCount--;
                if (contentDragEnterCount <= 0) {
                    contentDragEnterCount = 0;
                    cancelContentDrag("drag left window");
                }
            }, true);

            // Global map to store pending file paths for the import hook
            window.__pendingExternalFiles = window.__pendingExternalFiles || {};

            // External content drag state (for drags from browsers, text editors, other apps)
            // Content is captured from native drop event, but drag tracking uses Tauri events
            var pendingContentDropData = null;
            var pendingContentDropPos = null;
            var contentDropTimeout = null;
            var contentDragActive = false;
            var contentDragTarget = null;
            var contentDragTypes = [];
            var contentDragEnterCount = 0;

            // Create DataTransfer with content drag types (empty values until drop)
            function createContentDataTransfer() {
                var dt = new DataTransfer();
                // Use known types if available, otherwise use common types
                // so that TiddlyWiki's dropzone lights up during content drags
                var types = contentDragTypes.length > 0 ? contentDragTypes : [
                    "text/plain",
                    "text/html",
                    "text/uri-list",
                    "text/vnd.tiddler"
                ];
                types.forEach(function(type) {
                    if (type !== "Files") {
                        try {
                            dt.setData(type, "");
                        } catch(e) {}
                    }
                });
                return dt;
            }

            // Helper to cancel content drag and clear all dropzone highlights (mirrors cancelExternalDrag)
            function cancelContentDrag(reason) {
                if (!contentDragActive) return;

                var dt = createContentDataTransfer();

                // Fire dragleave on current target with relatedTarget=null to signal leaving window
                if (currentTarget) {
                    var leaveEvent = createSyntheticDragEvent("dragleave", null, dt, null);
                    leaveEvent.__tiddlyDesktopSynthetic = true;
                    currentTarget.dispatchEvent(leaveEvent);
                }

                // Fire dragleave and dragend on ALL elements with tc-dragover class
                // The dragend triggers TiddlyWiki's dropzone resetState() method
                document.querySelectorAll(".tc-dragover").forEach(function(el) {
                    var droppableLeaveEvent = createSyntheticDragEvent("dragleave", null, dt, null);
                    droppableLeaveEvent.__tiddlyDesktopSynthetic = true;
                    el.dispatchEvent(droppableLeaveEvent);
                    var dropzoneEndEvent = createSyntheticDragEvent("dragend", null, dt, null);
                    dropzoneEndEvent.__tiddlyDesktopSynthetic = true;
                    el.dispatchEvent(dropzoneEndEvent);
                    el.classList.remove("tc-dragover");
                });

                // Also fire dragend on dropzone elements that might not have tc-dragover yet
                document.querySelectorAll(".tc-dropzone").forEach(function(el) {
                    var dropzoneEndEvent = createSyntheticDragEvent("dragend", null, dt, null);
                    dropzoneEndEvent.__tiddlyDesktopSynthetic = true;
                    el.dispatchEvent(dropzoneEndEvent);
                });

                // Remove tc-dragging class from any elements that have it
                document.querySelectorAll(".tc-dragging").forEach(function(el) {
                    el.classList.remove("tc-dragging");
                });

                // Fire dragend on body as well
                var endEvent = createSyntheticDragEvent("dragend", null, dt, null);
                endEvent.__tiddlyDesktopSynthetic = true;
                document.body.dispatchEvent(endEvent);

                // Reset TiddlyWiki's internal drag state
                if (typeof $tw !== "undefined") {
                    $tw.dragInProgress = null;
                }

                // Reset content drag state
                contentDragActive = false;
                contentDragTarget = null;
                contentDragTypes = [];
                contentDragEnterCount = 0;
                enteredTarget = null;
                currentTarget = null;
            }

            // Function to process content drop data (mirrors tauri://drag-drop handling)
            function processContentDrop() {
                if (!pendingContentDropData) {
                    return;
                }

                var capturedData = pendingContentDropData;
                var pos = pendingContentDropPos;

                // Get drop target using same method as file drops
                var dropTarget = getTargetElement(pos);

                // Clear pending data
                pendingContentDropData = null;
                pendingContentDropPos = null;
                if (contentDropTimeout) {
                    clearTimeout(contentDropTimeout);
                    contentDropTimeout = null;
                }

                // Create a pure mock DataTransfer object - not based on real DataTransfer
                // This avoids WebView-specific issues with overriding native DataTransfer methods
                var dataMap = capturedData.data;
                var fileList = capturedData.files.slice();
                var typesList = Object.keys(dataMap);

                // Debug: log what data we received
                invoke("js_log", { message: "processContentDrop - types: " + JSON.stringify(typesList) });
                invoke("js_log", { message: "processContentDrop - has text/html: " + ("text/html" in dataMap) });
                if (dataMap["text/html"]) {
                    invoke("js_log", { message: "processContentDrop - text/html length: " + dataMap["text/html"].length });
                    invoke("js_log", { message: "processContentDrop - text/html preview: " + dataMap["text/html"].substring(0, 200) });
                }

                // Add 'Files' to types if we have files
                if (fileList.length > 0 && typesList.indexOf('Files') === -1) {
                    typesList.push('Files');
                }

                // Build items array with DataTransferItem-like objects
                var itemsArray = [];

                // Add string items for each data type
                typesList.forEach(function(type) {
                    if (type !== 'Files') {
                        itemsArray.push({
                            kind: "string",
                            type: type,
                            getAsString: function(callback) {
                                if (typeof callback === 'function') {
                                    var data = dataMap[type] || "";
                                    setTimeout(function() { callback(data); }, 0);
                                }
                            },
                            getAsFile: function() { return null; }
                        });
                    }
                });

                // Add file items
                fileList.forEach(function(file) {
                    itemsArray.push({
                        kind: "file",
                        type: file.type || "application/octet-stream",
                        getAsString: function(callback) {},
                        getAsFile: function() { return file; }
                    });
                });

                // Add DataTransferItemList methods
                itemsArray.add = function(data, type) {
                    if (data instanceof File) {
                        fileList.push(data);
                        this.push({
                            kind: "file",
                            type: data.type || "application/octet-stream",
                            getAsString: function() {},
                            getAsFile: function() { return data; }
                        });
                    } else if (typeof data === "string" && type) {
                        dataMap[type] = data;
                        if (typesList.indexOf(type) === -1) typesList.push(type);
                        this.push({
                            kind: "string",
                            type: type,
                            getAsString: function(cb) { if (cb) setTimeout(function() { cb(data); }, 0); },
                            getAsFile: function() { return null; }
                        });
                    }
                };
                itemsArray.remove = function(index) { this.splice(index, 1); };
                itemsArray.clear = function() { this.length = 0; };

                // Create the mock DataTransfer object
                var dt = {
                    types: typesList,
                    files: fileList,
                    items: itemsArray,
                    dropEffect: "copy",
                    effectAllowed: "all",
                    getData: function(type) {
                        var result = (type in dataMap) ? dataMap[type] : "";
                        invoke("js_log", { message: "getData('" + type + "') -> " + (result ? "has data (" + result.length + " chars)" : "empty") });
                        if (result && type === "text/html") {
                            // Log char codes to detect encoding issues
                            var codes = [];
                            for (var i = 0; i < Math.min(20, result.length); i++) {
                                codes.push(result.charCodeAt(i));
                            }
                            invoke("js_log", { message: "text/html first 20 char codes: " + JSON.stringify(codes) });
                        }
                        return result;
                    },
                    setData: function(type, value) {
                        dataMap[type] = value;
                        if (typesList.indexOf(type) === -1) {
                            typesList.push(type);
                        }
                    },
                    clearData: function(type) {
                        if (type) {
                            delete dataMap[type];
                            var idx = typesList.indexOf(type);
                            if (idx !== -1) typesList.splice(idx, 1);
                        } else {
                            for (var k in dataMap) delete dataMap[k];
                            typesList.length = 0;
                        }
                    },
                    setDragImage: function() {}
                };

                // Fire dragleave on current target first (to unhighlight dropzone)
                // Use relatedTarget=null to signal drag is ending, not moving to another element
                if (currentTarget) {
                    var leaveEvent = createSyntheticDragEvent("dragleave", pos, dt, null);
                    leaveEvent.__tiddlyDesktopSynthetic = true;
                    currentTarget.dispatchEvent(leaveEvent);
                }

                // Fire drop event
                invoke("js_log", { message: "Dispatching drop event to: " + (dropTarget.tagName || "unknown") + " at " + pos.x + "," + pos.y });
                var dropEvent = createSyntheticDragEvent("drop", pos, dt);
                dropEvent.__tiddlyDesktopSynthetic = true;
                dropTarget.dispatchEvent(dropEvent);
                invoke("js_log", { message: "Drop event dispatched, defaultPrevented=" + dropEvent.defaultPrevented });

                // Fire dragend to signal drag operation completed
                var endEvent = createSyntheticDragEvent("dragend", pos, dt);
                endEvent.__tiddlyDesktopSynthetic = true;
                document.body.dispatchEvent(endEvent);
                invoke("js_log", { message: "Dragend event dispatched" });

                // Reset drag state (mirrors tauri://drag-drop)
                pendingFilePaths = [];
                enteredTarget = null;
                currentTarget = null;
                isDragging = false;
                contentDragActive = false;
                contentDragTarget = null;
                contentDragTypes = [];
                // Reset native drag state
                nativeDragActive = false;
                nativeDragTarget = null;
                nativeDropInProgress = false;
            }

            // Capture native drop event to get DataTransfer content for external content drags
            // This runs in capture phase before any other handlers
            document.addEventListener("drop", function(event) {
                // Skip if this is an internal drag (handled separately)
                // Check window.__tiddlyDesktopDragData which is set during internal drags
                if (window.__tiddlyDesktopDragData || (typeof $tw !== "undefined" && $tw.dragInProgress)) {
                    return;
                }

                // Skip synthetic events we created ourselves
                if (event.__tiddlyDesktopSynthetic) {
                    return;
                }

                // Skip if Tauri is tracking this drag (file system or any external drag)
                // Tauri's tauri://drag-drop will handle it
                if (isDragging) {
                    return;
                }

                // When native handler (IDropTarget on Windows, GTK on Linux) is active,
                // prevent default browser behavior but don't capture data - the native
                // handler will provide real data via td-drag-content event.
                if (nativeDragActive) {
                    event.preventDefault();
                    event.stopPropagation();
                    return;
                }

                var dt = event.dataTransfer;
                if (!dt) return;

                // Capture all data from the DataTransfer before it becomes unavailable
                var capturedData = {
                    types: [],
                    data: {},
                    files: []
                };

                // Capture all data types
                for (var i = 0; i < dt.types.length; i++) {
                    var type = dt.types[i];
                    capturedData.types.push(type);
                    if (type !== "Files") {
                        try {
                            capturedData.data[type] = dt.getData(type);
                        } catch(e) {
                            // Ignore unsupported types
                        }
                    }
                }

                // Capture files
                if (dt.files && dt.files.length > 0) {
                    for (var j = 0; j < dt.files.length; j++) {
                        capturedData.files.push(dt.files[j]);
                    }
                }

                // Store captured content if we're tracking a content drag
                // Skip when nativeDragActive is true - the native handler (IDropTarget on Windows,
                // GTK on Linux) will provide the real data via td-drag-content event.
                // WebView2 cannot access external process clipboard data, so getData() returns empty.
                if (!nativeDragActive && contentDragActive && capturedData.types.length > 0 && (Object.keys(capturedData.data).length > 0 || capturedData.files.length > 0)) {
                    // Additional check: ensure we have actual non-empty content, not just empty strings
                    var hasActualContent = capturedData.files.length > 0 || Object.keys(capturedData.data).some(function(key) {
                        return capturedData.data[key] && capturedData.data[key].length > 0;
                    });
                    if (!hasActualContent) {
                        return; // Don't store empty data
                    }

                    pendingContentDropData = capturedData;
                    pendingContentDropPos = { x: event.clientX, y: event.clientY };

                    // Prevent default and stop propagation
                    event.preventDefault();
                    event.stopPropagation();

                    // On Windows/macOS, tauri://drag-drop may not fire for content drags
                    // Use a short timeout to process the drop directly if Tauri doesn't handle it
                    // Skip this when native handler is active (Linux GTK, Windows IDropTarget)
                    if (!isDragging) {
                        // Clear any existing timeout
                        if (contentDropTimeout) {
                            clearTimeout(contentDropTimeout);
                        }
                        // Set timeout to process drop if tauri://drag-drop doesn't fire
                        contentDropTimeout = setTimeout(function() {
                            if (pendingContentDropData) {
                                processContentDrop();
                            }
                        }, 50);
                    }
                }
            }, true);

            // Listen to drag-drop events - files dropped on window
            listen("tauri://drag-drop", function(event) {
                // Skip if native handler is active (Linux GTK, Windows IDropTarget)
                if (nativeDragActive) return;

                var paths = event.payload.paths || [];

                // Clear content drop timeout since tauri://drag-drop fired
                if (contentDropTimeout) {
                    clearTimeout(contentDropTimeout);
                    contentDropTimeout = null;
                }

                // Skip if this is from an internal drag (internal drags handle their own drop)
                var isInternalDrag = (typeof $tw !== "undefined" && $tw.dragInProgress) ||
                    (paths.length > 0 && paths.every(function(p) { return p.startsWith("data:"); }));

                if (isInternalDrag) {
                    isDragging = false;
                    pendingContentDropData = null;
                    pendingContentDropPos = null;
                    return;
                }

                // Check if we have external content (no file paths but captured data)
                if (paths.length === 0 && pendingContentDropData) {
                    // Use Tauri's position if we don't have one from native drop
                    if (!pendingContentDropPos && event.payload.position) {
                        pendingContentDropPos = event.payload.position;
                    }
                    processContentDrop();
                    return;
                }

                // Content drag but no captured data - reset state
                if (paths.length === 0 && contentDragActive) {
                    contentDragActive = false;
                    contentDragTarget = null;
                    contentDragTypes = [];
                }

                // Clear any pending content data since we're handling file paths
                pendingContentDropData = null;
                pendingContentDropPos = null;

                // No paths and no content data - nothing to do
                if (paths.length === 0) {
                    pendingFilePaths = [];
                    enteredTarget = null;
                    currentTarget = null;
                    isDragging = false;
                    return;
                }

                var dropTarget = getTargetElement(event.payload.position);
                var pos = event.payload.position;

                // Read all files and create File objects for the drop event
                var filePromises = paths.map(function(filepath) {
                    // Skip data URLs and non-file paths (from internal TiddlyWiki drag operations)
                    if (filepath.startsWith("data:") || (!filepath.startsWith("/") && !filepath.match(/^[A-Za-z]:\\/))) {
                        return Promise.resolve(null);
                    }

                    var filename = filepath.split(/[/\\]/).pop();
                    var mimeType = getMimeType(filename);

                    // Skip wiki files
                    if (filename.toLowerCase().endsWith(".html") || filename.toLowerCase().endsWith(".htm")) {
                        return Promise.resolve(null);
                    }

                    return invoke("read_file_as_binary", { path: filepath }).then(function(bytes) {
                        // Store the path in global map for the import hook to find
                        window.__pendingExternalFiles[filename] = filepath;

                        var file = new File([new Uint8Array(bytes)], filename, { type: mimeType });
                        return file;
                    }).catch(function(err) {
                        console.error("[TiddlyDesktop] Failed to read file:", filepath, err);
                        return null;
                    });
                });

                Promise.all(filePromises).then(function(files) {
                    var validFiles = files.filter(function(f) { return f !== null; });
                    if (validFiles.length === 0) return;

                    // Create DataTransfer with actual file content
                    var dt = new DataTransfer();
                    validFiles.forEach(function(file) {
                        dt.items.add(file);
                    });

                    // Fire dragleave on current target first (to unhighlight dropzone)
                    // Use relatedTarget=null to signal drag is ending, not moving to another element
                    if (currentTarget) {
                        var leaveEvent = createSyntheticDragEvent("dragleave", pos, dt, null);
                        currentTarget.dispatchEvent(leaveEvent);
                    }

                    // Fire drop event
                    var dropEvent = createSyntheticDragEvent("drop", pos, dt);
                    dropTarget.dispatchEvent(dropEvent);

                    // Fire dragend to signal drag operation completed
                    var endEvent = createSyntheticDragEvent("dragend", pos, dt);
                    document.body.dispatchEvent(endEvent);

                    // Clear pending files after a delay (import should be done by then)
                    setTimeout(function() {
                        window.__pendingExternalFiles = {};
                    }, 5000);
                });

                pendingFilePaths = [];
                enteredTarget = null;
                currentTarget = null;
                isDragging = false;
            });

            // Intercept file input clicks to use native Tauri dialog
            // This allows us to get the full file path for external attachments
            document.addEventListener('click', function(e) {
                var input = e.target;
                if (input.tagName === 'INPUT' && input.type === 'file') {
                    e.preventDefault();
                    e.stopPropagation();

                    var multiple = input.hasAttribute('multiple');
                    invoke('pick_files_for_import', { multiple: multiple }).then(function(paths) {
                        if (paths.length === 0) return;

                        // Read files and store paths for the import hook
                        var filePromises = paths.map(function(filepath) {
                            // Skip wiki files - they should be opened, not imported
                            var filename = filepath.split(/[/\\]/).pop();
                            if (filename.toLowerCase().endsWith('.html') || filename.toLowerCase().endsWith('.htm')) {
                                return Promise.resolve(null);
                            }

                            // Store the path in global map for the import hook to find
                            window.__pendingExternalFiles[filename] = filepath;

                            return invoke('read_file_as_binary', { path: filepath }).then(function(bytes) {
                                var mimeType = getMimeType(filename);
                                return new File([new Uint8Array(bytes)], filename, { type: mimeType });
                            }).catch(function(err) {
                                console.error('[TiddlyDesktop] Failed to read file:', filepath, err);
                                return null;
                            });
                        });

                        Promise.all(filePromises).then(function(files) {
                            var validFiles = files.filter(function(f) { return f !== null; });
                            if (validFiles.length === 0) return;

                            // Create a DataTransfer to build a FileList
                            var dt = new DataTransfer();
                            validFiles.forEach(function(file) {
                                dt.items.add(file);
                            });

                            // Assign files to the input and trigger change event
                            input.files = dt.files;
                            input.dispatchEvent(new Event('change', { bubbles: true }));

                            // Clear pending files after a delay
                            setTimeout(function() {
                                window.__pendingExternalFiles = {};
                            }, 5000);
                        });
                    }).catch(function(err) {
                        console.error('[TiddlyDesktop] Failed to pick files:', err);
                    });
                }
            }, true); // Use capture phase to intercept before browser opens dialog

            // Config tiddler titles (injected temporarily, stored in Tauri)
            var CONFIG_ENABLE = "$:/config/TiddlyDesktop/ExternalAttachments/Enable";
            var CONFIG_ABS_DESC = "$:/config/TiddlyDesktop/ExternalAttachments/UseAbsoluteForDescendents";
            var CONFIG_ABS_NONDESC = "$:/config/TiddlyDesktop/ExternalAttachments/UseAbsoluteForNonDescendents";
            var CONFIG_SETTINGS_TAB = "$:/plugins/tiddlydesktop/external-attachments/settings";
            var ALL_CONFIG_TIDDLERS = [CONFIG_ENABLE, CONFIG_ABS_DESC, CONFIG_ABS_NONDESC, CONFIG_SETTINGS_TAB];

            // Install TiddlyWiki import hook to handle external attachments
            function installImportHook() {
                if (typeof $tw === 'undefined' || !$tw.hooks) {
                    setTimeout(installImportHook, 100);
                    return;
                }

                $tw.hooks.addHook("th-importing-file", function(info) {
                    var file = info.file;
                    var filename = file.name;

                    // Read config from tiddlers
                    var externalEnabled = $tw.wiki.getTiddlerText(CONFIG_ENABLE, "yes") === "yes";
                    var useAbsDesc = $tw.wiki.getTiddlerText(CONFIG_ABS_DESC, "no") === "yes";
                    var useAbsNonDesc = $tw.wiki.getTiddlerText(CONFIG_ABS_NONDESC, "no") === "yes";

                    // Check if this file is in our pending external files map
                    var originalPath = window.__pendingExternalFiles && window.__pendingExternalFiles[filename];

                    if (originalPath && externalEnabled && info.isBinary) {
                        // Calculate the canonical URI
                        var canonicalUri = makePathRelative(originalPath, wikiPath, {
                            useAbsoluteForDescendents: useAbsDesc,
                            useAbsoluteForNonDescendents: useAbsNonDesc
                        });

                        // Remove from pending map
                        delete window.__pendingExternalFiles[filename];

                        // Call the callback with our external attachment tiddler fields
                        info.callback([
                            {
                                title: filename,
                                type: info.type,
                                "_canonical_uri": canonicalUri
                            }
                        ]);

                        // Return true to prevent default file reading
                        return true;
                    }

                    // Return false to let normal import proceed
                    return false;
                });

                console.log("[TiddlyDesktop] Import hook installed");
            }

            // Read current config from tiddlers and save to Tauri
            function saveConfigToTauri() {
                if (typeof $tw === 'undefined' || !$tw.wiki) return;

                var config = {
                    enabled: $tw.wiki.getTiddlerText(CONFIG_ENABLE, "yes") === "yes",
                    use_absolute_for_descendents: $tw.wiki.getTiddlerText(CONFIG_ABS_DESC, "no") === "yes",
                    use_absolute_for_non_descendents: $tw.wiki.getTiddlerText(CONFIG_ABS_NONDESC, "no") === "yes"
                };

                invoke("set_external_attachments_config", { wikiPath: wikiPath, config: config })
                    .catch(function(err) {
                        console.error("[TiddlyDesktop] Failed to save config:", err);
                    });
            }

            // Delete all injected config tiddlers
            function deleteConfigTiddlers() {
                if (typeof $tw === 'undefined' || !$tw.wiki) return;

                var originalNumChanges = $tw.saverHandler ? $tw.saverHandler.numChanges : 0;

                ALL_CONFIG_TIDDLERS.forEach(function(title) {
                    if ($tw.wiki.tiddlerExists(title)) {
                        $tw.wiki.deleteTiddler(title);
                    }
                });

                // Reset dirty counter so deletions don't trigger save prompt
                if ($tw.saverHandler) {
                    setTimeout(function() {
                        $tw.saverHandler.numChanges = originalNumChanges;
                        $tw.saverHandler.updateDirtyStatus();
                    }, 0);
                }
            }

            // Inject config tiddlers and settings UI
            function injectConfigTiddlers(config) {
                // Wait for TiddlyWiki and saverHandler to be fully initialized
                if (typeof $tw === 'undefined' || !$tw.wiki || !$tw.wiki.addTiddler || !$tw.saverHandler) {
                    setTimeout(function() { injectConfigTiddlers(config); }, 100);
                    return;
                }

                // Store the current dirty count before adding tiddlers
                var originalNumChanges = $tw.saverHandler.numChanges || 0;

                // Inject config tiddlers with values from Tauri
                $tw.wiki.addTiddler(new $tw.Tiddler({
                    title: CONFIG_ENABLE,
                    text: config.enabled ? "yes" : "no"
                }));
                $tw.wiki.addTiddler(new $tw.Tiddler({
                    title: CONFIG_ABS_DESC,
                    text: config.use_absolute_for_descendents ? "yes" : "no"
                }));
                $tw.wiki.addTiddler(new $tw.Tiddler({
                    title: CONFIG_ABS_NONDESC,
                    text: config.use_absolute_for_non_descendents ? "yes" : "no"
                }));

                // Inject settings tab using TiddlyWiki's native checkbox widgets
                $tw.wiki.addTiddler(new $tw.Tiddler({
                    title: CONFIG_SETTINGS_TAB,
                    caption: "External Attachments",
                    tags: "$:/tags/ControlPanel/SettingsTab",
                    text: "When importing binary files (images, PDFs, etc.) into this wiki, you can optionally store them as external references instead of embedding them.\n\n" +
                          "This keeps your wiki file smaller and allows the files to be edited externally.\n\n" +
                          "<$checkbox tiddler=\"" + CONFIG_ENABLE + "\" field=\"text\" checked=\"yes\" unchecked=\"no\" default=\"yes\"> Enable external attachments</$checkbox>\n\n" +
                          "<$checkbox tiddler=\"" + CONFIG_ABS_DESC + "\" field=\"text\" checked=\"yes\" unchecked=\"no\" default=\"no\"> Use absolute paths for files inside wiki folder</$checkbox>\n\n" +
                          "<$checkbox tiddler=\"" + CONFIG_ABS_NONDESC + "\" field=\"text\" checked=\"yes\" unchecked=\"no\" default=\"no\"> Use absolute paths for files outside wiki folder</$checkbox>"
                }));

                // Restore dirty counter after injection
                setTimeout(function() {
                    $tw.saverHandler.numChanges = originalNumChanges;
                    $tw.saverHandler.updateDirtyStatus();
                }, 0);

                // Watch for changes to config tiddlers and save to Tauri
                $tw.wiki.addEventListener("change", function(changes) {
                    if (changes[CONFIG_ENABLE] || changes[CONFIG_ABS_DESC] || changes[CONFIG_ABS_NONDESC]) {
                        saveConfigToTauri();
                    }
                });

                console.log("[TiddlyDesktop] External Attachments settings UI ready");
            }

            // Cleanup on window close: save config and delete tiddlers
            function setupCleanup() {
                window.addEventListener("beforeunload", function() {
                    saveConfigToTauri();
                    deleteConfigTiddlers();
                });

                // Also handle Tauri window close event
                if (window.__TAURI__ && window.__TAURI__.event) {
                    window.__TAURI__.event.listen("tauri://close-requested", function() {
                        saveConfigToTauri();
                        deleteConfigTiddlers();
                    });
                }

            }

            // Load config from Tauri, then inject tiddlers
            invoke("get_external_attachments_config", { wikiPath: wikiPath })
                .then(function(config) {
                    injectConfigTiddlers(config);
                })
                .catch(function(err) {
                    console.error("[TiddlyDesktop] Failed to load config, using defaults:", err);
                    injectConfigTiddlers({ enabled: true, use_absolute_for_descendents: false, use_absolute_for_non_descendents: false });
                });

            installImportHook();
            setupCleanup();

            // Track last clicked element for paste targeting
            // Paste events go to focused element (usually BODY), but we need to know
            // which dropzone the user interacted with
            var lastClickedElement = null;
            document.addEventListener("click", function(event) {
                lastClickedElement = event.target;
            }, true);

            // Paste event handler - intercept paste and use native clipboard reading
            // This ensures proper encoding handling (e.g., UTF-16LE from Firefox on Linux)
            document.addEventListener("paste", function(event) {
                // Skip if in a text input or contenteditable
                var target = event.target;
                if (target.tagName === "TEXTAREA" || target.tagName === "INPUT" || target.isContentEditable) {
                    return; // Let native paste work in text fields
                }

                // Skip if TiddlyWiki editor is handling it
                if (event.twEditor) {
                    return;
                }

                // Skip our own synthetic paste events
                if (event.__tiddlyDesktopSynthetic) {
                    return;
                }

                // Prevent default and handle via native clipboard
                event.preventDefault();
                event.stopPropagation();

                invoke("js_log", { message: "Paste event intercepted, reading native clipboard" });

                // Read clipboard content via Rust (handles encoding properly)
                invoke("get_clipboard_content").then(function(clipboardData) {
                    if (!clipboardData || !clipboardData.types || clipboardData.types.length === 0) {
                        invoke("js_log", { message: "Clipboard is empty or unreadable" });
                        return;
                    }

                    invoke("js_log", { message: "Clipboard content types: " + JSON.stringify(clipboardData.types) });

                    // Create a mock ClipboardData/DataTransfer for the synthetic paste event
                    var dataMap = clipboardData.data || {};
                    var typesList = clipboardData.types || [];

                    // Build items array matching ClipboardItem interface
                    var itemsArray = [];
                    typesList.forEach(function(type) {
                        itemsArray.push({
                            kind: "string",
                            type: type,
                            getAsString: function(callback) {
                                if (typeof callback === "function") {
                                    setTimeout(function() { callback(dataMap[type] || ""); }, 0);
                                }
                            },
                            getAsFile: function() { return null; }
                        });
                    });

                    // Create mock clipboardData object
                    var mockClipboardData = {
                        types: typesList,
                        items: itemsArray,
                        getData: function(type) {
                            return dataMap[type] || "";
                        },
                        setData: function() {},
                        clearData: function() {}
                    };

                    // Create synthetic paste event
                    var syntheticPaste = new ClipboardEvent("paste", {
                        bubbles: true,
                        cancelable: true,
                        composed: true
                    });

                    // Override clipboardData (readonly in real events, but we can define it on our object)
                    Object.defineProperty(syntheticPaste, "clipboardData", {
                        value: mockClipboardData,
                        writable: false
                    });

                    // Mark as our synthetic event
                    syntheticPaste.__tiddlyDesktopSynthetic = true;

                    // Use the last clicked element to determine which dropzone to target
                    // This is more precise than focus state since divs aren't focusable
                    var pasteTarget = lastClickedElement || target;
                    var dropzone = pasteTarget.closest ? pasteTarget.closest(".tc-dropzone") : null;

                    if (dropzone) {
                        // Last click was inside a dropzone - dispatch there, it will bubble up
                        invoke("js_log", { message: "Dispatching synthetic paste to: " + pasteTarget.tagName + " (inside dropzone)" });
                        pasteTarget.dispatchEvent(syntheticPaste);
                        invoke("js_log", { message: "Synthetic paste dispatched, defaultPrevented=" + syntheticPaste.defaultPrevented });
                    } else {
                        // Last click was not inside any dropzone
                        // Paste import only works when user clicked inside a dropzone area
                        invoke("js_log", { message: "Last clicked element (" + (pasteTarget ? pasteTarget.tagName : "none") + ") is not inside a dropzone - no import" });
                    }
                }).catch(function(err) {
                    invoke("js_log", { message: "Failed to read clipboard: " + err });
                });
            }, true); // Use capture phase to intercept before TiddlyWiki

            console.log("[TiddlyDesktop] External attachments ready for:", wikiPath);
        }

        setupExternalAttachments();

        // ========================================
        // Session Authentication Support
        // ========================================
        // Allows users to authenticate with external services (SharePoint, etc.)
        // and have the session cookies stored in the wiki's isolated session

        function setupSessionAuthentication() {
            if (window.__IS_MAIN_WIKI__) {
                console.log('[TiddlyDesktop] Main wiki - session authentication disabled');
                return;
            }

            var wikiPath = window.__WIKI_PATH__;
            var twReady = (typeof $tw !== "undefined") && $tw && $tw.wiki;
            if (!wikiPath || !twReady) {
                // Wiki not ready yet - retry (path injected via protocol handler, $tw needs boot)
                if (!window.__sessionAuthRetryCount) window.__sessionAuthRetryCount = 0;
                window.__sessionAuthRetryCount++;
                // Retry for up to 60 seconds (600 × 100ms) to handle very large wikis
                if (window.__sessionAuthRetryCount < 600) {
                    setTimeout(setupSessionAuthentication, 100);
                } else {
                    console.log('[TiddlyDesktop] Wiki not ready after 60s - session authentication disabled');
                }
                return;
            }

            var CONFIG_SETTINGS_TAB = "$:/plugins/tiddlydesktop/session-auth/settings";
            var CONFIG_AUTH_URLS = "$:/temp/tiddlydesktop-rs/session-auth/urls";
            var invoke = window.__TAURI__.core.invoke;

            function saveConfigToTauri() {
                // Collect all auth URL tiddlers
                var authUrls = [];
                $tw.wiki.filterTiddlers("[prefix[$:/temp/tiddlydesktop-rs/session-auth/url/]]").forEach(function(title) {
                    var tiddler = $tw.wiki.getTiddler(title);
                    if (tiddler) {
                        authUrls.push({
                            name: tiddler.fields.name || "",
                            url: tiddler.fields.url || ""
                        });
                    }
                });
                invoke("set_session_auth_config", {
                    wikiPath: wikiPath,
                    config: { auth_urls: authUrls }
                }).catch(function(err) {
                    console.error("[TiddlyDesktop] Failed to save session auth config:", err);
                });
            }

            function deleteConfigTiddlers() {
                $tw.wiki.filterTiddlers("[prefix[$:/temp/tiddlydesktop-rs/session-auth/]]").forEach(function(title) {
                    $tw.wiki.deleteTiddler(title);
                });
                $tw.wiki.deleteTiddler(CONFIG_SETTINGS_TAB);
            }

            function refreshUrlList() {
                // Count existing URLs for display
                var count = $tw.wiki.filterTiddlers("[prefix[$:/temp/tiddlydesktop-rs/session-auth/url/]]").length;
                $tw.wiki.setText(CONFIG_AUTH_URLS, "text", null, String(count));
            }

            function injectConfigTiddlers(config) {
                var originalNumChanges = $tw.saverHandler ? $tw.saverHandler.numChanges : 0;

                // Add auth URL entries
                if (config.auth_urls) {
                    config.auth_urls.forEach(function(entry, index) {
                        $tw.wiki.addTiddler(new $tw.Tiddler({
                            title: "$:/temp/tiddlydesktop-rs/session-auth/url/" + index,
                            name: entry.name,
                            url: entry.url,
                            text: ""
                        }));
                    });
                }

                // Inject settings tab with dynamic URL list
                var tabText = "Authenticate with external services to access protected resources (like SharePoint profile images).\n\n" +
                    "Session cookies will be stored in this wiki's isolated session data.\n\n" +
                    "!! Authentication URLs\n\n" +
                    "<$list filter=\"[prefix[$:/temp/tiddlydesktop-rs/session-auth/url/]]\" variable=\"urlTiddler\">\n" +
                    "<div style=\"display:flex;align-items:center;gap:8px;margin-bottom:8px;padding:8px;background:#f8f8f8;border-radius:4px;\">\n" +
                    "<div style=\"flex:1;\">\n" +
                    "<strong><$text text={{$(urlTiddler)$!!name}}/></strong><br/>\n" +
                    "<small><$text text={{$(urlTiddler)$!!url}}/></small>\n" +
                    "</div>\n" +
                    "<$button class=\"tc-btn-invisible tc-tiddlylink\" message=\"tm-tiddlydesktop-open-auth-url\" param=<<urlTiddler>> tooltip=\"Open login window\">\n" +
                    "{{$:/core/images/external-link}} Login\n" +
                    "</$button>\n" +
                    "<$button class=\"tc-btn-invisible tc-tiddlylink\" message=\"tm-tiddlydesktop-remove-auth-url\" param=<<urlTiddler>> tooltip=\"Remove this URL\">\n" +
                    "{{$:/core/images/delete-button}}\n" +
                    "</$button>\n" +
                    "</div>\n" +
                    "</$list>\n\n" +
                    "<$list filter=\"[prefix[$:/temp/tiddlydesktop-rs/session-auth/url/]count[]match[0]]\" variable=\"ignore\">\n" +
                    "//No authentication URLs configured.//\n\n" +
                    "</$list>\n" +
                    "!! Add New URL\n\n" +
                    "<$edit-text tiddler=\"$:/temp/tiddlydesktop-rs/session-auth/new-name\" tag=\"input\" placeholder=\"Name (e.g. SharePoint)\" default=\"\" class=\"tc-edit-texteditor\" style=\"width:100%;margin-bottom:4px;\"/>\n\n" +
                    "<$edit-text tiddler=\"$:/temp/tiddlydesktop-rs/session-auth/new-url\" tag=\"input\" placeholder=\"URL (e.g. https://company.sharepoint.com)\" default=\"\" class=\"tc-edit-texteditor\" style=\"width:100%;margin-bottom:8px;\"/>\n\n" +
                    "<$button message=\"tm-tiddlydesktop-add-auth-url\" class=\"tc-btn-big-green\">Add URL</$button>\n";

                $tw.wiki.addTiddler(new $tw.Tiddler({
                    title: CONFIG_SETTINGS_TAB,
                    caption: "Session Auth",
                    tags: "$:/tags/ControlPanel/SettingsTab",
                    text: tabText
                }));

                // Restore dirty counter
                setTimeout(function() {
                    if ($tw.saverHandler) {
                        $tw.saverHandler.numChanges = originalNumChanges;
                        $tw.saverHandler.updateDirtyStatus();
                    }
                }, 0);

                refreshUrlList();
                console.log("[TiddlyDesktop] Session Authentication settings UI ready");
            }

            // Message handler: add new auth URL
            $tw.rootWidget.addEventListener("tm-tiddlydesktop-add-auth-url", function(event) {
                var name = $tw.wiki.getTiddlerText("$:/temp/tiddlydesktop-rs/session-auth/new-name", "").trim();
                var url = $tw.wiki.getTiddlerText("$:/temp/tiddlydesktop-rs/session-auth/new-url", "").trim();

                if (!name || !url) {
                    alert("Please enter both a name and URL");
                    return;
                }

                // Validate URL
                var parsedUrl;
                try {
                    parsedUrl = new URL(url);
                } catch (e) {
                    alert("Please enter a valid URL");
                    return;
                }

                // Security: Only allow HTTPS (except localhost for development)
                var isHttps = parsedUrl.protocol === "https:";
                var isLocalhost = parsedUrl.hostname === "localhost" ||
                                  parsedUrl.hostname === "127.0.0.1" ||
                                  parsedUrl.hostname === "::1";
                var isLocalhostHttp = parsedUrl.protocol === "http:" && isLocalhost;

                if (!isHttps && !isLocalhostHttp) {
                    alert("Security: Only HTTPS URLs are allowed for authentication (except localhost)");
                    return;
                }

                // Find next available index
                var existingUrls = $tw.wiki.filterTiddlers("[prefix[$:/temp/tiddlydesktop-rs/session-auth/url/]]");
                var nextIndex = existingUrls.length;

                // Add the new URL tiddler
                $tw.wiki.addTiddler(new $tw.Tiddler({
                    title: "$:/temp/tiddlydesktop-rs/session-auth/url/" + nextIndex,
                    name: name,
                    url: url,
                    text: ""
                }));

                // Clear input fields
                $tw.wiki.deleteTiddler("$:/temp/tiddlydesktop-rs/session-auth/new-name");
                $tw.wiki.deleteTiddler("$:/temp/tiddlydesktop-rs/session-auth/new-url");

                // Save to Tauri
                saveConfigToTauri();
                refreshUrlList();
            });

            // Message handler: remove auth URL
            $tw.rootWidget.addEventListener("tm-tiddlydesktop-remove-auth-url", function(event) {
                var tiddlerTitle = event.param;
                if (tiddlerTitle) {
                    $tw.wiki.deleteTiddler(tiddlerTitle);
                    saveConfigToTauri();
                    refreshUrlList();
                }
            });

            // Message handler: open auth URL in new window
            $tw.rootWidget.addEventListener("tm-tiddlydesktop-open-auth-url", function(event) {
                var tiddlerTitle = event.param;
                if (tiddlerTitle) {
                    var tiddler = $tw.wiki.getTiddler(tiddlerTitle);
                    if (tiddler) {
                        var name = tiddler.fields.name || "Authentication";
                        var url = tiddler.fields.url;
                        if (url) {
                            invoke("open_auth_window", {
                                wikiPath: wikiPath,
                                url: url,
                                name: name
                            }).catch(function(err) {
                                console.error("[TiddlyDesktop] Failed to open auth window:", err);
                                alert("Failed to open authentication window: " + err);
                            });
                        }
                    }
                }
            });

            // Load config from Tauri
            invoke("get_session_auth_config", { wikiPath: wikiPath })
                .then(function(config) {
                    injectConfigTiddlers(config);
                })
                .catch(function(err) {
                    console.error("[TiddlyDesktop] Failed to load session auth config, using defaults:", err);
                    injectConfigTiddlers({ auth_urls: [] });
                });

            // Cleanup on window close
            window.addEventListener("beforeunload", function() {
                saveConfigToTauri();
                deleteConfigTiddlers();
            });

            console.log("[TiddlyDesktop] Session authentication ready for:", wikiPath);
        }

        setupSessionAuthentication();

        // Internal drag-and-drop polyfill for WebKitGTK (Linux only)
        // Native HTML5 drag-and-drop has issues in WebKitGTK but works fine in WebView2 (Windows) and WKWebView (macOS)
        (function setupInternalDragPolyfill() {
            // Only run on Linux where WebKitGTK has drag issues
            // Windows (WebView2) and macOS (WKWebView) work fine with native drags
            if (!/Linux/.test(navigator.userAgent)) {
                console.log("[TiddlyDesktop] Skipping internal drag polyfill on non-Linux platform");
                return;
            }

            // Store drag data globally since dataTransfer may not work reliably
            window.__tiddlyDesktopDragData = null;
            var internalDragSource = null;
            var internalDragImage = null;
            var internalDragActive = false;
            var dragImageOffsetX = 0;
            var dragImageOffsetY = 0;

            // Create a drag image element that follows the cursor
            // Extract background color from element or its ancestors
            function getBackgroundColor(element) {
                var el = element;
                while (el) {
                    var style = window.getComputedStyle(el);
                    var bg = style.backgroundColor;
                    // Check if background is not transparent
                    if (bg && bg !== "transparent" && bg !== "rgba(0, 0, 0, 0)") {
                        return bg;
                    }
                    el = el.parentElement;
                }
                // Fallback to CSS variable or white
                return "var(--tiddler-background, white)";
            }

            // Track if setDragImage was called with a blank/empty element
            var dragImageIsBlank = false;

            function createDragImage(sourceElement, clientX, clientY) {
                // Remove any existing drag image
                if (internalDragImage && internalDragImage.parentNode) {
                    internalDragImage.parentNode.removeChild(internalDragImage);
                }

                // Check if TiddlyWiki requested a blank drag image via setDragImage
                if (dragImageIsBlank) {
                    return null;
                }

                // Clone the element for the drag image
                var clone = sourceElement.cloneNode(true);
                clone.style.position = "fixed";
                clone.style.pointerEvents = "none";
                clone.style.zIndex = "999999";
                clone.style.opacity = "0.7";
                clone.style.transform = "scale(0.9)";
                clone.style.maxWidth = "300px";
                clone.style.maxHeight = "100px";
                clone.style.overflow = "hidden";
                clone.style.whiteSpace = "nowrap";
                clone.style.textOverflow = "ellipsis";
                clone.style.background = getBackgroundColor(sourceElement);
                clone.style.padding = "4px 8px";
                clone.style.borderRadius = "4px";
                clone.style.boxShadow = "0 2px 8px rgba(0,0,0,0.3)";

                // Calculate offset from mouse to element corner
                var rect = sourceElement.getBoundingClientRect();
                dragImageOffsetX = clientX - rect.left;
                dragImageOffsetY = clientY - rect.top;

                // Position at cursor
                clone.style.left = (clientX - dragImageOffsetX) + "px";
                clone.style.top = (clientY - dragImageOffsetY) + "px";

                document.body.appendChild(clone);
                internalDragImage = clone;
                return clone;
            }

            // Update drag image position
            function updateDragImagePosition(clientX, clientY) {
                if (internalDragImage) {
                    internalDragImage.style.left = (clientX - dragImageOffsetX) + "px";
                    internalDragImage.style.top = (clientY - dragImageOffsetY) + "px";
                }
            }

            // Remove drag image
            function removeDragImage() {
                if (internalDragImage && internalDragImage.parentNode) {
                    internalDragImage.parentNode.removeChild(internalDragImage);
                    internalDragImage = null;
                }
            }

            // Patch DataTransfer.prototype.setData to capture data as it's set
            // This is needed because getData() is restricted during dragstart
            var originalSetData = DataTransfer.prototype.setData;
            DataTransfer.prototype.setData = function(type, data) {
                // Store in our global cache
                if (!window.__tiddlyDesktopDragData) {
                    window.__tiddlyDesktopDragData = {};
                }
                window.__tiddlyDesktopDragData[type] = data;
                // Call original
                return originalSetData.call(this, type, data);
            };

            // Also patch getData to use our cache as fallback
            var originalGetData = DataTransfer.prototype.getData;
            DataTransfer.prototype.getData = function(type) {
                var result = originalGetData.call(this, type);
                if (!result && window.__tiddlyDesktopDragData && window.__tiddlyDesktopDragData[type]) {
                    return window.__tiddlyDesktopDragData[type];
                }
                return result;
            };

            // Store data when drag starts - capture phase to run before TiddlyWiki's handler
            document.addEventListener("dragstart", function(event) {
                // Skip synthetic events that we dispatched ourselves
                if (event.__tiddlyDesktopSynthetic) {
                    return;
                }

                var target = event.target;

                // Handle text nodes (e.g., when dragging selected text)
                // Text nodes don't have getAttribute/classList, so get the parent element
                if (target && target.nodeType !== 1) {
                    target = target.parentElement;
                }
                if (!target) return;

                // Only handle draggable elements: explicit draggable="true", tc-draggable class, or tc-tiddlylink (tiddler links)
                if (target.getAttribute("draggable") !== "true" && !target.classList.contains("tc-draggable") && !target.classList.contains("tc-tiddlylink")) {
                    // Check if any ancestor is draggable
                    target = target.closest("[draggable='true'], .tc-draggable, .tc-tiddlylink");
                    if (!target) return;
                }

                // Native drag in WebKitGTK is unreliable - cancel and use synthetic drag for all elements
                event.preventDefault();

                // Immediately start our synthetic drag (don't wait for mouse movement threshold)
                mouseDragStarted = true;
                internalDragActive = true;
                internalDragSource = target;
                mouseDownTarget = target;

                // Disable text selection during drag
                document.body.style.userSelect = "none";
                document.body.style.webkitUserSelect = "none";

                // Create and dispatch synthetic dragstart with fresh DataTransfer
                window.__tiddlyDesktopDragData = {};
                dragImageIsBlank = false;
                mouseDragDataTransfer = new DataTransfer();

                // Patch setDragImage to detect blank drag images
                var originalSetDragImage = mouseDragDataTransfer.setDragImage;
                mouseDragDataTransfer.setDragImage = function(element, x, y) {
                    // Detect if TiddlyWiki is setting a blank drag image
                    // TiddlyWiki uses an empty div (no children) for blank drag images
                    if (element && (!element.firstChild || element.offsetWidth === 0 || element.offsetHeight === 0)) {
                        dragImageIsBlank = true;
                    }
                    if (originalSetDragImage) {
                        originalSetDragImage.call(this, element, x, y);
                    }
                };

                var syntheticDragStart = createSyntheticDragEvent("dragstart", {
                    clientX: event.clientX,
                    clientY: event.clientY
                }, mouseDragDataTransfer);
                syntheticDragStart.__tiddlyDesktopSynthetic = true;

                target.dispatchEvent(syntheticDragStart);

                // Capture effectAllowed that TiddlyWiki set during dragstart
                window.__tiddlyDesktopEffectAllowed = mouseDragDataTransfer.effectAllowed || "all";

                // Create drag image that follows cursor (unless TiddlyWiki requested blank)
                createDragImage(target, event.clientX, event.clientY);

                // Fire initial dragenter on current element
                var enterTarget = document.elementFromPoint(event.clientX, event.clientY);
                if (enterTarget) {
                    var enterEvent = createSyntheticDragEvent("dragenter", {
                        clientX: event.clientX,
                        clientY: event.clientY,
                        relatedTarget: null
                    }, mouseDragDataTransfer);
                    enterTarget.dispatchEvent(enterEvent);
                    lastDragOverTarget = enterTarget;
                }
            }, true);

            // Clear drag data cache when starting a new drag
            document.addEventListener("dragstart", function(event) {
                // Reset the cache at the start of each drag operation
                // The setData patch will populate it as TiddlyWiki sets data
                window.__tiddlyDesktopDragData = {};
            }, true);

            // Enhance dragover to ensure drop is allowed
            document.addEventListener("dragover", function(event) {
                // Skip synthetic events (already handled)
                if (event.__tiddlyDesktopSynthetic) {
                    return;
                }
                if (internalDragActive) {
                    // Ensure drop is allowed for internal drags
                    event.preventDefault();
                    if (event.dataTransfer) {
                        // Use the effectAllowed set during dragstart, map to appropriate dropEffect
                        var effect = window.__tiddlyDesktopEffectAllowed || "all";
                        if (effect === "copyMove" || effect === "all") {
                            event.dataTransfer.dropEffect = "move";
                        } else if (effect === "copy" || effect === "copyLink") {
                            event.dataTransfer.dropEffect = "copy";
                        } else if (effect === "link" || effect === "linkMove") {
                            event.dataTransfer.dropEffect = "link";
                        } else if (effect === "move") {
                            event.dataTransfer.dropEffect = "move";
                        } else {
                            event.dataTransfer.dropEffect = "move";
                        }
                    }
                }
                // Note: External browser drags are now handled by the contentDragActive handlers above
            }, true);

            // Clean up when drag ends
            document.addEventListener("dragend", function(event) {
                window.__tiddlyDesktopDragData = null;
                window.__tiddlyDesktopEffectAllowed = null;
                internalDragSource = null;
                internalDragActive = false;
                removeDragImage();
                // Restore text selection
                document.body.style.userSelect = "";
                document.body.style.webkitUserSelect = "";
            }, true);

            // Helper to create synthetic drag events with proper dataTransfer
            // WebKitGTK may not set dataTransfer from DragEvent constructor
            function createSyntheticDragEvent(type, options, dataTransfer) {
                // Don't pass dataTransfer to constructor - it may reject non-native objects
                var event = new DragEvent(type, Object.assign({
                    bubbles: true,
                    cancelable: true
                }, options));

                // Always set dataTransfer via defineProperty
                if (dataTransfer) {
                    Object.defineProperty(event, 'dataTransfer', {
                        value: dataTransfer,
                        writable: false,
                        configurable: true
                    });
                }

                return event;
            }

            // Fallback: If native drag events don't fire or are unreliable, use mouse events
            // WebKitGTK has issues with drag events on non-anchor elements
            var mouseDownTarget = null;
            var mouseDownPos = null;
            var mouseDragStarted = false;
            var mouseDragDataTransfer = null;
            var DRAG_THRESHOLD = 3; // Small threshold for responsive feel

            document.addEventListener("mousedown", function(event) {
                // Find draggable element - check attribute, tc-draggable class, or tc-tiddlylink
                var target = event.target.closest("[draggable='true'], .tc-draggable, .tc-tiddlylink");
                if (target && event.button === 0) {
                    mouseDownTarget = target;
                    mouseDownPos = { x: event.clientX, y: event.clientY };
                    mouseDragStarted = false;
                }
            }, true);

            // Fallback: if native dragstart didn't fire (edge cases), use mouse movement threshold
            document.addEventListener("mousemove", function(event) {
                if (!mouseDownTarget || mouseDragStarted) return;

                // If drag already started via native handler, skip
                if (internalDragActive) {
                    mouseDownTarget = null;
                    return;
                }

                // Check if we've moved enough to start a drag
                var dx = event.clientX - mouseDownPos.x;
                var dy = event.clientY - mouseDownPos.y;
                if (Math.abs(dx) < DRAG_THRESHOLD && Math.abs(dy) < DRAG_THRESHOLD) return;

                // Native dragstart didn't fire - synthesize drag events as fallback
                mouseDragStarted = true;
                internalDragActive = true;
                internalDragSource = mouseDownTarget;

                // Disable text selection during drag
                document.body.style.userSelect = "none";
                document.body.style.webkitUserSelect = "none";

                // Create synthetic dragstart with a DataTransfer
                window.__tiddlyDesktopDragData = {};
                dragImageIsBlank = false;
                mouseDragDataTransfer = new DataTransfer();

                // Patch setDragImage to detect blank drag images
                var originalSetDragImage = mouseDragDataTransfer.setDragImage;
                mouseDragDataTransfer.setDragImage = function(element, x, y) {
                    // Detect if TiddlyWiki is setting a blank drag image
                    // TiddlyWiki uses an empty div (no children) for blank drag images
                    if (element && (!element.firstChild || element.offsetWidth === 0 || element.offsetHeight === 0)) {
                        dragImageIsBlank = true;
                    }
                    if (originalSetDragImage) {
                        originalSetDragImage.call(this, element, x, y);
                    }
                };

                var dragStartEvent = createSyntheticDragEvent("dragstart", {
                    clientX: mouseDownPos.x,
                    clientY: mouseDownPos.y
                }, mouseDragDataTransfer);
                dragStartEvent.__tiddlyDesktopSynthetic = true;

                mouseDownTarget.dispatchEvent(dragStartEvent);

                // Capture effectAllowed that TiddlyWiki set during dragstart
                window.__tiddlyDesktopEffectAllowed = mouseDragDataTransfer.effectAllowed || "all";

                // Create drag image that follows cursor (unless TiddlyWiki requested blank)
                createDragImage(mouseDownTarget, event.clientX, event.clientY);

                // Initial dragenter on current element
                var enterTarget = document.elementFromPoint(event.clientX, event.clientY);
                if (enterTarget) {
                    var enterEvent = createSyntheticDragEvent("dragenter", {
                        clientX: event.clientX,
                        clientY: event.clientY,
                        relatedTarget: null
                    }, mouseDragDataTransfer);
                    enterTarget.dispatchEvent(enterEvent);
                    lastDragOverTarget = enterTarget;
                }
            }, true);

            var lastDragOverTarget = null;

            document.addEventListener("mousemove", function(event) {
                if (!mouseDragStarted || !internalDragSource) return;

                // Update drag image position
                updateDragImagePosition(event.clientX, event.clientY);

                var target = document.elementFromPoint(event.clientX, event.clientY);
                if (!target) return;

                // If target changed, fire dragleave/dragenter
                if (lastDragOverTarget && lastDragOverTarget !== target) {
                    // Find droppable ancestors that we're leaving (not ancestors of new target)
                    var oldDroppables = [];
                    var el = lastDragOverTarget;
                    while (el) {
                        if (el.classList && (el.classList.contains("tc-droppable") || el.classList.contains("tc-dropzone"))) {
                            oldDroppables.push(el);
                        }
                        el = el.parentElement;
                    }

                    // Fire dragleave on the old target
                    var leaveEvent = createSyntheticDragEvent("dragleave", {
                        clientX: event.clientX,
                        clientY: event.clientY,
                        relatedTarget: target
                    }, mouseDragDataTransfer);
                    lastDragOverTarget.dispatchEvent(leaveEvent);

                    // Fire dragleave on any droppables we're leaving that don't contain the new target
                    oldDroppables.forEach(function(droppable) {
                        if (!droppable.contains(target)) {
                            var droppableLeaveEvent = createSyntheticDragEvent("dragleave", {
                                clientX: event.clientX,
                                clientY: event.clientY,
                                relatedTarget: target
                            }, mouseDragDataTransfer);
                            droppable.dispatchEvent(droppableLeaveEvent);
                        }
                    });

                    var enterEvent = createSyntheticDragEvent("dragenter", {
                        clientX: event.clientX,
                        clientY: event.clientY,
                        relatedTarget: lastDragOverTarget
                    }, mouseDragDataTransfer);
                    target.dispatchEvent(enterEvent);
                }
                lastDragOverTarget = target;

                // Fire dragover
                var overEvent = createSyntheticDragEvent("dragover", {
                    clientX: event.clientX,
                    clientY: event.clientY
                }, mouseDragDataTransfer);
                target.dispatchEvent(overEvent);
            }, true);

            document.addEventListener("mouseup", function(event) {
                if (mouseDragStarted && internalDragSource) {
                    var target = document.elementFromPoint(event.clientX, event.clientY);

                    // Fire dragleave
                    if (lastDragOverTarget) {
                        var leaveEvent = createSyntheticDragEvent("dragleave", {
                            clientX: event.clientX,
                            clientY: event.clientY,
                            relatedTarget: null
                        }, mouseDragDataTransfer);
                        lastDragOverTarget.dispatchEvent(leaveEvent);
                    }

                    // Fire drop - getData is globally patched to use our cache
                    if (target) {
                        var dropDt = new DataTransfer();
                        var dropEvent = createSyntheticDragEvent("drop", {
                            clientX: event.clientX,
                            clientY: event.clientY
                        }, dropDt);
                        target.dispatchEvent(dropEvent);
                    }

                    // Fire dragend
                    var endEvent = createSyntheticDragEvent("dragend", {
                        clientX: event.clientX,
                        clientY: event.clientY
                    }, mouseDragDataTransfer);
                    internalDragSource.dispatchEvent(endEvent);

                    lastDragOverTarget = null;
                    mouseDragDataTransfer = null;
                    // Restore text selection
                    document.body.style.userSelect = "";
                    document.body.style.webkitUserSelect = "";
                }

                mouseDownTarget = null;
                mouseDownPos = null;
                mouseDragStarted = false;
            }, true);

            // Helper to clear all dragover states and end drag
            function cancelDrag(reason) {
                if (!internalDragActive && !mouseDragStarted) return;

                var dt = mouseDragDataTransfer || new DataTransfer();

                // Fire dragleave on lastDragOverTarget
                if (lastDragOverTarget) {
                    var leaveEvent = createSyntheticDragEvent("dragleave", {
                        relatedTarget: null
                    }, dt);
                    lastDragOverTarget.dispatchEvent(leaveEvent);
                }

                // Fire dragleave and dragend on ALL elements with tc-dragover class
                // The dragend triggers TiddlyWiki's dropzone resetState() method
                document.querySelectorAll(".tc-dragover").forEach(function(el) {
                    var droppableLeaveEvent = createSyntheticDragEvent("dragleave", {
                        relatedTarget: null
                    }, dt);
                    el.dispatchEvent(droppableLeaveEvent);
                    var dropzoneEndEvent = createSyntheticDragEvent("dragend", {}, dt);
                    el.dispatchEvent(dropzoneEndEvent);
                    el.classList.remove("tc-dragover");
                });

                // Also fire dragend on dropzone elements that might not have tc-dragover yet
                document.querySelectorAll(".tc-dropzone").forEach(function(el) {
                    var dropzoneEndEvent = createSyntheticDragEvent("dragend", {}, dt);
                    el.dispatchEvent(dropzoneEndEvent);
                });

                // Remove tc-dragging class from any elements that have it
                document.querySelectorAll(".tc-dragging").forEach(function(el) {
                    el.classList.remove("tc-dragging");
                });

                if (internalDragSource) {
                    var endEvent = createSyntheticDragEvent("dragend", {}, dt);
                    internalDragSource.dispatchEvent(endEvent);
                }

                // Reset TiddlyWiki's internal drag state
                if (typeof $tw !== "undefined") {
                    $tw.dragInProgress = null;
                }

                window.__tiddlyDesktopDragData = null;
                window.__tiddlyDesktopEffectAllowed = null;
                internalDragSource = null;
                internalDragActive = false;
                mouseDownTarget = null;
                mouseDownPos = null;
                mouseDragStarted = false;
                mouseDragDataTransfer = null;
                lastDragOverTarget = null;
                removeDragImage();
                // Restore text selection
                document.body.style.userSelect = "";
                document.body.style.webkitUserSelect = "";
            }

            // Handle mouse leaving the window during drag
            document.addEventListener("mouseleave", function(event) {
                if (internalDragActive || mouseDragStarted) {
                    // Only cancel if mouse truly left the document (relatedTarget is null or outside)
                    // This prevents false positives from DOM manipulations
                    if (!event.relatedTarget || !document.contains(event.relatedTarget)) {
                        cancelDrag("mouse left window");
                    }
                }
            }, true);

            // Handle escape key to cancel drag
            document.addEventListener("keydown", function(event) {
                if (event.key === "Escape") {
                    cancelDrag("escape pressed");
                }
            }, true);

            // Handle window blur (switching to another app) during drag
            // Use a small delay to avoid false positives from transient focus changes
            window.addEventListener("blur", function(event) {
                if (internalDragActive || mouseDragStarted) {
                    setTimeout(function() {
                        // Only cancel if drag is still active and window truly lost focus
                        if ((internalDragActive || mouseDragStarted) && !document.hasFocus()) {
                            cancelDrag("window lost focus");
                        }
                    }, 100);
                }
            }, true);

            console.log("[TiddlyDesktop] Internal drag-and-drop polyfill ready");
        })();

        // tm-open-window and related handlers for opening tiddlers in new windows
        (function setupWindowHandlers() {
            function waitForTiddlyWikiReady() {
                if (typeof $tw === 'undefined' || !$tw.rootWidget) {
                    setTimeout(waitForTiddlyWikiReady, 100);
                    return;
                }

                // Skip main wiki - it uses its own startup.js handlers
                if (window.__IS_MAIN_WIKI__) {
                    console.log('[TiddlyDesktop] Main wiki - window handlers not needed');
                    return;
                }

                if (!window.__TAURI__ || !window.__TAURI__.core) {
                    setTimeout(waitForTiddlyWikiReady, 100);
                    return;
                }

                var invoke = window.__TAURI__.core.invoke;
                var windowLabel = window.__WINDOW_LABEL__ || 'unknown';

                // Store references to opened Tauri windows separately from TiddlyWiki's $tw.windows
                // TiddlyWiki expects $tw.windows entries to be actual Window objects with document.body
                // We use our own tracking to avoid conflicts
                window.__tiddlyDesktopWindows = window.__tiddlyDesktopWindows || {};

                // tm-open-window handler - opens tiddler in new window
                $tw.rootWidget.addEventListener('tm-open-window', function(event) {
                    var title = event.param || event.tiddlerTitle;
                    var paramObject = event.paramObject || {};
                    var windowTitle = paramObject.windowTitle || title;
                    var windowID = paramObject.windowID || title;
                    var template = paramObject.template || '$:/core/templates/single.tiddler.window';
                    var width = paramObject.width ? parseFloat(paramObject.width) : null;
                    var height = paramObject.height ? parseFloat(paramObject.height) : null;
                    var left = paramObject.left ? parseFloat(paramObject.left) : null;
                    var top = paramObject.top ? parseFloat(paramObject.top) : null;

                    // Collect any additional variables (any params not in the known list)
                    var knownParams = ['windowTitle', 'windowID', 'template', 'width', 'height', 'left', 'top'];
                    var extraVariables = {};
                    for (var key in paramObject) {
                        if (paramObject.hasOwnProperty(key) && knownParams.indexOf(key) === -1) {
                            extraVariables[key] = paramObject[key];
                        }
                    }
                    // Always include currentTiddler and tv-window-id
                    extraVariables.currentTiddler = title;
                    extraVariables['tv-window-id'] = windowID;

                    // Call Tauri command to open tiddler window
                    invoke('open_tiddler_window', {
                        parentLabel: windowLabel,
                        tiddlerTitle: title,
                        template: template,
                        windowTitle: windowTitle,
                        width: width,
                        height: height,
                        left: left,
                        top: top,
                        variables: JSON.stringify(extraVariables)
                    }).then(function(newLabel) {
                        // Store reference in our own tracking (not $tw.windows)
                        window.__tiddlyDesktopWindows[windowID] = { label: newLabel, title: title };
                    }).catch(function(err) {
                        console.error('[TiddlyDesktop] Failed to open tiddler window:', err);
                    });

                    // Prevent default TiddlyWiki handler
                    return false;
                });

                // tm-close-window handler
                $tw.rootWidget.addEventListener('tm-close-window', function(event) {
                    var windowID = event.param;
                    var windows = window.__tiddlyDesktopWindows || {};
                    if (windows[windowID]) {
                        var windowInfo = windows[windowID];
                        invoke('close_window_by_label', { label: windowInfo.label }).catch(function(err) {
                            console.error('[TiddlyDesktop] Failed to close window:', err);
                        });
                        delete windows[windowID];
                    }
                    return false;
                });

                // tm-close-all-windows handler
                $tw.rootWidget.addEventListener('tm-close-all-windows', function(event) {
                    var windows = window.__tiddlyDesktopWindows || {};
                    Object.keys(windows).forEach(function(windowID) {
                        var windowInfo = windows[windowID];
                        invoke('close_window_by_label', { label: windowInfo.label }).catch(function() {});
                    });
                    window.__tiddlyDesktopWindows = {};
                    return false;
                });

                // tm-open-external-window handler - opens URL in default browser
                $tw.rootWidget.addEventListener('tm-open-external-window', function(event) {
                    var url = event.param || 'https://tiddlywiki.com/';
                    // Use Tauri's opener plugin to open in default browser
                    if (window.__TAURI__ && window.__TAURI__.opener) {
                        window.__TAURI__.opener.openUrl(url).catch(function(err) {
                            console.error('[TiddlyDesktop] Failed to open external URL:', err);
                        });
                    }
                    return false;
                });

                // ========================================
                // Cross-window tiddler synchronization
                // Sync changes between parent wiki and tiddler windows
                // ========================================
                var wikiPath = window.__WIKI_PATH__ || '';
                var currentWindowLabel = window.__WINDOW_LABEL__ || 'unknown';
                var isReceivingSync = false;  // Flag to prevent sync loops
                var emit = window.__TAURI__.event.emit;
                var listen = window.__TAURI__.event.listen;

                // Listen for tiddler changes from other windows
                listen('wiki-tiddler-change', function(event) {
                    var payload = event.payload;
                    // Only process if it's for the same wiki and from a different window
                    if (payload.wikiPath === wikiPath && payload.sourceWindow !== currentWindowLabel) {
                        isReceivingSync = true;
                        try {
                            if (payload.deleted) {
                                $tw.wiki.deleteTiddler(payload.title);
                            } else if (payload.tiddler) {
                                $tw.wiki.addTiddler(new $tw.Tiddler(payload.tiddler));
                            }
                        } finally {
                            // Use setTimeout to ensure the change event has fired before clearing flag
                            setTimeout(function() { isReceivingSync = false; }, 0);
                        }
                    }
                });

                // Watch for local tiddler changes and broadcast to other windows
                $tw.wiki.addEventListener('change', function(changes) {
                    // Don't re-broadcast changes we received from sync
                    if (isReceivingSync) return;

                    Object.keys(changes).forEach(function(title) {
                        var tiddler = $tw.wiki.getTiddler(title);
                        var payload = {
                            wikiPath: wikiPath,
                            sourceWindow: currentWindowLabel,
                            title: title,
                            deleted: changes[title].deleted,
                            tiddler: tiddler ? tiddler.fields : null
                        };

                        emit('wiki-tiddler-change', payload);
                    });
                });

                console.log('[TiddlyDesktop] Window message handlers ready, sync enabled for:', wikiPath);
            }

            waitForTiddlyWikiReady();
        })();

    })();
    "#
}

/// Normalize a path for cross-platform compatibility
/// On Windows: removes \\?\ prefixes and ensures proper separators
fn normalize_path(path: PathBuf) -> PathBuf {
    // Use dunce to simplify Windows paths (removes \\?\ UNC prefixes)
    let normalized = dunce::simplified(&path).to_path_buf();

    #[cfg(target_os = "windows")]
    {
        let path_str = normalized.to_string_lossy();
        // Fix malformed paths like "C:resources" -> "C:\resources"
        if path_str.len() >= 2 {
            let chars: Vec<char> = path_str.chars().collect();
            if chars[1] == ':' && path_str.len() > 2 && chars[2] != '\\' && chars[2] != '/' {
                let fixed = format!("{}:\\{}", chars[0], &path_str[2..]);
                println!("Fixed malformed path: {} -> {}", path_str, fixed);
                return PathBuf::from(fixed);
            }
        }
    }

    normalized
}

/// Check if a path is a wiki folder (contains tiddlywiki.info)
fn is_wiki_folder(path: &std::path::Path) -> bool {
    path.is_dir() && path.join("tiddlywiki.info").exists()
}

/// Get the next available port for a wiki folder server
fn allocate_port(state: &AppState) -> u16 {
    let mut port = state.next_port.lock().unwrap();
    let allocated = *port;
    *port += 1;
    allocated
}

/// Check if system Node.js is available and compatible (v18+)
fn find_system_node() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let node_name = "node.exe";
    #[cfg(not(target_os = "windows"))]
    let node_name = "node";

    // Check if node is in PATH
    let mut cmd = Command::new(node_name);
    cmd.arg("--version");
    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);

    if let Ok(output) = cmd.output() {
        if output.status.success() {
            let version = String::from_utf8_lossy(&output.stdout);
            // Parse version (e.g., "v20.11.0" -> 20)
            if let Some(major) = version.trim().strip_prefix('v')
                .and_then(|v| v.split('.').next())
                .and_then(|m| m.parse::<u32>().ok())
            {
                // Require Node.js v18 or higher
                if major >= 18 {
                    println!("Found system Node.js {} in PATH", version.trim());
                    return Some(PathBuf::from(node_name));
                } else {
                    println!("System Node.js {} is too old (need v18+), using bundled", version.trim());
                }
            }
        }
    }
    None
}

/// Get path to Node.js binary (prefer system, fall back to bundled)
fn get_node_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    // First, try to use system Node.js if available and compatible
    if let Some(system_node) = find_system_node() {
        return Ok(system_node);
    }

    // Fall back to bundled Node.js
    let resource_path = get_resource_dir_path(app)
        .ok_or_else(|| "Failed to get resource directory".to_string())?;
    let resource_path = normalize_path(resource_path);

    #[cfg(target_os = "windows")]
    let node_name = "node.exe";
    #[cfg(not(target_os = "windows"))]
    let node_name = "node";

    // Tauri sidecars are placed in the same directory as the main executable
    let exe_dir = std::env::current_exe()
        .map_err(|e| format!("Failed to get exe path: {}", e))?
        .parent()
        .ok_or("Failed to get exe directory")?
        .to_path_buf();

    // Try different possible locations for bundled Node.js
    let possible_paths = [
        exe_dir.join(node_name),
        resource_path.join("resources").join("binaries").join(node_name),
        resource_path.join("binaries").join(node_name),
    ];

    for path in &possible_paths {
        if path.exists() {
            println!("Using bundled Node.js at {:?}", path);
            return Ok(path.clone());
        }
    }

    Err(format!("Node.js not found. Install Node.js v18+ or ensure bundled binary exists. Tried: {:?}", possible_paths))
}

/// Get path to bundled TiddlyWiki
fn get_tiddlywiki_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let resource_path = get_resource_dir_path(app)
        .ok_or_else(|| "Failed to get resource directory".to_string())?;
    let resource_path = normalize_path(resource_path);

    // Tarball structure has tiddlywiki directly in lib/tiddlydesktop-rs/tiddlywiki/
    let tw_path = resource_path.join("tiddlywiki").join("tiddlywiki.js");
    // Also check Tauri bundle structure with resources/ prefix
    let tw_path_bundled = resource_path.join("resources").join("tiddlywiki").join("tiddlywiki.js");

    // Also check in the development path
    let dev_path = PathBuf::from("src-tauri/resources/tiddlywiki/tiddlywiki.js");

    if tw_path.exists() {
        Ok(tw_path)
    } else if tw_path_bundled.exists() {
        Ok(tw_path_bundled)
    } else if dev_path.exists() {
        let canonical = dev_path.canonicalize().map_err(|e| e.to_string())?;
        Ok(normalize_path(canonical))
    } else {
        Err(format!("TiddlyWiki not found at {:?}, {:?}, or {:?}", tw_path, tw_path_bundled, dev_path))
    }
}

/// Ensure required plugins and autosave are enabled for a wiki folder
fn ensure_wiki_folder_config(wiki_path: &PathBuf) {
    // Ensure required plugins are in tiddlywiki.info
    let info_path = wiki_path.join("tiddlywiki.info");
    if info_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&info_path) {
            if let Ok(mut info) = serde_json::from_str::<serde_json::Value>(&content) {
                let required_plugins = vec!["tiddlywiki/tiddlyweb", "tiddlywiki/filesystem"];
                let mut modified = false;

                let plugins_array = info.get_mut("plugins")
                    .and_then(|v| v.as_array_mut());

                if let Some(arr) = plugins_array {
                    for plugin_path in &required_plugins {
                        if !arr.iter().any(|p| p.as_str() == Some(*plugin_path)) {
                            arr.push(serde_json::Value::String(plugin_path.to_string()));
                            modified = true;
                        }
                    }
                } else {
                    // Create plugins array with required plugins
                    let plugins: Vec<serde_json::Value> = required_plugins.iter()
                        .map(|p| serde_json::Value::String(p.to_string()))
                        .collect();
                    info["plugins"] = serde_json::Value::Array(plugins);
                    modified = true;
                }

                if modified {
                    if let Ok(updated_content) = serde_json::to_string_pretty(&info) {
                        if let Err(e) = std::fs::write(&info_path, updated_content) {
                            eprintln!("Warning: Failed to update tiddlywiki.info: {}", e);
                        } else {
                            println!("Added required plugins to tiddlywiki.info");
                        }
                    }
                }
            }
        }
    }

    // Ensure autosave is enabled
    let tiddlers_dir = wiki_path.join("tiddlers");
    let autosave_tiddler = tiddlers_dir.join("$__config_AutoSave.tid");

    // Only create if the tiddlers folder exists and autosave tiddler doesn't
    if tiddlers_dir.exists() && !autosave_tiddler.exists() {
        let autosave_content = "title: $:/config/AutoSave\n\nyes";
        if let Err(e) = std::fs::write(&autosave_tiddler, autosave_content) {
            eprintln!("Warning: Failed to enable autosave: {}", e);
        } else {
            println!("Enabled autosave for wiki folder");
        }
    }
}

/// Wait for TCP server with exponential backoff
fn wait_for_server_ready(port: u16, process: &mut Child, timeout: std::time::Duration) -> Result<(), String> {
    use std::net::TcpStream;
    use std::time::Instant;

    let start = Instant::now();
    let addr: std::net::SocketAddr = format!("127.0.0.1:{}", port).parse().unwrap();
    let mut delay = std::time::Duration::from_millis(50);

    loop {
        // Check if process died
        if let Ok(Some(status)) = process.try_wait() {
            return Err(format!("Server exited with status: {}", status));
        }

        // Try to connect
        if TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(100)).is_ok() {
            println!("Server ready on port {} ({:.1}s)", port, start.elapsed().as_secs_f64());
            return Ok(());
        }

        // Check timeout
        if start.elapsed() >= timeout {
            return Err(format!("Server failed to start within {:?}", timeout));
        }

        std::thread::sleep(delay);
        delay = (delay * 2).min(std::time::Duration::from_secs(1)); // Cap at 1s
    }
}

/// Open a wiki folder in a new window with its own server
/// Returns WikiEntry so frontend can update its wiki list
#[tauri::command]
async fn open_wiki_folder(app: tauri::AppHandle, path: String) -> Result<WikiEntry, String> {
    let path_buf = PathBuf::from(&path);
    let state = app.state::<AppState>();

    // Get folder name
    let folder_name = path_buf
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Unknown")
        .to_string();

    // Verify it's a wiki folder
    if !is_wiki_folder(&path_buf) {
        return Err("Not a valid wiki folder (missing tiddlywiki.info)".to_string());
    }

    // Check if this wiki folder is already open
    {
        let open_wikis = state.open_wikis.lock().unwrap();
        for (label, wiki_path) in open_wikis.iter() {
            if wiki_path == &path {
                // Focus existing window
                if let Some(window) = app.get_webview_window(label) {
                    let _ = window.set_focus();
                    // Return entry even when focusing existing window
                    return Ok(WikiEntry {
                        path: path.clone(),
                        filename: folder_name,
                        favicon: None,
                        is_folder: true,
                        backups_enabled: false,
                        backup_dir: None,
                        group: None,
                    });
                }
            }
        }
    }

    // Ensure required plugins and autosave are enabled
    ensure_wiki_folder_config(&path_buf);

    // Extract favicon from the wiki folder
    let favicon = extract_favicon_from_folder(&path_buf).await;

    // Allocate a port for this server
    let port = allocate_port(&state);

    // Generate unique window label
    let base_label = folder_name.replace(|c: char| !c.is_alphanumeric(), "-");
    let label = {
        let open_wikis = state.open_wikis.lock().unwrap();
        let mut label = format!("folder-{}", base_label);
        let mut counter = 1;
        while open_wikis.contains_key(&label) {
            label = format!("folder-{}-{}", base_label, counter);
            counter += 1;
        }
        label
    };

    // Track this wiki as open
    state.open_wikis.lock().unwrap().insert(label.clone(), path.clone());

    // Start the Node.js + TiddlyWiki server
    let node_path = get_node_path(&app)?;
    let tw_path = get_tiddlywiki_path(&app)?;

    println!("Starting wiki folder server:");
    println!("  Node.js: {:?}", node_path);
    println!("  TiddlyWiki: {:?}", tw_path);
    println!("  Wiki folder: {:?}", path_buf);
    println!("  Port: {}", port);

    let mut cmd = Command::new(&node_path);
    cmd.arg(&tw_path)
        .arg(&path_buf)
        .arg("--listen")
        .arg(format!("port={}", port))
        .arg("host=127.0.0.1");
    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);
    // On Linux, set up child to die when parent dies
    #[cfg(target_os = "linux")]
    unsafe {
        cmd.pre_exec(|| {
            // PR_SET_PDEATHSIG = 1, SIGKILL = 9
            libc::prctl(1, 9);
            Ok(())
        });
    }
    let mut child = cmd.spawn()
        .map_err(|e| format!("Failed to start TiddlyWiki server: {}", e))?;
    // On Windows, assign process to job object so it dies when parent dies
    #[cfg(target_os = "windows")]
    windows_job::assign_process_to_job(child.id());

    // Wait for server to be ready (10s timeout)
    if let Err(e) = wait_for_server_ready(port, &mut child, std::time::Duration::from_secs(10)) {
        let _ = child.kill();
        state.open_wikis.lock().unwrap().remove(&label);
        return Err(format!("Failed to start wiki server: {}", e));
    }

    // Store the server info
    state.wiki_servers.lock().unwrap().insert(label.clone(), WikiFolderServer {
        process: child,
        port,
        path: path.clone(),
    });

    let server_url = format!("http://127.0.0.1:{}", port);

    let icon = Image::from_bytes(include_bytes!("../icons/icon.png"))
        .map_err(|e| format!("Failed to load icon: {}", e))?;

    // Get isolated session directory for this wiki folder
    let session_dir = get_wiki_session_dir(&app, &path);

    let mut builder = WebviewWindowBuilder::new(&app, &label, WebviewUrl::External(server_url.parse().unwrap()))
        .title(&folder_name)
        .inner_size(1200.0, 800.0)
        .icon(icon)
        .map_err(|e| format!("Failed to set icon: {}", e))?
        .window_classname("tiddlydesktop-rs")
        .initialization_script(&get_init_script_with_path(&path))
        .devtools(false);

    // Apply isolated session if available
    if let Some(dir) = session_dir {
        builder = builder.data_directory(dir);
    }

    let window = builder
        // NOT calling disable_drag_drop_handler() - we need tauri://drag-drop events
        // for external attachments support. File drops are handled via Tauri events.
        .build()
        .map_err(|e| format!("Failed to create window: {}", e))?;

    // Set up platform-specific drag handlers
    #[cfg(target_os = "linux")]
    linux_drag::setup_drag_handlers(&window);
    #[cfg(target_os = "windows")]
    windows_drag::setup_drag_handlers(&window);
    #[cfg(target_os = "macos")]
    macos_drag::setup_drag_handlers(&window);

    // Handle window close - JS onCloseRequested handles unsaved changes confirmation
    let app_handle = app.clone();
    let label_clone = label.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::Destroyed = event {
            let state = app_handle.state::<AppState>();
            // Stop the server
            if let Some(mut server) = state.wiki_servers.lock().unwrap().remove(&label_clone) {
                let _ = server.process.kill();
            }
            // Remove from open wikis
            state.open_wikis.lock().unwrap().remove(&label_clone);
        }
    });

    // Create the wiki entry
    let entry = WikiEntry {
        path,
        filename: folder_name,
        favicon,
        is_folder: true,
        backups_enabled: false, // Not applicable for folder wikis (they use autosave)
        backup_dir: None,
        group: None,
    };

    // Add to recent files list
    let _ = add_to_recent_files(&app, entry.clone());

    // Return the wiki entry so frontend can update its list
    Ok(entry)
}

/// Check if a path is a wiki folder
#[tauri::command]
fn check_is_wiki_folder(_app: tauri::AppHandle, path: String) -> bool {
    let path_buf = PathBuf::from(&path);
    is_wiki_folder(&path_buf)
}

/// Edition info for UI display
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EditionInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub is_user_edition: bool,
}

/// Get list of available TiddlyWiki editions
#[tauri::command]
async fn get_available_editions(app: tauri::AppHandle) -> Result<Vec<EditionInfo>, String> {
    let tw_path = get_tiddlywiki_path(&app)?;
    let bundled_editions_dir = tw_path.parent()
        .ok_or("Failed to get TiddlyWiki directory")?
        .join("editions");

    if !bundled_editions_dir.exists() {
        return Err("Editions directory not found".to_string());
    }

    // Get user editions directory and create it if it doesn't exist
    let user_editions_dir = get_user_editions_dir(&app)?;
    if !user_editions_dir.exists() {
        let _ = std::fs::create_dir_all(&user_editions_dir);
    }

    // Common editions with friendly names and descriptions
    let edition_metadata: std::collections::HashMap<&str, (&str, &str)> = [
        ("server", ("Server", "Basic Node.js server wiki - recommended for most users")),
        ("empty", ("Empty", "Minimal empty wiki with no content")),
        ("full", ("Full", "Full-featured wiki with many plugins")),
        ("dev", ("Developer", "Development edition with extra tools")),
        ("tw5.com", ("TW5 Documentation", "Full TiddlyWiki documentation")),
        ("introduction", ("Introduction", "Introduction and tutorial content")),
        ("prerelease", ("Prerelease", "Latest prerelease features")),
    ].iter().cloned().collect();

    // Editions to skip (test/internal editions)
    let skip_editions = ["test", "testcommonjs", "pluginlibrary", "tiddlydesktop-rs"];

    // Helper to read editions from a directory
    let read_editions_from_dir = |dir: &PathBuf, is_user_edition: bool, skip_ids: &[&str]| -> Vec<EditionInfo> {
        if !dir.exists() {
            return Vec::new();
        }
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let path = entry.path();
                if !path.is_dir() {
                    return None;
                }
                let name = path.file_name()?.to_str()?;

                // Skip if in skip list
                if skip_ids.contains(&name) {
                    return None;
                }
                // Skip if no tiddlywiki.info
                if !path.join("tiddlywiki.info").exists() {
                    return None;
                }

                let (display_name, description) = edition_metadata
                    .get(name)
                    .map(|(n, d)| (n.to_string(), d.to_string()))
                    .unwrap_or_else(|| {
                        (name.replace('-', " ").replace('_', " "), format!("{} edition", name))
                    });

                Some(EditionInfo {
                    id: name.to_string(),
                    name: display_name,
                    description,
                    is_user_edition,
                })
            })
            .collect()
    };

    let mut editions = Vec::new();

    // First add the common/recommended built-in editions in order
    let priority_editions = ["server", "empty", "full", "dev"];
    for edition_id in &priority_editions {
        let edition_path = bundled_editions_dir.join(edition_id);
        if edition_path.exists() && edition_path.join("tiddlywiki.info").exists() {
            let (name, desc) = edition_metadata
                .get(*edition_id)
                .map(|(n, d)| (n.to_string(), d.to_string()))
                .unwrap_or_else(|| {
                    (edition_id.replace('-', " ").replace('_', " "), format!("{} edition", edition_id))
                });
            editions.push(EditionInfo {
                id: edition_id.to_string(),
                name,
                description: desc,
                is_user_edition: false,
            });
        }
    }

    // Then add user editions (sorted alphabetically)
    let mut user_editions = read_editions_from_dir(&user_editions_dir, true, &skip_editions);
    user_editions.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    let user_edition_ids: Vec<String> = user_editions.iter().map(|e| e.id.clone()).collect();
    editions.extend(user_editions);

    // Then add other built-in editions alphabetically (excluding priority and user editions with same id)
    let mut skip_for_builtin: Vec<&str> = skip_editions.to_vec();
    skip_for_builtin.extend(priority_editions.iter());
    for id in &user_edition_ids {
        skip_for_builtin.push(id.as_str());
    }
    let mut other_builtin = read_editions_from_dir(&bundled_editions_dir, false, &skip_for_builtin);
    other_builtin.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    editions.extend(other_builtin);

    println!("Editions: {} total ({} user editions from {:?})", editions.len(), user_edition_ids.len(), user_editions_dir);

    Ok(editions)
}

/// Plugin info for UI display
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub category: String,
}

/// Get list of available TiddlyWiki plugins
#[tauri::command]
async fn get_available_plugins(app: tauri::AppHandle) -> Result<Vec<PluginInfo>, String> {
    let tw_path = get_tiddlywiki_path(&app)?;
    let plugins_dir = tw_path.parent()
        .ok_or("Failed to get TiddlyWiki directory")?
        .join("plugins")
        .join("tiddlywiki");

    if !plugins_dir.exists() {
        return Err("Plugins directory not found".to_string());
    }

    let mut plugins = Vec::new();

    // Categories for organizing plugins
    let editor_plugins = ["codemirror", "codemirror-autocomplete", "codemirror-closebrackets",
        "codemirror-closetag", "codemirror-mode-css", "codemirror-mode-javascript",
        "codemirror-mode-markdown", "codemirror-mode-xml", "codemirror-search-replace"];
    let utility_plugins = ["markdown", "highlight", "katex", "jszip", "xlsx-utils", "qrcode", "innerwiki", "tiddlydesktop-rs-commands"];
    let storage_plugins = ["browser-storage", "filesystem", "tiddlyweb"];

    if let Ok(entries) = std::fs::read_dir(&plugins_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let plugin_info_path = path.join("plugin.info");
                if plugin_info_path.exists() {
                    if let Ok(content) = std::fs::read_to_string(&plugin_info_path) {
                        if let Ok(info) = serde_json::from_str::<serde_json::Value>(&content) {
                            let id = path.file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("")
                                .to_string();

                            // Skip internal/core plugins
                            if id == "tiddlyweb" || id == "filesystem" || id == "tiddlydesktop-rs" || id.starts_with("test") {
                                continue;
                            }

                            let name = info.get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or(&id)
                                .to_string();

                            let description = info.get("description")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();

                            // Determine category
                            let category = if editor_plugins.iter().any(|p| id.starts_with(p)) {
                                "Editor"
                            } else if utility_plugins.contains(&id.as_str()) {
                                "Utility"
                            } else if storage_plugins.contains(&id.as_str()) {
                                "Storage"
                            } else {
                                "Other"
                            }.to_string();

                            plugins.push(PluginInfo {
                                id,
                                name,
                                description,
                                category,
                            });
                        }
                    }
                }
            }
        }
    }

    // Sort by category, then by name
    plugins.sort_by(|a, b| {
        let cat_order = |c: &str| match c {
            "Editor" => 0,
            "Utility" => 1,
            "Storage" => 2,
            _ => 3,
        };
        cat_order(&a.category).cmp(&cat_order(&b.category))
            .then_with(|| a.name.cmp(&b.name))
    });

    Ok(plugins)
}

/// Initialize a new wiki folder with the specified edition and plugins
#[tauri::command]
async fn init_wiki_folder(app: tauri::AppHandle, path: String, edition: String, plugins: Vec<String>) -> Result<(), String> {
    let path_buf = PathBuf::from(&path);

    // Verify the folder exists
    if !path_buf.exists() {
        std::fs::create_dir_all(&path_buf)
            .map_err(|e| format!("Failed to create directory: {}", e))?;
    }

    // Check if already initialized
    if path_buf.join("tiddlywiki.info").exists() {
        return Err("Folder already contains a TiddlyWiki".to_string());
    }

    println!("Initializing wiki folder:");
    println!("  Target folder: {:?}", path_buf);
    println!("  Edition: {}", edition);
    println!("  Additional plugins: {:?}", plugins);

    // Use Node.js + TiddlyWiki to initialize the wiki
    let node_path = get_node_path(&app)?;
    let tw_path = get_tiddlywiki_path(&app)?;

    println!("  Node.js: {:?}", node_path);
    println!("  TiddlyWiki: {:?}", tw_path);

    // Run tiddlywiki --init <edition>
    let mut cmd = Command::new(&node_path);
    cmd.arg(&tw_path)
        .arg(&path_buf)
        .arg("--init")
        .arg(&edition);
    // Set TIDDLYWIKI_EDITION_PATH so TiddlyWiki can find user editions
    let user_editions_dir = get_user_editions_dir(&app)?;
    cmd.env("TIDDLYWIKI_EDITION_PATH", &user_editions_dir);
    #[cfg(target_os = "windows")]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let output = cmd.output()
        .map_err(|e| format!("Failed to run TiddlyWiki init: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!("TiddlyWiki init failed:\n{}\n{}", stdout, stderr));
    }

    // Verify initialization succeeded
    let info_path = path_buf.join("tiddlywiki.info");
    if !info_path.exists() {
        return Err("Initialization failed - tiddlywiki.info not created".to_string());
    }

    // Always ensure required plugins for server are present
    // Plus any additional user-selected plugins
    let required_plugins = vec!["tiddlywiki/tiddlyweb", "tiddlywiki/filesystem"];

    let content = std::fs::read_to_string(&info_path)
        .map_err(|e| format!("Failed to read tiddlywiki.info: {}", e))?;

    let mut info: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse tiddlywiki.info: {}", e))?;

    // Get or create plugins array
    let plugins_array = info.get_mut("plugins")
        .and_then(|v| v.as_array_mut());

    if let Some(arr) = plugins_array {
        // Add required plugins first
        for plugin_path in &required_plugins {
            if !arr.iter().any(|p| p.as_str() == Some(*plugin_path)) {
                arr.push(serde_json::Value::String(plugin_path.to_string()));
            }
        }
        // Add user-selected plugins
        for plugin in &plugins {
            let plugin_path = format!("tiddlywiki/{}", plugin);
            if !arr.iter().any(|p| p.as_str() == Some(&plugin_path)) {
                arr.push(serde_json::Value::String(plugin_path));
            }
        }
    } else {
        // Create new plugins array with required + user plugins
        let mut all_plugins: Vec<serde_json::Value> = required_plugins.iter()
            .map(|p| serde_json::Value::String(p.to_string()))
            .collect();
        for plugin in &plugins {
            all_plugins.push(serde_json::Value::String(format!("tiddlywiki/{}", plugin)));
        }
        info["plugins"] = serde_json::Value::Array(all_plugins);
    }

    // Write back
    let updated_content = serde_json::to_string_pretty(&info)
        .map_err(|e| format!("Failed to serialize tiddlywiki.info: {}", e))?;
    std::fs::write(&info_path, updated_content)
        .map_err(|e| format!("Failed to write tiddlywiki.info: {}", e))?;

    println!("Ensured tiddlyweb and filesystem plugins are present");

    // Create tiddlers folder if it doesn't exist
    let tiddlers_dir = path_buf.join("tiddlers");
    if !tiddlers_dir.exists() {
        std::fs::create_dir_all(&tiddlers_dir)
            .map_err(|e| format!("Failed to create tiddlers directory: {}", e))?;
    }

    // Enable autosave by creating the config tiddler
    let autosave_tiddler = tiddlers_dir.join("$__config_AutoSave.tid");
    let autosave_content = "title: $:/config/AutoSave\n\nyes";
    std::fs::write(&autosave_tiddler, autosave_content)
        .map_err(|e| format!("Failed to create autosave config: {}", e))?;

    println!("Enabled autosave for wiki folder");
    println!("Wiki folder initialized successfully");
    Ok(())
}

/// Create a single-file wiki with the specified edition and plugins
#[tauri::command]
async fn create_wiki_file(app: tauri::AppHandle, path: String, edition: String, plugins: Vec<String>) -> Result<(), String> {
    let output_path = PathBuf::from(&path);

    // Ensure it has .html extension
    let output_path = if output_path.extension().map(|e| e == "html" || e == "htm").unwrap_or(false) {
        output_path
    } else {
        output_path.with_extension("html")
    };

    println!("Creating single-file wiki:");
    println!("  Output: {:?}", output_path);
    println!("  Edition: {}", edition);
    println!("  Plugins: {:?}", plugins);

    // Use Node.js to build the wiki
    let node_path = get_node_path(&app)?;
    let tw_path = get_tiddlywiki_path(&app)?;
    let tw_dir = tw_path.parent().ok_or("Failed to get TiddlyWiki directory")?;

    // Create a temporary directory for the build
    let temp_dir = std::env::temp_dir().join(format!("tiddlydesktop-build-{}", std::process::id()));
    std::fs::create_dir_all(&temp_dir)
        .map_err(|e| format!("Failed to create temp directory: {}", e))?;

    println!("  Temp dir: {:?}", temp_dir);

    // Initialize the temp directory with the selected edition
    let mut init_cmd = Command::new(&node_path);
    init_cmd.arg(&tw_path)
        .arg(&temp_dir)
        .arg("--init")
        .arg(&edition);
    // Set TIDDLYWIKI_EDITION_PATH so TiddlyWiki can find user editions
    let user_editions_dir = get_user_editions_dir(&app)?;
    init_cmd.env("TIDDLYWIKI_EDITION_PATH", &user_editions_dir);
    #[cfg(target_os = "windows")]
    init_cmd.creation_flags(CREATE_NO_WINDOW);
    let init_output = init_cmd.output()
        .map_err(|e| format!("Failed to run TiddlyWiki init: {}", e))?;

    if !init_output.status.success() {
        let _ = std::fs::remove_dir_all(&temp_dir);
        let stderr = String::from_utf8_lossy(&init_output.stderr);
        return Err(format!("TiddlyWiki init failed: {}", stderr));
    }

    // Add plugins to tiddlywiki.info if any selected
    if !plugins.is_empty() {
        let info_path = temp_dir.join("tiddlywiki.info");
        if info_path.exists() {
            let content = std::fs::read_to_string(&info_path)
                .map_err(|e| format!("Failed to read tiddlywiki.info: {}", e))?;
            let mut info: serde_json::Value = serde_json::from_str(&content)
                .map_err(|e| format!("Failed to parse tiddlywiki.info: {}", e))?;

            let plugins_array = info.get_mut("plugins")
                .and_then(|v| v.as_array_mut());

            if let Some(arr) = plugins_array {
                for plugin in &plugins {
                    let plugin_path = format!("tiddlywiki/{}", plugin);
                    if !arr.iter().any(|p| p.as_str() == Some(&plugin_path)) {
                        arr.push(serde_json::Value::String(plugin_path));
                    }
                }
            } else {
                let plugin_values: Vec<serde_json::Value> = plugins.iter()
                    .map(|p| serde_json::Value::String(format!("tiddlywiki/{}", p)))
                    .collect();
                info["plugins"] = serde_json::Value::Array(plugin_values);
            }

            let updated_content = serde_json::to_string_pretty(&info)
                .map_err(|e| format!("Failed to serialize tiddlywiki.info: {}", e))?;
            std::fs::write(&info_path, updated_content)
                .map_err(|e| format!("Failed to write tiddlywiki.info: {}", e))?;
        }
    }

    // Get the output filename
    let output_filename = output_path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("wiki.html");

    // Build the single-file wiki
    let mut build_cmd = Command::new(&node_path);
    build_cmd.arg(&tw_path)
        .arg(&temp_dir)
        .arg("--output")
        .arg(temp_dir.join("output"))
        .arg("--render")
        .arg("$:/core/save/all")
        .arg(output_filename)
        .arg("text/plain")
        .current_dir(tw_dir);
    #[cfg(target_os = "windows")]
    build_cmd.creation_flags(CREATE_NO_WINDOW);
    let build_output = build_cmd.output()
        .map_err(|e| format!("Failed to build wiki: {}", e))?;

    if !build_output.status.success() {
        let _ = std::fs::remove_dir_all(&temp_dir);
        let stderr = String::from_utf8_lossy(&build_output.stderr);
        let stdout = String::from_utf8_lossy(&build_output.stdout);
        return Err(format!("Wiki build failed:\n{}\n{}", stdout, stderr));
    }

    // Move the output file to the target location
    let built_file = temp_dir.join("output").join(output_filename);
    if !built_file.exists() {
        let _ = std::fs::remove_dir_all(&temp_dir);
        return Err("Build succeeded but output file not found".to_string());
    }

    // Ensure parent directory exists
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create output directory: {}", e))?;
    }

    std::fs::copy(&built_file, &output_path)
        .map_err(|e| format!("Failed to copy wiki to destination: {}", e))?;

    // Clean up temp directory
    let _ = std::fs::remove_dir_all(&temp_dir);

    println!("Single-file wiki created successfully: {:?}", output_path);
    Ok(())
}

/// Check folder status - returns info about whether it's a wiki, empty, or has files
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FolderStatus {
    pub is_wiki: bool,
    pub is_empty: bool,
    pub has_files: bool,
    pub path: String,
    pub name: String,
}

#[tauri::command]
fn check_folder_status(path: String) -> Result<FolderStatus, String> {
    let path_buf = PathBuf::from(&path);

    if !path_buf.exists() {
        return Ok(FolderStatus {
            is_wiki: false,
            is_empty: true,
            has_files: false,
            path: path.clone(),
            name: path_buf.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("Unknown")
                .to_string(),
        });
    }

    if !path_buf.is_dir() {
        return Err("Path is not a directory".to_string());
    }

    let is_wiki = path_buf.join("tiddlywiki.info").exists();
    let has_files = std::fs::read_dir(&path_buf)
        .map(|entries| entries.count() > 0)
        .unwrap_or(false);

    Ok(FolderStatus {
        is_wiki,
        is_empty: !has_files,
        has_files,
        path: path.clone(),
        name: path_buf.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Unknown")
            .to_string(),
    })
}

/// Reveal file in system file manager
#[tauri::command]
async fn reveal_in_folder(path: String) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    {
        let path_buf = std::path::PathBuf::from(&path);
        let folder = path_buf.parent().unwrap_or(&path_buf);
        std::process::Command::new("xdg-open")
            .arg(folder)
            .spawn()
            .map_err(|e| e.to_string())?;
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("-R")
            .arg(&path)
            .spawn()
            .map_err(|e| e.to_string())?;
    }

    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer")
            .arg("/select,")
            .arg(&path)
            .spawn()
            .map_err(|e| e.to_string())?;
    }

    Ok(())
}

/// Read a file and return it as a base64 data URI
/// Used by wiki folders to support _canonical_uri with absolute paths
#[tauri::command]
async fn read_file_as_data_uri(path: String) -> Result<String, String> {
    let path_buf = PathBuf::from(&path);

    // Read the file
    let data = tokio::fs::read(&path_buf)
        .await
        .map_err(|e| format!("Failed to read file {}: {}", path, e))?;

    // Get MIME type and encode as base64
    let mime_type = get_mime_type(&path_buf);

    use base64::{engine::general_purpose::STANDARD, Engine};
    let base64_data = STANDARD.encode(&data);

    Ok(format!("data:{};base64,{}", mime_type, base64_data))
}

/// Read a file and return it as raw bytes
/// Used for external attachments drag-drop support
#[tauri::command]
async fn read_file_as_binary(path: String) -> Result<Vec<u8>, String> {
    let path_buf = PathBuf::from(&path);

    tokio::fs::read(&path_buf)
        .await
        .map_err(|e| format!("Failed to read file {}: {}", path, e))
}

/// Open a file picker dialog for importing files
/// Returns the selected file paths (empty if cancelled)
/// Used to replace browser's file input with native dialog that exposes full paths
#[tauri::command]
async fn pick_files_for_import(app: tauri::AppHandle, multiple: bool) -> Result<Vec<String>, String> {
    use tauri_plugin_dialog::DialogExt;

    let title = if multiple { "Import Files" } else { "Import File" };

    let paths: Vec<String> = if multiple {
        app.dialog()
            .file()
            .set_title(title)
            .blocking_pick_files()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| p.into_path().ok())
            .map(|p| p.to_string_lossy().to_string())
            .collect()
    } else {
        app.dialog()
            .file()
            .set_title(title)
            .blocking_pick_file()
            .and_then(|p| p.into_path().ok())
            .map(|p| vec![p.to_string_lossy().to_string()])
            .unwrap_or_default()
    };

    Ok(paths)
}

/// Get external attachments config for a specific wiki
#[tauri::command]
fn get_external_attachments_config(app: tauri::AppHandle, wiki_path: String) -> Result<ExternalAttachmentsConfig, String> {
    let configs = load_wiki_configs(&app)?;
    Ok(configs.external_attachments.get(&wiki_path).cloned().unwrap_or_default())
}

/// Set external attachments config for a specific wiki
#[tauri::command]
fn set_external_attachments_config(app: tauri::AppHandle, wiki_path: String, config: ExternalAttachmentsConfig) -> Result<(), String> {
    let mut configs = load_wiki_configs(&app)?;
    configs.external_attachments.insert(wiki_path, config);
    save_wiki_configs(&app, &configs)
}

/// Get session authentication URLs for a specific wiki
#[tauri::command]
fn get_session_auth_config(app: tauri::AppHandle, wiki_path: String) -> Result<SessionAuthConfig, String> {
    let configs = load_wiki_configs(&app)?;
    Ok(configs.session_auth.get(&wiki_path).cloned().unwrap_or_default())
}

/// Set session authentication URLs for a specific wiki
#[tauri::command]
fn set_session_auth_config(app: tauri::AppHandle, wiki_path: String, config: SessionAuthConfig) -> Result<(), String> {
    let mut configs = load_wiki_configs(&app)?;
    configs.session_auth.insert(wiki_path, config);
    save_wiki_configs(&app, &configs)
}

/// Open an authentication URL in a new window that shares the wiki's session
/// This allows users to log into external services and have cookies stored in the wiki's session
///
/// Security measures:
/// - Only HTTPS URLs are allowed (except localhost for development)
/// - DevTools are disabled to prevent credential inspection
/// - No JavaScript injection - pure browser window
/// - File protocol is blocked
#[tauri::command]
async fn open_auth_window(app: tauri::AppHandle, wiki_path: String, url: String, name: String) -> Result<(), String> {
    use tauri::WebviewWindowBuilder;
    use tauri::WebviewUrl;

    // Security: Validate URL scheme
    let url_lower = url.to_lowercase();

    // Block dangerous protocols
    if url_lower.starts_with("file:") {
        return Err("Security: File URLs are not allowed for authentication".to_string());
    }
    if url_lower.starts_with("javascript:") {
        return Err("Security: JavaScript URLs are not allowed".to_string());
    }
    if url_lower.starts_with("data:") {
        return Err("Security: Data URLs are not allowed for authentication".to_string());
    }

    // Only allow HTTPS (and localhost HTTP for development)
    let is_https = url_lower.starts_with("https://");
    let is_localhost_http = url_lower.starts_with("http://localhost")
        || url_lower.starts_with("http://127.0.0.1")
        || url_lower.starts_with("http://[::1]");

    if !is_https && !is_localhost_http {
        return Err("Security: Only HTTPS URLs are allowed for authentication (except localhost)".to_string());
    }

    // Get the session directory for this wiki (same as the wiki window uses)
    let session_dir = get_wiki_session_dir(&app, &wiki_path);

    // Create a unique label for the auth window
    let label = format!("auth-{}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis());

    // Build the auth window with security settings
    let mut builder = WebviewWindowBuilder::new(
        &app,
        &label,
        WebviewUrl::External(url.parse().map_err(|e| format!("Invalid URL: {}", e))?)
    )
    .title(format!("Login: {}", name))
    .inner_size(900.0, 700.0)
    .resizable(true)
    .center()
    // Security: Disable devtools in auth windows to prevent credential inspection
    .devtools(false);

    // Use the same session directory as the wiki
    if let Some(dir) = session_dir {
        builder = builder.data_directory(dir);
    }

    builder.build()
        .map_err(|e| format!("Failed to create auth window: {}", e))?;

    Ok(())
}

/// Open a wiki file in a new window
/// Returns WikiEntry so frontend can update its wiki list
#[tauri::command]
async fn open_wiki_window(app: tauri::AppHandle, path: String) -> Result<WikiEntry, String> {
    let path_buf = PathBuf::from(&path);
    let state = app.state::<AppState>();

    // Extract filename
    let filename = path_buf
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Unknown")
        .to_string();

    // Check if this wiki is already open
    {
        let open_wikis = state.open_wikis.lock().unwrap();
        for (label, wiki_path) in open_wikis.iter() {
            if wiki_path == &path {
                // Focus existing window
                if let Some(window) = app.get_webview_window(label) {
                    let _ = window.set_focus();
                    // Return entry even when focusing existing window
                    return Ok(WikiEntry {
                        path: path.clone(),
                        filename,
                        favicon: None,
                        is_folder: false,
                        backups_enabled: true,
                        backup_dir: None,
                        group: None,
                    });
                }
            }
        }
    }

    // Extract favicon - first try <head> link, then fall back to $:/favicon.ico tiddler
    let favicon = {
        if let Ok(content) = tokio::fs::read_to_string(&path_buf).await {
            extract_favicon(&content)
        } else {
            None
        }
    };

    // Create a unique key for this wiki path
    let path_key = base64_url_encode(&path);

    // Store the path mapping
    state.wiki_paths.lock().unwrap().insert(path_key.clone(), path_buf.clone());

    // Generate a unique window label
    let base_label = path_buf
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .replace(|c: char| !c.is_alphanumeric(), "-");

    // Ensure unique label
    let label = {
        let open_wikis = state.open_wikis.lock().unwrap();
        let mut label = format!("wiki-{}", base_label);
        let mut counter = 1;
        while open_wikis.contains_key(&label) {
            label = format!("wiki-{}-{}", base_label, counter);
            counter += 1;
        }
        label
    };

    // Track this wiki as open
    state.open_wikis.lock().unwrap().insert(label.clone(), path.clone());

    // Store label for this path so protocol handler can inject it
    state.wiki_paths.lock().unwrap().insert(format!("{}_label", path_key), PathBuf::from(&label));

    let title = path_buf
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("TiddlyWiki")
        .to_string();

    // Use wikifile:// protocol directly
    let wiki_url = format!("wikifile://localhost/{}", path_key);

    let icon = Image::from_bytes(include_bytes!("../icons/icon.png"))
        .map_err(|e| format!("Failed to load icon: {}", e))?;

    // Get isolated session directory for this wiki
    let session_dir = get_wiki_session_dir(&app, &path);

    let mut builder = WebviewWindowBuilder::new(&app, &label, WebviewUrl::External(wiki_url.parse().unwrap()))
        .title(&title)
        .inner_size(1200.0, 800.0)
        .icon(icon)
        .map_err(|e| format!("Failed to set icon: {}", e))?
        .window_classname("tiddlydesktop-rs")
        .initialization_script(get_dialog_init_script())
        .devtools(false);

    // Apply isolated session if available
    if let Some(dir) = session_dir {
        builder = builder.data_directory(dir);
    }

    let window = builder
        // NOT calling disable_drag_drop_handler() - we need tauri://drag-drop events
        // for external attachments support. File drops are handled via Tauri events.
        .build()
        .map_err(|e| format!("Failed to create window: {}", e))?;

    // Set up platform-specific drag handlers
    #[cfg(target_os = "linux")]
    linux_drag::setup_drag_handlers(&window);
    #[cfg(target_os = "windows")]
    windows_drag::setup_drag_handlers(&window);
    #[cfg(target_os = "macos")]
    macos_drag::setup_drag_handlers(&window);

    // Handle window close - JS onCloseRequested handles unsaved changes confirmation
    let app_handle = app.clone();
    let label_clone = label.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::Destroyed = event {
            let state = app_handle.state::<AppState>();
            state.open_wikis.lock().unwrap().remove(&label_clone);
        }
    });

    // Create the wiki entry
    let entry = WikiEntry {
        path,
        filename,
        favicon,
        is_folder: false,
        backups_enabled: true,
        backup_dir: None,
        group: None,
    };

    // Add to recent files list
    let _ = add_to_recent_files(&app, entry.clone());

    // Return the wiki entry so frontend can update its list
    Ok(entry)
}

/// Open a tiddler from a wiki in a new window (single-tiddler view)
/// The new window shares the same wiki and syncs changes via events
#[tauri::command]
async fn open_tiddler_window(
    app: tauri::AppHandle,
    parent_label: String,
    tiddler_title: String,
    template: Option<String>,
    window_title: Option<String>,
    width: Option<f64>,
    height: Option<f64>,
    left: Option<f64>,
    top: Option<f64>,
    variables: Option<String>, // JSON-encoded additional variables
) -> Result<String, String> {
    let state = app.state::<AppState>();

    // Get the wiki path from the parent window
    let wiki_path = {
        let open_wikis = state.open_wikis.lock().unwrap();
        open_wikis.get(&parent_label).cloned()
    }.ok_or_else(|| format!("Parent window '{}' not found", parent_label))?;

    // Create a unique key for this wiki path
    let path_key = base64_url_encode(&wiki_path);

    // Generate a unique window label for this tiddler window
    let safe_title = tiddler_title
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-')
        .take(30)
        .collect::<String>();

    let label = {
        let open_wikis = state.open_wikis.lock().unwrap();
        let mut label = format!("tiddler-{}-{}", safe_title, parent_label);
        let mut counter = 1;
        while open_wikis.contains_key(&label) {
            label = format!("tiddler-{}-{}-{}", safe_title, parent_label, counter);
            counter += 1;
        }
        label
    };

    // Build URL with query parameters for single-tiddler mode
    let encoded_tiddler = urlencoding::encode(&tiddler_title);
    let template_param = template.as_deref().unwrap_or("$:/core/templates/single.tiddler.window");
    let encoded_template = urlencoding::encode(template_param);
    let encoded_parent = urlencoding::encode(&parent_label);

    let mut wiki_url = format!(
        "wikifile://localhost/{}?tiddler={}&template={}&parent={}",
        path_key, encoded_tiddler, encoded_template, encoded_parent
    );

    // Add variables to URL if provided
    if let Some(vars) = &variables {
        let encoded_vars = urlencoding::encode(vars);
        wiki_url.push_str(&format!("&variables={}", encoded_vars));
    }

    // Track this window - map to same wiki path but with special marker
    state.open_wikis.lock().unwrap().insert(label.clone(), format!("{}#tiddler:{}", wiki_path, tiddler_title));

    // Store label for protocol handler
    state.wiki_paths.lock().unwrap().insert(format!("{}_label", path_key), PathBuf::from(&label));

    let title = window_title.unwrap_or_else(|| tiddler_title.clone());
    let win_width = width.unwrap_or(700.0);
    let win_height = height.unwrap_or(600.0);

    let icon = Image::from_bytes(include_bytes!("../icons/icon.png"))
        .map_err(|e| format!("Failed to load icon: {}", e))?;

    // Get isolated session directory - use the PARENT wiki's path so tiddler windows
    // share session with their parent wiki
    let session_dir = get_wiki_session_dir(&app, &wiki_path);

    let mut builder = WebviewWindowBuilder::new(&app, &label, WebviewUrl::External(wiki_url.parse().unwrap()))
        .title(&title)
        .inner_size(win_width, win_height)
        .icon(icon)
        .map_err(|e| format!("Failed to set icon: {}", e))?
        .window_classname("tiddlydesktop-rs")
        .initialization_script(get_dialog_init_script())
        .devtools(false);

    // Apply isolated session if available (shares with parent wiki)
    if let Some(dir) = session_dir {
        builder = builder.data_directory(dir);
    }

    // Set window position if specified
    if let (Some(x), Some(y)) = (left, top) {
        builder = builder.position(x, y);
    }

    let window = builder
        .build()
        .map_err(|e| format!("Failed to create tiddler window: {}", e))?;

    // Set up platform-specific drag handlers
    #[cfg(target_os = "linux")]
    linux_drag::setup_drag_handlers(&window);
    #[cfg(target_os = "windows")]
    windows_drag::setup_drag_handlers(&window);
    #[cfg(target_os = "macos")]
    macos_drag::setup_drag_handlers(&window);

    // Handle window close
    let app_handle = app.clone();
    let label_clone = label.clone();
    window.on_window_event(move |event| {
        if let tauri::WindowEvent::Destroyed = event {
            let state = app_handle.state::<AppState>();
            state.open_wikis.lock().unwrap().remove(&label_clone);
        }
    });

    Ok(label)
}

/// Simple base64 URL-safe encoding for path keys
fn base64_url_encode(input: &str) -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    URL_SAFE_NO_PAD.encode(input.as_bytes())
}

/// Decode base64 URL-safe string
fn base64_url_decode(input: &str) -> Option<String> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    URL_SAFE_NO_PAD
        .decode(input)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
}

/// Get the resource directory, preferring paths relative to executable for tarball installs
/// This avoids baked-in CI paths like /home/runner/...
fn get_resource_dir_path(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            // Tarball structure: bin/tiddlydesktop-rs with resources at ../lib/tiddlydesktop-rs/
            let tarball_resources = exe_dir.join("..").join("lib").join("tiddlydesktop-rs");
            if tarball_resources.exists() {
                if let Ok(canonical) = tarball_resources.canonicalize() {
                    return Some(canonical);
                }
            }

            // AppImage/installed structure: resources might be in ../lib/<app-name>
            // or alongside the binary
            let lib_resources = exe_dir.join("..").join("lib").join("tiddlydesktop-rs");
            if lib_resources.exists() {
                if let Ok(canonical) = lib_resources.canonicalize() {
                    return Some(canonical);
                }
            }
        }
    }

    // Fall back to Tauri's resource_dir (may have baked-in paths from CI)
    app.path().resource_dir().ok()
}

/// Get the base data directory, respecting portable mode
/// Checks for portable marker files in exe directory on all platforms
/// Falls back to app_data_dir for installed mode
fn get_data_dir(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    if let Ok(exe_path) = std::env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            // Check for portable marker files
            if exe_dir.join("portable").exists() || exe_dir.join("portable.txt").exists() {
                return Some(exe_dir.to_path_buf());
            }
            // Check if portable data file already exists (user chose portable mode previously)
            if exe_dir.join("tiddlydesktop.html").exists() {
                return Some(exe_dir.to_path_buf());
            }
        }
    }

    // Default: use app data directory (installed mode)
    app.path().app_data_dir().ok()
}

/// Get an isolated session data directory for a wiki
/// Each wiki gets its own session storage (cookies, localStorage, etc.)
/// This prevents cross-wiki data leakage from plugins/scripts
fn get_wiki_session_dir(app: &tauri::AppHandle, wiki_path: &str) -> Option<std::path::PathBuf> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    // Create a hash of the wiki path for a shorter directory name
    let mut hasher = DefaultHasher::new();
    wiki_path.hash(&mut hasher);
    let hash = hasher.finish();

    // Get data directory (respects portable mode)
    if let Some(data_dir) = get_data_dir(app) {
        let session_dir = data_dir.join("wiki_sessions").join(format!("{:016x}", hash));
        // Create the directory if it doesn't exist
        if let Err(e) = std::fs::create_dir_all(&session_dir) {
            eprintln!("[TiddlyDesktop] Failed to create session directory: {}", e);
            return None;
        }
        Some(session_dir)
    } else {
        None
    }
}

/// Parse query string into a HashMap
fn parse_query_string(query: Option<&str>) -> std::collections::HashMap<String, String> {
    let mut params = std::collections::HashMap::new();
    if let Some(q) = query {
        for pair in q.split('&') {
            if let Some((key, value)) = pair.split_once('=') {
                let key = urlencoding::decode(key).unwrap_or_default().to_string();
                let value = urlencoding::decode(value).unwrap_or_default().to_string();
                params.insert(key, value);
            }
        }
    }
    params
}

/// Handle wiki:// protocol requests
fn wiki_protocol_handler(app: &tauri::AppHandle, request: Request<Vec<u8>>) -> Response<Vec<u8>> {
    let uri = request.uri();
    let full_path = uri.path().trim_start_matches('/');

    // Extract path without query string and parse query params
    let (path, query_params) = {
        let query = uri.query();
        let path = full_path.split('?').next().unwrap_or(full_path);
        (path, parse_query_string(query))
    };

    // Single-tiddler mode params
    let single_tiddler = query_params.get("tiddler").cloned();
    let single_template = query_params.get("template").cloned();
    let parent_window = query_params.get("parent").cloned();
    let single_variables = query_params.get("variables").cloned(); // JSON-encoded extra variables

    // Handle OPTIONS preflight requests for CORS (required for PUT requests on some platforms)
    if request.method() == "OPTIONS" {
        return Response::builder()
            .status(200)
            .header("Access-Control-Allow-Origin", "*")
            .header("Access-Control-Allow-Methods", "GET, PUT, POST, OPTIONS")
            .header("Access-Control-Allow-Headers", "Content-Type")
            .header("Access-Control-Max-Age", "86400")
            .body(Vec::new())
            .unwrap();
    }

    // Handle title-sync requests: wikifile://title-sync/{label}/{title}
    if path.starts_with("title-sync/") {
        let parts: Vec<&str> = path.strip_prefix("title-sync/").unwrap().splitn(2, '/').collect();
        if parts.len() == 2 {
            let label = urlencoding::decode(parts[0]).unwrap_or_default().to_string();
            let title = urlencoding::decode(parts[1]).unwrap_or_default().to_string();

            // Update window title
            let app_clone = app.clone();
            let app_inner = app_clone.clone();
            let _ = app_clone.run_on_main_thread(move || {
                if let Some(window) = app_inner.get_webview_window(&label) {
                    let _ = window.set_title(&title);
                }
            });
        }
        return Response::builder()
            .status(200)
            .header("Access-Control-Allow-Origin", "*")
            .body(Vec::new())
            .unwrap();
    }

    // Handle save requests: wikifile://save/{base64-encoded-path}
    // Body contains the wiki content
    if path.starts_with("save/") {
        let path_key = path.strip_prefix("save/").unwrap();
        let wiki_path = match base64_url_decode(path_key) {
            Some(decoded) => PathBuf::from(decoded),
            None => {
                return Response::builder()
                    .status(400)
                    .body("Invalid path".as_bytes().to_vec())
                    .unwrap();
            }
        };

        let content = String::from_utf8_lossy(request.body()).to_string();

        // Check if backups should be created for this wiki
        let state = app.state::<AppState>();
        let wiki_path_str = wiki_path.to_string_lossy();
        let should_backup = should_create_backup(app, &state, wiki_path_str.as_ref());

        // Create backup if appropriate (synchronous since protocol handlers can't be async)
        if should_backup && wiki_path.exists() {
            if let Some(parent) = wiki_path.parent() {
                let filename = wiki_path.file_stem().and_then(|s| s.to_str()).unwrap_or("wiki");

                // Get custom backup directory if set, otherwise use default
                let backup_dir = match get_wiki_backup_dir(app, wiki_path_str.as_ref()) {
                    Some(custom_dir) => PathBuf::from(custom_dir),
                    None => parent.join(format!("{}.backups", filename)),
                };
                let _ = std::fs::create_dir_all(&backup_dir);

                let timestamp = Local::now().format("%Y%m%d-%H%M%S");
                let backup_name = format!("{}.{}.html", filename, timestamp);
                let backup_path = backup_dir.join(backup_name);
                let _ = std::fs::copy(&wiki_path, &backup_path);
            }
        }

        // Write to temp file then rename for atomic operation
        let temp_path = wiki_path.with_extension("tmp");
        match std::fs::write(&temp_path, &content) {
            Ok(_) => {
                match std::fs::rename(&temp_path, &wiki_path) {
                    Ok(_) => {
                        return Response::builder()
                            .status(200)
                            .header("Access-Control-Allow-Origin", "*")
                            .body(Vec::new())
                            .unwrap();
                    }
                    Err(_rename_err) => {
                        // On Windows, rename can fail if file is locked
                        // Fall back to direct write after removing temp file
                        let _ = std::fs::remove_file(&temp_path);
                        match std::fs::write(&wiki_path, &content) {
                            Ok(_) => {
                                return Response::builder()
                                    .status(200)
                                    .header("Access-Control-Allow-Origin", "*")
                                    .body(Vec::new())
                                    .unwrap();
                            }
                            Err(e) => {
                                return Response::builder()
                                    .status(500)
                                    .body(format!("Failed to save: {}", e).into_bytes())
                                    .unwrap();
                            }
                        }
                    }
                }
            }
            Err(e) => {
                return Response::builder()
                    .status(500)
                    .body(format!("Failed to write: {}", e).into_bytes())
                    .unwrap();
            }
        }
    }

    // Look up the actual file path
    let state = app.state::<AppState>();
    let paths = state.wiki_paths.lock().unwrap();

    let file_path = match paths.get(path) {
        Some(p) => p.clone(),
        None => {
            match base64_url_decode(path) {
                Some(decoded) => PathBuf::from(decoded),
                None => {
                    // Not a base64-encoded wiki path - this might be a _canonical_uri file request
                    // Get the wiki directory from the Referer header
                    drop(paths); // Release lock before handling file request

                    let referer = request.headers()
                        .get("referer")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");

                    // Extract wiki path from referer: wikifile://localhost/{base64_wiki_path}
                    let wiki_dir = if let Some(ref_path) = referer.strip_prefix("wikifile://localhost/") {
                        // The referer path might have query params or fragments, strip them
                        let ref_path = ref_path.split('?').next().unwrap_or(ref_path);
                        let ref_path = ref_path.split('#').next().unwrap_or(ref_path);

                        if let Some(decoded_wiki_path) = base64_url_decode(ref_path) {
                            PathBuf::from(&decoded_wiki_path)
                                .parent()
                                .map(|p| p.to_path_buf())
                        } else {
                            None
                        }
                    } else {
                        None
                    };

                    // Resolve the file path
                    let resolved_path = if is_absolute_filesystem_path(path) {
                        // Absolute path - use directly
                        PathBuf::from(path)
                    } else if let Some(wiki_dir) = wiki_dir {
                        // Relative path - resolve relative to wiki directory
                        wiki_dir.join(path)
                    } else {
                        // No wiki context and not absolute - can't resolve
                        return Response::builder()
                            .status(404)
                            .header("Access-Control-Allow-Origin", "*")
                            .body("File not found: no wiki context for relative path".as_bytes().to_vec())
                            .unwrap();
                    };

                    // Serve the file
                    match std::fs::read(&resolved_path) {
                        Ok(content) => {
                            let mime_type = get_mime_type(&resolved_path);
                            return Response::builder()
                                .status(200)
                                .header("Content-Type", mime_type)
                                .header("Access-Control-Allow-Origin", "*")
                                .body(content)
                                .unwrap();
                        }
                        Err(e) => {
                            return Response::builder()
                                .status(404)
                                .header("Access-Control-Allow-Origin", "*")
                                .body(format!("File not found: {} ({})", resolved_path.display(), e).as_bytes().to_vec())
                                .unwrap();
                        }
                    }
                }
            }
        }
    };

    // Get the window label for this path
    let window_label = paths.get(&format!("{}_label", path))
        .and_then(|p| p.to_str())
        .unwrap_or("")
        .to_string();

    drop(paths); // Release the lock before file I/O

    // Check if this is the main wiki
    let is_main_wiki = file_path == state.main_wiki_path;

    // Generate the save URL for this wiki
    let save_url = format!("wikifile://localhost/save/{}", path);

    // Prepare single-tiddler mode params for injection
    let single_tiddler_js = single_tiddler.as_ref()
        .map(|t| format!(r#"window.__SINGLE_TIDDLER__ = "{}";"#, t.replace('\\', "\\\\").replace('"', "\\\"")))
        .unwrap_or_default();
    let single_template_js = single_template.as_ref()
        .map(|t| format!(r#"window.__SINGLE_TEMPLATE__ = "{}";"#, t.replace('\\', "\\\\").replace('"', "\\\"")))
        .unwrap_or_default();
    let parent_window_js = parent_window.as_ref()
        .map(|p| format!(r#"window.__PARENT_WINDOW__ = "{}";"#, p.replace('\\', "\\\\").replace('"', "\\\"")))
        .unwrap_or_default();
    let single_variables_js = single_variables.as_ref()
        .map(|v| format!(r#"window.__SINGLE_VARIABLES__ = {};"#, v)) // Already JSON
        .unwrap_or_default();

    // Read file content
    let read_result = std::fs::read_to_string(&file_path);

    match read_result {
        Ok(content) => {
            // Inject variables and a custom saver for TiddlyWiki
            let script_injection = format!(
                r##"<script>
window.__WIKI_PATH__ = "{}";
window.__WINDOW_LABEL__ = "{}";
window.__SAVE_URL__ = "{}";
window.__IS_MAIN_WIKI__ = {};
{}
{}
{}
{}

// TiddlyDesktop initialization - handles both normal and encrypted wikis
(function() {{
    var SAVE_URL = "{}";

    // Check if this is an encrypted wiki
    function isEncryptedWiki() {{
        return !!document.getElementById('encryptedStoreArea');
    }}

    // Wait for TiddlyWiki to be fully ready (including decryption if needed)
    function waitForTiddlyWiki(callback) {{
        // For encrypted wikis, we must wait for $tw.wiki to exist
        // This means decryption has completed and boot has finished
        if (typeof $tw !== 'undefined' && $tw.wiki) {{
            callback();
        }} else {{
            setTimeout(function() {{ waitForTiddlyWiki(callback); }}, 50);
        }}
    }}

    // Main initialization that runs after TiddlyWiki is ready
    function initializeTiddlyDesktop() {{

    // Define the saver module globally so TiddlyWiki can find it during boot
    window.$TiddlyDesktopSaver = {{
        info: {{
            name: 'tiddlydesktop',
            priority: 5000,
            capabilities: ['save', 'autosave']
        }},
        canSave: function(wiki) {{
            return true;
        }},
        create: function(wiki) {{
            return {{
                wiki: wiki,
                info: {{
                    name: 'tiddlydesktop',
                    priority: 5000,
                    capabilities: ['save', 'autosave']
                }},
                canSave: function(wiki) {{
                    return true;
                }},
                save: function(text, method, callback) {{
                    var wikiPath = window.__WIKI_PATH__;

                    // Try Tauri IPC first (works reliably on all platforms)
                    if(window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke) {{
                        window.__TAURI__.core.invoke('save_wiki', {{
                            path: wikiPath,
                            content: text
                        }}).then(function() {{
                            callback(null);
                        }}).catch(function(err) {{
                            // IPC failed, try fetch as fallback
                            saveViaFetch(text, callback);
                        }});
                    }} else {{
                        // No Tauri IPC, use fetch
                        saveViaFetch(text, callback);
                    }}

                    function saveViaFetch(content, cb) {{
                        fetch(SAVE_URL, {{
                            method: 'PUT',
                            body: content
                        }}).then(function(response) {{
                            if(response.ok) {{
                                cb(null);
                            }} else {{
                                response.text().then(function(errText) {{
                                    cb('Save failed (HTTP ' + response.status + '): ' + (errText || response.statusText));
                                }}).catch(function() {{
                                    cb('Save failed: HTTP ' + response.status);
                                }});
                            }}
                        }}).catch(function(err) {{
                            cb('Save failed (fetch): ' + err.toString());
                        }});
                    }}

                    return true;
                }}
            }};
        }}
    }};

    // Hook into TiddlyWiki's module registration
    function registerWithTiddlyWiki() {{
        if(typeof $tw === 'undefined') {{
            setTimeout(registerWithTiddlyWiki, 50);
            return;
        }}

        // Register as a module if modules system exists
        if($tw.modules && $tw.modules.types) {{
            $tw.modules.types['saver'] = $tw.modules.types['saver'] || {{}};
            $tw.modules.types['saver']['$:/plugins/tiddlydesktop/saver'] = window.$TiddlyDesktopSaver;
            console.log('TiddlyDesktop saver: registered as module');
        }}

        // Wait for saverHandler and add directly
        function addToSaverHandler() {{
            if(!$tw.saverHandler) {{
                setTimeout(addToSaverHandler, 50);
                return;
            }}

            // Check if already added
            var alreadyAdded = $tw.saverHandler.savers.some(function(s) {{
                return s.info && s.info.name === 'tiddlydesktop';
            }});

            if(!alreadyAdded) {{
                var saver = window.$TiddlyDesktopSaver.create($tw.wiki);
                // Add to array and re-sort (TiddlyWiki iterates backwards, so highest priority must be at the END)
                $tw.saverHandler.savers.push(saver);
                $tw.saverHandler.savers.sort(function(a, b) {{
                    if(a.info.priority < b.info.priority) {{
                        return -1;
                    }} else if(a.info.priority > b.info.priority) {{
                        return 1;
                    }}
                    return 0;
                }});
            }}
        }}

        addToSaverHandler();
    }}

    registerWithTiddlyWiki();

    // Title sync - update window title when document title changes
    (function() {{
        var windowLabel = window.__WINDOW_LABEL__;
        var lastTitle = '';

        function syncTitle() {{
            var title = document.title;
            if (title && title !== lastTitle && window.__TAURI__ && window.__TAURI__.core) {{
                lastTitle = title;
                window.__TAURI__.core.invoke('set_window_title', {{
                    label: windowLabel,
                    title: title
                }}).catch(function() {{}});
            }}
        }}

        // Sync when DOM is ready
        if (document.readyState === 'loading') {{
            document.addEventListener('DOMContentLoaded', syncTitle);
        }} else {{
            syncTitle();
        }}

        // Hook into TiddlyWiki's change system when available
        function hookTiddlyWiki() {{
            if (typeof $tw !== 'undefined' && $tw.wiki) {{
                $tw.wiki.addEventListener('change', function() {{
                    setTimeout(syncTitle, 10);
                }});
            }} else {{
                setTimeout(hookTiddlyWiki, 100);
            }}
        }}
        hookTiddlyWiki();

        // Fallback: periodic sync
        setInterval(syncTitle, 2000);
    }})();

    // Favicon extraction for encrypted wikis
    // When the wiki is encrypted, we can't extract the favicon from the HTML during load
    // After decryption, extract it from $:/favicon.ico and send to Rust
    (function() {{
        var wikiPath = window.__WIKI_PATH__;

        function extractAndUpdateFavicon() {{
            if (typeof $tw === 'undefined' || !$tw.wiki) {{
                setTimeout(extractAndUpdateFavicon, 100);
                return;
            }}

            // Get the favicon tiddler
            var faviconTiddler = $tw.wiki.getTiddler('$:/favicon.ico');
            if (!faviconTiddler || !faviconTiddler.fields.text) {{
                return; // No favicon tiddler
            }}

            var text = faviconTiddler.fields.text;
            var type = faviconTiddler.fields.type || 'image/x-icon';

            // Build data URI
            var dataUri;
            if (text.startsWith('data:')) {{
                dataUri = text; // Already a data URI
            }} else {{
                // Assume base64 encoded
                dataUri = 'data:' + type + ';base64,' + text;
            }}

            // Send to Rust to update the wiki list entry
            if (window.__TAURI__ && window.__TAURI__.core) {{
                window.__TAURI__.core.invoke('update_wiki_favicon', {{
                    path: wikiPath,
                    favicon: dataUri
                }}).catch(function(err) {{
                    console.error('TiddlyDesktop: Failed to update favicon:', err);
                }});
            }}
        }}

        // Run once TiddlyWiki is ready
        extractAndUpdateFavicon();
    }})();

    // Single-tiddler window mode
    if (window.__SINGLE_TIDDLER__) {{
        (function() {{
            var tiddlerTitle = window.__SINGLE_TIDDLER__;
            var templateTitle = window.__SINGLE_TEMPLATE__ || '$:/core/templates/single.tiddler.window';
            var parentWindow = window.__PARENT_WINDOW__;

            function renderSingleTiddler() {{
                if (typeof $tw === 'undefined' || !$tw.wiki || !$tw.rootWidget) {{
                    setTimeout(renderSingleTiddler, 50);
                    return;
                }}

                // Hide the normal TiddlyWiki UI
                var pageContainer = document.querySelector('.tc-page-container');
                if (pageContainer) {{
                    pageContainer.style.display = 'none';
                }}

                // Create container for single tiddler view
                var container = document.createElement('div');
                container.className = 'tc-single-tiddler-window tc-body';
                document.body.appendChild(container);

                // Set up variables for the template
                var variables = {{
                    currentTiddler: tiddlerTitle,
                    'tv-window-id': window.__WINDOW_LABEL__
                }};

                // Merge any additional variables passed via paramObject
                if (window.__SINGLE_VARIABLES__) {{
                    var extraVars = window.__SINGLE_VARIABLES__;
                    for (var key in extraVars) {{
                        if (extraVars.hasOwnProperty(key)) {{
                            variables[key] = extraVars[key];
                        }}
                    }}
                }}

                // Render styles
                var styleWidgetNode = $tw.wiki.makeTranscludeWidget('$:/core/ui/PageStylesheet', {{
                    document: $tw.fakeDocument,
                    variables: variables,
                    importPageMacros: true
                }});
                var styleContainer = $tw.fakeDocument.createElement('style');
                styleWidgetNode.render(styleContainer, null);
                var styleElement = document.createElement('style');
                styleElement.innerHTML = styleContainer.textContent;
                document.head.appendChild(styleElement);

                // Render the tiddler using the template
                var parser = $tw.wiki.parseTiddler(templateTitle);
                var widgetNode = $tw.wiki.makeWidget(parser, {{
                    document: document,
                    parentWidget: $tw.rootWidget,
                    variables: variables
                }});
                widgetNode.render(container, null);

                // Set up refresh handler
                $tw.wiki.addEventListener('change', function(changes) {{
                    if (styleWidgetNode.refresh(changes, styleContainer, null)) {{
                        styleElement.innerHTML = styleContainer.textContent;
                    }}
                    widgetNode.refresh(changes);
                }});

                // Listen for keyboard shortcuts
                document.addEventListener('keydown', function(event) {{
                    if ($tw.keyboardManager) {{
                        $tw.keyboardManager.handleKeydownEvent(event);
                    }}
                }});

                // Handle popups
                document.documentElement.addEventListener('click', function(event) {{
                    if ($tw.popup) {{
                        $tw.popup.handleEvent(event);
                    }}
                }}, true);

                console.log('Single-tiddler window initialized for:', tiddlerTitle);
            }}

            renderSingleTiddler();
        }})();
    }}

    }} // End of initializeTiddlyDesktop

    // Start initialization based on whether wiki is encrypted
    // We need to wait for DOM to check for encryptedStoreArea
    function startInit() {{
        if (isEncryptedWiki()) {{
            // Encrypted wiki: wait for TiddlyWiki to fully boot (including decryption)
            console.log('TiddlyDesktop: Encrypted wiki detected, waiting for decryption...');
            waitForTiddlyWiki(function() {{
                console.log('TiddlyDesktop: Decryption complete, initializing...');
                initializeTiddlyDesktop();
            }});
        }} else {{
            // Normal wiki: initialize immediately (our code waits for $tw internally)
            initializeTiddlyDesktop();
        }}
    }}

    // Check DOM readiness before looking for encryptedStoreArea
    if (document.readyState === 'loading') {{
        document.addEventListener('DOMContentLoaded', startInit);
    }} else {{
        startInit();
    }}

    // External attachments support is provided by the initialization script (get_dialog_init_script)
}})();
</script>"##,
                file_path.display().to_string().replace('\\', "\\\\").replace('"', "\\\""),
                window_label.replace('\\', "\\\\").replace('"', "\\\""),
                save_url,
                is_main_wiki,
                single_tiddler_js,
                single_template_js,
                parent_window_js,
                single_variables_js,
                save_url
            );

            // Find <head> tag position - only search first 4KB, don't lowercase the whole file
            let search_area = &content[..content.len().min(4096)];
            let head_pos = search_area.find("<head")
                .or_else(|| search_area.find("<HEAD"))
                .or_else(|| search_area.find("<Head"));

            // Build response efficiently without extra allocations
            let mut response_bytes = Vec::with_capacity(content.len() + script_injection.len() + 100);

            if let Some(head_start) = head_pos {
                if let Some(close_offset) = content[head_start..].find('>') {
                    let insert_pos = head_start + close_offset + 1;
                    response_bytes.extend_from_slice(content[..insert_pos].as_bytes());
                    response_bytes.extend_from_slice(script_injection.as_bytes());
                    response_bytes.extend_from_slice(content[insert_pos..].as_bytes());
                } else {
                    response_bytes.extend_from_slice(script_injection.as_bytes());
                    response_bytes.extend_from_slice(content.as_bytes());
                }
            } else {
                response_bytes.extend_from_slice(script_injection.as_bytes());
                response_bytes.extend_from_slice(content.as_bytes());
            }

            Response::builder()
                .status(200)
                .header("Content-Type", "text/html; charset=utf-8")
                .header("Access-Control-Allow-Origin", "*")
                .body(response_bytes)
                .unwrap()
        }
        Err(e) => Response::builder()
            .status(500)
            .body(format!("Failed to read wiki: {}", e).into_bytes())
            .unwrap(),
    }
}

/// Reveal the main window, or recreate it if it was closed
fn reveal_or_create_main_window(app_handle: &tauri::AppHandle) {
    // Try to get existing window first
    if let Some(window) = app_handle.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }

    // Window was closed - recreate it
    let state = app_handle.state::<AppState>();
    let main_wiki_path = state.main_wiki_path.clone();
    let path_key = base64_url_encode(&main_wiki_path.to_string_lossy());
    let wiki_url = format!("wikifile://localhost/{}", path_key);

    if let Ok(icon) = Image::from_bytes(include_bytes!("../icons/icon.png")) {
        if let Ok(main_window) = WebviewWindowBuilder::new(
            app_handle,
            "main",
            WebviewUrl::External(wiki_url.parse().unwrap())
        )
            .title("TiddlyDesktopRS")
            .inner_size(800.0, 600.0)
            .icon(icon)
            .expect("Failed to set icon")
            .initialization_script(get_dialog_init_script())
            .build()
        {
            // Set up platform-specific drag handlers
            #[cfg(target_os = "linux")]
            linux_drag::setup_drag_handlers(&main_window);
            #[cfg(target_os = "windows")]
            windows_drag::setup_drag_handlers(&main_window);
            #[cfg(target_os = "macos")]
            macos_drag::setup_drag_handlers(&main_window);

            let _ = main_window.set_focus();
        }
    }
}

fn setup_system_tray(app: &tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let open_wiki = MenuItemBuilder::with_id("open_wiki", "Open Wiki...").build(app)?;
    let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;

    let menu = MenuBuilder::new(app)
        .item(&open_wiki)
        .separator()
        .item(&quit)
        .build()?;

    let _tray = TrayIconBuilder::new()
        .icon(Image::from_bytes(include_bytes!("../icons/32x32.png"))?)
        .menu(&menu)
        .tooltip("TiddlyDesktop")
        .on_menu_event(|app, event| {
            match event.id().as_ref() {
                "open_wiki" => {
                    reveal_or_create_main_window(app);
                }
                "quit" => {
                    app.exit(0);
                }
                _ => {}
            }
        })
        .on_tray_icon_event(|tray, event| {
            // Handle double-click on tray icon - reveal the main window
            if let tauri::tray::TrayIconEvent::DoubleClick { .. } = event {
                reveal_or_create_main_window(tray.app_handle());
            }
        })
        .build(app)?;

    Ok(())
}

pub fn run() {
    // Enable hardware acceleration on all platforms
    // Linux: WebKitGTK environment variables
    #[cfg(target_os = "linux")]
    {
        // Ensure hardware-accelerated compositing is enabled
        std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "0");
        // Enable DMA-BUF renderer for better hardware acceleration
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "0");
    }

    tauri::Builder::default()
        .setup(|app| {
            // Ensure main wiki exists (creates from template if needed)
            // This also handles first-run mode selection on macOS/Linux
            let main_wiki_path = ensure_main_wiki_exists(app)
                .map_err(|e| Box::new(std::io::Error::new(std::io::ErrorKind::Other, e)) as Box<dyn std::error::Error>)?;

            println!("Main wiki path: {:?}", main_wiki_path);

            // Initialize app state
            app.manage(AppState {
                wiki_paths: Mutex::new(HashMap::new()),
                open_wikis: Mutex::new(HashMap::new()),
                wiki_servers: Mutex::new(HashMap::new()),
                next_port: Mutex::new(8080),
                main_wiki_path: main_wiki_path.clone(),
            });

            // Create a unique key for the main wiki path
            let path_key = base64_url_encode(&main_wiki_path.to_string_lossy());

            // Store the path mapping for the protocol handler
            let state = app.state::<AppState>();
            state.wiki_paths.lock().unwrap().insert(path_key.clone(), main_wiki_path.clone());
            state.wiki_paths.lock().unwrap().insert(format!("{}_label", path_key), PathBuf::from("main"));

            // Track main wiki as open
            state.open_wikis.lock().unwrap().insert("main".to_string(), main_wiki_path.to_string_lossy().to_string());

            // Use wikifile:// protocol to load main wiki
            let wiki_url = format!("wikifile://localhost/{}", path_key);

            // Create the main window programmatically with initialization script
            let icon = Image::from_bytes(include_bytes!("../icons/icon.png"))?;
            let main_window = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(wiki_url.parse().unwrap()))
                .title("TiddlyDesktopRS")
                .inner_size(800.0, 600.0)
                .icon(icon)?
                .window_classname("tiddlydesktop-rs")
                .initialization_script(get_dialog_init_script())
                .devtools(false)
                .build()?;

            // Set up platform-specific drag handlers
            #[cfg(target_os = "linux")]
            linux_drag::setup_drag_handlers(&main_window);
            #[cfg(target_os = "windows")]
            windows_drag::setup_drag_handlers(&main_window);
            #[cfg(target_os = "macos")]
            macos_drag::setup_drag_handlers(&main_window);

            setup_system_tray(app)?;

            // Handle files passed as command-line arguments
            let args: Vec<String> = std::env::args().skip(1).collect();
            for arg in args {
                let path = PathBuf::from(&arg);
                // Only open files that exist and have .html or .htm extension
                if path.exists() && path.is_file() {
                    if let Some(ext) = path.extension() {
                        let ext_lower = ext.to_string_lossy().to_lowercase();
                        if ext_lower == "html" || ext_lower == "htm" {
                            let app_handle = app.handle().clone();
                            let path_str = arg.clone();
                            tauri::async_runtime::spawn(async move {
                                if let Ok(entry) = open_wiki_window(app_handle.clone(), path_str).await {
                                    // Emit event to refresh wiki list in main window
                                    let _ = app_handle.emit("wiki-list-changed", entry);
                                }
                            });
                        }
                    }
                }
            }

            Ok(())
        })
        .register_uri_scheme_protocol("wikifile", |ctx, request| {
            wiki_protocol_handler(ctx.app_handle(), request)
        })
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .invoke_handler(tauri::generate_handler![
            load_wiki,
            save_wiki,
            open_wiki_window,
            open_wiki_folder,
            open_tiddler_window,
            check_is_wiki_folder,
            check_folder_status,
            get_available_editions,
            get_available_plugins,
            init_wiki_folder,
            create_wiki_file,
            set_window_title,
            get_window_label,
            get_main_wiki_path,
            reveal_in_folder,
            show_alert,
            show_confirm,
            close_window,
            close_window_by_label,
            get_recent_files,
            remove_recent_file,
            set_wiki_backups,
            set_wiki_backup_dir,
            update_wiki_favicon,
            get_wiki_backup_dir_setting,
            set_wiki_group,
            get_wiki_groups,
            rename_wiki_group,
            delete_wiki_group,
            read_file_as_data_uri,
            read_file_as_binary,
            pick_files_for_import,
            get_external_attachments_config,
            set_external_attachments_config,
            get_session_auth_config,
            set_session_auth_config,
            open_auth_window,
            run_command,
            show_find_in_page,
            js_log,
            get_clipboard_content
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // Handle files opened via macOS file associations
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Opened { urls } = event {
                for url in urls {
                    if let Ok(path) = url.to_file_path() {
                        if let Some(ext) = path.extension() {
                            let ext_lower = ext.to_string_lossy().to_lowercase();
                            if ext_lower == "html" || ext_lower == "htm" {
                                let app_handle = app.clone();
                                let path_str = path.to_string_lossy().to_string();
                                tauri::async_runtime::spawn(async move {
                                    if let Ok(entry) = open_wiki_window(app_handle.clone(), path_str).await {
                                        // Emit event to refresh wiki list in main window
                                        let _ = app_handle.emit("wiki-list-changed", entry);
                                    }
                                });
                            }
                        }
                    }
                }
            }

            // Suppress unused variable warnings on non-macOS platforms
            #[cfg(not(target_os = "macos"))]
            let _ = (app, event);
        });
}
