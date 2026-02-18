//! Clipboard content handling for paste and copy operations
//!
//! This module provides platform-specific clipboard access for both reading
//! (paste) and writing (copy) operations.

use std::collections::HashMap;

/// Clipboard content data structure (same format as drag-drop)
#[derive(serde::Serialize)]
pub struct ClipboardContentData {
    pub types: Vec<String>,
    pub data: HashMap<String, String>,
}

/// Get clipboard content for paste handling
/// Returns content in the same format as drag-drop for consistent processing
#[tauri::command]
pub fn get_clipboard_content() -> Result<ClipboardContentData, String> {
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
    #[cfg(target_os = "android")]
    {
        // Android clipboard access not yet implemented
        Err("Clipboard not implemented on Android".to_string())
    }
}

#[cfg(target_os = "linux")]
fn get_clipboard_content_linux() -> Result<ClipboardContentData, String> {
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
            let looks_like_utf16le =
                raw_data[1] == 0 && raw_data[3] == 0 && raw_data[0] != 0 && raw_data[2] != 0;
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

            let looks_like_utf16be =
                raw_data[0] == 0 && raw_data[2] == 0 && raw_data[1] != 0 && raw_data[3] != 0;
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

    // Request formats in priority order (file URIs first for paste-from-file-manager)
    // File manager clipboard formats by DE:
    //   GNOME/Nautilus + XFCE/Thunar: x-special/gnome-copied-files
    //   KDE/Dolphin: x-special/kde-copied-files or x-special/KDE-copied-files
    //   MATE/Caja: x-special/mate-copied-files
    //   All DEs also typically provide: text/uri-list
    let formats_to_try = [
        "text/uri-list",
        "x-special/gnome-copied-files",
        "x-special/kde-copied-files",
        "x-special/KDE-copied-files",
        "x-special/mate-copied-files",
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
                            let mime_type =
                                if clipboard_type == "UTF8_STRING" || clipboard_type == "STRING" {
                                    "text/plain"
                                } else {
                                    clipboard_type
                                };

                            if !types.contains(&mime_type.to_string()) {
                                types.push(mime_type.to_string());
                                data.insert(mime_type.to_string(), text);
                                eprintln!(
                                    "[TiddlyDesktop] Clipboard: Got {} ({} chars)",
                                    mime_type,
                                    data.get(mime_type).map(|s| s.len()).unwrap_or(0)
                                );
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

    eprintln!(
        "[TiddlyDesktop] Clipboard: Returning {} types",
        types.len()
    );
    Ok(ClipboardContentData { types, data })
}

#[cfg(target_os = "windows")]
fn get_clipboard_content_windows() -> Result<ClipboardContentData, String> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, OpenClipboard, RegisterClipboardFormatA,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
    use windows::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT};

    let mut types = Vec::new();
    let mut data = HashMap::new();

    unsafe {
        if OpenClipboard(None).is_err() {
            return Err("Failed to open clipboard".to_string());
        }

        // Get CF_HDROP (file list from file manager copy)
        if let Ok(h) = GetClipboardData(CF_HDROP.0 as u32) {
            if !h.0.is_null() {
                let hdrop = windows::Win32::UI::Shell::HDROP(h.0);
                // Get number of files
                let count =
                    windows::Win32::UI::Shell::DragQueryFileW(hdrop, 0xFFFFFFFF, None);
                if count > 0 {
                    let mut uri_list = String::new();
                    for i in 0..count {
                        // Get required buffer size
                        let len = windows::Win32::UI::Shell::DragQueryFileW(
                            hdrop,
                            i,
                            None,
                        );
                        if len > 0 {
                            let mut buf = vec![0u16; (len + 1) as usize];
                            windows::Win32::UI::Shell::DragQueryFileW(
                                hdrop,
                                i,
                                Some(&mut buf),
                            );
                            let path = OsString::from_wide(&buf[..len as usize])
                                .to_string_lossy()
                                .to_string();
                            if !path.is_empty() {
                                // Convert backslashes and build file:// URI
                                let forward = path.replace('\\', "/");
                                if !uri_list.is_empty() {
                                    uri_list.push('\n');
                                }
                                uri_list.push_str("file:///");
                                uri_list.push_str(&forward);
                            }
                        }
                    }
                    if !uri_list.is_empty() {
                        types.push("text/uri-list".to_string());
                        data.insert("text/uri-list".to_string(), uri_list);
                        eprintln!(
                            "[TiddlyDesktop] Clipboard: Got CF_HDROP ({} files)",
                            count
                        );
                    }
                }
            }
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
                                eprintln!(
                                    "[TiddlyDesktop] Clipboard: Got text/html ({} chars)",
                                    content.len()
                                );
                            }
                        } else if !html.is_empty() {
                            types.push("text/html".to_string());
                            data.insert("text/html".to_string(), html.clone());
                            eprintln!(
                                "[TiddlyDesktop] Clipboard: Got text/html ({} chars)",
                                html.len()
                            );
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
                    let text = OsString::from_wide(&slice[..len])
                        .to_string_lossy()
                        .to_string();

                    if !text.is_empty() {
                        types.push("text/plain".to_string());
                        data.insert("text/plain".to_string(), text.clone());
                        eprintln!(
                            "[TiddlyDesktop] Clipboard: Got text/plain ({} chars)",
                            text.len()
                        );
                    }

                    let _ = GlobalUnlock(HGLOBAL(h.0));
                }
            }
        }

        let _ = CloseClipboard();
    }

    eprintln!(
        "[TiddlyDesktop] Clipboard: Returning {} types",
        types.len()
    );
    Ok(ClipboardContentData { types, data })
}

