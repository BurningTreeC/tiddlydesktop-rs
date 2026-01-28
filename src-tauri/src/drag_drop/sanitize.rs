//! Security sanitization for incoming drag content
//!
//! This module provides sanitization functions for content received from external
//! applications during drag-and-drop operations. External content is untrusted and
//! must be sanitized before being passed to JavaScript/TiddlyWiki.

use regex::Regex;
use std::path::PathBuf;
use std::sync::LazyLock;

/// Dangerous URL schemes that could execute code or access local files
const DANGEROUS_URL_SCHEMES: &[&str] = &[
    "javascript:",
    "vbscript:",
    "data:text/html",
    "data:application/javascript",
    "data:application/x-javascript",
    "about:",      // Can execute code in some contexts
    "blob:",       // Can contain executable content
];

/// Check if a URL has a dangerous scheme
pub fn is_dangerous_url(url: &str) -> bool {
    let url_lower = url.trim().to_lowercase();
    DANGEROUS_URL_SCHEMES.iter().any(|scheme| url_lower.starts_with(scheme))
}

/// Sanitize a list of URLs (e.g., text/uri-list format)
/// Removes any dangerous URLs from the list
#[cfg(any(target_os = "windows", target_os = "linux"))]
pub fn sanitize_uri_list(uri_list: &str) -> String {
    uri_list
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            // Keep comments and empty lines
            if trimmed.is_empty() || trimmed.starts_with('#') {
                return true;
            }
            // Filter out dangerous URLs
            if is_dangerous_url(trimmed) {
                eprintln!("[TiddlyDesktop] Security: Removed dangerous URL from uri-list");
                return false;
            }
            true
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// Regex patterns for HTML sanitization (compiled once)
static SCRIPT_TAG_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<script[^>]*>.*?</script>")
        .expect("Failed to compile SCRIPT_TAG_REGEX - this is a bug in the regex pattern")
});

static EVENT_HANDLER_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    // Match on* attributes like onclick, onload, onerror, etc.
    Regex::new(r#"(?i)\s+on\w+\s*=\s*["'][^"']*["']"#)
        .expect("Failed to compile EVENT_HANDLER_REGEX - this is a bug in the regex pattern")
});

static JAVASCRIPT_URL_ATTR_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    // Match href="javascript:..." or src="javascript:..." etc.
    Regex::new(r#"(?i)(href|src|action|formaction|data)\s*=\s*["']\s*javascript:[^"']*["']"#)
        .expect("Failed to compile JAVASCRIPT_URL_ATTR_REGEX - this is a bug in the regex pattern")
});

static STYLE_EXPRESSION_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    // Match CSS expressions (IE) and javascript in style attributes
    Regex::new(r#"(?i)expression\s*\([^)]*\)|javascript:"#)
        .expect("Failed to compile STYLE_EXPRESSION_REGEX - this is a bug in the regex pattern")
});

/// Sanitize HTML content by removing dangerous elements and attributes
/// This provides defense-in-depth; TiddlyWiki also sanitizes HTML
pub fn sanitize_html(html: &str) -> String {
    let mut result = html.to_string();

    // Remove <script> tags and their contents
    result = SCRIPT_TAG_REGEX.replace_all(&result, "").to_string();

    // Remove event handler attributes (onclick, onload, onerror, etc.)
    result = EVENT_HANDLER_REGEX.replace_all(&result, "").to_string();

    // Remove javascript: URLs in href, src, action attributes
    result = JAVASCRIPT_URL_ATTR_REGEX.replace_all(&result, "").to_string();

    // Remove CSS expressions and javascript in style attributes
    result = STYLE_EXPRESSION_REGEX.replace_all(&result, "").to_string();

    if result.len() != html.len() {
        eprintln!("[TiddlyDesktop] Security: Sanitized potentially dangerous HTML content");
    }

    result
}

