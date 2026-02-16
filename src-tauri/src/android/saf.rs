//! Storage Access Framework (SAF) operations for Android.
//!
//! This module provides file operations using Android's SAF, which allows
//! access to user-selected directories via `content://` URIs.
//!
//! Uses tauri-plugin-android-fs for the actual SAF operations.
//!
//! FileUri is serialized to JSON for storage. Format:
//! {"uri":"content://...","documentTopTreeUri":null}

#![cfg(target_os = "android")]

use tauri_plugin_android_fs::{AndroidFsExt, FileAccessMode, FileUri};
use std::io::{Read, Write};
use chrono::Local;

/// Get the app handle.
fn get_app() -> Result<tauri::AppHandle, String> {
    crate::get_global_app_handle()
        .ok_or_else(|| "App not initialized".to_string())
}

/// Parse a stored URI (JSON string) back into a FileUri.
///
/// Stored URIs are JSON-serialized FileUri objects, e.g.:
/// {"uri":"content://...","documentTopTreeUri":null}
fn parse_uri(uri_json: &str) -> Result<FileUri, String> {
    // If it looks like JSON (starts with {), parse it
    if uri_json.trim().starts_with('{') {
        FileUri::from_json_str(uri_json)
            .map_err(|e| format!("Failed to parse FileUri JSON: {:?}", e))
    } else {
        // Legacy or simple content:// URI - wrap in minimal JSON
        // This handles the case where we just have "content://..." stored
        let json = format!(r#"{{"uri":"{}","documentTopTreeUri":null}}"#, uri_json);
        FileUri::from_json_str(&json)
            .map_err(|e| format!("Failed to create FileUri from URI: {:?}", e))
    }
}

/// Convert a FileUri to a storable JSON string.
fn uri_to_string(uri: &FileUri) -> String {
    uri.to_json_string().unwrap_or_else(|_| String::new())
}

/// Read a document as a UTF-8 string.
pub fn read_document_string(uri: &str) -> Result<String, String> {
    let app = get_app()?;
    let api = app.android_fs();
    let file_uri = parse_uri(uri)?;

    // Open file for reading
    let mut file = api.open_file(&file_uri, FileAccessMode::Read)
        .map_err(|e| format!("Failed to open file for reading: {:?}", e))?;

    // Read contents
    let mut contents = String::new();
    file.read_to_string(&mut contents)
        .map_err(|e| format!("Failed to read file: {}", e))?;

    Ok(contents)
}

/// Read a document as raw bytes.
pub fn read_document_bytes(uri: &str) -> Result<Vec<u8>, String> {
    let app = get_app()?;
    let api = app.android_fs();
    let file_uri = parse_uri(uri)?;

    // Open file for reading
    let mut file = api.open_file(&file_uri, FileAccessMode::Read)
        .map_err(|e| format!("Failed to open file for reading: {:?}", e))?;

    // Read contents
    let mut contents = Vec::new();
    file.read_to_end(&mut contents)
        .map_err(|e| format!("Failed to read file: {}", e))?;

    Ok(contents)
}

/// Open a document for reading as a stream.
/// Returns a boxed reader that can be read in chunks to avoid loading the entire file into memory.
pub fn open_document_reader(uri: &str) -> Result<Box<dyn std::io::Read>, String> {
    let app = get_app()?;
    let api = app.android_fs();
    let file_uri = parse_uri(uri)?;

    let file = api.open_file(&file_uri, FileAccessMode::Read)
        .map_err(|e| format!("Failed to open file for reading: {:?}", e))?;

    Ok(Box::new(file))
}

/// Get the file size in bytes via SAF content resolver.
pub fn get_document_size(uri: &str) -> Result<u64, String> {
    let app = get_app()?;
    let api = app.android_fs();
    let file_uri = parse_uri(uri)?;

    api.get_len(&file_uri)
        .map_err(|e| format!("Failed to get file size: {:?}", e))
}

/// Write a string to a document.
pub fn write_document_string(uri: &str, content: &str) -> Result<(), String> {
    eprintln!("[SAF] write_document_string called");
    eprintln!("[SAF]   uri: {}", uri);
    eprintln!("[SAF]   content length: {} bytes", content.len());

    let app = get_app()?;
    let api = app.android_fs();
    let file_uri = parse_uri(uri)?;

    eprintln!("[SAF]   parsed file_uri successfully");

    // Open file for writing (truncate existing content)
    let mut file = api.open_file(&file_uri, FileAccessMode::WriteTruncate)
        .map_err(|e| {
            eprintln!("[SAF]   FAILED to open file for writing: {:?}", e);
            format!("Failed to open file for writing: {:?}", e)
        })?;

    eprintln!("[SAF]   file opened for writing");

    // Write contents
    file.write_all(content.as_bytes())
        .map_err(|e| {
            eprintln!("[SAF]   FAILED to write: {}", e);
            format!("Failed to write file: {}", e)
        })?;

    eprintln!("[SAF]   write successful");
    Ok(())
}

/// Write raw bytes to a document.
pub fn write_document_bytes(uri: &str, content: &[u8]) -> Result<(), String> {
    let app = get_app()?;
    let api = app.android_fs();
    let file_uri = parse_uri(uri)?;

    // Open file for writing (truncate existing content)
    let mut file = api.open_file(&file_uri, FileAccessMode::WriteTruncate)
        .map_err(|e| format!("Failed to open file for writing: {:?}", e))?;

    // Write contents
    file.write_all(content)
        .map_err(|e| format!("Failed to write file: {}", e))?;

    Ok(())
}

/// Check if a document exists.
pub fn document_exists(uri: &str) -> bool {
    let app = match get_app() {
        Ok(app) => app,
        Err(_) => return false,
    };
    let api = app.android_fs();
    let file_uri = match parse_uri(uri) {
        Ok(uri) => uri,
        Err(_) => return false,
    };

    // Try to get file info - if it succeeds, file exists
    api.get_name(&file_uri).is_ok()
}

/// Check if a URI points to a directory.
pub fn is_directory(uri: &str) -> bool {
    // Tree URIs (content://.../tree/...) are directories by definition
    // They come from ACTION_OPEN_DOCUMENT_TREE and always represent folders.
    // Check the raw URI string before parsing, since tree URIs may not work
    // with MIME type queries.
    let raw_uri = if uri.trim().starts_with('{') {
        serde_json::from_str::<serde_json::Value>(uri)
            .ok()
            .and_then(|json| json.get("uri").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .unwrap_or_default()
    } else {
        uri.to_string()
    };
    if raw_uri.contains("/tree/") {
        eprintln!("[SAF] is_directory: tree URI detected, returning true");
        return true;
    }

    let app = match get_app() {
        Ok(app) => app,
        Err(_) => return false,
    };
    let api = app.android_fs();
    let file_uri = match parse_uri(uri) {
        Ok(uri) => uri,
        Err(e) => {
            eprintln!("[SAF] is_directory: failed to parse URI '{}': {}", uri, e);
            return false;
        }
    };

    // Method 1: Check MIME type - directories have vnd.android.document/directory
    match api.get_mime_type(&file_uri) {
        Ok(mime) => {
            eprintln!("[SAF] is_directory: MIME type is '{}'", mime);
            if mime == "vnd.android.document/directory" {
                return true;
            }
            // If we got a MIME type and it's NOT a directory type, it's definitely a file
            // Common file MIME types start with text/, application/, image/, audio/, video/
            // The directory MIME type is specifically "vnd.android.document/directory"
            if !mime.is_empty() && mime != "application/octet-stream" {
                eprintln!("[SAF] is_directory: MIME type '{}' indicates file, not directory", mime);
                return false;
            }
        }
        Err(e) => {
            eprintln!("[SAF] is_directory: get_mime_type failed: {:?}", e);
        }
    }

    // Method 2: Try to open the file for reading - files can be opened, directories cannot
    // This is more reliable than read_dir() which can sometimes succeed on files
    match api.open_file(&file_uri, FileAccessMode::Read) {
        Ok(_) => {
            // Successfully opened as a file - it's NOT a directory
            eprintln!("[SAF] is_directory: open_file succeeded, so it's a FILE (not directory)");
            return false;
        }
        Err(e) => {
            eprintln!("[SAF] is_directory: open_file failed: {:?}", e);
        }
    }

    // Method 3: If we couldn't open as a file, try read_dir as last resort
    // This handles tree URIs from folder picker that may not have a MIME type
    match api.read_dir(&file_uri) {
        Ok(_) => {
            eprintln!("[SAF] is_directory: read_dir succeeded, treating as directory");
            true
        }
        Err(e) => {
            eprintln!("[SAF] is_directory: read_dir failed: {:?}", e);
            false
        }
    }
}

/// Directory entry with name and URI.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub uri: String,
    pub is_dir: bool,
    /// File size in bytes (0 for directories)
    pub size: u64,
}

/// List contents of a directory (names only, for backwards compatibility).
pub fn list_directory(uri: &str) -> Result<Vec<String>, String> {
    let app = get_app()?;
    let api = app.android_fs();
    let file_uri = parse_uri(uri)?;

    let entries = api.read_dir(&file_uri)
        .map_err(|e| format!("Failed to read directory: {:?}", e))?;

    let names: Vec<String> = entries.map(|entry| {
        match entry {
            tauri_plugin_android_fs::Entry::File { name, .. } => name,
            tauri_plugin_android_fs::Entry::Dir { name, .. } => name,
        }
    }).collect();

    Ok(names)
}

/// List contents of a directory with full URIs.
pub fn list_directory_entries(uri: &str) -> Result<Vec<DirEntry>, String> {
    let app = get_app()?;
    let api = app.android_fs();
    let file_uri = parse_uri(uri)?;

    let entries = api.read_dir(&file_uri)
        .map_err(|e| format!("Failed to read directory: {:?}", e))?;

    let result: Vec<DirEntry> = entries.map(|entry| {
        match entry {
            tauri_plugin_android_fs::Entry::File { name, uri, len, .. } => DirEntry {
                name,
                uri: uri_to_string(&uri),
                is_dir: false,
                size: len,
            },
            tauri_plugin_android_fs::Entry::Dir { name, uri, .. } => DirEntry {
                name,
                uri: uri_to_string(&uri),
                is_dir: true,
                size: 0,
            },
        }
    }).collect();

    Ok(result)
}

/// Find a file or directory by name in a directory.
/// Returns the URI if found.
pub fn find_in_directory(parent_uri: &str, name: &str) -> Result<Option<String>, String> {
    let entries = list_directory_entries(parent_uri)?;
    for entry in entries {
        if entry.name == name {
            return Ok(Some(entry.uri));
        }
    }
    Ok(None)
}

/// Find an existing subdirectory in a directory.
/// Returns the URI if found, None if not found.
/// Note: Directory creation is not supported by the Android FS plugin,
/// so users must pick existing directories via the directory picker.
pub fn find_subdirectory(parent_uri: &str, name: &str) -> Result<Option<String>, String> {
    let entries = list_directory_entries(parent_uri)?;
    for entry in entries {
        if entry.name == name && entry.is_dir {
            return Ok(Some(entry.uri));
        }
    }
    Ok(None)
}

/// Get the parent directory URI for a file.
///
/// For SAF URIs, this extracts the parent directory from the document path.
/// When `documentTopTreeUri` is available (a tree URI like `content://auth/tree/treeId`),
/// it's used to construct a proper tree document URI for the parent, which is required
/// for directory listing operations.
pub fn get_parent_uri(uri: &str) -> Result<String, String> {
    // Extract document path: everything after "/document/"
    fn extract_doc_path(full_uri: &str) -> Option<&str> {
        full_uri.find("/document/").map(|idx| &full_uri[idx + "/document/".len()..])
    }

    // Remove last path component from encoded document path (%2F is the separator)
    fn parent_doc_path(doc_path: &str) -> Option<&str> {
        let lower = doc_path.to_lowercase();
        lower.rfind("%2f").map(|idx| &doc_path[..idx])
    }

    // Try to parse as JSON FileUri
    if uri.trim().starts_with('{') {
        if let Ok(file_uri) = FileUri::from_json_str(uri) {
            if let Ok(json_str) = file_uri.to_json_string() {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&json_str) {
                    let main_uri = json.get("uri").and_then(|v| v.as_str()).unwrap_or("");
                    let tree_uri = json.get("documentTopTreeUri").and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty() && *s != "null");

                    // Get parent document path from the file's URI
                    if let Some(doc_path) = extract_doc_path(main_uri) {
                        if let Some(parent_path) = parent_doc_path(doc_path) {
                            // If we have a tree URI, construct a tree document URI for the parent.
                            // This is required for SAF directory listing to work.
                            // Strip any /document/ suffix from the tree URI to get just the tree root.
                            if let Some(tree_val) = tree_uri.filter(|t| t.contains("/tree/")) {
                                let tree_root = tree_val.find("/document/")
                                    .map(|idx| &tree_val[..idx])
                                    .unwrap_or(tree_val);
                                let parent_doc_uri = format!("{}/document/{}", tree_root, parent_path);
                                return Ok(format!(
                                    r#"{{"uri":"{}","documentTopTreeUri":"{}"}}"#,
                                    parent_doc_uri, tree_root
                                ));
                            }

                            // No tree URI — construct plain document URI (directory listing won't work)
                            let authority = main_uri.find("/document/")
                                .map(|idx| &main_uri[..idx])
                                .unwrap_or(main_uri);
                            let parent_uri = format!("{}/document/{}", authority, parent_path);
                            return Ok(format!(
                                r#"{{"uri":"{}","documentTopTreeUri":"{}"}}"#,
                                parent_uri, authority
                            ));
                        }
                    }
                }
            }
        }
    }

    // For simple content:// URIs (not wrapped in JSON)
    if uri.starts_with("content://") {
        if let Some(doc_path) = extract_doc_path(uri) {
            if let Some(parent_path) = parent_doc_path(doc_path) {
                let authority = uri.find("/document/")
                    .map(|idx| &uri[..idx])
                    .unwrap_or(uri);
                let parent_uri = format!("{}/document/{}", authority, parent_path);
                return Ok(format!(
                    r#"{{"uri":"{}","documentTopTreeUri":"{}"}}"#,
                    parent_uri, authority
                ));
            }
        }
    }

    Err("Cannot determine parent URI".to_string())
}

