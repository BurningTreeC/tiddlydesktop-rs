//! Rust-based TiddlyWeb-compatible HTTP server for wiki folders
//!
//! This provides wiki folder functionality on Android where Node.js isn't available.
//! Implements the TiddlyWeb API that TiddlyWiki's tiddlyweb plugin expects.
//!
//! On Android, uses Tauri's fs plugin to handle content:// URIs directly.

use std::collections::HashMap;
#[allow(unused_imports)] // Used by request.as_reader().read_to_string()
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use tiny_http::{Server, Request, Response, Header, Method};
use tauri_plugin_fs::FsExt;

/// A running wiki folder server
pub struct WikiFolderHttpServer {
    #[allow(dead_code)] // Handle kept alive to maintain server thread
    server_handle: Option<thread::JoinHandle<()>>,
    shutdown_flag: Arc<Mutex<bool>>,
    port: u16,
    #[allow(dead_code)] // Stored for potential future use (status display, debugging)
    wiki_path: PathBuf,
}

impl WikiFolderHttpServer {
    /// Start a new wiki folder server
    ///
    /// On Android, pass the app_handle to enable content:// URI support via fs plugin.
    /// On desktop, app_handle can be None to use standard filesystem.
    pub fn start(wiki_path: PathBuf, port: u16, app_handle: Option<tauri::AppHandle>) -> Result<Self, String> {
        let server = Server::http(format!("127.0.0.1:{}", port))
            .map_err(|e| format!("Failed to start HTTP server: {}", e))?;

        let shutdown_flag = Arc::new(Mutex::new(false));
        let shutdown_clone = shutdown_flag.clone();
        let path_clone = wiki_path.clone();

        let handle = thread::spawn(move || {
            run_server(server, path_clone, shutdown_clone, app_handle);
        });

        Ok(Self {
            server_handle: Some(handle),
            shutdown_flag,
            port,
            wiki_path,
        })
    }

    /// Get the server URL
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Stop the server
    pub fn stop(&mut self) {
        *self.shutdown_flag.lock().unwrap() = true;
        // The server will stop on the next request or timeout
    }
}

impl Drop for WikiFolderHttpServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_server(
    server: Server,
    wiki_path: PathBuf,
    shutdown_flag: Arc<Mutex<bool>>,
    app_handle: Option<tauri::AppHandle>,
) {
    // Load tiddlers into memory for faster access
    let tiddlers = Arc::new(Mutex::new(load_tiddlers(&wiki_path, app_handle.as_ref())));
    let app_handle = Arc::new(app_handle);

    loop {
        // Check shutdown flag
        if *shutdown_flag.lock().unwrap() {
            break;
        }

        // Wait for request with timeout
        match server.recv_timeout(std::time::Duration::from_millis(500)) {
            Ok(Some(request)) => {
                handle_request(request, &wiki_path, &tiddlers, &app_handle);
            }
            Ok(None) => continue, // Timeout, check shutdown flag
            Err(e) => {
                eprintln!("Server error: {}", e);
                break;
            }
        }
    }
}

fn handle_request(
    request: Request,
    wiki_path: &Path,
    tiddlers: &Arc<Mutex<HashMap<String, Tiddler>>>,
    app_handle: &Arc<Option<tauri::AppHandle>>,
) {
    let url = request.url().to_string();
    let method = request.method().clone();

    println!("[WikiServer] {} {}", method, url);

    // PUT requests need special handling because they consume the request body
    if method == Method::Put && url.starts_with("/recipes/default/tiddlers/") {
        let title = urlencoding::decode(&url[26..]).unwrap_or_default().to_string();
        handle_put_tiddler(request, &title, wiki_path, tiddlers, app_handle);
        return;
    }

    let response = match (&method, url.as_str()) {
        // TiddlyWeb API endpoints
        (Method::Get, "/status") => handle_status(),
        (Method::Get, "/recipes/default/tiddlers.json") => handle_get_tiddlers(tiddlers),
        (Method::Get, path) if path.starts_with("/recipes/default/tiddlers/") => {
            let title = urlencoding::decode(&path[26..]).unwrap_or_default().to_string();
            handle_get_tiddler(&title, tiddlers)
        }
        (Method::Delete, path) if path.starts_with("/recipes/default/tiddlers/") => {
            let title = urlencoding::decode(&path[26..]).unwrap_or_default().to_string();
            handle_delete_tiddler(&title, wiki_path, tiddlers, app_handle)
        }
        // Static file serving
        (Method::Get, "/") => serve_index(wiki_path, app_handle),
        (Method::Get, "/favicon.ico") => serve_favicon(wiki_path, app_handle),
        (Method::Get, path) => serve_static_file(wiki_path, path, app_handle),
        // OPTIONS for CORS
        (Method::Options, _) => handle_options(),
        _ => Response::from_string("Not Found").with_status_code(404),
    };

    let _ = request.respond(response);
}

