use std::{collections::HashMap, path::PathBuf, process::{Child, Command}, sync::Mutex};
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

/// Windows flag to prevent console window from appearing
#[cfg(target_os = "windows")]
const CREATE_NO_WINDOW: u32 = 0x08000000;
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
    /// Recently opened wiki entries
    recent_files: Mutex<Vec<WikiEntry>>,
    /// Path to the data file for persistence
    data_file: PathBuf,
    /// Running wiki folder servers (keyed by window label)
    wiki_servers: Mutex<HashMap<String, WikiFolderServer>>,
    /// Next available port for wiki folder servers
    next_port: Mutex<u16>,
}

const MAX_RECENT_FILES: usize = 50;

/// Get the path to the wiki list data file
fn get_data_file_path(app: &tauri::App) -> PathBuf {
    let app_data_dir = app.path().app_data_dir().expect("Failed to get app data dir");
    std::fs::create_dir_all(&app_data_dir).ok();
    app_data_dir.join("wiki-list.json")
}

/// Load wiki list from disk
fn load_wiki_list(data_file: &PathBuf) -> Vec<WikiEntry> {
    if data_file.exists() {
        match std::fs::read_to_string(data_file) {
            Ok(content) => {
                match serde_json::from_str(&content) {
                    Ok(entries) => return entries,
                    Err(e) => eprintln!("Failed to parse wiki list: {}", e),
                }
            }
            Err(e) => eprintln!("Failed to read wiki list: {}", e),
        }
    }
    Vec::new()
}

/// Save wiki list to disk
fn save_wiki_list(data_file: &PathBuf, entries: &[WikiEntry]) {
    match serde_json::to_string_pretty(entries) {
        Ok(json) => {
            if let Err(e) = std::fs::write(data_file, json) {
                eprintln!("Failed to save wiki list: {}", e);
            }
        }
        Err(e) => eprintln!("Failed to serialize wiki list: {}", e),
    }
}

/// Extract favicon from wiki HTML content
/// Only searches the first 64KB since favicon is always in <head>
fn extract_favicon(content: &str) -> Option<String> {
    // Only search the head section - favicon is always there
    // Limit search to first 64KB to avoid scanning large files
    let search_limit = content.len().min(65536);
    let search_content = &content[..search_limit];

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

    None
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

/// Add to recent files with optional favicon extraction
fn add_to_recent(state: &AppState, path: &str, favicon: Option<String>) {
    let mut recent = state.recent_files.lock().unwrap();

    // Check if already exists and preserve favicon and backup settings
    let existing_entry = recent.iter()
        .find(|e| e.path == path)
        .cloned();

    // Remove if already exists
    recent.retain(|e| e.path != path);

    // Extract filename
    let filename = PathBuf::from(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Unknown")
        .to_string();

    // Use provided favicon, or keep existing one
    let final_favicon = favicon.or(existing_entry.as_ref().and_then(|e| e.favicon.clone()));

    // Preserve backup setting from existing entry, default to true
    let backups_enabled = existing_entry.map(|e| e.backups_enabled).unwrap_or(true);

    // Add to front
    recent.insert(0, WikiEntry {
        path: path.to_string(),
        filename,
        favicon: final_favicon,
        is_folder: false, // Single file wikis are not folders
        backups_enabled,
    });

    // Trim to max size
    recent.truncate(MAX_RECENT_FILES);

    // Save to disk
    save_wiki_list(&state.data_file, &recent);
}

/// Get recent files
#[tauri::command]
fn get_recent_files(state: tauri::State<AppState>) -> Vec<WikiEntry> {
    state.recent_files.lock().unwrap().clone()
}

/// Remove a file from recent list
#[tauri::command]
fn remove_recent_file(state: tauri::State<AppState>, path: String) {
    let mut recent = state.recent_files.lock().unwrap();
    recent.retain(|e| e.path != path);
    // Save to disk
    save_wiki_list(&state.data_file, &recent);
}

/// Toggle backups for a wiki
#[tauri::command]
fn set_wiki_backups(state: tauri::State<AppState>, path: String, enabled: bool) {
    let mut recent = state.recent_files.lock().unwrap();
    if let Some(entry) = recent.iter_mut().find(|e| e.path == path) {
        entry.backups_enabled = enabled;
        // Save to disk
        save_wiki_list(&state.data_file, &recent);
    }
}

/// Check if backups are enabled for a wiki path
fn are_backups_enabled(state: &AppState, path: &str) -> bool {
    let recent = state.recent_files.lock().unwrap();
    recent.iter()
        .find(|e| e.path == path)
        .map(|e| e.backups_enabled)
        .unwrap_or(true) // Default to enabled if not found
}

/// Update favicon for a wiki
#[tauri::command]
fn update_wiki_favicon(state: tauri::State<AppState>, path: String, favicon: String) {
    let mut recent = state.recent_files.lock().unwrap();
    if let Some(entry) = recent.iter_mut().find(|e| e.path == path) {
        entry.favicon = Some(favicon);
        // Save to disk
        save_wiki_list(&state.data_file, &recent);
    }
}

/// Check if running on mobile (Android/iOS)
#[tauri::command]
fn is_mobile() -> bool {
    false
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
fn get_dialog_init_script() -> &'static str {
    r#"
    (function() {
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
#[tauri::command]
async fn open_wiki_folder(app: tauri::AppHandle, path: String) -> Result<(), String> {
    let path_buf = PathBuf::from(&path);
    let state = app.state::<AppState>();

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
                    return Ok(());
                }
            }
        }
    }

    // Ensure required plugins and autosave are enabled
    ensure_wiki_folder_config(&path_buf);

    // Allocate a port for this server
    let port = allocate_port(&state);

    // Add to recent files
    let folder_name = path_buf
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Unknown")
        .to_string();

    {
        let mut recent = state.recent_files.lock().unwrap();
        // Remove if already exists
        recent.retain(|e| e.path != path);
        // Add to front with is_folder flag
        recent.insert(0, WikiEntry {
            path: path.clone(),
            filename: folder_name.clone(),
            favicon: None, // Folders don't have embedded favicons
            is_folder: true,
            backups_enabled: false, // Not applicable for folder wikis (they use autosave)
        });
        recent.truncate(MAX_RECENT_FILES);
        save_wiki_list(&state.data_file, &recent);
    }

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
    let mut child = cmd.spawn()
        .map_err(|e| format!("Failed to start TiddlyWiki server: {}", e))?;

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
        .initialization_script(get_dialog_init_script())
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

    Ok(())
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
}

/// Get list of available TiddlyWiki editions
#[tauri::command]
async fn get_available_editions(app: tauri::AppHandle) -> Result<Vec<EditionInfo>, String> {
    let tw_path = get_tiddlywiki_path(&app)?;
    let editions_dir = tw_path.parent()
        .ok_or("Failed to get TiddlyWiki directory")?
        .join("editions");

    if !editions_dir.exists() {
        return Err("Editions directory not found".to_string());
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

    let mut editions = Vec::new();

    // First add the common/recommended editions in order
    let priority_editions = ["server", "empty", "full", "dev"];
    for edition_id in &priority_editions {
        let edition_path = editions_dir.join(edition_id);
        if edition_path.exists() && edition_path.join("tiddlywiki.info").exists() {
            if let Some((name, desc)) = edition_metadata.get(*edition_id) {
                editions.push(EditionInfo {
                    id: edition_id.to_string(),
                    name: name.to_string(),
                    description: desc.to_string(),
                });
            }
        }
    }

    // Then add other editions
    if let Ok(entries) = std::fs::read_dir(&editions_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    // Skip if already added or if it doesn't have tiddlywiki.info
                    if priority_editions.contains(&name) {
                        continue;
                    }
                    if !path.join("tiddlywiki.info").exists() {
                        continue;
                    }
                    // Skip test/internal editions
                    if name.starts_with("test") || name == "pluginlibrary" {
                        continue;
                    }

                    let (display_name, description) = edition_metadata
                        .get(name)
                        .map(|(n, d)| (n.to_string(), d.to_string()))
                        .unwrap_or_else(|| {
                            (name.replace('-', " ").replace('_', " "), format!("{} edition", name))
                        });

                    editions.push(EditionInfo {
                        id: name.to_string(),
                        name: display_name,
                        description,
                    });
                }
            }
        }
    }

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