/// Delete a document.
pub fn delete_document(uri: &str) -> Result<(), String> {
    let app = get_app()?;
    let api = app.android_fs();
    let file_uri = parse_uri(uri)?;

    api.remove_file(&file_uri)
        .map_err(|e| format!("Failed to delete document: {:?}", e))
}

/// Create a new file in a directory.
/// Returns the JSON-serialized FileUri of the new file.
///
/// NOTE: We pass None for MIME type to let Android infer from the filename.
/// This prevents filename mangling that occurs when specific MIME types are used
/// (e.g., "application/json" causes ".json" to be appended to filenames like "tiddlywiki.info").
pub fn create_file(parent_uri: &str, name: &str, _mime_type: Option<&str>) -> Result<String, String> {
    let app = get_app()?;
    let api = app.android_fs();
    let parent = parse_uri(parent_uri)?;

    // Pass None to let Android infer MIME type from filename, preserving exact filename
    let new_uri = api.create_new_file(&parent, name, None)
        .map_err(|e| format!("Failed to create file: {:?}", e))?;

    Ok(uri_to_string(&new_uri))
}

/// Create a new directory in a parent directory.
/// Returns the JSON-serialized FileUri of the new directory.
/// Note: This uses the DocumentsContract.createDocument API with directory MIME type.
pub fn create_directory(parent_uri: &str, name: &str) -> Result<String, String> {
    let app = get_app()?;
    let api = app.android_fs();
    let parent = parse_uri(parent_uri)?;

    // Use the directory MIME type to create a directory
    let new_uri = api.create_new_file(&parent, name, Some("vnd.android.document/directory"))
        .map_err(|e| format!("Failed to create directory: {:?}", e))?;

    Ok(uri_to_string(&new_uri))
}