// ============================================================================
// Tiddler Management
// ============================================================================

#[derive(Clone, Debug)]
struct Tiddler {
    title: String,
    fields: HashMap<String, String>,
    text: String,
}

impl Tiddler {
    fn to_json(&self) -> String {
        let mut obj: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();

        obj.insert("title".to_string(), serde_json::Value::String(self.title.clone()));

        for (key, value) in &self.fields {
            obj.insert(key.clone(), serde_json::Value::String(value.clone()));
        }

        if !self.text.is_empty() {
            obj.insert("text".to_string(), serde_json::Value::String(self.text.clone()));
        }

        serde_json::to_string(&obj).unwrap_or_default()
    }

    fn to_skinny_json(&self) -> String {
        // Skinny tiddlers don't include the text field
        let mut obj: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();

        obj.insert("title".to_string(), serde_json::Value::String(self.title.clone()));

        for (key, value) in &self.fields {
            if key != "text" {
                obj.insert(key.clone(), serde_json::Value::String(value.clone()));
            }
        }

        serde_json::to_string(&obj).unwrap_or_default()
    }
}

/// Load all tiddlers from the wiki folder
fn load_tiddlers(wiki_path: &Path, app_handle: Option<&tauri::AppHandle>) -> HashMap<String, Tiddler> {
    let mut tiddlers = HashMap::new();
    let tiddlers_dir = wiki_path.join("tiddlers");

    // Read directory entries - use fs plugin if available, else std::fs
    let entries: Vec<(String, bool)> = if let Some(app) = app_handle {
        match app.fs().read_dir(&tiddlers_dir) {
            Ok(iter) => iter
                .filter_map(|e| e.ok())
                .map(|e| (e.name.clone(), e.is_file))
                .collect(),
            Err(e) => {
                println!("[WikiServer] Could not read tiddlers dir: {}", e);
                return tiddlers;
            }
        }
    } else {
        match std::fs::read_dir(&tiddlers_dir) {
            Ok(iter) => iter
                .filter_map(|e| e.ok())
                .map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    let is_file = e.file_type().map(|t| t.is_file()).unwrap_or(false);
                    (name, is_file)
                })
                .collect(),
            Err(_) => return tiddlers,
        }
    };

    for (name, is_file) in entries {
        if !is_file {
            continue;
        }

        let file_path = tiddlers_dir.join(&name);

        if name.ends_with(".tid") {
            if let Some(content) = read_file_content(&file_path, app_handle) {
                if let Some(tiddler) = parse_tid_content(&content, &name) {
                    tiddlers.insert(tiddler.title.clone(), tiddler);
                }
            }
        } else if name.ends_with(".json") {
            if let Some(content) = read_file_content(&file_path, app_handle) {
                if let Some(tiddler) = parse_json_tiddler_content(&content) {
                    tiddlers.insert(tiddler.title.clone(), tiddler);
                }
            }
        }
    }

    println!("[WikiServer] Loaded {} tiddlers", tiddlers.len());
    tiddlers
}

/// Read file content using fs plugin if available, else std::fs
fn read_file_content(path: &Path, app_handle: Option<&tauri::AppHandle>) -> Option<String> {
    use std::io::Read;
    if let Some(app) = app_handle {
        use tauri_plugin_fs::{FsExt, OpenOptions};
        let opts = OpenOptions::new();
        let mut file = app.fs().open(path, opts).ok()?;
        let mut content = String::new();
        file.read_to_string(&mut content).ok()?;
        Some(content)
    } else {
        std::fs::read_to_string(path).ok()
    }
}

