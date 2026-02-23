//! Attachment file sync via chunked transfer.
//!
//! When a tiddler with `_canonical_uri` is synced, the referenced attachment
//! file is also transferred. Files are chunked into 256KB pieces and
//! deduplicated via SHA-256 hash comparison.

use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use super::protocol::{SyncMessage, ATTACHMENT_CHUNK_SIZE};
use super::server::SyncServer;

/// Tracks in-progress attachment transfers
struct InProgressTransfer {
    wiki_id: String,
    filename: String,
    expected_sha256: Vec<u8>,
    expected_size: u64,
    chunk_count: u32,
    received_chunks: HashMap<u32, Vec<u8>>,
    target_path: PathBuf,
}

/// Manages attachment file sync
pub struct AttachmentManager {
    /// Base paths for wiki attachments (wiki_id → wiki base path)
    wiki_paths: Mutex<HashMap<String, PathBuf>>,
    /// On Android: SAF wiki file URIs (wiki_id → SAF URI of wiki file)
    #[cfg(target_os = "android")]
    wiki_saf_uris: Mutex<HashMap<String, String>>,
    /// In-progress inbound transfers
    transfers: Mutex<HashMap<String, InProgressTransfer>>,
    /// Paths recently written by incoming sync — suppresses watcher re-broadcast.
    /// Key: "wiki_id:rel_path", Value: time written.
    recently_received: Mutex<HashMap<String, Instant>>,
    /// Transfers skipped because the local file is already up-to-date (SHA-256 match).
    /// Incoming chunks for these are silently discarded.
    /// Key: "wiki_id:filename"
    skipped_transfers: Mutex<HashSet<String>>,
    /// On Android: cached attachment snapshots for periodic scan diffing.
    /// Key: wiki_id, Value: Vec of (rel_path, file_size) for each attachment.
    #[cfg(target_os = "android")]
    attachment_cache: Mutex<HashMap<String, Vec<(String, u64)>>>,
}

/// How long to suppress watcher events for a file after receiving it from sync
const SUPPRESS_DURATION: std::time::Duration = std::time::Duration::from_secs(5);

impl AttachmentManager {
    pub fn new() -> Self {
        Self {
            wiki_paths: Mutex::new(HashMap::new()),
            #[cfg(target_os = "android")]
            wiki_saf_uris: Mutex::new(HashMap::new()),
            transfers: Mutex::new(HashMap::new()),
            recently_received: Mutex::new(HashMap::new()),
            skipped_transfers: Mutex::new(HashSet::new()),
            #[cfg(target_os = "android")]
            attachment_cache: Mutex::new(HashMap::new()),
        }
    }

    /// Check if a file change should be suppressed (was recently received from sync)
    pub fn should_suppress(&self, wiki_id: &str, rel_path: &str) -> bool {
        let key = format!("{}:{}", wiki_id, rel_path);
        let mut map = self.recently_received.lock().unwrap();
        if let Some(ts) = map.get(&key) {
            if ts.elapsed() < SUPPRESS_DURATION {
                return true;
            }
            // Expired — clean up
            map.remove(&key);
        }
        false
    }

    /// Mark a file as recently received from sync (suppresses watcher for it)
    fn mark_received(&self, wiki_id: &str, rel_path: &str) {
        let key = format!("{}:{}", wiki_id, rel_path);
        self.recently_received.lock().unwrap().insert(key, Instant::now());
    }

    /// Register a wiki's base path for attachment resolution
    pub fn register_wiki_path(&self, wiki_id: &str, base_path: PathBuf) {
        self.wiki_paths
            .lock()
            .unwrap()
            .insert(wiki_id.to_string(), base_path);
    }

    /// Register a wiki's SAF URI for attachment resolution on Android
    #[cfg(target_os = "android")]
    pub fn register_wiki_saf_uri(&self, wiki_id: &str, saf_uri: String) {
        self.wiki_saf_uris
            .lock()
            .unwrap()
            .insert(wiki_id.to_string(), saf_uri);
    }

