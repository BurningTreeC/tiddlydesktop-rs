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
use chrono::Local;
use serde::{Deserialize, Serialize};
use tauri::{
    image::Image,
    http::{Request, Response},
    menu::{MenuBuilder, MenuItemBuilder},
    tray::TrayIconBuilder,
    Manager, WebviewUrl, WebviewWindowBuilder,
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
}

fn default_backups_enabled() -> bool {
    true
}

/// Get the path to the recent files JSON
fn get_recent_files_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    Ok(data_dir.join("recent_wikis.json"))
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
fn add_to_recent_files(app: &tauri::AppHandle, entry: WikiEntry) -> Result<(), String> {
    let mut entries = load_recent_files_from_disk(app);

    // Remove existing entry with same path (if any)
    entries.retain(|e| e.path != entry.path);

    // Add new entry at the beginning
    entries.insert(0, entry);

    // Keep only the most recent 50 entries
    entries.truncate(50);

    save_recent_files_to_disk(app, &entries)
}

/// Get recent files list
#[tauri::command]
fn get_recent_files(app: tauri::AppHandle) -> Vec<WikiEntry> {
    load_recent_files_from_disk(&app)
}

/// Remove a wiki from the recent files list
#[tauri::command]
fn remove_recent_file(app: tauri::AppHandle, path: String) -> Result<(), String> {
    let mut entries = load_recent_files_from_disk(&app);
    entries.retain(|e| e.path != path);
    save_recent_files_to_disk(&app, &entries)
}