/// Find or create a subdirectory.
/// Returns the URI of the existing or newly created directory.
pub fn find_or_create_subdirectory(parent_uri: &str, name: &str) -> Result<String, String> {
    // First try to find existing
    if let Some(uri) = find_subdirectory(parent_uri, name)? {
        return Ok(uri);
    }

    // Try to create it
    create_directory(parent_uri, name)
}

/// Persist access permission for a URI across app restarts.
pub fn persist_permission(uri: &str) -> Result<(), String> {
    let app = get_app()?;
    let api = app.android_fs_async();
    let file_uri = parse_uri(uri)?;

    // Use block_on to call async method from sync context
    tauri::async_runtime::block_on(async {
        api.file_picker().persist_uri_permission(&file_uri).await
            .map_err(|e| format!("Failed to persist permission: {:?}", e))
    })
}

/// Check if we have persisted permission for a URI.
/// Note: This is a simplified check - we try to access the file.
pub fn has_permission(uri: &str) -> bool {
    // Simple check: try to get file name
    let app = match get_app() {
        Ok(app) => app,
        Err(_) => return false,
    };
    let api = app.android_fs();
    let file_uri = match parse_uri(uri) {
        Ok(u) => u,
        Err(_) => return false,
    };

    // If we can access the file, we have permission
    api.get_name(&file_uri).is_ok()
}

/// Release persisted permission for a URI.
/// Note: There doesn't seem to be an API for this, so it's a no-op.
pub fn release_permission(_uri: &str) {
    // No-op - the plugin doesn't provide a release method
    // Permissions are released by the system when the app is uninstalled
    // or when the user revokes access from Settings
}

/// Open a directory picker and return the selected directory as JSON-serialized FileUri.
/// Note: For backup directories that need write access, use pick_directory_with_write() instead.
pub async fn pick_directory() -> Result<Option<String>, String> {
    let app = get_app()?;
    let api = app.android_fs_async();

    match api.file_picker().pick_dir(None, false).await {
        Ok(Some(uri)) => {
            // Persist permission for future access
            let _ = api.file_picker().persist_uri_permission(&uri).await;
            Ok(Some(uri_to_string(&uri)))
        }
        Ok(None) => Ok(None),
        Err(e) => Err(format!("Directory picker failed: {:?}", e)),
    }
}

/// Open a directory picker for directories that need write access (like backup directories).
/// After picking, verifies write access by creating and deleting a test file.
/// Returns the selected directory as JSON-serialized FileUri, or an error if write access is not available.
pub async fn pick_directory_with_write() -> Result<Option<String>, String> {
    let app = get_app()?;
    let api = app.android_fs_async();

    match api.file_picker().pick_dir(None, false).await {
        Ok(Some(uri)) => {
            // Persist permission for future access
            let _ = api.file_picker().persist_uri_permission(&uri).await;

            let uri_str = uri_to_string(&uri);

            // Test write access by creating and deleting a test file
            let test_filename = format!(".tiddlydesktop_write_test_{}", std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0));

            match create_file(&uri_str, &test_filename, None) {
                Ok(test_file_uri) => {
                    // Write access works - delete the test file
                    let _ = delete_document(&test_file_uri);
                    eprintln!("[SAF] Write access verified for backup directory");
                    Ok(Some(uri_str))
                }
                Err(e) => {
                    eprintln!("[SAF] Write access test failed: {}", e);
                    Err("The selected folder does not have write permission. Please try selecting the folder again, or choose a different location.".to_string())
                }
            }
        }
        Ok(None) => Ok(None),
        Err(e) => Err(format!("Directory picker failed: {:?}", e)),
    }
}

/// Open a file picker for HTML files, then request folder access for attachments.
/// Two-step process:
/// 1. File picker - user selects the exact wiki file they want
/// 2. Folder picker - grants tree access to the folder (for creating attachments)
/// Returns JSON with uri and documentTopTreeUri set.
pub async fn pick_wiki_file() -> Result<Option<String>, String> {
    pick_wiki_file_with_folder_access().await
}

