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
// Node.js servers use port range 38000-38999.
// WikiHttpServer (Kotlin, :wiki process) uses 39000-39999.
// Separate ranges prevent port collisions between processes.
static NEXT_PORT: AtomicU16 = AtomicU16::new(38000);

/// Cached app data directory path. Resolved once, reused forever.
/// Works in both main process (via Tauri) and :wiki process (via JNI).
static APP_DATA_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Get the app data directory, working in both the main process and :wiki process.
/// Tries Tauri AppHandle first, falls back to JNI.
fn get_app_data_dir() -> Result<PathBuf, String> {
    if let Some(dir) = APP_DATA_DIR.get() {
        return Ok(dir.clone());
    }

    // Try Tauri first (works in main process)
    if let Some(app) = crate::get_global_app_handle() {
        use tauri::Manager;
        if let Ok(data_dir) = app.path().app_data_dir() {
            let _ = APP_DATA_DIR.set(data_dir.clone());
            return Ok(data_dir);
        }
    }

    // Fallback: get data dir via JNI (works in :wiki process)
    let dir = get_app_data_dir_jni()?;
    let _ = APP_DATA_DIR.set(dir.clone());
    Ok(dir)
}

/// Get the app data directory via JNI (ApplicationInfo.dataDir).
fn get_app_data_dir_jni() -> Result<PathBuf, String> {
    use super::wiki_activity::get_java_vm;

    let vm = get_java_vm()?;
    let mut env = vm.attach_current_thread()
        .map_err(|e| format!("Failed to attach thread: {}", e))?;

    let activity_thread_class = env.find_class("android/app/ActivityThread")
        .map_err(|e| format!("Failed to find ActivityThread: {}", e))?;

    let app_context = env.call_static_method(
        &activity_thread_class,
        "currentApplication",
        "()Landroid/app/Application;",
        &[],
    ).map_err(|e| format!("Failed to get current application: {}", e))?
        .l().map_err(|e| format!("Failed to convert: {}", e))?;

    let app_info = env.call_method(
        &app_context,
        "getApplicationInfo",
        "()Landroid/content/pm/ApplicationInfo;",
        &[],
    ).map_err(|e| format!("Failed to get applicationInfo: {}", e))?
        .l().map_err(|e| format!("Failed to convert: {}", e))?;

    let data_dir_jstr = env.get_field(
        &app_info,
        "dataDir",
        "Ljava/lang/String;",
    ).map_err(|e| format!("Failed to get dataDir: {}", e))?
        .l().map_err(|e| format!("Failed to convert: {}", e))?;

    let data_dir_str: String = env.get_string((&data_dir_jstr).into())
        .map_err(|e| format!("Failed to convert string: {}", e))?
        .into();

    eprintln!("[NodeBridge] App data dir (JNI): {}", data_dir_str);
    Ok(PathBuf::from(data_dir_str))
}

/// Get a writable temp directory for Android.
/// `/tmp` isn't writable on Android — use the app's data directory instead.
fn android_temp_dir() -> PathBuf {
    if let Ok(data_dir) = get_app_data_dir() {
        return data_dir.join("tmp");
    }
    // Last resort fallback (shouldn't happen)
    PathBuf::from("/data/local/tmp")
}

/// Active file sync watchers for SAF wikis
/// Maps local_path -> SyncWatcher
static SAF_SYNC_WATCHERS: std::sync::LazyLock<Mutex<HashMap<String, Arc<SyncWatcher>>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Tracks which wiki_path (SAF URI) maps to which local_path
/// Used for cleanup when a wiki is closed
static WIKI_LOCAL_PATHS: std::sync::LazyLock<Mutex<HashMap<String, String>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

/// Tracks running folder wiki servers: wiki_path -> server_url
/// Prevents starting duplicate Node.js servers for the same wiki
static RUNNING_SERVERS: std::sync::LazyLock<Mutex<HashMap<String, String>>> =
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

/// Get the URL of an already-running server for the given wiki path, if any.
pub fn get_running_server(wiki_path: &str) -> Option<String> {
    let servers = RUNNING_SERVERS.lock().unwrap();
    servers.get(wiki_path).cloned()
}