#[cfg(target_os = "macos")]
fn get_clipboard_content_macos() -> Result<ClipboardContentData, String> {
    use objc2_app_kit::NSPasteboard;
    use objc2_foundation::NSString;

    let mut types = Vec::new();
    let mut data = HashMap::new();

    let pasteboard = NSPasteboard::generalPasteboard();

    // Check for file URLs first (files copied from Finder)
    let file_url_type = NSString::from_str("public.file-url");
    if let Some(ns_str) = pasteboard.stringForType(&file_url_type) {
        let url_str = ns_str.to_string();
        if !url_str.is_empty() && url_str.starts_with("file://") {
            types.push("text/uri-list".to_string());
            data.insert("text/uri-list".to_string(), url_str.clone());
            eprintln!(
                "[TiddlyDesktop] Clipboard: Got public.file-url ({} chars)",
                url_str.len()
            );
        }
    }

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
                eprintln!(
                    "[TiddlyDesktop] Clipboard: Got {} ({} chars)",
                    mime_type, len
                );
            }
        }
    }

    eprintln!(
        "[TiddlyDesktop] Clipboard: Returning {} types",
        types.len()
    );
    Ok(ClipboardContentData { types, data })
}

/// Set clipboard content (copy to clipboard)
/// This is used to override TiddlyWiki's tm-copy-to-clipboard which uses
/// document.execCommand("copy") that doesn't work reliably in webviews.
#[tauri::command]
pub fn set_clipboard_content(text: String) -> Result<bool, String> {
    #[cfg(target_os = "linux")]
    {
        set_clipboard_content_linux(&text)
    }
    #[cfg(target_os = "windows")]
    {
        set_clipboard_content_windows(&text)
    }
    #[cfg(target_os = "macos")]
    {
        set_clipboard_content_macos(&text)
    }
    #[cfg(target_os = "android")]
    {
        // Android clipboard access not yet implemented
        let _ = text; // Silence unused warning
        Err("Clipboard not implemented on Android".to_string())
    }
}

#[cfg(target_os = "linux")]
fn set_clipboard_content_linux(text: &str) -> Result<bool, String> {
    let display = gdk::Display::default().ok_or("No display")?;
    let clipboard = gtk::Clipboard::default(&display).ok_or("No clipboard")?;

    clipboard.set_text(text);

    // Store the clipboard content so it persists after the app loses focus
    clipboard.store();

    eprintln!(
        "[TiddlyDesktop] Clipboard: Set text ({} chars)",
        text.len()
    );
    Ok(true)
}

#[cfg(target_os = "windows")]
fn set_clipboard_content_windows(text: &str) -> Result<bool, String> {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    unsafe {
        if OpenClipboard(None).is_err() {
            return Err("Failed to open clipboard".to_string());
        }

        if EmptyClipboard().is_err() {
            let _ = CloseClipboard();
            return Err("Failed to empty clipboard".to_string());
        }

        // Convert to UTF-16 with null terminator
        let wide: Vec<u16> = OsStr::new(text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let size = wide.len() * std::mem::size_of::<u16>();

        let h_mem = GlobalAlloc(GMEM_MOVEABLE, size).map_err(|e| format!("GlobalAlloc failed: {}", e))?;

        let ptr = GlobalLock(h_mem) as *mut u16;
        if ptr.is_null() {
            let _ = CloseClipboard();
            return Err("GlobalLock failed".to_string());
        }

        std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
        let _ = GlobalUnlock(h_mem);

        let result = SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(h_mem.0)));
        let _ = CloseClipboard();

        if result.is_err() {
            return Err("SetClipboardData failed".to_string());
        }

        eprintln!(
            "[TiddlyDesktop] Clipboard: Set text ({} chars)",
            text.len()
        );
        Ok(true)
    }
}

#[cfg(target_os = "macos")]
fn set_clipboard_content_macos(text: &str) -> Result<bool, String> {
    use objc2_app_kit::NSPasteboard;
    use objc2_foundation::NSString;

    let pasteboard = NSPasteboard::generalPasteboard();

    // Clear the pasteboard
    pasteboard.clearContents();

    // Set the string
    let ns_string = NSString::from_str(text);
    let success = pasteboard.setString_forType(&ns_string, unsafe {
        &*objc2_app_kit::NSPasteboardTypeString
    });

    if success {
        eprintln!(
            "[TiddlyDesktop] Clipboard: Set text ({} chars)",
            text.len()
        );
        Ok(true)
    } else {
        Err("Failed to set clipboard content".to_string())
    }
}