    /// Compare a fresh directory listing against the cached state.
    /// Returns (changed_entries, deleted_rel_paths).
    /// "Changed" means new or modified (size differs).
    #[cfg(target_os = "android")]
    pub fn diff_attachment_snapshot(
        &self,
        wiki_id: &str,
        fresh: &[(String, u64)],
    ) -> (Vec<String>, Vec<String>) {
        let cache = self.attachment_cache.lock().unwrap();
        let old = match cache.get(wiki_id) {
            Some(v) => v,
            None => {
                // No cache yet — everything is "new"
                let changed: Vec<String> = fresh.iter().map(|(p, _)| p.clone()).collect();
                return (changed, Vec::new());
            }
        };

        // Build lookup: rel_path → size
        let old_map: HashMap<&str, u64> = old.iter().map(|(p, s)| (p.as_str(), *s)).collect();
        let fresh_map: HashMap<&str, u64> = fresh.iter().map(|(p, s)| (p.as_str(), *s)).collect();

        // New or modified
        let mut changed = Vec::new();
        for (path, size) in fresh {
            match old_map.get(path.as_str()) {
                None => changed.push(path.clone()),
                Some(old_size) if *old_size != *size => changed.push(path.clone()),
                _ => {}
            }
        }

        // Deleted
        let deleted: Vec<String> = old
            .iter()
            .filter(|(p, _)| !fresh_map.contains_key(p.as_str()))
            .map(|(p, _)| p.clone())
            .collect();

        (changed, deleted)
    }

    /// Replace the cached attachment snapshot for a wiki.
    #[cfg(target_os = "android")]
    pub fn update_attachment_cache(&self, wiki_id: &str, entries: Vec<(String, u64)>) {
        self.attachment_cache
            .lock()
            .unwrap()
            .insert(wiki_id.to_string(), entries);
    }

    /// Check if a tiddler JSON contains a _canonical_uri field.
    /// Returns the URI if it's a relative path (not an external URL or absolute path).
    /// Handles both `./path/to/file` and `path/to/file` formats.
    pub fn extract_canonical_uri(tiddler_json: &str) -> Option<String> {
        if let Ok(tiddler) = serde_json::from_str::<serde_json::Value>(tiddler_json) {
            if let Some(uri) = tiddler.get("_canonical_uri").and_then(|v| v.as_str()) {
                if uri.is_empty() {
                    return None;
                }
                // Reject external URLs and absolute paths
                if uri.starts_with("http:") || uri.starts_with("https:")
                    || uri.starts_with("data:") || uri.starts_with("blob:")
                    || uri.starts_with("file:") || uri.starts_with("/")
                {
                    return None;
                }
                // Windows absolute paths (e.g., C:\...)
                if uri.len() >= 3 && uri.as_bytes().get(1) == Some(&b':') {
                    return None;
                }
                // Reject path traversal sequences
                if uri.contains("..") {
                    eprintln!("[LAN Sync] Security: Rejected _canonical_uri with path traversal: {}", uri);
                    return None;
                }
                // Also check percent-encoded traversal (%2e%2e, %2E%2E, etc.)
                if let Ok(decoded) = urlencoding::decode(uri) {
                    if decoded.contains("..") {
                        eprintln!("[LAN Sync] Security: Rejected _canonical_uri with encoded path traversal: {}", uri);
                        return None;
                    }
                }
                eprintln!("[LAN Sync] Found _canonical_uri in tiddler: {}", uri);
                return Some(uri.to_string());
            }
        }
        None
    }

