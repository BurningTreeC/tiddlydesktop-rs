//! Core data types for TiddlyDesktop
//!
//! This module contains the fundamental data structures used throughout the application:
//! - Wiki entry representation
//! - Configuration structures for external attachments and authentication
//! - Internal wiki configuration storage

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A wiki entry in the recent files list
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WikiEntry {
    pub path: String,
    pub filename: String,
    #[serde(default)]
    pub favicon: Option<String>, // Data URI for favicon
    #[serde(default)]
    pub is_folder: bool, // true if this is a wiki folder
    #[serde(default = "default_backups_enabled")]
    pub backups_enabled: bool, // whether to create backups on save (single-file only)
    #[serde(default)]
    pub backup_dir: Option<String>, // custom backup directory (if None, uses .backups folder next to wiki)
    #[serde(default)]
    pub group: Option<String>, // group name for organizing wikis (None = "Ungrouped")
}

fn default_backups_enabled() -> bool {
    true
}

/// Configuration for external attachments per wiki
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExternalAttachmentsConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub use_absolute_for_descendents: bool,
    #[serde(default)]
    pub use_absolute_for_non_descendents: bool,
}

impl Default for ExternalAttachmentsConfig {
    fn default() -> Self {
        Self {
            enabled: true, // Enable by default
            use_absolute_for_descendents: false,
            use_absolute_for_non_descendents: false,
        }
    }
}

fn default_true() -> bool {
    true
}

/// A single authentication URL entry
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthUrlEntry {
    pub name: String,
    pub url: String,
}

/// Configuration for session authentication URLs per wiki
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SessionAuthConfig {
    #[serde(default)]
    pub auth_urls: Vec<AuthUrlEntry>,
}

/// Window state (size, position, monitor) for a wiki
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WindowState {
    pub width: u32,
    pub height: u32,
    pub x: i32,
    pub y: i32,
    /// Monitor name (may not be unique if multiple identical monitors)
    #[serde(default)]
    pub monitor_name: Option<String>,
    /// Monitor position for unique identification (top-left corner)
    #[serde(default)]
    pub monitor_x: i32,
    #[serde(default)]
    pub monitor_y: i32,
    /// Whether the window was maximized
    #[serde(default)]
    pub maximized: bool,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            width: 1200,
            height: 800,
            x: 100,
            y: 100,
            monitor_name: None,
            monitor_x: 0,
            monitor_y: 0,
            maximized: false,
        }
    }
}

/// All wiki configs stored in a single file, keyed by wiki path
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct WikiConfigs {
    #[serde(default)]
    pub external_attachments: HashMap<String, ExternalAttachmentsConfig>,
    #[serde(default)]
    pub session_auth: HashMap<String, SessionAuthConfig>,
    #[serde(default)]
    pub window_states: HashMap<String, WindowState>,
}

/// Application-wide settings (language, etc.)
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct AppSettings {
    /// UI language code (e.g., "en-GB", "de-DE"). None = auto-detect from OS
    #[serde(default)]
    pub language: Option<String>,
}

/// Information about a TiddlyWiki edition
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EditionInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub is_user_edition: bool,
}

/// Information about a TiddlyWiki plugin
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub category: String,
}

/// Status of a folder for wiki creation
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FolderStatus {
    pub is_wiki: bool,
    pub is_empty: bool,
    pub has_files: bool,
    pub path: String,
    pub name: String,
}

/// Result from running a command
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommandResult {
    pub success: bool,
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