/// Validate a file path for safety (basic validation for drag-drop)
/// Returns None if the path contains path traversal sequences
pub fn validate_file_path(path: &str) -> Option<String> {
    // Check for path traversal attempts
    if path.contains("..") {
        eprintln!("[TiddlyDesktop] Security: Rejected path with traversal sequence: {}", path);
        return None;
    }

    // Check for tilde expansion (home directory)
    if path.starts_with('~') {
        eprintln!("[TiddlyDesktop] Security: Rejected path with tilde expansion: {}", path);
        return None;
    }

    // Check for percent-encoded traversal
    let decoded = urlencoding::decode(path).unwrap_or_else(|_| path.into());
    if decoded.contains("..") {
        eprintln!("[TiddlyDesktop] Security: Rejected path with encoded traversal sequence");
        return None;
    }

    // On Windows, also check for alternate data streams and device names
    #[cfg(target_os = "windows")]
    {
        // Block alternate data streams
        if path.contains(':') && !path.chars().nth(1).map(|c| c == ':').unwrap_or(false) {
            // Allow drive letters (C:) but block ADS (file.txt:stream)
            if path.matches(':').count() > 1 {
                eprintln!("[TiddlyDesktop] Security: Rejected path with alternate data stream");
                return None;
            }
        }
        // Block device names
        const DEVICE_NAMES: &[&str] = &[
            "CON", "PRN", "AUX", "NUL",
            "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9",
            "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
        ];
        let filename = Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        if DEVICE_NAMES.contains(&filename.to_uppercase().as_str()) {
            eprintln!("[TiddlyDesktop] Security: Rejected reserved device name in path");
            return None;
        }
    }

    Some(path.to_string())
}

/// Validate that a path is a valid wiki file path for reading
/// This is used by Tauri commands that load wiki files
pub fn validate_wiki_path(path: &str) -> Result<PathBuf, String> {
    // Basic validation
    if path.is_empty() {
        return Err("Path is empty".to_string());
    }

    // Check for path traversal and other issues
    if validate_file_path(path).is_none() {
        return Err("Path contains invalid sequences".to_string());
    }

    // The path must be absolute
    let path_buf = PathBuf::from(path);
    if !path_buf.is_absolute() {
        return Err("Path must be absolute".to_string());
    }

    // Canonicalize to resolve symlinks and normalize
    let canonical = dunce::canonicalize(&path_buf)
        .map_err(|e| format!("Failed to resolve path: {}", e))?;

    // Verify it's a file (not a directory) - for load/save operations
    if canonical.is_dir() {
        return Err("Path is a directory, not a file".to_string());
    }

    // Check file extension - must be .html or .htm for wiki files
    match canonical.extension().and_then(|e| e.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("html") || ext.eq_ignore_ascii_case("htm") => {}
        _ => {
            return Err("Wiki files must have .html or .htm extension".to_string());
        }
    }

    Ok(canonical)
}

/// Validate that a path is a valid wiki file path for writing
/// Unlike validate_wiki_path, this allows the file to not exist yet (but parent must exist)
pub fn validate_wiki_path_for_write(path: &str) -> Result<PathBuf, String> {
    // Basic validation
    if path.is_empty() {
        return Err("Path is empty".to_string());
    }

    // Check for path traversal and other issues
    if validate_file_path(path).is_none() {
        return Err("Path contains invalid sequences".to_string());
    }

    // The path must be absolute
    let path_buf = PathBuf::from(path);
    if !path_buf.is_absolute() {
        return Err("Path must be absolute".to_string());
    }

    // Check file extension first - must be .html or .htm for wiki files
    match path_buf.extension().and_then(|e| e.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("html") || ext.eq_ignore_ascii_case("htm") => {}
        _ => {
            return Err("Wiki files must have .html or .htm extension".to_string());
        }
    }

    // If file exists, canonicalize it
    if path_buf.exists() {
        let canonical = dunce::canonicalize(&path_buf)
            .map_err(|e| format!("Failed to resolve path: {}", e))?;

        if canonical.is_dir() {
            return Err("Path is a directory, not a file".to_string());
        }

        return Ok(canonical);
    }

    // File doesn't exist - validate that parent directory exists and is accessible
    let parent = path_buf.parent()
        .ok_or_else(|| "Path has no parent directory".to_string())?;

    if !parent.exists() {
        return Err("Parent directory does not exist".to_string());
    }

    // Canonicalize the parent to ensure it's valid
    let canonical_parent = dunce::canonicalize(parent)
        .map_err(|e| format!("Failed to resolve parent directory: {}", e))?;

    if !canonical_parent.is_dir() {
        return Err("Parent path is not a directory".to_string());
    }

    // Return the path with canonicalized parent + original filename
    let filename = path_buf.file_name()
        .ok_or_else(|| "Path has no filename".to_string())?;

    Ok(canonical_parent.join(filename))
}