/// Read file bytes using fs plugin if available, else std::fs
fn read_file_bytes(path: &Path, app_handle: Option<&tauri::AppHandle>) -> Option<Vec<u8>> {
    use std::io::Read;
    if let Some(app) = app_handle {
        use tauri_plugin_fs::{FsExt, OpenOptions};
        let opts = OpenOptions::new();
        let mut file = app.fs().open(path, opts).ok()?;
        let mut content = Vec::new();
        file.read_to_end(&mut content).ok()?;
        Some(content)
    } else {
        std::fs::read(path).ok()
    }
}

/// Write file using fs plugin if available, else std::fs
fn write_file(path: &Path, content: &[u8], app_handle: Option<&tauri::AppHandle>) -> Result<(), String> {
    use std::io::Write;
    if let Some(app) = app_handle {
        use tauri_plugin_fs::{FsExt, OpenOptions};
        let mut opts = OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        let mut file = app.fs().open(path, opts)
            .map_err(|e| format!("Failed to open file for writing: {}", e))?;
        file.write_all(content)
            .map_err(|e| format!("Failed to write file: {}", e))
    } else {
        std::fs::write(path, content)
            .map_err(|e| format!("Failed to write file: {}", e))
    }
}

/// Create directory using fs plugin if available, else std::fs
/// Note: On Android with content:// URIs, directory creation may not be supported
fn create_dir(path: &Path, app_handle: Option<&tauri::AppHandle>) -> Result<(), String> {
    let _ = app_handle; // Directory creation via content:// URIs is handled differently
    // For regular paths, use std::fs
    std::fs::create_dir_all(path)
        .map_err(|e| format!("Failed to create dir: {}", e))
}

/// Remove file using fs plugin if available, else std::fs
/// Note: On Android with content:// URIs, we can't easily delete files
fn remove_file(path: &Path, app_handle: Option<&tauri::AppHandle>) -> Result<(), String> {
    let _ = app_handle; // File deletion via content:// URIs is not supported this way
    // For regular paths, use std::fs
    std::fs::remove_file(path)
        .map_err(|e| format!("Failed to remove file: {}", e))
}

/// Check if file exists using fs plugin if available, else std::fs
fn file_exists(path: &Path, app_handle: Option<&tauri::AppHandle>) -> bool {
    if let Some(app) = app_handle {
        use tauri_plugin_fs::{FsExt, OpenOptions};
        let opts = OpenOptions::new();
        app.fs().open(path, opts).is_ok()
    } else {
        path.exists()
    }
}

fn parse_tid_content(content: &str, filename: &str) -> Option<Tiddler> {
    let mut lines = content.lines();
    let mut fields = HashMap::new();
    let mut title = String::new();

    // Parse header fields
    for line in lines.by_ref() {
        if line.is_empty() {
            break; // Empty line separates header from body
        }

        if let Some((key, value)) = line.split_once(": ") {
            if key == "title" {
                title = value.to_string();
            }
            fields.insert(key.to_string(), value.to_string());
        }
    }

    // Rest is the text content
    let text: String = lines.collect::<Vec<_>>().join("\n");

    if title.is_empty() {
        // Try to derive title from filename
        let stem = filename.trim_end_matches(".tid");
        title = urlencoding::decode(stem).unwrap_or_default().to_string();
    }

    Some(Tiddler { title, fields, text })
}

fn parse_json_tiddler_content(content: &str) -> Option<Tiddler> {
    let json: serde_json::Value = serde_json::from_str(content).ok()?;

    let title = json.get("title")?.as_str()?.to_string();
    let text = json.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();

    let mut fields = HashMap::new();
    if let Some(obj) = json.as_object() {
        for (key, value) in obj {
            if key != "title" && key != "text" {
                if let Some(s) = value.as_str() {
                    fields.insert(key.clone(), s.to_string());
                }
            }
        }
    }

    Some(Tiddler { title, fields, text })
}

fn save_tiddler_to_file(
    wiki_path: &Path,
    tiddler: &Tiddler,
    app_handle: Option<&tauri::AppHandle>,
) -> Result<(), String> {
    let tiddlers_dir = wiki_path.join("tiddlers");
    create_dir(&tiddlers_dir, app_handle)?;

    // Encode title for filename (handle special characters)
    let safe_filename = encode_filename(&tiddler.title);
    let file_path = tiddlers_dir.join(format!("{}.tid", safe_filename));

    // Build .tid content
    let mut content = String::new();

    // Write title first
    content.push_str(&format!("title: {}\n", tiddler.title));

    // Write other fields
    for (key, value) in &tiddler.fields {
        if key != "title" && key != "text" {
            content.push_str(&format!("{}: {}\n", key, value));
        }
    }

    // Empty line separator
    content.push('\n');

    // Write text
    content.push_str(&tiddler.text);

    write_file(&file_path, content.as_bytes(), app_handle)
}

