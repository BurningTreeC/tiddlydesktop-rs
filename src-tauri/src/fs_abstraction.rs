//! File system abstraction layer for cross-platform wiki file operations.
//!
//! This module provides a minimal abstraction that allows:
//! - Desktop (Linux/Windows/macOS): Direct filesystem access via std::fs
//! - Android: Storage Access Framework (SAF) via tauri-plugin-android-fs
//!
//! The abstraction is intentionally minimal - only the operations needed
//! for TiddlyDesktop wiki file handling are included.

// Allow dead code since many functions are prepared for future integration
#![allow(dead_code)]

use std::path::Path;

// ============================================================================
// Desktop Implementation (Linux, Windows, macOS)
// ============================================================================

/// Read a wiki HTML file to string.
///
/// On desktop: Uses std::fs::read_to_string
/// On Android: Uses SAF to read from content:// URI
#[cfg(not(target_os = "android"))]
pub fn read_wiki_file(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read wiki file: {}", e))
}

/// Write content to a wiki HTML file.
///
/// On desktop: Uses atomic write (temp file + rename) with fallback
/// On Android: Uses SAF to write to content:// URI
#[cfg(not(target_os = "android"))]
pub fn write_wiki_file(path: &Path, content: &str) -> Result<(), String> {
    // Try atomic write: write to temp, then rename
    let temp_path = path.with_extension("tmp");

    if let Err(e) = std::fs::write(&temp_path, content) {
        return Err(format!("Failed to write temp file: {}", e));
    }

    match std::fs::rename(&temp_path, path) {
        Ok(_) => Ok(()),
        Err(_rename_err) => {
            // Rename can fail on Windows if file is locked
            // Fall back to direct write
            let _ = std::fs::remove_file(&temp_path);
            std::fs::write(path, content)
                .map_err(|e| format!("Failed to write wiki file: {}", e))
        }
    }
}

/// Read a bundled asset file (for tdasset:// protocol).
///
/// On desktop: Uses std::fs::read
/// On Android: Uses std::fs::read (assets are in app bundle)
#[cfg(not(target_os = "android"))]
pub fn read_asset_file(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path)
        .map_err(|e| format!("Failed to read asset: {}", e))
}

/// Read any file as bytes (for relative path resolution in wikis).
///
/// On desktop: Uses std::fs::read
/// On Android: Uses SAF if content:// URI, otherwise std::fs
#[cfg(not(target_os = "android"))]
pub fn read_file(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path)
        .map_err(|e| format!("Failed to read file: {}", e))
}

/// Check if a path exists.
#[cfg(not(target_os = "android"))]
pub fn exists(path: &Path) -> bool {
    path.exists()
}

/// Check if a path is a directory.
#[cfg(not(target_os = "android"))]
pub fn is_directory(path: &Path) -> bool {
    path.is_dir()
}

/// List directory contents.
#[cfg(not(target_os = "android"))]
pub fn list_directory(path: &Path) -> Result<Vec<String>, String> {
    std::fs::read_dir(path)
        .map_err(|e| format!("Failed to read directory: {}", e))?
        .filter_map(|entry| {
            entry.ok().map(|e| e.file_name().to_string_lossy().to_string())
        })
        .collect::<Vec<_>>()
        .pipe(Ok)
}

/// Create directory and all parent directories.
#[cfg(not(target_os = "android"))]
pub fn create_dir_all(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path)
        .map_err(|e| format!("Failed to create directory: {}", e))
}

/// Copy a file.
#[cfg(not(target_os = "android"))]
pub fn copy_file(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::copy(from, to)
        .map(|_| ())
        .map_err(|e| format!("Failed to copy file: {}", e))
}

/// Remove a file.
#[cfg(not(target_os = "android"))]
pub fn remove_file(path: &Path) -> Result<(), String> {
    std::fs::remove_file(path)
        .map_err(|e| format!("Failed to remove file: {}", e))
}

// ============================================================================
// Android Implementation
// ============================================================================

