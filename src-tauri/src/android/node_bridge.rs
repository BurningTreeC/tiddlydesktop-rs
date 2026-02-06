//! Node.js process spawning for Android.
//!
//! Runs TiddlyWiki commands by spawning Node.js as a subprocess.
//! This approach allows multiple Node.js instances and avoids the
//! "single instance" limitation of embedded nodejs-mobile.

#![cfg(target_os = "android")]

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// Port counter for wiki servers
static NEXT_PORT: AtomicU16 = AtomicU16::new(39000);

/// Active file sync watchers for SAF wikis
/// Maps local_path -> SyncWatcher
static SAF_SYNC_WATCHERS: std::sync::LazyLock<Mutex<HashMap<String, Arc<SyncWatcher>>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Tracks which wiki_path (SAF URI) maps to which local_path
/// Used for cleanup when a wiki is closed
static WIKI_LOCAL_PATHS: std::sync::LazyLock<Mutex<HashMap<String, String>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// A file sync watcher that polls for changes and syncs them back to SAF.
pub struct SyncWatcher {
    local_path: String,
    saf_uri: String,
    running: AtomicBool,
    /// Last modification times for each file (relative path -> mtime)
    file_mtimes: Mutex<HashMap<String, SystemTime>>,
}

impl SyncWatcher {
    /// Create a new sync watcher.
    fn new(local_path: String, saf_uri: String) -> Self {
        Self {
            local_path,
            saf_uri,
            running: AtomicBool::new(false),
            file_mtimes: Mutex::new(HashMap::new()),
        }
    }

    /// Start the sync watcher in a background thread.
    fn start(self: &Arc<Self>) {
        if self.running.swap(true, Ordering::SeqCst) {
            return; // Already running
        }

        let watcher = Arc::clone(self);

        std::thread::spawn(move || {
            eprintln!("[SyncWatcher] Started for: {}", watcher.local_path);

            // Initial scan to populate mtimes
            watcher.scan_files();

            // Poll every 2 seconds for changes
            while watcher.running.load(Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_secs(2));

                if !watcher.running.load(Ordering::SeqCst) {
                    break;
                }

                watcher.check_and_sync();
            }

            eprintln!("[SyncWatcher] Stopped for: {}", watcher.local_path);
        });
    }

    /// Stop the sync watcher.
    fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    /// Scan all files and record their modification times.
    fn scan_files(&self) {
        let mut mtimes = self.file_mtimes.lock().unwrap();
        mtimes.clear();

        if let Err(e) = self.scan_directory_recursive(&self.local_path, "", &mut mtimes) {
            eprintln!("[SyncWatcher] Error during initial scan: {}", e);
        }

        eprintln!("[SyncWatcher] Initial scan found {} files", mtimes.len());
    }

    /// Recursively scan a directory and record modification times.
    fn scan_directory_recursive(
        &self,
        base_path: &str,
        rel_path: &str,
        mtimes: &mut HashMap<String, SystemTime>,
    ) -> Result<(), String> {
        let full_path = if rel_path.is_empty() {
            PathBuf::from(base_path)
        } else {
            PathBuf::from(base_path).join(rel_path)
        };

        let entries = std::fs::read_dir(&full_path)
            .map_err(|e| format!("Failed to read directory {:?}: {}", full_path, e))?;

        for entry in entries.flatten() {
            let file_name = entry.file_name().to_string_lossy().to_string();
            let child_rel = if rel_path.is_empty() {
                file_name.clone()
            } else {
                format!("{}/{}", rel_path, file_name)
            };

            let file_type = entry.file_type().ok();

            if file_type.map(|t| t.is_dir()).unwrap_or(false) {
                self.scan_directory_recursive(base_path, &child_rel, mtimes)?;
            } else if file_type.map(|t| t.is_file()).unwrap_or(false) {
                if let Ok(metadata) = entry.metadata() {
                    if let Ok(mtime) = metadata.modified() {
                        mtimes.insert(child_rel, mtime);
                    }
                }
            }
        }

        Ok(())
    }

    /// Check for changed files and sync them to SAF.
    fn check_and_sync(&self) {
        let mut current_mtimes = HashMap::new();
        if let Err(e) = self.scan_directory_recursive(&self.local_path, "", &mut current_mtimes) {
            eprintln!("[SyncWatcher] Error scanning files: {}", e);
            return;
        }

        let mut old_mtimes = self.file_mtimes.lock().unwrap();
        let mut changed_files = Vec::new();
        let mut deleted_files = Vec::new();

        // Find new or modified files
        for (path, mtime) in &current_mtimes {
            match old_mtimes.get(path) {
                Some(old_mtime) if old_mtime == mtime => {
                    // Unchanged
                }
                _ => {
                    // New or modified
                    changed_files.push(path.clone());
                }
            }
        }

        // Find deleted files (in old_mtimes but not in current_mtimes)
        for path in old_mtimes.keys() {
            if !current_mtimes.contains_key(path) {
                deleted_files.push(path.clone());
            }
        }

        // Update the stored mtimes
        *old_mtimes = current_mtimes;
        drop(old_mtimes);

        // Sync changed files
        if !changed_files.is_empty() {
            eprintln!("[SyncWatcher] {} files changed, syncing to SAF...", changed_files.len());

            for rel_path in changed_files {
                if let Err(e) = self.sync_file_to_saf(&rel_path) {
                    eprintln!("[SyncWatcher] Error syncing {}: {}", rel_path, e);
                } else {
                    eprintln!("[SyncWatcher] Synced: {}", rel_path);
                }
            }
        }

        // Delete files from SAF that were deleted locally
        if !deleted_files.is_empty() {
            eprintln!("[SyncWatcher] {} files deleted locally, removing from SAF...", deleted_files.len());

            for rel_path in deleted_files {
                if let Err(e) = self.delete_file_from_saf(&rel_path) {
                    eprintln!("[SyncWatcher] Error deleting {}: {}", rel_path, e);
                } else {
                    eprintln!("[SyncWatcher] Deleted from SAF: {}", rel_path);
                }
            }
        }
    }

    /// Sync a single file to SAF.
    fn sync_file_to_saf(&self, rel_path: &str) -> Result<(), String> {
        use crate::android::saf;

        let local_file = PathBuf::from(&self.local_path).join(rel_path);

        // Read the local file
        let content = std::fs::read(&local_file)
            .map_err(|e| format!("Failed to read {:?}: {}", local_file, e))?;

        // Navigate to the correct SAF directory
        let parts: Vec<&str> = rel_path.split('/').collect();
        let file_name = parts.last().ok_or("Empty path")?;

        let mut current_saf_uri = self.saf_uri.clone();

        // Navigate/create subdirectories
        for dir_name in &parts[..parts.len() - 1] {
            current_saf_uri = saf::find_or_create_subdirectory(&current_saf_uri, dir_name)?;
        }

        // Find or create the file
        let file_uri = if let Ok(Some(uri)) = saf::find_in_directory(&current_saf_uri, file_name) {
            uri
        } else {
            // Determine MIME type
            let mime = if file_name.ends_with(".tid") {
                "text/plain"
            } else if file_name.ends_with(".json") {
                "application/json"
            } else if file_name.ends_with(".html") || file_name.ends_with(".htm") {
                "text/html"
            } else if file_name.ends_with(".meta") {
                "text/plain"
            } else {
                "application/octet-stream"
            };
            saf::create_file(&current_saf_uri, file_name, Some(mime))?
        };

        saf::write_document_bytes(&file_uri, &content)?;

        Ok(())
    }

    /// Delete a file from SAF that was deleted locally.
    fn delete_file_from_saf(&self, rel_path: &str) -> Result<(), String> {
        use crate::android::saf;

        // Navigate to the correct SAF directory
        let parts: Vec<&str> = rel_path.split('/').collect();
        let file_name = parts.last().ok_or("Empty path")?;

        let mut current_saf_uri = self.saf_uri.clone();

        // Navigate to parent directory
        for dir_name in &parts[..parts.len() - 1] {
            if let Ok(Some(uri)) = saf::find_subdirectory(&current_saf_uri, dir_name) {
                current_saf_uri = uri;
            } else {
                // Parent directory doesn't exist in SAF, nothing to delete
                return Ok(());
            }
        }

        // Find and delete the file
        if let Ok(Some(file_uri)) = saf::find_in_directory(&current_saf_uri, file_name) {
            saf::delete_document(&file_uri)?;
        }
        // If file doesn't exist in SAF, that's fine - it's already gone

        Ok(())
    }
}

