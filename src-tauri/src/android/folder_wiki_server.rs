//! Embedded HTTP server for folder wiki support on Android.
//!
//! This module provides a TiddlyWiki-compatible HTTP server that:
//! - Serves the wiki HTML at GET /
//! - Implements the TiddlyWeb sync protocol for tiddler operations
//! - Uses SAF (Storage Access Framework) for all file operations
//!
//! This allows folder wikis to work the same way as on desktop, with
//! live editing and autosave to individual .tid files.

#![cfg(target_os = "android")]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use tiny_http::{Server, Request, Response, Header, Method, StatusCode};
use serde::{Deserialize, Serialize};

use super::saf;

/// A running folder wiki server instance.
pub struct FolderWikiServer {
    /// The port the server is running on
    pub port: u16,
    /// Handle to stop the server
    stop_flag: Arc<Mutex<bool>>,
    /// Thread handle
    _thread: Option<thread::JoinHandle<()>>,
}

/// Tiddler data structure for JSON serialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TiddlerJson {
    pub title: String,
    #[serde(flatten)]
    pub fields: HashMap<String, serde_json::Value>,
}

/// Server status response.
#[derive(Serialize)]
struct StatusResponse {
    username: String,
    anonymous: bool,
    read_only: bool,
    space: SpaceInfo,
    tiddlywiki_version: String,
}

#[derive(Serialize)]
struct SpaceInfo {
    recipe: String,
}

/// Wiki folder context for serving.
struct WikiContext {
    /// Root URI of the wiki folder (SAF content:// URI)
    folder_uri: String,
    /// Cached HTML content of the rendered wiki
    html_content: String,
    /// URI of the tiddlers directory
    tiddlers_uri: Option<String>,
}

impl FolderWikiServer {
    /// Start a new folder wiki server.
    ///
    /// # Arguments
    /// * `folder_uri` - SAF content:// URI of the wiki folder
    /// * `html_content` - Pre-rendered wiki HTML content
    ///
    /// # Returns
    /// The server instance with the port it's running on.
    pub fn start(folder_uri: String, html_content: String) -> Result<Self, String> {
        // Find an available port
        let port = find_available_port()?;

        eprintln!("[FolderWikiServer] Starting on port {}", port);

        // Create the HTTP server
        let server = Server::http(format!("127.0.0.1:{}", port))
            .map_err(|e| format!("Failed to start HTTP server: {}", e))?;

        let stop_flag = Arc::new(Mutex::new(false));
        let stop_flag_clone = stop_flag.clone();

        // Find the tiddlers directory
        let tiddlers_uri = saf::find_subdirectory(&folder_uri, "tiddlers")
            .ok()
            .flatten();

        if tiddlers_uri.is_none() {
            eprintln!("[FolderWikiServer] Warning: No tiddlers directory found in wiki folder");
        }

        let context = Arc::new(Mutex::new(WikiContext {
            folder_uri,
            html_content,
            tiddlers_uri,
        }));

        // Spawn server thread
        let thread = thread::spawn(move || {
            run_server(server, context, stop_flag_clone);
        });

        Ok(Self {
            port,
            stop_flag,
            _thread: Some(thread),
        })
    }

    /// Get the URL to connect to this server.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Stop the server.
    pub fn stop(&self) {
        *self.stop_flag.lock().unwrap() = true;
    }
}

impl Drop for FolderWikiServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Find an available port for the server.
fn find_available_port() -> Result<u16, String> {
    // Try ports in range 39000-39999 (same range as desktop)
    for port in 39000..40000 {
        if std::net::TcpListener::bind(format!("127.0.0.1:{}", port)).is_ok() {
            return Ok(port);
        }
    }
    Err("No available ports found".to_string())
}

/// Main server loop.
fn run_server(server: Server, context: Arc<Mutex<WikiContext>>, stop_flag: Arc<Mutex<bool>>) {
    eprintln!("[FolderWikiServer] Server thread started");

    // Set a timeout so we can check the stop flag periodically
    server.incoming_requests().for_each(|request| {
        if *stop_flag.lock().unwrap() {
            return;
        }

        if let Err(e) = handle_request(request, &context) {
            eprintln!("[FolderWikiServer] Error handling request: {}", e);
        }
    });

    eprintln!("[FolderWikiServer] Server thread stopped");
}

