//! Utility functions for TiddlyDesktop
//!
//! This module contains common utility functions used throughout the application:
//! - HTML encoding/decoding
//! - MIME type detection
//! - Path utilities
//! - Base64 encoding

use std::path::PathBuf;

/// Decode basic HTML entities
pub fn html_decode(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#34;", "\"")
}

/// Encode basic HTML entities
pub fn html_encode(s: &str) -> String {
    s.replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace("\"", "&quot;")
}

/// Get MIME type from file extension
pub fn get_mime_type(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .as_deref()
    {
        // Images
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("ico") => "image/x-icon",
        Some("bmp") => "image/bmp",
        Some("tiff") | Some("tif") => "image/tiff",
        Some("heic") | Some("heif") => "image/heic",
        // Audio
        Some("mp3") => "audio/mpeg",
        Some("m4a") => "audio/mp4",
        Some("aac") => "audio/aac",
        Some("ogg") | Some("oga") => "audio/ogg",
        Some("opus") => "audio/opus",
        Some("wav") => "audio/wav",
        Some("flac") => "audio/flac",
        Some("aiff") | Some("aif") => "audio/aiff",
        Some("wma") => "audio/x-ms-wma",
        Some("mid") | Some("midi") => "audio/midi",
        // Video
        Some("mp4") | Some("m4v") => "video/mp4",
        Some("webm") => "video/webm",
        Some("ogv") => "video/ogg",
        Some("avi") => "video/x-msvideo",
        Some("mov") => "video/quicktime",
        Some("wmv") => "video/x-ms-wmv",
        Some("mkv") => "video/x-matroska",
        Some("3gp") => "video/3gpp",
        // Documents
        Some("pdf") => "application/pdf",
        Some("json") => "application/json",
        Some("xml") => "application/xml",
        Some("doc") => "application/msword",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("xls") => "application/vnd.ms-excel",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        Some("ppt") => "application/vnd.ms-powerpoint",
        Some("pptx") => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        // Text
        Some("txt") => "text/plain",
        Some("html") | Some("htm") => "text/html",
        Some("css") => "text/css",
        Some("js") => "application/javascript",
        Some("csv") => "text/csv",
        Some("md") => "text/markdown",
        Some("tid") => "text/vnd.tiddlywiki",
        // Fonts
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("eot") => "application/vnd.ms-fontobject",
        // Archives
        Some("zip") => "application/zip",
        Some("tar") => "application/x-tar",
        Some("gz") | Some("gzip") => "application/gzip",
        Some("rar") => "application/vnd.rar",
        Some("7z") => "application/x-7z-compressed",
        // Default
        _ => "application/octet-stream",
    }
}

/// Check if a path string looks like an absolute filesystem path
pub fn is_absolute_filesystem_path(path: &str) -> bool {
    // Unix absolute path
    if path.starts_with('/') {
        return true;
    }
    // Windows absolute path (e.g., C:\, D:\, etc.)
    if path.len() >= 3 {
        let bytes = path.as_bytes();
        if bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && (bytes[2] == b'\\' || bytes[2] == b'/')
        {
            return true;
        }
    }
    false
}

/// Compare two paths for equality
/// Uses canonical path comparison when possible to handle symlinks and different representations
/// Falls back to string comparison if canonicalization fails
pub fn paths_equal(path1: &str, path2: &str) -> bool {
    // Try to canonicalize both paths for robust comparison
    let canonical1 = dunce::canonicalize(path1);
    let canonical2 = dunce::canonicalize(path2);

    match (canonical1, canonical2) {
        (Ok(c1), Ok(c2)) => c1 == c2,
        _ => {
            // Fall back to string comparison if canonicalization fails
            // (e.g., for paths that don't exist yet)
            #[cfg(target_os = "windows")]
            {
                path1.eq_ignore_ascii_case(path2)
            }
            #[cfg(not(target_os = "windows"))]
            {
                path1 == path2
            }
        }
    }
}

/// Normalize a path for cross-platform compatibility
/// On Windows: removes \\?\ prefixes and ensures proper separators
pub fn normalize_path(path: PathBuf) -> PathBuf {
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
pub fn is_wiki_folder(path: &std::path::Path) -> bool {
    path.is_dir() && path.join("tiddlywiki.info").exists()
}

/// Simple base64 URL-safe encoding for path keys
pub fn base64_url_encode(input: &str) -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    URL_SAFE_NO_PAD.encode(input.as_bytes())
}

/// Decode base64 URL-safe string
pub fn base64_url_decode(input: &str) -> Option<String> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    URL_SAFE_NO_PAD
        .decode(input)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
}