/// Open a file picker for HTML files, then request folder access for attachments.
/// Two-step process:
/// 1. File picker - user selects the exact wiki file they want
/// 2. Folder picker - grants tree access to the folder (for creating attachments)
/// Returns JSON with uri and documentTopTreeUri set.
pub async fn pick_wiki_file_with_folder_access() -> Result<Option<String>, String> {
    let app = get_app()?;
    let api = app.android_fs_async();

    // Step 1: Let user pick the specific wiki file
    let file_uri_opt = api.file_picker().pick_file(
        None,            // Initial location
        &["text/html"],  // Filter for HTML files
        false,           // Not local-only
    ).await.map_err(|e| format!("File picker failed: {:?}", e))?;

    let file_uri = match file_uri_opt {
        Some(uri) => uri,
        None => return Ok(None),
    };

    // Persist read permission for the file
    let _ = api.file_picker().persist_uri_permission(&file_uri).await;

    // Get the file URI as JSON
    let file_uri_str = uri_to_string(&file_uri);
    eprintln!("[SAF] pick_wiki_file: selected file URI: {}", file_uri_str);

    // Step 2: Request folder access for attachments
    // Show a message explaining why we need folder access
    eprintln!("[SAF] pick_wiki_file: requesting folder access for attachments...");

    let tree_uri_opt = api.file_picker().pick_dir(
        None,   // Initial location (ideally would be same folder as selected file)
        false,  // Persist permission (we do it manually below)
    ).await.map_err(|e| format!("Folder picker failed: {:?}", e))?;

    // Build the result JSON
    let mut json: serde_json::Value = serde_json::from_str(&file_uri_str)
        .unwrap_or_else(|_| serde_json::json!({"uri": file_uri_str}));

    // If user granted folder access, add tree URI
    if let Some(tree_uri) = tree_uri_opt {
        // Persist write permission for the tree
        let _ = api.file_picker().persist_uri_permission(&tree_uri).await;

        let tree_uri_str = uri_to_string(&tree_uri);
        let tree_json: serde_json::Value = serde_json::from_str(&tree_uri_str)
            .map_err(|e| format!("Failed to parse tree URI JSON: {}", e))?;

        if let Some(tree_uri_value) = tree_json.get("uri") {
            json["documentTopTreeUri"] = tree_uri_value.clone();
            eprintln!("[SAF] pick_wiki_file: folder access granted: {}", tree_uri_value);
        }
    } else {
        eprintln!("[SAF] pick_wiki_file: folder access denied/skipped - attachments won't work");
        // Still allow opening the wiki, just without attachment support
        json["documentTopTreeUri"] = serde_json::Value::Null;
    }

    Ok(Some(json.to_string()))
}

/// Open a save dialog to create a new wiki file.
/// Two-step process:
/// 1. Save dialog (ACTION_CREATE_DOCUMENT) - user picks location AND filename
/// 2. Folder picker - grants tree access for attachments
/// Returns the JSON-serialized FileUri of the new file with documentTopTreeUri set.
pub async fn save_wiki_file(suggested_name: &str) -> Result<Option<String>, String> {
    let app = get_app()?;
    let api = app.android_fs_async();

    // Step 1: Show save dialog - user picks location and can edit filename
    let file_uri_opt = api.file_picker().save_file(
        None,           // Initial location
        suggested_name, // Suggested filename (user can edit)
        Some("text/html"),
        false,          // Not local-only
    ).await.map_err(|e| format!("Save dialog failed: {:?}", e))?;

    let file_uri = match file_uri_opt {
        Some(uri) => uri,
        None => return Ok(None),
    };

    // Persist permission for the file
    let _ = api.file_picker().persist_uri_permission(&file_uri).await;

    // Get the file URI as JSON
    let file_uri_str = uri_to_string(&file_uri);
    eprintln!("[SAF] save_wiki_file: created file URI: {}", file_uri_str);

    // Step 2: Request folder access for attachments
    eprintln!("[SAF] save_wiki_file: requesting folder access for attachments...");

    let tree_uri_opt = api.file_picker().pick_dir(
        None,   // Initial location (ideally same folder as the file)
        false,  // Persist permission (we do it manually below)
    ).await.map_err(|e| format!("Folder picker failed: {:?}", e))?;

    // Build the result JSON
    let mut json: serde_json::Value = serde_json::from_str(&file_uri_str)
        .unwrap_or_else(|_| serde_json::json!({"uri": file_uri_str}));

    // If user granted folder access, add tree URI
    if let Some(tree_uri) = tree_uri_opt {
        // Persist write permission for the tree
        let _ = api.file_picker().persist_uri_permission(&tree_uri).await;

        let tree_uri_str = uri_to_string(&tree_uri);
        let tree_json: serde_json::Value = serde_json::from_str(&tree_uri_str)
            .map_err(|e| format!("Failed to parse tree URI JSON: {}", e))?;

        if let Some(tree_uri_value) = tree_json.get("uri") {
            json["documentTopTreeUri"] = tree_uri_value.clone();
            eprintln!("[SAF] save_wiki_file: folder access granted: {}", tree_uri_value);
        }
    } else {
        eprintln!("[SAF] save_wiki_file: folder access denied/skipped - attachments won't work");
        json["documentTopTreeUri"] = serde_json::Value::Null;
    }

    Ok(Some(json.to_string()))
}

/// Open the folder containing a wiki in the system file manager.
/// The path should be a JSON string with uri and optionally documentTopTreeUri.
pub fn reveal_in_file_manager(path_json: &str) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;

    eprintln!("[SAF] reveal_in_file_manager: path_json={}", path_json);

    // Parse the path JSON to get the folder URI
    let folder_uri = if path_json.trim().starts_with('{') {
        let json: serde_json::Value = serde_json::from_str(path_json)
            .map_err(|e| format!("Failed to parse path JSON: {}", e))?;

        // Prefer documentTopTreeUri (folder access) if available
        if let Some(tree_uri) = json.get("documentTopTreeUri").and_then(|v| v.as_str()) {
            if !tree_uri.is_empty() && tree_uri != "null" {
                tree_uri.to_string()
            } else if let Some(file_uri) = json.get("uri").and_then(|v| v.as_str()) {
                // Fall back to file URI - the file manager will show its parent
                file_uri.to_string()
            } else {
                return Err("No valid URI found in path JSON".to_string());
            }
        } else if let Some(file_uri) = json.get("uri").and_then(|v| v.as_str()) {
            file_uri.to_string()
        } else {
            return Err("No valid URI found in path JSON".to_string());
        }
    } else {
        // Plain URI string
        path_json.to_string()
    };

    // Tree URIs (content://.../tree/...) can't be opened with ACTION_VIEW.
    // Convert to document URIs (content://.../document/...) which file managers handle.
    let open_uri = if folder_uri.contains("/tree/") && !folder_uri.contains("/document/") {
        folder_uri.replacen("/tree/", "/document/", 1)
    } else {
        folder_uri
    };

    eprintln!("[SAF] reveal_in_file_manager: opening uri={}", open_uri);

    // Use the opener plugin to open the URI
    let app = get_app()?;
    app.opener()
        .open_url(&open_uri, None::<&str>)
        .map_err(|e| format!("Failed to open file manager: {:?}", e))
}

