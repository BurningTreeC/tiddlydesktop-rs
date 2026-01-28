//! Wiki storage and recent files management
//!
//! This module handles persistent storage for TiddlyDesktop:
//! - Recent wikis list (wiki_list.json)
//! - Wiki-specific configurations (external attachments, session auth)

use std::path::PathBuf;
use tauri::Manager;
use crate::types::{WikiEntry, WikiConfigs, ExternalAttachmentsConfig, SessionAuthConfig, AppSettings};
use crate::utils;

/// Get the path to the recent files JSON
pub fn get_recent_files_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    Ok(data_dir.join("recent_wikis.json"))
}

/// Get the path to the wiki configs JSON
pub fn get_wiki_configs_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    Ok(data_dir.join("wiki_configs.json"))
}

/// Get the path to the app settings JSON
pub fn get_app_settings_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    Ok(data_dir.join("app_settings.json"))
}

/// Load app settings from disk
pub fn load_app_settings(app: &tauri::AppHandle) -> Result<AppSettings, String> {
    let path = get_app_settings_path(app)?;
    if path.exists() {
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read app settings: {}", e))?;
        serde_json::from_str(&content)
            .map_err(|e| format!("Failed to parse app settings: {}", e))
    } else {
        Ok(AppSettings::default())
    }
}

/// Save app settings to disk
pub fn save_app_settings(app: &tauri::AppHandle, settings: &AppSettings) -> Result<(), String> {
    let path = get_app_settings_path(app)?;

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Failed to create config directory: {}", e))?;
    }

    let content = serde_json::to_string_pretty(settings)
        .map_err(|e| format!("Failed to serialize app settings: {}", e))?;
    std::fs::write(&path, content)
        .map_err(|e| format!("Failed to write app settings: {}", e))
}

/// Detect system locale and return a language code
pub fn detect_system_language() -> String {
    use sys_locale::get_locale;

    // Get system locale (e.g., "en-US", "de-DE", "fr-FR")
    let locale = get_locale().unwrap_or_else(|| "en-GB".to_string());

    // sys-locale returns formats like "en-US" or "en_US" - normalize to TiddlyWiki format
    locale.replace('_', "-")
}

/// Get the effective language (user preference or system-detected)
pub fn get_effective_language(app: &tauri::AppHandle) -> String {
    let settings = load_app_settings(app).unwrap_or_default();
    let system_lang = detect_system_language();
    let effective = settings.language.clone().unwrap_or_else(|| system_lang.clone());
    eprintln!("[TiddlyDesktop] get_effective_language: saved={:?}, system={}, effective={}",
        settings.language, system_lang, effective);
    effective
}

/// Load all wiki configs from disk
pub fn load_wiki_configs(app: &tauri::AppHandle) -> Result<WikiConfigs, String> {
    let path = get_wiki_configs_path(app)?;
    if path.exists() {
        let content = std::fs::read_to_string(&path)
            .map_err(|e| format!("Failed to read wiki configs: {}", e))?;
        serde_json::from_str(&content)
            .map_err(|e| format!("Failed to parse wiki configs: {}", e))
    } else {
        Ok(WikiConfigs::default())
    }
}

/// Save all wiki configs to disk
pub fn save_wiki_configs(app: &tauri::AppHandle, configs: &WikiConfigs) -> Result<(), String> {
    let path = get_wiki_configs_path(app)?;

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Failed to create config directory: {}", e))?;
    }

    let content = serde_json::to_string_pretty(configs)
        .map_err(|e| format!("Failed to serialize wiki configs: {}", e))?;
    std::fs::write(&path, content)
        .map_err(|e| format!("Failed to write wiki configs: {}", e))
}

/// Load recent files from disk
pub fn load_recent_files_from_disk(app: &tauri::AppHandle) -> Vec<WikiEntry> {
    let path = match get_recent_files_path(app) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };

    if !path.exists() {
        return Vec::new();
    }

    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Save recent files to disk
pub fn save_recent_files_to_disk(app: &tauri::AppHandle, entries: &[WikiEntry]) -> Result<(), String> {
    let path = get_recent_files_path(app)?;

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let json = serde_json::to_string_pretty(entries).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())
}

/// Add or update a wiki in the recent files list
pub fn add_to_recent_files(app: &tauri::AppHandle, mut entry: WikiEntry) -> Result<(), String> {
    let mut entries = load_recent_files_from_disk(app);

    // Preserve backup settings from existing entry (if any)
    if let Some(existing) = entries.iter().find(|e| utils::paths_equal(&e.path, &entry.path)) {
        entry.backups_enabled = existing.backups_enabled;
        entry.backup_dir = existing.backup_dir.clone();
    }

    // Remove existing entry with same path (if any)
    entries.retain(|e| !utils::paths_equal(&e.path, &entry.path));

    // Add new entry at the beginning
    entries.insert(0, entry);

    // Keep only the most recent 50 entries
    entries.truncate(50);

    save_recent_files_to_disk(app, &entries)
}

// ============================================================================
// Tauri Commands
// ============================================================================