/// Start a file sync watcher for an SAF wiki.
/// The watcher polls for file changes and syncs them back to SAF.
pub fn start_saf_sync_watcher(local_path: &str, saf_uri: &str) {
    let watcher = Arc::new(SyncWatcher::new(local_path.to_string(), saf_uri.to_string()));

    {
        let mut watchers = SAF_SYNC_WATCHERS.lock().unwrap();
        // Stop any existing watcher for this path
        if let Some(old_watcher) = watchers.remove(local_path) {
            old_watcher.stop();
        }
        watchers.insert(local_path.to_string(), Arc::clone(&watcher));
    }

    watcher.start();
}

/// Stop the file sync watcher for an SAF wiki and clean up the local copy.
pub fn stop_saf_sync_watcher(local_path: &str) {
    let mut watchers = SAF_SYNC_WATCHERS.lock().unwrap();
    if let Some(watcher) = watchers.remove(local_path) {
        watcher.stop();
    }
    drop(watchers);

    // Clean up the local copy
    if !local_path.is_empty() {
        eprintln!("[SyncWatcher] Cleaning up local wiki copy: {}", local_path);
        if let Err(e) = std::fs::remove_dir_all(local_path) {
            eprintln!("[SyncWatcher] Warning: Failed to clean up local copy: {}", e);
        }
    }
}

/// Clean up all stale wiki mirror directories.
/// Call this on app startup to remove any leftover copies from previous sessions.
pub fn cleanup_stale_wiki_mirrors() {
    let app = match crate::get_global_app_handle() {
        Some(app) => app,
        None => return,
    };

    use tauri::Manager;
    let data_dir = match app.path().app_data_dir() {
        Ok(dir) => dir,
        Err(_) => return,
    };

    let mirrors_dir = data_dir.join("wiki-mirrors");
    if !mirrors_dir.exists() {
        return;
    }

    // Get the list of local paths that have active watchers
    let active_paths: Vec<String> = {
        let watchers = SAF_SYNC_WATCHERS.lock().unwrap();
        watchers.keys().cloned().collect()
    };

    // Remove any mirror directories that don't have active watchers
    if let Ok(entries) = std::fs::read_dir(&mirrors_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let path_str = path.to_string_lossy().to_string();
                if !active_paths.iter().any(|p| p == &path_str) {
                    eprintln!("[SyncWatcher] Cleaning up stale wiki mirror: {:?}", path);
                    let _ = std::fs::remove_dir_all(&path);
                }
            }
        }
    }
}

/// Get the path to the Node.js executable.
/// On Android, the binary MUST be in the native library directory as libnode.so
/// (native libs are executable, unlike files in app data directory due to W^X policy)
pub fn get_node_path() -> Result<PathBuf, String> {
    // Get native library directory - this is the ONLY location where binaries can be executed
    let native_lib_dir = get_native_library_dir()?;
    let node_path = PathBuf::from(&native_lib_dir).join("libnode.so");

    eprintln!("[NodeBridge] Looking for node at: {:?}", node_path);

    if node_path.exists() {
        eprintln!("[NodeBridge] Found node at native lib path: {:?}", node_path);
        Ok(node_path)
    } else {
        Err(format!("Node.js binary (libnode.so) not found in native library directory: {}", native_lib_dir))
    }
}

