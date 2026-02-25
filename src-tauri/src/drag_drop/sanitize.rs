//! Security sanitization for incoming drag content
//!
//! This module provides sanitization functions for content received from external
//! applications during drag-and-drop operations. External content is untrusted and
//! must be sanitized before being passed to JavaScript/TiddlyWiki.

#[cfg(any(target_os = "linux", target_os = "macos"))]
use regex::Regex;
#[cfg(target_os = "windows")]
use std::path::Path;
use std::path::PathBuf;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::sync::LazyLock;

/// Dangerous URL schemes that could execute code or access local files
/// Used by Linux and macOS for incoming drag content sanitization
#[cfg(any(target_os = "linux", target_os = "macos"))]
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
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn is_dangerous_url(url: &str) -> bool {
    let url_lower = url.trim().to_lowercase();
    DANGEROUS_URL_SCHEMES.iter().any(|scheme| url_lower.starts_with(scheme))
}

/// Sanitize a list of URLs (e.g., text/uri-list format)
/// Removes any dangerous URLs from the list
#[cfg(target_os = "linux")]
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
// Only needed on Linux and macOS where we handle incoming drag content
#[cfg(any(target_os = "linux", target_os = "macos"))]
static SCRIPT_TAG_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<script[^>]*>.*?</script>")
        .expect("Failed to compile SCRIPT_TAG_REGEX - this is a bug in the regex pattern")
});

#[cfg(any(target_os = "linux", target_os = "macos"))]
static EVENT_HANDLER_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    // Match on* attributes like onclick, onload, onerror, etc.
    Regex::new(r#"(?i)\s+on\w+\s*=\s*["'][^"']*["']"#)
        .expect("Failed to compile EVENT_HANDLER_REGEX - this is a bug in the regex pattern")
});

#[cfg(any(target_os = "linux", target_os = "macos"))]
static JAVASCRIPT_URL_ATTR_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    // Match href="javascript:..." or src="javascript:..." etc.
    Regex::new(r#"(?i)(href|src|action|formaction|data)\s*=\s*["']\s*javascript:[^"']*["']"#)
        .expect("Failed to compile JAVASCRIPT_URL_ATTR_REGEX - this is a bug in the regex pattern")
});

#[cfg(any(target_os = "linux", target_os = "macos"))]
static STYLE_EXPRESSION_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    // Match CSS expressions (IE) and javascript in style attributes
    Regex::new(r#"(?i)expression\s*\([^)]*\)|javascript:"#)
        .expect("Failed to compile STYLE_EXPRESSION_REGEX - this is a bug in the regex pattern")
});