    /// Get the full path for an attachment file.
    /// Validates that the resolved path stays within the wiki's base directory
    /// to prevent path traversal attacks from malicious sync peers.
    fn resolve_path(&self, wiki_id: &str, relative_path: &str) -> Option<PathBuf> {
        let paths = self.wiki_paths.lock().unwrap();
        let base = paths.get(wiki_id)?;
        let clean_path = relative_path.strip_prefix("./").unwrap_or(relative_path);

        // Reject obvious traversal before even joining
        if clean_path.contains("..") {
            eprintln!("[LAN Sync] Security: Rejected attachment path with traversal: {}", relative_path);
            return None;
        }

        let joined = base.join(clean_path);

        // Canonicalize and verify containment (catches symlink escapes)
        // For new files the parent must exist and be within base
        let canonical_base = dunce::canonicalize(base).unwrap_or_else(|_| base.clone());
        if let Ok(canonical) = dunce::canonicalize(&joined) {
            if !canonical.starts_with(&canonical_base) {
                eprintln!("[LAN Sync] Security: Attachment path escapes wiki dir: {} -> {}", relative_path, canonical.display());
                return None;
            }
            Some(canonical)
        } else if let Some(parent) = joined.parent() {
            // File doesn't exist yet — canonicalize parent and check
            if let Ok(canonical_parent) = dunce::canonicalize(parent) {
                if !canonical_parent.starts_with(&canonical_base) {
                    eprintln!("[LAN Sync] Security: Attachment parent escapes wiki dir: {} -> {}", relative_path, canonical_parent.display());
                    return None;
                }
            }
            Some(joined)
        } else {
            Some(joined)
        }
    }

    /// Compute SHA-256 hash of a file
    pub fn hash_file(path: &Path) -> Result<Vec<u8>, String> {
        let data = std::fs::read(path).map_err(|e| format!("Read failed: {}", e))?;
        let mut hasher = Sha256::new();
        hasher.update(&data);
        Ok(hasher.finalize().to_vec())
    }

    /// Prepare an attachment for sending: compute hash and stream chunks.
    /// Uses two passes to avoid loading the entire file into memory:
    /// 1. First pass: compute SHA-256 hash and file size
    /// 2. Second pass: stream chunks via bounded channel
    /// If `peers` is Some, sends only to those peers; otherwise broadcasts to all.
    pub async fn prepare_outbound(
        &self,
        wiki_id: &str,
        relative_path: &str,
        server: &SyncServer,
        peers: Option<&[String]>,
    ) -> Result<(), String> {
        use std::io::Read;

        let full_path = self
            .resolve_path(wiki_id, relative_path)
            .ok_or_else(|| format!("No base path for wiki {}", wiki_id))?;

        if !full_path.exists() {
            return Err(format!("Attachment not found: {:?}", full_path));
        }

        // First pass: compute SHA-256 hash and file size incrementally
        let (sha256, file_size) = {
            let mut hasher = Sha256::new();
            let mut file = std::fs::File::open(&full_path)
                .map_err(|e| format!("Open failed: {}", e))?;
            let mut buf = [0u8; 8192];
            let mut total = 0u64;
            loop {
                let n = file.read(&mut buf).map_err(|e| format!("Read failed: {}", e))?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
                total += n as u64;
            }
            (hasher.finalize().to_vec(), total)
        };

        let chunk_count =
            ((file_size as usize + ATTACHMENT_CHUNK_SIZE - 1) / ATTACHMENT_CHUNK_SIZE) as u32;

        // Send AttachmentChanged header
        let header_msg = SyncMessage::AttachmentChanged {
            wiki_id: wiki_id.to_string(),
            filename: relative_path.to_string(),
            file_size,
            sha256,
            chunk_count,
        };
        if let Some(peer_ids) = peers {
            server.send_to_peers(peer_ids, &header_msg).await;
        } else {
            server.broadcast(&header_msg).await;
        }

        // Second pass: stream chunks via bounded channel (max ~8MB buffered)
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(8);
        let read_path = full_path.clone();
        let chunk_size = ATTACHMENT_CHUNK_SIZE;
        let read_handle = std::thread::spawn(move || -> Result<(), String> {
            let mut file = std::fs::File::open(&read_path)
                .map_err(|e| format!("Open failed: {}", e))?;
            let mut buf = vec![0u8; chunk_size];
            loop {
                let n = file.read(&mut buf).map_err(|e| format!("Read error: {}", e))?;
                if n == 0 {
                    break;
                }
                use base64::Engine;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&buf[..n]);
                if tx.blocking_send(b64).is_err() {
                    break;
                }
            }
            Ok(())
        });