/// Query ContentResolver _data column for the full filesystem path.
/// Tries the original document URI first, then if the document ID is msf:N,
/// queries MediaStore directly. Returns a clean display path.
fn query_data_column(uri_str: &str) -> Result<String, String> {
    use crate::android::wiki_activity::get_java_vm;

    let vm = get_java_vm()?;
    let mut env = vm.attach_current_thread()
        .map_err(|e| format!("JNI attach failed: {}", e))?;

    // Get application context → ContentResolver
    let activity_thread_class = env.find_class("android/app/ActivityThread")
        .map_err(|e| format!("ActivityThread class: {}", e))?;
    let app_context = env.call_static_method(
        &activity_thread_class,
        "currentApplication",
        "()Landroid/app/Application;",
        &[],
    ).map_err(|e| format!("currentApplication: {}", e))?
     .l().map_err(|e| format!("Application result: {}", e))?;
    if app_context.is_null() {
        return Err("No application context".to_string());
    }
    let resolver = env.call_method(
        &app_context,
        "getContentResolver",
        "()Landroid/content/ContentResolver;",
        &[],
    ).map_err(|e| format!("getContentResolver: {}", e))?
     .l().map_err(|e| format!("ContentResolver: {}", e))?;

    // Extract MediaStore ID from the document URI
    let decoded = urlencoding::decode(uri_str).unwrap_or(uri_str.into());
    let media_id: Option<i64> = if let Some(doc_idx) = decoded.find("/document/") {
        let after_doc = &decoded[doc_idx + 10..];
        if after_doc.starts_with("msf:") {
            after_doc[4..].parse::<i64>().ok()
        } else if after_doc.starts_with("document:") {
            after_doc[9..].parse::<i64>().ok()
        } else if after_doc.starts_with("document%3A") || after_doc.starts_with("document%3a") {
            after_doc[11..].parse::<i64>().ok()
        } else if after_doc.chars().all(|c| c.is_ascii_digit()) && !after_doc.is_empty() {
            after_doc.parse::<i64>().ok()
        } else {
            None
        }
    } else {
        None
    };

    // Columns to try, in order of preference
    let columns_to_try: &[(&str, &str)] = &[
        ("_data", "_data"),
        ("relative_path", "relative_path"),
    ];

    // URIs to try for each column
    let mut uris_to_try = vec![uri_str.to_string()];
    if let Some(id) = media_id {
        uris_to_try.push(format!("content://media/external/file/{}", id));
    }

    let string_class = env.find_class("java/lang/String")
        .map_err(|e| format!("String class: {}", e))?;

    // For MediaStore URI, try to get relative_path + _display_name combined
    if let Some(id) = media_id {
        let ms_uri = format!("content://media/external/file/{}", id);
        eprintln!("[SAF] query_data_column: trying MediaStore URI {} with relative_path+_display_name", ms_uri);

        let j_uri_str = env.new_string(&ms_uri)
            .map_err(|e| format!("JNI string: {}", e))?;
        let uri_obj = env.call_static_method(
            "android/net/Uri",
            "parse",
            "(Ljava/lang/String;)Landroid/net/Uri;",
            &[(&j_uri_str).into()],
        ).map_err(|e| format!("Uri.parse: {}", e))?
         .l().map_err(|e| format!("Uri.parse result: {}", e))?;

        // Projection: [relative_path, _display_name, _data]
        let j_rp = env.new_string("relative_path").map_err(|e| format!("JNI: {}", e))?;
        let j_dn = env.new_string("_display_name").map_err(|e| format!("JNI: {}", e))?;
        let j_data = env.new_string("_data").map_err(|e| format!("JNI: {}", e))?;
        let projection = env.new_object_array(3, &string_class, &j_rp)
            .map_err(|e| format!("Array: {}", e))?;
        env.set_object_array_element(&projection, 1, &j_dn).map_err(|e| format!("Array set: {}", e))?;
        env.set_object_array_element(&projection, 2, &j_data).map_err(|e| format!("Array set: {}", e))?;

        let null_obj = jni::objects::JObject::null();
        let cursor_result = env.call_method(
            &resolver,
            "query",
            "(Landroid/net/Uri;[Ljava/lang/String;Ljava/lang/String;[Ljava/lang/String;Ljava/lang/String;)Landroid/database/Cursor;",
            &[(&uri_obj).into(), (&projection).into(), (&null_obj).into(), (&null_obj).into(), (&null_obj).into()],
        );

        if env.exception_check().unwrap_or(false) {
            let exc = env.exception_occurred().ok();
            if let Some(ref exc_obj) = exc {
                let msg = env.call_method(exc_obj, "getMessage", "()Ljava/lang/String;", &[])
                    .ok().and_then(|v| v.l().ok())
                    .and_then(|o| if o.is_null() { None } else { env.get_string((&o).into()).ok().map(|s| String::from(s)) });
                eprintln!("[SAF] query_data_column: exception on MediaStore: {:?}", msg);
            }
            env.exception_clear().ok();
        } else if let Ok(v) = cursor_result {
            if let Ok(cursor) = v.l() {
                if !cursor.is_null() {
                    let has_row = env.call_method(&cursor, "moveToFirst", "()Z", &[])
                        .ok().and_then(|v| v.z().ok()).unwrap_or(false);
                    if has_row {
                        // Try _data first (column index 2)
                        let data_val = env.call_method(&cursor, "getString", "(I)Ljava/lang/String;", &[jni::objects::JValue::Int(2)])
                            .ok().and_then(|v| v.l().ok())
                            .and_then(|o| if o.is_null() { None } else { env.get_string((&o).into()).ok().map(|s| String::from(s)) });
                        eprintln!("[SAF] query_data_column: _data={:?}", data_val);

                        if let Some(ref dp) = data_val {
                            if !dp.is_empty() {
                                let _ = env.call_method(&cursor, "close", "()V", &[]);
                                let display = dp
                                    .strip_prefix("/storage/emulated/0/")
                                    .or_else(|| dp.strip_prefix("/storage/self/primary/"))
                                    .or_else(|| dp.strip_prefix("/sdcard/"))
                                    .unwrap_or(dp);
                                return Ok(display.to_string());
                            }
                        }

                        // Try relative_path (column index 0) + _display_name (column index 1)
                        let rp_val = env.call_method(&cursor, "getString", "(I)Ljava/lang/String;", &[jni::objects::JValue::Int(0)])
                            .ok().and_then(|v| v.l().ok())
                            .and_then(|o| if o.is_null() { None } else { env.get_string((&o).into()).ok().map(|s| String::from(s)) });
                        let dn_val = env.call_method(&cursor, "getString", "(I)Ljava/lang/String;", &[jni::objects::JValue::Int(1)])
                            .ok().and_then(|v| v.l().ok())
                            .and_then(|o| if o.is_null() { None } else { env.get_string((&o).into()).ok().map(|s| String::from(s)) });
                        eprintln!("[SAF] query_data_column: relative_path={:?}, _display_name={:?}", rp_val, dn_val);

                        let _ = env.call_method(&cursor, "close", "()V", &[]);

                        if let Some(rp) = rp_val {
                            if !rp.is_empty() {
                                if let Some(dn) = dn_val {
                                    let combined = format!("{}{}", rp.trim_end_matches('/'), if dn.is_empty() { String::new() } else { format!("/{}", dn) });
                                    return Ok(combined);
                                }
                                return Ok(rp);
                            }
                        }
                    } else {
                        eprintln!("[SAF] query_data_column: MediaStore cursor has no rows");
                        let _ = env.call_method(&cursor, "close", "()V", &[]);
                    }
                } else {
                    eprintln!("[SAF] query_data_column: MediaStore cursor is null");
                }
            }
        }
    }

    // Fallback: try _data on original document URI
    {
        eprintln!("[SAF] query_data_column: fallback trying _data on original URI: {}", uri_str);
        let j_uri_str = env.new_string(uri_str)
            .map_err(|e| format!("JNI string: {}", e))?;
        let uri_obj = env.call_static_method(
            "android/net/Uri",
            "parse",
            "(Ljava/lang/String;)Landroid/net/Uri;",
            &[(&j_uri_str).into()],
        ).map_err(|e| format!("Uri.parse: {}", e))?
         .l().map_err(|e| format!("Uri.parse result: {}", e))?;

        let j_data = env.new_string("_data")
            .map_err(|e| format!("JNI string: {}", e))?;
        let projection = env.new_object_array(1, &string_class, &j_data)
            .map_err(|e| format!("Array: {}", e))?;

        let null_obj = jni::objects::JObject::null();
        let cursor_result = env.call_method(
            &resolver,
            "query",
            "(Landroid/net/Uri;[Ljava/lang/String;Ljava/lang/String;[Ljava/lang/String;Ljava/lang/String;)Landroid/database/Cursor;",
            &[(&uri_obj).into(), (&projection).into(), (&null_obj).into(), (&null_obj).into(), (&null_obj).into()],
        );

        if !env.exception_check().unwrap_or(false) {
            if let Ok(v) = cursor_result {
                if let Ok(cursor) = v.l() {
                    if !cursor.is_null() {
                        let has_row = env.call_method(&cursor, "moveToFirst", "()Z", &[])
                            .ok().and_then(|v| v.z().ok()).unwrap_or(false);
                        if has_row {
                            let data_obj = env.call_method(&cursor, "getString", "(I)Ljava/lang/String;", &[jni::objects::JValue::Int(0)])
                                .ok().and_then(|v| v.l().ok());
                            let _ = env.call_method(&cursor, "close", "()V", &[]);
                            if let Some(ref o) = data_obj {
                                if !o.is_null() {
                                    if let Ok(s) = env.get_string(o.into()) {
                                        let data_path = String::from(s);
                                        if !data_path.is_empty() {
                                            let display = data_path
                                                .strip_prefix("/storage/emulated/0/")
                                                .or_else(|| data_path.strip_prefix("/storage/self/primary/"))
                                                .or_else(|| data_path.strip_prefix("/sdcard/"))
                                                .unwrap_or(&data_path);
                                            return Ok(display.to_string());
                                        }
                                    }
                                }
                            }
                        } else {
                            let _ = env.call_method(&cursor, "close", "()V", &[]);
                        }
                    }
                }
            }
        } else {
            env.exception_clear().ok();
        }
    }

    Err("_data not available from any URI".to_string())
}