/// Handle a single HTTP request.
fn handle_request(request: Request, context: &Arc<Mutex<WikiContext>>) -> Result<(), String> {
    let method = request.method().clone();
    let url = request.url().to_string();

    eprintln!("[FolderWikiServer] {} {}", method, url);

    match (&method, url.as_str()) {
        // Serve wiki HTML
        (Method::Get, "/") | (Method::Get, "/index.html") => {
            serve_wiki_html(request, context)
        }

        // Server status
        (Method::Get, "/status") => {
            serve_status(request)
        }

        // List all tiddlers
        (Method::Get, "/recipes/default/tiddlers.json") => {
            serve_tiddler_list(request, context)
        }

        // Skinny tiddlers list (titles only, for initial sync)
        (Method::Get, path) if path.starts_with("/recipes/default/tiddlers.json?") => {
            serve_tiddler_list(request, context)
        }

        // Get/Put/Delete individual tiddler
        (Method::Get, path) if path.starts_with("/recipes/default/tiddlers/") => {
            let title = urlencoding::decode(&path[27..])
                .map(|s| s.to_string())
                .unwrap_or_default();
            serve_tiddler(request, context, &title)
        }
        (Method::Put, path) if path.starts_with("/recipes/default/tiddlers/") => {
            let title = urlencoding::decode(&path[27..])
                .map(|s| s.to_string())
                .unwrap_or_default();
            save_tiddler(request, context, &title)
        }
        (Method::Delete, path) if path.starts_with("/recipes/default/tiddlers/") => {
            let title = urlencoding::decode(&path[27..])
                .map(|s| s.to_string())
                .unwrap_or_default();
            delete_tiddler(request, context, &title)
        }

        // Bag endpoint (alternative for saving)
        (Method::Put, path) if path.starts_with("/bags/default/tiddlers/") => {
            let title = urlencoding::decode(&path[23..])
                .map(|s| s.to_string())
                .unwrap_or_default();
            save_tiddler(request, context, &title)
        }

        // Static files from wiki folder
        (Method::Get, path) if !path.starts_with("/recipes/") && !path.starts_with("/bags/") => {
            serve_static_file(request, context, path)
        }

        // Not found
        _ => {
            let response = Response::from_string("Not Found")
                .with_status_code(StatusCode(404));
            request.respond(response)
                .map_err(|e| format!("Failed to send response: {}", e))
        }
    }
}

/// Serve the wiki HTML.
fn serve_wiki_html(request: Request, context: &Arc<Mutex<WikiContext>>) -> Result<(), String> {
    let ctx = context.lock().unwrap();
    let html = ctx.html_content.clone();
    drop(ctx);

    let response = Response::from_string(html)
        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap());

    request.respond(response)
        .map_err(|e| format!("Failed to send response: {}", e))
}

/// Serve server status.
fn serve_status(request: Request) -> Result<(), String> {
    let status = StatusResponse {
        username: "GUEST".to_string(),
        anonymous: true,
        read_only: false,
        space: SpaceInfo {
            recipe: "default".to_string(),
        },
        tiddlywiki_version: "5.3.6".to_string(),
    };

    let json = serde_json::to_string(&status)
        .map_err(|e| format!("Failed to serialize status: {}", e))?;

    let response = Response::from_string(json)
        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap());

    request.respond(response)
        .map_err(|e| format!("Failed to send response: {}", e))
}