/// Register a running server for a wiki path.
pub fn register_running_server(wiki_path: &str, server_url: &str) {
    let mut servers = RUNNING_SERVERS.lock().unwrap();
    servers.insert(wiki_path.to_string(), server_url.to_string());
}

/// Unregister a running server for a wiki path (called on wiki close).
pub fn unregister_running_server(wiki_path: &str) {
    let mut servers = RUNNING_SERVERS.lock().unwrap();
    servers.remove(wiki_path);
}

/// Track the mapping from wiki_path (SAF URI) to local_path for cleanup.
pub fn track_wiki_local_path(wiki_path: &str, local_path: &str) {
    let mut paths = WIKI_LOCAL_PATHS.lock().unwrap();
    paths.insert(wiki_path.to_string(), local_path.to_string());
}

/// Get a local filesystem path for an SAF folder wiki.
/// Checks the in-memory mapping first, then tries the computed local mirror path.
/// If neither exists, creates a fresh local copy via SAF.
pub fn get_or_create_local_copy(saf_uri: &str) -> Result<String, String> {
    // Check in-memory mapping first
    {
        let paths = WIKI_LOCAL_PATHS.lock().unwrap();
        if let Some(local) = paths.get(saf_uri) {
            let p = std::path::Path::new(local.as_str());
            if p.is_dir() {
                return Ok(local.clone());
            }
        }
    }

    // Check the expected local mirror path (survives app restarts)
    let data_dir = get_app_data_dir()?;
    let uri_hash = format!("{:x}", md5::compute(saf_uri.as_bytes()));
    let mirror_dir = data_dir.join("wiki-mirrors").join(&uri_hash);
    if mirror_dir.is_dir() && mirror_dir.join("tiddlywiki.info").exists() {
        let local = mirror_dir.to_string_lossy().to_string();
        // Re-register in memory
        let mut paths = WIKI_LOCAL_PATHS.lock().unwrap();
        paths.insert(saf_uri.to_string(), local.clone());
        return Ok(local);
    }

    // No local copy exists — create one from SAF
    copy_saf_wiki_to_local(saf_uri)
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
    let data_dir = match get_app_data_dir() {
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
    let data_dir = get_app_data_dir()?;
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
    let data_dir = get_app_data_dir()?;
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

/// Strip tiddlyweb and filesystem plugins from a tiddlywiki.info file.
/// These plugins are designed for client-server folder wikis and cause problems
/// in standalone single-file wikis.
fn strip_server_plugins_from_info(info_path: &std::path::Path) -> Result<(), String> {
    let content = std::fs::read_to_string(info_path)
        .map_err(|e| format!("Failed to read tiddlywiki.info: {}", e))?;
    let mut info: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse tiddlywiki.info: {}", e))?;
    if let Some(arr) = info.get_mut("plugins").and_then(|v| v.as_array_mut()) {
        arr.retain(|p| {
            let name = p.as_str().unwrap_or("");
            name != "tiddlywiki/tiddlyweb" && name != "tiddlywiki/filesystem"
        });
    }
    let updated = serde_json::to_string_pretty(&info)
        .map_err(|e| format!("Failed to serialize tiddlywiki.info: {}", e))?;
    std::fs::write(info_path, updated)
        .map_err(|e| format!("Failed to write tiddlywiki.info: {}", e))?;
    Ok(())
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
    let temp_dir = android_temp_dir().join(format!("tw-build-{}", std::process::id()));
    std::fs::create_dir_all(&temp_dir)
        .map_err(|e| format!("Failed to create temp directory: {}", e))?;

    let output_dir = temp_dir.join("output");
    std::fs::create_dir_all(&output_dir)
        .map_err(|e| format!("Failed to create output directory: {}", e))?;

    // Always copy the edition to a temp dir so we can modify tiddlywiki.info
    // (add user-selected plugins and strip server-only plugins)
    let temp_edition = temp_dir.join("edition");
    let original_edition = tw_dir.join("editions").join(edition);
    copy_dir_recursive(&original_edition, &temp_edition)?;

    // Add user-selected plugins to tiddlywiki.info
    let info_path = temp_edition.join("tiddlywiki.info");
    if !plugins.is_empty() && info_path.exists() {
        let info_content = std::fs::read_to_string(&info_path)
            .map_err(|e| format!("Failed to read tiddlywiki.info: {}", e))?;

        if let Ok(mut info) = serde_json::from_str::<serde_json::Value>(&info_content) {
            let plugins_array = info.get_mut("plugins")
                .and_then(|v| v.as_array_mut());

            if let Some(arr) = plugins_array {
                for plugin in plugins {
                    let plugin_path = format!("tiddlywiki/{}", plugin);
                    let plugin_value = serde_json::Value::String(plugin_path.clone());
                    if !arr.contains(&plugin_value) {
                        arr.push(plugin_value);
                        eprintln!("[NodeBridge]   Added plugin: {}", plugin_path);
                    }
                }
            } else {
                let plugin_values: Vec<serde_json::Value> = plugins.iter()
                    .map(|p| serde_json::Value::String(format!("tiddlywiki/{}", p)))
                    .collect();
                info["plugins"] = serde_json::Value::Array(plugin_values);
                eprintln!("[NodeBridge]   Created plugins array with {} plugins", plugins.len());
            }

            let modified_info = serde_json::to_string_pretty(&info)
                .map_err(|e| format!("Failed to serialize tiddlywiki.info: {}", e))?;
            std::fs::write(&info_path, modified_info)
                .map_err(|e| format!("Failed to write tiddlywiki.info: {}", e))?;
        }
    }

    // Strip server-only plugins that don't work in single-file wikis
    if info_path.exists() {
        strip_server_plugins_from_info(&info_path)?;
    }

    let edition_path = temp_edition.to_string_lossy().to_string();

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
    cmd.stderr(Stdio::piped());

    // Spawn as a background process
    let mut child = cmd.spawn()
        .map_err(|e| format!("Failed to start TiddlyWiki server: {}", e))?;

    // Forward Node.js stderr to logcat in a background thread
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            use std::io::{BufRead, BufReader};
            let reader = BufReader::new(stderr);
            for line in reader.lines().flatten() {
                eprintln!("[NodeJS] {}", line);
            }
        });
    }

    let url = format!("http://127.0.0.1:{}", port);

    // Brief poll for server startup — don't block long since WikiActivity's
    // error handler and watchdog will auto-retry if the server isn't ready yet.
    // This avoids blocking the landing page for 5+ seconds under load.
    eprintln!("[NodeBridge] Waiting for server to start on port {}...", port);
    let mut server_ready = false;
    for attempt in 0..6 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if std::net::TcpStream::connect(format!("127.0.0.1:{}", port)).is_ok() {
            eprintln!("[NodeBridge] Server ready after {} ms", (attempt + 1) * 500);
            server_ready = true;
            break;
        }
    }

    if !server_ready {
        eprintln!("[NodeBridge] Server not ready yet, WikiActivity will retry");
    }

    eprintln!("[NodeBridge] Server started at: {}", url);

    Ok(url)
}