pub fn get_display_name(uri: &str) -> Result<String, String> {
    let app = get_app()?;
    let api = app.android_fs();
    let file_uri = parse_uri(uri)?;

    api.get_name(&file_uri)
        .map_err(|e| format!("Failed to get file name: {:?}", e))
}

/// Extract a human-readable path from a SAF content:// URI.
///
/// SAF URIs often contain the document path encoded in them:
/// - `content://com.android.externalstorage.documents/document/primary:Documents%2FMyWiki.html`
///   -> "Documents/MyWiki.html"
/// - `content://com.android.providers.downloads.documents/document/12345`
///   -> "Downloads" (fallback)
///
/// Returns a user-friendly path string for display purposes.
pub fn get_display_path(uri_json: &str) -> String {
    // Try to parse as JSON first
    let (uri_str, tree_uri) = if uri_json.starts_with('{') {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(uri_json) {
            let uri = parsed.get("uri")
                .and_then(|v| v.as_str())
                .unwrap_or(uri_json)
                .to_string();
            let tree = parsed.get("documentTopTreeUri")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            (uri, tree)
        } else {
            (uri_json.to_string(), None)
        }
    } else {
        (uri_json.to_string(), None)
    };

    eprintln!("[SAF] get_display_path: uri_str={}", uri_str);

    // Try to extract path from common SAF URI patterns
    if let Some(path) = extract_path_from_saf_uri(&uri_str) {
        eprintln!("[SAF] get_display_path: from URI pattern: {}", path);
        return path;
    }

    // If main URI is opaque but we have a tree URI, combine tree path + filename
    if let Some(ref tree) = tree_uri {
        if let Some(tree_path) = extract_path_from_saf_uri(tree) {
            // Get the filename from the document provider
            if let Ok(name) = get_display_name(uri_json) {
                let combined = format!("{}/{}", tree_path.trim_end_matches('/'), name);
                eprintln!("[SAF] get_display_path: from tree+name: {}", combined);
                return combined;
            }
        }
    }

    // Fallback: query ContentResolver for _data (full filesystem path)
    match query_data_column(&uri_str) {
        Ok(path) => {
            eprintln!("[SAF] get_display_path: from _data query: {}", path);
            return path;
        }
        Err(e) => {
            eprintln!("[SAF] get_display_path: _data query failed: {}", e);
        }
    }

    // Fallback: try to get display name only
    if let Ok(name) = get_display_name(uri_json) {
        return name;
    }

    // Last resort: show a shortened URI
    if uri_str.len() > 50 {
        format!("...{}", &uri_str[uri_str.len() - 40..])
    } else {
        uri_str
    }
}