/// Serve list of all tiddlers.
fn serve_tiddler_list(request: Request, context: &Arc<Mutex<WikiContext>>) -> Result<(), String> {
    let ctx = context.lock().unwrap();
    let tiddlers_uri = match &ctx.tiddlers_uri {
        Some(uri) => uri.clone(),
        None => {
            drop(ctx);
            let response = Response::from_string("[]")
                .with_header(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap());
            return request.respond(response)
                .map_err(|e| format!("Failed to send response: {}", e));
        }
    };
    drop(ctx);

    // Check if this is a skinny request (titles only)
    let url = request.url();
    let skinny = url.contains("fat=0") || url.contains("skinny=1");

    // List tiddler files
    let entries = saf::list_directory_entries(&tiddlers_uri)
        .unwrap_or_default();

    let mut tiddlers = Vec::new();

    for entry in entries {
        if entry.is_dir {
            continue;
        }

        // Parse .tid files
        if entry.name.ends_with(".tid") {
            if let Ok(content) = saf::read_document_string(&entry.uri) {
                if let Some(tiddler) = parse_tid_file(&content, skinny) {
                    tiddlers.push(tiddler);
                }
            }
        }
        // Parse .json files (tiddler JSON format)
        else if entry.name.ends_with(".json") && !entry.name.starts_with("$") {
            if let Ok(content) = saf::read_document_string(&entry.uri) {
                if let Ok(tiddler) = serde_json::from_str::<TiddlerJson>(&content) {
                    if skinny {
                        // Return only revision info for skinny request
                        let mut skinny_tiddler = HashMap::new();
                        skinny_tiddler.insert("title".to_string(), serde_json::Value::String(tiddler.title));
                        if let Some(rev) = tiddler.fields.get("revision") {
                            skinny_tiddler.insert("revision".to_string(), rev.clone());
                        }
                        tiddlers.push(skinny_tiddler);
                    } else {
                        let mut full = HashMap::new();
                        full.insert("title".to_string(), serde_json::Value::String(tiddler.title));
                        for (k, v) in tiddler.fields {
                            full.insert(k, v);
                        }
                        tiddlers.push(full);
                    }
                }
            }
        }
    }

    let json = serde_json::to_string(&tiddlers)
        .map_err(|e| format!("Failed to serialize tiddlers: {}", e))?;

    let response = Response::from_string(json)
        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap());

    request.respond(response)
        .map_err(|e| format!("Failed to send response: {}", e))
}

/// Parse a .tid file into a tiddler.
fn parse_tid_file(content: &str, skinny: bool) -> Option<HashMap<String, serde_json::Value>> {
    let mut fields = HashMap::new();
    let mut in_body = false;
    let mut body_lines = Vec::new();

    for line in content.lines() {
        if in_body {
            body_lines.push(line);
        } else if line.is_empty() {
            in_body = true;
        } else if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_string();
            let value = value.trim().to_string();
            fields.insert(key, serde_json::Value::String(value));
        }
    }

    // Must have a title
    if !fields.contains_key("title") {
        return None;
    }

    if skinny {
        // Return only title and revision for skinny request
        let mut skinny = HashMap::new();
        if let Some(title) = fields.remove("title") {
            skinny.insert("title".to_string(), title);
        }
        if let Some(rev) = fields.get("revision") {
            skinny.insert("revision".to_string(), rev.clone());
        }
        Some(skinny)
    } else {
        // Include body text
        if !body_lines.is_empty() {
            fields.insert("text".to_string(), serde_json::Value::String(body_lines.join("\n")));
        }
        Some(fields)
    }
}

/// Serve a single tiddler.
fn serve_tiddler(request: Request, context: &Arc<Mutex<WikiContext>>, title: &str) -> Result<(), String> {
    let ctx = context.lock().unwrap();
    let tiddlers_uri = match &ctx.tiddlers_uri {
        Some(uri) => uri.clone(),
        None => {
            drop(ctx);
            let response = Response::from_string("Tiddler not found")
                .with_status_code(StatusCode(404));
            return request.respond(response)
                .map_err(|e| format!("Failed to send response: {}", e));
        }
    };
    drop(ctx);

    // Try to find the tiddler file
    let filename = title_to_filename(title);

    // Try .tid first
    if let Ok(Some(uri)) = saf::find_in_directory(&tiddlers_uri, &format!("{}.tid", filename)) {
        if let Ok(content) = saf::read_document_string(&uri) {
            if let Some(tiddler) = parse_tid_file(&content, false) {
                let json = serde_json::to_string(&tiddler)
                    .map_err(|e| format!("Failed to serialize: {}", e))?;
                let response = Response::from_string(json)
                    .with_header(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap());
                return request.respond(response)
                    .map_err(|e| format!("Failed to send response: {}", e));
            }
        }
    }

    // Try .json
    if let Ok(Some(uri)) = saf::find_in_directory(&tiddlers_uri, &format!("{}.json", filename)) {
        if let Ok(content) = saf::read_document_string(&uri) {
            let response = Response::from_string(content)
                .with_header(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap());
            return request.respond(response)
                .map_err(|e| format!("Failed to send response: {}", e));
        }
    }

    // Not found
    let response = Response::from_string("Tiddler not found")
        .with_status_code(StatusCode(404));
    request.respond(response)
        .map_err(|e| format!("Failed to send response: {}", e))
}