/// Prepare library symlinks for node execution.
/// Node expects versioned library names (e.g., libz.so.1) but Android only packages
/// files ending in .so. We create symlinks from versioned names to unversioned files.
/// Returns the path to the symlink directory.
fn prepare_library_symlinks() -> Result<PathBuf, String> {
    let app = crate::get_global_app_handle()
        .ok_or_else(|| "App not initialized".to_string())?;

    use tauri::Manager;
    let data_dir = app.path().app_data_dir()
        .map_err(|e| format!("Failed to get app data dir: {}", e))?;

    let symlink_dir = data_dir.join("node-libs");
    std::fs::create_dir_all(&symlink_dir)
        .map_err(|e| format!("Failed to create symlink dir: {}", e))?;

    let native_lib_dir = get_native_library_dir()?;

    // Symlinks to create: (versioned_name, unversioned_name)
    let symlinks = [
        ("libz.so.1", "libz.so"),
        ("libcrypto.so.3", "libcrypto.so"),
        ("libssl.so.3", "libssl.so"),
        ("libicui18n.so.78", "libicui18n.so"),
        ("libicuuc.so.78", "libicuuc.so"),
        ("libicudata.so.78", "libicudata.so"),
    ];

    for (versioned, unversioned) in symlinks {
        let link_path = symlink_dir.join(versioned);
        let target_path = PathBuf::from(&native_lib_dir).join(unversioned);

        // Remove existing symlink if it exists
        let _ = std::fs::remove_file(&link_path);

        // Create symlink
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            if let Err(e) = symlink(&target_path, &link_path) {
                eprintln!("[NodeBridge] Warning: Failed to create symlink {} -> {:?}: {}", versioned, target_path, e);
            } else {
                eprintln!("[NodeBridge] Created symlink: {} -> {:?}", versioned, target_path);
            }
        }
    }

    Ok(symlink_dir)
}

/// Get the native library directory from Android context via JNI
fn get_native_library_dir() -> Result<String, String> {
    use crate::android::wiki_activity::get_java_vm;

    let vm = get_java_vm()?;
    let mut env = vm.attach_current_thread()
        .map_err(|e| format!("Failed to attach thread: {}", e))?;

    // Get ActivityThread.currentApplication()
    let activity_thread_class = env.find_class("android/app/ActivityThread")
        .map_err(|e| format!("Failed to find ActivityThread: {}", e))?;

    let app_context = env.call_static_method(
        &activity_thread_class,
        "currentApplication",
        "()Landroid/app/Application;",
        &[],
    ).map_err(|e| format!("Failed to get current application: {}", e))?
        .l().map_err(|e| format!("Failed to convert: {}", e))?;

    // Get applicationInfo
    let app_info = env.call_method(
        &app_context,
        "getApplicationInfo",
        "()Landroid/content/pm/ApplicationInfo;",
        &[],
    ).map_err(|e| format!("Failed to get applicationInfo: {}", e))?
        .l().map_err(|e| format!("Failed to convert: {}", e))?;

    // Get nativeLibraryDir field
    let native_lib_dir = env.get_field(
        &app_info,
        "nativeLibraryDir",
        "Ljava/lang/String;",
    ).map_err(|e| format!("Failed to get nativeLibraryDir: {}", e))?
        .l().map_err(|e| format!("Failed to convert: {}", e))?;

    // Convert to Rust string
    let native_lib_str: String = env.get_string((&native_lib_dir).into())
        .map_err(|e| format!("Failed to convert string: {}", e))?
        .into();

    eprintln!("[NodeBridge] Native library dir: {}", native_lib_str);
    Ok(native_lib_str)
}

/// Get the path to the extracted TiddlyWiki resources.
pub fn get_tiddlywiki_dir() -> Result<PathBuf, String> {
    let app = crate::get_global_app_handle()
        .ok_or_else(|| "App not initialized".to_string())?;

    use tauri::Manager;
    let data_dir = app.path().app_data_dir()
        .map_err(|e| format!("Failed to get app data dir: {}", e))?;

    let tw_dir = data_dir.join("tiddlywiki");

    if tw_dir.exists() {
        Ok(tw_dir)
    } else {
        Err("TiddlyWiki resources not found. Please restart the app.".to_string())
    }
}

