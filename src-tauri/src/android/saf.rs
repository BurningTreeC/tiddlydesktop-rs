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
    let app = match get_app() {
        Ok(app) => app,
        Err(_) => return false,
    };
    let api = app.android_fs();
    let file_uri = match parse_uri(uri) {
        Ok(uri) => uri,
        Err(_) => return false,
    };

    // Check MIME type - directories have vnd.android.document/directory
    match api.get_mime_type(&file_uri) {
        Ok(mime) => mime == "vnd.android.document/directory",
        Err(_) => false,
    }
}

/// Directory entry with name and URI.
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub uri: String,
    pub is_dir: bool,
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
            tauri_plugin_android_fs::Entry::File { name, uri, .. } => DirEntry {
                name,
                uri: uri_to_string(&uri),
                is_dir: false,
            },
            tauri_plugin_android_fs::Entry::Dir { name, uri, .. } => DirEntry {
                name,
                uri: uri_to_string(&uri),
                is_dir: true,
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
/// For SAF URIs, this tries to extract the tree URI from the FileUri.
/// Returns None if the parent cannot be determined.
pub fn get_parent_uri(uri: &str) -> Result<String, String> {
    // Try to parse as JSON FileUri to get the documentTopTreeUri
    if uri.trim().starts_with('{') {
        if let Ok(file_uri) = FileUri::from_json_str(uri) {
            // Get the tree URI (parent directory) from the FileUri
            // The documentTopTreeUri field contains the tree root for tree URIs
            let json_str = file_uri.to_json_string()
                .map_err(|e| format!("Failed to serialize FileUri: {:?}", e))?;

            // Parse the JSON to extract documentTopTreeUri
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&json_str) {
                if let Some(tree_uri) = json.get("documentTopTreeUri").and_then(|v| v.as_str()) {
                    // Return the tree URI as a FileUri JSON
                    let parent_json = format!(r#"{{"uri":"{}","documentTopTreeUri":null}}"#, tree_uri);
                    return Ok(parent_json);
                }
            }
        }
    }

    // For simple content:// URIs, try to extract parent from the path
    // content://authority/tree/treeId/document/documentPath
    // The parent would be the tree URI without the document part
    if uri.starts_with("content://") {
        // This is a simplified heuristic - proper SAF parent navigation requires the API
        if let Some(doc_idx) = uri.find("/document/") {
            let tree_part = &uri[..doc_idx];
            // Return just the tree URI
            let parent_json = format!(r#"{{"uri":"{}","documentTopTreeUri":null}}"#, tree_part);
            return Ok(parent_json);
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

/// Open a file picker for HTML files and return the selected file as JSON-serialized FileUri.
pub async fn pick_wiki_file() -> Result<Option<String>, String> {
    let app = get_app()?;
    let api = app.android_fs_async();

    // Show file picker for HTML files
    let files = api.file_picker().pick_files(
        None,                           // Initial location
        &["text/html", "text/plain"],   // MIME types for wiki files
        false,                          // local_only
    ).await.map_err(|e| format!("File picker failed: {:?}", e))?;

    if files.is_empty() {
        Ok(None)
    } else {
        let uri = &files[0];
        // Persist permission for future access
        let _ = api.file_picker().persist_uri_permission(uri).await;
        Ok(Some(uri_to_string(uri)))
    }
}

/// Open a save dialog to create a new wiki file.
/// Returns the JSON-serialized FileUri of the new file.
pub async fn save_wiki_file(suggested_name: &str) -> Result<Option<String>, String> {
    let app = get_app()?;
    let api = app.android_fs_async();

    match api.file_picker().save_file(
        None,                    // Initial location
        suggested_name,          // Suggested file name
        Some("text/html"),       // MIME type
        false,                   // local_only
    ).await {
        Ok(Some(uri)) => {
            // Persist permission for future access
            let _ = api.file_picker().persist_uri_permission(&uri).await;
            Ok(Some(uri_to_string(&uri)))
        }
        Ok(None) => Ok(None),
        Err(e) => Err(format!("Save dialog failed: {:?}", e)),
    }
}

/// Get the display name of a file from its URI.
pub fn get_display_name(uri: &str) -> Result<String, String> {
    let app = get_app()?;
    let api = app.android_fs();
    let file_uri = parse_uri(uri)?;

    api.get_name(&file_uri)
        .map_err(|e| format!("Failed to get file name: {:?}", e))
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

    // Copy the wiki file to the backup location
    copy_document(wiki_uri, backup_dir_uri, &backup_name)
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
pub async fn pick_backup_directory() -> Result<Option<String>, String> {
    pick_directory().await
}