/// Save a tiddler.
fn save_tiddler(mut request: Request, context: &Arc<Mutex<WikiContext>>, title: &str) -> Result<(), String> {
    let ctx = context.lock().unwrap();
    let tiddlers_uri = match &ctx.tiddlers_uri {
        Some(uri) => uri.clone(),
        None => {
            drop(ctx);
            let response = Response::from_string("Tiddlers directory not found")
                .with_status_code(StatusCode(500));
            return request.respond(response)
                .map_err(|e| format!("Failed to send response: {}", e));
        }
    };
    drop(ctx);

    // Read request body
    let mut body = String::new();
    request.as_reader().read_to_string(&mut body)
        .map_err(|e| format!("Failed to read request body: {}", e))?;

    // Parse as JSON tiddler
    let tiddler: TiddlerJson = serde_json::from_str(&body)
        .map_err(|e| format!("Failed to parse tiddler JSON: {}", e))?;

    eprintln!("[FolderWikiServer] Saving tiddler: {}", title);

    // Convert to .tid format
    let tid_content = tiddler_to_tid(&tiddler);

    // Generate filename
    let filename = format!("{}.tid", title_to_filename(title));

    // Check if file exists
    let existing_uri = saf::find_in_directory(&tiddlers_uri, &filename).ok().flatten();

    let file_uri = if let Some(uri) = existing_uri {
        // Update existing file
        uri
    } else {
        // Create new file
        saf::create_file(&tiddlers_uri, &filename, Some("text/plain"))
            .map_err(|e| format!("Failed to create tiddler file: {}", e))?
    };

    // Write content
    saf::write_document_string(&file_uri, &tid_content)
        .map_err(|e| format!("Failed to write tiddler: {}", e))?;

    // Return the saved tiddler with ETag
    let etag = format!("\"default/{}/{}:\"",
        urlencoding::encode("default"),
        urlencoding::encode(title));

    let response = Response::from_string(&body)
        .with_status_code(StatusCode(204))
        .with_header(Header::from_bytes(&b"ETag"[..], etag.as_bytes()).unwrap());

    request.respond(response)
        .map_err(|e| format!("Failed to send response: {}", e))
}

/// Convert a TiddlerJson to .tid file format.
fn tiddler_to_tid(tiddler: &TiddlerJson) -> String {
    let mut lines = Vec::new();

    // Add title field first
    lines.push(format!("title: {}", tiddler.title));

    // Add other fields (except text)
    let mut text_content = None;
    for (key, value) in &tiddler.fields {
        if key == "text" {
            text_content = value.as_str().map(|s| s.to_string());
        } else if key != "title" {
            let value_str = match value {
                serde_json::Value::String(s) => s.clone(),
                _ => value.to_string(),
            };
            lines.push(format!("{}: {}", key, value_str));
        }
    }

    // Empty line separates headers from body
    lines.push(String::new());

    // Add text content
    if let Some(text) = text_content {
        lines.push(text);
    }

    lines.join("\n")
}