/// Run a TiddlyWiki command and wait for completion.
///
/// # Arguments
/// * `args` - Arguments to pass to TiddlyWiki (e.g., ["editions/empty", "--build", "index"])
///
/// # Returns
/// Result with stdout on success, or error message
pub fn run_tiddlywiki_command(args: &[&str]) -> Result<String, String> {
    let node_path = get_node_path()?;
    let tw_dir = get_tiddlywiki_dir()?;
    let tiddlywiki_js = tw_dir.join("tiddlywiki.js");

    eprintln!("[NodeBridge] Running TiddlyWiki command:");
    eprintln!("[NodeBridge]   node: {:?}", node_path);
    eprintln!("[NodeBridge]   script: {:?}", tiddlywiki_js);
    eprintln!("[NodeBridge]   args: {:?}", args);

    // Debug: Check node binary status
    match std::fs::metadata(&node_path) {
        Ok(meta) => {
            eprintln!("[NodeBridge]   node exists: true, size: {} bytes", meta.len());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = meta.permissions().mode();
                eprintln!("[NodeBridge]   node permissions: {:o}", mode);

                // Try to set executable permission right before execution
                if mode & 0o111 == 0 {
                    eprintln!("[NodeBridge]   No execute permission, attempting to set...");
                    let mut perms = meta.permissions();
                    perms.set_mode(0o755);
                    if let Err(e) = std::fs::set_permissions(&node_path, perms) {
                        eprintln!("[NodeBridge]   Failed to set permissions: {}", e);
                    } else {
                        eprintln!("[NodeBridge]   Permissions set successfully");
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("[NodeBridge]   node exists: false, error: {}", e);
        }
    }

    // Prepare library symlinks for versioned library names
    let symlink_dir = prepare_library_symlinks()?;
    let native_lib_dir = get_native_library_dir()?;

    // Set LD_LIBRARY_PATH to include both native libs and symlink directory
    let ld_library_path = format!("{}:{}", symlink_dir.display(), native_lib_dir);
    eprintln!("[NodeBridge] LD_LIBRARY_PATH: {}", ld_library_path);

    let mut cmd = Command::new(&node_path);
    cmd.env("LD_LIBRARY_PATH", &ld_library_path);
    cmd.arg(&tiddlywiki_js);
    cmd.args(args);
    cmd.current_dir(&tw_dir);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let output = cmd.output()
        .map_err(|e| format!("Failed to execute Node.js: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    eprintln!("[NodeBridge] Exit code: {:?}", output.status.code());
    if !stdout.is_empty() {
        eprintln!("[NodeBridge] stdout: {}", stdout);
    }
    if !stderr.is_empty() {
        eprintln!("[NodeBridge] stderr: {}", stderr);
    }

    if output.status.success() {
        Ok(stdout)
    } else {
        Err(format!("TiddlyWiki command failed: {}\n{}", stderr, stdout))
    }
}

/// Build a single-file wiki from an edition.
///
/// # Arguments
/// * `edition` - Edition name (e.g., "empty")
/// * `output_path` - Local filesystem path for the output HTML
/// * `plugins` - List of plugin IDs to include (e.g., ["markdown", "codemirror"])
pub fn build_wiki_file(
    edition: &str,
    output_path: &str,
    plugins: &[String],
) -> Result<(), String> {
    let tw_dir = get_tiddlywiki_dir()?;

    eprintln!("[NodeBridge] Building wiki file:");
    eprintln!("[NodeBridge]   edition: {}", edition);
    eprintln!("[NodeBridge]   output: {}", output_path);
    eprintln!("[NodeBridge]   plugins: {:?}", plugins);

    // Create temp directory for build output
    let temp_dir = std::env::temp_dir().join(format!("tw-build-{}", std::process::id()));
    std::fs::create_dir_all(&temp_dir)
        .map_err(|e| format!("Failed to create temp directory: {}", e))?;

    let output_dir = temp_dir.join("output");
    std::fs::create_dir_all(&output_dir)
        .map_err(|e| format!("Failed to create output directory: {}", e))?;

    // If plugins are specified, we need to create a temporary wiki folder,
    // modify tiddlywiki.info to include the plugins, then build
    let edition_path = if !plugins.is_empty() {
        // Create a temp edition folder based on the original edition
        let temp_edition = temp_dir.join("edition");
        let original_edition = tw_dir.join("editions").join(edition);

        // Copy the edition folder
        copy_dir_recursive(&original_edition, &temp_edition)?;

        // Modify tiddlywiki.info to add plugins
        let info_path = temp_edition.join("tiddlywiki.info");
        if info_path.exists() {
            let info_content = std::fs::read_to_string(&info_path)
                .map_err(|e| format!("Failed to read tiddlywiki.info: {}", e))?;

            if let Ok(mut info) = serde_json::from_str::<serde_json::Value>(&info_content) {
                // Get or create plugins array
                let plugins_array = info.get_mut("plugins")
                    .and_then(|v| v.as_array_mut());

                if let Some(arr) = plugins_array {
                    // Add new plugins
                    for plugin in plugins {
                        let plugin_path = format!("tiddlywiki/{}", plugin);
                        let plugin_value = serde_json::Value::String(plugin_path.clone());
                        if !arr.contains(&plugin_value) {
                            arr.push(plugin_value);
                            eprintln!("[NodeBridge]   Added plugin: {}", plugin_path);
                        }
                    }
                } else {
                    // Create plugins array
                    let plugin_values: Vec<serde_json::Value> = plugins.iter()
                        .map(|p| serde_json::Value::String(format!("tiddlywiki/{}", p)))
                        .collect();
                    info["plugins"] = serde_json::Value::Array(plugin_values);
                    eprintln!("[NodeBridge]   Created plugins array with {} plugins", plugins.len());
                }

                // Write back modified tiddlywiki.info
                let modified_info = serde_json::to_string_pretty(&info)
                    .map_err(|e| format!("Failed to serialize tiddlywiki.info: {}", e))?;
                std::fs::write(&info_path, modified_info)
                    .map_err(|e| format!("Failed to write tiddlywiki.info: {}", e))?;
            }
        }

        temp_edition.to_string_lossy().to_string()
    } else {
        format!("editions/{}", edition)
    };

    let output_dir_str = output_dir.to_string_lossy();

    // Build the wiki: tiddlywiki <edition_path> --output <output_dir> --build index
    // IMPORTANT: --output must come BEFORE --build because TiddlyWiki processes commands left-to-right
    let args = vec![
        edition_path.as_str(),
        "--output", &output_dir_str,
        "--build", "index",
    ];

    run_tiddlywiki_command(&args)?;

    // Debug: List files in output directory
    eprintln!("[NodeBridge] Checking output directory: {:?}", output_dir);
    if let Ok(entries) = std::fs::read_dir(&output_dir) {
        for entry in entries.flatten() {
            eprintln!("[NodeBridge]   Found file: {:?}", entry.path());
        }
    }

    // Also check temp_dir in case output went there
    eprintln!("[NodeBridge] Checking temp directory: {:?}", temp_dir);
    if let Ok(entries) = std::fs::read_dir(&temp_dir) {
        for entry in entries.flatten() {
            eprintln!("[NodeBridge]   Found: {:?}", entry.path());
        }
    }

    // Copy the output file
    let generated = output_dir.join("index.html");
    if generated.exists() {
        // Log the size of the generated file
        if let Ok(metadata) = std::fs::metadata(&generated) {
            eprintln!("[NodeBridge] Generated file size: {} bytes ({:.2} MB)",
                metadata.len(),
                metadata.len() as f64 / 1_048_576.0);
        }

        std::fs::copy(&generated, output_path)
            .map_err(|e| format!("Failed to copy wiki to destination: {}", e))?;
        let _ = std::fs::remove_dir_all(&temp_dir);
        eprintln!("[NodeBridge] Wiki built successfully: {}", output_path);
        Ok(())
    } else {
        // Check if there's any HTML file in the output directory
        let mut found_html = None;
        if let Ok(entries) = std::fs::read_dir(&output_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "html").unwrap_or(false) {
                    found_html = Some(path);
                    break;
                }
            }
        }

        if let Some(html_path) = found_html {
            eprintln!("[NodeBridge] Found alternative HTML file: {:?}", html_path);
            std::fs::copy(&html_path, output_path)
                .map_err(|e| format!("Failed to copy wiki to destination: {}", e))?;
            let _ = std::fs::remove_dir_all(&temp_dir);
            eprintln!("[NodeBridge] Wiki built successfully: {}", output_path);
            Ok(())
        } else {
            let _ = std::fs::remove_dir_all(&temp_dir);
            Err("Wiki build succeeded but output file not found".to_string())
        }
    }
}

/// Initialize a wiki folder from an edition.
///
/// # Arguments
/// * `edition` - Edition name (e.g., "server")
/// * `folder_path` - Local filesystem path for the wiki folder
/// * `plugins` - List of plugin IDs to include (currently unused)
pub fn init_wiki_folder(
    edition: &str,
    folder_path: &str,
    _plugins: &[String],
) -> Result<(), String> {
    eprintln!("[NodeBridge] Initializing wiki folder:");
    eprintln!("[NodeBridge]   edition: {}", edition);
    eprintln!("[NodeBridge]   folder: {}", folder_path);

    // Create the folder
    std::fs::create_dir_all(folder_path)
        .map_err(|e| format!("Failed to create folder: {}", e))?;

    // Initialize: tiddlywiki <folder_path> --init <edition>
    let args = vec![folder_path, "--init", edition];

    run_tiddlywiki_command(&args)?;

    eprintln!("[NodeBridge] Wiki folder initialized: {}", folder_path);
    Ok(())
}

/// Start a TiddlyWiki server for a folder wiki.
/// Spawns Node.js in the background running `tiddlywiki --listen`.
///
/// # Arguments
/// * `folder_path` - Path to the wiki folder (must be local filesystem path)
/// * `port` - Port to listen on
///
/// # Returns
/// The server URL on success
///
/// NOTE: For SAF wikis, use `render_folder_wiki_html` + `folder_wiki_server::start_server`
/// instead. This function is for local filesystem paths only.
pub fn start_wiki_server(folder_path: &str, port: u16) -> Result<String, String> {
    let node_path = get_node_path()?;
    let tw_dir = get_tiddlywiki_dir()?;
    let tiddlywiki_js = tw_dir.join("tiddlywiki.js");

    eprintln!("[NodeBridge] Starting TiddlyWiki server:");
    eprintln!("[NodeBridge]   folder: {}", folder_path);
    eprintln!("[NodeBridge]   port: {}", port);

    // Prepare library symlinks for versioned library names
    let symlink_dir = prepare_library_symlinks()?;
    let native_lib_dir = get_native_library_dir()?;
    let ld_library_path = format!("{}:{}", symlink_dir.display(), native_lib_dir);

    let port_arg = format!("port={}", port);

    let mut cmd = Command::new(&node_path);
    cmd.env("LD_LIBRARY_PATH", &ld_library_path);
    cmd.arg(&tiddlywiki_js);
    cmd.arg(folder_path);
    cmd.arg("--listen");
    cmd.arg(&port_arg);
    cmd.arg("host=127.0.0.1");
    cmd.current_dir(&tw_dir);
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());

    // Spawn as a background process
    let _child = cmd.spawn()
        .map_err(|e| format!("Failed to start TiddlyWiki server: {}", e))?;

    let url = format!("http://127.0.0.1:{}", port);

    // Wait for the server to start by polling the port
    // Node.js can take a while to start on Android
    eprintln!("[NodeBridge] Waiting for server to start on port {}...", port);
    let mut server_ready = false;
    for attempt in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(250));
        if std::net::TcpStream::connect(format!("127.0.0.1:{}", port)).is_ok() {
            eprintln!("[NodeBridge] Server ready after {} ms", (attempt + 1) * 250);
            server_ready = true;
            break;
        }
    }

    if !server_ready {
        eprintln!("[NodeBridge] Warning: Server may not be ready yet, proceeding anyway");
    }

    eprintln!("[NodeBridge] Server started at: {}", url);

    Ok(url)
}