/// Debug logging from JavaScript - prints to terminal
#[tauri::command]
pub fn js_log(message: String) {
    eprintln!("[TiddlyDesktop] JS: {}", message);
}

/// Get recent files list
#[tauri::command]
pub fn get_recent_files(app: tauri::AppHandle) -> Vec<WikiEntry> {
    load_recent_files_from_disk(&app)
}

/// Remove a wiki from the recent files list
#[tauri::command]
pub fn remove_recent_file(app: tauri::AppHandle, path: String) -> Result<(), String> {
    let mut entries = load_recent_files_from_disk(&app);
    entries.retain(|e| !utils::paths_equal(&e.path, &path));
    save_recent_files_to_disk(&app, &entries)
}

/// Set backups enabled/disabled for a wiki
#[tauri::command]
pub fn set_wiki_backups(app: tauri::AppHandle, path: String, enabled: bool) -> Result<(), String> {
    let mut entries = load_recent_files_from_disk(&app);

    for entry in entries.iter_mut() {
        if utils::paths_equal(&entry.path, &path) {
            entry.backups_enabled = enabled;
            break;
        }
    }

    save_recent_files_to_disk(&app, &entries)
}

/// Set custom backup directory for a wiki (None to use default .backups folder)
#[tauri::command]
pub fn set_wiki_backup_dir(app: tauri::AppHandle, path: String, backup_dir: Option<String>) -> Result<(), String> {
    // Security: Validate the backup directory path if provided
    let validated_backup_dir = match backup_dir {
        Some(dir) => {
            // Use the security validation function
            let validated = crate::drag_drop::sanitize::validate_directory_path(&dir)?;
            Some(validated.to_string_lossy().to_string())
        }
        None => None,
    };

    let mut entries = load_recent_files_from_disk(&app);

    for entry in entries.iter_mut() {
        if utils::paths_equal(&entry.path, &path) {
            entry.backup_dir = validated_backup_dir;
            break;
        }
    }

    save_recent_files_to_disk(&app, &entries)
}

/// Get favicon for a wiki from storage
pub fn get_wiki_favicon(app: &tauri::AppHandle, path: &str) -> Option<String> {
    let entries = load_recent_files_from_disk(app);
    for entry in entries {
        if utils::paths_equal(&entry.path, path) {
            return entry.favicon;
        }
    }
    None
}

/// Get window state for a wiki
pub fn get_window_state(app: &tauri::AppHandle, path: &str) -> Option<crate::types::WindowState> {
    let configs = load_wiki_configs(app).ok()?;
    configs.window_states.get(path).cloned()
}

/// Save window state for a wiki
#[tauri::command]
pub fn save_window_state(
    app: tauri::AppHandle,
    path: String,
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    monitor_name: Option<String>,
    monitor_x: Option<i32>,
    monitor_y: Option<i32>,
    maximized: bool,
) -> Result<(), String> {
    eprintln!("[TiddlyDesktop] Saving window state for '{}': {}x{} at ({}, {}), monitor=({}, {}), maximized={}",
        path, width, height, x, y, monitor_x.unwrap_or(0), monitor_y.unwrap_or(0), maximized);
    let mut configs = load_wiki_configs(&app)?;
    configs.window_states.insert(path, crate::types::WindowState {
        width,
        height,
        x,
        y,
        monitor_name,
        monitor_x: monitor_x.unwrap_or(0),
        monitor_y: monitor_y.unwrap_or(0),
        maximized,
    });
    save_wiki_configs(&app, &configs)
}

/// Maximum size for favicon data URIs (1MB)
const MAX_FAVICON_SIZE: usize = 1024 * 1024;

/// Update favicon for a wiki (used after decryption when favicon wasn't available initially)
#[tauri::command]
pub fn update_wiki_favicon(app: tauri::AppHandle, path: String, favicon: Option<String>) -> Result<(), String> {
    use tauri::Emitter;

    // Security: Validate favicon size
    if let Some(ref fav) = favicon {
        if fav.len() > MAX_FAVICON_SIZE {
            return Err(format!(
                "Favicon too large ({} bytes, max {} bytes)",
                fav.len(),
                MAX_FAVICON_SIZE
            ));
        }
    }

    let mut entries = load_recent_files_from_disk(&app);

    for entry in entries.iter_mut() {
        if utils::paths_equal(&entry.path, &path) {
            entry.favicon = favicon.clone();
            break;
        }
    }

    save_recent_files_to_disk(&app, &entries)?;

    // Emit event to update just this favicon in the landing page
    let _ = app.emit("wiki-favicon-updated", serde_json::json!({
        "path": path,
        "favicon": favicon
    }));

    Ok(())
}

/// Set group for a wiki (None to move to "Ungrouped")
#[tauri::command]
pub fn set_wiki_group(app: tauri::AppHandle, path: String, group: Option<String>) -> Result<(), String> {
    let mut entries = load_recent_files_from_disk(&app);

    for entry in entries.iter_mut() {
        if utils::paths_equal(&entry.path, &path) {
            entry.group = group;
            break;
        }
    }

    save_recent_files_to_disk(&app, &entries)
}