/// Delete a tiddler.
fn delete_tiddler(request: Request, context: &Arc<Mutex<WikiContext>>, title: &str) -> Result<(), String> {
    let ctx = context.lock().unwrap();
    let tiddlers_uri = match &ctx.tiddlers_uri {
        Some(uri) => uri.clone(),
        None => {
            drop(ctx);
            let response = Response::from_string("Tiddlers directory not found")
                .with_status_code(StatusCode(500));
            return request.respond(response)
                .map_err(|e| format!("Failed to send response: {}", e));
        }
    };
    drop(ctx);

    eprintln!("[FolderWikiServer] Deleting tiddler: {}", title);

    let filename = title_to_filename(title);

    // Try to delete .tid file
    if let Ok(Some(uri)) = saf::find_in_directory(&tiddlers_uri, &format!("{}.tid", filename)) {
        saf::delete_document(&uri)?;
    }
    // Also try .json
    else if let Ok(Some(uri)) = saf::find_in_directory(&tiddlers_uri, &format!("{}.json", filename)) {
        saf::delete_document(&uri)?;
    }

    let response = Response::from_string("")
        .with_status_code(StatusCode(204));

    request.respond(response)
        .map_err(|e| format!("Failed to send response: {}", e))
}

/// Serve a static file from the wiki folder.
fn serve_static_file(request: Request, context: &Arc<Mutex<WikiContext>>, path: &str) -> Result<(), String> {
    let ctx = context.lock().unwrap();
    let folder_uri = ctx.folder_uri.clone();
    drop(ctx);

    // Remove leading slash
    let path = path.trim_start_matches('/');

    // Navigate through path components
    let mut current_uri = folder_uri;
    let components: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    for (i, component) in components.iter().enumerate() {
        let decoded = urlencoding::decode(component)
            .map(|s| s.to_string())
            .unwrap_or_else(|_| component.to_string());

        if i == components.len() - 1 {
            // Last component - should be a file
            if let Ok(Some(uri)) = saf::find_in_directory(&current_uri, &decoded) {
                if let Ok(content) = saf::read_document_bytes(&uri) {
                    let mime_type = guess_mime_type(&decoded);
                    let response = Response::from_data(content)
                        .with_header(Header::from_bytes(&b"Content-Type"[..], mime_type.as_bytes()).unwrap());
                    return request.respond(response)
                        .map_err(|e| format!("Failed to send response: {}", e));
                }
            }
        } else {
            // Directory component
            if let Ok(Some(uri)) = saf::find_subdirectory(&current_uri, &decoded) {
                current_uri = uri;
            } else {
                break;
            }
        }
    }

    // Not found
    let response = Response::from_string("Not Found")
        .with_status_code(StatusCode(404));
    request.respond(response)
        .map_err(|e| format!("Failed to send response: {}", e))
}

/// Convert a tiddler title to a safe filename.
fn title_to_filename(title: &str) -> String {
    // Replace characters that are problematic in filenames
    let mut filename = String::with_capacity(title.len());

    for c in title.chars() {
        match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => {
                filename.push('_');
            }
            '$' => {
                // System tiddlers start with $ - encode as _
                if filename.is_empty() {
                    filename.push_str("_");
                } else {
                    filename.push(c);
                }
            }
            _ => filename.push(c),
        }
    }

    // Handle some edge cases
    if filename.is_empty() {
        filename = "_unnamed".to_string();
    }

    filename
}

