//! Wiki storage and recent files management
//!
//! This module handles persistent storage for TiddlyDesktop:
//! - Recent wikis list (wiki_list.json)
//! - Wiki-specific configurations (external attachments, session auth)

use std::path::{Path, PathBuf};
use tauri::{Emitter, Manager};
use crate::types::{WikiEntry, WikiConfigs, ExternalAttachmentsConfig, SessionAuthConfig, AppSettings};
use crate::utils;

/// Atomic write with backup: keeps a .bak copy of the previous file, writes to
/// a .tmp file first, then renames over the target. Prevents data loss if the
/// process is killed mid-write (std::fs::write truncates first → empty file).
fn atomic_write_with_backup(path: &Path, content: &str) -> Result<(), String> {
    let backup_path = path.with_extension("json.bak");
    if path.exists() {
        let _ = std::fs::copy(path, &backup_path);
    }
    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, content)
        .map_err(|e| format!("Failed to write temp file {}: {}", tmp_path.display(), e))?;
    std::fs::rename(&tmp_path, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        format!("Failed to rename {} -> {}: {}", tmp_path.display(), path.display(), e)
    })
}

/// Load a JSON config from a .bak backup file. Returns default on failure.
fn load_json_from_backup<T: serde::de::DeserializeOwned + Default>(backup_path: &Path) -> Result<T, String> {
    if !backup_path.exists() {
        eprintln!("[WikiStorage] No backup at {} — using defaults", backup_path.display());
        return Ok(T::default());
    }
    match std::fs::read_to_string(backup_path) {
        Ok(s) if s.trim().is_empty() => {
            eprintln!("[WikiStorage] Backup is also empty — using defaults");
            Ok(T::default())
        }
        Ok(s) => match serde_json::from_str(&s) {
            Ok(c) => {
                eprintln!("[WikiStorage] Recovered from backup at {}", backup_path.display());
                Ok(c)
            }
            Err(e) => {
                eprintln!("[WikiStorage] Backup also corrupt: {} — using defaults", e);
                Ok(T::default())
            }
        },
        Err(e) => {
            eprintln!("[WikiStorage] Failed to read backup: {} — using defaults", e);
            Ok(T::default())
        }
    }
}

/// Get the path to the recent files JSON
pub fn get_recent_files_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let data_dir = crate::get_data_dir(app)?;
    Ok(data_dir.join("recent_wikis.json"))
}

/// Get the path to the wiki configs JSON
pub fn get_wiki_configs_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let data_dir = crate::get_data_dir(app)?;
    Ok(data_dir.join("wiki_configs.json"))
}

/// Get the path to the app settings JSON
pub fn get_app_settings_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let data_dir = crate::get_data_dir(app)?;
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

/// Save app settings to disk (atomic write with backup)
pub fn save_app_settings(app: &tauri::AppHandle, settings: &AppSettings) -> Result<(), String> {
    let path = get_app_settings_path(app)?;

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Failed to create config directory: {}", e))?;
    }

    let content = serde_json::to_string_pretty(settings)
        .map_err(|e| format!("Failed to serialize app settings: {}", e))?;
    atomic_write_with_backup(&path, &content)
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

/// Load all wiki configs from disk (with backup recovery on corruption)
pub fn load_wiki_configs(app: &tauri::AppHandle) -> Result<WikiConfigs, String> {
    let path = get_wiki_configs_path(app)?;
    if !path.exists() {
        return Ok(WikiConfigs::default());
    }
    match std::fs::read_to_string(&path) {
        Ok(content) if content.trim().is_empty() => {
            eprintln!("[WikiStorage] WARNING: wiki_configs.json is empty — trying backup");
            load_json_from_backup::<WikiConfigs>(&path.with_extension("json.bak"))
        }
        Ok(content) => match serde_json::from_str(&content) {
            Ok(c) => Ok(c),
            Err(e) => {
                eprintln!("[WikiStorage] WARNING: Failed to parse wiki_configs.json: {} — trying backup", e);
                load_json_from_backup::<WikiConfigs>(&path.with_extension("json.bak"))
            }
        },
        Err(e) => {
            eprintln!("[WikiStorage] WARNING: Failed to read wiki_configs.json: {} — trying backup", e);
            load_json_from_backup::<WikiConfigs>(&path.with_extension("json.bak"))
        }
    }
}