/// Find an available port for a Node.js wiki server (range 38000-38999).
/// WikiHttpServer uses 39000-39999 in the :wiki process.
pub fn find_available_port() -> Result<u16, String> {
    // Try ports starting from the counter
    let start = NEXT_PORT.fetch_add(1, Ordering::SeqCst);

    for offset in 0..1000 {
        let port = start + offset;
        if port > 38999 {
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

    let data_dir = get_app_data_dir()?;

    // Create a unique local directory for this wiki based on URI hash
    let uri_hash = format!("{:x}", md5::compute(saf_uri.as_bytes()));
    let local_wiki_dir = data_dir.join("wiki-mirrors").join(&uri_hash);

    eprintln!("[NodeBridge] Copying SAF wiki to local:");
    eprintln!("[NodeBridge]   SAF URI: {}", saf_uri);
    eprintln!("[NodeBridge]   Local path: {:?}", local_wiki_dir);

    // Stop any existing Rust SyncWatcher for this path BEFORE clearing the directory.
    // If a watcher is running and we clear the directory, it would see all files as
    // "deleted" and propagate those deletions to SAF — destroying the wiki source.
    {
        let local_path_str = local_wiki_dir.to_string_lossy().to_string();
        let mut watchers = SAF_SYNC_WATCHERS.lock().unwrap();
        if let Some(watcher) = watchers.remove(&local_path_str) {
            watcher.stop();
            eprintln!("[NodeBridge] Stopped existing SyncWatcher before clearing local dir");
        }
    }

    // Clear the local directory first to remove any stale files
    // (e.g. tiddler files saved by syncer that no longer exist in SAF source)
    if local_wiki_dir.exists() {
        std::fs::remove_dir_all(&local_wiki_dir)
            .map_err(|e| format!("Failed to clear local wiki directory: {}", e))?;
    }

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

    // Prepare tiddlywiki.info for Android: add server plugins, resolve includeWikis paths
    let info_path = std::path::PathBuf::from(&local_path).join("tiddlywiki.info");
    if info_path.exists() {
        match prepare_info_for_android(&info_path) {
            Ok(()) => eprintln!("[NodeBridge] Prepared tiddlywiki.info for Android"),
            Err(e) => eprintln!("[NodeBridge] Warning: Failed to prepare tiddlywiki.info: {}", e),
        }
    }

    // Track the mapping from wiki_path to local_path for cleanup
    {
        let mut paths = WIKI_LOCAL_PATHS.lock().unwrap();
        paths.insert(saf_uri.to_string(), local_path.clone());
    }

    // Find an available port
    let port = find_available_port()?;

    // Start the TiddlyWiki server on the local copy
    let server_url = start_wiki_server(&local_path, port)?;

    // NOTE: The SAF sync watcher now runs in Kotlin (WikiActivity.kt) in the :wiki process,
    // so it survives main process death. Do NOT start the Rust SyncWatcher here — it races
    // with copy_saf_wiki_to_local's remove_dir_all and can delete all files from SAF.

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

    // Prepare tiddlywiki.info: resolve includeWikis paths, add server plugins
    let info_path = std::path::PathBuf::from(&local_path).join("tiddlywiki.info");
    if info_path.exists() {
        match prepare_info_for_android(&info_path) {
            Ok(()) => eprintln!("[NodeBridge] Prepared tiddlywiki.info for rendering"),
            Err(e) => eprintln!("[NodeBridge] Warning: Failed to prepare tiddlywiki.info: {}", e),
        }
    }

    // Create temp output directory
    let temp_output = android_temp_dir().join(format!("tw-render-{}", std::process::id()));
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

/// Prepare tiddlywiki.info for running on Android:
/// 1. Add tiddlyweb and filesystem plugins (required for client-server mode)
/// 2. Create symlinks so relative includeWikis paths resolve to bundled editions
/// 3. Create symlinks for config.default-tiddler-location if it points outside the wiki folder
///
/// The tiddlywiki.info file is NOT modified for includeWikis/config paths — we keep the
/// original relative paths and make them resolve by placing symlinks at the expected locations.
pub fn prepare_info_for_android(info_path: &std::path::Path) -> Result<(), String> {
    use std::io::{Read, Write};

    let tw_dir = get_tiddlywiki_dir()?;
    let editions_dir = tw_dir.join("editions");

    // Read the existing tiddlywiki.info
    let mut file = std::fs::File::open(info_path)
        .map_err(|e| format!("Failed to open tiddlywiki.info: {}", e))?;
    let mut content = String::new();
    file.read_to_string(&mut content)
        .map_err(|e| format!("Failed to read tiddlywiki.info: {}", e))?;
    drop(file);

    // Parse as JSON
    let mut info: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse tiddlywiki.info: {}", e))?;

    let mut modified = false;

    // --- 1. Ensure server plugins are present ---
    if info.get("plugins").is_none() {
        info["plugins"] = serde_json::json!([]);
    }
    let plugins = info["plugins"].as_array_mut()
        .ok_or_else(|| "plugins is not an array".to_string())?;

    for plugin in &["tiddlywiki/tiddlyweb", "tiddlywiki/filesystem"] {
        let plugin_str = serde_json::Value::String(plugin.to_string());
        if !plugins.contains(&plugin_str) {
            plugins.push(plugin_str);
            eprintln!("[NodeBridge] Added plugin: {}", plugin);
            modified = true;
        }
    }

    // --- 2. Create symlinks for includeWikis relative paths ---
    let wiki_dir = info_path.parent()
        .ok_or_else(|| "Cannot get wiki directory".to_string())?;

    if let Some(include_wikis) = info.get("includeWikis") {
        if let Some(arr) = include_wikis.as_array() {
            for entry in arr.iter() {
                let rel_path = if let Some(s) = entry.as_str() {
                    s.to_string()
                } else if let Some(p) = entry.get("path").and_then(|v| v.as_str()) {
                    p.to_string()
                } else {
                    continue;
                };

                create_edition_symlink(&rel_path, wiki_dir, &editions_dir);
            }
        }
    }

    // --- 3. Create symlink for config.default-tiddler-location ---
    if let Some(config) = info.get("config") {
        if let Some(loc) = config.get("default-tiddler-location").and_then(|v| v.as_str()) {
            if loc.starts_with("../") || loc.starts_with("..\\") {
                create_edition_symlink(loc, wiki_dir, &editions_dir);
            }
        }
    }

    // Only write back if we added plugins (don't touch includeWikis/config paths)
    if modified {
        let updated_content = serde_json::to_string_pretty(&info)
            .map_err(|e| format!("Failed to serialize tiddlywiki.info: {}", e))?;
        let mut file = std::fs::File::create(info_path)
            .map_err(|e| format!("Failed to create tiddlywiki.info: {}", e))?;
        file.write_all(updated_content.as_bytes())
            .map_err(|e| format!("Failed to write tiddlywiki.info: {}", e))?;
    }

    Ok(())
}

/// Create a symlink so that a relative path from a wiki resolves to a bundled edition.
///
/// For example, if `rel_path` is `"../tw5.com"` and the wiki is at
/// `/data/.../wiki-mirrors/{hash}/`, this creates a symlink at
/// `/data/.../wiki-mirrors/tw5.com` → `{app_data_dir}/tiddlywiki/editions/tw5.com`
///
/// For paths like `"../tw5.com/tiddlers"`, we symlink the top-level directory `"tw5.com"`.
fn create_edition_symlink(
    rel_path: &str,
    wiki_dir: &std::path::Path,
    editions_dir: &std::path::Path,
) {
    // Check if the relative path already resolves (e.g. sibling wiki exists)
    let resolved = wiki_dir.join(rel_path);
    if resolved.exists() {
        eprintln!("[NodeBridge] includeWiki '{}' already resolves to {:?}", rel_path, resolved);
        return;
    }

    // Strip leading "../" to get the edition name
    // "../tw5.com" → "tw5.com", "../tw5.com/tiddlers" → "tw5.com/tiddlers"
    let stripped = rel_path.trim_start_matches("../").trim_start_matches("..\\");

    // Get the top-level directory name (e.g. "tw5.com" from "tw5.com/tiddlers")
    let top_dir = stripped.split('/').next().unwrap_or(stripped);
    let top_dir = top_dir.split('\\').next().unwrap_or(top_dir);

    // Check if the edition exists in the bundled editions
    let edition_path = editions_dir.join(top_dir);
    if !edition_path.exists() {
        eprintln!("[NodeBridge] Warning: bundled edition '{}' not found at {:?}", top_dir, edition_path);
        return;
    }

    // The symlink target: where the relative path resolves from wiki_dir
    // For "../tw5.com", this is wiki_dir/../tw5.com = wiki_dir.parent()/tw5.com
    let link_path = if let Some(parent) = wiki_dir.parent() {
        parent.join(top_dir)
    } else {
        eprintln!("[NodeBridge] Warning: cannot get parent of wiki dir for symlink");
        return;
    };

    // Remove existing symlink if it points to the wrong place
    if link_path.exists() || link_path.symlink_metadata().is_ok() {
        if let Ok(target) = std::fs::read_link(&link_path) {
            if target == edition_path {
                eprintln!("[NodeBridge] Symlink already correct: {:?} → {:?}", link_path, edition_path);
                return;
            }
        }
        let _ = std::fs::remove_file(&link_path);
        let _ = std::fs::remove_dir_all(&link_path);
    }

    // Create symlink
    match std::os::unix::fs::symlink(&edition_path, &link_path) {
        Ok(()) => eprintln!("[NodeBridge] Created symlink: {:?} → {:?}", link_path, edition_path),
        Err(e) => eprintln!("[NodeBridge] Failed to create symlink {:?} → {:?}: {}", link_path, edition_path, e),
    }
}

/// Legacy wrapper — calls prepare_info_for_android
fn add_server_plugins_to_info(info_path: &std::path::Path) -> Result<(), String> {
    prepare_info_for_android(info_path)
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
    let temp_dir = android_temp_dir().join(format!("tw-convert-{}", std::process::id()));
    std::fs::create_dir_all(&temp_dir)
        .map_err(|e| format!("Failed to create temp directory: {}", e))?;

    let temp_wiki = temp_dir.join("source.html");
    std::fs::write(&temp_wiki, &wiki_content)
        .map_err(|e| format!("Failed to write temp wiki: {}", e))?;

    // Create temp directory for output folder
    let temp_output = temp_dir.join("output");
    std::fs::create_dir_all(&temp_output)
        .map_err(|e| format!("Failed to create temp output directory: {}", e))?;

    // Run TiddlyWiki conversion: tiddlywiki --load <source> --deletetiddlers <filters> --savewikifolder <dest>
    // We strip TiddlyDesktop-injected tiddlers that were accidentally saved or shouldn't be in standalone wikis
    let temp_wiki_str = temp_wiki.to_string_lossy();
    let temp_output_str = temp_output.to_string_lossy();

    let args = vec![
        "--load", temp_wiki_str.as_ref(),
        // TiddlyDesktop injected plugin and tiddlers (filter for prefix match)
        "--deletetiddlers", "[prefix[$:/plugins/tiddlywiki/tiddlydesktop-rs]]",
        "--deletetiddlers", "[prefix[$:/plugins/tiddlydesktop-rs]]",
        "--deletetiddlers", "[prefix[$:/temp/tiddlydesktop]]",
        "--savewikifolder", temp_output_str.as_ref(),
    ];

    eprintln!("[NodeBridge] Running TiddlyWiki --savewikifolder command");
    run_tiddlywiki_command(&args)?;

    // Verify conversion succeeded
    let tiddlywiki_info_path = temp_output.join("tiddlywiki.info");
    if !tiddlywiki_info_path.exists() {
        let _ = std::fs::remove_dir_all(&temp_dir);
        return Err("Conversion failed - tiddlywiki.info not created".to_string());
    }

    // Add tiddlyweb and filesystem plugins to tiddlywiki.info for proper folder wiki operation
    match add_server_plugins_to_info(&tiddlywiki_info_path) {
        Ok(()) => eprintln!("[NodeBridge] Added server plugins to tiddlywiki.info"),
        Err(e) => eprintln!("[NodeBridge] Warning: Failed to add server plugins: {}", e),
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

    // Prepare tiddlywiki.info: resolve includeWikis paths so rendering works
    let info_path = std::path::PathBuf::from(&local_path).join("tiddlywiki.info");
    if info_path.exists() {
        match prepare_info_for_android(&info_path) {
            Ok(()) => eprintln!("[NodeBridge] Prepared tiddlywiki.info for conversion"),
            Err(e) => eprintln!("[NodeBridge] Warning: Failed to prepare tiddlywiki.info: {}", e),
        }
    }

    // Create temp output directory
    let temp_output = android_temp_dir().join(format!("tw-convert-out-{}", std::process::id()));
    std::fs::create_dir_all(&temp_output)
        .map_err(|e| format!("Failed to create temp output dir: {}", e))?;

    let temp_output_str = temp_output.to_string_lossy();

    // Run TiddlyWiki render: tiddlywiki <folder> --deletetiddlers <plugins> --output <temp> --render '$:/core/save/all' wiki.html text/plain
    // We must remove:
    // - Server-only plugins (tiddlyweb, filesystem) that don't work in single-file wikis
    // - TiddlyDesktop-injected tiddlers that shouldn't be saved to standalone wikis
    let args = vec![
        local_path.as_str(),
        // Server-only plugins
        "--deletetiddlers", "$:/plugins/tiddlywiki/tiddlyweb",
        "--deletetiddlers", "$:/plugins/tiddlywiki/filesystem",
        // TiddlyDesktop injected plugin and tiddlers (filter for prefix match)
        "--deletetiddlers", "[prefix[$:/plugins/tiddlywiki/tiddlydesktop-rs]]",
        "--deletetiddlers", "[prefix[$:/plugins/tiddlydesktop-rs]]",
        "--deletetiddlers", "[prefix[$:/temp/tiddlydesktop]]",
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