/// Guess MIME type from filename.
fn guess_mime_type(filename: &str) -> String {
    let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        // Text/markup
        "html" | "htm" => "text/html".to_string(),
        "css" => "text/css".to_string(),
        "js" => "application/javascript".to_string(),
        "json" => "application/json".to_string(),
        "txt" => "text/plain".to_string(),
        "xml" => "application/xml".to_string(),
        "md" => "text/markdown".to_string(),
        "tid" => "text/vnd.tiddlywiki".to_string(),
        // Images
        "png" => "image/png".to_string(),
        "jpg" | "jpeg" => "image/jpeg".to_string(),
        "gif" => "image/gif".to_string(),
        "svg" => "image/svg+xml".to_string(),
        "webp" => "image/webp".to_string(),
        "ico" => "image/x-icon".to_string(),
        "bmp" => "image/bmp".to_string(),
        "tiff" | "tif" => "image/tiff".to_string(),
        "heic" | "heif" => "image/heic".to_string(),
        // Audio
        "mp3" => "audio/mpeg".to_string(),
        "m4a" => "audio/mp4".to_string(),
        "aac" => "audio/aac".to_string(),
        "ogg" | "oga" => "audio/ogg".to_string(),
        "opus" => "audio/opus".to_string(),
        "wav" => "audio/wav".to_string(),
        "flac" => "audio/flac".to_string(),
        "aiff" | "aif" => "audio/aiff".to_string(),
        "wma" => "audio/x-ms-wma".to_string(),
        "mid" | "midi" => "audio/midi".to_string(),
        // Video
        "mp4" | "m4v" => "video/mp4".to_string(),
        "webm" => "video/webm".to_string(),
        "ogv" => "video/ogg".to_string(),
        "avi" => "video/x-msvideo".to_string(),
        "mov" => "video/quicktime".to_string(),
        "wmv" => "video/x-ms-wmv".to_string(),
        "mkv" => "video/x-matroska".to_string(),
        "3gp" => "video/3gpp".to_string(),
        // Fonts
        "woff" => "font/woff".to_string(),
        "woff2" => "font/woff2".to_string(),
        "ttf" => "font/ttf".to_string(),
        "otf" => "font/otf".to_string(),
        "eot" => "application/vnd.ms-fontobject".to_string(),
        // Documents
        "pdf" => "application/pdf".to_string(),
        "doc" => "application/msword".to_string(),
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document".to_string(),
        "xls" => "application/vnd.ms-excel".to_string(),
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet".to_string(),
        "ppt" => "application/vnd.ms-powerpoint".to_string(),
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation".to_string(),
        // Archives
        "zip" => "application/zip".to_string(),
        "tar" => "application/x-tar".to_string(),
        "gz" | "gzip" => "application/gzip".to_string(),
        "rar" => "application/vnd.rar".to_string(),
        "7z" => "application/x-7z-compressed".to_string(),
        // Default
        _ => "application/octet-stream".to_string(),
    }
}

/// Global registry of running servers (keyed by folder URI).
static SERVERS: std::sync::OnceLock<Mutex<HashMap<String, Arc<FolderWikiServer>>>> = std::sync::OnceLock::new();

fn get_servers() -> &'static Mutex<HashMap<String, Arc<FolderWikiServer>>> {
    SERVERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Start a server for a folder wiki and return its URL.
/// If a server is already running for this folder, returns its URL.
pub fn start_server(folder_uri: &str, html_content: String) -> Result<String, String> {
    let mut servers = get_servers().lock().unwrap();

    // Check if server already exists
    if let Some(server) = servers.get(folder_uri) {
        return Ok(server.url());
    }

    // Start new server
    let server = FolderWikiServer::start(folder_uri.to_string(), html_content)?;
    let url = server.url();
    servers.insert(folder_uri.to_string(), Arc::new(server));

    Ok(url)
}

/// Stop a server for a folder wiki.
pub fn stop_server(folder_uri: &str) {
    let mut servers = get_servers().lock().unwrap();
    if let Some(server) = servers.remove(folder_uri) {
        server.stop();
    }
}

/// Stop all running servers.
pub fn stop_all_servers() {
    let mut servers = get_servers().lock().unwrap();
    for (_, server) in servers.drain() {
        server.stop();
    }
}

// =============================================================================
// Single-file Wiki Server (for WikiActivity)
// =============================================================================

/// A simple HTTP server for serving a single-file wiki.
/// Used by WikiActivity to load single-file wikis via HTTP.
/// Also serves external attachments from any user-accessible path.
pub struct SingleFileWikiServer {
    pub port: u16,
    stop_flag: Arc<Mutex<bool>>,
    _thread: Option<thread::JoinHandle<()>>,
}

/// Context for single-file wiki server.
struct SingleFileContext {
    /// The wiki HTML content
    wiki_content: Arc<Mutex<String>>,
    /// Base directory of the wiki file (for relative paths)
    wiki_dir: Option<String>,
    /// The wiki file path (for saving)
    wiki_path: String,
}