/// Save all wiki configs to disk (atomic write with backup)
pub fn save_wiki_configs(app: &tauri::AppHandle, configs: &WikiConfigs) -> Result<(), String> {
    let path = get_wiki_configs_path(app)?;

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Failed to create config directory: {}", e))?;
    }

    let content = serde_json::to_string_pretty(configs)
        .map_err(|e| format!("Failed to serialize wiki configs: {}", e))?;
    atomic_write_with_backup(&path, &content)
        .map_err(|e| format!("Failed to write wiki configs: {}", e))
}

/// Load recent files from disk (with backup recovery on corruption)
pub fn load_recent_files_from_disk(app: &tauri::AppHandle) -> Vec<WikiEntry> {
    let path = match get_recent_files_path(app) {
        Ok(p) => p,
        Err(_) => return Vec::new(),
    };

    if !path.exists() {
        return Vec::new();
    }

    match std::fs::read_to_string(&path) {
        Ok(content) if content.trim().is_empty() => {
            eprintln!("[WikiStorage] WARNING: recent_wikis.json is empty — trying backup");
            load_json_from_backup::<Vec<WikiEntry>>(&path.with_extension("json.bak"))
                .unwrap_or_default()
        }
        Ok(content) => match serde_json::from_str(&content) {
            Ok(entries) => entries,
            Err(e) => {
                eprintln!("[WikiStorage] WARNING: Failed to parse recent_wikis.json: {} — trying backup", e);
                load_json_from_backup::<Vec<WikiEntry>>(&path.with_extension("json.bak"))
                    .unwrap_or_default()
            }
        },
        Err(e) => {
            eprintln!("[WikiStorage] WARNING: Failed to read recent_wikis.json: {} — trying backup", e);
            load_json_from_backup::<Vec<WikiEntry>>(&path.with_extension("json.bak"))
                .unwrap_or_default()
        }
    }
}

/// Save recent files to disk (atomic write with backup)
pub fn save_recent_files_to_disk(app: &tauri::AppHandle, entries: &[WikiEntry]) -> Result<(), String> {
    let path = get_recent_files_path(app)?;

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let json = serde_json::to_string_pretty(entries).map_err(|e| e.to_string())?;
    atomic_write_with_backup(&path, &json)
}