/// Get all unique group names from the wiki list
#[tauri::command]
pub fn get_wiki_groups(app: tauri::AppHandle) -> Vec<String> {
    let entries = load_recent_files_from_disk(&app);
    let mut groups: Vec<String> = entries
        .iter()
        .filter_map(|e| e.group.clone())
        .collect();
    groups.sort();
    groups.dedup();
    groups
}

/// Rename a group (updates all wikis in that group)
#[tauri::command]
pub fn rename_wiki_group(app: tauri::AppHandle, old_name: String, new_name: String) -> Result<(), String> {
    let mut entries = load_recent_files_from_disk(&app);

    for entry in entries.iter_mut() {
        if entry.group.as_ref() == Some(&old_name) {
            entry.group = Some(new_name.clone());
        }
    }

    save_recent_files_to_disk(&app, &entries)
}

/// Delete a group (moves all wikis to Ungrouped)
#[tauri::command]
pub fn delete_wiki_group(app: tauri::AppHandle, group_name: String) -> Result<(), String> {
    let mut entries = load_recent_files_from_disk(&app);

    for entry in entries.iter_mut() {
        if entry.group.as_ref() == Some(&group_name) {
            entry.group = None;
        }
    }

    save_recent_files_to_disk(&app, &entries)
}

/// Get current backup directory setting for a wiki (None means default .backups folder)
#[tauri::command]
pub fn get_wiki_backup_dir_setting(app: tauri::AppHandle, path: String) -> Option<String> {
    let entries = load_recent_files_from_disk(&app);

    for entry in entries {
        if utils::paths_equal(&entry.path, &path) {
            return entry.backup_dir;
        }
    }

    None
}

/// Get external attachments config for a wiki
#[tauri::command]
pub fn get_external_attachments_config(app: tauri::AppHandle, wiki_path: String) -> Result<ExternalAttachmentsConfig, String> {
    let configs = load_wiki_configs(&app)?;
    Ok(configs.external_attachments.get(&wiki_path).cloned().unwrap_or_default())
}

/// Set external attachments config for a wiki
#[tauri::command]
pub fn set_external_attachments_config(app: tauri::AppHandle, wiki_path: String, config: ExternalAttachmentsConfig) -> Result<(), String> {
    let mut configs = load_wiki_configs(&app)?;
    configs.external_attachments.insert(wiki_path, config);
    save_wiki_configs(&app, &configs)
}

/// Get session auth config for a wiki
#[tauri::command]
pub fn get_session_auth_config(app: tauri::AppHandle, wiki_path: String) -> Result<SessionAuthConfig, String> {
    let configs = load_wiki_configs(&app)?;
    Ok(configs.session_auth.get(&wiki_path).cloned().unwrap_or_default())
}

/// Set session auth config for a wiki
#[tauri::command]
pub fn set_session_auth_config(app: tauri::AppHandle, wiki_path: String, config: SessionAuthConfig) -> Result<(), String> {
    let mut configs = load_wiki_configs(&app)?;
    configs.session_auth.insert(wiki_path, config);
    save_wiki_configs(&app, &configs)
}

/// Get current UI language (user preference or auto-detected)
#[tauri::command]
pub fn get_language(app: tauri::AppHandle) -> String {
    get_effective_language(&app)
}

/// Set UI language preference (empty string = auto-detect from OS)
#[tauri::command]
pub fn set_language(app: tauri::AppHandle, language: String) -> Result<(), String> {
    eprintln!("[TiddlyDesktop] set_language called with: '{}'", language);
    let mut settings = load_app_settings(&app)?;
    settings.language = if language.is_empty() { None } else { Some(language.clone()) };
    save_app_settings(&app, &settings)?;
    eprintln!("[TiddlyDesktop] Language saved: {:?}", settings.language);
    Ok(())
}

/// Check if user has a custom language set (vs auto-detect)
#[tauri::command]
pub fn has_custom_language(app: tauri::AppHandle) -> bool {
    load_app_settings(&app)
        .map(|s| s.language.is_some())
        .unwrap_or(false)
}

/// Get system-detected language (ignoring user preference)
#[tauri::command]
pub fn get_system_language() -> String {
    detect_system_language()
}

/// Get current palette preference
#[tauri::command]
pub fn get_palette(app: tauri::AppHandle) -> Option<String> {
    load_app_settings(&app)
        .map(|s| s.palette)
        .unwrap_or(None)
}

/// Set palette preference (empty string = default)
#[tauri::command]
pub fn set_palette(app: tauri::AppHandle, palette: String) -> Result<(), String> {
    eprintln!("[TiddlyDesktop] set_palette called with: '{}'", palette);
    let mut settings = load_app_settings(&app)?;
    settings.palette = if palette.is_empty() { None } else { Some(palette.clone()) };
    save_app_settings(&app, &settings)?;
    eprintln!("[TiddlyDesktop] Palette saved: {:?}", settings.palette);
    Ok(())
}