impl SingleFileWikiServer {
    /// Start a new single-file wiki server.
    ///
    /// # Arguments
    /// * `wiki_path` - The wiki file path (for saving)
    /// * `wiki_content` - The wiki HTML content
    /// * `wiki_dir` - Optional base directory for resolving relative paths
    pub fn start(wiki_path: String, wiki_content: String, wiki_dir: Option<String>) -> Result<Self, String> {
        let port = find_available_port()?;

        eprintln!("[SingleFileWikiServer] Starting on port {}", port);
        eprintln!("[SingleFileWikiServer] Wiki path: {}", wiki_path);
        if let Some(ref dir) = wiki_dir {
            eprintln!("[SingleFileWikiServer] Wiki directory: {}", dir);
        }

        let server = Server::http(format!("127.0.0.1:{}", port))
            .map_err(|e| format!("Failed to start HTTP server: {}", e))?;

        let stop_flag = Arc::new(Mutex::new(false));
        let stop_flag_clone = stop_flag.clone();

        let context = Arc::new(SingleFileContext {
            wiki_content: Arc::new(Mutex::new(wiki_content)),
            wiki_dir,
            wiki_path,
        });

        let thread = thread::spawn(move || {
            run_single_file_server(server, context, stop_flag_clone);
        });

        Ok(Self {
            port,
            stop_flag,
            _thread: Some(thread),
        })
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn stop(&self) {
        *self.stop_flag.lock().unwrap() = true;
    }
}

impl Drop for SingleFileWikiServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Run a server that serves the wiki HTML and external attachments.
fn run_single_file_server(server: Server, context: Arc<SingleFileContext>, stop_flag: Arc<Mutex<bool>>) {
    eprintln!("[SingleFileWikiServer] Server thread started");

    server.incoming_requests().for_each(|mut request| {
        if *stop_flag.lock().unwrap() {
            return;
        }

        let url = request.url().to_string();
        let method = request.method().clone();

        // Handle different endpoints
        let response = if (url == "/" || url == "/index.html") && method == Method::Get {
            // Serve wiki HTML
            let content = context.wiki_content.lock().unwrap().clone();
            Response::from_string(&content)
                .with_header(Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap())
        } else if (url == "/" || url == "/save") && method == Method::Put {
            // Save wiki content
            match handle_save_request(&mut request, &context) {
                Ok(()) => {
                    eprintln!("[SingleFileWikiServer] Wiki saved successfully");
                    Response::from_string("OK")
                        .with_status_code(StatusCode(200))
                }
                Err(e) => {
                    eprintln!("[SingleFileWikiServer] Save failed: {}", e);
                    Response::from_string(&e)
                        .with_status_code(StatusCode(500))
                }
            }
        } else if url.starts_with("/_file/") {
            // Serve external file by encoded path
            // URL format: /_file/<base64url-encoded-path>
            let encoded_path = &url[7..]; // Skip "/_file/"
            match serve_external_file(encoded_path) {
                Ok(resp) => resp,
                Err(e) => {
                    eprintln!("[SingleFileWikiServer] Error serving file: {}", e);
                    Response::from_string("File not found")
                        .with_status_code(StatusCode(404))
                }
            }
        } else if url.starts_with("/_relative/") {
            // Serve file relative to wiki directory
            // URL format: /_relative/<url-encoded-relative-path>
            let encoded_path = &url[11..]; // Skip "/_relative/"
            match serve_relative_file(encoded_path, &context.wiki_dir) {
                Ok(resp) => resp,
                Err(e) => {
                    eprintln!("[SingleFileWikiServer] Error serving relative file: {}", e);
                    Response::from_string("File not found")
                        .with_status_code(StatusCode(404))
                }
            }
        } else {
            // Unknown endpoint
            Response::from_string("Not Found")
                .with_status_code(StatusCode(404))
        };

        let _ = request.respond(response);
    });

    eprintln!("[SingleFileWikiServer] Server thread stopped");
}

/// Handle a PUT request to save the wiki content.
fn handle_save_request(request: &mut Request, context: &Arc<SingleFileContext>) -> Result<(), String> {
    // Read the body
    let mut body = String::new();
    request.as_reader().read_to_string(&mut body)
        .map_err(|e| format!("Failed to read request body: {}", e))?;

    eprintln!("[SingleFileWikiServer] Saving {} bytes to: {}", body.len(), context.wiki_path);

    // Save to the SAF location
    saf::write_document_string(&context.wiki_path, &body)?;

    // Update the cached content
    *context.wiki_content.lock().unwrap() = body;

    Ok(())
}

/// Serve an external file by its base64url-encoded path.
/// The path can be either a filesystem path or a SAF content:// URI.
fn serve_external_file(encoded_path: &str) -> Result<Response<std::io::Cursor<Vec<u8>>>, String> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};

    // Decode the path
    let path_bytes = URL_SAFE_NO_PAD.decode(encoded_path)
        .map_err(|e| format!("Invalid base64 encoding: {}", e))?;
    let path = String::from_utf8(path_bytes)
        .map_err(|e| format!("Invalid UTF-8 in path: {}", e))?;

    eprintln!("[SingleFileWikiServer] Serving external file: {}", path);

    // Read the file content
    let content = if path.starts_with("content://") || path.starts_with("{") {
        // SAF URI - use SAF module
        saf::read_document_bytes(&path)?
    } else {
        // Filesystem path
        std::fs::read(&path)
            .map_err(|e| format!("Failed to read file: {}", e))?
    };

    // Guess MIME type from path
    let filename = path.rsplit('/').next().unwrap_or(&path);
    let mime_type = guess_mime_type(filename);

    Ok(Response::from_data(content)
        .with_header(Header::from_bytes(&b"Content-Type"[..], mime_type.as_bytes()).unwrap()))
}

