//! Linux-only localhost HTTP media server for GStreamer playback.
//!
//! GStreamer (used by WebKitGTK for video/audio) cannot play media from custom
//! URI schemes like tdasset://. It only supports http:// (via souphttpsrc) and
//! file:// (via filesrc). Since file:// is blocked from wikifile:// page origins,
//! we serve media files via a localhost HTTP server.
//!
//! Security model:
//! - Bound to 127.0.0.1 only (no external access)
//! - Per-file token allowlist: only files explicitly registered by the wiki can be served
//! - Path validation: same sanitize checks as tdasset:// protocol
//! - Opaque tokens: URLs contain no filesystem path information
//!
//! HTTP features:
//! - HTTP/1.1 keep-alive (connection reuse for smooth seeking)
//! - Range requests (206 Partial Content for seeking)
//! - ETag caching (304 Not Modified, If-Range)
//! - CORS (Access-Control-Allow-Origin for cross-scheme loading)

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use crate::utils;

/// Per-file token entry.
struct MediaEntry {
    path: PathBuf,
    mime_type: String,
}

/// Localhost HTTP server that serves only token-registered media files.
pub struct MediaServer {
    port: u16,
    tokens: Arc<Mutex<HashMap<String, MediaEntry>>>,
}

impl MediaServer {
    /// Start the media server on a random localhost port.
    pub fn start() -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        let tokens: Arc<Mutex<HashMap<String, MediaEntry>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let tokens_clone = tokens.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let tokens = tokens_clone.clone();
                std::thread::spawn(move || {
                    serve_connection(stream, &tokens);
                });
            }
        });

        eprintln!("[MediaServer] Started on 127.0.0.1:{}", port);
        Ok(Self { port, tokens })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Register a file and return its opaque token.
    /// The caller is responsible for path validation before calling this.
    pub fn register(&self, path: PathBuf) -> String {
        let mime_type = utils::get_mime_type(&path).to_string();
        let token = generate_token();
        self.tokens.lock().unwrap().insert(
            token.clone(),
            MediaEntry { path, mime_type },
        );
        token
    }
}

