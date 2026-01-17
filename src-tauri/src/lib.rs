use std::{collections::HashMap, path::PathBuf, sync::Mutex};
use chrono::Local;
use serde::{Deserialize, Serialize};
use tauri::{
    http::{Request, Response},
    image::Image,
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
fn extract_favicon(content: &str) -> Option<String> {
    // Look for favicon link with data URI
    // Common patterns:
    // <link id="faviconLink" rel="shortcut icon" href="data:image/...">
    // <link rel="icon" href="data:image/...">

    // Find favicon link elements
    for pattern in &["<link", "<LINK"] {
        let mut search_pos = 0;
        while let Some(link_start) = content[search_pos..].find(pattern) {
            let abs_start = search_pos + link_start;
            if let Some(link_end) = content[abs_start..].find('>') {
                let link_tag = &content[abs_start..abs_start + link_end + 1];
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
async fn load_wiki(path: String) -> Result<String, String> {
    tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| format!("Failed to read wiki: {}", e))
}

/// Save wiki content to disk with backup
#[tauri::command]
async fn save_wiki(path: String, content: String) -> Result<(), String> {
    let path = PathBuf::from(&path);

    // Create backup first
    create_backup(&path).await?;

    // Write to a temp file first, then rename for atomic operation
    let temp_path = path.with_extension("tmp");

    tokio::fs::write(&temp_path, &content)
        .await
        .map_err(|e| format!("Failed to write temp file: {}", e))?;

    tokio::fs::rename(&temp_path, &path)
        .await
        .map_err(|e| format!("Failed to rename temp file: {}", e))?;

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

    // Check if already exists and preserve/update favicon
    let existing_favicon = recent.iter()
        .find(|e| e.path == path)
        .and_then(|e| e.favicon.clone());

    // Remove if already exists
    recent.retain(|e| e.path != path);

    // Extract filename
    let filename = PathBuf::from(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Unknown")
        .to_string();

    // Use provided favicon, or keep existing one
    let final_favicon = favicon.or(existing_favicon);

    // Add to front
    recent.insert(0, WikiEntry {
        path: path.to_string(),
        filename,
        favicon: final_favicon,
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

/// Reveal file in system file manager
#[tauri::command]
async fn reveal_in_folder(path: String) -> Result<(), String> {
    let path_buf = std::path::PathBuf::from(&path);
    let folder = path_buf.parent().unwrap_or(&path_buf);

    #[cfg(target_os = "linux")]
    {
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

    // Read wiki content and extract favicon
    let favicon = tokio::fs::read_to_string(&path_buf)
        .await
        .ok()
        .and_then(|content| extract_favicon(&content));

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
        .build()
        .map_err(|e| format!("Failed to create window: {}", e))?;

    // Handle window close - remove from open_wikis
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

        // Create backup and save synchronously (protocol handlers can't be async)
        if wiki_path.exists() {
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
                    Err(e) => {
                        return Response::builder()
                            .status(500)
                            .body(format!("Failed to save: {}", e).into_bytes())
                            .unwrap();
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

    match std::fs::read_to_string(&file_path) {
        Ok(content) => {
            // Inject __WIKI_PATH__ and __WINDOW_LABEL__ for the saver and title sync
            let script_injection = format!(
                r#"<script>window.__WIKI_PATH__ = "{}"; window.__WINDOW_LABEL__ = "{}";</script>"#,
                file_path.display().to_string().replace('\\', "\\\\").replace('"', "\\\""),
                window_label.replace('\\', "\\\\").replace('"', "\\\"")
            );

            // Insert script after <head...>
            let modified_content = if let Some(head_pos) = content.to_lowercase().find("<head") {
                if let Some(close_pos) = content[head_pos..].find('>') {
                    let insert_pos = head_pos + close_pos + 1;
                    format!("{}{}{}", &content[..insert_pos], script_injection, &content[insert_pos..])
                } else {
                    format!("{}{}", script_injection, content)
                }
            } else {
                format!("{}{}", script_injection, content)
            };

            Response::builder()
                .status(200)
                .header("Content-Type", "text/html; charset=utf-8")
                .header("Access-Control-Allow-Origin", "*")
                .body(modified_content.into_bytes())
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
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
            });

            // Set icon for the main window
            if let Some(window) = app.get_webview_window("main") {
                let icon = Image::from_bytes(include_bytes!("../icons/icon.png"))?;
                window.set_icon(icon)?;
            }

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
            set_window_title,
            get_window_label,
            get_recent_files,
            remove_recent_file,
            reveal_in_folder,
            update_wiki_favicon
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