/// Extract a readable path from SAF URI patterns.
fn extract_path_from_saf_uri(uri: &str) -> Option<String> {
    // Common pattern: content://authority/document/storage:path or content://authority/tree/storage:path/document/...

    // URL decode the URI first
    let decoded = urlencoding::decode(uri).ok()?.into_owned();

    // Pattern 1: .../document/primary:path or .../document/home:path
    // Skips opaque numeric IDs (e.g. msf:12345 from recents picker)
    if let Some(doc_idx) = decoded.find("/document/") {
        let after_doc = &decoded[doc_idx + 10..]; // Skip "/document/"
        if let Some(colon_idx) = after_doc.find(':') {
            let path = &after_doc[colon_idx + 1..];
            // Clean up any remaining encoded characters and normalize slashes
            let clean_path = path.replace("%2F", "/").replace("%20", " ");
            if !clean_path.is_empty() && !clean_path.chars().all(|c| c.is_ascii_digit()) {
                return Some(clean_path);
            }
        }
    }

    // Pattern 2: .../tree/primary:path (for folder wikis)
    if let Some(tree_idx) = decoded.find("/tree/") {
        let after_tree = &decoded[tree_idx + 6..]; // Skip "/tree/"
        // Find the end of the tree path (before /document/ if present)
        let tree_path = if let Some(doc_idx) = after_tree.find("/document/") {
            &after_tree[..doc_idx]
        } else {
            after_tree
        };
        if let Some(colon_idx) = tree_path.find(':') {
            let path = &tree_path[colon_idx + 1..];
            let clean_path = path.replace("%2F", "/").replace("%20", " ");
            if !clean_path.is_empty() {
                return Some(clean_path);
            }
        }
    }

    // Pattern 3: Downloads provider - show "Downloads"
    if decoded.contains("downloads.documents") {
        return Some("Downloads".to_string());
    }

    // Pattern 4: Media provider
    if decoded.contains("media/") {
        if decoded.contains("/images/") {
            return Some("Pictures".to_string());
        } else if decoded.contains("/video/") {
            return Some("Videos".to_string());
        } else if decoded.contains("/audio/") {
            return Some("Music".to_string());
        }
    }

    None
}

/// Get all persisted URI permissions as JSON-serialized FileUris.
/// Note: This queries the API for available permissions.
pub fn get_persisted_permissions() -> Result<Vec<String>, String> {
    // The plugin doesn't provide a direct way to list persisted permissions
    // This would need to be tracked by our app separately
    Ok(Vec::new())
}

/// Copy a document to a new location.
/// Creates a new file in the destination directory and copies content.
pub fn copy_document(source_uri: &str, dest_dir_uri: &str, dest_name: &str) -> Result<String, String> {
    // Read source content
    let content = read_document_bytes(source_uri)?;

    // Create destination file
    let new_uri = create_file(dest_dir_uri, dest_name, Some("text/html"))?;

    // Write content to new file
    write_document_bytes(&new_uri, &content)?;

    Ok(new_uri)
}

/// Create a backup of a wiki file.
/// Returns the URI of the backup file.
pub fn create_backup(wiki_uri: &str, backup_dir_uri: &str, filename_stem: &str) -> Result<String, String> {
    // Generate timestamped backup filename
    let timestamp = Local::now().format("%Y%m%d-%H%M%S");
    let backup_name = format!("{}.{}.html", filename_stem, timestamp);

    eprintln!("[SAF] create_backup: wiki_uri={}", wiki_uri);
    eprintln!("[SAF] create_backup: backup_dir_uri={}", backup_dir_uri);
    eprintln!("[SAF] create_backup: backup_name={}", backup_name);

    // Copy the wiki file to the backup location
    match copy_document(wiki_uri, backup_dir_uri, &backup_name) {
        Ok(uri) => {
            eprintln!("[SAF] create_backup: success, backup_uri={}", uri);
            Ok(uri)
        }
        Err(e) => {
            eprintln!("[SAF] create_backup: FAILED - {}", e);
            Err(e)
        }
    }
}

/// Clean up old backups in a directory, keeping only the most recent ones.
pub fn cleanup_old_backups(backup_dir_uri: &str, filename_stem: &str, keep: usize) -> Result<(), String> {
    if keep == 0 {
        return Ok(()); // Keep unlimited
    }

    let entries = list_directory_entries(backup_dir_uri)?;

    // Filter to only backup files matching this wiki's pattern
    let prefix = format!("{}.", filename_stem);
    let mut backups: Vec<DirEntry> = entries
        .into_iter()
        .filter(|e| !e.is_dir && e.name.starts_with(&prefix) && e.name.ends_with(".html"))
        .collect();

    // Sort by name (which includes timestamp, so alphabetical = chronological)
    backups.sort_by(|a, b| b.name.cmp(&a.name)); // Reverse order (newest first)

    // Delete old backups beyond the keep limit
    for old_backup in backups.into_iter().skip(keep) {
        let _ = delete_document(&old_backup.uri);
    }

    Ok(())
}