fn delete_tiddler_file(
    wiki_path: &Path,
    title: &str,
    app_handle: Option<&tauri::AppHandle>,
) -> Result<(), String> {
    let tiddlers_dir = wiki_path.join("tiddlers");
    let safe_filename = encode_filename(title);

    // Try both .tid and .json extensions
    for ext in &["tid", "json"] {
        let file_path = tiddlers_dir.join(format!("{}.{}", safe_filename, ext));
        if file_exists(&file_path, app_handle) {
            remove_file(&file_path, app_handle)?;
            return Ok(());
        }
    }

    Ok(()) // Tiddler file not found, that's fine
}

fn encode_filename(title: &str) -> String {
    // Replace characters that are problematic in filenames
    title
        .replace('/', "_")
        .replace('\\', "_")
        .replace(':', "_")
        .replace('*', "_")
        .replace('?', "_")
        .replace('"', "_")
        .replace('<', "_")
        .replace('>', "_")
        .replace('|', "_")
}

// ============================================================================
// API Handlers
// ============================================================================

fn handle_status() -> Response<std::io::Cursor<Vec<u8>>> {
    let status = serde_json::json!({
        "username": "GUEST",
        "anonymous": true,
        "read_only": false,
        "space": {
            "recipe": "default"
        },
        "tiddlywiki_version": "5.3.0"
    });

    Response::from_string(status.to_string())
        .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
        .with_header(Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap())
}

fn handle_get_tiddlers(tiddlers: &Arc<Mutex<HashMap<String, Tiddler>>>) -> Response<std::io::Cursor<Vec<u8>>> {
    let tiddlers = tiddlers.lock().unwrap();

    // Return skinny tiddlers (without text)
    let skinny: Vec<String> = tiddlers.values()
        .map(|t| t.to_skinny_json())
        .collect();

    let json = format!("[{}]", skinny.join(","));

    Response::from_string(json)
        .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
        .with_header(Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap())
}

fn handle_get_tiddler(title: &str, tiddlers: &Arc<Mutex<HashMap<String, Tiddler>>>) -> Response<std::io::Cursor<Vec<u8>>> {
    let tiddlers = tiddlers.lock().unwrap();

    if let Some(tiddler) = tiddlers.get(title) {
        Response::from_string(tiddler.to_json())
            .with_header(Header::from_bytes("Content-Type", "application/json").unwrap())
            .with_header(Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap())
    } else {
        Response::from_string("Tiddler not found")
            .with_status_code(404)
            .with_header(Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap())
    }
}

fn handle_put_tiddler(
    mut request: Request,
    title: &str,
    wiki_path: &Path,
    tiddlers: &Arc<Mutex<HashMap<String, Tiddler>>>,
    app_handle: &Arc<Option<tauri::AppHandle>>,
) {
    // Read request body
    let mut body = String::new();
    if let Err(e) = request.as_reader().read_to_string(&mut body) {
        let response = Response::from_string(format!("Failed to read body: {}", e))
            .with_status_code(400)
            .with_header(Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap());
        let _ = request.respond(response);
        return;
    }

    // Parse JSON
    let json: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            let response = Response::from_string(format!("Invalid JSON: {}", e))
                .with_status_code(400)
                .with_header(Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap());
            let _ = request.respond(response);
            return;
        }
    };

    // Extract fields
    let text = json.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let mut fields = HashMap::new();

    if let Some(obj) = json.as_object() {
        for (key, value) in obj {
            if key != "title" && key != "text" {
                if let Some(s) = value.as_str() {
                    fields.insert(key.clone(), s.to_string());
                }
            }
        }
    }

    let tiddler = Tiddler {
        title: title.to_string(),
        fields,
        text,
    };

    // Save to file (uses fs plugin on Android for content:// URI support)
    if let Err(e) = save_tiddler_to_file(wiki_path, &tiddler, app_handle.as_ref().as_ref()) {
        let response = Response::from_string(format!("Failed to save: {}", e))
            .with_status_code(500)
            .with_header(Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap());
        let _ = request.respond(response);
        return;
    }

    println!("[WikiServer] Saved tiddler '{}'", title);

    // Update in-memory cache
    tiddlers.lock().unwrap().insert(title.to_string(), tiddler);

    let response = Response::from_string("")
        .with_status_code(204)
        .with_header(Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap());
    let _ = request.respond(response);
}