/// Find an available port for a wiki server.
pub fn find_available_port() -> Result<u16, String> {
    // Try ports starting from the counter
    let start = NEXT_PORT.fetch_add(1, Ordering::SeqCst);

    for offset in 0..1000 {
        let port = start + offset;
        if port > 40000 {
            break;
        }
        if std::net::TcpListener::bind(format!("127.0.0.1:{}", port)).is_ok() {
            return Ok(port);
        }
    }

    Err("No available ports found".to_string())
}

/// Copy a wiki folder from SAF to a local directory for Node.js access.
/// Returns the local path where the wiki was copied.
///
/// # Arguments
/// * `saf_uri` - The SAF content:// URI of the wiki folder
///
/// # Returns
/// The local filesystem path where the wiki was copied
///
/// NOTE: This function is used internally by `render_folder_wiki_html` to
/// temporarily copy the wiki for Node.js rendering. For runtime wiki access,
/// `FolderWikiServer` now accesses SAF directly without this copy pattern.
pub fn copy_saf_wiki_to_local(saf_uri: &str) -> Result<String, String> {
    use crate::android::saf;

    let app = crate::get_global_app_handle()
        .ok_or_else(|| "App not initialized".to_string())?;

    use tauri::Manager;
    let data_dir = app.path().app_data_dir()
        .map_err(|e| format!("Failed to get app data dir: {}", e))?;

    // Create a unique local directory for this wiki based on URI hash
    let uri_hash = format!("{:x}", md5::compute(saf_uri.as_bytes()));
    let local_wiki_dir = data_dir.join("wiki-mirrors").join(&uri_hash);

    eprintln!("[NodeBridge] Copying SAF wiki to local:");
    eprintln!("[NodeBridge]   SAF URI: {}", saf_uri);
    eprintln!("[NodeBridge]   Local path: {:?}", local_wiki_dir);

    // Create the local directory
    std::fs::create_dir_all(&local_wiki_dir)
        .map_err(|e| format!("Failed to create local wiki directory: {}", e))?;

    // Copy all files from SAF to local
    copy_saf_directory_recursive(saf_uri, &local_wiki_dir)?;

    Ok(local_wiki_dir.to_string_lossy().to_string())
}