/// Set backups enabled/disabled for a wiki
#[tauri::command]
fn set_wiki_backups(app: tauri::AppHandle, path: String, enabled: bool) -> Result<(), String> {
    let mut entries = load_recent_files_from_disk(&app);

    for entry in entries.iter_mut() {
        if entry.path == path {
            entry.backups_enabled = enabled;
            break;
        }
    }

    save_recent_files_to_disk(&app, &entries)
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
    let resource_path = app.path().resource_dir().map_err(|e| e.to_string())?;
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
async fn create_backup(path: &PathBuf) -> Result<(), String> {
    if !path.exists() {
        return Ok(()); // No backup needed for new files
    }

    let parent = path.parent().ok_or("No parent directory")?;
    let filename = path.file_stem().and_then(|s| s.to_str()).unwrap_or("wiki");

    // Create .backups folder next to the wiki
    let backup_dir = parent.join(format!("{}.backups", filename));
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
async fn save_wiki(_app: tauri::AppHandle, path: String, content: String) -> Result<(), String> {
    let path_buf = PathBuf::from(&path);

    // Create backup first
    create_backup(&path_buf).await?;

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

/// Set window title
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

/// Check if backups should be created for a wiki path
/// Backups are enabled for all user wikis, but not for the main TiddlyDesktop wiki
fn should_create_backup(state: &AppState, path: &str) -> bool {
    // Don't backup the main TiddlyDesktop wiki
    let main_wiki = state.main_wiki_path.to_string_lossy();
    if path == main_wiki {
        return false;
    }
    // Enable backups for all other wikis
    true
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
                var loadingTiddlers = {};
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

                function resolveFilesystemPath(path) {
                    if(isAbsolutePath(path)) {
                        return path;
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
                    return basePath + separator + path.replace(/[/\\]/g, separator);
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

                        console.log('[TiddlyDesktop] httpRequest for filesystem path:', resolvedPath);
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

                // Handle _canonical_uri with filesystem paths (absolute or relative)
                function loadTiddlerContent(title) {
                    var tiddler = $tw.wiki.getTiddler(title);
                    if(!tiddler) return;

                    var uri = tiddler.fields._canonical_uri;
                    if(!uri || !isFilesystemPath(uri)) return;
                    if(loadingTiddlers[title]) return;

                    var resolvedPath = resolveFilesystemPath(uri);
                    if(!resolvedPath) return;

                    loadingTiddlers[title] = true;
                    console.log('[TiddlyDesktop] Loading _canonical_uri:', resolvedPath, 'for:', title);

                    invoke('read_file_as_data_uri', { path: resolvedPath })
                        .then(function(dataUri) {
                            var match = dataUri.match(/^data:([^;]+);base64,(.+)$/);
                            if(match) {
                                var newFields = Object.assign({}, tiddler.fields, {
                                    text: match[2]
                                });
                                delete newFields._canonical_uri;
                                // Use syncer.storeTiddler to avoid marking wiki as dirty
                                if($tw.syncer && $tw.syncer.storeTiddler) {
                                    $tw.syncer.storeTiddler(newFields);
                                } else {
                                    $tw.wiki.addTiddler(new $tw.Tiddler(newFields));
                                }
                                console.log('[TiddlyDesktop] Loaded content for:', title);
                            }
                            delete loadingTiddlers[title];
                        })
                        .catch(function(err) {
                            console.error('[TiddlyDesktop] Failed to load:', uri, err);
                            delete loadingTiddlers[title];
                        });
                }

                // Process existing tiddlers with absolute _canonical_uri
                $tw.wiki.each(function(tiddler, title) {
                    loadTiddlerContent(title);
                });

                // Listen for new/changed tiddlers
                $tw.wiki.addEventListener('change', function(changes) {
                    $tw.utils.each(changes, function(change, title) {
                        if(!change.deleted) {
                            loadTiddlerContent(title);
                        }
                    });
                });

                console.log('[TiddlyDesktop] Filesystem support installed');
            }

            waitForTiddlyWiki();
        }

        setupFilesystemSupport();

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
    let resource_path = app.path().resource_dir()
        .map_err(|e| format!("Failed to get resource dir: {}", e))?;
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
    let resource_path = app.path().resource_dir()
        .map_err(|e| format!("Failed to get resource dir: {}", e))?;
    let resource_path = normalize_path(resource_path);

    let tw_path = resource_path.join("resources").join("tiddlywiki").join("tiddlywiki.js");

    // Also check in the development path
    let dev_path = PathBuf::from("src-tauri/resources/tiddlywiki/tiddlywiki.js");

    if tw_path.exists() {
        Ok(tw_path)
    } else if dev_path.exists() {
        let canonical = dev_path.canonicalize().map_err(|e| e.to_string())?;
        Ok(normalize_path(canonical))
    } else {
        Err(format!("TiddlyWiki not found at {:?} or {:?}", tw_path, dev_path))
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
    let window = WebviewWindowBuilder::new(&app, &label, WebviewUrl::External(server_url.parse().unwrap()))
        .title(&folder_name)
        .inner_size(1200.0, 800.0)
        .icon(icon)
        .map_err(|e| format!("Failed to set icon: {}", e))?
        .window_classname("tiddlydesktop-rs")
        .initialization_script(&get_init_script_with_path(&path))
        .disable_drag_drop_handler() // Enable HTML5 drag & drop in webview
        .build()
        .map_err(|e| format!("Failed to create window: {}", e))?;

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
    let utility_plugins = ["markdown", "highlight", "katex", "jszip", "xlsx-utils", "qrcode", "innerwiki"];
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
                            if id == "tiddlyweb" || id == "filesystem" || id.starts_with("test") {
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
    let window = WebviewWindowBuilder::new(&app, &label, WebviewUrl::External(wiki_url.parse().unwrap()))
        .title(&title)
        .inner_size(1200.0, 800.0)
        .icon(icon)
        .map_err(|e| format!("Failed to set icon: {}", e))?
        .window_classname("tiddlydesktop-rs")
        .initialization_script(get_dialog_init_script())
        .disable_drag_drop_handler() // Enable HTML5 drag & drop in webview
        .build()
        .map_err(|e| format!("Failed to create window: {}", e))?;

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
    };

    // Add to recent files list
    let _ = add_to_recent_files(&app, entry.clone());

    // Return the wiki entry so frontend can update its list
    Ok(entry)
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

/// Handle wiki:// protocol requests
fn wiki_protocol_handler(app: &tauri::AppHandle, request: Request<Vec<u8>>) -> Response<Vec<u8>> {
    let uri = request.uri();
    let path = uri.path().trim_start_matches('/');

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
        let should_backup = should_create_backup(&state, wiki_path.to_string_lossy().as_ref());

        // Create backup if appropriate (synchronous since protocol handlers can't be async)
        if should_backup && wiki_path.exists() {
            if let Some(parent) = wiki_path.parent() {
                let filename = wiki_path.file_stem().and_then(|s| s.to_str()).unwrap_or("wiki");
                let backup_dir = parent.join(format!("{}.backups", filename));
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

// TiddlyDesktop custom saver - registers as a proper TiddlyWiki module before boot
(function() {{
    var SAVE_URL = "{}";

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
                console.log('TiddlyDesktop saver: added to saverHandler, total savers:', $tw.saverHandler.savers.length);
                console.log('TiddlyDesktop saver: savers are (saveWiki checks from end):', $tw.saverHandler.savers.map(function(s) {{
                    return s.info ? s.info.name + ' (pri:' + s.info.priority + ')' : 'unknown';
                }}).join(', '));
            }}
        }}

        addToSaverHandler();
    }}

    registerWithTiddlyWiki();
}})();
</script>"##,
                file_path.display().to_string().replace('\\', "\\\\").replace('"', "\\\""),
                window_label.replace('\\', "\\\\").replace('"', "\\\""),
                save_url,
                is_main_wiki,
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
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.set_focus();
                    }
                }
                "quit" => {
                    app.exit(0);
                }
                _ => {}
            }
        })
        .build(app)?;

    Ok(())
}

pub fn run() {
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
            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(wiki_url.parse().unwrap()))
                .title("TiddlyDesktopRS")
                .inner_size(800.0, 600.0)
                .icon(icon)?
                .window_classname("tiddlydesktop-rs")
                .initialization_script(get_dialog_init_script())
                .build()?;

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
                                let _ = open_wiki_window(app_handle, path_str).await;
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
            get_recent_files,
            remove_recent_file,
            set_wiki_backups,
            read_file_as_data_uri
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
                                    let _ = open_wiki_window(app_handle, path_str).await;
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