/// Serve a file relative to the wiki directory.
fn serve_relative_file(encoded_path: &str, wiki_dir: &Option<String>) -> Result<Response<std::io::Cursor<Vec<u8>>>, String> {
    let wiki_dir = wiki_dir.as_ref()
        .ok_or_else(|| "Wiki directory not set".to_string())?;

    // URL-decode the relative path
    let relative_path = urlencoding::decode(encoded_path)
        .map_err(|e| format!("Invalid URL encoding: {}", e))?
        .to_string();

    eprintln!("[SingleFileWikiServer] Serving relative file: {} from {}", relative_path, wiki_dir);

    // Read the file content
    let content = if wiki_dir.starts_with("content://") || wiki_dir.starts_with("{") {
        // SAF URI - navigate to the file
        let file_uri = saf::find_in_directory(wiki_dir, &relative_path)?
            .ok_or_else(|| format!("File not found: {}", relative_path))?;
        saf::read_document_bytes(&file_uri)?
    } else {
        // Filesystem path
        let full_path = std::path::Path::new(wiki_dir).join(&relative_path);
        std::fs::read(&full_path)
            .map_err(|e| format!("Failed to read file: {}", e))?
    };

    // Guess MIME type
    let mime_type = guess_mime_type(&relative_path);

    Ok(Response::from_data(content)
        .with_header(Header::from_bytes(&b"Content-Type"[..], mime_type.as_bytes()).unwrap()))
}

/// Registry for single-file wiki servers (keyed by wiki path).
static SINGLE_FILE_SERVERS: std::sync::OnceLock<Mutex<HashMap<String, Arc<SingleFileWikiServer>>>> = std::sync::OnceLock::new();

fn get_single_file_servers() -> &'static Mutex<HashMap<String, Arc<SingleFileWikiServer>>> {
    SINGLE_FILE_SERVERS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Start a server for a single-file wiki and return its URL.
///
/// # Arguments
/// * `wiki_path` - Path to the wiki file (used as key for tracking)
/// * `wiki_dir` - Directory containing the wiki (for relative path resolution)
/// * `wiki_content` - The wiki HTML content
pub fn start_single_file_server(wiki_path: &str, wiki_dir: Option<String>, wiki_content: String) -> Result<String, String> {
    let mut servers = get_single_file_servers().lock().unwrap();

    // Check if server already exists
    if let Some(server) = servers.get(wiki_path) {
        return Ok(server.url());
    }

    // Start new server
    let server = SingleFileWikiServer::start(wiki_path.to_string(), wiki_content, wiki_dir)?;
    let url = server.url();
    servers.insert(wiki_path.to_string(), Arc::new(server));

    Ok(url)
}

/// Stop a single-file wiki server.
pub fn stop_single_file_server(wiki_path: &str) {
    let mut servers = get_single_file_servers().lock().unwrap();
    if let Some(server) = servers.remove(wiki_path) {
        server.stop();
    }
}