/// Add or update a wiki in the recent files list
pub fn add_to_recent_files(app: &tauri::AppHandle, mut entry: WikiEntry) -> Result<(), String> {
    let mut entries = load_recent_files_from_disk(app);

    // Preserve settings from existing entry (if any)
    if let Some(existing) = entries.iter().find(|e| utils::paths_equal(&e.path, &entry.path)) {
        entry.backups_enabled = existing.backups_enabled;
        entry.backup_dir = existing.backup_dir.clone();
        // Preserve LAN sync settings unless the new entry explicitly sets them
        if !entry.sync_enabled && existing.sync_enabled {
            entry.sync_enabled = existing.sync_enabled;
        }
        if entry.sync_id.is_none() && existing.sync_id.is_some() {
            entry.sync_id = existing.sync_id.clone();
        }
        // Preserve relay room assignment
        if entry.relay_room.is_none() && existing.relay_room.is_some() {
            entry.relay_room = existing.relay_room.clone();
        }
    }

    // Remove existing entry with same path (if any)
    entries.retain(|e| !utils::paths_equal(&e.path, &entry.path));

    // Add new entry at the beginning
    entries.insert(0, entry);

    // Keep only the most recent 50 entries
    entries.truncate(50);

    save_recent_files_to_disk(app, &entries)?;

    Ok(())
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
    #[allow(unused_mut)]
    let mut entries = load_recent_files_from_disk(&app);

    // On Android, load favicons from files saved by WikiActivity for entries missing data URIs
    #[cfg(target_os = "android")]
    {
        let files_dir = app.path().app_data_dir().ok().map(|d| d.join("files"));
        if let Some(files_dir) = files_dir {
            let favicons_dir = files_dir.join("favicons");
            if favicons_dir.exists() {
                let mut updated = false;
                for entry in entries.iter_mut() {
                    if entry.favicon.is_none() {
                        // Look for favicon file using MD5 hash (matching WikiActivity.saveFavicon)
                        let path_hash = format!("{:x}", md5::compute(entry.path.as_bytes()));
                        let mut favicon_file = ["png", "jpg", "gif", "svg", "ico"]
                            .iter()
                            .map(|ext| (favicons_dir.join(format!("{}.{}", path_hash, ext)), *ext))
                            .find(|(p, _)| p.exists());
                        // Fallback: check for old hashCode-based filenames
                        if favicon_file.is_none() {
                            let old_hash = java_string_hash_code(&entry.path).unsigned_abs();
                            favicon_file = ["png", "jpg", "gif", "svg", "ico"]
                                .iter()
                                .map(|ext| (favicons_dir.join(format!("{}.{}", old_hash, ext)), *ext))
                                .find(|(p, _)| p.exists());
                        }
                        if let Some((path, ext)) = favicon_file {
                            if let Ok(data) = std::fs::read(&path) {
                                use base64::Engine;
                                let b64 = base64::engine::general_purpose::STANDARD
                                    .encode(&data);
                                let mime = match ext {
                                    "png" => "image/png",
                                    "jpg" => "image/jpeg",
                                    "gif" => "image/gif",
                                    "svg" => "image/svg+xml",
                                    _ => "image/x-icon",
                                };
                                entry.favicon = Some(format!("data:{};base64,{}", mime, b64));
                                updated = true;
                            }
                        }
                    }
                }
                // Persist any newly loaded favicons so we don't re-read files every time
                if updated {
                    let _ = save_recent_files_to_disk(&app, &entries);
                }
            }
        }
    }

    entries
}

/// Remove a wiki from the recent files list
#[tauri::command]
pub fn remove_recent_file(app: tauri::AppHandle, path: String) -> Result<(), String> {
    let mut entries = load_recent_files_from_disk(&app);

    // Find the entry before removing so we can clean up related data
    let removed_entry = entries.iter().find(|e| utils::paths_equal(&e.path, &path)).cloned();

    entries.retain(|e| !utils::paths_equal(&e.path, &path));
    save_recent_files_to_disk(&app, &entries)?;

    // Clean up wiki_configs.json entries for this wiki
    if let Ok(mut configs) = load_wiki_configs(&app) {
        let mut changed = false;
        changed |= configs.external_attachments.remove(&path).is_some();
        changed |= configs.session_auth.remove(&path).is_some();
        changed |= configs.window_states.remove(&path).is_some();
        if changed {
            let _ = save_wiki_configs(&app, &configs);
        }
    }

    // Clean up sync data if the wiki had a sync_id
    if let Some(ref entry) = removed_entry {
        if let Some(ref sync_id) = entry.sync_id {
            let data_dir = crate::get_data_dir(&app).unwrap_or_default();
            // Clean up sync_state
            let state_path = data_dir.join("sync_state").join(format!("{}.json", sync_id));
            let _ = std::fs::remove_file(state_path);
            // Clean up tombstones
            let tombstone_path = data_dir.join("lan_sync_tombstones").join(format!("{}.json", sync_id));
            let _ = std::fs::remove_file(tombstone_path);
            // Clean up fingerprint cache entry
            if let Some(mgr) = crate::lan_sync::get_sync_manager() {
                mgr.remove_fingerprint_cache(sync_id);
            }
        }
    }

    // Broadcast updated WikiManifest so peers no longer see removed wikis
    if let Some(mgr) = crate::lan_sync::get_sync_manager() {
        let mgr = mgr.clone();
        tauri::async_runtime::spawn(async move {
            mgr.broadcast_wiki_manifest().await;
        });
    }

    Ok(())
}