fn handle_delete_tiddler(
    title: &str,
    wiki_path: &Path,
    tiddlers: &Arc<Mutex<HashMap<String, Tiddler>>>,
    app_handle: &Arc<Option<tauri::AppHandle>>,
) -> Response<std::io::Cursor<Vec<u8>>> {
    // Delete from file system (uses fs plugin on Android for content:// URI support)
    if let Err(e) = delete_tiddler_file(wiki_path, title, app_handle.as_ref().as_ref()) {
        return Response::from_string(format!("Failed to delete: {}", e))
            .with_status_code(500)
            .with_header(Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap());
    }

    println!("[WikiServer] Deleted tiddler '{}'", title);

    // Remove from in-memory cache
    tiddlers.lock().unwrap().remove(title);

    Response::from_string("")
        .with_status_code(204)
        .with_header(Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap())
}

fn handle_options() -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string("")
        .with_status_code(200)
        .with_header(Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap())
        .with_header(Header::from_bytes("Access-Control-Allow-Methods", "GET, PUT, DELETE, OPTIONS").unwrap())
        .with_header(Header::from_bytes("Access-Control-Allow-Headers", "Content-Type, X-Requested-With").unwrap())
}

// ============================================================================
// Static File Serving
// ============================================================================

fn serve_index(wiki_path: &Path, app_handle: &Arc<Option<tauri::AppHandle>>) -> Response<std::io::Cursor<Vec<u8>>> {
    // Try to serve tiddlywiki.html or index.html
    for filename in &["tiddlywiki.html", "index.html", "output/index.html"] {
        let file_path = wiki_path.join(filename);
        if let Some(content) = read_file_bytes(&file_path, app_handle.as_ref().as_ref()) {
            return Response::from_data(content)
                .with_header(Header::from_bytes("Content-Type", "text/html; charset=utf-8").unwrap())
                .with_header(Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap());
        }
    }

    // Generate a minimal TiddlyWiki HTML that loads from the server
    let html = generate_tiddlywiki_html();
    Response::from_string(html)
        .with_header(Header::from_bytes("Content-Type", "text/html; charset=utf-8").unwrap())
        .with_header(Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap())
}

fn serve_favicon(wiki_path: &Path, app_handle: &Arc<Option<tauri::AppHandle>>) -> Response<std::io::Cursor<Vec<u8>>> {
    let favicon_path = wiki_path.join("tiddlers").join("favicon.ico");
    if let Some(content) = read_file_bytes(&favicon_path, app_handle.as_ref().as_ref()) {
        return Response::from_data(content)
            .with_header(Header::from_bytes("Content-Type", "image/x-icon").unwrap());
    }

    Response::from_string("")
        .with_status_code(404)
}

fn serve_static_file(wiki_path: &Path, path: &str, app_handle: &Arc<Option<tauri::AppHandle>>) -> Response<std::io::Cursor<Vec<u8>>> {
    let file_path = wiki_path.join(path.trim_start_matches('/'));

    if let Some(content) = read_file_bytes(&file_path, app_handle.as_ref().as_ref()) {
        let content_type = guess_content_type(&file_path);
        return Response::from_data(content)
            .with_header(Header::from_bytes("Content-Type", content_type).unwrap())
            .with_header(Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap());
    }

    Response::from_string("Not Found")
        .with_status_code(404)
}

fn guess_content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("css") => "text/css",
        Some("js") => "application/javascript",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("tid") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn generate_tiddlywiki_html() -> String {
    // Generate a minimal HTML page that bootstraps TiddlyWiki from the server
    // This is the "lazy loading" approach used by tiddlyweb
    r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>TiddlyWiki</title>
    <style>
        body { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif; margin: 40px; }
        .loading { text-align: center; padding: 50px; }
    </style>
</head>
<body>
    <div class="loading">
        <h1>Loading TiddlyWiki...</h1>
        <p>If this message persists, please ensure the wiki is properly initialized.</p>
    </div>
    <script>
        // This page should be replaced by the actual TiddlyWiki HTML
        // For now, redirect to load tiddlers via API
        console.log('TiddlyWiki server is running');
    </script>
</body>
</html>"#.to_string()
}