#[cfg(target_os = "android")]
pub fn read_wiki_file(path: &Path) -> Result<String, String> {
    // On Android, path might be a content:// URI stored as a path string
    // or a JSON-serialized FileUri starting with "{"
    let path_str = path.to_string_lossy();

    if path_str.starts_with("content://") || path_str.starts_with('{') {
        // SAF URI - use Android FS plugin
        crate::android::saf::read_document_string(&path_str)
    } else {
        // Regular path (e.g., app-internal storage)
        std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read wiki file: {}", e))
    }
}

#[cfg(target_os = "android")]
pub fn write_wiki_file(path: &Path, content: &str) -> Result<(), String> {
    let path_str = path.to_string_lossy();

    if path_str.starts_with("content://") || path_str.starts_with('{') {
        // SAF URI - use Android FS plugin
        crate::android::saf::write_document_string(&path_str, content)
    } else {
        // Regular path (e.g., app-internal storage)
        std::fs::write(path, content)
            .map_err(|e| format!("Failed to write wiki file: {}", e))
    }
}

#[cfg(target_os = "android")]
pub fn read_asset_file(path: &Path) -> Result<Vec<u8>, String> {
    // Assets are always in app bundle, use regular fs
    std::fs::read(path)
        .map_err(|e| format!("Failed to read asset: {}", e))
}

#[cfg(target_os = "android")]
pub fn read_file(path: &Path) -> Result<Vec<u8>, String> {
    let path_str = path.to_string_lossy();

    if path_str.starts_with("content://") || path_str.starts_with('{') {
        crate::android::saf::read_document_bytes(&path_str)
    } else {
        std::fs::read(path)
            .map_err(|e| format!("Failed to read file: {}", e))
    }
}

#[cfg(target_os = "android")]
pub fn exists(path: &Path) -> bool {
    let path_str = path.to_string_lossy();

    if path_str.starts_with("content://") || path_str.starts_with('{') {
        crate::android::saf::document_exists(&path_str)
    } else {
        path.exists()
    }
}

#[cfg(target_os = "android")]
pub fn is_directory(path: &Path) -> bool {
    let path_str = path.to_string_lossy();

    if path_str.starts_with("content://") || path_str.starts_with('{') {
        crate::android::saf::is_directory(&path_str)
    } else {
        path.is_dir()
    }
}

#[cfg(target_os = "android")]
pub fn list_directory(path: &Path) -> Result<Vec<String>, String> {
    let path_str = path.to_string_lossy();

    if path_str.starts_with("content://") || path_str.starts_with('{') {
        crate::android::saf::list_directory(&path_str)
    } else {
        std::fs::read_dir(path)
            .map_err(|e| format!("Failed to read directory: {}", e))?
            .filter_map(|entry| {
                entry.ok().map(|e| e.file_name().to_string_lossy().to_string())
            })
            .collect::<Vec<_>>()
            .pipe(Ok)
    }
}

#[cfg(target_os = "android")]
pub fn create_dir_all(path: &Path) -> Result<(), String> {
    let path_str = path.to_string_lossy();

    if path_str.starts_with("content://") || path_str.starts_with('{') {
        // SAF directories are created implicitly when creating files
        Ok(())
    } else {
        std::fs::create_dir_all(path)
            .map_err(|e| format!("Failed to create directory: {}", e))
    }
}

#[cfg(target_os = "android")]
pub fn copy_file(from: &Path, to: &Path) -> Result<(), String> {
    // Read from source, write to destination
    let content = read_file(from)?;
    let to_str = to.to_string_lossy();

    if to_str.starts_with("content://") || to_str.starts_with('{') {
        crate::android::saf::write_document_bytes(&to_str, &content)
    } else {
        std::fs::write(to, &content)
            .map_err(|e| format!("Failed to copy file: {}", e))
    }
}

#[cfg(target_os = "android")]
pub fn remove_file(path: &Path) -> Result<(), String> {
    let path_str = path.to_string_lossy();

    if path_str.starts_with("content://") || path_str.starts_with('{') {
        crate::android::saf::delete_document(&path_str)
    } else {
        std::fs::remove_file(path)
            .map_err(|e| format!("Failed to remove file: {}", e))
    }
}

// ============================================================================
// Utility trait for pipe syntax
// ============================================================================

trait Pipe: Sized {
    fn pipe<F, R>(self, f: F) -> R
    where
        F: FnOnce(Self) -> R,
    {
        f(self)
    }
}

impl<T> Pipe for T {}