/// Reconcile the Rust JSON config with the authoritative WikiList from the frontend.
/// Removes any entries from the Rust JSON that are NOT in the provided list of paths.
/// This prevents stale entries from being broadcast to sync peers.
/// Returns the number of entries removed.
#[tauri::command]
pub fn reconcile_recent_files(app: tauri::AppHandle, paths: Vec<String>) -> Result<i32, String> {
    let entries = load_recent_files_from_disk(&app);
    let before_count = entries.len();

    // Safety guard: if the WikiList is empty but we have entries on disk,
    // don't wipe everything — the WikiList tiddler may have been lost
    // (e.g. HTML not saved, migration issue, or race condition on startup).
    if paths.is_empty() && before_count > 0 {
        eprintln!("[WikiStorage] Reconcile: WikiList is empty but JSON has {} entries — skipping to prevent data loss", before_count);
        return Ok(0);
    }

    // Keep only entries whose path is in the authoritative WikiList
    let mut kept: Vec<WikiEntry> = Vec::new();
    let mut removed: Vec<WikiEntry> = Vec::new();
    for entry in entries {
        let found = paths.iter().any(|p| utils::paths_equal(p, &entry.path));
        if found {
            kept.push(entry);
        } else {
            removed.push(entry);
        }
    }

    let removed_count = removed.len() as i32;
    if removed_count == 0 {
        return Ok(0);
    }

    eprintln!("[WikiStorage] Reconcile: removing {} stale entries from Rust config (had {}, WikiList has {})",
        removed_count, before_count, paths.len());

    // Save the cleaned list
    save_recent_files_to_disk(&app, &kept)?;

    // Clean up related data for each removed entry
    if let Ok(mut configs) = load_wiki_configs(&app) {
        let mut changed = false;
        for entry in &removed {
            changed |= configs.external_attachments.remove(&entry.path).is_some();
            changed |= configs.session_auth.remove(&entry.path).is_some();
            changed |= configs.window_states.remove(&entry.path).is_some();
        }
        if changed {
            let _ = save_wiki_configs(&app, &configs);
        }
    }

    // Clean up sync data for removed entries
    let data_dir = crate::get_data_dir(&app).unwrap_or_default();
    for entry in &removed {
        if let Some(ref sync_id) = entry.sync_id {
            let state_path = data_dir.join("sync_state").join(format!("{}.json", sync_id));
            let _ = std::fs::remove_file(state_path);
            let tombstone_path = data_dir.join("lan_sync_tombstones").join(format!("{}.json", sync_id));
            let _ = std::fs::remove_file(tombstone_path);
            if let Some(mgr) = crate::lan_sync::get_sync_manager() {
                mgr.remove_fingerprint_cache(sync_id);
            }
        }
    }

    // Broadcast updated manifest so peers no longer see removed wikis
    if let Some(mgr) = crate::lan_sync::get_sync_manager() {
        let mgr = mgr.clone();
        tauri::async_runtime::spawn(async move {
            mgr.broadcast_wiki_manifest().await;
        });
    }

    Ok(removed_count)
}

