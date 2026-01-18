//! Rust-based TiddlyWeb-compatible HTTP server for wiki folders
//!
//! This provides wiki folder functionality on Android where Node.js isn't available.
//! Implements the TiddlyWeb API that TiddlyWiki's tiddlyweb plugin expects.

use std::collections::HashMap;
#[allow(unused_imports)] // Used by request.as_reader().read_to_string()
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use tiny_http::{Server, Request, Response, Header, Method};

/// A running wiki folder server
pub struct WikiFolderHttpServer {
    server_handle: Option<thread::JoinHandle<()>>,
    shutdown_flag: Arc<Mutex<bool>>,
    port: u16,
    wiki_path: PathBuf,
}

impl WikiFolderHttpServer {
    /// Start a new wiki folder server
    pub fn start(wiki_path: PathBuf, port: u16) -> Result<Self, String> {
        let server = Server::http(format!("127.0.0.1:{}", port))
            .map_err(|e| format!("Failed to start HTTP server: {}", e))?;

        let shutdown_flag = Arc::new(Mutex::new(false));
        let shutdown_clone = shutdown_flag.clone();
        let path_clone = wiki_path.clone();

        let handle = thread::spawn(move || {
            run_server(server, path_clone, shutdown_clone);
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

fn run_server(server: Server, wiki_path: PathBuf, shutdown_flag: Arc<Mutex<bool>>) {
    // Load tiddlers into memory for faster access
    let tiddlers = Arc::new(Mutex::new(load_tiddlers(&wiki_path)));

    loop {
        // Check shutdown flag
        if *shutdown_flag.lock().unwrap() {
            break;
        }

        // Wait for request with timeout
        match server.recv_timeout(std::time::Duration::from_millis(500)) {
            Ok(Some(request)) => {
                handle_request(request, &wiki_path, &tiddlers);
            }
            Ok(None) => continue, // Timeout, check shutdown flag
            Err(e) => {
                eprintln!("Server error: {}", e);
                break;
            }
        }
    }
}

fn handle_request(request: Request, wiki_path: &Path, tiddlers: &Arc<Mutex<HashMap<String, Tiddler>>>) {
    let url = request.url().to_string();
    let method = request.method().clone();

    println!("[WikiServer] {} {}", method, url);

    // PUT requests need special handling because they consume the request body
    if method == Method::Put && url.starts_with("/recipes/default/tiddlers/") {
        let title = urlencoding::decode(&url[26..]).unwrap_or_default().to_string();
        handle_put_tiddler(request, &title, wiki_path, tiddlers);
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
            handle_delete_tiddler(&title, wiki_path, tiddlers)
        }
        // Static file serving
        (Method::Get, "/") => serve_index(wiki_path),
        (Method::Get, "/favicon.ico") => serve_favicon(wiki_path),
        (Method::Get, path) => serve_static_file(wiki_path, path),
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

fn load_tiddlers(wiki_path: &Path) -> HashMap<String, Tiddler> {
    let mut tiddlers = HashMap::new();
    let tiddlers_dir = wiki_path.join("tiddlers");

    if !tiddlers_dir.exists() {
        return tiddlers;
    }

    if let Ok(entries) = std::fs::read_dir(&tiddlers_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "tid").unwrap_or(false) {
                if let Some(tiddler) = parse_tid_file(&path) {
                    tiddlers.insert(tiddler.title.clone(), tiddler);
                }
            } else if path.extension().map(|e| e == "json").unwrap_or(false) {
                // Handle .json tiddler files
                if let Some(tiddler) = parse_json_tiddler(&path) {
                    tiddlers.insert(tiddler.title.clone(), tiddler);
                }
            }
        }
    }

    println!("[WikiServer] Loaded {} tiddlers", tiddlers.len());
    tiddlers
}

fn parse_tid_file(path: &Path) -> Option<Tiddler> {
    let content = std::fs::read_to_string(path).ok()?;
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
        title = path.file_stem()?.to_string_lossy().to_string();
        // Handle URL-encoded filenames
        title = urlencoding::decode(&title).unwrap_or_default().to_string();
    }

    Some(Tiddler { title, fields, text })
}

fn parse_json_tiddler(path: &Path) -> Option<Tiddler> {
    let content = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;

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

fn save_tiddler_to_file(wiki_path: &Path, tiddler: &Tiddler) -> Result<(), String> {
    let tiddlers_dir = wiki_path.join("tiddlers");
    std::fs::create_dir_all(&tiddlers_dir)
        .map_err(|e| format!("Failed to create tiddlers dir: {}", e))?;

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

    std::fs::write(&file_path, content)
        .map_err(|e| format!("Failed to write tiddler: {}", e))?;

    Ok(())
}

fn delete_tiddler_file(wiki_path: &Path, title: &str) -> Result<(), String> {
    let tiddlers_dir = wiki_path.join("tiddlers");
    let safe_filename = encode_filename(title);

    // Try both .tid and .json extensions
    for ext in &["tid", "json"] {
        let file_path = tiddlers_dir.join(format!("{}.{}", safe_filename, ext));
        if file_path.exists() {
            std::fs::remove_file(&file_path)
                .map_err(|e| format!("Failed to delete tiddler: {}", e))?;
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
    tiddlers: &Arc<Mutex<HashMap<String, Tiddler>>>
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

    // Save to file
    if let Err(e) = save_tiddler_to_file(wiki_path, &tiddler) {
        let response = Response::from_string(format!("Failed to save: {}", e))
            .with_status_code(500)
            .with_header(Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap());
        let _ = request.respond(response);
        return;
    }

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
    tiddlers: &Arc<Mutex<HashMap<String, Tiddler>>>
) -> Response<std::io::Cursor<Vec<u8>>> {
    // Delete from file system
    if let Err(e) = delete_tiddler_file(wiki_path, title) {
        return Response::from_string(format!("Failed to delete: {}", e))
            .with_status_code(500)
            .with_header(Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap());
    }

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

fn serve_index(wiki_path: &Path) -> Response<std::io::Cursor<Vec<u8>>> {
    // Try to serve tiddlywiki.html or index.html
    for filename in &["tiddlywiki.html", "index.html", "output/index.html"] {
        let file_path = wiki_path.join(filename);
        if file_path.exists() {
            if let Ok(content) = std::fs::read(&file_path) {
                return Response::from_data(content)
                    .with_header(Header::from_bytes("Content-Type", "text/html; charset=utf-8").unwrap())
                    .with_header(Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap());
            }
        }
    }

    // Generate a minimal TiddlyWiki HTML that loads from the server
    let html = generate_tiddlywiki_html();
    Response::from_string(html)
        .with_header(Header::from_bytes("Content-Type", "text/html; charset=utf-8").unwrap())
        .with_header(Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap())
}

fn serve_favicon(wiki_path: &Path) -> Response<std::io::Cursor<Vec<u8>>> {
    let favicon_path = wiki_path.join("tiddlers").join("favicon.ico");
    if favicon_path.exists() {
        if let Ok(content) = std::fs::read(&favicon_path) {
            return Response::from_data(content)
                .with_header(Header::from_bytes("Content-Type", "image/x-icon").unwrap());
        }
    }

    Response::from_string("")
        .with_status_code(404)
}

fn serve_static_file(wiki_path: &Path, path: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let file_path = wiki_path.join(path.trim_start_matches('/'));

    if file_path.exists() && file_path.is_file() {
        if let Ok(content) = std::fs::read(&file_path) {
            let content_type = guess_content_type(&file_path);
            return Response::from_data(content)
                .with_header(Header::from_bytes("Content-Type", content_type).unwrap())
                .with_header(Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap());
        }
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