/// Sanitize HTML content by removing dangerous elements and attributes
/// This provides defense-in-depth; TiddlyWiki also sanitizes HTML
#[cfg(any(target_os = "linux", target_os = "macos"))]
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

    // Check for percent-encoded traversal (including double/triple encoding)
    // Recursively decode until no more changes to catch %252e%252e -> %2e%2e -> ..
    let mut decoded = path.to_string();
    for _ in 0..5 {  // Max 5 levels of encoding (more than enough)
        match urlencoding::decode(&decoded) {
            Ok(new_decoded) => {
                if new_decoded == decoded {
                    break; // No more decoding possible
                }
                decoded = new_decoded.into_owned();
            }
            Err(_) => break,
        }
    }
    if decoded.contains("..") {
        eprintln!("[TiddlyDesktop] Security: Rejected path with encoded traversal sequence");
        return None;
    }
    // Also check the decoded path for tilde
    if decoded.starts_with('~') {
        eprintln!("[TiddlyDesktop] Security: Rejected path with encoded tilde expansion");
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
/// Used by Linux for incoming drag content sanitization
#[cfg(target_os = "linux")]
pub fn sanitize_file_paths(paths: Vec<String>) -> Vec<String> {
    paths
        .into_iter()
        .filter_map(|p| validate_file_path(&p))
        .collect()
}

/// Check if a UNC path uses an admin or system share.
/// Admin shares give direct access to entire drives (\\server\C$) or system
/// resources (\\server\ADMIN$), bypassing normal share permissions.
/// Blocks: drive-letter shares (C$, D$, ...), ADMIN$, IPC$, PRINT$.
#[cfg(target_os = "windows")]
fn is_unc_admin_share(path_lower: &str) -> bool {
    // UNC path format: \\server\share\...
    if !path_lower.starts_with("\\\\") {
        return false;
    }
    // Skip past \\server\ to get to the share name
    let after_prefix = &path_lower[2..];
    let server_end = match after_prefix.find('\\') {
        Some(pos) => pos,
        None => return false, // Just \\server with no share component
    };
    let after_server = &after_prefix[server_end + 1..];
    // Extract share name (up to next \ or end of string)
    let share_name = match after_server.find('\\') {
        Some(pos) => &after_server[..pos],
        None => after_server,
    };
    if share_name.is_empty() {
        return false;
    }
    // Block drive-letter admin shares: a$ through z$
    if share_name.len() == 2
        && share_name.ends_with('$')
        && share_name.as_bytes()[0].is_ascii_alphabetic()
    {
        eprintln!("[TiddlyDesktop] Security: Blocked admin drive share: {}", share_name);
        return true;
    }
    // Block well-known system admin shares
    if matches!(share_name, "admin$" | "ipc$" | "print$") {
        eprintln!("[TiddlyDesktop] Security: Blocked system admin share: {}", share_name);
        return true;
    }
    false
}

/// Verify the user has actual filesystem permissions to access a path.
/// For existing paths, checks metadata directly.
/// For non-existing paths (new file creation), walks up to the nearest
/// existing ancestor directory and checks that.
#[cfg(not(target_os = "android"))]
fn verify_path_permissions(path: &std::path::Path) -> bool {
    use std::io::ErrorKind;
    let mut current = path.to_path_buf();
    loop {
        match std::fs::metadata(&current) {
            Ok(_) => return true,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                // File doesn't exist yet — walk up to parent
                match current.parent() {
                    Some(parent) if parent != current.as_path() && !parent.as_os_str().is_empty() => {
                        current = parent.to_path_buf();
                    }
                    _ => return false, // Reached filesystem root without finding accessible ancestor
                }
            }
            Err(_) => {
                // Permission denied, network error, or other access failure
                return false;
            }
        }
    }
}