        let wid = wiki_id.to_string();
        let fname = relative_path.to_string();
        let peers_owned: Option<Vec<String>> = peers.map(|p| p.to_vec());
        let mut idx = 0u32;
        while let Some(b64) = rx.recv().await {
            let chunk_msg = SyncMessage::AttachmentChunk {
                wiki_id: wid.clone(),
                filename: fname.clone(),
                chunk_index: idx,
                data_base64: b64,
            };
            if let Some(ref peer_ids) = peers_owned {
                server.send_to_peers(peer_ids, &chunk_msg).await;
            } else {
                server.broadcast(&chunk_msg).await;
            }
            idx += 1;
        }

        // Wait for reader thread
        match read_handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err("Reader thread panicked".to_string()),
        }

        eprintln!(
            "[LAN Sync] Broadcast attachment {} ({} chunks)",
            relative_path, idx
        );
        Ok(())
    }

    /// Handle an incoming AttachmentChanged message
    pub fn handle_attachment_changed(
        &self,
        wiki_id: &str,
        filename: &str,
        file_size: u64,
        sha256: &[u8],
        chunk_count: u32,
    ) -> bool {
        let transfer_key = format!("{}:{}", wiki_id, filename);

        // Clear any previous skip marker for this file
        self.skipped_transfers.lock().unwrap().remove(&transfer_key);

        // Check if we already have this file with the same hash
        #[cfg(not(target_os = "android"))]
        {
            if let Some(path) = self.resolve_path(wiki_id, filename) {
                if path.exists() {
                    if let Ok(local_hash) = Self::hash_file(&path) {
                        if local_hash == sha256 {
                            eprintln!(
                                "[LAN Sync] Attachment '{}' already up-to-date (SHA-256 match)",
                                filename
                            );
                            self.skipped_transfers.lock().unwrap().insert(transfer_key);
                            return false; // Skip transfer
                        }
                    }
                }
            }
        }

        // On Android, check via SAF
        #[cfg(target_os = "android")]
        {
            if let Some(file_uri) = self.resolve_saf_attachment(wiki_id, filename) {
                use std::io::Read;
                if let Ok(mut reader) = crate::android::saf::open_document_reader(&file_uri) {
                    let mut hasher = Sha256::new();
                    let mut buf = [0u8; 8192];
                    loop {
                        match reader.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => hasher.update(&buf[..n]),
                            Err(_) => break,
                        }
                    }
                    let local_hash = hasher.finalize().to_vec();
                    if local_hash == sha256 {
                        eprintln!(
                            "[LAN Sync] Attachment '{}' already up-to-date (SHA-256 match)",
                            filename
                        );
                        self.skipped_transfers.lock().unwrap().insert(transfer_key);
                        return false;
                    }
                }
            }
        }

        let target_path = {
            #[cfg(not(target_os = "android"))]
            {
                match self.resolve_path(wiki_id, filename) {
                    Some(p) => p,
                    None => {
                        eprintln!(
                            "[LAN Sync] Cannot resolve path for attachment '{}' in wiki '{}' — wiki not registered",
                            filename, wiki_id
                        );
                        let registered: Vec<String> = self.wiki_paths.lock().unwrap().keys().cloned().collect();
                        eprintln!("[LAN Sync] Registered wiki IDs: {:?}", registered);
                        return false;
                    },
                }
            }
            #[cfg(target_os = "android")]
            {
                // On Android we don't use target_path for writing (use SAF instead)
                // but we need a placeholder for the struct
                if self.wiki_saf_uris.lock().unwrap().contains_key(wiki_id) {
                    PathBuf::from(filename)
                } else {
                    eprintln!("[LAN Sync] No SAF URI registered for wiki {}", wiki_id);
                    return false;
                }
            }
        };

        let mut transfers = self.transfers.lock().unwrap();
        transfers.insert(
            transfer_key,
            InProgressTransfer {
                wiki_id: wiki_id.to_string(),
                filename: filename.to_string(),
                expected_sha256: sha256.to_vec(),
                expected_size: file_size,
                chunk_count,
                received_chunks: HashMap::new(),
                target_path,
            },
        );

        true // Need chunks
    }

    /// Handle an incoming AttachmentChunk message
    pub fn handle_attachment_chunk(
        &self,
        wiki_id: &str,
        filename: &str,
        chunk_index: u32,
        data_base64: &str,
    ) -> Result<bool, String> {
        let transfer_key = format!("{}:{}", wiki_id, filename);

        // Silently discard chunks for transfers we skipped (file already up-to-date)
        if self.skipped_transfers.lock().unwrap().contains(&transfer_key) {
            return Ok(false);
        }

        let mut transfers = self.transfers.lock().unwrap();

        let transfer = transfers
            .get_mut(&transfer_key)
            .ok_or_else(|| "No in-progress transfer".to_string())?;

        let chunk_data = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            data_base64,
        )
        .map_err(|e| format!("Base64 decode failed: {}", e))?;

        transfer.received_chunks.insert(chunk_index, chunk_data);

        // Check if all chunks received
        if transfer.received_chunks.len() as u32 == transfer.chunk_count {
            // Reassemble file
            let mut data = Vec::with_capacity(transfer.expected_size as usize);
            for i in 0..transfer.chunk_count {
                if let Some(chunk) = transfer.received_chunks.get(&i) {
                    data.extend_from_slice(chunk);
                } else {
                    return Err(format!("Missing chunk {}", i));
                }
            }

            // Verify SHA-256
            let mut hasher = Sha256::new();
            hasher.update(&data);
            let actual_hash = hasher.finalize().to_vec();
            if actual_hash != transfer.expected_sha256 {
                return Err("SHA-256 mismatch after reassembly".to_string());
            }

            let wiki_id_owned = wiki_id.to_string();
            let filename_owned = filename.to_string();
            transfers.remove(&transfer_key);

            // Write file — platform-specific
            #[cfg(not(target_os = "android"))]
            {
                let target = self.resolve_path(&wiki_id_owned, &filename_owned)
                    .ok_or_else(|| "No base path registered".to_string())?;
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("Create dir failed: {}", e))?;
                }
                std::fs::write(&target, &data)
                    .map_err(|e| format!("Write attachment failed: {}", e))?;
            }

            #[cfg(target_os = "android")]
            {
                self.write_attachment_via_saf(&wiki_id_owned, &filename_owned, &data)?;
            }

            // Suppress watcher events for this file (avoid re-broadcasting)
            self.mark_received(&wiki_id_owned, &filename_owned);

            eprintln!(
                "[LAN Sync] Attachment '{}' received ({} bytes)",
                filename_owned,
                data.len()
            );

            return Ok(true); // Transfer complete
        }

        Ok(false) // More chunks needed
    }

    /// Handle an incoming AttachmentDeleted message
    pub fn handle_attachment_deleted(&self, wiki_id: &str, filename: &str) -> Result<(), String> {
        #[cfg(not(target_os = "android"))]
        {
            if let Some(path) = self.resolve_path(wiki_id, filename) {
                if path.exists() {
                    std::fs::remove_file(&path)
                        .map_err(|e| format!("Delete attachment failed: {}", e))?;
                    self.mark_received(wiki_id, filename);
                    eprintln!("[LAN Sync] Attachment deleted: {}", filename);
                }
            }
        }

        #[cfg(target_os = "android")]
        {
            if let Some(file_uri) = self.resolve_saf_attachment(wiki_id, filename) {
                if let Err(e) = crate::android::saf::delete_document(&file_uri) {
                    return Err(format!("SAF delete failed: {}", e));
                }
                self.mark_received(wiki_id, filename);
                eprintln!("[LAN Sync] Attachment deleted (SAF): {}", filename);
            }
        }

        Ok(())
    }

    /// Resolve a relative attachment path to a SAF URI on Android.
    /// E.g., "attachments/image.png" → content:// URI of the file
    #[cfg(target_os = "android")]
    fn resolve_saf_attachment(&self, wiki_id: &str, relative_path: &str) -> Option<String> {
        let uris = self.wiki_saf_uris.lock().unwrap();
        let wiki_uri = uris.get(wiki_id)?;
        let wiki_uri = wiki_uri.clone();
        drop(uris);

        let clean_path = relative_path.strip_prefix("./").unwrap_or(relative_path);
        let components: Vec<&str> = clean_path.split('/').collect();
        if components.is_empty() {
            return None;
        }

        // Get parent directory of wiki file
        let parent_uri = crate::android::saf::get_parent_uri(&wiki_uri).ok()?;

        // Navigate directory components
        let mut current_uri = parent_uri;
        for i in 0..components.len().saturating_sub(1) {
            match crate::android::saf::find_subdirectory(&current_uri, components[i]) {
                Ok(Some(uri)) => current_uri = uri,
                _ => return None,
            }
        }

        // Find the file in the final directory
        let file_name = components.last()?;
        let entries = crate::android::saf::list_directory_entries(&current_uri).ok()?;
        for entry in entries {
            if entry.name == *file_name && !entry.is_dir {
                return Some(entry.uri);
            }
        }

        None
    }

    /// Write attachment data to the correct location via SAF on Android.
    /// Creates the directory structure if needed, and creates or overwrites the file.
    #[cfg(target_os = "android")]
    fn write_attachment_via_saf(
        &self,
        wiki_id: &str,
        filename: &str,
        data: &[u8],
    ) -> Result<(), String> {
        let uris = self.wiki_saf_uris.lock().unwrap();
        let wiki_uri = match uris.get(wiki_id) {
            Some(u) => u.clone(),
            None => return Err(format!("No SAF URI registered for wiki {}", wiki_id)),
        };
        drop(uris);

        let clean_path = filename.strip_prefix("./").unwrap_or(filename);
        let components: Vec<&str> = clean_path.split('/').collect();
        if components.is_empty() {
            return Err("Empty filename".to_string());
        }

        // Get parent directory of wiki file
        let parent_uri = crate::android::saf::get_parent_uri(&wiki_uri)?;

        // Navigate/create directory components
        let mut current_uri = parent_uri;
        for i in 0..components.len().saturating_sub(1) {
            current_uri =
                crate::android::saf::find_or_create_subdirectory(&current_uri, components[i])?;
        }

        // Check if file already exists (overwrite) or create new
        let file_name = components.last().unwrap();
        let existing_uri = crate::android::saf::list_directory_entries(&current_uri)
            .ok()
            .and_then(|entries| {
                entries
                    .into_iter()
                    .find(|e| e.name == *file_name && !e.is_dir)
                    .map(|e| e.uri)
            });

        let file_uri = match existing_uri {
            Some(uri) => uri,
            None => crate::android::saf::create_file(&current_uri, file_name, None)?,
        };

        crate::android::saf::write_document_bytes(&file_uri, data)?;
        eprintln!(
            "[LAN Sync] Wrote attachment via SAF: {} ({} bytes)",
            filename,
            data.len()
        );
        Ok(())
    }
}