/// Generate a random 32-character hex token from /dev/urandom.
fn generate_token() -> String {
    let mut bytes = [0u8; 16];
    if let Ok(mut f) = File::open("/dev/urandom") {
        let _ = Read::read_exact(&mut f, &mut bytes);
    } else {
        let t = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        let tid = std::thread::current().id();
        let hash = format!("{:?}{:?}", t, tid);
        for (i, b) in hash.bytes().enumerate() {
            bytes[i % 16] ^= b;
        }
    }
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Compute a stable ETag from file size + modification time.
fn compute_etag(metadata: &fs::Metadata) -> String {
    let size = metadata.len();
    let mtime = metadata
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("\"{:x}-{:x}\"", size, mtime)
}

// ──────────────────────────────────────────────────────────────────────────────
// Connection handling
// ──────────────────────────────────────────────────────────────────────────────

/// Serve a keep-alive connection: handle multiple sequential HTTP requests.
fn serve_connection(stream: TcpStream, tokens: &Mutex<HashMap<String, MediaEntry>>) {
    // TCP_NODELAY: disable Nagle's algorithm so headers are sent immediately.
    // Critical for low-latency range responses during video seeking.
    let _ = stream.set_nodelay(true);
    let _ = stream.set_write_timeout(Some(Duration::from_secs(300)));

    let read_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let mut reader = BufReader::with_capacity(8192, read_stream);
    let mut writer = stream;

    loop {
        // Idle timeout: close connection if no request arrives within 60 seconds
        if reader
            .get_ref()
            .set_read_timeout(Some(Duration::from_secs(60)))
            .is_err()
        {
            break;
        }

        match serve_one_request(&mut reader, &mut writer, tokens) {
            Ok(true) => continue,  // keep-alive — wait for next request
            Ok(false) => break,    // client requested close or HTTP/1.0
            Err(_) => break,       // ECONNRESET, timeout, broken pipe — all expected
        }
    }
}

/// Parsed HTTP request.
struct Request {
    method: String,
    path: String,
    http_version: String,
    range: Option<String>,
    if_none_match: Option<String>,
    if_range: Option<String>,
    keep_alive: bool,
}

/// Parse one HTTP request from the connection.
fn read_request(reader: &mut BufReader<TcpStream>) -> io::Result<Option<Request>> {
    let mut request_line = String::new();
    let n = reader.read_line(&mut request_line)?;
    if n == 0 {
        return Ok(None); // Client closed connection cleanly
    }

    let parts: Vec<&str> = request_line.trim().split_whitespace().collect();
    if parts.len() < 2 {
        return Ok(None);
    }

    let method = parts[0].to_string();
    let path = parts[1].to_string();
    let http_version = if parts.len() >= 3 {
        parts[2].to_string()
    } else {
        "HTTP/1.0".to_string()
    };

    // HTTP/1.1 defaults to keep-alive; HTTP/1.0 defaults to close
    let default_keep_alive = http_version.contains("1.1");
    let mut keep_alive = default_keep_alive;
    let mut range = None;
    let mut if_none_match = None;
    let mut if_range = None;

    // Read headers
    loop {
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        // Case-insensitive header matching
        if let Some(value) = header_value(trimmed, "Range") {
            range = Some(value);
        } else if let Some(value) = header_value(trimmed, "If-None-Match") {
            if_none_match = Some(value);
        } else if let Some(value) = header_value(trimmed, "If-Range") {
            if_range = Some(value);
        } else if let Some(value) = header_value(trimmed, "Connection") {
            let lower = value.to_lowercase();
            if lower.contains("close") {
                keep_alive = false;
            } else if lower.contains("keep-alive") {
                keep_alive = true;
            }
        }
    }

    Ok(Some(Request {
        method,
        path,
        http_version,
        range,
        if_none_match,
        if_range,
        keep_alive,
    }))
}

/// Extract header value (case-insensitive name match).
fn header_value(line: &str, name: &str) -> Option<String> {
    if line.len() > name.len() + 1
        && line[..name.len()].eq_ignore_ascii_case(name)
        && line.as_bytes()[name.len()] == b':'
    {
        Some(line[name.len() + 1..].trim().to_string())
    } else {
        None
    }
}

/// Handle one HTTP request. Returns Ok(true) for keep-alive, Ok(false) to close.
fn serve_one_request(
    reader: &mut BufReader<TcpStream>,
    writer: &mut TcpStream,
    tokens: &Mutex<HashMap<String, MediaEntry>>,
) -> io::Result<bool> {
    let req = match read_request(reader)? {
        Some(r) => r,
        None => return Ok(false),
    };

    let keep_alive = req.keep_alive;
    let conn_value = if keep_alive { "keep-alive" } else { "close" };

    // CORS preflight
    if req.method == "OPTIONS" {
        let resp = format!(
            "{} 204 No Content\r\n\
             Access-Control-Allow-Origin: *\r\n\
             Access-Control-Allow-Methods: GET, HEAD, OPTIONS\r\n\
             Access-Control-Allow-Headers: Range, If-None-Match, If-Range\r\n\
             Access-Control-Max-Age: 86400\r\n\
             Content-Length: 0\r\n\
             Connection: {}\r\n\
             \r\n",
            req.http_version, conn_value
        );
        writer.write_all(resp.as_bytes())?;
        return Ok(keep_alive);
    }

    // Only GET and HEAD
    if req.method != "GET" && req.method != "HEAD" {
        send_error(writer, &req.http_version, 405, "Method Not Allowed", conn_value)?;
        return Ok(false);
    }

    // Parse /media/{token}
    let token = match req.path.strip_prefix("/media/") {
        Some(rest) => rest.split('?').next().unwrap_or(rest),
        None => {
            send_error(writer, &req.http_version, 404, "Not Found", conn_value)?;
            return Ok(keep_alive);
        }
    };

    // Look up token
    let (file_path, mime_type) = {
        let map = tokens.lock().unwrap();
        match map.get(token) {
            Some(entry) => (entry.path.clone(), entry.mime_type.clone()),
            None => {
                send_error(writer, &req.http_version, 404, "Not Found", conn_value)?;
                return Ok(keep_alive);
            }
        }
    };

    // Open file and get metadata
    let mut file = match File::open(&file_path) {
        Ok(f) => f,
        Err(_) => {
            send_error(writer, &req.http_version, 404, "Not Found", conn_value)?;
            return Ok(keep_alive);
        }
    };

    let metadata = match file.metadata() {
        Ok(m) if m.len() > 0 => m,
        _ => {
            send_error(writer, &req.http_version, 404, "Not Found", conn_value)?;
            return Ok(keep_alive);
        }
    };

    let file_size = metadata.len();
    let etag = compute_etag(&metadata);
    let is_head = req.method == "HEAD";

    // If-None-Match: return 304 if ETag matches (browser already has this version)
    if let Some(ref client_etag) = req.if_none_match {
        if etag_matches(client_etag, &etag) {
            let resp = format!(
                "{} 304 Not Modified\r\n\
                 ETag: {}\r\n\
                 Access-Control-Allow-Origin: *\r\n\
                 Connection: {}\r\n\
                 \r\n",
                req.http_version, etag, conn_value
            );
            writer.write_all(resp.as_bytes())?;
            return Ok(keep_alive);
        }
    }

    // Handle range request
    if let Some(ref range_str) = req.range {
        // If-Range: only serve range if ETag matches; otherwise serve full file
        let range_valid = match req.if_range {
            Some(ref ir_etag) => etag_matches(ir_etag, &etag),
            None => true,
        };

        if range_valid {
            if let Some((start, end)) = parse_range(range_str, file_size) {
                return serve_range(
                    writer, &mut file, &req.http_version, &mime_type, &etag,
                    file_size, start, end, is_head, conn_value,
                )
                .map(|_| keep_alive);
            }
            // Invalid range
            let resp = format!(
                "{} 416 Range Not Satisfiable\r\n\
                 Content-Range: bytes */{}\r\n\
                 Content-Length: 0\r\n\
                 ETag: {}\r\n\
                 Access-Control-Allow-Origin: *\r\n\
                 Connection: {}\r\n\
                 \r\n",
                req.http_version, file_size, etag, conn_value
            );
            writer.write_all(resp.as_bytes())?;
            return Ok(keep_alive);
        }
        // If-Range didn't match → fall through to full file response
    }

    // Full file response (200 OK)
    let resp = format!(
        "{} 200 OK\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Accept-Ranges: bytes\r\n\
         ETag: {}\r\n\
         Cache-Control: public, max-age=604800, immutable\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Expose-Headers: Content-Range, Accept-Ranges, ETag\r\n\
         Connection: {}\r\n\
         \r\n",
        req.http_version, mime_type, file_size, etag, conn_value
    );
    writer.write_all(resp.as_bytes())?;

    if !is_head {
        send_file_data(&mut file, writer, file_size)?;
    }

    Ok(keep_alive)
}

/// Serve a 206 Partial Content range response.
fn serve_range(
    writer: &mut TcpStream,
    file: &mut File,
    http_version: &str,
    mime_type: &str,
    etag: &str,
    file_size: u64,
    start: u64,
    end: u64,
    is_head: bool,
    conn_value: &str,
) -> io::Result<()> {
    let length = end - start + 1;

    file.seek(SeekFrom::Start(start))?;

    let resp = format!(
        "{} 206 Partial Content\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Content-Range: bytes {}-{}/{}\r\n\
         Accept-Ranges: bytes\r\n\
         ETag: {}\r\n\
         Cache-Control: public, max-age=604800, immutable\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Expose-Headers: Content-Range, Accept-Ranges, ETag\r\n\
         Connection: {}\r\n\
         \r\n",
        http_version, mime_type, length, start, end, file_size, etag, conn_value
    );
    writer.write_all(resp.as_bytes())?;

    if !is_head {
        send_file_data(file, writer, length)?;
    }

    Ok(())
}

// ──────────────────────────────────────────────────────────────────────────────
// HTTP utilities
// ──────────────────────────────────────────────────────────────────────────────

/// Check if a client ETag matches our ETag. Handles `*` and comma-separated lists.
fn etag_matches(client: &str, server: &str) -> bool {
    let client = client.trim();
    if client == "*" {
        return true;
    }
    // May be comma-separated list of ETags
    for tag in client.split(',') {
        let tag = tag.trim().trim_start_matches("W/"); // Ignore weak indicator
        if tag == server {
            return true;
        }
    }
    false
}

/// Parse a Range header. Returns (start, end) inclusive, or None if invalid.
fn parse_range(range_str: &str, file_size: u64) -> Option<(u64, u64)> {
    let range_str = range_str.trim();
    let spec = range_str.strip_prefix("bytes=")?;

    // No multipart ranges
    if spec.contains(',') {
        return None;
    }

    if let Some(suffix) = spec.strip_prefix('-') {
        // bytes=-N (last N bytes)
        let n: u64 = suffix.parse().ok()?;
        if n == 0 || n > file_size {
            return None;
        }
        Some((file_size - n, file_size - 1))
    } else {
        let (start_str, end_str) = spec.split_once('-')?;
        let start: u64 = start_str.parse().ok()?;
        if start >= file_size {
            return None;
        }
        let end = if end_str.is_empty() {
            file_size - 1
        } else {
            end_str.parse::<u64>().ok()?.min(file_size - 1)
        };
        if end < start {
            return None;
        }
        Some((start, end))
    }
}

/// Send `length` bytes from file to stream in 256KB chunks.
fn send_file_data(file: &mut File, stream: &mut TcpStream, length: u64) -> io::Result<()> {
    let mut remaining = length;
    let mut buf = [0u8; 262144]; // 256KB buffer
    while remaining > 0 {
        let to_read = (remaining as usize).min(buf.len());
        let n = file.read(&mut buf[..to_read])?;
        if n == 0 {
            break;
        }
        stream.write_all(&buf[..n])?;
        remaining -= n as u64;
    }
    Ok(())
}

/// Send a simple HTTP error response.
fn send_error(
    stream: &mut TcpStream,
    http_version: &str,
    status: u16,
    reason: &str,
    conn_value: &str,
) -> io::Result<()> {
    let resp = format!(
        "{} {} {}\r\n\
         Content-Length: 0\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Connection: {}\r\n\
         \r\n",
        http_version, status, reason, conn_value
    );
    stream.write_all(resp.as_bytes())
}
