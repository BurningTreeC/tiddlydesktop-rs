//! Android-specific implementations for TiddlyDesktop.
//!
//! This module is only compiled on Android builds (`target_os = "android"`).
//! It provides:
//!
//! - **SAF (Storage Access Framework)** - Read/write files in user-selected directories
//! - **Persistent permissions** - Maintain access across app restarts
//! - **Node.js Bridge** - Spawns Node.js for TiddlyWiki operations (rendering, conversion)
//! - **Folder Wiki Server** - HTTP server with direct SAF access for folder wikis
//!
//! ## Architecture
//!
//! On Android, users cannot grant blanket filesystem access. Instead:
//! 1. User picks a directory via SAF file picker
//! 2. App receives a `content://` URI with read/write permissions
//! 3. App persists the permission with `takePersistableUriPermission()`
//! 4. On subsequent launches, app can access that directory without re-prompting
//!
//! ## TiddlyWiki Folder Wiki Execution
//!
//! For folder wikis with SAF URIs, we use a two-phase approach:
//!
//! 1. **Initial render**: Node.js renders the wiki to HTML (temp copy, then cleaned up)
//! 2. **Runtime serving**: `FolderWikiServer` serves the HTML and handles TiddlyWeb
//!    protocol for tiddler CRUD, accessing SAF directly without keeping a local copy
//!
//! This approach provides:
//! - Direct SAF access (no sync needed, changes written immediately)
//! - Reduced storage usage (no persistent local copy)
//! - Better reliability (no sync race conditions)
//!
//! ## Wiki Conversion
//!
//! Both conversion directions are supported:
//! - **File to Folder**: Uses `tiddlywiki --load <file> --savewikifolder <folder>`
//! - **Folder to File**: Uses `tiddlywiki <folder> --render '$:/core/save/all'`
//!
//! ## Backups
//!
//! Automatic backups are supported when the wiki is opened with tree access:
//! - When user opens a wiki via folder picker (grants tree access), backups go to
//!   a `.backups` folder in the same directory as the wiki file
//! - When user picks the wiki file directly (no tree access), backups require
//!   a custom backup directory set in wiki settings
//!
//! ## Usage
//!
//! The `fs_abstraction` module automatically routes `content://` URIs to this module.
//! Desktop code paths remain unchanged.

#![cfg(target_os = "android")]
#![allow(dead_code)]
#![allow(unused_imports)]

pub mod saf;
pub mod folder_wiki_server;
pub mod node_bridge;
pub mod wiki_activity;

// Re-export commonly used items (for future integration)
pub use saf::{
    read_document_string,
    read_document_bytes,
    write_document_string,
    write_document_bytes,
    document_exists,
    is_directory,
    list_directory,
    delete_document,
};

/// Initialize Android-specific functionality.
/// Called during app startup on Android builds.
pub fn init() -> Result<(), String> {
    eprintln!("[TiddlyDesktop] Android module initialized");
    Ok(())
}