/// Get the backup directory for a wiki.
/// If custom_backup_dir is set, returns that.
/// Otherwise, if we have tree access (documentTopTreeUri set), creates a .backups folder
/// in the same directory as the wiki file.
///
/// The default backup location is a `.backups` folder next to the wiki file.
pub fn get_backup_directory(wiki_uri: &str, custom_backup_dir: Option<&str>) -> Result<Option<String>, String> {
    // If custom backup dir is set, use that
    if let Some(custom_dir) = custom_backup_dir {
        return Ok(Some(custom_dir.to_string()));
    }

    // Check if we have tree access by looking at the JSON structure
    // If the URI has documentTopTreeUri set, we can create/access the .backups folder
    if wiki_uri.starts_with('{') {
        if let Ok(uri_json) = serde_json::from_str::<serde_json::Value>(wiki_uri) {
            if let Some(tree_uri) = uri_json.get("documentTopTreeUri").and_then(|v| v.as_str()) {
                if !tree_uri.is_empty() {
                    // We have tree access - the tree URI is the parent directory
                    // Get the wiki's filename to determine backup folder name
                    if let Ok(filename) = get_display_name(wiki_uri) {
                        let stem = filename.strip_suffix(".html")
                            .or_else(|| filename.strip_suffix(".htm"))
                            .unwrap_or(&filename);

                        let backup_dir_name = format!("{}.backups", stem);

                        // The tree URI is the parent directory - create/find .backups folder there
                        let tree_json = format!(r#"{{"uri":"{}","documentTopTreeUri":"{}"}}"#, tree_uri, tree_uri);

                        // Try to find or create the backup directory
                        match find_or_create_subdirectory(&tree_json, &backup_dir_name) {
                            Ok(backup_uri) => {
                                eprintln!("[SAF] Using backup directory: {}", backup_dir_name);
                                return Ok(Some(backup_uri));
                            }
                            Err(e) => {
                                eprintln!("[SAF] Failed to create backup directory {}: {}", backup_dir_name, e);
                                // Fall through to return None
                            }
                        }
                    }
                }
            }
        }
    }

    // No tree access - can't create backup directory automatically
    // This happens when user picked the wiki file directly without folder access
    // They need to set a custom backup directory in wiki settings, or re-open the wiki
    // via the folder picker to grant tree access
    eprintln!("[SAF] No tree access for wiki - backups disabled. Re-open wiki via folder picker for automatic backups.");
    Ok(None)
}

/// Pick a backup directory via SAF directory picker.
/// Uses pick_directory_with_write() to ensure write access is available.
pub async fn pick_backup_directory() -> Result<Option<String>, String> {
    pick_directory_with_write().await
}

/// Copy an attachment file to the wiki's attachments folder.
/// Returns the relative path to use as _canonical_uri (e.g., "./attachments/image.png").
///
/// This is used on Android where SAF content:// URIs can't be stored as _canonical_uri
/// because permissions expire. Instead, we copy to a local attachments folder.
pub fn copy_attachment_to_wiki(wiki_uri: &str, source_uri: &str, filename: &str) -> Result<String, String> {
    eprintln!("[SAF] copy_attachment_to_wiki: wiki_uri={}", wiki_uri);
    eprintln!("[SAF] copy_attachment_to_wiki: source_uri={}", source_uri);
    eprintln!("[SAF] copy_attachment_to_wiki: filename={}", filename);

    // Get the documentTopTreeUri from the wiki URI - this is the parent directory
    let tree_uri = if wiki_uri.starts_with('{') {
        if let Ok(uri_json) = serde_json::from_str::<serde_json::Value>(wiki_uri) {
            uri_json.get("documentTopTreeUri")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        } else {
            None
        }
    } else {
        None
    };

    let tree_uri = tree_uri.ok_or_else(|| {
        "No tree access for wiki - cannot create attachments folder. Re-open wiki via folder picker.".to_string()
    })?;

    // Create a FileUri for the parent directory
    let tree_json = format!(r#"{{"uri":"{}","documentTopTreeUri":"{}"}}"#, tree_uri, tree_uri);

    // Create or find the "attachments" folder
    let attachments_dir = find_or_create_subdirectory(&tree_json, "attachments")?;
    eprintln!("[SAF] copy_attachment_to_wiki: attachments_dir={}", attachments_dir);

    // Read source content
    let source_content = read_document_bytes(source_uri)?;
    eprintln!("[SAF] copy_attachment_to_wiki: read {} bytes from source", source_content.len());

    // Check if a file with the same name already exists
    let entries = list_directory_entries(&attachments_dir).unwrap_or_default();

    // Find a unique filename
    let final_name = find_unique_filename(&entries, filename, &source_content);
    eprintln!("[SAF] copy_attachment_to_wiki: final_name={}", final_name);

    // If the file already exists with same content, just return the path
    if final_name == filename {
        for entry in &entries {
            if entry.name == filename {
                // File exists - check if content is the same
                if let Ok(existing_content) = read_document_bytes(&entry.uri) {
                    if existing_content == source_content {
                        eprintln!("[SAF] copy_attachment_to_wiki: identical file already exists, skipping copy");
                        return Ok(format!("./attachments/{}", filename));
                    }
                }
                break;
            }
        }
    }

    // Guess MIME type from filename
    let mime_type = guess_mime_type(&final_name);

    // Create the new file
    let new_file_uri = create_file(&attachments_dir, &final_name, Some(mime_type))?;
    eprintln!("[SAF] copy_attachment_to_wiki: created file {}", new_file_uri);

    // Write content
    write_document_bytes(&new_file_uri, &source_content)?;
    eprintln!("[SAF] copy_attachment_to_wiki: wrote content successfully");

    Ok(format!("./attachments/{}", final_name))
}

/// Find a unique filename in a directory.
/// If a file with the same name and content exists, returns that name.
/// If a file with the same name but different content exists, returns name-N.ext.
fn find_unique_filename(entries: &[DirEntry], filename: &str, content: &[u8]) -> String {
    // Check if file already exists
    let existing = entries.iter().find(|e| e.name == filename);

    if existing.is_none() {
        return filename.to_string();
    }

    // File exists - check if content is identical
    if let Some(entry) = existing {
        if let Ok(existing_content) = read_document_bytes(&entry.uri) {
            if existing_content == *content {
                // Identical file already exists
                return filename.to_string();
            }
        }
    }

    // Different content - need a unique name
    let (stem, ext) = if let Some(dot_pos) = filename.rfind('.') {
        (&filename[..dot_pos], &filename[dot_pos..])
    } else {
        (filename, "")
    };

    for n in 1..1000 {
        let candidate = format!("{}-{}{}", stem, n, ext);
        if !entries.iter().any(|e| e.name == candidate) {
            return candidate;
        }
    }

    // Fallback with timestamp
    let timestamp = Local::now().format("%Y%m%d%H%M%S");
    format!("{}-{}{}", stem, timestamp, ext)
}

/// Save attachment content directly to the wiki's attachments folder.
/// Used when we have file content (e.g., from file picker) instead of a source URI.
/// Returns the relative path to use as _canonical_uri (e.g., "./attachments/image.png").
pub fn save_attachment_content(wiki_uri: &str, content: &[u8], filename: &str) -> Result<String, String> {
    eprintln!("[SAF] save_attachment_content: wiki_uri={}", wiki_uri);
    eprintln!("[SAF] save_attachment_content: filename={}, content_len={}", filename, content.len());

    // Get the documentTopTreeUri from the wiki URI - this is the parent directory
    let tree_uri = if wiki_uri.starts_with('{') {
        if let Ok(uri_json) = serde_json::from_str::<serde_json::Value>(wiki_uri) {
            uri_json.get("documentTopTreeUri")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        } else {
            None
        }
    } else {
        None
    };

    let tree_uri = tree_uri.ok_or_else(|| {
        "No tree access for wiki - cannot create attachments folder. Re-open wiki via folder picker.".to_string()
    })?;

    // Create a FileUri for the parent directory
    let tree_json = format!(r#"{{"uri":"{}","documentTopTreeUri":"{}"}}"#, tree_uri, tree_uri);

    // Create or find the "attachments" folder
    let attachments_dir = find_or_create_subdirectory(&tree_json, "attachments")?;
    eprintln!("[SAF] save_attachment_content: attachments_dir={}", attachments_dir);

    // Check if a file with the same name already exists
    let entries = list_directory_entries(&attachments_dir).unwrap_or_default();

    // Find a unique filename
    let final_name = find_unique_filename(&entries, filename, content);
    eprintln!("[SAF] save_attachment_content: final_name={}", final_name);

    // If the file already exists with same content, just return the path
    if final_name == filename {
        for entry in &entries {
            if entry.name == filename {
                // File exists - check if content is the same
                if let Ok(existing_content) = read_document_bytes(&entry.uri) {
                    if existing_content == content {
                        eprintln!("[SAF] save_attachment_content: identical file already exists, skipping save");
                        return Ok(format!("./attachments/{}", filename));
                    }
                }
                break;
            }
        }
    }

    // Guess MIME type from filename
    let mime_type = guess_mime_type(&final_name);

    // Create the new file
    let new_file_uri = create_file(&attachments_dir, &final_name, Some(mime_type))?;
    eprintln!("[SAF] save_attachment_content: created file {}", new_file_uri);

    // Write content
    write_document_bytes(&new_file_uri, content)?;
    eprintln!("[SAF] save_attachment_content: wrote content successfully");

    Ok(format!("./attachments/{}", final_name))
}

/// Guess MIME type from filename extension.
fn guess_mime_type(filename: &str) -> &'static str {
    let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "pdf" => "application/pdf",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "txt" => "text/plain",
        "json" => "application/json",
        "xml" => "application/xml",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" => "application/javascript",
        _ => "application/octet-stream",
    }
}