/// Recursively copy a local directory to another local path.
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<(), String> {
    if !src.exists() {
        return Err(format!("Source directory does not exist: {:?}", src));
    }

    std::fs::create_dir_all(dst)
        .map_err(|e| format!("Failed to create directory {:?}: {}", dst, e))?;

    let entries = std::fs::read_dir(src)
        .map_err(|e| format!("Failed to read directory {:?}: {}", src, e))?;

    for entry in entries.flatten() {
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)
                .map_err(|e| format!("Failed to copy {:?} to {:?}: {}", src_path, dst_path, e))?;
        }
    }

    Ok(())
}

/// Recursively copy a SAF directory to a local path.
fn copy_saf_directory_recursive(saf_uri: &str, local_path: &std::path::Path) -> Result<(), String> {
    use crate::android::saf;

    let entries = saf::list_directory_entries(saf_uri)
        .map_err(|e| format!("Failed to list SAF directory: {}", e))?;

    for entry in entries {
        let local_file_path = local_path.join(&entry.name);

        if entry.is_dir {
            // Create local subdirectory and recurse
            std::fs::create_dir_all(&local_file_path)
                .map_err(|e| format!("Failed to create directory {:?}: {}", local_file_path, e))?;
            copy_saf_directory_recursive(&entry.uri, &local_file_path)?;
        } else {
            // Copy file content
            let content = saf::read_document_bytes(&entry.uri)
                .map_err(|e| format!("Failed to read {}: {}", entry.name, e))?;
            std::fs::write(&local_file_path, &content)
                .map_err(|e| format!("Failed to write {:?}: {}", local_file_path, e))?;
            eprintln!("[NodeBridge]   Copied: {}", entry.name);
        }
    }

    Ok(())
}

/// Sync changes from a local wiki directory back to SAF.
/// This should be called when the wiki is closed or periodically.
///
/// # Arguments
/// * `local_path` - The local filesystem path of the wiki
/// * `saf_uri` - The SAF content:// URI to sync to
///
/// DEPRECATED: With `FolderWikiServer` using direct SAF access,
/// sync is no longer needed - changes are written directly to SAF.
pub fn sync_local_wiki_to_saf(local_path: &str, saf_uri: &str) -> Result<(), String> {
    use crate::android::saf;

    eprintln!("[NodeBridge] Syncing local wiki back to SAF:");
    eprintln!("[NodeBridge]   Local: {}", local_path);
    eprintln!("[NodeBridge]   SAF: {}", saf_uri);

    sync_directory_to_saf_recursive(std::path::Path::new(local_path), saf_uri)?;

    eprintln!("[NodeBridge] Sync complete");
    Ok(())
}

/// Recursively sync a local directory to SAF.
fn sync_directory_to_saf_recursive(local_path: &std::path::Path, saf_uri: &str) -> Result<(), String> {
    use crate::android::saf;

    let entries = std::fs::read_dir(local_path)
        .map_err(|e| format!("Failed to read local directory: {}", e))?;

    for entry in entries.flatten() {
        let file_name = entry.file_name().to_string_lossy().to_string();
        let file_path = entry.path();

        if file_path.is_dir() {
            // Find or create subdirectory in SAF
            let sub_uri = saf::find_or_create_subdirectory(saf_uri, &file_name)?;
            sync_directory_to_saf_recursive(&file_path, &sub_uri)?;
        } else {
            // Sync file to SAF
            let content = std::fs::read(&file_path)
                .map_err(|e| format!("Failed to read {:?}: {}", file_path, e))?;

            // Find existing file or create new one
            let file_uri = if let Ok(Some(uri)) = saf::find_in_directory(saf_uri, &file_name) {
                uri
            } else {
                // Determine MIME type
                let mime = if file_name.ends_with(".tid") {
                    "text/plain"
                } else if file_name.ends_with(".json") {
                    "application/json"
                } else if file_name.ends_with(".html") || file_name.ends_with(".htm") {
                    "text/html"
                } else {
                    "application/octet-stream"
                };
                saf::create_file(saf_uri, &file_name, Some(mime))?
            };

            saf::write_document_bytes(&file_uri, &content)?;
        }
    }

    Ok(())
}

/// Start a TiddlyWiki server for a SAF wiki folder.
/// Copies the wiki to local storage, starts Node.js server, and sets up file sync.
///
/// Changes made by Node.js are automatically synced back to SAF every 2 seconds.
///
/// # Arguments
/// * `saf_uri` - The SAF content:// URI of the wiki folder
///
/// # Returns
/// Tuple of (server_url, local_path) on success
pub fn start_saf_wiki_server(saf_uri: &str) -> Result<(String, String), String> {
    // Copy wiki from SAF to local
    let local_path = copy_saf_wiki_to_local(saf_uri)?;

    // Track the mapping from wiki_path to local_path for cleanup
    {
        let mut paths = WIKI_LOCAL_PATHS.lock().unwrap();
        paths.insert(saf_uri.to_string(), local_path.clone());
    }

    // Find an available port
    let port = find_available_port()?;

    // Start the TiddlyWiki server on the local copy
    let server_url = start_wiki_server(&local_path, port)?;

    // Start the file sync watcher to sync changes back to SAF
    start_saf_sync_watcher(&local_path, saf_uri);
    eprintln!("[NodeBridge] Started SAF sync watcher for: {}", local_path);

    Ok((server_url, local_path))
}