/// Open a wiki file in a new window
#[tauri::command]
async fn open_wiki_window(app: tauri::AppHandle, path: String) -> Result<(), String> {
    let path_buf = PathBuf::from(&path);
    let state = app.state::<AppState>();

    // Check if this wiki is already open
    {
        let open_wikis = state.open_wikis.lock().unwrap();
        for (label, wiki_path) in open_wikis.iter() {
            if wiki_path == &path {
                // Focus existing window
                if let Some(window) = app.get_webview_window(label) {
                    let _ = window.set_focus();
                    return Ok(());
                }
            }
        }
    }

    // Read only the first 64KB to extract favicon (it's always in <head>)
    let favicon = {
        use tokio::io::AsyncReadExt;
        let mut buffer = vec![0u8; 65536];
        if let Ok(mut file) = tokio::fs::File::open(&path_buf).await {
            if let Ok(bytes_read) = file.read(&mut buffer).await {
                buffer.truncate(bytes_read);
                String::from_utf8(buffer).ok().and_then(|s| extract_favicon(&s))
            } else {
                None
            }
        } else {
            None
        }
    };

    // Add to recent files with favicon
    add_to_recent(&state, &path, favicon);

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

    Ok(())
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

        // Check if backups are enabled for this wiki
        let state = app.state::<AppState>();
        let backups_enabled = are_backups_enabled(&state, wiki_path.to_string_lossy().as_ref());

        // Create backup if enabled (synchronous since protocol handlers can't be async)
        if backups_enabled && wiki_path.exists() {
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
                    return Response::builder()
                        .status(404)
                        .body("Wiki not found".as_bytes().to_vec())
                        .unwrap();
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
            // Get data file path and load wiki list
            let data_file = get_data_file_path(app);
            let wiki_list = load_wiki_list(&data_file);

            // Initialize app state with loaded data
            app.manage(AppState {
                wiki_paths: Mutex::new(HashMap::new()),
                open_wikis: Mutex::new(HashMap::new()),
                recent_files: Mutex::new(wiki_list),
                data_file,
                wiki_servers: Mutex::new(HashMap::new()),
                next_port: Mutex::new(8080),
            });

            // Create the main window programmatically with initialization script
            let icon = Image::from_bytes(include_bytes!("../icons/icon.png"))?;
            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("TiddlyDesktopRS")
                .inner_size(800.0, 600.0)
                .icon(icon)?
                .window_classname("tiddlydesktop-rs")
                .initialization_script(get_dialog_init_script())
                .build()?;

            setup_system_tray(app)?;

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
            get_recent_files,
            remove_recent_file,
            set_wiki_backups,
            reveal_in_folder,
            update_wiki_favicon,
            is_mobile,
            show_alert,
            show_confirm,
            close_window
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