/// Validate that a path exists and is a directory
/// Used for backup directory validation
pub fn validate_directory_path(path: &str) -> Result<PathBuf, String> {
    if path.is_empty() {
        return Err("Path is empty".to_string());
    }

    if validate_file_path(path).is_none() {
        return Err("Path contains invalid sequences".to_string());
    }

    let path_buf = PathBuf::from(path);
    if !path_buf.is_absolute() {
        return Err("Path must be absolute".to_string());
    }

    let canonical = dunce::canonicalize(&path_buf)
        .map_err(|e| format!("Failed to resolve path: {}", e))?;

    if !canonical.is_dir() {
        return Err("Path is not a directory".to_string());
    }

    Ok(canonical)
}

/// Sanitize a list of file paths
/// Returns only the valid paths
pub fn sanitize_file_paths(paths: Vec<String>) -> Vec<String> {
    paths
        .into_iter()
        .filter_map(|p| validate_file_path(&p))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dangerous_urls() {
        // Dangerous schemes
        assert!(is_dangerous_url("javascript:alert(1)"));
        assert!(is_dangerous_url("JAVASCRIPT:alert(1)"));
        assert!(is_dangerous_url("  javascript:alert(1)"));
        assert!(is_dangerous_url("vbscript:msgbox(1)"));
        assert!(is_dangerous_url("data:text/html,<script>alert(1)</script>"));
        assert!(is_dangerous_url("about:blank"));
        assert!(is_dangerous_url("blob:http://example.com/uuid"));

        // Safe schemes
        assert!(!is_dangerous_url("https://example.com"));
        assert!(!is_dangerous_url("http://example.com"));
        assert!(!is_dangerous_url("file:///path/to/file"));
        assert!(!is_dangerous_url("data:text/plain,hello"));
        assert!(!is_dangerous_url("data:image/png;base64,ABC123"));
    }

    #[test]
    fn test_html_sanitization() {
        // Script tags
        assert_eq!(
            sanitize_html("<p>Hello</p><script>alert(1)</script><p>World</p>"),
            "<p>Hello</p><p>World</p>"
        );

        // Event handlers
        assert_eq!(
            sanitize_html(r#"<img src="x" onerror="alert(1)">"#),
            r#"<img src="x">"#
        );

        // JavaScript URLs (note: space before > remains after attribute removal, but the dangerous URL is gone)
        assert_eq!(
            sanitize_html(r#"<a href="javascript:alert(1)">Click</a>"#),
            r#"<a >Click</a>"#
        );

        // Safe HTML passes through
        assert_eq!(
            sanitize_html("<p>Hello <b>World</b></p>"),
            "<p>Hello <b>World</b></p>"
        );
    }

    #[test]
    fn test_path_validation() {
        // Valid paths
        assert!(validate_file_path("/home/user/file.txt").is_some());
        assert!(validate_file_path("C:\\Users\\file.txt").is_some());

        // Path traversal
        assert!(validate_file_path("/home/user/../etc/passwd").is_none());
        assert!(validate_file_path("..\\..\\Windows\\System32").is_none());

        // Tilde expansion
        assert!(validate_file_path("~/Documents/file.txt").is_none());
        assert!(validate_file_path("~user/file.txt").is_none());

        // Percent-encoded traversal
        assert!(validate_file_path("/home/user/%2e%2e/etc/passwd").is_none());
        assert!(validate_file_path("/path/%2E%2E/secret").is_none());
    }
}