/// Check if a path is within the current user's accessible directories
/// This is more restrictive than just blocking system directories - it only allows:
/// 1. The current user's home directory
/// 2. Shared temporary directories (/tmp)
/// 3. Mounted volumes (for external drives)
pub fn is_user_accessible_path(path: &std::path::Path) -> bool {
    // Get the current user's home directory (not needed on Android)
    #[cfg(not(target_os = "android"))]
    let home_dir = match dirs::home_dir() {
        Some(dir) => dir,
        None => {
            eprintln!("[TiddlyDesktop] Security: Could not determine home directory");
            return false;
        }
    };

    #[cfg(not(target_os = "android"))]
    let path_str = path.to_string_lossy();

    #[cfg(target_os = "windows")]
    {
        let path_lower = path_str.to_lowercase();
        let home_lower = home_dir.to_string_lossy().to_lowercase();

        // Block known Windows system directories on any drive
        let is_system_path =
            path_lower.starts_with("c:\\windows")
            || path_lower.starts_with("c:\\program files")
            || path_lower.starts_with("c:\\program files (x86)")
            || path_lower.starts_with("c:\\programdata")
            || path_lower.starts_with("c:\\system volume information")
            || path_lower.starts_with("c:\\$recycle.bin");

        let location_allowed = !is_system_path && (
            // Allow: any local drive path (C:\data, D:\wikis, etc.)
            (path_lower.len() >= 3 && path_lower.as_bytes()[1] == b':')
            // Allow: UNC paths (\\server\share\...) — network shares the user connected to.
            // Mapped network drives (e.g. I:) resolve to UNC paths after canonicalization.
            // But NOT admin/system shares (\\*\C$, \\*\ADMIN$, etc.)
            || (path_lower.starts_with("\\\\") && !is_unc_admin_share(&path_lower))
        );

        if !location_allowed {
            return false;
        }

        // Location is in an allowed category — now verify the user has actual
        // filesystem permissions (checks the path or nearest existing ancestor)
        if !verify_path_permissions(path) {
            eprintln!(
                "[TiddlyDesktop] Security: Path in allowed location but user lacks permissions: {}",
                path.display()
            );
            return false;
        }

        true
    }

    #[cfg(target_os = "macos")]
    {
        let location_allowed =
            // Allow: current user's home directory
            path.starts_with(&home_dir)
            // Allow: /tmp and /private/tmp for temporary files
            || path_str.starts_with("/tmp") || path_str.starts_with("/private/tmp")
            // Allow: /Volumes for external drives and mounted volumes
            || path_str.starts_with("/Volumes")
            // Allow: /Applications for app resources (read-only typically)
            || path_str.starts_with("/Applications");

        if !location_allowed {
            return false;
        }

        if !verify_path_permissions(path) {
            eprintln!(
                "[TiddlyDesktop] Security: Path in allowed location but user lacks permissions: {}",
                path.display()
            );
            return false;
        }

        true
    }

    #[cfg(target_os = "linux")]
    {
        let location_allowed =
            // Allow: current user's home directory
            path.starts_with(&home_dir)
            // Allow: /tmp for temporary files
            || path_str.starts_with("/tmp")
            // Allow: /media and /mnt for mounted drives
            || path_str.starts_with("/media") || path_str.starts_with("/mnt")
            // Allow: /run/media for user-mounted drives (common on modern distros)
            || path_str.starts_with("/run/media");

        if !location_allowed {
            return false;
        }

        if !verify_path_permissions(path) {
            eprintln!(
                "[TiddlyDesktop] Security: Path in allowed location but user lacks permissions: {}",
                path.display()
            );
            return false;
        }

        true
    }

    #[cfg(target_os = "android")]
    {
        // On Android, SAF handles permissions via content:// URIs
        // For filesystem paths, allow the app's private data directory
        // and any paths the user explicitly granted via SAF
        let path_str = path.to_string_lossy();

        // Allow app's data directory
        if path_str.starts_with("/data/data/") || path_str.starts_with("/data/user/") {
            return true;
        }

        // Allow external storage locations
        if path_str.starts_with("/storage/emulated/") || path_str.starts_with("/sdcard/") {
            return true;
        }

        // Allow content:// URIs (these are validated by Android's permission system)
        // Also allow JSON-serialized FileUri (starts with "{")
        if path_str.starts_with("content://") || path_str.starts_with('{') {
            return true;
        }

        false
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux", target_os = "android")))]
    {
        // For unknown platforms, only allow home directory
        path.starts_with(&home_dir)
    }
}

/// Validate a file path for reading user files
/// Ensures path is safe and within user-accessible directories
pub fn validate_user_file_path(path: &str) -> Result<PathBuf, String> {
    if path.is_empty() {
        return Err("Path is empty".to_string());
    }

    // Check for path traversal and other issues
    if validate_file_path(path).is_none() {
        return Err("Path contains invalid sequences".to_string());
    }

    let path_buf = PathBuf::from(path);
    if !path_buf.is_absolute() {
        return Err("Path must be absolute".to_string());
    }

    // Canonicalize to resolve symlinks
    let canonical = dunce::canonicalize(&path_buf)
        .map_err(|e| format!("Failed to resolve path: {}", e))?;

    // Must be a file, not a directory
    if canonical.is_dir() {
        return Err("Path is a directory, not a file".to_string());
    }

    // Check it's in a user-accessible location
    if !is_user_accessible_path(&canonical) {
        eprintln!("[TiddlyDesktop] Security: Blocked access to system path: {}", canonical.display());
        return Err("Access to system directories is not allowed".to_string());
    }

    Ok(canonical)
}

/// Validate a directory path for user access
/// Ensures path is safe and within user-accessible directories
pub fn validate_user_directory_path(path: &str) -> Result<PathBuf, String> {
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

    if !is_user_accessible_path(&canonical) {
        eprintln!("[TiddlyDesktop] Security: Blocked access to system path: {}", canonical.display());
        return Err("Access to system directories is not allowed".to_string());
    }

    Ok(canonical)
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

        // Percent-encoded traversal (single encoding)
        assert!(validate_file_path("/home/user/%2e%2e/etc/passwd").is_none());
        assert!(validate_file_path("/path/%2E%2E/secret").is_none());

        // Double-encoded traversal (%25 = %, so %252e%252e -> %2e%2e -> ..)
        assert!(validate_file_path("/home/user/%252e%252e/etc/passwd").is_none());

        // Triple-encoded traversal
        assert!(validate_file_path("/path/%25252e%25252e/secret").is_none());

        // Encoded tilde
        assert!(validate_file_path("%7e/Documents/file.txt").is_none());
        assert!(validate_file_path("%257e/Documents/file.txt").is_none());
    }

    #[test]
    fn test_user_accessible_path() {
        use std::path::Path;

        // Get current user's home for testing
        let home = dirs::home_dir().expect("Should have home dir");
        let home_str = home.to_string_lossy();

        #[cfg(target_os = "linux")]
        {
            // Current user's home directory should be accessible
            assert!(is_user_accessible_path(&home));
            assert!(is_user_accessible_path(Path::new(&format!("{}/Documents", home_str))));
            assert!(is_user_accessible_path(Path::new(&format!("{}/.config", home_str))));

            // Temp directory should be accessible
            assert!(is_user_accessible_path(Path::new("/tmp/file.txt")));

            // USB/mounted drives should be accessible
            assert!(is_user_accessible_path(Path::new("/media/user/USB")));
            assert!(is_user_accessible_path(Path::new("/mnt/external")));
            assert!(is_user_accessible_path(Path::new("/run/media/user/USB")));

            // Other users' home directories should NOT be accessible
            assert!(!is_user_accessible_path(Path::new("/home/otheruser/Documents")));

            // System directories should be blocked
            assert!(!is_user_accessible_path(Path::new("/etc/passwd")));
            assert!(!is_user_accessible_path(Path::new("/usr/bin/bash")));
            assert!(!is_user_accessible_path(Path::new("/var/log/syslog")));
            assert!(!is_user_accessible_path(Path::new("/root/.bashrc")));
            assert!(!is_user_accessible_path(Path::new("/opt/app")));
        }

        #[cfg(target_os = "macos")]
        {
            // Current user's home directory should be accessible
            assert!(is_user_accessible_path(&home));
            assert!(is_user_accessible_path(Path::new(&format!("{}/Documents", home_str))));

            // Mounted volumes (USB drives) should be accessible
            assert!(is_user_accessible_path(Path::new("/Volumes/USB")));

            // Applications should be accessible (for resources)
            assert!(is_user_accessible_path(Path::new("/Applications/App.app")));

            // Temp should be accessible
            assert!(is_user_accessible_path(Path::new("/tmp/file.txt")));

            // Other users' home directories should NOT be accessible
            assert!(!is_user_accessible_path(Path::new("/Users/otheruser/Documents")));

            // System directories should be blocked
            assert!(!is_user_accessible_path(Path::new("/etc/passwd")));
            assert!(!is_user_accessible_path(Path::new("/System/Library")));
            assert!(!is_user_accessible_path(Path::new("/usr/bin/bash")));
        }

        #[cfg(target_os = "windows")]
        {
            // Current user's home directory should be accessible
            assert!(is_user_accessible_path(&home));

            // Other drives (USB, etc.) should be accessible
            assert!(is_user_accessible_path(Path::new("D:\\Projects")));
            assert!(is_user_accessible_path(Path::new("E:\\USB_Drive")));

            // Other users' directories on C: should NOT be accessible
            // (unless it's under current user's home)
            assert!(!is_user_accessible_path(Path::new("C:\\Users\\otheruser\\Documents")));

            // System directories should be blocked
            assert!(!is_user_accessible_path(Path::new("C:\\Windows\\System32")));
            assert!(!is_user_accessible_path(Path::new("C:\\Program Files\\App")));
        }
    }
}