/// Clean up the local copy of a wiki when it's closed.
/// Should be called when a folder wiki activity is closed.
///
/// # Arguments
/// * `wiki_path` - The SAF URI of the wiki that was closed
pub fn cleanup_wiki_local_copy(wiki_path: &str) {
    eprintln!("[NodeBridge] cleanup_wiki_local_copy: {}", wiki_path);

    // Get and remove the local path from tracking
    let local_path = {
        let mut paths = WIKI_LOCAL_PATHS.lock().unwrap();
        paths.remove(wiki_path)
    };

    if let Some(local_path) = local_path {
        // Stop the sync watcher (this also deletes the local directory)
        stop_saf_sync_watcher(&local_path);
        eprintln!("[NodeBridge] Cleaned up local copy for wiki: {}", wiki_path);
    } else {
        eprintln!("[NodeBridge] No local copy found for wiki: {}", wiki_path);
    }
}

/// Render a folder wiki to HTML using Node.js TiddlyWiki.
/// This renders the wiki once for initial serving, while FolderWikiServer
/// handles subsequent tiddler updates via TiddlyWeb protocol.
///
/// For SAF URIs: Copies to local, renders, then cleans up the local copy.
/// The rendered HTML is used by FolderWikiServer which accesses SAF directly.
///
/// # Arguments
/// * `saf_uri` - The SAF content:// URI of the wiki folder
///
/// # Returns
/// The rendered wiki HTML content
pub fn render_folder_wiki_html(saf_uri: &str) -> Result<String, String> {
    eprintln!("[NodeBridge] render_folder_wiki_html: Rendering wiki from SAF");

    // Copy wiki from SAF to local temporarily
    let local_path = copy_saf_wiki_to_local(saf_uri)?;

    // Create temp output directory
    let temp_output = std::env::temp_dir().join(format!("tw-render-{}", std::process::id()));
    std::fs::create_dir_all(&temp_output)
        .map_err(|e| format!("Failed to create temp output dir: {}", e))?;

    let temp_output_str = temp_output.to_string_lossy();

    // Render the wiki: tiddlywiki <folder> --output <temp_dir> --render '$:/core/save/all' wiki.html text/plain
    // Note: --output specifies the output directory, the filename is the third argument to --render
    let args = vec![
        local_path.as_str(),
        "--output", temp_output_str.as_ref(),
        "--render", "$:/core/save/all", "wiki.html", "text/plain",
    ];

    eprintln!("[NodeBridge] Running TiddlyWiki render command");
    run_tiddlywiki_command(&args)?;

    let output_file = temp_output.join("wiki.html");

    // Read the rendered HTML
    let html_content = if output_file.exists() {
        std::fs::read_to_string(&output_file)
            .map_err(|e| format!("Failed to read rendered HTML: {}", e))?
    } else {
        // Check temp_output for any .html file
        let mut found_html = None;
        if let Ok(entries) = std::fs::read_dir(&temp_output) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "html").unwrap_or(false) {
                    found_html = Some(path);
                    break;
                }
            }
        }

        if let Some(html_path) = found_html {
            std::fs::read_to_string(&html_path)
                .map_err(|e| format!("Failed to read rendered HTML: {}", e))?
        } else {
            // Clean up before returning error
            let _ = std::fs::remove_dir_all(&temp_output);
            let _ = std::fs::remove_dir_all(&local_path);
            return Err("Render succeeded but output HTML not found".to_string());
        }
    };

    // Clean up temporary directories
    let _ = std::fs::remove_dir_all(&temp_output);
    let _ = std::fs::remove_dir_all(&local_path);

    eprintln!("[NodeBridge] render_folder_wiki_html: Rendered {} bytes of HTML", html_content.len());
    Ok(html_content)
}

/// Convert a single-file wiki to a folder wiki.
///
/// # Arguments
/// * `source_saf_uri` - SAF URI of the source single-file wiki
/// * `dest_saf_uri` - SAF URI of the destination folder (must exist)
///
/// # Returns
/// Ok(()) on success
pub fn convert_file_to_folder(source_saf_uri: &str, dest_saf_uri: &str) -> Result<(), String> {
    use crate::android::saf;

    eprintln!("[NodeBridge] convert_file_to_folder:");
    eprintln!("[NodeBridge]   Source: {}", source_saf_uri);
    eprintln!("[NodeBridge]   Dest: {}", dest_saf_uri);

    // Read the source wiki file
    let wiki_content = saf::read_document_bytes(source_saf_uri)
        .map_err(|e| format!("Failed to read source wiki: {}", e))?;

    // Create temp directory for the source file
    let temp_dir = std::env::temp_dir().join(format!("tw-convert-{}", std::process::id()));
    std::fs::create_dir_all(&temp_dir)
        .map_err(|e| format!("Failed to create temp directory: {}", e))?;

    let temp_wiki = temp_dir.join("source.html");
    std::fs::write(&temp_wiki, &wiki_content)
        .map_err(|e| format!("Failed to write temp wiki: {}", e))?;

    // Create temp directory for output folder
    let temp_output = temp_dir.join("output");
    std::fs::create_dir_all(&temp_output)
        .map_err(|e| format!("Failed to create temp output directory: {}", e))?;

    // Run TiddlyWiki conversion: tiddlywiki --load <source> --savewikifolder <dest>
    let temp_wiki_str = temp_wiki.to_string_lossy();
    let temp_output_str = temp_output.to_string_lossy();

    let args = vec![
        "--load", temp_wiki_str.as_ref(),
        "--savewikifolder", temp_output_str.as_ref(),
    ];

    eprintln!("[NodeBridge] Running TiddlyWiki --savewikifolder command");
    run_tiddlywiki_command(&args)?;

    // Verify conversion succeeded
    if !temp_output.join("tiddlywiki.info").exists() {
        let _ = std::fs::remove_dir_all(&temp_dir);
        return Err("Conversion failed - tiddlywiki.info not created".to_string());
    }

    // Copy the folder structure to SAF
    copy_local_directory_to_saf(&temp_output, dest_saf_uri)?;

    // Clean up
    let _ = std::fs::remove_dir_all(&temp_dir);

    eprintln!("[NodeBridge] Successfully converted to folder wiki");
    Ok(())
}