/// Save the full wiki list from the frontend to JSON.
/// This replaces the entire recent_wikis.json with the provided entries,
/// ensuring the JSON config stays in sync with the frontend WikiList tiddler.
#[tauri::command]
pub fn save_full_wiki_list(app: tauri::AppHandle, entries: Vec<WikiEntry>) -> Result<(), String> {
    // Safety guard: refuse to save an empty list if we have entries on disk.
    // This prevents accidental data loss if JS sends an empty array due to a bug.
    if entries.is_empty() {
        let existing = load_recent_files_from_disk(&app);
        if !existing.is_empty() {
            eprintln!("[WikiStorage] save_full_wiki_list: refusing to overwrite {} entries with empty list", existing.len());
            return Ok(());
        }
    }
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
    // Validate the backup directory path if provided
    let validated_backup_dir = match backup_dir {
        Some(dir) => {
            // Android SAF URIs don't need filesystem validation
            // They're validated by Android's permission system
            if dir.starts_with("content://") || dir.starts_with('{') {
                Some(dir)
            } else {
                // Desktop: Use security validation function
                let validated = crate::drag_drop::sanitize::validate_directory_path(&dir)?;
                Some(validated.to_string_lossy().to_string())
            }
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

/// Set the maximum number of backups to keep for a wiki (None = default 20, 0 = unlimited)
#[tauri::command]
pub fn set_wiki_backup_count(app: tauri::AppHandle, path: String, count: Option<u32>) -> Result<(), String> {
    let mut entries = load_recent_files_from_disk(&app);

    for entry in entries.iter_mut() {
        if utils::paths_equal(&entry.path, &path) {
            entry.backup_count = count;
            break;
        }
    }

    save_recent_files_to_disk(&app, &entries)
}

/// Get the backup count setting for a wiki (returns None if using default)
pub fn get_wiki_backup_count(app: &tauri::AppHandle, path: &str) -> Option<u32> {
    let entries = load_recent_files_from_disk(app);
    for entry in entries {
        if utils::paths_equal(&entry.path, path) {
            return entry.backup_count;
        }
    }
    None
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

/// Set LAN sync enabled/disabled for a wiki. Assigns a sync_id (UUID) when first enabled.
/// Notifies open wiki windows to start/stop syncing and broadcasts updated manifest to peers.
#[tauri::command]
pub fn set_wiki_sync(app: tauri::AppHandle, path: String, enabled: bool) -> Result<String, String> {
    let mut entries = load_recent_files_from_disk(&app);
    let mut sync_id = String::new();

    for entry in entries.iter_mut() {
        if utils::paths_equal(&entry.path, &path) {
            entry.sync_enabled = enabled;
            if enabled && entry.sync_id.is_none() {
                // Only generate a sync_id if one doesn't exist yet.
                // Preserving existing sync_ids is critical — they're how
                // wikis are matched across devices in the manifest exchange.
                entry.sync_id = Some(crate::lan_sync::pairing::generate_random_id());
            }
            // Note: we do NOT clear sync_id on disable. This allows
            // re-enabling to keep the same ID, maintaining cross-device matching.
            sync_id = entry.sync_id.clone().unwrap_or_default();
            break;
        }
    }

    save_recent_files_to_disk(&app, &entries)?;

    // Notify open wiki windows to start or stop syncing.
    // Tauri app.emit() only reaches windows in the SAME Tauri app, but wiki
    // windows are separate processes (separate tauri::Builder). So we also
    // send via IPC (TCP) which DOES cross process boundaries.
    if enabled {
        let _ = app.emit("lan-sync-activate", serde_json::json!({
            "wiki_path": path,
            "sync_id": sync_id,
        }));
        eprintln!("[LAN Sync] Sync enabled for wiki: {} (sync_id: {})", path, sync_id);
    } else {
        let _ = app.emit("lan-sync-deactivate", serde_json::json!({
            "wiki_path": path,
        }));
        eprintln!("[LAN Sync] Sync disabled for wiki: {}", path);
    }

    // Also notify via IPC (cross-process to wiki windows)
    #[cfg(not(target_os = "android"))]
    {
        if let Some(server) = crate::GLOBAL_IPC_SERVER.get() {
            let payload = if enabled {
                serde_json::json!({
                    "type": "sync-activate",
                    "wiki_path": path,
                    "sync_id": sync_id,
                }).to_string()
            } else {
                serde_json::json!({
                    "type": "sync-deactivate",
                    "wiki_path": path,
                }).to_string()
            };
            server.send_lan_sync_to_all("*", &payload);
        }
    }

    // On Android, notify wiki windows via bridge (they poll for changes)
    #[cfg(target_os = "android")]
    {
        if !enabled && !sync_id.is_empty() {
            crate::lan_sync::queue_bridge_deactivate(&sync_id, &path);
        }
    }

    // Broadcast updated wiki manifest to connected peers
    if let Some(mgr) = crate::lan_sync::get_sync_manager() {
        let mgr = mgr.clone();
        tauri::async_runtime::spawn(async move {
            mgr.broadcast_wiki_manifest().await;
        });
    }

    Ok(sync_id)
}

/// Get the sync_id for a wiki (empty string if sync not enabled)
#[tauri::command]
pub fn get_wiki_sync_id(app: tauri::AppHandle, path: String) -> String {
    let entries = load_recent_files_from_disk(&app);
    eprintln!("[LAN Sync] get_wiki_sync_id: path={:?}, {} entries in recent_wikis", path, entries.len());
    for entry in &entries {
        if utils::paths_equal(&entry.path, &path) {
            if entry.sync_enabled {
                let id = entry.sync_id.clone().unwrap_or_default();
                eprintln!("[LAN Sync] get_wiki_sync_id: matched! sync_id={}", id);
                return id;
            }
            eprintln!("[LAN Sync] get_wiki_sync_id: matched but sync not enabled");
            return String::new();
        }
    }
    // Log all entry paths for debugging path matching issues
    for entry in &entries {
        eprintln!("[LAN Sync] get_wiki_sync_id: no match — entry.path={:?}", entry.path);
    }
    String::new()
}

/// Link a local wiki to a remote wiki's sync_id (peer-assisted matching after reinstall).
/// The user selects which local wiki corresponds to a remote wiki from the peer's manifest.
/// Also auto-assigns the wiki to the peer's room so sync permission checks pass.
#[tauri::command]
pub async fn lan_sync_link_wiki(app: tauri::AppHandle, path: String, sync_id: String, from_device_id: Option<String>, room_code: Option<String>) -> Result<Option<String>, String> {
    let mut entries = load_recent_files_from_disk(&app);
    let mut found = false;

    // Use room_code from UI if provided, otherwise look up from peer connection
    let resolved_room = if room_code.is_some() {
        room_code
    } else if let Some(ref device_id) = from_device_id {
        crate::lan_sync::find_peer_room(device_id).await
    } else {
        None
    };

    for entry in entries.iter_mut() {
        if utils::paths_equal(&entry.path, &path) {
            entry.sync_enabled = true;
            entry.sync_id = Some(sync_id.clone());
            // Always set relay_room when linking (not just when None)
            if resolved_room.is_some() {
                entry.relay_room = resolved_room.clone();
            }
            found = true;
            break;
        }
    }

    if !found {
        return Err("Wiki not found in recent files".to_string());
    }

    save_recent_files_to_disk(&app, &entries)?;

    // Tell the wiki window (if open) to activate sync.
    // Tauri app.emit() only reaches windows in the SAME Tauri app, but wiki
    // windows are separate processes. IPC (TCP) crosses process boundaries.
    let _ = app.emit("lan-sync-activate", serde_json::json!({
        "wiki_path": path,
        "sync_id": sync_id,
    }));

    // Also notify via IPC (cross-process to wiki windows)
    #[cfg(not(target_os = "android"))]
    {
        if let Some(server) = crate::GLOBAL_IPC_SERVER.get() {
            let payload = serde_json::json!({
                "type": "sync-activate",
                "wiki_path": path,
                "sync_id": sync_id,
            }).to_string();
            server.send_lan_sync_to_all("*", &payload);
        }
    }

    eprintln!("[LAN Sync] Linked wiki for sync: {} -> {} (room: {:?})", path, sync_id, resolved_room);

    // Broadcast updated wiki manifest so peers know we now have this wiki
    if let Some(mgr) = crate::lan_sync::get_sync_manager() {
        let mgr = mgr.clone();
        tauri::async_runtime::spawn(async move {
            mgr.broadcast_wiki_manifest().await;
        });
    }

    Ok(resolved_room)
}

/// Get all sync-enabled wikis (for WikiManifest exchange)
pub fn get_sync_enabled_wikis(app: &tauri::AppHandle) -> Vec<(String, String, bool)> {
    // Returns vec of (sync_id, filename, is_folder)
    let entries = load_recent_files_from_disk(app);
    entries
        .into_iter()
        .filter(|e| e.sync_enabled && e.sync_id.is_some())
        .map(|e| (e.sync_id.unwrap(), e.filename, e.is_folder))
        .collect()
}

/// Get the file path for a wiki by its sync_id
pub fn get_wiki_path_by_sync_id(app: &tauri::AppHandle, sync_id: &str) -> Option<String> {
    let entries = load_recent_files_from_disk(app);
    entries
        .into_iter()
        .find(|e| e.sync_enabled && e.sync_id.as_deref() == Some(sync_id))
        .map(|e| e.path)
}

/// Check if a wiki with the given sync_id exists locally
pub fn has_wiki_with_sync_id(app: &tauri::AppHandle, sync_id: &str) -> bool {
    let entries = load_recent_files_from_disk(app);
    entries
        .iter()
        .any(|e| e.sync_id.as_deref() == Some(sync_id))
}

/// Set the relay room for a wiki (None to unassign)
#[tauri::command]
pub fn set_wiki_relay_room(app: tauri::AppHandle, path: String, room_code: Option<String>) -> Result<(), String> {
    let mut entries = load_recent_files_from_disk(&app);

    for entry in entries.iter_mut() {
        if crate::utils::paths_equal(&entry.path, &path) {
            entry.relay_room = room_code.clone();
            // Assigning a room implicitly enables sync and ensures a sync_id exists
            if room_code.is_some() {
                entry.sync_enabled = true;
                if entry.sync_id.is_none() {
                    use rand::Rng;
                    let mut rng = rand::rng();
                    let id = format!("{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
                        rng.random::<u32>(), rng.random::<u16>(), rng.random::<u16>(),
                        rng.random::<u16>(), rng.random::<u64>() & 0xFFFFFFFFFFFF);
                    eprintln!("[wiki_storage] Generated sync_id for wiki: {}", id);
                    entry.sync_id = Some(id);
                }
            }
            break;
        }
    }

    // Find the sync_id for the wiki (after potential generation above)
    let sync_id_for_event = entries.iter()
        .find(|e| crate::utils::paths_equal(&e.path, &path))
        .and_then(|e| e.sync_id.clone());

    save_recent_files_to_disk(&app, &entries)?;

    // Notify already-open wiki windows to activate sync.
    // Tauri app.emit() only reaches windows in the SAME Tauri app, but wiki
    // windows are separate processes. IPC (TCP) crosses process boundaries.
    if room_code.is_some() {
        if let Some(ref sid) = sync_id_for_event {
            let _ = app.emit("lan-sync-activate", serde_json::json!({
                "wiki_path": path,
                "sync_id": sid,
            }));

            // Also notify via IPC (cross-process to wiki windows)
            #[cfg(not(target_os = "android"))]
            {
                if let Some(server) = crate::GLOBAL_IPC_SERVER.get() {
                    let payload = serde_json::json!({
                        "type": "sync-activate",
                        "wiki_path": path,
                        "sync_id": sid,
                    }).to_string();
                    server.send_lan_sync_to_all("*", &payload);
                }
            }
        }
    }

    // Broadcast updated wiki manifest so relay peers see the change
    if let Some(mgr) = crate::lan_sync::get_sync_manager() {
        let mgr = mgr.clone();
        tauri::async_runtime::spawn(async move {
            mgr.broadcast_wiki_manifest().await;
        });
    }

    Ok(())
}

/// Get all sync-enabled wikis assigned to a specific relay room
pub fn get_sync_wikis_for_room(app: &tauri::AppHandle, room_code: &str) -> Vec<(String, String, bool)> {
    // Returns vec of (sync_id, filename, is_folder)
    let entries = load_recent_files_from_disk(app);
    entries
        .into_iter()
        .filter(|e| e.sync_enabled && e.sync_id.is_some()
            && e.relay_room.as_deref() == Some(room_code))
        .map(|e| (e.sync_id.unwrap(), e.filename, e.is_folder))
        .collect()
}

/// Get the relay room assigned to a wiki (by path)
pub fn get_wiki_relay_room(app: &tauri::AppHandle, path: &str) -> Option<String> {
    let entries = load_recent_files_from_disk(app);
    entries
        .into_iter()
        .find(|e| crate::utils::paths_equal(&e.path, path))
        .and_then(|e| e.relay_room)
}

/// Get the relay room assigned to a wiki (by sync_id)
pub fn get_wiki_relay_room_by_sync_id(app: &tauri::AppHandle, sync_id: &str) -> Option<String> {
    let entries = load_recent_files_from_disk(app);
    entries
        .into_iter()
        .find(|e| e.sync_id.as_deref() == Some(sync_id))
        .and_then(|e| e.relay_room)
}

/// Clear relay_room from all wikis assigned to a given room code (used when removing a room)
pub fn clear_relay_room_for_code(app: &tauri::AppHandle, room_code: &str) {
    let mut entries = load_recent_files_from_disk(app);
    let mut changed = false;
    for entry in entries.iter_mut() {
        if entry.relay_room.as_deref() == Some(room_code) {
            entry.relay_room = None;
            changed = true;
        }
    }
    if changed {
        let _ = save_recent_files_to_disk(app, &entries);
    }
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

/// Compute Java's String.hashCode() for compatibility with WikiActivity.FaviconInterface.
/// Java's algorithm: for each char c, hash = hash * 31 + c (using wrapping i32 arithmetic).
#[cfg(target_os = "android")]
fn java_string_hash_code(s: &str) -> i32 {
    let mut hash: i32 = 0;
    for c in s.encode_utf16() {
        hash = hash.wrapping_mul(31).wrapping_add(c as i32);
    }
    hash
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

/// Get custom plugin path URI (Android SAF content:// URI)
#[tauri::command]
pub fn get_custom_plugin_path(app: tauri::AppHandle) -> Option<String> {
    load_app_settings(&app)
        .map(|s| s.custom_plugin_path_uri)
        .unwrap_or(None)
}

/// Set custom plugin path URI (empty string clears it)
#[tauri::command]
pub fn set_custom_plugin_path(app: tauri::AppHandle, uri: String) -> Result<(), String> {
    eprintln!("[TiddlyDesktop] set_custom_plugin_path called with: '{}'", uri);
    let mut settings = load_app_settings(&app)?;
    settings.custom_plugin_path_uri = if uri.is_empty() { None } else { Some(uri) };
    save_app_settings(&app, &settings)?;
    eprintln!("[TiddlyDesktop] Custom plugin path saved: {:?}", settings.custom_plugin_path_uri);
    Ok(())
}

/// Get custom edition path URI (Android SAF content:// URI)
#[tauri::command]
pub fn get_custom_edition_path(app: tauri::AppHandle) -> Option<String> {
    load_app_settings(&app)
        .map(|s| s.custom_edition_path_uri)
        .unwrap_or(None)
}

/// Set custom edition path URI (empty string clears it)
#[tauri::command]
pub fn set_custom_edition_path(app: tauri::AppHandle, uri: String) -> Result<(), String> {
    eprintln!("[TiddlyDesktop] set_custom_edition_path called with: '{}'", uri);
    let mut settings = load_app_settings(&app)?;
    settings.custom_edition_path_uri = if uri.is_empty() { None } else { Some(uri) };
    save_app_settings(&app, &settings)?;
    eprintln!("[TiddlyDesktop] Custom edition path saved: {:?}", settings.custom_edition_path_uri);
    Ok(())
}