// ── Attachment Directory Watcher (Desktop only) ─────────────────────────────

/// Events from the attachment directory watcher
#[cfg(not(target_os = "android"))]
#[derive(Debug)]
pub enum AttachmentEvent {
    /// A file was created or modified in the attachments directory
    Changed { wiki_id: String, rel_path: String },
    /// A file was deleted from the attachments directory
    Deleted { wiki_id: String, rel_path: String },
}

/// Watches attachment directories for file changes and sends events
/// to a channel for processing by the sync system.
#[cfg(not(target_os = "android"))]
pub struct AttachmentWatcher {
    _watcher: notify::RecommendedWatcher,
}

#[cfg(not(target_os = "android"))]
impl AttachmentWatcher {
    /// Start watching attachment directories for all sync-enabled wikis.
    ///
    /// `wiki_dirs`: Vec of (wiki_id, wiki_parent_dir, attachments_dir)
    /// `event_tx`: Channel to send debounced attachment events
    pub fn start(
        wiki_dirs: Vec<(String, PathBuf, PathBuf)>,
        event_tx: tokio::sync::mpsc::UnboundedSender<AttachmentEvent>,
    ) -> Result<Self, String> {
        use notify::{Config, RecursiveMode, Watcher};
        use std::time::Duration;

        // Map: attachments_dir → (wiki_id, wiki_parent_dir)
        let dir_map: std::sync::Arc<HashMap<PathBuf, (String, PathBuf)>> =
            std::sync::Arc::new(
                wiki_dirs
                    .iter()
                    .map(|(wid, parent, att_dir)| (att_dir.clone(), (wid.clone(), parent.clone())))
                    .collect(),
            );

        let (notify_tx, notify_rx) = std::sync::mpsc::channel();
        let mut watcher = notify::RecommendedWatcher::new(notify_tx, Config::default())
            .map_err(|e| format!("Failed to create file watcher: {}", e))?;

        for (_, _, att_dir) in &wiki_dirs {
            if att_dir.is_dir() {
                watcher
                    .watch(att_dir, RecursiveMode::Recursive)
                    .map_err(|e| format!("Watch failed for {}: {}", att_dir.display(), e))?;
                eprintln!(
                    "[LAN Sync] Watching attachments: {}",
                    att_dir.display()
                );
            }
        }

        // Spawn a thread to process notify events with debouncing.
        // Uses a pending map: on each event, store/update the entry.
        // Every 100ms, flush entries older than 500ms.
        let dm = dir_map;
        std::thread::spawn(move || {
            use std::sync::mpsc::RecvTimeoutError;

            // pending: path → (event, timestamp of last update)
            let mut pending: HashMap<PathBuf, (AttachmentEvent, Instant)> = HashMap::new();
            let debounce = Duration::from_millis(500);
            let poll = Duration::from_millis(100);

            loop {
                // Receive events with timeout
                match notify_rx.recv_timeout(poll) {
                    Ok(Ok(event)) => {
                        use notify::EventKind;
                        let is_change = matches!(
                            event.kind,
                            EventKind::Create(_) | EventKind::Modify(_)
                        );
                        let is_delete = matches!(event.kind, EventKind::Remove(_));

                        if !is_change && !is_delete {
                            // Flush mature entries below
                        } else {
                            for path in &event.paths {
                                // For changes, skip directories
                                if is_change && !path.is_file() {
                                    continue;
                                }
                                // Find which wiki this belongs to
                                for (att_dir, (wiki_id, parent_dir)) in dm.iter() {
                                    if path.starts_with(att_dir) {
                                        let rel_path = path
                                            .strip_prefix(parent_dir)
                                            .unwrap_or(path)
                                            .to_string_lossy()
                                            .to_string();

                                        let evt = if is_change {
                                            AttachmentEvent::Changed {
                                                wiki_id: wiki_id.clone(),
                                                rel_path,
                                            }
                                        } else {
                                            AttachmentEvent::Deleted {
                                                wiki_id: wiki_id.clone(),
                                                rel_path,
                                            }
                                        };
                                        pending.insert(path.clone(), (evt, Instant::now()));
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        eprintln!("[LAN Sync] Watcher error: {}", e);
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }

                // Flush mature pending entries (older than debounce duration)
                let now = Instant::now();
                let ready: Vec<PathBuf> = pending
                    .iter()
                    .filter(|(_, (_, ts))| now.duration_since(*ts) >= debounce)
                    .map(|(k, _)| k.clone())
                    .collect();

                for key in ready {
                    if let Some((evt, _)) = pending.remove(&key) {
                        // Final check: for Changed events, verify file still exists
                        let should_send = match &evt {
                            AttachmentEvent::Changed { .. } => key.is_file(),
                            AttachmentEvent::Deleted { .. } => true,
                        };
                        if should_send {
                            if event_tx.send(evt).is_err() {
                                return; // Receiver dropped
                            }
                        }
                    }
                }
            }

            eprintln!("[LAN Sync] Attachment watcher thread stopped");
        });

        Ok(Self { _watcher: watcher })
    }
}