/// Convert a folder wiki to a single-file wiki.
///
/// # Arguments
/// * `source_saf_uri` - SAF URI of the source folder wiki
/// * `dest_saf_uri` - SAF URI of the destination file (will be created/overwritten)
///
/// # Returns
/// Ok(()) on success
pub fn convert_folder_to_file(source_saf_uri: &str, dest_saf_uri: &str) -> Result<(), String> {
    use crate::android::saf;

    eprintln!("[NodeBridge] convert_folder_to_file:");
    eprintln!("[NodeBridge]   Source: {}", source_saf_uri);
    eprintln!("[NodeBridge]   Dest: {}", dest_saf_uri);

    // Copy the folder wiki from SAF to local
    let local_path = copy_saf_wiki_to_local(source_saf_uri)?;

    // Create temp output directory
    let temp_output = std::env::temp_dir().join(format!("tw-convert-out-{}", std::process::id()));
    std::fs::create_dir_all(&temp_output)
        .map_err(|e| format!("Failed to create temp output dir: {}", e))?;

    let temp_output_str = temp_output.to_string_lossy();

    // Run TiddlyWiki render: tiddlywiki <folder> --deletetiddlers <plugins> --output <temp> --render '$:/core/save/all' wiki.html text/plain
    // We must remove server-only plugins (tiddlyweb, filesystem) that don't work in single-file wikis
    let args = vec![
        local_path.as_str(),
        "--deletetiddlers", "$:/plugins/tiddlywiki/tiddlyweb",
        "--deletetiddlers", "$:/plugins/tiddlywiki/filesystem",
        "--output", temp_output_str.as_ref(),
        "--render", "$:/core/save/all", "wiki.html", "text/plain",
    ];

    eprintln!("[NodeBridge] Running TiddlyWiki --render command");
    run_tiddlywiki_command(&args)?;

    // Find the output file
    let output_file = temp_output.join("wiki.html");
    let html_content = if output_file.exists() {
        std::fs::read(&output_file)
            .map_err(|e| format!("Failed to read output file: {}", e))?
    } else {
        // Check for any .html file
        let mut found_html = None;
        if let Ok(entries) = std::fs::read_dir(&temp_output) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "html").unwrap_or(false) {
                    found_html = Some(path);
                    break;
                }
            }
        }

        if let Some(html_path) = found_html {
            std::fs::read(&html_path)
                .map_err(|e| format!("Failed to read output file: {}", e))?
        } else {
            let _ = std::fs::remove_dir_all(&temp_output);
            let _ = std::fs::remove_dir_all(&local_path);
            return Err("Conversion succeeded but output HTML not found".to_string());
        }
    };

    // Write the HTML to the destination SAF URI
    saf::write_document_bytes(dest_saf_uri, &html_content)?;

    // Clean up
    let _ = std::fs::remove_dir_all(&temp_output);
    let _ = std::fs::remove_dir_all(&local_path);

    eprintln!("[NodeBridge] Successfully converted to single-file wiki ({} bytes)", html_content.len());
    Ok(())
}

/// Copy a local directory structure to a SAF directory.
fn copy_local_directory_to_saf(local_path: &std::path::Path, saf_uri: &str) -> Result<(), String> {
    use crate::android::saf;

    let entries = std::fs::read_dir(local_path)
        .map_err(|e| format!("Failed to read local directory: {}", e))?;

    for entry in entries.flatten() {
        let file_name = entry.file_name().to_string_lossy().to_string();
        let file_path = entry.path();

        if file_path.is_dir() {
            // Create subdirectory in SAF and recurse
            let sub_uri = saf::find_or_create_subdirectory(saf_uri, &file_name)?;
            copy_local_directory_to_saf(&file_path, &sub_uri)?;
        } else {
            // Copy file to SAF
            let content = std::fs::read(&file_path)
                .map_err(|e| format!("Failed to read {:?}: {}", file_path, e))?;

            // Determine MIME type
            let mime = if file_name.ends_with(".tid") {
                "text/plain"
            } else if file_name.ends_with(".json") {
                "application/json"
            } else if file_name.ends_with(".html") || file_name.ends_with(".htm") {
                "text/html"
            } else if file_name.ends_with(".js") {
                "application/javascript"
            } else if file_name.ends_with(".css") {
                "text/css"
            } else if file_name.ends_with(".meta") {
                "text/plain"
            } else {
                "application/octet-stream"
            };

            // Check if file exists, otherwise create it
            let file_uri = if let Ok(Some(uri)) = saf::find_in_directory(saf_uri, &file_name) {
                uri
            } else {
                saf::create_file(saf_uri, &file_name, Some(mime))?
            };

            saf::write_document_bytes(&file_uri, &content)?;
            eprintln!("[NodeBridge]   Copied: {}", file_name);
        }
    }

    Ok(())
}

/// Ensure the Node.js binary is ready to use.
/// On Android, the binary MUST be in the native library directory (as libnode.so)
/// which is automatically executable due to Android's security model.
pub fn ensure_node_binary(_app: &tauri::App) -> Result<(), String> {
    // Get native library directory - this is the ONLY location where binaries can be executed
    let native_lib_dir = get_native_library_dir()?;
    let node_path = PathBuf::from(&native_lib_dir).join("libnode.so");

    eprintln!("[NodeBridge] Checking for node at native lib path: {:?}", node_path);

    if node_path.exists() {
        // Check file size to ensure it's a real binary
        if let Ok(metadata) = std::fs::metadata(&node_path) {
            eprintln!("[NodeBridge] Node.js binary found: {:?} ({} bytes)", node_path, metadata.len());
            if metadata.len() > 1_000_000 {
                // Node binary should be > 1MB
                return Ok(());
            } else {
                return Err(format!("libnode.so exists but is too small ({} bytes) - may be corrupted", metadata.len()));
            }
        }
    }

    Err(format!("Node.js binary (libnode.so) not found in native library directory: {}. Ensure libnode.so is included in jniLibs.", native_lib_dir))
}
