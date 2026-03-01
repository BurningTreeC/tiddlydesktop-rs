//! LAN Sync module — real-time tiddler synchronization across devices on the same network.
//!
//! This module provides:
//! - Encrypted WebSocket connections between devices (ChaCha20-Poly1305)
//! - Room-based authentication (shared room code + password)
//! - UDP broadcast discovery of peers on the LAN
//! - Vector clock-based conflict resolution
//! - Chunked attachment file transfer
//!
//! Architecture:
//! - One sync server per device in the main Tauri process
//! - All wikis multiplexed over one WebSocket per peer
//! - Desktop: bridges to wiki windows via IPC
//! - Android: bridges to :wiki process via HTTP


pub mod attachments;
#[cfg(target_os = "android")]
pub mod android_bridge;
pub mod bridge;
pub mod client;
pub mod conflict;
pub mod discovery;
pub mod pairing;
pub mod protocol;
pub mod server;
pub mod wiki_info;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use tokio::sync::{mpsc, RwLock};

use self::attachments::AttachmentManager;
use self::bridge::{SyncBridge, SyncToWiki, WikiToSync};
use self::conflict::ConflictManager;
use self::discovery::{DiscoveryEvent, DiscoveryManager};
use self::pairing::PairingManager;
use self::protocol::SyncMessage;
use self::server::{PeerConnection, ServerEvent, SyncServer};

use crate::relay_sync::RelaySyncManager;
use crate::GLOBAL_APP_HANDLE;
use tauri::{Emitter, Manager};

/// Global sync manager instance
static SYNC_MANAGER: OnceLock<Arc<SyncManager>> = OnceLock::new();

/// IPC client for wiki processes on desktop (set by lib.rs when running in wiki mode).
/// Used by Tauri commands to route LAN sync messages to the main process.
#[cfg(not(target_os = "android"))]
static IPC_CLIENT_FOR_SYNC: OnceLock<Arc<std::sync::Mutex<Option<crate::ipc::IpcClient>>>> =
    OnceLock::new();

/// Queue for LAN sync messages received via IPC in wiki processes (desktop only).
/// JS polls this queue via the `lan_sync_poll_ipc` Tauri command.
#[cfg(not(target_os = "android"))]
static IPC_SYNC_QUEUE: OnceLock<std::sync::Mutex<Vec<String>>> = OnceLock::new();

/// Push a LAN sync payload to the IPC queue for JS to poll.
#[cfg(not(target_os = "android"))]
pub fn queue_lan_sync_ipc(payload_json: String) {
    let queue = IPC_SYNC_QUEUE.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    queue.lock().unwrap().push(payload_json);
}

/// Per-wiki sync configuration stored in wiki configs
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct WikiSyncConfig {
    /// Whether sync is enabled for this wiki
    pub enabled: bool,
    /// The wiki's unique sync ID (UUID)
    pub wiki_id: Option<String>,
}

/// Status of the LAN sync system
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncStatus {
    pub running: bool,
    pub device_id: String,
    pub device_name: String,
    pub port: Option<u16>,
    pub connected_peers: Vec<PeerInfo>,
    pub relay_connected: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PeerInfo {
    pub device_id: String,
    pub device_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
}

/// Info about a wiki available from a remote peer
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RemoteWikiInfo {
    pub wiki_id: String,
    pub wiki_name: String,
    pub is_folder: bool,
    pub from_device_id: String,
    pub from_device_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room_code: Option<String>,
}

/// The main sync manager that coordinates all sync components
pub struct SyncManager {
    data_dir: std::path::PathBuf,
    pairing_manager: Arc<PairingManager>,
    conflict_manager: Arc<ConflictManager>,
    attachment_manager: Arc<AttachmentManager>,
    bridge: Arc<SyncBridge>,
    /// Fast flag checked by the event loop to skip messages after stop()
    running: std::sync::atomic::AtomicBool,
    server: RwLock<Option<SyncServer>>,
    /// Relay sync manager for cross-network sync
    relay_manager: Option<Arc<RelaySyncManager>>,
    discovery: RwLock<Option<DiscoveryManager>>,
    /// Channel sender for wiki-to-sync messages
    wiki_tx: mpsc::UnboundedSender<WikiToSync>,
    /// Server event sender
    event_tx: mpsc::UnboundedSender<ServerEvent>,
    /// Event receivers (consumed once when event loop starts)
    event_rx: tokio::sync::Mutex<Option<mpsc::UnboundedReceiver<ServerEvent>>>,
    wiki_rx: tokio::sync::Mutex<Option<mpsc::UnboundedReceiver<WikiToSync>>>,
    /// Connected peers — shared between server (inbound) and client (outbound) connections
    peers: Arc<RwLock<HashMap<String, PeerConnection>>>,
    /// Device IDs with active WebSocket connections (shared with discovery thread
    /// to prevent timing out peers that are actively connected)
    connected_peer_ids: Arc<std::sync::RwLock<HashSet<String>>>,
    /// Room code → group key (shared with SyncServer for LAN room auth)
    room_keys: Arc<RwLock<HashMap<String, [u8; 32]>>>,
    /// Active room codes we're in (shared with discovery for beacon broadcast)
    active_room_codes: Arc<std::sync::RwLock<Vec<String>>>,
    /// Peer device_id → room codes from their discovery beacons (cleartext, for old peers)
    peer_rooms: Arc<RwLock<HashMap<String, Vec<String>>>>,
    /// Peer device_id → room code hashes from their discovery beacons (preferred for matching)
    peer_room_hashes: Arc<RwLock<HashMap<String, Vec<String>>>>,
    /// Remote wikis available from connected peers (device_id → wiki list)
    remote_wikis: RwLock<HashMap<String, Vec<protocol::WikiInfo>>>,
    /// Attachment directory watcher (desktop only)
    #[cfg(not(target_os = "android"))]
    attachment_watcher: RwLock<Option<attachments::AttachmentWatcher>>,
    /// Channel for attachment watcher events (desktop only)
    #[cfg(not(target_os = "android"))]
    attachment_event_rx: tokio::sync::Mutex<Option<mpsc::UnboundedReceiver<attachments::AttachmentEvent>>>,
    /// Incoming wiki file transfers: (wiki_id → accumulated chunks)
    incoming_transfers: RwLock<HashMap<String, WikiTransferState>>,
    /// Last-known addresses for paired peers (device_id → (addr, port))
    last_known_addrs: RwLock<HashMap<String, (String, u16)>>,
    /// Pending fingerprint requests: (wiki_id, device_id) → request time.
    /// If no response within 15s, skip and log warning.
    pending_fingerprint_requests: RwLock<HashMap<(String, String), std::time::Instant>>,
    /// Active reconnection tasks (device_id → abort handle)
    reconnect_tasks: RwLock<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// Cached local fingerprints per wiki (wiki_id → fingerprints).
    /// Updated every time JS sends fingerprints. Used by pre_request_sync to
    /// send cached fingerprints to peers before JS boots, enabling pre-boot sync.
    local_fingerprint_cache: std::sync::Mutex<HashMap<String, Vec<protocol::TiddlerFingerprint>>>,
    /// Buffer for sync messages targeting wikis whose JS hasn't connected yet.
    /// Maps wiki_id → list of JSON payloads. Populated by emit_to_wiki when
    /// pre_request_sync has been called. Drained by on_wiki_opened.
    #[cfg(not(target_os = "android"))]
    pre_sync_buffer: std::sync::Mutex<HashMap<String, Vec<String>>>,
    /// Always-on buffer for apply-change/apply-deletion events that arrive
    /// when no wiki window is open.  Unlike pre_sync_buffer (which only
    /// captures events after pre_request_sync), this captures tiddler data
    /// from TiddlerChanged/FullSyncBatch that would otherwise be lost.
    /// Drained by on_wiki_opened so changes appear instantly without a
    /// round-trip to the peer.
    #[cfg(not(target_os = "android"))]
    pending_wiki_changes: std::sync::Mutex<HashMap<String, Vec<String>>>,
    /// Deferred vector clocks for buffered tiddler changes.
    /// When emit_to_wiki buffers a message (0 IPC clients), the vector clock
    /// is stored here. Drained by on_wiki_opened after buffer delivery.
    /// Maps wiki_id → Vec<(title, clock, is_deletion)>.
    #[cfg(not(target_os = "android"))]
    pending_wiki_clocks: std::sync::Mutex<HashMap<String, Vec<(String, protocol::VectorClock, bool)>>>,
    /// Tracks tiddler titles that were merged into local_fingerprint_cache
    /// from FullSyncBatch while no wiki JS was connected.  These tiddlers
    /// exist in the Rust cache but NOT in the wiki file, so they must be
    /// excluded when sending cached fingerprints to peers (otherwise the
    /// peer thinks we already have them and skips the diff).
    /// Cleared when JS sends real fingerprints (which become the new cache).
    cache_merge_overrides: std::sync::Mutex<HashMap<String, std::collections::HashSet<String>>>,
    /// Deduplication: last time we sent fingerprints to (wiki_id, device_id).
    /// Suppresses redundant sends within a 3-second window.
    last_fp_send: std::sync::Mutex<HashMap<(String, String), std::time::Instant>>,
    /// Deduplication: last time we forwarded a peer's fingerprints to JS for (wiki_id, from_device_id).
    /// Suppresses redundant compare-fingerprints forwards within a 3-second window.
    last_fp_forward: std::sync::Mutex<HashMap<(String, String), std::time::Instant>>,
    /// Cached tiddlywiki.info content per wiki (wiki_id → (content_json, content_hash, timestamp))
    /// Only populated for folder wikis. Updated on wiki open and when remote changes arrive.
    wiki_info_cache: std::sync::Mutex<HashMap<String, (String, String, u64)>>,
    /// Incoming plugin file transfers: (wiki_id, plugin_name) → accumulated file data
    incoming_plugin_transfers: std::sync::Mutex<HashMap<(String, String), HashMap<String, Vec<u8>>>>,
    /// Android bridge for cross-process communication with :wiki process
    #[cfg(target_os = "android")]
    android_bridge: std::sync::Mutex<Option<android_bridge::AndroidBridge>>,

    // ── Collaborative editing state ──────────────────────────────────

    /// Remote editors: (wiki_id, tiddler_title) → HashSet<(device_id, device_name, user_name)>
    collab_editors: std::sync::Mutex<HashMap<(String, String), HashSet<(String, String, String)>>>,
    /// Local editors: (wiki_id, tiddler_title) — tiddlers this device is currently editing
    local_collab_editors: std::sync::Mutex<HashSet<(String, String)>>,
    /// Local TiddlyWiki username (from JS UserNameAnnounce)
    local_user_name: std::sync::Mutex<Option<String>>,
    /// Remote peer usernames: device_id → user_name (works for both LAN and relay peers)
    peer_user_names: std::sync::Mutex<HashMap<String, String>>,
    /// Port of the local collab WebSocket server (0 if not running)
    collab_ws_port: std::sync::atomic::AtomicU16,
    /// Connected collab WebSocket clients: wiki_id → list of sender handles
    collab_ws_clients: std::sync::Mutex<HashMap<String, Vec<tokio::sync::mpsc::UnboundedSender<String>>>>,
    /// Shutdown signal for collab WebSocket server
    collab_ws_shutdown: tokio::sync::watch::Sender<bool>,
    /// Last inbound awareness sender per tiddler: (wiki_id, tiddler_title) → (device_id, timestamp)
    /// Used to suppress echo-back: when JS re-sends awareness we just received, we exclude the original sender
    awareness_last_inbound: std::sync::Mutex<HashMap<(String, String), (String, std::time::Instant)>>,
}

/// State for an incoming wiki file transfer
pub struct WikiTransferState {
    pub wiki_name: String,
    pub is_folder: bool,
    /// Target directory chosen by the user
    pub target_dir: String,
    /// Files that have been fully written to disk: filename → disk path
    pub written_files: Vec<(String, std::path::PathBuf)>,
    /// Currently open file being written to (for streaming chunks to disk)
    current_file: Option<(String, std::fs::File)>,
    pub chunks_received: u32,
    /// Room to assign after transfer completes (passed from UI)
    pub room_code: Option<String>,
}

impl SyncManager {
    /// Initialize the sync manager (called once at app startup)
    pub fn init(data_dir: &std::path::Path) -> Arc<Self> {
        let identity = pairing::load_or_create_device_identity(data_dir);
        eprintln!(
            "[LAN Sync] Device identity: {} ({})",
            identity.device_name, identity.device_id
        );

        let pairing_manager = Arc::new(PairingManager::new(
            identity.device_id.clone(),
            identity.device_name.clone(),
            identity.display_name.clone(),
            data_dir.to_path_buf(),
        ));

        let conflict_manager = Arc::new(ConflictManager::new(
            identity.device_id.clone(),
            data_dir,
        ));

        let attachment_manager = Arc::new(AttachmentManager::new());

        let (bridge, wiki_rx) = SyncBridge::new();
        let wiki_tx = bridge.wiki_tx.clone();
        let bridge = Arc::new(bridge);

        let (event_tx, event_rx) = mpsc::unbounded_channel();

        // Create relay sync manager (uses same event channel for unified event loop)
        let relay_manager = Arc::new(RelaySyncManager::new(
            data_dir,
            pairing_manager.clone(),
            event_tx.clone(),
        ));

        // Load persisted fingerprint cache from disk
        let fingerprint_cache = Self::load_fingerprint_cache(data_dir);
        if !fingerprint_cache.is_empty() {
            eprintln!(
                "[LAN Sync] Loaded fingerprint cache for {} wikis from disk",
                fingerprint_cache.len()
            );
        }

        let manager = Arc::new(Self {
            data_dir: data_dir.to_path_buf(),
            pairing_manager,
            conflict_manager,
            attachment_manager,
            bridge,
            running: std::sync::atomic::AtomicBool::new(false),
            server: RwLock::new(None),
            relay_manager: Some(relay_manager),
            discovery: RwLock::new(None),
            wiki_tx,
            event_tx,
            event_rx: tokio::sync::Mutex::new(Some(event_rx)),
            wiki_rx: tokio::sync::Mutex::new(Some(wiki_rx)),
            peers: Arc::new(RwLock::new(HashMap::new())),
            connected_peer_ids: Arc::new(std::sync::RwLock::new(HashSet::new())),
            room_keys: Arc::new(RwLock::new(HashMap::new())),
            active_room_codes: Arc::new(std::sync::RwLock::new(Vec::new())),
            peer_rooms: Arc::new(RwLock::new(HashMap::new())),
            peer_room_hashes: Arc::new(RwLock::new(HashMap::new())),
            remote_wikis: RwLock::new(HashMap::new()),
            #[cfg(not(target_os = "android"))]
            attachment_watcher: RwLock::new(None),
            #[cfg(not(target_os = "android"))]
            attachment_event_rx: tokio::sync::Mutex::new(None),
            incoming_transfers: RwLock::new(HashMap::new()),
            last_known_addrs: RwLock::new(HashMap::new()),
            reconnect_tasks: RwLock::new(HashMap::new()),
            pending_fingerprint_requests: RwLock::new(HashMap::new()),
            local_fingerprint_cache: std::sync::Mutex::new(fingerprint_cache),
            #[cfg(not(target_os = "android"))]
            pre_sync_buffer: std::sync::Mutex::new(HashMap::new()),
            #[cfg(not(target_os = "android"))]
            pending_wiki_changes: std::sync::Mutex::new(HashMap::new()),
            #[cfg(not(target_os = "android"))]
            pending_wiki_clocks: std::sync::Mutex::new(HashMap::new()),
            cache_merge_overrides: std::sync::Mutex::new(HashMap::new()),
            last_fp_send: std::sync::Mutex::new(HashMap::new()),
            last_fp_forward: std::sync::Mutex::new(HashMap::new()),
            wiki_info_cache: std::sync::Mutex::new(HashMap::new()),
            incoming_plugin_transfers: std::sync::Mutex::new(HashMap::new()),
            #[cfg(target_os = "android")]
            android_bridge: std::sync::Mutex::new(None),
            local_user_name: std::sync::Mutex::new(None),
            peer_user_names: std::sync::Mutex::new(HashMap::new()),
            collab_editors: std::sync::Mutex::new(HashMap::new()),
            local_collab_editors: std::sync::Mutex::new(HashSet::new()),
            collab_ws_port: std::sync::atomic::AtomicU16::new(0),
            collab_ws_clients: std::sync::Mutex::new(HashMap::new()),
            collab_ws_shutdown: tokio::sync::watch::channel(false).0,
            awareness_last_inbound: std::sync::Mutex::new(HashMap::new()),
        });

        // Store globally
        let _ = SYNC_MANAGER.set(manager.clone());

        manager
    }

    /// Get a reference to the pairing manager
    pub fn pairing_manager(&self) -> &PairingManager {
        &self.pairing_manager
    }

    /// Load fingerprint cache from disk
    fn load_fingerprint_cache(
        data_dir: &std::path::Path,
    ) -> HashMap<String, Vec<protocol::TiddlerFingerprint>> {
        let cache_file = data_dir.join("sync-fingerprints.json");
        match std::fs::read_to_string(&cache_file) {
            Ok(json) => {
                serde_json::from_str(&json).unwrap_or_default()
            }
            Err(_) => HashMap::new(),
        }
    }

    /// Save fingerprint cache to disk (called from background thread)
    fn save_fingerprint_cache_to_disk(
        data_dir: &std::path::Path,
        cache: &HashMap<String, Vec<protocol::TiddlerFingerprint>>,
    ) {
        let cache_file = data_dir.join("sync-fingerprints.json");
        match serde_json::to_string(cache) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&cache_file, json) {
                    eprintln!("[LAN Sync] Failed to save fingerprint cache: {}", e);
                }
            }
            Err(e) => {
                eprintln!("[LAN Sync] Failed to serialize fingerprint cache: {}", e);
            }
        }
    }

    /// Get cached local fingerprints for a wiki, excluding any titles that
    /// were merged from FullSyncBatch while no wiki JS was connected.
    /// These "override" titles exist in the cache but NOT in the wiki file,
    /// so sending them would falsely tell the peer we already have them.
    fn get_accurate_cached_fingerprints(&self, wiki_id: &str) -> Option<Vec<protocol::TiddlerFingerprint>> {
        let cache = self.local_fingerprint_cache.lock().ok()?;
        let fps = cache.get(wiki_id)?.clone();
        let overrides = self.cache_merge_overrides.lock().ok()
            .and_then(|o| o.get(wiki_id).cloned());
        if let Some(override_titles) = overrides {
            if !override_titles.is_empty() {
                let filtered: Vec<_> = fps.into_iter()
                    .filter(|f| !override_titles.contains(&f.title))
                    .collect();
                return Some(filtered);
            }
        }
        Some(fps)
    }

    /// Deduplication: check if we recently sent fingerprints to this peer for this wiki.
    /// Returns true if we should skip (sent within last 3s). Records the send if not skipped.
    fn dedup_fp_send(&self, wiki_id: &str, device_id: &str) -> bool {
        let key = (wiki_id.to_string(), device_id.to_string());
        let now = std::time::Instant::now();
        if let Ok(mut map) = self.last_fp_send.lock() {
            if let Some(last) = map.get(&key) {
                if now.duration_since(*last).as_secs() < 3 {
                    return true; // skip — sent recently
                }
            }
            map.insert(key, now);
        }
        false
    }

    /// Deduplication: check if we recently forwarded this peer's fingerprints to JS.
    /// Returns true if we should skip (forwarded within last 3s). Records the forward if not skipped.
    fn dedup_fp_forward(&self, wiki_id: &str, from_device_id: &str) -> bool {
        let key = (wiki_id.to_string(), from_device_id.to_string());
        let now = std::time::Instant::now();
        if let Ok(mut map) = self.last_fp_forward.lock() {
            if let Some(last) = map.get(&key) {
                if now.duration_since(*last).as_secs() < 3 {
                    return true; // skip — forwarded recently
                }
            }
            map.insert(key, now);
        }
        false
    }

    /// Update fingerprint cache and persist to disk in background.
    /// Also clears cache_merge_overrides since JS fingerprints are the
    /// source of truth (JS sent them, so they reflect what's in the file).
    fn update_fingerprint_cache(&self, wiki_id: &str, fingerprints: Vec<protocol::TiddlerFingerprint>) {
        // Clear overrides — JS fingerprints are authoritative
        if let Ok(mut overrides) = self.cache_merge_overrides.lock() {
            overrides.remove(wiki_id);
        }
        let cache_snapshot = if let Ok(mut cache) = self.local_fingerprint_cache.lock() {
            // Skip disk write if fingerprints haven't changed
            if let Some(existing) = cache.get(wiki_id) {
                if *existing == fingerprints {
                    return;
                }
            }
            cache.insert(wiki_id.to_string(), fingerprints);
            cache.clone()
        } else {
            return;
        };
        // Persist to disk in a background thread to avoid blocking
        let data_dir = self.data_dir.clone();
        std::thread::spawn(move || {
            Self::save_fingerprint_cache_to_disk(&data_dir, &cache_snapshot);
        });
    }

    /// Remove a wiki's fingerprint cache entry and persist to disk.
    /// Called when a wiki is removed from the recent files list.
    pub fn remove_fingerprint_cache(&self, wiki_id: &str) {
        if let Ok(mut overrides) = self.cache_merge_overrides.lock() {
            overrides.remove(wiki_id);
        }
        let cache_snapshot = if let Ok(mut cache) = self.local_fingerprint_cache.lock() {
            if cache.remove(wiki_id).is_some() {
                cache.clone()
            } else {
                return;
            }
        } else {
            return;
        };
        let data_dir = self.data_dir.clone();
        std::thread::spawn(move || {
            Self::save_fingerprint_cache_to_disk(&data_dir, &cache_snapshot);
        });
    }

    /// Start the background event loop and auto-connect relay rooms.
    /// Called once at init time — does NOT start LAN sync server/discovery.
    /// Persists in-memory config to disk before starting sync to ensure
    /// on-disk state is authoritative (guards against stale config after crash).
    pub async fn start_background(&self, app: Option<tauri::AppHandle>) {
        // Start the event processing loop (only once — takes ownership of receivers)
        let event_rx = self.event_rx.lock().await.take();
        let wiki_rx = self.wiki_rx.lock().await.take();
        #[cfg(not(target_os = "android"))]
        let att_rx = self.attachment_event_rx.lock().await.take();
        if let (Some(erx), Some(wrx)) = (event_rx, wiki_rx) {
            let mgr = get_sync_manager().unwrap();
            #[cfg(not(target_os = "android"))]
            let att_rx = att_rx;
            tokio::spawn(async move {
                #[cfg(not(target_os = "android"))]
                {
                    mgr.run_event_loop(erx, wrx, att_rx).await;
                }
                #[cfg(target_os = "android")]
                {
                    mgr.run_event_loop(erx, wrx).await;
                }
            });
        }

        // Start Android bridge early so relay-only sync can deliver changes to wiki windows
        #[cfg(target_os = "android")]
        {
            if self.android_bridge.lock().unwrap().is_none() {
                match android_bridge::AndroidBridge::start(self.wiki_tx.clone()) {
                    Ok(bridge) => {
                        *self.android_bridge.lock().unwrap() = Some(bridge);
                    }
                    Err(e) => {
                        eprintln!("[LAN Sync] Failed to start Android bridge in background: {}", e);
                    }
                }
            }
        }

        // Persist in-memory state to disk before starting sync.
        // This ensures the JSON config files are the source of truth — if the app
        // crashed after a UI change but before the file was written, re-saving now
        // makes the persisted state match what was loaded into memory.
        let config_persisted = if let Some(relay) = &self.relay_manager {
            match relay.persist_and_verify_config().await {
                Ok(()) => {
                    eprintln!("[LAN Sync] Relay config persisted to disk — OK");
                    true
                }
                Err(e) => {
                    eprintln!("[LAN Sync] FATAL: Failed to persist relay config — sync will NOT start: {}", e);
                    false
                }
            }
        } else {
            true // No relay manager = nothing to persist
        };

        let wikis_persisted = if let Some(ref app) = app {
            let entries = crate::wiki_storage::load_recent_files_from_disk(app);
            match crate::wiki_storage::save_recent_files_to_disk(app, &entries) {
                Ok(()) => {
                    eprintln!("[LAN Sync] Wiki list persisted to disk — OK ({} entries)", entries.len());
                    true
                }
                Err(e) => {
                    eprintln!("[LAN Sync] FATAL: Failed to persist wiki list — sync will NOT start: {}", e);
                    false
                }
            }
        } else {
            true // No app handle provided = skip wiki list persistence
        };

        if !config_persisted || !wikis_persisted {
            eprintln!("[LAN Sync] Aborting sync startup — config persistence failed");
            return;
        }

        // Auto-connect relay rooms
        if let Some(relay) = &self.relay_manager {
            if let Err(e) = relay.start_all().await {
                eprintln!("[Relay] Failed to start auto-connect rooms: {}", e);
            }
            // Start LAN server + discovery whenever rooms are configured,
            // even if relay connection failed (enables LAN-only sync)
            if relay.has_any_rooms().await {
                if let Err(e) = self.start().await {
                    eprintln!("[LAN Sync] Auto-start LAN server failed: {}", e);
                }
                #[cfg(target_os = "android")]
                start_sync_foreground_service();
            }
        }

        // Notify the frontend that relay config may have changed (e.g. auth cleared
        // on token validation failure, or rooms connected after boot)
        if let Some(ref app) = app {
            let _ = app.emit("relay-sync-config-updated", ());
        }
    }

    /// Start the sync server and mDNS discovery
    pub async fn start(&self) -> Result<(), String> {
        // Guard against multiple starts
        if self.server.read().await.is_some() {
            eprintln!("[LAN Sync] Already running, ignoring start request");
            return Ok(());
        }

        // Rebuild room keys from relay config before starting
        self.update_room_keys().await;

        // Start WebSocket server (shares our peers map + room keys for auth)
        let server = SyncServer::start(
            self.room_keys.clone(),
            self.pairing_manager.device_id().to_string(),
            self.pairing_manager.device_name().to_string(),
            self.event_tx.clone(),
            self.peers.clone(),
        )
        .await?;

        let port = server.port();

        // Store server immediately so it's tracked even if mDNS fails
        self.running.store(true, std::sync::atomic::Ordering::Release);
        *self.server.write().await = Some(server);

        // Wire relay manager into server for transparent LAN→relay fallback
        if let Some(relay) = &self.relay_manager {
            if let Some(ref server) = *self.server.read().await {
                server.set_relay_manager(relay.clone()).await;
            }
        }

        // Start local collab WebSocket server for low-latency Yjs transport
        #[cfg(not(target_os = "android"))]
        {
            let self_arc = get_sync_manager().unwrap();
            self_arc.start_collab_ws_server().await;
        }

        // Update active room codes for discovery beacons
        self.update_active_room_codes().await;

        // Start UDP broadcast discovery (non-fatal — sync works without it, just no auto-discovery)
        let (discovery_tx, mut discovery_rx) = mpsc::unbounded_channel();
        let our_name_for_discovery = self.pairing_manager.device_name();
        match DiscoveryManager::new(
            self.pairing_manager.device_id(),
            &our_name_for_discovery,
            port,
            discovery_tx,
            self.connected_peer_ids.clone(),
            self.active_room_codes.clone(),
        ) {
            Ok(discovery) => {
                *self.discovery.write().await = Some(discovery);

                // Spawn discovery event handler — room-based auto-connect
                let mgr_peers = self.peers.clone();
                let our_device_id = self.pairing_manager.device_id().to_string();
                let our_device_name = self.pairing_manager.device_name().to_string();
                let event_tx_for_discovery = self.event_tx.clone();
                let peer_rooms_ref = get_sync_manager().map(|m| Arc::clone(&m.peer_rooms));
                let peer_room_hashes_ref = get_sync_manager().map(|m| Arc::clone(&m.peer_room_hashes));
                let room_keys_ref = get_sync_manager().map(|m| Arc::clone(&m.room_keys));
                tokio::spawn(async move {
                    // Track when we first saw a room-sharing peer without being connected,
                    // for fallback: if the smaller-ID peer can't connect to us after
                    // 3s, we (larger-ID) connect ourselves.
                    let mut waiting_since: HashMap<String, std::time::Instant> = HashMap::new();

                    while let Some(event) = discovery_rx.recv().await {
                        match event {
                            DiscoveryEvent::PeerDiscovered {
                                device_id,
                                device_name: _,
                                addr,
                                port,
                                rooms: peer_room_codes,
                                room_hashes: peer_room_hashes,
                            } => {
                                // Track last-known address for reconnection
                                if let Some(mgr) = get_sync_manager() {
                                    mgr.last_known_addrs.write().await
                                        .insert(device_id.clone(), (addr.clone(), port));
                                }

                                // Store peer's room codes and hashes
                                if let Some(ref pr) = peer_rooms_ref {
                                    pr.write().await.insert(device_id.clone(), peer_room_codes.clone());
                                }
                                if let Some(ref prh) = peer_room_hashes_ref {
                                    prh.write().await.insert(device_id.clone(), peer_room_hashes.clone());
                                }

                                // Find a shared room between us and this peer
                                let our_rooms = if let Some(mgr) = get_sync_manager() {
                                    mgr.active_room_codes.read()
                                        .unwrap_or_else(|e| e.into_inner())
                                        .clone()
                                } else {
                                    vec![]
                                };
                                // Match rooms by comparing hashes (never uses cleartext codes)
                                let shared_rooms = protocol::select_all_shared_rooms_by_hash(&our_rooms, &peer_room_hashes);

                                if let Some(room_code) = shared_rooms.first().cloned() {
                                    // We share at least one room — auto-connect via LAN
                                    let peers = mgr_peers.read().await;
                                    if peers.contains_key(&device_id) {
                                        // Already connected — update room codes with any newly shared rooms
                                        drop(peers);
                                        if let Some(mgr) = get_sync_manager() {
                                            let old_codes = if let Some(ref server) = *mgr.server.read().await {
                                                let old = server.peer_room_codes(&device_id).await;
                                                server.add_peer_room_codes(&device_id, &shared_rooms).await;
                                                old
                                            } else {
                                                vec![]
                                            };
                                            // If room codes changed, re-send manifests in both directions
                                            let mut new_rooms_added = false;
                                            for r in &shared_rooms {
                                                if !old_codes.contains(r) {
                                                    new_rooms_added = true;
                                                    break;
                                                }
                                            }
                                            if new_rooms_added {
                                                eprintln!("[LAN Sync] New shared rooms discovered for {}, re-sending manifest", &device_id[..8.min(device_id.len())]);
                                                mgr.send_wiki_manifest_to_peer(&device_id).await;
                                                // Push updated peer lists to wiki windows
                                                if let Some(app) = GLOBAL_APP_HANDLE.get() {
                                                    let _ = app.emit("lan-sync-peers-updated", serde_json::json!({}));
                                                }
                                                #[cfg(not(target_os = "android"))]
                                                mgr.push_peer_updates_to_ipc().await;
                                            }
                                        }
                                        waiting_since.remove(&device_id);
                                        continue;
                                    }
                                    drop(peers);

                                    // Cancel any existing reconnection backoff task
                                    if let Some(mgr) = get_sync_manager() {
                                        if let Some(handle) = mgr.reconnect_tasks.write().await.remove(&device_id) {
                                            handle.abort();
                                            eprintln!("[LAN Sync] Cancelled backoff reconnection for {} (discovered via UDP)", device_id);
                                        }
                                    }

                                    // Get group key for this room
                                    let group_key = if let Some(ref rk) = room_keys_ref {
                                        rk.read().await.get(&room_code).copied()
                                    } else {
                                        None
                                    };
                                    let group_key = match group_key {
                                        Some(k) => k,
                                        None => {
                                            eprintln!("[LAN Sync] No group key for room {} — skipping", room_code);
                                            continue;
                                        }
                                    };

                                    // Deterministic tie-breaking: only the device with
                                    // the smaller device ID initiates the outbound connection.
                                    if our_device_id > device_id {
                                        let first_seen = waiting_since.entry(device_id.clone())
                                            .or_insert_with(std::time::Instant::now);
                                        let elapsed = first_seen.elapsed();
                                        if elapsed >= std::time::Duration::from_secs(1) {
                                            let peers_clone = mgr_peers.clone();
                                            let etx = event_tx_for_discovery.clone();
                                            let did = device_id.clone();
                                            let our_did = our_device_id.clone();
                                            let our_dn = our_device_name.clone();
                                            let rc = room_code.clone();
                                            tokio::spawn(async move {
                                                if peers_clone.read().await.contains_key(&did) {
                                                    return;
                                                }
                                                eprintln!(
                                                    "[LAN Sync] Fallback: connecting to peer {} via room {} (we have larger ID, waited {:.1}s)",
                                                    did, rc, elapsed.as_secs_f64()
                                                );
                                                if let Err(e) = client::connect_to_room_peer(
                                                    &addr, port, &did, &our_did, &our_dn,
                                                    &rc, &group_key, peers_clone, etx,
                                                ).await {
                                                    eprintln!(
                                                        "[LAN Sync] Failed to connect to room peer {}: {}",
                                                        did, e
                                                    );
                                                }
                                            });
                                        }
                                        continue;
                                    }

                                    let peers_clone = mgr_peers.clone();
                                    let etx = event_tx_for_discovery.clone();
                                    let our_did = our_device_id.clone();
                                    let our_dn = our_device_name.clone();
                                    let rc = room_code.clone();
                                    tokio::spawn(async move {
                                        if let Err(e) = client::connect_to_room_peer(
                                            &addr, port, &device_id, &our_did, &our_dn,
                                            &rc, &group_key, peers_clone, etx,
                                        ).await {
                                            eprintln!(
                                                "[LAN Sync] Failed to connect to room peer {}: {}",
                                                device_id, e
                                            );
                                        }
                                    });
                                }
                                // If no shared room, just ignore (peer is not in any of our rooms)
                            }
                            DiscoveryEvent::PeerLost { device_id } => {
                                eprintln!("[LAN Sync] Peer lost: {}", device_id);
                                if let Some(ref pr) = peer_rooms_ref {
                                    pr.write().await.remove(&device_id);
                                }
                                if let Some(ref prh) = peer_room_hashes_ref {
                                    prh.write().await.remove(&device_id);
                                }
                            }
                        }
                    }
                });
            }
            Err(e) => {
                eprintln!("[LAN Sync] mDNS discovery failed (sync still works, but no auto-discovery): {}", e);
            }
        }

        // Register wiki base paths for attachment resolution (all platforms)
        self.register_wiki_attachment_paths();

        // Start attachment directory watcher (desktop only)
        #[cfg(not(target_os = "android"))]
        {
            self.start_attachment_watcher().await;
        }

        // Note: event loop is started by start_background() at init time,
        // not here. This method only starts the LAN sync server and discovery.

        // Start foreground service to keep main process alive (Android only)
        #[cfg(target_os = "android")]
        start_sync_foreground_service();

        // Start Android bridge HTTP server for cross-process communication
        // (skip if already started by start_background — dropping the old bridge
        // would delete the port file that the new bridge just wrote)
        #[cfg(target_os = "android")]
        {
            if self.android_bridge.lock().unwrap().is_none() {
                match android_bridge::AndroidBridge::start(self.wiki_tx.clone()) {
                    Ok(bridge) => {
                        *self.android_bridge.lock().unwrap() = Some(bridge);
                    }
                    Err(e) => {
                        eprintln!("[LAN Sync] Failed to start Android bridge (wiki sync won't work): {}", e);
                    }
                }
            }
        }

        // Desktop: periodically push peer lists to wiki processes via IPC.
        // Wiki processes are separate OS processes where get_sync_manager() returns None,
        // so they can't query peers directly via Tauri commands.
        // Only sends when the peer list actually changes (avoids unnecessary TW5 refresh cycles).
        #[cfg(not(target_os = "android"))]
        {
            tokio::spawn(async {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
                let mut last_peers: std::collections::HashMap<String, String> = std::collections::HashMap::new();
                loop {
                    interval.tick().await;
                    let mgr = match get_sync_manager() {
                        Some(m) => m,
                        None => break,
                    };
                    if !mgr.running.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    let server = match crate::GLOBAL_IPC_SERVER.get() {
                        Some(s) => s,
                        None => continue,
                    };
                    let app = match GLOBAL_APP_HANDLE.get() {
                        Some(a) => a,
                        None => continue,
                    };
                    // For each sync-enabled wiki, push its peer list only if changed
                    let entries = crate::wiki_storage::load_recent_files_from_disk(app);
                    for entry in &entries {
                        if entry.sync_enabled {
                            if let Some(ref sync_id) = entry.sync_id {
                                if !sync_id.is_empty() {
                                    let peers = mgr.get_wiki_peers(sync_id).await;
                                    let payload = serde_json::json!({
                                        "type": "peer-update",
                                        "wiki_id": sync_id,
                                        "peers": peers,
                                    }).to_string();
                                    let prev = last_peers.get(sync_id.as_str());
                                    if prev.map_or(true, |p| p != &payload) {
                                        last_peers.insert(sync_id.clone(), payload.clone());
                                        server.send_lan_sync_to_all(sync_id, &payload);
                                    }
                                }
                            }
                        }
                    }
                }
            });
        }

        // Android: periodically update the foreground notification with total peer count.
        #[cfg(target_os = "android")]
        {
            tokio::spawn(async {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
                let mut last_count: i32 = -1;
                loop {
                    interval.tick().await;
                    let mgr = match get_sync_manager() {
                        Some(m) => m,
                        None => break,
                    };
                    if !mgr.running.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                    let status = mgr.get_status().await;
                    let count = status.connected_peers.len() as i32;
                    if count != last_count {
                        last_count = count;
                        update_sync_notification_peers(count);
                    }
                }
            });
        }

        eprintln!("[LAN Sync] Started on port {}", port);
        Ok(())
    }

    /// Register wiki base paths for attachment resolution (all platforms).
    /// This is needed so that incoming attachments can be saved to the correct location.
    fn register_wiki_attachment_paths(&self) {
        let app = match GLOBAL_APP_HANDLE.get() {
            Some(a) => a,
            None => return,
        };

        let sync_wikis = crate::wiki_storage::get_sync_enabled_wikis(app);
        for (sync_id, _name, is_folder) in &sync_wikis {
            if *is_folder {
                continue;
            }
            if let Some(wiki_path) = crate::wiki_storage::get_wiki_path_by_sync_id(app, sync_id) {
                #[cfg(not(target_os = "android"))]
                {
                    let wiki_file = std::path::Path::new(&wiki_path);
                    if let Some(parent) = wiki_file.parent() {
                        self.attachment_manager
                            .register_wiki_path(sync_id, parent.to_path_buf());
                    }
                }
                #[cfg(target_os = "android")]
                {
                    // On Android, wiki_path is a SAF URI (content://...)
                    // Pre-populate the attachment cache so the first 30s scan
                    // has a baseline to diff against (avoids broadcasting all
                    // existing files as "new" on the first tick).
                    let entries_with_size = collect_attachment_entries_with_size(&wiki_path);
                    let snapshot: Vec<(String, u64)> = entries_with_size
                        .iter()
                        .map(|(e, sz)| (e.rel_path.clone(), *sz))
                        .collect();
                    self.attachment_manager
                        .update_attachment_cache(sync_id, snapshot);
                    self.attachment_manager
                        .register_wiki_saf_uri(sync_id, wiki_path);
                }
            }
        }
    }

    /// Start watching attachments directories for sync-enabled wikis (desktop only)
    #[cfg(not(target_os = "android"))]
    async fn start_attachment_watcher(&self) {
        let app = match GLOBAL_APP_HANDLE.get() {
            Some(a) => a,
            None => return,
        };

        let sync_wikis = crate::wiki_storage::get_sync_enabled_wikis(app);
        // Collect connected room codes for filtering
        let connected_rooms: HashSet<String> = if let Some(relay) = &self.relay_manager {
            relay.get_rooms().await.into_iter()
                .filter(|r| r.connected)
                .map(|r| r.room_code)
                .collect()
        } else {
            HashSet::new()
        };
        let mut wiki_dirs = Vec::new();

        for (sync_id, _name, is_folder) in &sync_wikis {
            if *is_folder {
                continue; // Folder wikis don't have separate attachment directories
            }
            // Only watch wikis assigned to a connected room
            let room = crate::wiki_storage::get_wiki_relay_room_by_sync_id(app, sync_id);
            match room {
                Some(ref rc) if connected_rooms.contains(rc) => {}
                _ => continue, // No room or room not connected — skip
            }
            if let Some(wiki_path) = crate::wiki_storage::get_wiki_path_by_sync_id(app, sync_id) {
                let wiki_file = std::path::Path::new(&wiki_path);
                if let Some(parent) = wiki_file.parent() {
                    let attachments_dir = parent.join("attachments");
                    if attachments_dir.is_dir() {
                        // Register wiki base path with attachment manager
                        self.attachment_manager
                            .register_wiki_path(sync_id, parent.to_path_buf());
                        wiki_dirs.push((
                            sync_id.clone(),
                            parent.to_path_buf(),
                            attachments_dir,
                        ));
                    }
                }
            }
        }

        if wiki_dirs.is_empty() {
            eprintln!("[LAN Sync] No attachments directories to watch");
            return;
        }

        let (att_tx, att_rx) = mpsc::unbounded_channel();
        match attachments::AttachmentWatcher::start(wiki_dirs, att_tx) {
            Ok(watcher) => {
                *self.attachment_watcher.write().await = Some(watcher);
                *self.attachment_event_rx.lock().await = Some(att_rx);
                eprintln!("[LAN Sync] Attachment watcher started");
            }
            Err(e) => {
                eprintln!(
                    "[LAN Sync] Failed to start attachment watcher (sync still works): {}",
                    e
                );
            }
        }
    }

    /// Store local username and broadcast to all connected peers
    pub async fn announce_username(&self, user_name: String) {
        eprintln!("[LAN Sync] announce_username: storing and broadcasting '{}'", user_name);
        // Store locally
        if let Ok(mut guard) = self.local_user_name.lock() {
            *guard = Some(user_name.clone());
        }
        // Broadcast to all peers
        let msg = SyncMessage::UserNameAnnounce { user_name };
        if let Some(ref server) = *self.server.read().await {
            server.broadcast(&msg).await;
        } else {
            eprintln!("[LAN Sync] announce_username: no server to broadcast to");
        }
    }

    /// Stop the sync server and discovery
    pub async fn stop(&self) {
        // Mark as not running immediately so the event loop stops processing messages
        self.running.store(false, std::sync::atomic::Ordering::Release);

        // Cancel all reconnection tasks
        {
            let mut tasks = self.reconnect_tasks.write().await;
            for (device_id, handle) in tasks.drain() {
                handle.abort();
                eprintln!("[LAN Sync] Cancelled reconnection task for {} on stop", device_id);
            }
        }

        // Gracefully close all peer connections before stopping
        if let Some(ref server) = *self.server.read().await {
            server.close_all_peers().await;
        }

        *self.server.write().await = None;

        // NOTE: Do NOT stop relay rooms here — relay rooms are independent of LAN sync.
        // Relay rooms are managed via their own connect/disconnect commands.

        if let Some(mut disc) = self.discovery.write().await.take() {
            disc.shutdown();
        }
        // Stop attachment watcher (desktop only)
        #[cfg(not(target_os = "android"))]
        {
            *self.attachment_watcher.write().await = None;
        }
        self.peers.write().await.clear();
        if let Ok(mut set) = self.connected_peer_ids.write() {
            set.clear();
        }
        self.peer_rooms.write().await.clear();
        self.remote_wikis.write().await.clear();
        self.last_known_addrs.write().await.clear();

        // Stop collab WebSocket server
        let _ = self.collab_ws_shutdown.send(true);
        self.collab_ws_port.store(0, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut clients) = self.collab_ws_clients.lock() {
            clients.clear();
        }
        if let Ok(mut editors) = self.collab_editors.lock() {
            editors.clear();
        }
        if let Ok(mut local) = self.local_collab_editors.lock() {
            local.clear();
        }

        // Stop Android bridge
        #[cfg(target_os = "android")]
        {
            if let Ok(mut guard) = self.android_bridge.lock() {
                if let Some(ref mut bridge) = *guard {
                    bridge.stop();
                }
                *guard = None;
            }
        }

        // Stop foreground service only if no relay rooms are connected (Android only)
        #[cfg(target_os = "android")]
        {
            let any_relay_connected = if let Some(relay) = &self.relay_manager {
                !relay.connected_peers().await.is_empty()
            } else {
                false
            };
            if !any_relay_connected {
                stop_sync_foreground_service();
            }
        }

        eprintln!("[LAN Sync] Stopped");
    }

    /// Send a sync message to a peer, trying LAN server first then relay.
    /// This is the unified send method that works regardless of connection type.
    pub async fn send_to_peer_any(&self, device_id: &str, msg: &SyncMessage) -> Result<(), String> {
        if let Some(ref server) = *self.server.read().await {
            return server.send_to_peer(device_id, msg).await;
        }
        if let Some(relay) = &self.relay_manager {
            if relay.has_peer(device_id).await {
                return relay.send_to_peer(device_id, msg).await;
            }
        }
        Err(format!("Peer {} not connected via LAN or relay", device_id))
    }

    /// Send a sync message to multiple peers, routing each via LAN or relay.
    pub async fn send_to_peers_any(&self, peers: &[String], msg: &SyncMessage) {
        for peer_id in peers {
            if let Err(e) = self.send_to_peer_any(peer_id, msg).await {
                eprintln!("[Sync] Failed to send to peer {}: {}", peer_id, e);
            }
        }
    }

    /// Get all connected peers (LAN + relay, deduped).
    /// Returns (device_id, device_name) pairs.
    pub async fn connected_peers_all(&self) -> Vec<(String, String)> {
        let mut seen = HashSet::new();
        let mut result = Vec::new();

        // LAN peers
        if let Some(ref server) = *self.server.read().await {
            for (id, name, _user_name) in server.connected_peers().await {
                if seen.insert(id.clone()) {
                    result.push((id, name));
                }
            }
        }

        // Relay peers
        if let Some(relay) = &self.relay_manager {
            let rooms = relay.get_rooms().await;
            for room in &rooms {
                for peer in &room.connected_peers {
                    if seen.insert(peer.device_id.clone()) {
                        result.push((peer.device_id.clone(), peer.device_name.clone()));
                    }
                }
            }
        }

        result
    }

    /// Get all peers (LAN + relay) that should sync a specific wiki.
    /// Uses the wiki's assigned relay room to find all peers in that room.
    async fn get_all_peers_for_wiki(&self, wiki_id: &str) -> Vec<String> {
        let mut peers = HashSet::new();
        if let Some(app) = GLOBAL_APP_HANDLE.get() {
            // All peers in this wiki's assigned room (both LAN and relay)
            if let Some(relay) = &self.relay_manager {
                if let Some(room_code) = crate::wiki_storage::get_wiki_relay_room_by_sync_id(app, wiki_id) {
                    // LAN peers authenticated for this room
                    for (did, pc) in self.peers.read().await.iter() {
                        if pc.auth_room_codes.iter().any(|rc| rc == &room_code) {
                            peers.insert(did.clone());
                        }
                    }
                    // Relay peers in this room
                    for room in relay.get_rooms().await {
                        if room.room_code == room_code && room.connected {
                            for peer in room.connected_peers {
                                peers.insert(peer.device_id);
                            }
                        }
                    }
                }
            }
        }
        peers.into_iter().collect()
    }

    /// Get relay-only peers for a wiki (peers in the relay room but NOT connected via LAN).
    /// Used to avoid double-sending to LAN peers that are also in a relay room.
    async fn get_relay_only_peers_for_wiki(&self, wiki_id: &str) -> Vec<String> {
        let app = match GLOBAL_APP_HANDLE.get() {
            Some(a) => a,
            None => return vec![],
        };
        let lan_peers: HashSet<String> = self.peers.read().await.keys().cloned().collect();
        let mut relay_only = Vec::new();
        if let Some(relay) = &self.relay_manager {
            if let Some(room_code) = crate::wiki_storage::get_wiki_relay_room_by_sync_id(app, wiki_id) {
                for room in relay.get_rooms().await {
                    if room.room_code == room_code && room.connected {
                        for peer in room.connected_peers {
                            if !lan_peers.contains(&peer.device_id) {
                                relay_only.push(peer.device_id);
                            }
                        }
                    }
                }
            }
        }
        relay_only
    }

    /// Handle local changes when LAN server is not running (relay-only mode).
    /// Performs conflict_manager operations AND routes through relay rooms.
    async fn handle_local_change_relay(&self, change: WikiToSync) {
        let relay = match &self.relay_manager {
            Some(r) => r,
            None => return,
        };
        let app = match GLOBAL_APP_HANDLE.get() {
            Some(a) => a,
            None => return,
        };

        match change {
            WikiToSync::TiddlerChanged { wiki_id, title, tiddler_json } => {
                if !ConflictManager::should_sync_tiddler(&title) {
                    return;
                }
                let room_code = match crate::wiki_storage::get_wiki_relay_room_by_sync_id(app, &wiki_id) {
                    Some(rc) => rc,
                    None => return,
                };
                eprintln!("[Relay] Broadcasting local change: '{}' via room {}", title, room_code);
                let clock = self.conflict_manager.record_local_change(&wiki_id, &title);
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let msg = SyncMessage::TiddlerChanged {
                    wiki_id,
                    title,
                    tiddler_json,
                    vector_clock: clock,
                    timestamp,
                };
                let _ = relay.send_to_room(&room_code, &msg).await;
            }
            WikiToSync::TiddlerDeleted { wiki_id, title } => {
                if !ConflictManager::should_sync_tiddler(&title) {
                    return;
                }
                let room_code = match crate::wiki_storage::get_wiki_relay_room_by_sync_id(app, &wiki_id) {
                    Some(rc) => rc,
                    None => return,
                };
                let clock = self.conflict_manager.record_local_deletion(&wiki_id, &title);
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let msg = SyncMessage::TiddlerDeleted {
                    wiki_id,
                    title,
                    vector_clock: clock,
                    timestamp,
                };
                let _ = relay.send_to_room(&room_code, &msg).await;
            }
            WikiToSync::WikiOpened { wiki_id, .. } => {
                self.conflict_manager.load_wiki_state(&wiki_id);
                self.broadcast_wiki_manifest().await;
            }
            WikiToSync::WikiClosed { wiki_id } => {
                eprintln!("[Relay] Wiki closed: {}", wiki_id);
            }
            WikiToSync::CollabEditingStarted { wiki_id, tiddler_title, device_id, device_name, user_name } => {
                let room_code = match crate::wiki_storage::get_wiki_relay_room_by_sync_id(app, &wiki_id) {
                    Some(rc) => rc,
                    None => return,
                };
                let _ = relay.send_to_room(&room_code, &SyncMessage::EditingStarted {
                    wiki_id, tiddler_title, device_id, device_name, user_name,
                }).await;
            }
            WikiToSync::CollabEditingStopped { wiki_id, tiddler_title, device_id } => {
                let room_code = match crate::wiki_storage::get_wiki_relay_room_by_sync_id(app, &wiki_id) {
                    Some(rc) => rc,
                    None => return,
                };
                let _ = relay.send_to_room(&room_code, &SyncMessage::EditingStopped {
                    wiki_id, tiddler_title, device_id,
                }).await;
            }
            WikiToSync::CollabUpdate { wiki_id, tiddler_title, update_base64 } => {
                let room_code = match crate::wiki_storage::get_wiki_relay_room_by_sync_id(app, &wiki_id) {
                    Some(rc) => rc,
                    None => return,
                };
                let _ = relay.send_to_room(&room_code, &SyncMessage::CollabUpdate {
                    wiki_id, tiddler_title, update_base64,
                }).await;
            }
            WikiToSync::CollabAwareness { wiki_id, tiddler_title, update_base64 } => {
                let room_code = match crate::wiki_storage::get_wiki_relay_room_by_sync_id(app, &wiki_id) {
                    Some(rc) => rc,
                    None => return,
                };
                // Exclude the peer that sent us this awareness (suppress echo-back)
                let mut exclude = std::collections::HashSet::new();
                if let Some(echo_peer) = self.get_awareness_echo_peer(&wiki_id, &tiddler_title) {
                    exclude.insert(echo_peer);
                }
                let _ = relay.send_to_room_excluding(&room_code, &SyncMessage::CollabAwareness {
                    wiki_id, tiddler_title, update_base64,
                }, &exclude).await;
            }
            WikiToSync::CollabPeerSaved { wiki_id, tiddler_title, saved_title, device_id, device_name } => {
                let room_code = match crate::wiki_storage::get_wiki_relay_room_by_sync_id(app, &wiki_id) {
                    Some(rc) => rc,
                    None => return,
                };
                let _ = relay.send_to_room(&room_code, &SyncMessage::PeerSaved {
                    wiki_id, tiddler_title, saved_title, device_id, device_name,
                }).await;
            }
        }
    }

    /// Route a local change through relay rooms (send-only, no conflict_manager).
    /// Called AFTER bridge.handle_local_change when LAN server is running,
    /// to additionally route through relay rooms for wikis assigned to rooms.
    /// Peers already reachable via LAN are excluded to avoid duplicate delivery.
    async fn relay_route_change(
        &self,
        change: &WikiToSync,
        lan_peer_ids: &std::collections::HashSet<String>,
    ) {
        let relay = match &self.relay_manager {
            Some(r) => r,
            None => return,
        };
        let app = match GLOBAL_APP_HANDLE.get() {
            Some(a) => a,
            None => return,
        };

        match change {
            WikiToSync::TiddlerChanged { wiki_id, title, tiddler_json } => {
                if !ConflictManager::should_sync_tiddler(title) {
                    return;
                }
                let room_code = match crate::wiki_storage::get_wiki_relay_room_by_sync_id(app, wiki_id) {
                    Some(rc) => rc,
                    None => return,
                };
                // conflict_manager already called by bridge — just get the current clock
                let clock = self.conflict_manager.get_clock(wiki_id, title);
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let msg = SyncMessage::TiddlerChanged {
                    wiki_id: wiki_id.clone(),
                    title: title.clone(),
                    tiddler_json: tiddler_json.clone(),
                    vector_clock: clock,
                    timestamp,
                };
                eprintln!("[Relay] Additionally routing change '{}' via room {} (excluding {} LAN peers)", title, room_code, lan_peer_ids.len());
                let _ = relay.send_to_room_excluding(&room_code, &msg, lan_peer_ids).await;
            }
            WikiToSync::TiddlerDeleted { wiki_id, title } => {
                if !ConflictManager::should_sync_tiddler(title) {
                    return;
                }
                let room_code = match crate::wiki_storage::get_wiki_relay_room_by_sync_id(app, wiki_id) {
                    Some(rc) => rc,
                    None => return,
                };
                let clock = self.conflict_manager.get_clock(wiki_id, title);
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let msg = SyncMessage::TiddlerDeleted {
                    wiki_id: wiki_id.clone(),
                    title: title.clone(),
                    vector_clock: clock,
                    timestamp,
                };
                let _ = relay.send_to_room_excluding(&room_code, &msg, lan_peer_ids).await;
            }
            WikiToSync::WikiOpened { .. } => {
                // Send manifest to relay rooms too (bridge only sends to LAN peers)
                self.broadcast_wiki_manifest().await;
            }
            WikiToSync::CollabEditingStarted { wiki_id, tiddler_title, device_id, device_name, user_name } => {
                let room_code = match crate::wiki_storage::get_wiki_relay_room_by_sync_id(app, wiki_id) {
                    Some(rc) => rc,
                    None => return,
                };
                let _ = relay.send_to_room_excluding(&room_code, &SyncMessage::EditingStarted {
                    wiki_id: wiki_id.clone(), tiddler_title: tiddler_title.clone(),
                    device_id: device_id.clone(), device_name: device_name.clone(),
                    user_name: user_name.clone(),
                }, lan_peer_ids).await;
            }
            WikiToSync::CollabEditingStopped { wiki_id, tiddler_title, device_id } => {
                let room_code = match crate::wiki_storage::get_wiki_relay_room_by_sync_id(app, wiki_id) {
                    Some(rc) => rc,
                    None => return,
                };
                let _ = relay.send_to_room_excluding(&room_code, &SyncMessage::EditingStopped {
                    wiki_id: wiki_id.clone(), tiddler_title: tiddler_title.clone(),
                    device_id: device_id.clone(),
                }, lan_peer_ids).await;
            }
            WikiToSync::CollabUpdate { wiki_id, tiddler_title, update_base64 } => {
                let room_code = match crate::wiki_storage::get_wiki_relay_room_by_sync_id(app, wiki_id) {
                    Some(rc) => rc,
                    None => return,
                };
                let _ = relay.send_to_room_excluding(&room_code, &SyncMessage::CollabUpdate {
                    wiki_id: wiki_id.clone(), tiddler_title: tiddler_title.clone(),
                    update_base64: update_base64.clone(),
                }, lan_peer_ids).await;
            }
            WikiToSync::CollabAwareness { wiki_id, tiddler_title, update_base64 } => {
                let room_code = match crate::wiki_storage::get_wiki_relay_room_by_sync_id(app, wiki_id) {
                    Some(rc) => rc,
                    None => return,
                };
                // Also exclude the echo peer (the one who sent us this awareness)
                let mut exclude = lan_peer_ids.clone();
                if let Some(echo_peer) = self.get_awareness_echo_peer(wiki_id, tiddler_title) {
                    exclude.insert(echo_peer);
                }
                let _ = relay.send_to_room_excluding(&room_code, &SyncMessage::CollabAwareness {
                    wiki_id: wiki_id.clone(), tiddler_title: tiddler_title.clone(),
                    update_base64: update_base64.clone(),
                }, &exclude).await;
            }
            WikiToSync::CollabPeerSaved { wiki_id, tiddler_title, saved_title, device_id, device_name } => {
                let room_code = match crate::wiki_storage::get_wiki_relay_room_by_sync_id(app, wiki_id) {
                    Some(rc) => rc,
                    None => return,
                };
                let _ = relay.send_to_room_excluding(&room_code, &SyncMessage::PeerSaved {
                    wiki_id: wiki_id.clone(), tiddler_title: tiddler_title.clone(),
                    saved_title: saved_title.clone(),
                    device_id: device_id.clone(), device_name: device_name.clone(),
                }, lan_peer_ids).await;
            }
            _ => {}
        }
    }


    /// Send tiddler fingerprints to a specific peer for diff-based sync.
    /// The peer will compare and send only tiddlers that differ.
    pub async fn send_tiddler_fingerprints(
        &self,
        wiki_id: &str,
        to_device_id: &str,
        fingerprints: Vec<protocol::TiddlerFingerprint>,
    ) -> Result<(), String> {
        // Check if this peer is in the wiki's assigned room (LAN or relay)
        if let Some(app) = GLOBAL_APP_HANDLE.get() {
            let allowed = self.is_peer_allowed_for_wiki(app, wiki_id, to_device_id).await;
            if !allowed {
                eprintln!(
                    "[LAN Sync] Skipping fingerprints to {} — not in room for wiki {}",
                    to_device_id, wiki_id
                );
                return Ok(());
            }
        }

        // Cache fingerprints for pre-boot sync and persist to disk
        self.update_fingerprint_cache(wiki_id, fingerprints.clone());

        let msg = SyncMessage::TiddlerFingerprints {
            wiki_id: wiki_id.to_string(),
            from_device_id: self.pairing_manager.device_id().to_string(),
            fingerprints,
            is_reply: false,
        };

        self.send_to_peer_any(to_device_id, &msg)
            .await
            .map_err(|e| format!("Failed to send fingerprints: {}", e))?;

        // Clear pending fingerprint request (response received successfully)
        self.pending_fingerprint_requests.write().await.remove(
            &(wiki_id.to_string(), to_device_id.to_string()),
        );

        eprintln!(
            "[LAN Sync] Sent tiddler fingerprints for wiki {} to peer {}",
            wiki_id, to_device_id
        );

        Ok(())
    }

    /// Broadcast tiddler fingerprints to connected peers that share this wiki's room.
    /// Called proactively by JS when a wiki's sync activates — no event round-trip needed.
    pub async fn broadcast_tiddler_fingerprints(
        &self,
        wiki_id: &str,
        fingerprints: Vec<protocol::TiddlerFingerprint>,
    ) -> Result<(), String> {
        // Cache fingerprints for pre-boot sync and persist to disk
        self.update_fingerprint_cache(wiki_id, fingerprints.clone());

        if GLOBAL_APP_HANDLE.get().is_none() {
            return Ok(());
        }

        // Get all peers allowed for this wiki (via room membership)
        let allowed: HashSet<String> = self.get_all_peers_for_wiki(wiki_id).await.into_iter().collect();

        if allowed.is_empty() {
            return Ok(());
        }

        let remote = self.remote_wikis.read().await;
        let mut sent_count = 0u32;

        for (device_id, wikis) in remote.iter() {
            if !allowed.contains(device_id) {
                continue;
            }
            if wikis.iter().any(|w| w.wiki_id == wiki_id) {
                if self.dedup_fp_send(wiki_id, device_id) {
                    eprintln!(
                        "[Sync] Dedup: skipping broadcast fingerprints to peer {} for wiki {}",
                        device_id, wiki_id
                    );
                    continue;
                }
                let msg = SyncMessage::TiddlerFingerprints {
                    wiki_id: wiki_id.to_string(),
                    from_device_id: self.pairing_manager.device_id().to_string(),
                    fingerprints: fingerprints.clone(),
                    is_reply: false,
                };
                if let Err(e) = self.send_to_peer_any(device_id, &msg).await {
                    eprintln!(
                        "[Sync] Failed to send fingerprints to peer {}: {}",
                        device_id, e
                    );
                } else {
                    sent_count += 1;
                }
            }
        }

        if sent_count > 0 {
            eprintln!(
                "[Sync] Broadcast {} fingerprints for wiki {} to {} peers",
                fingerprints.len(), wiki_id, sent_count
            );
        }
        Ok(())
    }

    /// Send a full sync batch to a specific peer.
    /// Called by JS when it has gathered tiddlers in response to a `lan-sync-dump-tiddlers` event.
    pub async fn send_full_sync_batch(
        &self,
        wiki_id: &str,
        to_device_id: &str,
        tiddlers: Vec<TiddlerBatch>,
        is_last_batch: bool,
    ) -> Result<(), String> {
        // Attach vector clocks from our conflict manager.
        // Only increment the clock for tiddlers not yet tracked (never synced before).
        // Already-tracked tiddlers use their existing clock — this prevents an
        // infinite loop where each send increments the clock, causing the peer
        // to see a newer version and re-send back, ad infinitum.
        let sync_tiddlers: Vec<protocol::SyncTiddler> = tiddlers
            .into_iter()
            .filter(|t| conflict::ConflictManager::should_sync_tiddler(&t.title))
            .map(|t| {
                let existing = self.conflict_manager.get_clock(wiki_id, &t.title);
                let clock = if existing.clocks.is_empty() {
                    // Never tracked — increment once to establish authorship
                    self.conflict_manager.record_local_change(wiki_id, &t.title)
                } else {
                    existing
                };
                protocol::SyncTiddler {
                    title: t.title,
                    tiddler_json: t.tiddler_json,
                    vector_clock: clock,
                }
            })
            .collect();

        if sync_tiddlers.is_empty() && !is_last_batch {
            return Ok(());
        }

        let msg = SyncMessage::FullSyncBatch {
            wiki_id: wiki_id.to_string(),
            tiddlers: sync_tiddlers,
            is_last_batch,
        };

        self.send_to_peer_any(to_device_id, &msg)
            .await
            .map_err(|e| format!("Failed to send full sync batch: {}", e))?;

        if is_last_batch {
            eprintln!(
                "[LAN Sync] Full sync batch complete for wiki {} to peer {}",
                wiki_id, to_device_id
            );
        }

        Ok(())
    }

    /// Get the shared peers map
    fn get_peers_arc(&self) -> Arc<RwLock<HashMap<String, PeerConnection>>> {
        self.peers.clone()
    }

    /// Get current sync status
    pub async fn get_status(&self) -> SyncStatus {
        let server = self.server.read().await;
        let port = server.as_ref().map(|s| s.port());
        let connected = if let Some(s) = server.as_ref() {
            s.connected_peers().await
        } else {
            vec![]
        };

        let relay_connected = if let Some(ref relay) = self.relay_manager {
            relay.any_room_connected().await
        } else {
            false
        };

        SyncStatus {
            running: server.is_some(),
            device_id: self.pairing_manager.device_id().to_string(),
            device_name: self.pairing_manager.device_name().to_string(),
            port,
            connected_peers: connected
                .into_iter()
                .map(|(id, name, user_name)| PeerInfo {
                    device_id: id,
                    device_name: name,
                    user_name,
                })
                .collect(),
            relay_connected,
        }
    }

    /// Check if the relay is connected (any room joined with active WebSocket).
    async fn is_relay_connected(&self) -> bool {
        if let Some(relay) = &self.relay_manager {
            relay.any_room_connected().await
        } else {
            false
        }
    }

    /// Check if sync is active — either relay is connected OR we have LAN peers.
    /// Used as a gate for sync data exchange.
    async fn is_sync_active(&self) -> bool {
        if self.is_relay_connected().await {
            return true;
        }
        !self.peers.read().await.is_empty()
    }

    /// Push peer lists for all sync-enabled wikis to wiki processes via IPC.
    /// Called immediately on peer connect/disconnect for instant UI updates.
    #[cfg(not(target_os = "android"))]
    async fn push_peer_updates_to_ipc(&self) {
        let server = match crate::GLOBAL_IPC_SERVER.get() {
            Some(s) => s,
            None => {
                eprintln!("[LAN Sync] push_peer_updates_to_ipc: no IPC server");
                return;
            }
        };
        let app = match GLOBAL_APP_HANDLE.get() {
            Some(a) => a,
            None => return,
        };
        let entries = crate::wiki_storage::load_recent_files_from_disk(app);
        for entry in &entries {
            if entry.sync_enabled {
                if let Some(ref sync_id) = entry.sync_id {
                    if !sync_id.is_empty() {
                        let peers = self.get_wiki_peers(sync_id).await;
                        eprintln!(
                            "[LAN Sync] push_peer_updates_to_ipc: wiki={} room={:?} peers={}",
                            sync_id,
                            entry.relay_room.as_deref().unwrap_or("(none)"),
                            peers.len()
                        );
                        let payload = serde_json::json!({
                            "type": "peer-update",
                            "wiki_id": sync_id,
                            "peers": peers,
                        }).to_string();
                        server.send_lan_sync_to_all(sync_id, &payload);
                    }
                }
            }
        }
    }

    /// Get peers connected to a specific wiki (both LAN and relay), with names.
    /// Only includes peers that are in the same room AND have this wiki open
    /// (announced in their WikiManifest). Excludes the local device.
    pub async fn get_wiki_peers(&self, wiki_id: &str) -> Vec<PeerInfo> {
        let mut seen = HashSet::new();
        let mut result = Vec::new();
        let app = match GLOBAL_APP_HANDLE.get() {
            Some(a) => a,
            None => return result,
        };
        let room_code = crate::wiki_storage::get_wiki_relay_room_by_sync_id(app, wiki_id);
        let local_device_id = self.pairing_manager.device_id();

        // Collect device_ids that have this wiki open (from WikiManifest announcements)
        let peers_with_wiki: HashSet<String> = {
            let remote = self.remote_wikis.read().await;
            remote.iter()
                .filter(|(_, wikis)| wikis.iter().any(|w| w.wiki_id == wiki_id))
                .map(|(device_id, _)| device_id.clone())
                .collect()
        };

        // LAN peers authenticated for this wiki's room AND having this wiki open
        // NOTE: Collect connected_peers() BEFORE taking a read lock on self.peers
        // to avoid potential deadlock (both reference the same Arc<RwLock>).
        if let Some(ref rc) = room_code {
            let connected = if let Some(ref server) = *self.server.read().await {
                server.connected_peers().await
            } else {
                vec![]
            };
            let peers_guard = self.peers.read().await;
            for (id, name, user_name) in connected {
                if id == local_device_id { continue; }
                if !peers_with_wiki.contains(&id) { continue; }
                if let Some(pc) = peers_guard.get(&id) {
                    if pc.auth_room_codes.iter().any(|r| r == rc.as_str()) {
                        if seen.insert(id.clone()) {
                            result.push(PeerInfo { device_id: id, device_name: name, user_name });
                        }
                    }
                }
            }
            drop(peers_guard);
        }

        // Relay peers in this wiki's room AND having this wiki open
        if let Some(ref rc) = room_code {
            if let Some(ref relay) = self.relay_manager {
                for room in relay.get_rooms().await {
                    if room.room_code == *rc && room.connected {
                        for peer in room.connected_peers {
                            if peer.device_id == local_device_id { continue; }
                            if !peers_with_wiki.contains(&peer.device_id) { continue; }
                            if seen.insert(peer.device_id.clone()) {
                                result.push(PeerInfo {
                                    device_id: peer.device_id.clone(),
                                    device_name: peer.device_name.clone(),
                                    user_name: None,
                                });
                            }
                        }
                    }
                }
            }
        }

        // Fill in missing user_names from the device-level peer_user_names map
        // (handles relay peers and any LAN peers where the connection struct missed it)
        if let Ok(names) = self.peer_user_names.lock() {
            for peer in &mut result {
                if peer.user_name.is_none() {
                    if let Some(name) = names.get(&peer.device_id) {
                        peer.user_name = Some(name.clone());
                    }
                }
            }
        }

        result
    }

    /// Check if a peer is allowed to sync a specific wiki (via room membership).
    async fn is_peer_allowed_for_wiki(&self, app: &tauri::AppHandle, wiki_id: &str, device_id: &str) -> bool {
        if let Some(room_code) = crate::wiki_storage::get_wiki_relay_room_by_sync_id(app, wiki_id) {
            // Check LAN peer room
            if let Some(pc) = self.peers.read().await.get(device_id) {
                if pc.auth_room_codes.iter().any(|rc| rc == &room_code) {
                    return true;
                }
            }
            // Check relay rooms
            if let Some(relay) = &self.relay_manager {
                if relay.find_all_device_rooms(device_id).await.iter().any(|rc| rc == &room_code) {
                    return true;
                }
            }
        }
        false
    }

    /// Rebuild room_keys from the relay config (called when rooms change).
    async fn update_room_keys(&self) {
        if let Some(relay) = &self.relay_manager {
            let rooms = relay.get_rooms().await;
            let mut keys = self.room_keys.write().await;
            keys.clear();
            for room in &rooms {
                if let Some(creds) = relay.get_room_credentials(&room.room_code).await {
                    let group_key = RelaySyncManager::derive_group_key(&creds.2, &room.room_code);
                    keys.insert(room.room_code.clone(), group_key);
                }
            }
            eprintln!("[LAN Sync] Updated room keys: {} rooms", keys.len());
        }
    }

    /// Update active_room_codes from ALL configured rooms (for discovery beacons).
    /// Every configured room is included — LAN discovery works independently of
    /// relay connection status. Removing a room from config stops LAN discovery.
    async fn update_active_room_codes(&self) {
        if let Some(relay) = &self.relay_manager {
            let rooms = relay.get_rooms().await;
            let codes: Vec<String> = rooms.iter()
                .filter(|r| r.connected)
                .map(|r| r.room_code.clone())
                .collect();
            if let Ok(mut arc) = self.active_room_codes.write() {
                *arc = codes.clone();
            }
            eprintln!("[LAN Sync] Updated active room codes: {:?}", codes);
        }
    }

    /// Trigger an immediate UDP discovery beacon so peers see room changes instantly.
    async fn force_discovery_beacon(&self) {
        if let Some(ref discovery) = *self.discovery.read().await {
            discovery.force_beacon();
        }
    }

    /// Called when a sync-enabled wiki window opens. Triggers catch-up sync
    /// with any connected peers that have this wiki.
    pub async fn on_wiki_opened(&self, wiki_id: &str) {
        // Broadcast manifest to ALL peers (LAN + relay rooms) so they know our
        // wiki list and can trigger fingerprint exchange for catch-up sync.
        // This is critical for relay sync where the desktop direct path
        // (lan_sync_wiki_opened → on_wiki_opened) bypasses the event loop.
        self.broadcast_wiki_manifest().await;

        let remote = self.remote_wikis.read().await;
        eprintln!(
            "[LAN Sync] on_wiki_opened: wiki_id={}, remote_wikis has {} peers",
            wiki_id,
            remote.len()
        );
        for (did, wlist) in remote.iter() {
            let wids: Vec<&str> = wlist.iter().map(|w| w.wiki_id.as_str()).collect();
            eprintln!("[LAN Sync]   peer {} has wikis: {:?}", did, wids);
        }
        let app = match GLOBAL_APP_HANDLE.get() {
            Some(a) => a,
            None => return,
        };

        // Find all connected peers that have this wiki AND are in the wiki's room
        for (device_id, wikis) in remote.iter() {
            if wikis.iter().any(|w| w.wiki_id == wiki_id) {
                // Verify this peer is room-authenticated for this wiki
                if !self.is_peer_allowed_for_wiki(app, wiki_id, device_id).await {
                    eprintln!(
                        "[LAN Sync] Wiki {} opened — skipping peer {} (not in room)",
                        wiki_id, device_id
                    );
                    continue;
                }
                eprintln!(
                    "[LAN Sync] Wiki {} opened — requesting fingerprint sync from peer {}",
                    wiki_id, device_id
                );
                // Track this fingerprint request for timeout detection
                self.pending_fingerprint_requests.write().await.insert(
                    (wiki_id.to_string(), device_id.clone()),
                    std::time::Instant::now(),
                );
                // Ask JS to send tiddler fingerprints (title + modified)
                // so the peer can compare and send only what's different
                Self::emit_to_wiki(
                    &wiki_id,
                    "lan-sync-send-fingerprints",
                    serde_json::json!({
                        "type": "send-fingerprints",
                        "wiki_id": wiki_id,
                        "to_device_id": device_id,
                    }),
                );

                // Also send our attachment manifest for single-file wikis.
                // Spawned as background task to avoid blocking the event loop.
                if let Some(wiki_path) = crate::wiki_storage::get_wiki_path_by_sync_id(app, wiki_id) {
                    let did = device_id.to_string();
                    let wid = wiki_id.to_string();
                    if let Some(mgr) = get_sync_manager() {
                        tokio::spawn(async move {
                            mgr.send_attachment_manifest(&did, &wid, &wiki_path).await;
                        });
                    }
                }
            }
        }

        // Deliver pending tiddler changes that arrived while no wiki window was open
        // (these are apply-change/apply-deletion events preserved in the always-on buffer)
        #[cfg(not(target_os = "android"))]
        {
            let pending = self.pending_wiki_changes.lock().unwrap().remove(wiki_id).unwrap_or_default();
            if !pending.is_empty() {
                eprintln!(
                    "[LAN Sync] Delivering {} pending tiddler changes for wiki {}",
                    pending.len(), wiki_id
                );
                if let Some(server) = crate::GLOBAL_IPC_SERVER.get() {
                    for msg in &pending {
                        server.send_lan_sync_to_all(wiki_id, msg);
                    }
                }
            }

            // Accept deferred vector clocks for the buffered changes we just delivered.
            // These clocks were stored by spawn_emit_task when IPC had 0 clients.
            let pending_clocks = self.pending_wiki_clocks.lock().unwrap().remove(wiki_id).unwrap_or_default();
            if !pending_clocks.is_empty() {
                eprintln!(
                    "[LAN Sync] Accepting {} deferred vector clocks for wiki {}",
                    pending_clocks.len(), wiki_id
                );
                for (title, clock, is_deletion) in pending_clocks {
                    if is_deletion {
                        self.conflict_manager.accept_remote_deletion(wiki_id, &title, &clock);
                    } else {
                        self.conflict_manager.accept_remote_change(wiki_id, &title, &clock);
                    }
                }
            }
        }

        // Deliver any messages buffered by pre_request_sync
        #[cfg(not(target_os = "android"))]
        {
            let buffered = self.pre_sync_buffer.lock().unwrap().remove(wiki_id).unwrap_or_default();
            if !buffered.is_empty() {
                eprintln!(
                    "[LAN Sync] Delivering {} buffered pre-sync messages for wiki {}",
                    buffered.len(), wiki_id
                );
                if let Some(server) = crate::GLOBAL_IPC_SERVER.get() {
                    for msg in &buffered {
                        server.send_lan_sync_to_all(wiki_id, msg);
                    }
                }
            }
        }

        // Push current peer list to the newly-connected wiki window.
        // peer-update messages are NOT buffered (unlike apply-change/apply-deletion),
        // so any peer-update sent before the wiki's IPC client connected was lost.
        #[cfg(not(target_os = "android"))]
        {
            let peers = self.get_wiki_peers(wiki_id).await;
            if !peers.is_empty() {
                eprintln!(
                    "[LAN Sync] Pushing {} peers to newly-opened wiki {}",
                    peers.len(), wiki_id
                );
                let payload = serde_json::json!({
                    "type": "peer-update",
                    "wiki_id": wiki_id,
                    "peers": peers,
                }).to_string();
                if let Some(server) = crate::GLOBAL_IPC_SERVER.get() {
                    server.send_lan_sync_to_all(wiki_id, &payload);
                }
            }
        }
    }

    /// Pre-request sync from peers before the wiki's JS has booted.
    /// Called from open_wiki_window when a sync-enabled wiki is opened.
    /// Sends RequestFingerprints to all peers sharing this wiki, and also
    /// sends our cached fingerprints so the peer can start computing and
    /// sending diffs before our JS boots.
    /// On desktop, initializes the pre_sync_buffer for message capture.
    /// On Android, the bridge queue handles buffering automatically.
    pub async fn pre_request_sync(&self, wiki_id: &str) {
        // Desktop: Initialize buffer for this wiki
        #[cfg(not(target_os = "android"))]
        {
            self.pre_sync_buffer.lock().unwrap().insert(wiki_id.to_string(), Vec::new());
        }

        let remote = self.remote_wikis.read().await;
        let server_guard = self.server.read().await;
        let server = match server_guard.as_ref() {
            Some(s) => s,
            None => return,
        };

        // Load cached local fingerprints for this wiki, excluding any
        // tiddlers merged from FullSyncBatch while wiki JS was closed
        let cached_fps = self.get_accurate_cached_fingerprints(wiki_id);

        // Only send to peers that are in the wiki's assigned room
        let allowed_peers = self.get_all_peers_for_wiki(wiki_id).await;

        for (device_id, wikis) in remote.iter() {
            if !allowed_peers.contains(device_id) {
                continue;
            }
            if wikis.iter().any(|w| w.wiki_id == wiki_id) {
                eprintln!(
                    "[LAN Sync] Pre-requesting fingerprints for wiki {} from peer {}",
                    wiki_id, device_id
                );
                // Ask peer for their fingerprints (peer will forward to their JS)
                let _ = server.send_to_peer(
                    device_id,
                    &SyncMessage::RequestFingerprints {
                        wiki_id: wiki_id.to_string(),
                    },
                ).await;

                // Send our cached fingerprints so the peer can compute diffs
                // and start sending us tiddlers before our JS boots.
                // Always send (pre_request_sync is the critical fast path).
                // Record timestamp so dedup suppresses redundant sends from
                // handle_wiki_manifest / reciprocal that fire shortly after.
                let _ = self.dedup_fp_send(wiki_id, device_id);
                let fps = cached_fps.clone().unwrap_or_default();
                eprintln!(
                    "[LAN Sync] Sending {} fingerprints for wiki {} to peer {} (pre-boot)",
                    fps.len(), wiki_id, device_id
                );
                let _ = server.send_to_peer(
                    device_id,
                    &SyncMessage::TiddlerFingerprints {
                        wiki_id: wiki_id.to_string(),
                        from_device_id: self.pairing_manager.device_id().to_string(),
                        fingerprints: fps,
                        is_reply: false,
                    },
                ).await;
            }
        }
    }

    /// Notify that a tiddler changed (called from JS via Tauri command)
    pub fn notify_tiddler_changed(&self, wiki_id: &str, title: &str, tiddler_json: &str) {
        let _ = self.wiki_tx.send(WikiToSync::TiddlerChanged {
            wiki_id: wiki_id.to_string(),
            title: title.to_string(),
            tiddler_json: tiddler_json.to_string(),
        });
    }

    /// Notify that a tiddler was deleted (called from JS via Tauri command)
    pub fn notify_tiddler_deleted(&self, wiki_id: &str, title: &str) {
        let _ = self.wiki_tx.send(WikiToSync::TiddlerDeleted {
            wiki_id: wiki_id.to_string(),
            title: title.to_string(),
        });
    }

    // ── Collaborative editing methods ────────────────────────────────

    /// Notify peers that we started editing a tiddler
    pub fn notify_collab_editing_started(&self, wiki_id: &str, tiddler_title: &str) {
        eprintln!("[Collab] notify_collab_editing_started: wiki={}, tiddler={}", wiki_id, tiddler_title);
        // Skip excluded tiddlers (defense-in-depth — JS also filters)
        if !conflict::ConflictManager::should_sync_tiddler(tiddler_title) {
            eprintln!("[Collab] Skipping excluded tiddler: {}", tiddler_title);
            return;
        }
        // Track locally so we can re-broadcast on peer reconnect
        if let Ok(mut local) = self.local_collab_editors.lock() {
            local.insert((wiki_id.to_string(), tiddler_title.to_string()));
        }
        let device_id = self.pairing_manager.device_id().to_string();
        let device_name = self.pairing_manager.device_name().to_string();
        let user_name = self.local_user_name.lock().ok()
            .and_then(|g| g.clone())
            .unwrap_or_default();
        let _ = self.wiki_tx.send(WikiToSync::CollabEditingStarted {
            wiki_id: wiki_id.to_string(),
            tiddler_title: tiddler_title.to_string(),
            device_id,
            device_name,
            user_name,
        });
    }

    /// Notify peers that we stopped editing a tiddler
    pub fn notify_collab_editing_stopped(&self, wiki_id: &str, tiddler_title: &str) {
        eprintln!("[Collab] notify_collab_editing_stopped: wiki={}, tiddler={}", wiki_id, tiddler_title);
        // Remove from local tracking
        if let Ok(mut local) = self.local_collab_editors.lock() {
            local.remove(&(wiki_id.to_string(), tiddler_title.to_string()));
        }
        let device_id = self.pairing_manager.device_id().to_string();
        let _ = self.wiki_tx.send(WikiToSync::CollabEditingStopped {
            wiki_id: wiki_id.to_string(),
            tiddler_title: tiddler_title.to_string(),
            device_id,
        });
    }

    /// Notify peers that we saved a tiddler being collaboratively edited
    pub fn notify_collab_peer_saved(&self, wiki_id: &str, tiddler_title: &str, saved_title: &str) {
        eprintln!("[Collab] notify_collab_peer_saved: wiki={}, tiddler={}, saved_as={}", wiki_id, tiddler_title, saved_title);
        let device_id = self.pairing_manager.device_id().to_string();
        let device_name = self.pairing_manager.device_name().to_string();
        let _ = self.wiki_tx.send(WikiToSync::CollabPeerSaved {
            wiki_id: wiki_id.to_string(),
            tiddler_title: tiddler_title.to_string(),
            saved_title: saved_title.to_string(),
            device_id,
            device_name,
        });
    }

    /// Send a Yjs document update to peers
    pub fn send_collab_update(&self, wiki_id: &str, tiddler_title: &str, update_base64: &str) {
        if !conflict::ConflictManager::should_sync_tiddler(tiddler_title) { return; }
        eprintln!("[Collab] send_collab_update: wiki={}, tiddler={}, len={}", wiki_id, tiddler_title, update_base64.len());
        let _ = self.wiki_tx.send(WikiToSync::CollabUpdate {
            wiki_id: wiki_id.to_string(),
            tiddler_title: tiddler_title.to_string(),
            update_base64: update_base64.to_string(),
        });
    }

    /// Send a Yjs awareness update to peers
    pub fn send_collab_awareness(&self, wiki_id: &str, tiddler_title: &str, update_base64: &str) {
        if !conflict::ConflictManager::should_sync_tiddler(tiddler_title) { return; }
        eprintln!("[Collab] send_collab_awareness: wiki={}, tiddler={}, len={}", wiki_id, tiddler_title, update_base64.len());
        let _ = self.wiki_tx.send(WikiToSync::CollabAwareness {
            wiki_id: wiki_id.to_string(),
            tiddler_title: tiddler_title.to_string(),
            update_base64: update_base64.to_string(),
        });
    }

    /// Get the device_id that most recently sent us awareness for this tiddler (within 500ms).
    /// Used by the bridge/relay to exclude that peer from the outbound broadcast (echo suppression).
    pub fn get_awareness_echo_peer(&self, wiki_id: &str, tiddler_title: &str) -> Option<String> {
        let key = (wiki_id.to_string(), tiddler_title.to_string());
        if let Ok(map) = self.awareness_last_inbound.lock() {
            if let Some((device_id, ts)) = map.get(&key) {
                if ts.elapsed().as_millis() < 500 {
                    return Some(device_id.clone());
                }
            }
        }
        None
    }

    /// Get remote editors for a tiddler (device_id, device_name, user_name)
    pub fn get_remote_editors(&self, wiki_id: &str, tiddler_title: &str) -> Vec<(String, String, String)> {
        let key = (wiki_id.to_string(), tiddler_title.to_string());
        if let Ok(editors) = self.collab_editors.lock() {
            editors
                .get(&key)
                .map(|set| set.iter().cloned().collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    /// Get the collab WebSocket server port (0 if not running)
    pub fn get_collab_ws_port(&self) -> u16 {
        self.collab_ws_port.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Send a JSON message to all collab WebSocket clients for a wiki
    fn send_collab_ws_message(&self, wiki_id: &str, msg: &str) {
        if let Ok(clients) = self.collab_ws_clients.lock() {
            if let Some(senders) = clients.get(wiki_id) {
                for sender in senders {
                    let _ = sender.send(msg.to_string());
                }
            }
        }
    }

    /// Send a collab message to JS via WebSocket (preferred) or IPC fallback
    fn emit_collab_to_wiki(&self, wiki_id: &str, payload: serde_json::Value) {
        let msg = serde_json::to_string(&payload).unwrap_or_default();

        // Try collab WebSocket first (low-latency push)
        let mut sent_ws = false;
        if let Ok(mut clients) = self.collab_ws_clients.lock() {
            if let Some(senders) = clients.get_mut(wiki_id) {
                // Proactively remove dead senders (channel receiver dropped)
                senders.retain(|s| !s.is_closed());
                if senders.is_empty() {
                    clients.remove(wiki_id);
                } else {
                    // Only count as sent if at least one send actually succeeded
                    let mut any_ok = false;
                    for sender in senders.iter() {
                        if sender.send(msg.clone()).is_ok() {
                            any_ok = true;
                        }
                    }
                    if any_ok {
                        sent_ws = true;
                    }
                }
            }
        }

        // Fallback to regular emit_to_wiki (IPC on desktop, bridge on Android)
        if !sent_ws {
            Self::emit_to_wiki(wiki_id, "lan-sync-collab", payload);
        }
    }

    /// Start the local collab WebSocket server (called from start())
    #[cfg(not(target_os = "android"))]
    async fn start_collab_ws_server(self: &Arc<Self>) {
        use tokio::net::TcpListener;

        let listener = match TcpListener::bind("127.0.0.1:0").await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[Collab WS] Failed to bind: {}", e);
                return;
            }
        };
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
        if port == 0 {
            eprintln!("[Collab WS] Bound to port 0, aborting");
            return;
        }
        self.collab_ws_port.store(port, std::sync::atomic::Ordering::Relaxed);
        eprintln!("[Collab WS] Server listening on port {}", port);

        let mgr = Arc::clone(self);
        let mut shutdown_rx = self.collab_ws_shutdown.subscribe();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok((stream, addr)) => {
                                eprintln!("[Collab WS] New connection from {}", addr);
                                let mgr2 = Arc::clone(&mgr);
                                tokio::spawn(async move {
                                    mgr2.handle_collab_ws_connection(stream).await;
                                });
                            }
                            Err(e) => {
                                eprintln!("[Collab WS] Accept error: {}", e);
                            }
                        }
                    }
                    _ = shutdown_rx.changed() => {
                        eprintln!("[Collab WS] Shutdown signal received");
                        break;
                    }
                }
            }
        });
    }

    /// Handle a single collab WebSocket connection
    #[cfg(not(target_os = "android"))]
    async fn handle_collab_ws_connection(self: Arc<Self>, stream: tokio::net::TcpStream) {
        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::accept_async;

        let ws_stream = match accept_async(stream).await {
            Ok(ws) => ws,
            Err(e) => {
                eprintln!("[Collab WS] Handshake error: {}", e);
                return;
            }
        };

        let (mut ws_sender, mut ws_receiver) = ws_stream.split();

        // Create a channel for outbound messages (Rust → JS)
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();

        // Wiki ID is set when the client sends an "identify" message
        let wiki_id: Arc<tokio::sync::Mutex<Option<String>>> = Arc::new(tokio::sync::Mutex::new(None));

        // Spawn outbound message forwarder with periodic WebSocket-level pings.
        // Pings detect zombie connections (e.g. JS side went to background and OS
        // killed the TCP connection silently). Without pings, the dead sender stays
        // in collab_ws_clients and silently drops messages (IPC fallback is skipped).
        let wiki_id_out = Arc::clone(&wiki_id);
        let outbound = tokio::spawn(async move {
            let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(30));
            ping_interval.tick().await; // skip immediate first tick
            loop {
                tokio::select! {
                    biased;
                    msg = rx.recv() => {
                        match msg {
                            Some(msg) => {
                                if ws_sender
                                    .send(tokio_tungstenite::tungstenite::Message::Text(msg.into()))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                    _ = ping_interval.tick() => {
                        // Send WebSocket protocol-level ping; browser auto-responds with pong.
                        // If send fails, connection is dead.
                        if ws_sender
                            .send(tokio_tungstenite::tungstenite::Message::Ping(vec![].into()))
                            .await
                            .is_err()
                        {
                            eprintln!("[Collab WS] Ping send failed — connection dead");
                            break;
                        }
                    }
                }
            }
            let _ = wiki_id_out; // prevent warning
        });

        // Process inbound messages
        while let Some(msg) = ws_receiver.next().await {
            let msg = match msg {
                Ok(m) => m,
                Err(_) => break,
            };

            if msg.is_close() {
                break;
            }

            // Skip pong frames (response to our pings — handled by tungstenite)
            if msg.is_pong() {
                continue;
            }

            let text = match msg.into_text() {
                Ok(t) => t,
                Err(_) => continue,
            };

            // Parse JSON message
            let json: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let msg_type = json["type"].as_str().unwrap_or("");

            match msg_type {
                // Application-level ping from JS (used on visibility restore to verify connection)
                "ping" => {
                    let _ = tx.send(r#"{"type":"pong"}"#.to_string());
                }
                "identify" => {
                    let wid = json["wiki_id"].as_str().unwrap_or("").to_string();
                    if !wid.is_empty() {
                        eprintln!("[Collab WS] Client identified for wiki: {}", wid);
                        // Register this sender for the wiki
                        if let Ok(mut clients) = self.collab_ws_clients.lock() {
                            clients.entry(wid.clone()).or_insert_with(Vec::new).push(tx.clone());
                        }
                        // Send current remote editing sessions so the client knows immediately
                        if let Ok(editors) = self.collab_editors.lock() {
                            for ((eid_wiki, eid_title), device_set) in editors.iter() {
                                if eid_wiki == &wid {
                                    for (device_id, device_name, user_name) in device_set {
                                        let msg = serde_json::json!({
                                            "type": "editing-started",
                                            "wiki_id": eid_wiki,
                                            "tiddler_title": eid_title,
                                            "device_id": device_id,
                                            "device_name": device_name,
                                            "user_name": user_name,
                                        });
                                        let _ = tx.send(msg.to_string());
                                    }
                                }
                            }
                        }
                        *wiki_id.lock().await = Some(wid);
                    }
                }
                "startEditing" => {
                    let wid = json["wiki_id"].as_str().unwrap_or("");
                    let title = json["tiddler_title"].as_str().unwrap_or("");
                    if !wid.is_empty() && !title.is_empty() {
                        self.notify_collab_editing_started(wid, title);
                    }
                }
                "stopEditing" => {
                    let wid = json["wiki_id"].as_str().unwrap_or("");
                    let title = json["tiddler_title"].as_str().unwrap_or("");
                    if !wid.is_empty() && !title.is_empty() {
                        self.notify_collab_editing_stopped(wid, title);
                    }
                }
                "sendUpdate" => {
                    let wid = json["wiki_id"].as_str().unwrap_or("");
                    let title = json["tiddler_title"].as_str().unwrap_or("");
                    let data = json["update_base64"].as_str().unwrap_or("");
                    if !wid.is_empty() && !title.is_empty() && !data.is_empty() {
                        self.send_collab_update(wid, title, data);
                    }
                }
                "sendAwareness" => {
                    let wid = json["wiki_id"].as_str().unwrap_or("");
                    let title = json["tiddler_title"].as_str().unwrap_or("");
                    let data = json["update_base64"].as_str().unwrap_or("");
                    if !wid.is_empty() && !title.is_empty() && !data.is_empty() {
                        self.send_collab_awareness(wid, title, data);
                    }
                }
                "peerSaved" => {
                    let wid = json["wiki_id"].as_str().unwrap_or("");
                    let title = json["tiddler_title"].as_str().unwrap_or("");
                    let saved = json["saved_title"].as_str().unwrap_or("");
                    if !wid.is_empty() && !title.is_empty() && !saved.is_empty() {
                        self.notify_collab_peer_saved(wid, title, saved);
                    }
                }
                _ => {}
            }
        }

        // Cleanup: remove this sender from collab_ws_clients
        if let Some(wid) = wiki_id.lock().await.as_ref() {
            if let Ok(mut clients) = self.collab_ws_clients.lock() {
                if let Some(senders) = clients.get_mut(wid) {
                    senders.retain(|s| !s.is_closed());
                    if senders.is_empty() {
                        clients.remove(wid);
                    }
                }
            }
        }

        outbound.abort();
        eprintln!("[Collab WS] Connection closed");
    }

    /// Main event loop processing server events, wiki changes, and attachment events
    #[cfg(not(target_os = "android"))]
    async fn run_event_loop(
        self: Arc<Self>,
        mut event_rx: mpsc::UnboundedReceiver<ServerEvent>,
        mut wiki_rx: mpsc::UnboundedReceiver<WikiToSync>,
        att_rx: Option<mpsc::UnboundedReceiver<attachments::AttachmentEvent>>,
    ) {
        // Drain the sync-to-wiki channel and emit Tauri events
        let emit_task = self.spawn_emit_task();

        // Wrap att_rx in a struct so we can poll it in select!
        let mut att_rx = att_rx;

        // Periodic timer for fingerprint timeouts and dirty state flushing
        let mut maintenance_interval = tokio::time::interval(std::time::Duration::from_secs(15));
        maintenance_interval.tick().await; // skip immediate first tick

        loop {
            // Create a future that resolves when an attachment event is available,
            // or never resolves if there's no watcher
            let att_event = async {
                if let Some(ref mut rx) = att_rx {
                    rx.recv().await
                } else {
                    // Never resolves — no watcher active
                    std::future::pending::<Option<attachments::AttachmentEvent>>().await
                }
            };

            tokio::select! {
                Some(event) = event_rx.recv() => {
                    self.handle_server_event(event).await;
                }
                Some(change) = wiki_rx.recv() => {
                    // Skip all outbound sync if no peers are connected
                    if !self.is_sync_active().await {
                        continue;
                    }
                    // Check for _canonical_uri before consuming the change
                    let attachment_info = match &change {
                        WikiToSync::TiddlerChanged { wiki_id, tiddler_json, .. } => {
                            attachments::AttachmentManager::extract_canonical_uri(tiddler_json)
                                .map(|uri| (wiki_id.clone(), uri))
                        }
                        _ => None,
                    };
                    if let Some(ref server) = *self.server.read().await {
                        // LAN server running: bridge handles LAN peers + conflict_manager
                        let change_for_relay = change.clone();
                        self.bridge.handle_local_change(
                            change,
                            &self.conflict_manager,
                            server,
                        ).await;
                        // Collect LAN peer device IDs to exclude from relay routing
                        // (they already received the message via LAN)
                        let lan_peer_ids: std::collections::HashSet<String> = server
                            .lan_connected_peers()
                            .await
                            .into_iter()
                            .map(|(id, _name)| id)
                            .collect();
                        // Additionally route through relay rooms, skipping LAN-reachable peers
                        self.relay_route_change(&change_for_relay, &lan_peer_ids).await;
                    } else {
                        // Relay-only: handle conflict_manager + relay routing
                        self.handle_local_change_relay(change).await;
                    }
                    // If tiddler had _canonical_uri, send just that specific file to all peers
                    if let Some((wiki_id, canonical_uri)) = attachment_info {
                        self.broadcast_single_attachment(&wiki_id, &canonical_uri).await;
                    }
                }
                Some(att_event) = att_event => {
                    self.handle_attachment_event(att_event).await;
                }
                _ = maintenance_interval.tick() => {
                    self.check_fingerprint_timeouts().await;
                    self.conflict_manager.flush_dirty_states();
                    self.conflict_manager.prune_tombstones();
                }
                else => break,
            }
        }

        emit_task.abort();
    }

    /// Main event loop (Android — no attachment watcher, uses periodic scanning instead)
    #[cfg(target_os = "android")]
    async fn run_event_loop(
        self: Arc<Self>,
        mut event_rx: mpsc::UnboundedReceiver<ServerEvent>,
        mut wiki_rx: mpsc::UnboundedReceiver<WikiToSync>,
    ) {
        let emit_task = self.spawn_emit_task();

        let mut maintenance_interval = tokio::time::interval(std::time::Duration::from_secs(15));
        maintenance_interval.tick().await;

        let mut attachment_scan_interval = tokio::time::interval(std::time::Duration::from_secs(30));
        attachment_scan_interval.tick().await;

        loop {
            tokio::select! {
                Some(event) = event_rx.recv() => {
                    self.handle_server_event(event).await;
                }
                Some(change) = wiki_rx.recv() => {
                    // Skip all outbound sync if no peers are connected
                    if !self.is_sync_active().await {
                        continue;
                    }
                    // Check for _canonical_uri before consuming the change
                    let attachment_info = match &change {
                        WikiToSync::TiddlerChanged { wiki_id, tiddler_json, .. } => {
                            attachments::AttachmentManager::extract_canonical_uri(tiddler_json)
                                .map(|uri| (wiki_id.clone(), uri))
                        }
                        _ => None,
                    };
                    // Extract wiki_id from WikiOpened so we can trigger fingerprint
                    // sync after the manifest is broadcast (catch-up for reopened wikis)
                    let opened_wiki_id = match &change {
                        WikiToSync::WikiOpened { wiki_id, is_folder, .. } if *is_folder => Some(wiki_id.clone()),
                        WikiToSync::WikiOpened { wiki_id, .. } => Some(wiki_id.clone()),
                        _ => None,
                    };
                    // For folder wikis, read tiddlywiki.info and pass to sync
                    let folder_wiki_info = match &change {
                        WikiToSync::WikiOpened { wiki_id, is_folder: true, .. } => {
                            if let Some(app) = crate::GLOBAL_APP_HANDLE.get() {
                                crate::wiki_storage::get_wiki_path_by_sync_id(app, wiki_id)
                                    .and_then(|wp| {
                                        let info_path = std::path::PathBuf::from(&wp).join("tiddlywiki.info");
                                        std::fs::read_to_string(&info_path).ok()
                                    })
                                    .map(|content| (wiki_id.clone(), content))
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };
                    if let Some(ref server) = *self.server.read().await {
                        // LAN server running: bridge handles LAN peers + conflict_manager
                        let change_for_relay = change.clone();
                        self.bridge.handle_local_change(
                            change,
                            &self.conflict_manager,
                            server,
                        ).await;
                        // Collect LAN peer device IDs to exclude from relay routing
                        let lan_peer_ids: std::collections::HashSet<String> = server
                            .lan_connected_peers()
                            .await
                            .into_iter()
                            .map(|(id, _name)| id)
                            .collect();
                        // Additionally route through relay rooms, skipping LAN-reachable peers
                        self.relay_route_change(&change_for_relay, &lan_peer_ids).await;
                    } else {
                        // Relay-only: handle conflict_manager + relay routing
                        self.handle_local_change_relay(change).await;
                    }
                    // If tiddler had _canonical_uri, send just that specific file to all peers
                    if let Some((wiki_id, canonical_uri)) = attachment_info {
                        self.broadcast_single_attachment(&wiki_id, &canonical_uri).await;
                    }
                    // Trigger fingerprint-based catch-up sync when a wiki opens.
                    // on_wiki_opened sends our fingerprints to each connected peer
                    // so they can compare and send back tiddlers we're missing.
                    if let Some(wiki_id) = opened_wiki_id {
                        self.on_wiki_opened(&wiki_id).await;
                    }
                    // Register tiddlywiki.info for folder wikis (for sync)
                    if let Some((wiki_id, content)) = folder_wiki_info {
                        self.set_wiki_info(&wiki_id, &content).await;
                    }
                }
                _ = maintenance_interval.tick() => {
                    self.check_fingerprint_timeouts().await;
                    self.conflict_manager.flush_dirty_states();
                    self.conflict_manager.prune_tombstones();
                }
                _ = attachment_scan_interval.tick() => {
                    self.scan_android_attachments().await;
                }
                else => break,
            }
        }

        emit_task.abort();
    }

    /// Check for fingerprint requests that have timed out (no response from JS within 15s)
    async fn check_fingerprint_timeouts(&self) {
        let mut pending = self.pending_fingerprint_requests.write().await;
        let now = std::time::Instant::now();
        let timeout = std::time::Duration::from_secs(15);

        pending.retain(|(wiki_id, device_id), requested_at| {
            if now.duration_since(*requested_at) > timeout {
                eprintln!(
                    "[LAN Sync] Fingerprint request timed out for wiki {} to device {} — skipping (peer will sync incrementally)",
                    wiki_id, device_id
                );
                false
            } else {
                true
            }
        });
    }

    /// Spawn the task that emits sync-to-wiki events.
    /// On desktop: emits Tauri events (received by all wiki windows in the same process).
    /// On Android: queues changes to the Android bridge (polled by JS in :wiki process).
    ///
    /// Vector clocks are merged only after confirmed delivery to IPC clients.
    /// If no clients are connected (message buffered), clocks are stored in
    /// pending_wiki_clocks and merged when on_wiki_opened drains the buffer.
    fn spawn_emit_task(&self) -> tokio::task::JoinHandle<()> {
        let sync_to_wiki_rx = self.bridge.sync_to_wiki_rx.clone();
        let conflict_manager = self.conflict_manager.clone();

        tokio::spawn(async move {
            let mut rx = sync_to_wiki_rx.lock().await;
            while let Some(msg) = rx.recv().await {
                match msg {
                    SyncToWiki::ApplyTiddlerChange { wiki_id, title, tiddler_json, vector_clock } => {
                        let payload = serde_json::json!({
                            "type": "apply-change",
                            "wiki_id": wiki_id,
                            "title": title,
                            "tiddler_json": tiddler_json,
                        });
                        let sent = Self::emit_to_wiki(&wiki_id, "lan-sync-apply-change", payload);
                        // Accept vector clock only after confirmed delivery
                        if let Some(clock) = vector_clock {
                            if sent > 0 {
                                conflict_manager.accept_remote_change(&wiki_id, &title, &clock);
                            } else {
                                // Buffer clock for acceptance when wiki JS connects
                                #[cfg(not(target_os = "android"))]
                                if let Some(mgr) = get_sync_manager() {
                                    if let Ok(mut buf) = mgr.pending_wiki_clocks.lock() {
                                        let vec = buf.entry(wiki_id.clone()).or_insert_with(Vec::new);
                                        vec.push((title, clock, false));
                                    }
                                }
                            }
                        }
                    }
                    SyncToWiki::ApplyTiddlerDeletion { wiki_id, title, vector_clock } => {
                        let payload = serde_json::json!({
                            "type": "apply-deletion",
                            "wiki_id": wiki_id,
                            "title": title,
                        });
                        let sent = Self::emit_to_wiki(&wiki_id, "lan-sync-apply-deletion", payload);
                        // Accept vector clock only after confirmed delivery
                        if let Some(clock) = vector_clock {
                            if sent > 0 {
                                conflict_manager.accept_remote_deletion(&wiki_id, &title, &clock);
                            } else {
                                #[cfg(not(target_os = "android"))]
                                if let Some(mgr) = get_sync_manager() {
                                    if let Ok(mut buf) = mgr.pending_wiki_clocks.lock() {
                                        let vec = buf.entry(wiki_id.clone()).or_insert_with(Vec::new);
                                        vec.push((title, clock, true));
                                    }
                                }
                            }
                        }
                    }
                    SyncToWiki::SaveConflict { wiki_id, title, .. } => {
                        let payload = serde_json::json!({
                            "type": "conflict",
                            "wiki_id": wiki_id,
                            "title": title,
                        });
                        Self::emit_to_wiki(&wiki_id, "lan-sync-conflict", payload);
                    }
                }
            }
        })
    }

    /// Emit a sync event to wiki windows.
    /// Desktop: IPC to wiki processes. Android: queue to bridge for JS polling.
    /// Returns the number of IPC clients that received the message (0 = buffered).
    fn emit_to_wiki(wiki_id: &str, _event_name: &str, payload: serde_json::Value) -> usize {
        // On Android, queue to bridge for JS polling
        #[cfg(target_os = "android")]
        {
            let event_type = payload["type"].as_str().unwrap_or("unknown");
            eprintln!("[LAN Sync] emit_to_wiki (Android): wiki_id={}, event={}", wiki_id, event_type);
            if let Some(mgr) = get_sync_manager() {
                if let Ok(guard) = mgr.android_bridge.lock() {
                    if let Some(ref bridge) = *guard {
                        bridge.queue_change(wiki_id, payload.clone());
                        eprintln!("[LAN Sync] emit_to_wiki (Android): queued to bridge");
                        return 1; // Android bridge always accepts
                    } else {
                        eprintln!("[LAN Sync] emit_to_wiki (Android): bridge is None!");
                    }
                }
            }
            return 0;
        }

        // On desktop, send via IPC to wiki processes (they're separate processes)
        #[cfg(not(target_os = "android"))]
        {
            let event_type = payload["type"].as_str().unwrap_or("unknown");
            eprintln!("[LAN Sync] emit_to_wiki: wiki_id={}, event={}", wiki_id, event_type);
            let payload_json = serde_json::to_string(&payload).unwrap_or_default();

            let sent_count = if let Some(server) = crate::GLOBAL_IPC_SERVER.get() {
                server.send_lan_sync_to_all(wiki_id, &payload_json)
            } else {
                eprintln!("[LAN Sync] emit_to_wiki: GLOBAL_IPC_SERVER not set!");
                0
            };

            // Buffer if no IPC clients received the message (wiki not yet open,
            // or same-process on Linux/macOS — send_lan_sync_to_all already
            // pushed to IPC_SYNC_QUEUE as fallback)
            if sent_count == 0 {
                if let Some(mgr) = get_sync_manager() {
                    let mut buffered = false;
                    // First try the pre_sync_buffer (active after pre_request_sync)
                    if let Ok(mut buf) = mgr.pre_sync_buffer.lock() {
                        if let Some(vec) = buf.get_mut(wiki_id) {
                            if vec.len() < 5000 {
                                vec.push(payload_json.clone());
                                buffered = true;
                            }
                        }
                    }
                    // If not buffered there, store apply-change/apply-deletion
                    // in the always-on pending buffer so tiddler data isn't lost
                    if !buffered && (event_type == "apply-change" || event_type == "apply-deletion") {
                        if let Ok(mut buf) = mgr.pending_wiki_changes.lock() {
                            let vec = buf.entry(wiki_id.to_string()).or_insert_with(Vec::new);
                            if vec.len() < 5000 {
                                vec.push(payload_json);
                            }
                        }
                    }
                }
            }

            sent_count
        }
    }

    /// Handle an attachment event from the file watcher
    #[cfg(not(target_os = "android"))]
    async fn handle_attachment_event(&self, event: attachments::AttachmentEvent) {
        // Allow attachment sync if we have any connected peers
        if !self.is_sync_active().await {
            return;
        }
        match event {
            attachments::AttachmentEvent::Changed { wiki_id, rel_path } => {
                // Block disallowed file types from being synced outbound
                if !attachments::is_allowed_attachment(&rel_path) {
                    eprintln!(
                        "[LAN Sync] Skipping outbound sync of '{}' — file type not allowed",
                        rel_path
                    );
                    return;
                }
                // Check if this file was recently received from sync (suppress echo)
                if self.attachment_manager.should_suppress(&wiki_id, &rel_path) {
                    return;
                }
                // Get all peers (LAN + relay) for this wiki
                let all_peers = self.get_all_peers_for_wiki(&wiki_id).await;
                if all_peers.is_empty() {
                    return;
                }
                eprintln!(
                    "[LAN Sync] Attachment changed (watcher): {} in wiki {} — sending to {} peers",
                    rel_path, wiki_id, all_peers.len()
                );
                if let Some(ref server) = *self.server.read().await {
                    // Send to LAN peers in this wiki's room via optimized prepare_outbound path
                    let all_wiki_peers = self.get_all_peers_for_wiki(&wiki_id).await;
                    let lan_peer_ids: HashSet<String> = self.peers.read().await.keys().cloned().collect();
                    let lan_peers: Vec<String> = all_wiki_peers.into_iter()
                        .filter(|p| lan_peer_ids.contains(p))
                        .collect();
                    if !lan_peers.is_empty() {
                        if let Err(e) = self
                            .attachment_manager
                            .prepare_outbound(&wiki_id, &rel_path, server, Some(&lan_peers))
                            .await
                        {
                            eprintln!("[LAN Sync] Failed to sync changed attachment via LAN: {}", e);
                        }
                    }
                }
                // Send to relay peers via send_attachment_to_peer (which uses send_to_peer_any)
                let relay_peers = self.get_relay_only_peers_for_wiki(&wiki_id).await;
                if !relay_peers.is_empty() {
                    let wiki_path = if let Some(app) = GLOBAL_APP_HANDLE.get() {
                        crate::wiki_storage::get_wiki_path_by_sync_id(app, &wiki_id)
                    } else {
                        None
                    };
                    if let Some(wp) = wiki_path {
                        let all_entries = collect_attachment_entries(&wp);
                        if let Some(entry) = all_entries.into_iter().find(|e| e.rel_path == rel_path) {
                            for peer_id in &relay_peers {
                                if let Err(e) = send_attachment_to_peer(&entry, &wiki_id, peer_id, self).await {
                                    eprintln!("[LAN Sync] Failed to sync attachment {} to relay peer {}: {}", rel_path, peer_id, e);
                                }
                            }
                        }
                    }
                }
            }
            attachments::AttachmentEvent::Deleted { wiki_id, rel_path } => {
                // Block disallowed file types
                if !attachments::is_allowed_attachment(&rel_path) {
                    return;
                }
                // Check if this deletion was from incoming sync
                if self.attachment_manager.should_suppress(&wiki_id, &rel_path) {
                    return;
                }
                // Get all peers (LAN + relay) for this wiki
                let all_peers = self.get_all_peers_for_wiki(&wiki_id).await;
                if all_peers.is_empty() {
                    return;
                }
                eprintln!(
                    "[LAN Sync] Attachment deleted (watcher): {} in wiki {}",
                    rel_path, wiki_id
                );
                let msg = SyncMessage::AttachmentDeleted {
                    wiki_id: wiki_id.clone(),
                    filename: rel_path.clone(),
                };
                self.send_to_peers_any(&all_peers, &msg).await;
            }
        }
    }

    /// Handle a server event
    async fn handle_server_event(&self, event: ServerEvent) {
        match event {
            ServerEvent::PeerConnected {
                device_id,
                device_name,
            } => {
                eprintln!(
                    "[LAN Sync] Peer connected: {} ({})",
                    device_name, device_id
                );
                // Cancel any active reconnection task for this peer
                if let Some(handle) = self.reconnect_tasks.write().await.remove(&device_id) {
                    handle.abort();
                    eprintln!("[LAN Sync] Cancelled reconnection task for {}", device_id);
                }
                // Track in connected set (shared with discovery thread to prevent timeout)
                if let Ok(mut set) = self.connected_peer_ids.write() {
                    set.insert(device_id.clone());
                }

                // The initial connection authenticated for only one room, but
                // we may share additional rooms with this peer.  Add all shared
                // rooms to auth_room_codes now so manifests and peer counts
                // are correct from the start.
                {
                    let our_rooms = self.active_room_codes.read()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    let peer_hashes = self.peer_room_hashes.read().await;
                    if let Some(hashes) = peer_hashes.get(&device_id) {
                        let shared = protocol::select_all_shared_rooms_by_hash(&our_rooms, hashes);
                        drop(peer_hashes);
                        if shared.len() > 1 {
                            if let Some(ref server) = *self.server.read().await {
                                server.add_peer_room_codes(&device_id, &shared).await;
                                eprintln!(
                                    "[LAN Sync] Peer {} connected — set all {} shared room codes: {:?}",
                                    &device_id[..8.min(device_id.len())], shared.len(), shared
                                );
                            }
                        }
                    }
                }

                if let Some(app) = GLOBAL_APP_HANDLE.get() {
                    let _ = app.emit("lan-sync-peer-connected", serde_json::json!({
                        "device_id": device_id,
                        "device_name": device_name,
                    }));
                    let _ = app.emit("lan-sync-peers-updated", serde_json::json!({}));
                }

                // Immediately push updated peer lists to wiki processes via IPC
                #[cfg(not(target_os = "android"))]
                self.push_peer_updates_to_ipc().await;

                // Send WikiManifest to the newly connected peer
                self.send_wiki_manifest_to_peer(&device_id).await;

                // Send cached tiddlywiki.info for folder wikis
                self.send_wiki_info_to_peer(&device_id).await;

                // Send our username if we have one
                let local_name = self.local_user_name.lock().ok().and_then(|g| g.clone());
                if let Some(name) = local_name {
                    let msg = SyncMessage::UserNameAnnounce {
                        user_name: name,
                    };
                    if let Err(e) = self.send_to_peer_any(&device_id, &msg).await {
                        eprintln!("[LAN Sync] Failed to send UserNameAnnounce to {}: {}", device_id, e);
                    }
                }

                // Re-broadcast local collab editing sessions so the peer knows what we're editing
                if let Ok(local) = self.local_collab_editors.lock() {
                    if !local.is_empty() {
                        eprintln!("[Collab] Re-broadcasting {} local editing sessions to reconnected peer {}", local.len(), device_id);
                        let my_device_id = self.pairing_manager.device_id().to_string();
                        let my_device_name = self.pairing_manager.device_name().to_string();
                        let my_user_name = self.local_user_name.lock().ok()
                            .and_then(|g| g.clone())
                            .unwrap_or_default();
                        for (wiki_id, tiddler_title) in local.iter() {
                            let _ = self.wiki_tx.send(WikiToSync::CollabEditingStarted {
                                wiki_id: wiki_id.clone(),
                                tiddler_title: tiddler_title.clone(),
                                device_id: my_device_id.clone(),
                                device_name: my_device_name.clone(),
                                user_name: my_user_name.clone(),
                            });
                        }
                    }
                }
            }
            ServerEvent::PeerDisconnected { device_id } => {
                eprintln!("[LAN Sync] Peer disconnected: {}", device_id);
                // Remove from connected set
                if let Ok(mut set) = self.connected_peer_ids.write() {
                    set.remove(&device_id);
                }
                // Remove remote wikis from this peer
                self.remote_wikis.write().await.remove(&device_id);

                // Clean up collab editors from this peer and emit editing-stopped events
                if let Ok(mut editors) = self.collab_editors.lock() {
                    let keys: Vec<(String, String)> = editors.keys().cloned().collect();
                    for key in keys {
                        if let Some(set) = editors.get_mut(&key) {
                            let removed: Vec<(String, String, String)> = set.iter()
                                .filter(|(did, _, _)| did == &device_id)
                                .cloned()
                                .collect();
                            for (did, _, _) in &removed {
                                set.retain(|(d, _, _)| d != did);
                                // Emit editing-stopped for each removed editor
                                self.emit_collab_to_wiki(&key.0, serde_json::json!({
                                    "type": "editing-stopped",
                                    "wiki_id": key.0,
                                    "tiddler_title": key.1,
                                    "device_id": did,
                                }));
                            }
                            if set.is_empty() {
                                editors.remove(&key);
                            }
                        }
                    }
                }

                // Immediately push updated peer lists to wiki processes via IPC
                #[cfg(not(target_os = "android"))]
                self.push_peer_updates_to_ipc().await;

                if let Some(app) = GLOBAL_APP_HANDLE.get() {
                    let _ = app.emit("lan-sync-peer-disconnected", serde_json::json!({
                        "device_id": device_id,
                    }));
                    let _ = app.emit("lan-sync-peers-updated", serde_json::json!({}));
                    // Emit updated available wikis
                    let available = self.get_available_remote_wikis().await;
                    let _ = app.emit("lan-sync-remote-wikis-updated", &available);
                }

                // Auto-reconnect to room peers with exponential backoff.
                // Only the device with the smaller device ID initiates reconnection.
                // Look up the room this peer was connected via.
                let peer_room_code = {
                    // Match rooms by comparing hashes (never uses cleartext codes)
                    let our_rooms = self.active_room_codes.read()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    let peer_hashes_guard = self.peer_room_hashes.read().await;
                    peer_hashes_guard.get(&device_id)
                        .filter(|h| !h.is_empty())
                        .and_then(|h| protocol::select_shared_room_by_hash(&our_rooms, h))
                };
                if let Some(room_code) = peer_room_code {
                    if self.server.read().await.is_some() {
                        let our_id = self.pairing_manager.device_id().to_string();
                        if our_id > device_id {
                            eprintln!("[LAN Sync] We have larger ID — waiting for peer {} to reconnect to us", device_id);
                        } else {
                            let last_addr = self.last_known_addrs.read().await.get(&device_id).cloned();
                            let group_key = self.room_keys.read().await.get(&room_code).copied();
                            if let (Some((addr, port)), Some(gk)) = (last_addr, group_key) {
                                let peers = self.peers.clone();
                                let etx = self.event_tx.clone();
                                let did = device_id.clone();
                                let our_did = our_id.clone();
                                let our_dn = self.pairing_manager.device_name().to_string();
                                let rc = room_code.clone();
                                let handle = tokio::spawn(async move {
                                    let delays = [2u64, 4, 8, 16, 30, 30, 30, 30, 30, 30];
                                    for (attempt, &delay) in delays.iter().enumerate() {
                                        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
                                        if peers.read().await.contains_key(&did) {
                                            eprintln!("[LAN Sync] Peer {} already reconnected, stopping retry", did);
                                            return;
                                        }
                                        eprintln!(
                                            "[LAN Sync] Reconnect attempt {} for peer {} via room {} at {}:{}",
                                            attempt + 1, did, rc, addr, port
                                        );
                                        match client::connect_to_room_peer(
                                            &addr, port, &did, &our_did, &our_dn,
                                            &rc, &gk, peers.clone(), etx.clone(),
                                        ).await {
                                            Ok(()) => {
                                                eprintln!("[LAN Sync] Reconnected to peer {}", did);
                                                return;
                                            }
                                            Err(e) => {
                                                eprintln!("[LAN Sync] Reconnect failed for {}: {}", did, e);
                                            }
                                        }
                                    }
                                    eprintln!("[LAN Sync] Giving up reconnection to peer {} after {} attempts", did, delays.len());
                                });
                                self.reconnect_tasks.write().await.insert(device_id, handle);
                            }
                        }
                    }
                }
            }
            ServerEvent::SyncMessageReceived {
                from_device_id,
                message,
            } => {
                // Log message type for diagnostics
                let msg_type = match &message {
                    SyncMessage::TiddlerChanged { title, .. } => format!("TiddlerChanged({})", title),
                    SyncMessage::TiddlerDeleted { title, .. } => format!("TiddlerDeleted({})", title),
                    SyncMessage::FullSyncBatch { tiddlers, is_last_batch, .. } => format!("FullSyncBatch({} tiddlers, last={})", tiddlers.len(), is_last_batch),
                    SyncMessage::TiddlerFingerprints { fingerprints, .. } => format!("TiddlerFingerprints({})", fingerprints.len()),
                    SyncMessage::WikiManifest { wikis, .. } => format!("WikiManifest({} wikis)", wikis.len()),
                    SyncMessage::AttachmentChanged { filename, .. } => format!("AttachmentChanged({})", filename),
                    SyncMessage::AttachmentChunk { filename, chunk_index, .. } => format!("AttachmentChunk({} #{})", filename, chunk_index),
                    SyncMessage::AttachmentDeleted { filename, .. } => format!("AttachmentDeleted({})", filename),
                    SyncMessage::RequestFingerprints { ref wiki_id } => format!("RequestFingerprints({})", wiki_id),
                    SyncMessage::WikiInfoChanged { ref wiki_id, ref content_hash, .. } => format!("WikiInfoChanged({}, hash={})", wiki_id, &content_hash[..8.min(content_hash.len())]),
                    SyncMessage::WikiInfoRequest { ref wiki_id } => format!("WikiInfoRequest({})", wiki_id),
                    SyncMessage::PluginManifest { ref plugin_name, .. } => format!("PluginManifest({})", plugin_name),
                    SyncMessage::RequestPluginFiles { ref plugin_name, .. } => format!("RequestPluginFiles({})", plugin_name),
                    SyncMessage::PluginFileChunk { ref plugin_name, ref rel_path, chunk_index, .. } => format!("PluginFileChunk({}/{} #{})", plugin_name, rel_path, chunk_index),
                    SyncMessage::PluginFilesComplete { ref plugin_name, .. } => format!("PluginFilesComplete({})", plugin_name),
                    SyncMessage::EditingStarted { ref tiddler_title, ref device_id, .. } => format!("EditingStarted({}, {})", tiddler_title, device_id),
                    SyncMessage::EditingStopped { ref tiddler_title, ref device_id, .. } => format!("EditingStopped({}, {})", tiddler_title, device_id),
                    SyncMessage::CollabUpdate { ref tiddler_title, .. } => format!("CollabUpdate({})", tiddler_title),
                    SyncMessage::CollabAwareness { ref tiddler_title, .. } => format!("CollabAwareness({})", tiddler_title),
                    SyncMessage::RoomLeave { ref room_code } => format!("RoomLeave({})", room_code),
                    _ => "Other".to_string(),
                };
                eprintln!("[LAN Sync] << {} from {}", msg_type, from_device_id);

                // Handle RoomLeave immediately (before is_sync_active check)
                // — the peer is telling us they're leaving a room.
                if let SyncMessage::RoomLeave { ref room_code } = message {
                    eprintln!(
                        "[LAN Sync] Peer {} leaving room {}",
                        from_device_id, room_code
                    );
                    if let Some(ref server) = *self.server.read().await {
                        let fully_disconnected = server.remove_room_from_peer(&from_device_id, room_code).await;
                        if fully_disconnected {
                            if let Ok(mut set) = self.connected_peer_ids.write() {
                                set.remove(&from_device_id);
                            }
                            self.remote_wikis.write().await.remove(&from_device_id);
                            if let Some(app) = GLOBAL_APP_HANDLE.get() {
                                let _ = app.emit("lan-sync-peer-disconnected", serde_json::json!({
                                    "device_id": &from_device_id,
                                }));
                                let _ = app.emit("lan-sync-peers-updated", serde_json::json!({}));
                                let available = self.get_available_remote_wikis().await;
                                let _ = app.emit("lan-sync-remote-wikis-updated", &available);
                            }
                        } else {
                            // Peer still has other rooms — just refresh status
                            if let Some(app) = GLOBAL_APP_HANDLE.get() {
                                let _ = app.emit("lan-sync-peers-updated", serde_json::json!({}));
                            }
                        }
                        #[cfg(not(target_os = "android"))]
                        self.push_peer_updates_to_ipc().await;
                    }
                    return;
                }

                // Skip all sync messages if no peers are connected.
                if !self.is_sync_active().await {
                    eprintln!("[Sync] Ignoring {} — no active sync peers", msg_type);
                    return;
                }
                {
                    let sync_wiki_id: Option<&str> = match &message {
                        SyncMessage::TiddlerFingerprints { wiki_id, .. } |
                        SyncMessage::TiddlerChanged { wiki_id, .. } |
                        SyncMessage::TiddlerDeleted { wiki_id, .. } |
                        SyncMessage::FullSyncBatch { wiki_id, .. } |
                        SyncMessage::RequestFullSync { wiki_id, .. } |
                        SyncMessage::RequestFingerprints { wiki_id, .. } |
                        SyncMessage::AttachmentChanged { wiki_id, .. } |
                        SyncMessage::AttachmentChunk { wiki_id, .. } |
                        SyncMessage::AttachmentDeleted { wiki_id, .. } |
                        SyncMessage::AttachmentManifest { wiki_id, .. } |
                        SyncMessage::RequestAttachments { wiki_id, .. } |
                        SyncMessage::WikiInfoChanged { wiki_id, .. } |
                        SyncMessage::WikiInfoRequest { wiki_id, .. } |
                        SyncMessage::PluginManifest { wiki_id, .. } |
                        SyncMessage::RequestPluginFiles { wiki_id, .. } |
                        SyncMessage::PluginFileChunk { wiki_id, .. } |
                        SyncMessage::PluginFilesComplete { wiki_id, .. } |
                        SyncMessage::EditingStarted { wiki_id, .. } |
                        SyncMessage::EditingStopped { wiki_id, .. } |
                        SyncMessage::CollabUpdate { wiki_id, .. } |
                        SyncMessage::CollabAwareness { wiki_id, .. } |
                        SyncMessage::RequestWikiFile { wiki_id, .. } |
                        SyncMessage::WikiFileChunk { wiki_id, .. } |
                        SyncMessage::WikiFileComplete { wiki_id, .. } |
                        SyncMessage::PeerSaved { wiki_id, .. } |
                        SyncMessage::ConflictNotification { wiki_id, .. } => Some(wiki_id.as_str()),
                        // Device-level messages only (WikiManifest, UserNameAnnounce, etc.)
                        _ => None,
                    };
                    if let Some(wid) = sync_wiki_id {
                        // Wiki file transfer messages (Get Wiki) bypass the
                        // sync-enabled check: the wiki doesn't exist locally yet
                        // so it can't be in the sync list.  We verify instead
                        // that an incoming transfer was pre-registered by
                        // request_wiki_from_peer (RequestWikiFile is always
                        // allowed because the sender handles it, not us).
                        let is_wiki_transfer = matches!(
                            &message,
                            SyncMessage::WikiFileChunk { .. }
                            | SyncMessage::WikiFileComplete { .. }
                            | SyncMessage::RequestWikiFile { .. }
                        );
                        let has_active_transfer = if is_wiki_transfer {
                            self.incoming_transfers.read().await.contains_key(wid)
                        } else {
                            false
                        };
                        if !is_wiki_transfer || !has_active_transfer {
                            if let Some(app) = GLOBAL_APP_HANDLE.get() {
                                let sync_wikis = crate::wiki_storage::get_sync_enabled_wikis(app);
                                if !sync_wikis.iter().any(|(id, _, _)| id == wid) {
                                    // RequestWikiFile is handled by the sender
                                    // (our wiki IS sync-enabled on our side)
                                    if !matches!(&message, SyncMessage::RequestWikiFile { .. }) {
                                        eprintln!(
                                            "[LAN Sync] Ignoring {} — wiki {} no longer sync-enabled",
                                            msg_type, wid
                                        );
                                        return;
                                    }
                                }
                                // Per-peer filter: check if this peer is in the wiki's room
                                // (skip for active wiki transfers — peer was already validated
                                // when the transfer was requested)
                                if !has_active_transfer {
                                    let allowed = self.is_peer_allowed_for_wiki(app, wid, &from_device_id).await;
                                    if !allowed {
                                        eprintln!(
                                            "[LAN Sync] Ignoring {} from {} — not in room for wiki {}",
                                            msg_type, from_device_id, wid
                                        );
                                        return;
                                    }
                                }
                            }
                        }
                    }
                }

                // Handle attachment messages, wiki transfer messages, and manifest separately
                match message {
                    SyncMessage::AttachmentChanged {
                        ref wiki_id,
                        ref filename,
                        file_size,
                        ref sha256,
                        chunk_count,
                    } => {
                        self.attachment_manager.handle_attachment_changed(
                            wiki_id,
                            filename,
                            file_size,
                            sha256,
                            chunk_count,
                        );
                    }
                    SyncMessage::AttachmentChunk {
                        ref wiki_id,
                        ref filename,
                        chunk_index,
                        ref data_base64,
                    } => {
                        match self.attachment_manager.handle_attachment_chunk(
                            wiki_id,
                            filename,
                            chunk_index,
                            data_base64,
                        ) {
                            Ok(true) => {
                                // Transfer complete — notify wiki window to reload elements
                                let payload = serde_json::json!({
                                    "type": "attachment-received",
                                    "wiki_id": wiki_id,
                                    "filename": filename,
                                });
                                Self::emit_to_wiki(wiki_id, "lan-sync-attachment-received", payload);
                            }
                            Ok(false) => {} // more chunks needed
                            Err(e) => {
                                eprintln!("[LAN Sync] Attachment chunk error: {}", e);
                            }
                        }
                    }
                    SyncMessage::AttachmentDeleted { ref wiki_id, ref filename } => {
                        let _ = self
                            .attachment_manager
                            .handle_attachment_deleted(wiki_id, filename);
                    }
                    SyncMessage::WikiManifest { ref wikis } => {
                        self.handle_wiki_manifest(&from_device_id, wikis).await;
                    }
                    SyncMessage::AttachmentManifest { ref wiki_id, ref files } => {
                        // Spawn as background task to avoid blocking event loop
                        // (hash computation can take 20+ seconds on Android/SAF)
                        let wiki_id = wiki_id.clone();
                        let files = files.clone();
                        let from_id = from_device_id.clone();
                        if let Some(mgr) = get_sync_manager() {
                            tokio::spawn(async move {
                                mgr.handle_attachment_manifest(&from_id, &wiki_id, &files).await;
                            });
                        }
                    }
                    SyncMessage::RequestAttachments { ref wiki_id, ref files } => {
                        self.handle_request_attachments(&from_device_id, wiki_id, files).await;
                    }
                    SyncMessage::WikiInfoChanged {
                        ref wiki_id,
                        ref content_json,
                        ref content_hash,
                        timestamp,
                    } => {
                        let wid = wiki_id.clone();
                        let cj = content_json.clone();
                        let ch = content_hash.clone();
                        let fid = from_device_id.clone();
                        if let Some(mgr) = get_sync_manager() {
                            // Spawn as background task — may do disk I/O
                            tokio::spawn(async move {
                                mgr.handle_wiki_info_changed(&fid, &wid, &cj, &ch, timestamp).await;
                            });
                        }
                    }
                    SyncMessage::WikiInfoRequest { ref wiki_id } => {
                        // Peer is requesting our tiddlywiki.info — send cached version
                        // Only respond if peer is in the wiki's assigned room
                        let wiki_req_allowed = if let Some(app) = GLOBAL_APP_HANDLE.get() {
                            self.is_peer_allowed_for_wiki(app, wiki_id, &from_device_id).await
                        } else {
                            false
                        };
                        if !wiki_req_allowed {
                            eprintln!("[Sync] Denied WikiInfoRequest for {} from {} — not in wiki's room", wiki_id, &from_device_id[..8.min(from_device_id.len())]);
                        } else {
                            let cached = self.wiki_info_cache.lock().ok()
                                .and_then(|c| c.get(wiki_id).cloned());
                            if let Some((content, hash, ts)) = cached {
                                let msg = SyncMessage::WikiInfoChanged {
                                    wiki_id: wiki_id.clone(),
                                    content_json: content,
                                    content_hash: hash,
                                    timestamp: ts,
                                };
                                let _ = self.send_to_peer_any(&from_device_id, &msg).await;
                            }
                        }
                    }
                    SyncMessage::PluginManifest {
                        ref wiki_id,
                        ref plugin_name,
                        ref files,
                        ref version,
                    } => {
                        self.handle_plugin_manifest(&from_device_id, wiki_id, plugin_name, files, version.as_deref()).await;
                    }
                    SyncMessage::RequestPluginFiles {
                        ref wiki_id,
                        ref plugin_name,
                        ref needed_files,
                    } => {
                        self.handle_request_plugin_files(&from_device_id, wiki_id, plugin_name, needed_files).await;
                    }
                    SyncMessage::PluginFileChunk {
                        ref wiki_id,
                        ref plugin_name,
                        ref rel_path,
                        chunk_index,
                        chunk_count,
                        ref data_base64,
                    } => {
                        self.handle_plugin_file_chunk(wiki_id, plugin_name, rel_path, chunk_index, chunk_count, data_base64);
                    }
                    SyncMessage::PluginFilesComplete {
                        ref wiki_id,
                        ref plugin_name,
                    } => {
                        self.handle_plugin_files_complete(wiki_id, plugin_name);
                    }

                    // ── Collaborative editing messages ──────────────────
                    SyncMessage::EditingStarted {
                        ref wiki_id,
                        ref tiddler_title,
                        ref device_id,
                        ref device_name,
                        ref user_name,
                    } => {
                        eprintln!("[Collab] INBOUND EditingStarted: wiki={}, tiddler={}, from_device={}, user={}", wiki_id, tiddler_title, device_id, user_name);
                        // Skip messages from our own device (self-echo guard)
                        let my_device_id = self.pairing_manager.device_id();
                        if device_id == my_device_id {
                            eprintln!("[Collab] Skipping self-echoed EditingStarted for {}", tiddler_title);
                            return;
                        }
                        // Use user_name from the message (reliable), fall back to peers map
                        let peer_user_name = if !user_name.is_empty() {
                            user_name.clone()
                        } else {
                            self.peers.read().await
                                .get(&from_device_id)
                                .and_then(|pc| pc.user_name.clone())
                                .unwrap_or_default()
                        };
                        // Track remote editor (with user_name)
                        let key = (wiki_id.clone(), tiddler_title.clone());
                        if let Ok(mut editors) = self.collab_editors.lock() {
                            let set = editors.entry(key).or_insert_with(HashSet::new);
                            // Remove any existing entry for this device_id (user_name may have changed)
                            set.retain(|(did, _, _)| did != device_id);
                            set.insert((device_id.clone(), device_name.clone(), peer_user_name.clone()));
                        }
                        // Forward to JS (with user_name)
                        self.emit_collab_to_wiki(wiki_id, serde_json::json!({
                            "type": "editing-started",
                            "wiki_id": wiki_id,
                            "tiddler_title": tiddler_title,
                            "device_id": device_id,
                            "device_name": device_name,
                            "user_name": peer_user_name,
                        }));
                    }
                    SyncMessage::EditingStopped {
                        ref wiki_id,
                        ref tiddler_title,
                        ref device_id,
                    } => {
                        eprintln!("[Collab] INBOUND EditingStopped: wiki={}, tiddler={}, from_device={}", wiki_id, tiddler_title, device_id);
                        // Skip messages from our own device (self-echo guard)
                        let my_device_id = self.pairing_manager.device_id();
                        if device_id == my_device_id {
                            eprintln!("[Collab] Skipping self-echoed EditingStopped for {}", tiddler_title);
                            return;
                        }
                        // Remove remote editor
                        let key = (wiki_id.clone(), tiddler_title.clone());
                        if let Ok(mut editors) = self.collab_editors.lock() {
                            if let Some(set) = editors.get_mut(&key) {
                                set.retain(|(did, _, _)| did != device_id);
                                if set.is_empty() {
                                    editors.remove(&key);
                                }
                            }
                        }
                        // Forward to JS
                        self.emit_collab_to_wiki(wiki_id, serde_json::json!({
                            "type": "editing-stopped",
                            "wiki_id": wiki_id,
                            "tiddler_title": tiddler_title,
                            "device_id": device_id,
                        }));
                    }
                    SyncMessage::CollabUpdate {
                        ref wiki_id,
                        ref tiddler_title,
                        ref update_base64,
                    } => {
                        // Skip messages from our own device (self-echo guard)
                        if from_device_id == self.pairing_manager.device_id() {
                            return;
                        }
                        self.emit_collab_to_wiki(wiki_id, serde_json::json!({
                            "type": "collab-update",
                            "wiki_id": wiki_id,
                            "tiddler_title": tiddler_title,
                            "update_base64": update_base64,
                        }));
                    }
                    SyncMessage::CollabAwareness {
                        ref wiki_id,
                        ref tiddler_title,
                        ref update_base64,
                    } => {
                        // Skip messages from our own device (self-echo guard)
                        if from_device_id == self.pairing_manager.device_id() {
                            return;
                        }
                        // Record sender so outbound echo-back can be suppressed
                        if let Ok(mut map) = self.awareness_last_inbound.lock() {
                            map.insert(
                                (wiki_id.to_string(), tiddler_title.to_string()),
                                (from_device_id.to_string(), std::time::Instant::now()),
                            );
                        }
                        self.emit_collab_to_wiki(wiki_id, serde_json::json!({
                            "type": "collab-awareness",
                            "wiki_id": wiki_id,
                            "tiddler_title": tiddler_title,
                            "update_base64": update_base64,
                        }));
                    }
                    SyncMessage::PeerSaved {
                        ref wiki_id,
                        ref tiddler_title,
                        ref saved_title,
                        ref device_id,
                        ref device_name,
                    } => {
                        eprintln!("[Collab] INBOUND PeerSaved: wiki={}, tiddler={}, saved_as={}, from={}", wiki_id, tiddler_title, saved_title, device_id);
                        // Skip messages from our own device (self-echo guard)
                        if device_id == self.pairing_manager.device_id() {
                            eprintln!("[Collab] Skipping self-echoed PeerSaved for {}", tiddler_title);
                            return;
                        }
                        self.emit_collab_to_wiki(wiki_id, serde_json::json!({
                            "type": "peer-saved",
                            "wiki_id": wiki_id,
                            "tiddler_title": tiddler_title,
                            "saved_title": saved_title,
                            "device_id": device_id,
                            "device_name": device_name,
                        }));
                    }

                    SyncMessage::TiddlerFingerprints {
                        ref wiki_id,
                        ref from_device_id,
                        ref fingerprints,
                        is_reply,
                    } => {
                        eprintln!(
                            "[LAN Sync] Received {} fingerprints from {} for wiki {} (is_reply={})",
                            fingerprints.len(), from_device_id, wiki_id, is_reply
                        );
                        // Dedup: skip forwarding to JS if we just forwarded this peer's
                        // fingerprints for this wiki within the last 3 seconds
                        if self.dedup_fp_forward(wiki_id, from_device_id) {
                            eprintln!(
                                "[LAN Sync] Dedup: skipping compare-fingerprints forward for wiki {} from {}",
                                wiki_id, from_device_id
                            );
                        } else {
                            // Forward peer's fingerprints to our JS so it can compare
                            // and send only tiddlers that differ.
                            // Must preserve the `deleted` flag for tombstone propagation.
                            let fp_list: Vec<serde_json::Value> = fingerprints.iter().map(|f| {
                                let mut v = serde_json::json!({"title": f.title, "modified": f.modified});
                                if f.deleted == Some(true) {
                                    v["deleted"] = serde_json::json!(true);
                                }
                                if let Some(ref ver) = f.version {
                                    v["version"] = serde_json::json!(ver);
                                }
                                v
                            }).collect();
                            Self::emit_to_wiki(
                                wiki_id,
                                "lan-sync-compare-fingerprints",
                                serde_json::json!({
                                    "type": "compare-fingerprints",
                                    "wiki_id": wiki_id,
                                    "from_device_id": from_device_id,
                                    "fingerprints": fp_list,
                                }),
                            );
                        }

                        // If this is NOT a reply, send our cached fingerprints
                        // back as a reply so the peer can compute the reverse diff.
                        // Replies never trigger further replies (prevents ping-pong).
                        // Always send, even if empty — empty means "I have nothing",
                        // so the peer sends everything (tiddlerDiffers filters dupes).
                        // Dedup: skip if we already sent to this peer recently.
                        if !is_reply {
                            if self.dedup_fp_send(wiki_id, from_device_id) {
                                eprintln!(
                                    "[LAN Sync] Dedup: skipping reciprocal fingerprints to {} for wiki {}",
                                    from_device_id, wiki_id
                                );
                            } else {
                                let fps = self.get_accurate_cached_fingerprints(wiki_id)
                                    .unwrap_or_default();
                                eprintln!(
                                    "[LAN Sync] Sending {} reciprocal fingerprints to {} for wiki {}",
                                    fps.len(), from_device_id, wiki_id
                                );
                                let reply_msg = SyncMessage::TiddlerFingerprints {
                                    wiki_id: wiki_id.to_string(),
                                    from_device_id: self.pairing_manager.device_id().to_string(),
                                    fingerprints: fps,
                                    is_reply: true,
                                };
                                if let Err(e) = self.send_to_peer_any(from_device_id, &reply_msg).await {
                                    eprintln!(
                                        "[LAN Sync] Failed to send reciprocal fingerprints: {}",
                                        e
                                    );
                                }
                            }
                        }
                    }
                    SyncMessage::RequestFingerprints { ref wiki_id } => {
                        // Peer is about to open this wiki — they want our
                        // fingerprints so their JS can compare on arrival.
                        eprintln!(
                            "[LAN Sync] Peer {} requested fingerprints for wiki {}",
                            from_device_id, wiki_id
                        );
                        Self::emit_to_wiki(
                            wiki_id,
                            "lan-sync-send-fingerprints",
                            serde_json::json!({
                                "type": "send-fingerprints",
                                "wiki_id": wiki_id,
                                "to_device_id": from_device_id,
                            }),
                        );
                    }
                    SyncMessage::UserNameAnnounce { ref user_name } => {
                        eprintln!(
                            "[LAN Sync] Peer {} announced username: {}",
                            from_device_id, user_name
                        );
                        // Store user_name on the LAN peer connection
                        if let Some(ref server) = *self.server.read().await {
                            server.set_peer_user_name(&from_device_id, user_name.clone()).await;
                        }
                        // Also store in device-level map (works for relay peers too)
                        if let Ok(mut names) = self.peer_user_names.lock() {
                            names.insert(from_device_id.clone(), user_name.clone());
                        }
                        // Emit peers-updated event to all wiki windows
                        if let Some(app) = GLOBAL_APP_HANDLE.get() {
                            let _ = app.emit("lan-sync-peers-updated", serde_json::json!({}));
                        }
                        // Push updated peer data (with new username) to wiki windows
                        #[cfg(not(target_os = "android"))]
                        self.push_peer_updates_to_ipc().await;
                    }
                    SyncMessage::RequestWikiFile { ref wiki_id, ref have_files } => {
                        self.handle_request_wiki_file(&from_device_id, wiki_id, have_files).await;
                    }
                    SyncMessage::WikiFileChunk {
                        ref wiki_id,
                        ref wiki_name,
                        is_folder,
                        ref filename,
                        chunk_index,
                        chunk_count,
                        ref data_base64,
                    } => {
                        self.handle_wiki_file_chunk(
                            wiki_id, wiki_name, is_folder, filename,
                            chunk_index, chunk_count, data_base64, &from_device_id,
                        ).await;
                    }
                    SyncMessage::WikiFileComplete {
                        ref wiki_id,
                        ref wiki_name,
                        is_folder,
                    } => {
                        self.handle_wiki_file_complete(wiki_id, wiki_name, is_folder, &from_device_id).await;
                    }
                    _ => {
                        // Extract fingerprints from FullSyncBatch tiddlers before
                        // passing ownership to handle_remote_message.  Used to
                        // update our fingerprint cache so reciprocal/cached sends
                        // reflect received tiddlers (prevents re-sending).
                        let batch_fps: Option<(String, Vec<(String, String, Option<String>)>)> = match &message {
                            SyncMessage::FullSyncBatch { wiki_id, tiddlers, .. } => {
                                let fps: Vec<(String, String, Option<String>)> = tiddlers.iter().filter_map(|t| {
                                    if let Ok(fields) = serde_json::from_str::<serde_json::Value>(&t.tiddler_json) {
                                        let modified = fields.get("modified")
                                            .and_then(|m| m.as_str())
                                            .unwrap_or("")
                                            .to_string();
                                        let version = fields.get("version")
                                            .and_then(|v| v.as_str())
                                            .map(|s| s.to_string());
                                        Some((t.title.clone(), modified, version))
                                    } else {
                                        None
                                    }
                                }).collect();
                                if !fps.is_empty() {
                                    Some((wiki_id.clone(), fps))
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        };

                        // Tiddler sync messages — returns (is_last_batch, applied_count)
                        let (is_last_batch, applied) = self.bridge.handle_remote_message(
                            &from_device_id,
                            message,
                            &self.conflict_manager,
                        );

                        // Merge received tiddler fingerprints into cache so
                        // subsequent cached sends don't trigger duplicate diffs.
                        // Also track these as "overrides" — tiddlers in the cache
                        // that may not be in the wiki file yet (if JS isn't open).
                        if applied > 0 {
                            if let Some((wiki_id, fps)) = batch_fps {
                                let titles: Vec<String> = fps.iter().map(|(t, _, _)| t.clone()).collect();
                                if let Ok(mut cache) = self.local_fingerprint_cache.lock() {
                                    let entry = cache.entry(wiki_id.clone()).or_insert_with(Vec::new);
                                    for (title, modified, version) in fps {
                                        if let Some(existing) = entry.iter_mut().find(|f| f.title == title) {
                                            existing.modified = modified;
                                            existing.version = version;
                                        } else {
                                            entry.push(protocol::TiddlerFingerprint { title, modified, deleted: None, version });
                                        }
                                    }
                                }
                                // Track as overrides so get_accurate_cached_fingerprints
                                // excludes them when telling peers what we have
                                if let Ok(mut overrides) = self.cache_merge_overrides.lock() {
                                    let set = overrides.entry(wiki_id).or_insert_with(std::collections::HashSet::new);
                                    for t in titles {
                                        set.insert(t);
                                    }
                                }
                            }
                        }
                        // After processing the last batch of a full sync with actual changes,
                        // schedule a verification pass: re-send our fingerprints to the sender
                        // after a 5s delay (time for changes to propagate to JS).
                        // This catches any tiddlers that were lost in transit.
                        if is_last_batch && applied > 0 {
                            // Extract wiki_id from pending fingerprint requests (we know which wikis
                            // are syncing with this peer)
                            let shared_wiki_ids: Vec<String> = {
                                let pending = self.pending_fingerprint_requests.read().await;
                                pending.keys()
                                    .filter(|(_, did)| did == &from_device_id)
                                    .map(|(wid, _)| wid.clone())
                                    .collect()
                            };
                            // Also check remote wikis for shared wiki IDs
                            let remote_wiki_ids: Vec<String> = {
                                let remote = self.remote_wikis.read().await;
                                if let Some(wikis) = remote.get(&from_device_id) {
                                    wikis.iter().map(|w| w.wiki_id.clone()).collect()
                                } else {
                                    vec![]
                                }
                            };
                            let device_id = from_device_id.clone();
                            let all_wiki_ids: Vec<String> = shared_wiki_ids.into_iter()
                                .chain(remote_wiki_ids.into_iter())
                                .collect::<std::collections::HashSet<_>>()
                                .into_iter()
                                .collect();
                            if !all_wiki_ids.is_empty() {
                                eprintln!(
                                    "[LAN Sync] Scheduling verification pass in 5s for {} wiki(s) with peer {}",
                                    all_wiki_ids.len(), device_id
                                );
                                tokio::spawn(async move {
                                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                                    for wid in all_wiki_ids {
                                        Self::emit_to_wiki(
                                            &wid,
                                            "lan-sync-send-fingerprints",
                                            serde_json::json!({
                                                "type": "send-fingerprints",
                                                "wiki_id": wid,
                                                "to_device_id": device_id,
                                            }),
                                        );
                                    }
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    /// Send our WikiManifest to a specific peer (filtered to wikis in shared rooms)
    async fn send_wiki_manifest_to_peer(&self, device_id: &str) {
        if let Some(app) = GLOBAL_APP_HANDLE.get() {
            // Find all rooms this peer shares with us (LAN auth_room_codes + relay room)
            let room_codes: Vec<String> = {
                let lan_rooms = self.peers.read().await
                    .get(device_id)
                    .map(|pc| pc.auth_room_codes.clone())
                    .unwrap_or_default();
                if !lan_rooms.is_empty() {
                    lan_rooms
                } else if let Some(relay) = &self.relay_manager {
                    relay.find_all_device_rooms(device_id).await
                } else {
                    vec![]
                }
            };

            // Union wikis from all shared rooms, tracking which room each wiki belongs to
            let mut seen_wiki_ids = std::collections::HashSet::new();
            let mut all_sync_wikis = Vec::new();
            for rc in &room_codes {
                for wiki in crate::wiki_storage::get_sync_wikis_for_room(app, rc) {
                    if seen_wiki_ids.insert(wiki.0.clone()) {
                        all_sync_wikis.push((wiki, rc.clone()));
                    }
                }
            }
            if room_codes.is_empty() {
                eprintln!("[Manifest] Device {} not found in any room", &device_id[..8.min(device_id.len())]);
            }

            let wikis: Vec<protocol::WikiInfo> = all_sync_wikis
                .into_iter()
                .map(|((sync_id, name, is_folder), rc)| protocol::WikiInfo {
                    wiki_id: sync_id,
                    wiki_name: name,
                    is_folder,
                    room_code: Some(rc),
                })
                .collect();
            eprintln!("[Manifest] Sending {} wikis to {} (rooms {:?})", wikis.len(), &device_id[..8.min(device_id.len())], room_codes);

            let msg = SyncMessage::WikiManifest { wikis };
            if let Err(e) = self.send_to_peer_any(device_id, &msg).await {
                eprintln!("[Sync] Failed to send WikiManifest to {}: {}", device_id, e);
            }
        }
    }

    /// Broadcast our WikiManifest to all connected peers (per-room filtered).
    /// Each peer only sees wikis assigned to the room they share with us.
    /// Called when wiki sync is toggled or wiki list changes.
    /// Also re-registers attachment paths since the wiki list may have changed.
    pub async fn broadcast_wiki_manifest(&self) {
        // Re-register attachment paths so newly linked/toggled wikis get
        // their paths registered for incoming attachment resolution.
        self.register_wiki_attachment_paths();

        if let Some(app) = GLOBAL_APP_HANDLE.get() {
            if let Some(ref server) = *self.server.read().await {
                // Send per-peer manifests to LAN peers (filtered by auth rooms)
                let lan_peers = server.lan_connected_peers().await;
                for (peer_id, _) in &lan_peers {
                    // Get all rooms this LAN peer shares with us
                    let room_codes = self.peers.read().await
                        .get(peer_id.as_str())
                        .map(|pc| pc.auth_room_codes.clone())
                        .unwrap_or_default();
                    // Union wikis from all shared rooms, tracking which room each wiki belongs to
                    let mut seen_wiki_ids = std::collections::HashSet::new();
                    let mut all_sync_wikis = Vec::new();
                    for rc in &room_codes {
                        for wiki in crate::wiki_storage::get_sync_wikis_for_room(app, rc) {
                            if seen_wiki_ids.insert(wiki.0.clone()) {
                                all_sync_wikis.push((wiki, rc.clone()));
                            }
                        }
                    }
                    let wikis: Vec<protocol::WikiInfo> = all_sync_wikis
                        .into_iter()
                        .map(|((sync_id, name, is_folder), rc)| protocol::WikiInfo {
                            wiki_id: sync_id,
                            wiki_name: name,
                            is_folder,
                            room_code: Some(rc),
                        })
                        .collect();
                    let msg = SyncMessage::WikiManifest { wikis };
                    if let Err(e) = server.send_to_peer(&peer_id, &msg).await {
                        eprintln!("[LAN Sync] Failed to send WikiManifest to {}: {}", peer_id, e);
                    }
                }
                if !lan_peers.is_empty() {
                    eprintln!("[LAN Sync] Sent per-peer WikiManifest to {} LAN peers", lan_peers.len());
                }
            }

            // Send per-room manifests through relay (excluding LAN-reachable peers)
            if let Some(relay) = &self.relay_manager {
                let lan_peer_ids: std::collections::HashSet<String> =
                    if let Some(ref server) = *self.server.read().await {
                        server.lan_connected_peers().await.into_iter().map(|(id, _)| id).collect()
                    } else {
                        std::collections::HashSet::new()
                    };
                let connected_rooms = relay.get_connected_room_codes().await;
                for room_code in &connected_rooms {
                    let sync_wikis = crate::wiki_storage::get_sync_wikis_for_room(app, room_code);
                    let wikis: Vec<protocol::WikiInfo> = sync_wikis
                        .into_iter()
                        .map(|(sync_id, name, is_folder)| protocol::WikiInfo {
                            wiki_id: sync_id,
                            wiki_name: name,
                            is_folder,
                            room_code: Some(room_code.clone()),
                        })
                        .collect();
                    let msg = SyncMessage::WikiManifest { wikis };
                    if let Err(e) = relay.send_to_room_excluding(room_code, &msg, &lan_peer_ids).await {
                        eprintln!("[Relay] Failed to send WikiManifest to room {}: {}", room_code, e);
                    }
                }
                if !connected_rooms.is_empty() {
                    eprintln!("[Relay] Sent per-room WikiManifest to {} rooms", connected_rooms.len());
                }
            }
        }
    }

    /// Handle an incoming WikiManifest from a peer.
    /// Merges with existing wikis for this peer (relay sends per-room manifests
    /// that arrive separately, so we must union rather than replace).
    async fn handle_wiki_manifest(&self, from_device_id: &str, wikis: &[protocol::WikiInfo]) {
        eprintln!(
            "[LAN Sync] Received WikiManifest from {} with {} wikis",
            from_device_id,
            wikis.len()
        );
        {
            let mut remote = self.remote_wikis.write().await;
            let entry = remote.entry(from_device_id.to_string()).or_insert_with(Vec::new);
            for wiki in wikis {
                if let Some(existing) = entry.iter_mut().find(|w| w.wiki_id == wiki.wiki_id) {
                    // Update room_code if provided (peer may have reassigned wiki)
                    if wiki.room_code.is_some() {
                        existing.room_code = wiki.room_code.clone();
                    }
                } else {
                    entry.push(wiki.clone());
                }
            }
        }

        // Emit updated available wikis to the UI
        if let Some(app) = GLOBAL_APP_HANDLE.get() {
            let available = self.get_available_remote_wikis().await;
            let _ = app.emit("lan-sync-remote-wikis-updated", &available);

            // For each shared wiki (exists both locally and remotely AND peer is allowed),
            // trigger a full sync to catch up on any missed changes
            let local_wikis = crate::wiki_storage::get_sync_enabled_wikis(app);
            eprintln!("[LAN Sync] Local sync-enabled wikis: {:?}", local_wikis.iter().map(|(id, name, _)| format!("{}={}", name, id)).collect::<Vec<_>>());
            for remote_wiki in wikis {
                eprintln!("[LAN Sync] Remote wiki: {}={}", remote_wiki.wiki_name, remote_wiki.wiki_id);
                let local_entry = local_wikis.iter().find(|(sync_id, _, _)| sync_id == &remote_wiki.wiki_id);
                if let Some((sync_id, _, is_folder)) = local_entry {
                    // Check if sync is allowed: peer must be in the wiki's room
                    let allowed = self.is_peer_allowed_for_wiki(app, sync_id, from_device_id).await;
                    if !allowed {
                        eprintln!(
                            "[LAN Sync] Skipping shared wiki '{}' — peer {} not in room",
                            remote_wiki.wiki_name, from_device_id
                        );
                        continue;
                    }
                    eprintln!(
                        "[LAN Sync] Shared wiki '{}' ({}) — requesting fingerprint sync from JS",
                        remote_wiki.wiki_name, remote_wiki.wiki_id
                    );
                    // Track this fingerprint request for timeout detection
                    self.pending_fingerprint_requests.write().await.insert(
                        (remote_wiki.wiki_id.clone(), from_device_id.to_string()),
                        std::time::Instant::now(),
                    );
                    // Ask JS to send tiddler fingerprints for diff-based sync
                    Self::emit_to_wiki(
                        &remote_wiki.wiki_id,
                        "lan-sync-send-fingerprints",
                        serde_json::json!({
                            "type": "send-fingerprints",
                            "wiki_id": remote_wiki.wiki_id,
                            "to_device_id": from_device_id,
                        }),
                    );

                    // Also send cached fingerprints directly from Rust so the
                    // peer can start computing diffs immediately — don't wait
                    // for JS to collect and send fingerprints (saves 1-3s).
                    // Dedup: skip if we already sent to this peer recently.
                    if self.dedup_fp_send(&remote_wiki.wiki_id, from_device_id) {
                        eprintln!(
                            "[LAN Sync] Dedup: skipping cached fingerprints for wiki {} to peer {} (on manifest)",
                            remote_wiki.wiki_id, from_device_id
                        );
                    } else {
                    let fps = self.get_accurate_cached_fingerprints(&remote_wiki.wiki_id)
                        .unwrap_or_default();
                    eprintln!(
                        "[Sync] Sending {} cached fingerprints for wiki {} to peer {} (on manifest)",
                        fps.len(), remote_wiki.wiki_id, from_device_id
                    );
                    let _ = self.send_to_peer_any(
                        from_device_id,
                        &SyncMessage::TiddlerFingerprints {
                            wiki_id: remote_wiki.wiki_id.clone(),
                            from_device_id: self.pairing_manager.device_id().to_string(),
                            fingerprints: fps,
                            is_reply: false,
                        },
                    ).await;
                    } // end dedup else

                    // For single-file wikis, also send our attachment manifest
                    // so the peer can detect missing/outdated files from interrupted syncs.
                    // Spawned as background task to avoid blocking the event loop.
                    if !is_folder {
                        if let Some(wiki_path) = crate::wiki_storage::get_wiki_path_by_sync_id(app, sync_id) {
                            let from_id = from_device_id.to_string();
                            let sid = sync_id.clone();
                            if let Some(mgr) = get_sync_manager() {
                                tokio::spawn(async move {
                                    mgr.send_attachment_manifest(&from_id, &sid, &wiki_path).await;
                                });
                            }
                        }
                    }
                }
            }
        }
    }

    /// Get available remote wikis (not already present locally)
    async fn get_available_remote_wikis(&self) -> Vec<RemoteWikiInfo> {
        let remote = self.remote_wikis.read().await;
        let mut result = Vec::new();

        // Get connected peers info for device names (LAN + relay)
        let peers_info: HashMap<String, String> = self.connected_peers_all().await
            .into_iter().collect();

        let app = GLOBAL_APP_HANDLE.get();

        for (device_id, wikis) in remote.iter() {
            let device_name = peers_info
                .get(device_id)
                .cloned()
                .unwrap_or_else(|| device_id.clone());

            for wiki in wikis {
                // Only include wikis that don't already exist locally
                let has_locally = app
                    .map(|a| crate::wiki_storage::has_wiki_with_sync_id(a, &wiki.wiki_id))
                    .unwrap_or(false);

                if !has_locally {
                    result.push(RemoteWikiInfo {
                        wiki_id: wiki.wiki_id.clone(),
                        wiki_name: wiki.wiki_name.clone(),
                        is_folder: wiki.is_folder,
                        from_device_id: device_id.clone(),
                        from_device_name: device_name.clone(),
                        room_code: wiki.room_code.clone(),
                    });
                }
            }
        }

        result
    }

    // ── tiddlywiki.info sync ───────────────────────────────────────────

    /// Store tiddlywiki.info content for a folder wiki and broadcast to peers.
    /// Called when a folder wiki is opened (from lib.rs or WikiActivity).
    pub async fn set_wiki_info(&self, wiki_id: &str, content_json: &str) {
        let content_hash = wiki_info::hash_content(content_json);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        // Store in cache
        if let Ok(mut cache) = self.wiki_info_cache.lock() {
            cache.insert(
                wiki_id.to_string(),
                (content_json.to_string(), content_hash.clone(), timestamp),
            );
        }

        // Broadcast to all connected peers
        self.broadcast_wiki_info(wiki_id, content_json, &content_hash, timestamp)
            .await;
    }

    /// Broadcast WikiInfoChanged to peers allowed for this wiki
    async fn broadcast_wiki_info(
        &self,
        wiki_id: &str,
        content_json: &str,
        content_hash: &str,
        timestamp: u64,
    ) {
        let peers = self.get_all_peers_for_wiki(wiki_id).await;
        if peers.is_empty() {
            return;
        }
        let msg = SyncMessage::WikiInfoChanged {
            wiki_id: wiki_id.to_string(),
            content_json: content_json.to_string(),
            content_hash: content_hash.to_string(),
            timestamp,
        };
        // Send via LAN peers
        if let Some(ref server) = *self.server.read().await {
            server.send_to_peers(&peers, &msg).await;
            eprintln!(
                "[LAN Sync] Broadcast WikiInfoChanged for wiki {} (hash={}) to {} LAN peers",
                wiki_id,
                &content_hash[..content_hash.len().min(8)],
                peers.len()
            );
        }
        // Also broadcast via relay room if wiki is assigned to one (excluding LAN peers)
        if let Some(relay) = &self.relay_manager {
            if let Some(room_code) = crate::wiki_storage::get_wiki_relay_room_by_sync_id(
                GLOBAL_APP_HANDLE.get().unwrap(), wiki_id,
            ) {
                let lan_peer_ids: std::collections::HashSet<String> =
                    if let Some(ref server) = *self.server.read().await {
                        server.lan_connected_peers().await.into_iter().map(|(id, _)| id).collect()
                    } else {
                        std::collections::HashSet::new()
                    };
                let _ = relay.send_to_room_excluding(&room_code, &msg, &lan_peer_ids).await;
            }
        }
    }

    /// Send cached WikiInfoChanged to a specific peer (on peer connect)
    async fn send_wiki_info_to_peer(&self, device_id: &str) {
        let entries: Vec<(String, String, String, u64)> = {
            let cache = match self.wiki_info_cache.lock() {
                Ok(c) => c,
                Err(_) => return,
            };
            cache
                .iter()
                .map(|(wid, (content, hash, ts))| {
                    (wid.clone(), content.clone(), hash.clone(), *ts)
                })
                .collect()
        };

        if entries.is_empty() {
            return;
        }

        for (wiki_id, content_json, content_hash, timestamp) in entries {
            // Only send wiki info if peer is in the wiki's assigned room
            let info_allowed = if let Some(app) = GLOBAL_APP_HANDLE.get() {
                self.is_peer_allowed_for_wiki(app, &wiki_id, device_id).await
            } else {
                false
            };
            if !info_allowed {
                continue;
            }
            let msg = SyncMessage::WikiInfoChanged {
                wiki_id: wiki_id.clone(),
                content_json,
                content_hash: content_hash.clone(),
                timestamp,
            };
            if let Err(e) = self.send_to_peer_any(device_id, &msg).await {
                eprintln!(
                    "[Sync] Failed to send WikiInfoChanged for {} to {}: {}",
                    wiki_id, device_id, e
                );
            }
        }
    }

    /// Handle an incoming WikiInfoChanged from a peer.
    /// Merges the remote tiddlywiki.info with our local copy (union of arrays),
    /// writes the merged version, and emits a reload warning to JS.
    async fn handle_wiki_info_changed(
        &self,
        from_device_id: &str,
        wiki_id: &str,
        content_json: &str,
        content_hash: &str,
        _timestamp: u64,
    ) {
        // Check if we have this wiki locally
        let app = match crate::GLOBAL_APP_HANDLE.get() {
            Some(a) => a,
            None => return,
        };
        let wiki_path = match crate::wiki_storage::get_wiki_path_by_sync_id(app, wiki_id) {
            Some(p) => p,
            None => {
                eprintln!(
                    "[LAN Sync] WikiInfoChanged for unknown wiki {} — ignoring",
                    wiki_id
                );
                return;
            }
        };

        // Check if hash differs from our cached version
        let our_hash = self.wiki_info_cache.lock().ok()
            .and_then(|c| c.get(wiki_id).map(|(_, h, _)| h.clone()));
        if our_hash.as_deref() == Some(content_hash) {
            eprintln!(
                "[LAN Sync] WikiInfoChanged for {} — hash matches, no merge needed",
                wiki_id
            );
            return;
        }

        // Parse remote tiddlywiki.info
        let remote_info = match wiki_info::WikiInfo::parse(content_json) {
            Ok(info) => info,
            Err(e) => {
                eprintln!(
                    "[LAN Sync] Failed to parse remote tiddlywiki.info for {}: {}",
                    wiki_id, e
                );
                return;
            }
        };

        // Read our local tiddlywiki.info
        let wiki_path_buf = std::path::PathBuf::from(&wiki_path);
        let info_path = wiki_path_buf.join("tiddlywiki.info");
        let local_content = match std::fs::read_to_string(&info_path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "[LAN Sync] Failed to read local tiddlywiki.info for {}: {}",
                    wiki_id, e
                );
                return;
            }
        };

        let local_info = match wiki_info::WikiInfo::parse(&local_content) {
            Ok(info) => info,
            Err(e) => {
                eprintln!(
                    "[LAN Sync] Failed to parse local tiddlywiki.info for {}: {}",
                    wiki_id, e
                );
                return;
            }
        };

        // Merge (union of arrays)
        let merged = wiki_info::merge_wiki_info(&local_info, &remote_info);
        let merged_json = match merged.to_json() {
            Ok(j) => j,
            Err(e) => {
                eprintln!("[LAN Sync] Failed to serialize merged tiddlywiki.info: {}", e);
                return;
            }
        };

        // Check if merge actually changed anything (compare arrays directly to avoid
        // false positives from JSON re-serialization formatting differences)
        let merged_hash = wiki_info::hash_content(&merged_json);
        if merged.plugins == local_info.plugins
            && merged.themes == local_info.themes
            && merged.languages == local_info.languages
        {
            eprintln!(
                "[LAN Sync] WikiInfoChanged for {} — merge produced no changes",
                wiki_id
            );
            // Still update our cache with the latest hash
            if let Ok(mut cache) = self.wiki_info_cache.lock() {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                cache.insert(wiki_id.to_string(), (merged_json, merged_hash, ts));
            }
            return;
        }

        eprintln!(
            "[LAN Sync] Merging tiddlywiki.info for wiki {} from peer {}",
            wiki_id, from_device_id
        );

        // Determine which items are new (for plugin file transfer)
        let new_plugins = wiki_info::new_items(&local_info.plugins, &remote_info.plugins);
        let new_themes = wiki_info::new_items(&local_info.themes, &remote_info.themes);
        let new_languages = wiki_info::new_items(&local_info.languages, &remote_info.languages);

        // Determine which items exist on both sides (for version update checking)
        let shared_plugins = wiki_info::shared_items(&local_info.plugins, &remote_info.plugins);
        let shared_themes = wiki_info::shared_items(&local_info.themes, &remote_info.themes);
        let shared_languages = wiki_info::shared_items(&local_info.languages, &remote_info.languages);

        // Check availability of new items and request transfers for non-bundled ones
        let resources_dir = self.get_resources_dir();
        let synced_dir = self.get_synced_plugins_dir();

        let mut items_needing_transfer: Vec<(String, String)> = Vec::new(); // (category, name)

        for plugin in &new_plugins {
            if !wiki_info::is_bundled_plugin(&resources_dir, plugin)
                && !wiki_info::is_synced_item(&synced_dir, "plugins", plugin)
                && !wiki_path_buf.join("plugins").join(plugin).is_dir()
            {
                eprintln!(
                    "[LAN Sync] New plugin '{}' not found locally — will request from peer",
                    plugin
                );
                items_needing_transfer.push(("plugins".to_string(), plugin.clone()));
            } else {
                eprintln!("[LAN Sync] New plugin '{}' already available locally", plugin);
            }
        }

        for theme in &new_themes {
            if !wiki_info::is_bundled_theme(&resources_dir, theme)
                && !wiki_info::is_synced_item(&synced_dir, "themes", theme)
                && !wiki_path_buf.join("themes").join(theme).is_dir()
            {
                items_needing_transfer.push(("themes".to_string(), theme.clone()));
            }
        }

        for language in &new_languages {
            if !wiki_info::is_bundled_language(&resources_dir, language)
                && !wiki_info::is_synced_item(&synced_dir, "languages", language)
                && !wiki_path_buf.join("languages").join(language).is_dir()
            {
                items_needing_transfer.push(("languages".to_string(), language.clone()));
            }
        }

        // Collect shared items that need version comparison (request manifests)
        let mut items_needing_version_check: Vec<(String, String)> = Vec::new();
        for plugin in &shared_plugins {
            items_needing_version_check.push(("plugins".to_string(), plugin.clone()));
        }
        for theme in &shared_themes {
            items_needing_version_check.push(("themes".to_string(), theme.clone()));
        }
        for language in &shared_languages {
            items_needing_version_check.push(("languages".to_string(), language.clone()));
        }

        // Write merged tiddlywiki.info
        if let Err(e) = std::fs::write(&info_path, &merged_json) {
            eprintln!(
                "[LAN Sync] Failed to write merged tiddlywiki.info for {}: {}",
                wiki_id, e
            );
            return;
        }

        // Update our cache
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        if let Ok(mut cache) = self.wiki_info_cache.lock() {
            cache.insert(
                wiki_id.to_string(),
                (merged_json.clone(), merged_hash.clone(), ts),
            );
        }

        eprintln!(
            "[LAN Sync] Wrote merged tiddlywiki.info for wiki {} (new plugins: {}, themes: {}, languages: {})",
            wiki_id, new_plugins.len(), new_themes.len(), new_languages.len()
        );

        // Emit reload warning to JS
        Self::emit_to_wiki(
            wiki_id,
            "lan-sync-wiki-info-changed",
            serde_json::json!({
                "type": "wiki-info-changed",
                "wiki_id": wiki_id,
            }),
        );

        // Request plugin files for items that need transfer (new plugins)
        if !items_needing_transfer.is_empty() {
            for (category, name) in &items_needing_transfer {
                let plugin_key = format!("{}/{}", category, name);
                eprintln!(
                    "[Sync] Requesting plugin manifest for new '{}' from peer {}",
                    plugin_key, from_device_id
                );
                let _ = self.send_to_peer_any(
                    from_device_id,
                    &SyncMessage::RequestPluginFiles {
                        wiki_id: wiki_id.to_string(),
                        plugin_name: plugin_key,
                        needed_files: vec![],
                    },
                ).await;
            }
        }

        // Request manifests for shared items to check for version updates
        for (category, name) in &items_needing_version_check {
            let plugin_key = format!("{}/{}", category, name);
            eprintln!(
                "[Sync] Requesting plugin manifest for shared '{}' from peer {} (version check)",
                plugin_key, from_device_id
            );
            let _ = self.send_to_peer_any(
                from_device_id,
                &SyncMessage::RequestPluginFiles {
                    wiki_id: wiki_id.to_string(),
                    plugin_name: plugin_key,
                    needed_files: vec![],
                },
            ).await;
        }

        // Broadcast our updated wiki info to other peers
        self.broadcast_wiki_info(wiki_id, &merged_json, &merged_hash, ts)
            .await;
    }

    /// Handle RequestPluginFiles from a peer — send manifest or specific files
    async fn handle_request_plugin_files(
        &self,
        from_device_id: &str,
        wiki_id: &str,
        plugin_name: &str,
        needed_files: &[String],
    ) {
        let app = match crate::GLOBAL_APP_HANDLE.get() {
            Some(a) => a,
            None => return,
        };
        let wiki_path = match crate::wiki_storage::get_wiki_path_by_sync_id(app, wiki_id) {
            Some(p) => p,
            None => return,
        };

        // Parse plugin_name: "plugins/tiddlywiki/markdown" → category="plugins", name="tiddlywiki/markdown"
        let (category, name) = match plugin_name.split_once('/') {
            Some((cat, rest)) if cat == "plugins" || cat == "themes" || cat == "languages" => {
                (cat, rest)
            }
            _ => {
                eprintln!(
                    "[LAN Sync] Invalid plugin name format: {}",
                    plugin_name
                );
                return;
            }
        };

        let resources_dir = self.get_resources_dir();
        let synced_dir = self.get_synced_plugins_dir();
        let extra_dirs = self.get_extra_plugin_dirs();
        let wiki_folder = std::path::PathBuf::from(&wiki_path);

        let item_dir = match wiki_info::find_item_dir(
            &wiki_folder,
            &resources_dir,
            &synced_dir,
            &extra_dirs,
            category,
            name,
        ) {
            Some(d) => d,
            None => {
                eprintln!(
                    "[LAN Sync] Plugin '{}' not found on this device — can't send",
                    plugin_name
                );
                return;
            }
        };

        if needed_files.is_empty() {
            // Send manifest first
            let manifest = wiki_info::item_dir_manifest(&item_dir);
            let version = wiki_info::read_plugin_version(&item_dir);
            eprintln!(
                "[LAN Sync] Sending PluginManifest for '{}' v{} ({} files) to peer {}",
                plugin_name,
                version.as_deref().unwrap_or("?"),
                manifest.len(),
                from_device_id
            );
            let msg = SyncMessage::PluginManifest {
                wiki_id: wiki_id.to_string(),
                plugin_name: plugin_name.to_string(),
                files: manifest,
                version,
            };
            let _ = self.send_to_peer_any(from_device_id, &msg).await;
        } else {
            // Send specific files as chunks
            self.send_plugin_files(from_device_id, wiki_id, plugin_name, &item_dir, needed_files)
                .await;
        }
    }

    /// Send plugin files in chunks to a peer
    async fn send_plugin_files(
        &self,
        to_device_id: &str,
        wiki_id: &str,
        plugin_name: &str,
        item_dir: &std::path::Path,
        files: &[String],
    ) {
        use base64::Engine;

        for rel_path in files {
            // Validate path stays within plugin directory (prevents path traversal)
            let file_path = match validate_contained_path(item_dir, rel_path) {
                Some(p) => p,
                None => {
                    eprintln!(
                        "[Sync] Security: Rejected plugin file request with unsafe path: {}",
                        rel_path
                    );
                    continue;
                }
            };
            let data = match std::fs::read(&file_path) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!(
                        "[Sync] Failed to read plugin file {}: {}",
                        file_path.display(),
                        e
                    );
                    continue;
                }
            };

            let chunk_size = protocol::ATTACHMENT_CHUNK_SIZE;
            let chunk_count = ((data.len() + chunk_size - 1) / chunk_size) as u32;

            for i in 0..chunk_count {
                let start = i as usize * chunk_size;
                let end = std::cmp::min(start + chunk_size, data.len());
                let chunk_data = &data[start..end];
                let data_base64 =
                    base64::engine::general_purpose::STANDARD.encode(chunk_data);

                let msg = SyncMessage::PluginFileChunk {
                    wiki_id: wiki_id.to_string(),
                    plugin_name: plugin_name.to_string(),
                    rel_path: rel_path.clone(),
                    chunk_index: i,
                    chunk_count,
                    data_base64,
                };
                if let Err(e) = self.send_to_peer_any(to_device_id, &msg).await {
                    eprintln!(
                        "[Sync] Failed to send plugin file chunk: {}",
                        e
                    );
                    return;
                }
                // Pace chunk sends to avoid saturating relay
                tokio::time::sleep(std::time::Duration::from_millis(
                    protocol::ATTACHMENT_CHUNK_DELAY_MS,
                ))
                .await;
            }
        }

        // Send completion signal
        let msg = SyncMessage::PluginFilesComplete {
            wiki_id: wiki_id.to_string(),
            plugin_name: plugin_name.to_string(),
        };
        let _ = self.send_to_peer_any(to_device_id, &msg).await;
    }

    /// Handle PluginManifest from a peer — compare with local and request missing/updated files
    async fn handle_plugin_manifest(
        &self,
        from_device_id: &str,
        wiki_id: &str,
        plugin_name: &str,
        remote_files: &[protocol::AttachmentFileInfo],
        remote_version: Option<&str>,
    ) {
        let app = match crate::GLOBAL_APP_HANDLE.get() {
            Some(a) => a,
            None => return,
        };

        // Parse category/name from plugin_name
        let (category, name) = match plugin_name.split_once('/') {
            Some((cat, rest)) if cat == "plugins" || cat == "themes" || cat == "languages" => {
                (cat, rest)
            }
            _ => return,
        };

        let resources_dir = self.get_resources_dir();
        let synced_dir = self.get_synced_plugins_dir();
        let extra_dirs = self.get_extra_plugin_dirs();
        let wiki_path = crate::wiki_storage::get_wiki_path_by_sync_id(app, wiki_id)
            .unwrap_or_default();
        let wiki_folder = std::path::PathBuf::from(&wiki_path);

        // Find the local copy of this plugin (wiki-local, synced, extra paths, or bundled)
        let local_dir = wiki_info::find_item_dir(
            &wiki_folder,
            &resources_dir,
            &synced_dir,
            &extra_dirs,
            category,
            name,
        );

        let local_manifest = match &local_dir {
            Some(dir) => wiki_info::item_dir_manifest(dir),
            None => vec![],
        };

        // If plugin exists locally, compare versions to determine direction
        if !local_manifest.is_empty() {
            if let (Some(remote_ver), Some(ref dir)) = (remote_version, &local_dir) {
                let local_ver = wiki_info::read_plugin_version(dir);
                if let Some(ref lv) = local_ver {
                    if !wiki_info::version_is_newer(remote_ver, lv) {
                        eprintln!(
                            "[LAN Sync] Plugin '{}' local v{} >= remote v{} — skipping",
                            plugin_name, lv, remote_ver
                        );
                        return;
                    }
                    eprintln!(
                        "[LAN Sync] Plugin '{}' remote v{} > local v{} — updating",
                        plugin_name, remote_ver, lv
                    );
                }
            }
        }

        // Build hash map of local files
        let local_hashes: std::collections::HashMap<&str, &str> = local_manifest
            .iter()
            .map(|f| (f.rel_path.as_str(), f.sha256_hex.as_str()))
            .collect();

        // Find files that are missing or have different hashes
        let needed: Vec<String> = remote_files
            .iter()
            .filter(|rf| {
                match local_hashes.get(rf.rel_path.as_str()) {
                    Some(local_hash) => *local_hash != rf.sha256_hex, // different hash
                    None => true,                                      // missing
                }
            })
            .map(|rf| rf.rel_path.clone())
            .collect();

        if needed.is_empty() {
            eprintln!(
                "[LAN Sync] Plugin '{}' already up to date ({} files match)",
                plugin_name,
                remote_files.len()
            );
            return;
        }

        eprintln!(
            "[LAN Sync] Requesting {} files for plugin '{}' from peer {}",
            needed.len(),
            plugin_name,
            from_device_id
        );

        let msg = SyncMessage::RequestPluginFiles {
            wiki_id: wiki_id.to_string(),
            plugin_name: plugin_name.to_string(),
            needed_files: needed,
        };
        let _ = self.send_to_peer_any(from_device_id, &msg).await;
    }

    /// Handle an incoming PluginFileChunk — accumulate data
    fn handle_plugin_file_chunk(
        &self,
        wiki_id: &str,
        plugin_name: &str,
        rel_path: &str,
        chunk_index: u32,
        chunk_count: u32,
        data_base64: &str,
    ) {
        // Early reject path traversal in rel_path before even accumulating data
        let normalized = rel_path.replace('\\', "/");
        let clean = normalized.strip_prefix("./").unwrap_or(&normalized);
        for component in clean.split('/') {
            if component == ".." || component.is_empty() {
                eprintln!(
                    "[LAN Sync] Security: Rejected plugin file chunk with unsafe path: {}",
                    rel_path
                );
                return;
            }
        }

        use base64::Engine;
        let data = match base64::engine::general_purpose::STANDARD.decode(data_base64) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[LAN Sync] Failed to decode plugin file chunk: {}", e);
                return;
            }
        };

        let key = (wiki_id.to_string(), plugin_name.to_string());
        let mut transfers = match self.incoming_plugin_transfers.lock() {
            Ok(t) => t,
            Err(_) => return,
        };
        let file_map = transfers.entry(key).or_insert_with(HashMap::new);

        // Append data to the file
        let entry = file_map.entry(rel_path.to_string()).or_insert_with(Vec::new);
        entry.extend_from_slice(&data);

        if chunk_index + 1 < chunk_count {
            // More chunks coming for this file
            return;
        }

        eprintln!(
            "[LAN Sync] Received complete file: {}/{} ({} bytes)",
            plugin_name,
            rel_path,
            entry.len()
        );
    }

    /// Handle PluginFilesComplete — write all accumulated files to plugins dir
    fn handle_plugin_files_complete(&self, wiki_id: &str, plugin_name: &str) {
        let key = (wiki_id.to_string(), plugin_name.to_string());
        let file_map = match self.incoming_plugin_transfers.lock() {
            Ok(mut t) => t.remove(&key).unwrap_or_default(),
            Err(_) => return,
        };

        if file_map.is_empty() {
            eprintln!(
                "[LAN Sync] PluginFilesComplete for '{}' but no files accumulated",
                plugin_name
            );
            return;
        }

        // Parse category/name
        let (category, name) = match plugin_name.split_once('/') {
            Some((cat, rest)) if cat == "plugins" || cat == "themes" || cat == "languages" => {
                (cat, rest)
            }
            _ => {
                eprintln!("[LAN Sync] Invalid plugin name: {}", plugin_name);
                return;
            }
        };

        let synced_dir = self.get_synced_plugins_dir();
        let target_dir = synced_dir.join(category).join(name);
        if let Err(e) = std::fs::create_dir_all(&target_dir) {
            eprintln!(
                "[LAN Sync] Failed to create synced plugin dir {:?}: {}",
                target_dir, e
            );
            return;
        }

        let mut written = 0;
        for (rel_path, data) in &file_map {
            // Validate path stays within target plugin directory (prevents path traversal)
            let file_path = match validate_contained_path(&target_dir, rel_path) {
                Some(p) => p,
                None => {
                    eprintln!(
                        "[LAN Sync] Security: Rejected plugin file write with unsafe path: {}",
                        rel_path
                    );
                    continue;
                }
            };
            // Create parent directories if needed
            if let Some(parent) = file_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::write(&file_path, data) {
                eprintln!(
                    "[LAN Sync] Failed to write synced plugin file {:?}: {}",
                    file_path, e
                );
            } else {
                written += 1;
            }
        }

        eprintln!(
            "[LAN Sync] Wrote {}/{} files for plugin '{}' to {:?}",
            written,
            file_map.len(),
            plugin_name,
            target_dir
        );

        // Emit reload warning
        Self::emit_to_wiki(
            wiki_id,
            "lan-sync-wiki-info-changed",
            serde_json::json!({
                "type": "wiki-info-changed",
                "wiki_id": wiki_id,
            }),
        );
    }

    /// Get the bundled TiddlyWiki resources directory
    fn get_resources_dir(&self) -> std::path::PathBuf {
        // Try to resolve from app handle
        if let Some(app) = crate::GLOBAL_APP_HANDLE.get() {
            if let Ok(res_dir) = app.path().resource_dir() {
                let tw_dir = res_dir.join("tiddlywiki");
                if tw_dir.exists() {
                    return tw_dir;
                }
            }
        }
        // Fallback: try relative to executable
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let candidates = [
                    dir.join("resources").join("tiddlywiki"),
                    dir.join("..").join("lib").join("tiddlydesktop-rs").join("resources").join("tiddlywiki"),
                    dir.join("..").join("Resources").join("tiddlywiki"),
                ];
                for c in &candidates {
                    if c.exists() {
                        return c.clone();
                    }
                }
            }
        }
        std::path::PathBuf::from("resources/tiddlywiki")
    }

    /// Get the synced plugins directory (created if it doesn't exist)
    fn get_synced_plugins_dir(&self) -> std::path::PathBuf {
        let dir = self.data_dir.join("plugins");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// Get extra plugin search paths from TIDDLYWIKI_PLUGIN_PATH env var
    /// and custom plugin path setting. These are directories that contain
    /// plugin subdirectories (e.g. {path}/tiddlywiki/markdown/).
    fn get_extra_plugin_dirs(&self) -> Vec<std::path::PathBuf> {
        let mut dirs = Vec::new();
        // Check TIDDLYWIKI_PLUGIN_PATH env var
        if let Ok(env_path) = std::env::var("TIDDLYWIKI_PLUGIN_PATH") {
            let sep = if cfg!(windows) { ';' } else { ':' };
            for p in env_path.split(sep) {
                if !p.is_empty() {
                    let pb = std::path::PathBuf::from(p);
                    if pb.is_dir() {
                        dirs.push(pb);
                    }
                }
            }
        }
        // Check custom plugin path from app settings
        if let Some(app) = crate::GLOBAL_APP_HANDLE.get() {
            let settings = crate::wiki_storage::load_app_settings(app).unwrap_or_default();
            if let Some(ref custom_uri) = settings.custom_plugin_path_uri {
                if !custom_uri.is_empty() {
                    // On Android this is a SAF URI — the synced local copy lives at
                    // {app_data}/custom_plugins/ (synced by node_bridge::sync_custom_plugins_from_saf)
                    let custom_local = self.data_dir.join("custom_plugins");
                    if custom_local.is_dir() {
                        dirs.push(custom_local);
                    }
                    // On desktop, try as a regular filesystem path
                    #[cfg(not(target_os = "android"))]
                    {
                        let pb = std::path::PathBuf::from(custom_uri);
                        if pb.is_dir() && !dirs.contains(&pb) {
                            dirs.push(pb);
                        }
                    }
                }
            }
        }
        dirs
    }

    /// Handle a RequestWikiFile from a peer — read the wiki and send it back in chunks.
    /// `peer_have_files` lists files the peer already has (with SHA-256 hashes);
    /// we skip sending files whose hash matches.
    async fn handle_request_wiki_file(
        &self,
        from_device_id: &str,
        wiki_id: &str,
        peer_have_files: &[protocol::AttachmentFileInfo],
    ) {
        eprintln!(
            "[LAN Sync] Peer {} requested wiki file: {} (peer has {} files already)",
            from_device_id, wiki_id, peer_have_files.len()
        );

        // Build a lookup set of files the peer already has: rel_path → (sha256_hex, file_size)
        let peer_files: std::collections::HashMap<&str, (&str, u64)> = peer_have_files
            .iter()
            .map(|f| (f.rel_path.as_str(), (f.sha256_hex.as_str(), f.file_size)))
            .collect();

        let app = match GLOBAL_APP_HANDLE.get() {
            Some(a) => a,
            None => return,
        };

        let wiki_path = match crate::wiki_storage::get_wiki_path_by_sync_id(app, wiki_id) {
            Some(p) => p,
            None => {
                eprintln!("[LAN Sync] Wiki {} not found locally, can't serve", wiki_id);
                return;
            }
        };

        // Get wiki info for the response
        let entries = crate::wiki_storage::load_recent_files_from_disk(app);
        let entry = match entries.iter().find(|e| crate::utils::paths_equal(&e.path, &wiki_path)) {
            Some(e) => e.clone(),
            None => return,
        };

        let is_folder = entry.is_folder;
        let wiki_name = entry.filename.clone();
        let from_id = from_device_id.to_string();
        let wid = wiki_id.to_string();

        // Get the sync manager for sending (works via LAN or relay)
        let mgr = match get_sync_manager() {
            Some(m) => m,
            None => return,
        };

        if is_folder {
            // For folder wikis, walk the directory and stream each file.
            // On Android, folder wiki paths are SAF content:// URIs — resolve to
            // the local filesystem mirror so collect_files_recursive can walk it.
            let resolved_path;
            #[cfg(target_os = "android")]
            {
                if wiki_path.starts_with("content://") || wiki_path.starts_with('{') {
                    match crate::android::node_bridge::get_or_create_local_copy(&wiki_path) {
                        Ok(local) => {
                            eprintln!("[LAN Sync] Resolved SAF folder wiki to local: {}", local);
                            resolved_path = local;
                        }
                        Err(e) => {
                            eprintln!("[LAN Sync] Failed to resolve SAF folder wiki: {}", e);
                            return;
                        }
                    }
                } else {
                    resolved_path = wiki_path.clone();
                }
            }
            #[cfg(not(target_os = "android"))]
            {
                resolved_path = wiki_path.clone();
            }

            let folder_path = std::path::Path::new(&resolved_path);
            if !folder_path.is_dir() {
                eprintln!("[LAN Sync] Folder wiki path is not a directory: {}", resolved_path);
                return;
            }

            // Collect all files recursively
            let mut file_list = Vec::new();
            collect_files_recursive(folder_path, folder_path, &mut file_list);

            let mut files_sent = 0u32;
            for (full_path, rel_path) in &file_list {
                let entry = AttachmentEntry {
                    rel_path: rel_path.clone(),
                    source: full_path.to_string_lossy().to_string(),
                };
                match stream_file_chunks(
                    &entry, &wid, &wiki_name, true, &from_id, &mgr,
                ).await {
                    Ok(()) => { files_sent += 1; }
                    Err(e) => {
                        eprintln!("[LAN Sync] Failed to stream file {}: {}", rel_path, e);
                    }
                }
            }

            eprintln!("[LAN Sync] Sent {} files for folder wiki {}", files_sent, wiki_name);
        } else {
            // Single-file wiki — stream the wiki file plus attachments folder.
            // Use the same streaming approach for the wiki file itself to avoid
            // loading potentially large HTML files into memory on Android.
            let wiki_entry = AttachmentEntry {
                rel_path: wiki_name.clone(),
                source: wiki_path.clone(),
            };
            if let Err(e) = stream_file_chunks(
                &wiki_entry, &wid, &wiki_name, false, &from_id, &mgr,
            ).await {
                eprintln!("[LAN Sync] Failed to stream wiki file {}: {}", wiki_name, e);
                return;
            }

            // Also send the attachments folder if it exists
            // Collect file metadata only (not data) to avoid OOM with large attachments
            let attachment_entries = collect_attachment_entries(&wiki_path);
            if !attachment_entries.is_empty() {
                // Skip files the peer already has with matching SHA-256 hash
                // (partially-written files will have a different hash and get re-sent)
                let mut skipped = 0u32;
                let total = attachment_entries.len();
                for entry in &attachment_entries {
                    if let Some(&(_peer_hash, peer_size)) = peer_files.get(entry.rel_path.as_str()) {
                        // Compare file sizes — if they match, the file was fully transferred.
                        // (Partially-written files will have a different size.)
                        // We skip full SHA-256 comparison to avoid reading every file through SAF.
                        let our_size = get_file_size(&entry.source);
                        if our_size > 0 && our_size == peer_size {
                            skipped += 1;
                            continue; // Peer already has this file (same size = fully transferred)
                        }
                        if our_size > 0 && our_size != peer_size {
                            eprintln!(
                                "[LAN Sync] Size mismatch for {}: ours={} peer={} — re-sending",
                                entry.rel_path, our_size, peer_size
                            );
                        }
                        // Size unknown or mismatch — send the file
                    }
                    if let Err(e) = stream_file_chunks(
                        entry, &wid, &wiki_name, false, &from_id, &mgr,
                    ).await {
                        eprintln!("[LAN Sync] Failed to send attachment {}: {}", entry.rel_path, e);
                        continue;
                    }
                }
                if skipped > 0 {
                    eprintln!(
                        "[LAN Sync] Sent {} attachment files for wiki {} (skipped {} already synced)",
                        total - skipped as usize, wiki_name, skipped
                    );
                } else {
                    eprintln!("[LAN Sync] Sent {} attachment files for wiki {}", total, wiki_name);
                }
            }
        }

        // Send completion message
        let complete_msg = SyncMessage::WikiFileComplete {
            wiki_id: wid,
            wiki_name,
            is_folder,
        };
        if let Err(e) = mgr.send_to_peer_any(&from_id, &complete_msg).await {
            eprintln!("[LAN Sync] Failed to send WikiFileComplete: {}", e);
        }
    }

    /// Handle an incoming WikiFileChunk — write chunk data directly to disk
    async fn handle_wiki_file_chunk(
        &self,
        wiki_id: &str,
        wiki_name: &str,
        is_folder: bool,
        filename: &str,
        _chunk_index: u32,
        _chunk_count: u32,
        data_base64: &str,
        from_device_id: &str,
    ) {
        use std::io::Write;

        let mut transfers = self.incoming_transfers.write().await;
        let state = transfers.entry(wiki_id.to_string()).or_insert_with(|| {
            WikiTransferState {
                wiki_name: wiki_name.to_string(),
                is_folder,
                target_dir: String::new(),
                written_files: Vec::new(),
                current_file: None,
                chunks_received: 0,
                room_code: None,
            }
        });

        // Decode the base64 chunk data
        use base64::Engine;
        let data = match base64::engine::general_purpose::STANDARD.decode(data_base64) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[LAN Sync] Base64 decode error for {}: {}", filename, e);
                return;
            }
        };

        // Check if we need to switch to a new file
        let need_new_file = match &state.current_file {
            Some((current_name, _)) => current_name != filename,
            None => true,
        };

        if need_new_file {
            // Close previous file if any, and early-register the wiki
            // when the first file (the wiki HTML) is done receiving.
            if let Some((prev_name, _prev_file)) = state.current_file.take() {
                eprintln!("[LAN Sync] Finished receiving file: {}", prev_name);

                // When the wiki HTML file is the first written file and we're
                // switching to the next file (an attachment), register the wiki
                // locally so interrupted transfers are recognized as "shared"
                // on the next connection (enabling attachment manifest catch-up).
                // Skip on Android — chunks go to temp dir; final registration
                // happens in handle_wiki_file_complete after SAF copy.
                #[cfg(not(target_os = "android"))]
                if state.written_files.len() == 1 && !is_folder {
                    if let Some(app) = GLOBAL_APP_HANDLE.get() {
                        if !crate::wiki_storage::has_wiki_with_sync_id(app, wiki_id) {
                            let wiki_path = state.written_files[0].1.to_string_lossy().to_string();
                            // Use room_code from transfer state if available,
                            // otherwise fall back to peer lookup
                            let relay_room = if state.room_code.is_some() {
                                state.room_code.clone()
                            } else if let Some(mgr) = get_sync_manager() {
                                mgr.peers.read().await
                                    .get(from_device_id)
                                    .and_then(|pc| pc.auth_room_codes.first().cloned())
                            } else {
                                None
                            };
                            let entry = crate::types::WikiEntry {
                                path: wiki_path.clone(),
                                filename: wiki_name.to_string(),
                                display_path: None,
                                favicon: None,
                                is_folder: false,
                                backups_enabled: true,
                                backup_dir: None,
                                backup_count: None,
                                group: None,
                                sync_enabled: true,
                                sync_id: Some(wiki_id.to_string()),
                                sync_peers: vec![],
                                relay_room,
                            };
                            if let Err(e) = crate::wiki_storage::add_to_recent_files(app, entry) {
                                eprintln!("[LAN Sync] Failed to early-register wiki: {}", e);
                            } else {
                                eprintln!("[LAN Sync] Early-registered wiki '{}' ({})", wiki_name, wiki_id);
                            }
                        }
                    }
                }
            }

            // Determine the file path.
            // On Android, target_dir is a content:// URI — we can't write via std::fs,
            // so use a temp dir in app data and copy to SAF on completion.
            let target_dir = {
                #[cfg(target_os = "android")]
                {
                    if let Some(app) = GLOBAL_APP_HANDLE.get() {
                        let mut dir = app.path()
                            .app_data_dir()
                            .unwrap_or_else(|_| std::path::PathBuf::from("."));
                        dir.push("sync_temp");
                        dir.push(wiki_id);
                        let _ = std::fs::create_dir_all(&dir);
                        dir
                    } else {
                        std::path::PathBuf::from(".")
                    }
                }
                #[cfg(not(target_os = "android"))]
                {
                    if state.target_dir.is_empty() {
                        if let Some(app) = GLOBAL_APP_HANDLE.get() {
                            app.path()
                                .download_dir()
                                .unwrap_or_else(|_| std::path::PathBuf::from("."))
                        } else {
                            std::path::PathBuf::from(".")
                        }
                    } else {
                        std::path::PathBuf::from(&state.target_dir)
                    }
                }
            };

            let file_path = if is_folder {
                let folder_dir = target_dir.join(wiki_name);
                match validate_contained_path(&folder_dir, filename) {
                    Some(p) => p,
                    None => {
                        eprintln!(
                            "[LAN Sync] Security: Rejected wiki file with unsafe path: {}",
                            filename
                        );
                        return;
                    }
                }
            } else {
                // Single-file wiki: filename should be just the wiki name, no subdirectories
                if filename.contains('/') || filename.contains('\\') || filename.contains("..") {
                    eprintln!(
                        "[LAN Sync] Security: Rejected single-file wiki name with path components: {}",
                        filename
                    );
                    return;
                }
                target_dir.join(filename)
            };

            // Create parent directories
            if let Some(parent) = file_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }

            // Open file for writing
            match std::fs::File::create(&file_path) {
                Ok(f) => {
                    state.written_files.push((filename.to_string(), file_path));
                    state.current_file = Some((filename.to_string(), f));
                }
                Err(e) => {
                    eprintln!("[LAN Sync] Failed to create file {:?}: {}", file_path, e);
                    return;
                }
            }
        }

        // Write chunk data to the open file
        if let Some((_, ref mut file)) = state.current_file {
            if !data.is_empty() {
                if let Err(e) = file.write_all(&data) {
                    eprintln!("[LAN Sync] Failed to write chunk for {}: {}", filename, e);
                }
            }
        }

        state.chunks_received += 1;
    }

    /// Handle WikiFileComplete — assemble and write the wiki to disk
    async fn handle_wiki_file_complete(&self, wiki_id: &str, wiki_name: &str, is_folder: bool, from_device_id: &str) {
        let transfer = {
            let mut transfers = self.incoming_transfers.write().await;
            transfers.remove(wiki_id)
        };

        let mut transfer = match transfer {
            Some(t) => t,
            None => {
                eprintln!("[LAN Sync] WikiFileComplete for unknown transfer: {}", wiki_id);
                return;
            }
        };

        // Close last open file
        if let Some((last_name, _)) = transfer.current_file.take() {
            eprintln!("[LAN Sync] Finished receiving file: {}", last_name);
        }

        eprintln!(
            "[LAN Sync] Wiki transfer complete: '{}' ({} files, {} chunks)",
            wiki_name,
            transfer.written_files.len(),
            transfer.chunks_received
        );

        if transfer.written_files.is_empty() {
            eprintln!("[LAN Sync] No files received in transfer");
            return;
        }

        // Find the wiki file path
        let wiki_path;

        #[cfg(target_os = "android")]
        {
            // On Android, copy files from temp dir to the SAF target directory
            match copy_transfer_to_saf(
                wiki_id,
                wiki_name,
                is_folder,
                &transfer.target_dir,
                &transfer.written_files,
            ) {
                Ok(saf_wiki_path) => {
                    wiki_path = saf_wiki_path;
                    // Clean up temp dir
                    if let Some(app) = GLOBAL_APP_HANDLE.get() {
                        if let Ok(data_dir) = app.path().app_data_dir() {
                            let temp_dir = data_dir.join("sync_temp").join(wiki_id);
                            let _ = std::fs::remove_dir_all(&temp_dir);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[LAN Sync] Failed to copy wiki to SAF: {}", e);
                    // Clean up temp dir
                    if let Some(app) = GLOBAL_APP_HANDLE.get() {
                        if let Ok(data_dir) = app.path().app_data_dir() {
                            let temp_dir = data_dir.join("sync_temp").join(wiki_id);
                            let _ = std::fs::remove_dir_all(&temp_dir);
                        }
                    }
                    return;
                }
            }
        }

        #[cfg(not(target_os = "android"))]
        {
            wiki_path = if is_folder {
                let target_dir = if transfer.target_dir.is_empty() {
                    if let Some(app) = GLOBAL_APP_HANDLE.get() {
                        app.path().download_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
                    } else {
                        std::path::PathBuf::from(".")
                    }
                } else {
                    std::path::PathBuf::from(&transfer.target_dir)
                };
                target_dir.join(wiki_name).to_string_lossy().to_string()
            } else {
                // Find the wiki HTML file (not attachment files)
                let mut wiki_file_path = String::new();
                for (filename, path) in &transfer.written_files {
                    if filename == wiki_name {
                        wiki_file_path = path.to_string_lossy().to_string();
                        break;
                    }
                }
                if wiki_file_path.is_empty() {
                    // Fallback: use the first file
                    wiki_file_path = transfer.written_files[0].1.to_string_lossy().to_string();
                }
                wiki_file_path
            };
        }

        eprintln!("[LAN Sync] Wiki received and saved to: {}", wiki_path);

        // Add to recent files and enable sync
        if let Some(app) = GLOBAL_APP_HANDLE.get() {
            // Use room_code from transfer state (passed from UI) if available,
            // otherwise fall back to peer connection lookup
            let relay_room = if transfer.room_code.is_some() {
                transfer.room_code.clone()
            } else {
                // Check LAN peer's auth rooms (use first shared room)
                let lan_room = self.peers.read().await
                    .get(from_device_id)
                    .and_then(|pc| pc.auth_room_codes.first().cloned());
                if let Some(rc) = lan_room {
                    Some(rc)
                } else if let Some(relay) = self.relay_manager.as_ref() {
                    relay.find_device_room(from_device_id).await
                } else {
                    None
                }
            };
            let relay_room_for_event = relay_room.clone();
            let entry = crate::types::WikiEntry {
                path: wiki_path.clone(),
                filename: wiki_name.to_string(),
                display_path: None,
                favicon: None,
                is_folder,
                backups_enabled: true,
                backup_dir: None,
                backup_count: None,
                group: None,
                sync_enabled: true,
                sync_id: Some(wiki_id.to_string()),
                sync_peers: vec![],
                relay_room,
            };
            if let Err(e) = crate::wiki_storage::add_to_recent_files(app, entry) {
                eprintln!("[LAN Sync] Failed to add received wiki to recent files: {}", e);
            }

            // Emit event to refresh the wiki list
            let _ = app.emit("lan-sync-wiki-received", serde_json::json!({
                "wiki_id": wiki_id,
                "wiki_name": wiki_name,
                "wiki_path": wiki_path,
                "is_folder": is_folder,
                "relay_room": relay_room_for_event,
            }));
        }
    }

    /// Request a wiki file from a peer (called from Tauri command)
    pub async fn request_wiki_from_peer(
        &self,
        wiki_id: &str,
        from_device_id: &str,
        target_dir: &str,
        room_code: Option<String>,
    ) -> Result<(), String> {
        // Pre-register the transfer state with the target directory
        {
            let mut transfers = self.incoming_transfers.write().await;
            transfers.insert(wiki_id.to_string(), WikiTransferState {
                wiki_name: String::new(),
                is_folder: false,
                target_dir: target_dir.to_string(),
                written_files: Vec::new(),
                current_file: None,
                chunks_received: 0,
                room_code,
            });
        }

        // Build a manifest of files we already have in the target directory
        // so the sender can skip files that are already fully synced.
        let have_files = {
            let target_path = std::path::Path::new(target_dir);
            if target_path.is_dir() {
                let mut files = Vec::new();
                // Check for attachments folder
                let attachments_dir = target_path.join("attachments");
                if attachments_dir.is_dir() {
                    let mut file_list = Vec::new();
                    collect_files_recursive(target_path, &attachments_dir, &mut file_list);
                    for (full_path, rel_path) in &file_list {
                        if let Some(hash) = compute_file_sha256_hex(&full_path.to_string_lossy()) {
                            let file_size = std::fs::metadata(full_path)
                                .map(|m| m.len())
                                .unwrap_or(0);
                            files.push(protocol::AttachmentFileInfo {
                                rel_path: rel_path.clone(),
                                sha256_hex: hash,
                                file_size,
                            });
                        }
                    }
                }
                eprintln!(
                    "[LAN Sync] Sending RequestWikiFile with {} existing files",
                    files.len()
                );
                files
            } else {
                Vec::new()
            }
        };

        // Send the request to the peer (via LAN server or relay)
        let msg = SyncMessage::RequestWikiFile {
            wiki_id: wiki_id.to_string(),
            have_files,
        };
        if let Some(ref s) = *self.server.read().await {
            s.send_to_peer(from_device_id, &msg).await?;
        } else if let Some(relay) = &self.relay_manager {
            relay.send_to_peer(from_device_id, &msg).await?;
        } else {
            return Err("No sync connection available".to_string());
        }
        eprintln!("[LAN Sync] Requested wiki {} from peer {}", wiki_id, from_device_id);
        Ok(())
    }

    /// Build and send our attachment manifest for a wiki to a specific peer.
    /// The manifest lists all files in our attachments directory with SHA-256 hashes
    /// so the peer can detect missing or outdated files.
    async fn send_attachment_manifest(&self, to_device_id: &str, wiki_id: &str, wiki_path: &str) {
        let entries = collect_attachment_entries(wiki_path);
        if entries.is_empty() {
            return;
        }

        // Move SHA-256 computation off the event loop into a blocking thread
        let entry_data: Vec<(String, String)> = entries
            .iter()
            .map(|e| (e.rel_path.clone(), e.source.clone()))
            .collect();

        let files = match tokio::task::spawn_blocking(move || {
            let mut files = Vec::new();
            for (rel_path, source) in &entry_data {
                if let Some(hash_hex) = compute_file_sha256_hex(source) {
                    let file_size = {
                        #[cfg(target_os = "android")]
                        { 0u64 }
                        #[cfg(not(target_os = "android"))]
                        { std::fs::metadata(source).map(|m| m.len()).unwrap_or(0) }
                    };
                    files.push(protocol::AttachmentFileInfo {
                        rel_path: rel_path.clone(),
                        sha256_hex: hash_hex,
                        file_size,
                    });
                }
            }
            files
        }).await {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[LAN Sync] Attachment manifest hashing failed: {}", e);
                return;
            }
        };

        if files.is_empty() {
            return;
        }

        eprintln!(
            "[LAN Sync] Sending attachment manifest for wiki {} ({} files) to {}",
            wiki_id,
            files.len(),
            to_device_id
        );

        let msg = SyncMessage::AttachmentManifest {
            wiki_id: wiki_id.to_string(),
            files,
        };
        let _ = self.send_to_peer_any(to_device_id, &msg).await;
    }

    /// Send a single attachment file to all connected peers.
    /// Called when a tiddler with _canonical_uri changes — much faster than
    /// broadcast_attachment_manifest because it only sends one file instead of
    /// hashing all 100+ attachments.
    async fn broadcast_single_attachment(&self, wiki_id: &str, canonical_uri: &str) {
        if !self.is_sync_active().await {
            return;
        }
        let app = match GLOBAL_APP_HANDLE.get() {
            Some(a) => a,
            None => return,
        };

        let wiki_path = match crate::wiki_storage::get_wiki_path_by_sync_id(app, wiki_id) {
            Some(p) => p,
            None => return,
        };

        // Normalize the relative path (strip leading "./")
        let rel_path = canonical_uri.strip_prefix("./").unwrap_or(canonical_uri);

        // Find this specific file in the attachment entries.
        // On Android, the file may still be copying in a background thread (race condition
        // with copyToAttachments in WikiActivity.kt), so retry with increasing delays.
        let entry;
        #[cfg(target_os = "android")]
        {
            let wp = wiki_path.clone();
            let rp = rel_path.to_string();
            let found = tokio::task::spawn_blocking(move || {
                for attempt in 0..4u64 {
                    if attempt > 0 {
                        std::thread::sleep(std::time::Duration::from_secs(attempt));
                    }
                    let entries = collect_attachment_entries(&wp);
                    if let Some(e) = entries.into_iter().find(|e| e.rel_path == rp) {
                        return Some(e);
                    }
                }
                None
            }).await.unwrap_or(None);
            entry = match found {
                Some(e) => e,
                None => {
                    eprintln!(
                        "[LAN Sync] Attachment file not found after retries: {}",
                        rel_path
                    );
                    return;
                }
            };
        }
        #[cfg(not(target_os = "android"))]
        {
            let all_entries = collect_attachment_entries(&wiki_path);
            entry = match all_entries.into_iter().find(|e| e.rel_path == rel_path) {
                Some(e) => e,
                None => {
                    eprintln!(
                        "[LAN Sync] Attachment file not found for broadcast: {}",
                        rel_path
                    );
                    return;
                }
            };
        }

        // Collect peers for this wiki's room (LAN + relay, room-filtered)
        let peer_ids = self.get_all_peers_for_wiki(wiki_id).await;

        if peer_ids.is_empty() {
            return;
        }

        eprintln!(
            "[LAN Sync] Broadcasting single attachment '{}' to {} peers",
            rel_path,
            peer_ids.len()
        );

        for peer_id in peer_ids {
            if let Err(e) = send_attachment_to_peer(&entry, wiki_id, &peer_id, self).await {
                eprintln!(
                    "[LAN Sync] Failed to send attachment {} to {}: {}",
                    rel_path, peer_id, e
                );
            }
        }
    }

    /// Periodically scan attachment directories on Android and broadcast changes.
    /// Since Android SAF doesn't support inotify-style watches, we poll every 30s.
    #[cfg(target_os = "android")]
    async fn scan_android_attachments(&self) {
        // Skip if no connected peers
        if !self.is_sync_active().await {
            return;
        }
        // Skip if no connected peers (LAN or relay)
        if self.connected_peers_all().await.is_empty() {
            return;
        }

        let app = match GLOBAL_APP_HANDLE.get() {
            Some(a) => a,
            None => return,
        };

        let sync_wikis = crate::wiki_storage::get_sync_enabled_wikis(app);
        for (sync_id, _name, is_folder) in &sync_wikis {
            if *is_folder {
                continue;
            }
            // Only scan wikis assigned to a connected room with room-authenticated peers
            let peer_ids = self.get_all_peers_for_wiki(sync_id).await;
            if peer_ids.is_empty() {
                continue;
            }
            let wiki_path = match crate::wiki_storage::get_wiki_path_by_sync_id(app, sync_id) {
                Some(p) => p,
                None => continue,
            };

            // Collect entries with sizes in a blocking task (SAF calls)
            let wp = wiki_path.clone();
            let entries_with_size = match tokio::task::spawn_blocking(move || {
                collect_attachment_entries_with_size(&wp)
            }).await {
                Ok(v) => v,
                Err(_) => continue,
            };

            // Build the snapshot for diffing: (rel_path, size)
            let fresh_snapshot: Vec<(String, u64)> = entries_with_size
                .iter()
                .map(|(e, sz)| (e.rel_path.clone(), *sz))
                .collect();

            // Diff against cache
            let (changed, deleted) = self
                .attachment_manager
                .diff_attachment_snapshot(sync_id, &fresh_snapshot);

            // Build a lookup from rel_path → AttachmentEntry for changed files
            let entry_map: HashMap<&str, &AttachmentEntry> = entries_with_size
                .iter()
                .map(|(e, _)| (e.rel_path.as_str(), e))
                .collect();

            // Broadcast changed files (respecting echo suppression + extension allowlist)
            for rel_path in &changed {
                if !attachments::is_allowed_attachment(rel_path) {
                    continue;
                }
                if self.attachment_manager.should_suppress(sync_id, rel_path) {
                    continue;
                }
                if let Some(entry) = entry_map.get(rel_path.as_str()) {
                    for peer_id in &peer_ids {
                        if let Err(e) =
                            send_attachment_to_peer(entry, sync_id, peer_id, self).await
                        {
                            eprintln!(
                                "[LAN Sync] scan: failed to send {} to {}: {}",
                                rel_path, peer_id, e
                            );
                        }
                    }
                }
            }

            // Broadcast deletions (respecting echo suppression + extension allowlist)
            if !peer_ids.is_empty() {
                for rel_path in &deleted {
                    if !attachments::is_allowed_attachment(rel_path) {
                        continue;
                    }
                    if self.attachment_manager.should_suppress(sync_id, rel_path) {
                        continue;
                    }
                    let msg = SyncMessage::AttachmentDeleted {
                        wiki_id: sync_id.clone(),
                        filename: rel_path.clone(),
                    };
                    self.send_to_peers_any(&peer_ids, &msg).await;
                    eprintln!(
                        "[LAN Sync] scan: broadcast deletion of {} to {} peers",
                        rel_path, peer_ids.len()
                    );
                }
            }

            // Update cache with fresh snapshot
            self.attachment_manager
                .update_attachment_cache(sync_id, fresh_snapshot);
        }
    }

    /// Broadcast attachment manifest for a wiki to ALL connected peers.
    /// Used on initial connection to sync all missing attachments.
    async fn broadcast_attachment_manifest(&self, wiki_id: &str) {
        if !self.is_sync_active().await {
            return;
        }
        let app = match GLOBAL_APP_HANDLE.get() {
            Some(a) => a,
            None => return,
        };

        let wiki_path = match crate::wiki_storage::get_wiki_path_by_sync_id(app, wiki_id) {
            Some(p) => p,
            None => return,
        };

        let peers = self.server.read().await;
        let peer_ids: Vec<String> = if let Some(ref server) = *peers {
            server.connected_peers().await.into_iter().map(|(id, _, _)| id).collect()
        } else {
            return;
        };
        drop(peers);

        for peer_id in peer_ids {
            self.send_attachment_manifest(&peer_id, wiki_id, &wiki_path).await;
        }
    }

    /// Handle an incoming AttachmentManifest — compare with local files and request missing ones
    async fn handle_attachment_manifest(
        &self,
        from_device_id: &str,
        wiki_id: &str,
        remote_files: &[protocol::AttachmentFileInfo],
    ) {
        let app = match GLOBAL_APP_HANDLE.get() {
            Some(a) => a,
            None => return,
        };

        let wiki_path = match crate::wiki_storage::get_wiki_path_by_sync_id(app, wiki_id) {
            Some(p) => p,
            None => return,
        };

        eprintln!(
            "[LAN Sync] Received attachment manifest for wiki {} ({} files) from {}",
            wiki_id,
            remote_files.len(),
            from_device_id
        );

        // Compare remote files with our local files (hash computation off event loop)
        let local_entries = collect_attachment_entries(&wiki_path);
        let entry_data: Vec<(String, String)> = local_entries
            .iter()
            .map(|e| (e.rel_path.clone(), e.source.clone()))
            .collect();

        let local_hashes: HashMap<String, String> = match tokio::task::spawn_blocking(move || {
            entry_data
                .iter()
                .filter_map(|(rel_path, source)| {
                    compute_file_sha256_hex(source)
                        .map(|hash| (rel_path.clone(), hash))
                })
                .collect::<HashMap<String, String>>()
        }).await {
            Ok(h) => h,
            Err(e) => {
                eprintln!("[LAN Sync] Attachment manifest comparison hashing failed: {}", e);
                return;
            }
        };

        // Find files that are missing or have different hashes
        let mut needed: Vec<String> = Vec::new();
        for remote_file in remote_files {
            // Skip disallowed file types
            if !attachments::is_allowed_attachment(&remote_file.rel_path) {
                continue;
            }
            match local_hashes.get(&remote_file.rel_path) {
                Some(local_hash) if local_hash == &remote_file.sha256_hex => {
                    // File is up to date
                }
                _ => {
                    // Missing or hash mismatch
                    needed.push(remote_file.rel_path.clone());
                }
            }
        }

        if needed.is_empty() {
            eprintln!("[LAN Sync] All attachments up to date for wiki {}", wiki_id);
            return;
        }

        eprintln!(
            "[LAN Sync] Requesting {} missing/outdated attachments for wiki {} from {}",
            needed.len(),
            wiki_id,
            from_device_id
        );

        let msg = SyncMessage::RequestAttachments {
            wiki_id: wiki_id.to_string(),
            files: needed,
        };
        let _ = self.send_to_peer_any(from_device_id, &msg).await;
    }

    /// Handle a RequestAttachments message — send the requested files to the peer
    /// using AttachmentChanged + AttachmentChunk messages so the receiver writes
    /// them to the correct wiki attachment directory (not the download directory).
    async fn handle_request_attachments(
        &self,
        from_device_id: &str,
        wiki_id: &str,
        requested_files: &[String],
    ) {
        let app = match GLOBAL_APP_HANDLE.get() {
            Some(a) => a,
            None => return,
        };

        let wiki_path = match crate::wiki_storage::get_wiki_path_by_sync_id(app, wiki_id) {
            Some(p) => p,
            None => return,
        };

        eprintln!(
            "[LAN Sync] Sending {} requested attachments for wiki {} to {}",
            requested_files.len(),
            wiki_id,
            from_device_id
        );

        // Build a set of requested file paths for quick lookup
        let requested_set: HashSet<&str> = requested_files.iter().map(|s| s.as_str()).collect();

        // Get all attachment entries and filter to only requested ones
        let all_entries = collect_attachment_entries(&wiki_path);

        for entry in &all_entries {
            if !requested_set.contains(entry.rel_path.as_str()) {
                continue;
            }
            if let Err(e) =
                send_attachment_to_peer(entry, wiki_id, from_device_id, self).await
            {
                eprintln!(
                    "[LAN Sync] Failed to send requested attachment {}: {}",
                    entry.rel_path, e
                );
            }
        }
    }
}

/// Get file size. On Android uses SAF; on desktop uses std::fs.
fn get_file_size(source: &str) -> u64 {
    #[cfg(target_os = "android")]
    {
        crate::android::saf::get_document_size(source).unwrap_or(0)
    }
    #[cfg(not(target_os = "android"))]
    {
        std::fs::metadata(source).map(|m| m.len()).unwrap_or(0)
    }
}

/// Copy received wiki files from temp dir to a SAF content:// target directory on Android.
/// Returns the SAF URI path of the wiki (either the HTML file URI or the folder URI).
#[cfg(target_os = "android")]
fn copy_transfer_to_saf(
    wiki_id: &str,
    wiki_name: &str,
    is_folder: bool,
    target_dir_uri: &str,
    written_files: &[(String, std::path::PathBuf)],
) -> Result<String, String> {
    use crate::android::saf;

    eprintln!(
        "[LAN Sync] Copying {} files to SAF: {}",
        written_files.len(),
        target_dir_uri
    );

    // For folder wikis, create a subdirectory named after the wiki
    let base_dir_uri = if is_folder {
        saf::find_or_create_subdirectory(target_dir_uri, wiki_name)?
    } else {
        target_dir_uri.to_string()
    };

    let mut wiki_file_uri = String::new();

    for (filename, temp_path) in written_files {
        // Read file content from temp dir
        let content = std::fs::read(temp_path).map_err(|e| {
            format!("Failed to read temp file {:?}: {}", temp_path, e)
        })?;

        // Determine which SAF directory to create the file in
        // Files may have subdirectory paths like "tiddlers/MyTiddler.tid" or "attachments/photo.jpg"
        let parts: Vec<&str> = filename.split('/').collect();
        let (parent_uri, file_name) = if parts.len() > 1 {
            // Need to create subdirectories
            let mut current_uri = base_dir_uri.clone();
            for dir_part in &parts[..parts.len() - 1] {
                current_uri = saf::find_or_create_subdirectory(&current_uri, dir_part)?;
            }
            (current_uri, parts[parts.len() - 1])
        } else {
            (base_dir_uri.clone(), filename.as_str())
        };

        // Create the file in SAF and write content
        let file_uri = saf::create_file(&parent_uri, file_name, None)?;
        saf::write_document_bytes(&file_uri, &content)?;

        // Track the wiki HTML file URI (for single-file wikis)
        if filename == wiki_name {
            wiki_file_uri = file_uri.clone();
        }

        eprintln!("[LAN Sync] Copied to SAF: {}", filename);
    }

    // Note: SAF permissions for target_dir_uri are already persisted by the folder picker.
    // Do NOT call saf::persist_permission() here — it uses block_on() which panics
    // when called from an async context (handle_wiki_file_complete).

    // Return the wiki path (SAF URI)
    let result = if is_folder {
        base_dir_uri
    } else if !wiki_file_uri.is_empty() {
        wiki_file_uri
    } else if !written_files.is_empty() {
        // Fallback: return the target dir + wiki name
        eprintln!("[LAN Sync] Warning: wiki file URI not found, using target dir");
        target_dir_uri.to_string()
    } else {
        return Err("No files to copy".to_string());
    };

    eprintln!("[LAN Sync] Wiki copied to SAF, path: {}", result);
    Ok(result)
}

/// Compute SHA-256 hash of a file and return as hex string.
/// On Android, reads via SAF; on Desktop, reads from filesystem.
fn compute_file_sha256_hex(source: &str) -> Option<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let reader_result: Result<Box<dyn std::io::Read>, String> = {
        #[cfg(target_os = "android")]
        {
            crate::android::saf::open_document_reader(source)
        }
        #[cfg(not(target_os = "android"))]
        {
            std::fs::File::open(source)
                .map(|f| Box::new(f) as Box<dyn std::io::Read>)
                .map_err(|e| format!("{}", e))
        }
    };

    match reader_result {
        Ok(mut reader) => {
            let mut hasher = Sha256::new();
            let mut buf = vec![0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => hasher.update(&buf[..n]),
                    Err(_) => return None,
                }
            }
            let hash = hasher.finalize();
            Some(hash.iter().map(|b| format!("{:02x}", b)).collect::<String>())
        }
        Err(_) => None,
    }
}

/// Metadata for an attachment file (no data loaded yet)
struct AttachmentEntry {
    rel_path: String,
    /// On Android: SAF URI string; on Desktop: filesystem path string
    source: String,
}

/// Stream an attachment file in chunks over the WebSocket using constant memory.
///
/// Uses a dedicated reader thread + bounded channel (4 chunks ≈ 1MB buffer) to
/// avoid loading entire files into memory. This is critical for large video/audio
/// files on Android where memory is limited.
///
/// On Android, SAF file handles aren't Send, so the file is first copied to a
/// temporary local file, then streamed from there.
/// Stream a file as WikiFileChunk messages to a peer using bounded channels.
/// Works for wiki files, folder wiki files, and attachment files.
/// On Android, copies SAF content to a temp file first (SAF handles aren't Send).
async fn stream_file_chunks(
    entry: &AttachmentEntry,
    wiki_id: &str,
    wiki_name: &str,
    is_folder: bool,
    to_peer: &str,
    mgr: &SyncManager,
) -> Result<(), String> {
    use std::io::Read;

    let chunk_size = protocol::ATTACHMENT_CHUNK_SIZE;

    // On Android, SAF content:// URIs need to be copied to a temp file first
    // (SAF handles aren't Send, so we can't use them from a std::thread reader thread).
    // Local filesystem paths (e.g., from folder wiki mirrors) can be used directly.
    // On desktop, use the source path directly.
    #[cfg(target_os = "android")]
    let (local_path, is_temp) = {
        let is_saf = entry.source.starts_with("content://") || entry.source.starts_with('{');
        if is_saf {
            let temp_dir = if let Some(app) = GLOBAL_APP_HANDLE.get() {
                app.path()
                    .cache_dir()
                    .unwrap_or_else(|_| std::env::temp_dir())
            } else {
                std::env::temp_dir()
            };
            let temp_path = temp_dir.join(format!(
                "td_sync_{:x}",
                md5::compute(entry.source.as_bytes())
            ));
            {
                let mut reader = crate::android::saf::open_document_reader(&entry.source)?;
                let mut file = std::fs::File::create(&temp_path)
                    .map_err(|e| format!("Create temp failed: {}", e))?;
                std::io::copy(&mut reader, &mut file)
                    .map_err(|e| format!("Copy to temp failed: {}", e))?;
            }
            (temp_path, true)
        } else {
            // Local filesystem path — use directly (no SAF needed)
            (std::path::PathBuf::from(&entry.source), false)
        }
    };

    #[cfg(not(target_os = "android"))]
    let (local_path, is_temp) = (std::path::PathBuf::from(&entry.source), false);

    // Stream from the local file using a bounded channel.
    // A dedicated reader thread reads one chunk at a time and feeds the channel.
    // The async side receives chunks and sends them to the peer.
    // Max memory: ~8 chunks (8MB) in the channel + 1 chunk (1MB) read buffer.
    let (tx, mut rx) = mpsc::channel::<String>(8);
    let read_path = local_path.clone();
    let read_handle = std::thread::spawn(move || -> Result<(), String> {
        let mut file = std::fs::File::open(&read_path)
            .map_err(|e| format!("Failed to open {}: {}", read_path.display(), e))?;
        let mut buf = vec![0u8; chunk_size];
        loop {
            let n = file
                .read(&mut buf)
                .map_err(|e| format!("Read error: {}", e))?;
            if n == 0 {
                break;
            }
            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&buf[..n]);
            // blocking_send applies backpressure — blocks if channel is full
            if tx.blocking_send(b64).is_err() {
                break; // Receiver dropped (e.g., send error)
            }
        }
        Ok(())
    });

    let rel_path = &entry.rel_path;
    let mut idx = 0u32;
    while let Some(b64) = rx.recv().await {
        let msg = SyncMessage::WikiFileChunk {
            wiki_id: wiki_id.to_string(),
            wiki_name: wiki_name.to_string(),
            is_folder,
            filename: rel_path.clone(),
            chunk_index: idx,
            chunk_count: 0, // Unknown upfront; receiver uses filename change, not count
            data_base64: b64,
        };
        mgr.send_to_peer_any(to_peer, &msg).await?;
        idx += 1;
    }

    // Wait for reader thread to finish
    match read_handle.join() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            if is_temp {
                let _ = std::fs::remove_file(&local_path);
            }
            return Err(format!("Reader error for {}: {}", rel_path, e));
        }
        Err(_) => {
            if is_temp {
                let _ = std::fs::remove_file(&local_path);
            }
            return Err(format!("Reader thread panicked for {}", rel_path));
        }
    }

    // Handle empty files
    if idx == 0 {
        use base64::Engine;
        let msg = SyncMessage::WikiFileChunk {
            wiki_id: wiki_id.to_string(),
            wiki_name: wiki_name.to_string(),
            is_folder,
            filename: rel_path.clone(),
            chunk_index: 0,
            chunk_count: 1,
            data_base64: base64::engine::general_purpose::STANDARD.encode(b""),
        };
        mgr.send_to_peer_any(to_peer, &msg).await?;
    }

    // Cleanup temp file (Android)
    if is_temp {
        let _ = std::fs::remove_file(&local_path);
    }

    eprintln!(
        "[LAN Sync] Streamed attachment {} ({} chunks)",
        rel_path, idx
    );
    Ok(())
}

/// Send an attachment file to a specific peer as AttachmentChanged + AttachmentChunk messages.
/// This ensures the receiver writes the file to the wiki's attachment directory (via
/// handle_attachment_changed + handle_attachment_chunk in attachments.rs).
/// Routes via LAN or relay depending on peer connectivity (uses send_to_peer_any).
async fn send_attachment_to_peer(
    entry: &AttachmentEntry,
    wiki_id: &str,
    to_peer: &str,
    mgr: &SyncManager,
) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;

    let chunk_size = protocol::ATTACHMENT_CHUNK_SIZE;

    // On Android, copy SAF to temp file (SAF handles aren't Send)
    #[cfg(target_os = "android")]
    let (local_path, is_temp) = {
        let temp_dir = if let Some(app) = GLOBAL_APP_HANDLE.get() {
            app.path()
                .cache_dir()
                .unwrap_or_else(|_| std::env::temp_dir())
        } else {
            std::env::temp_dir()
        };
        let temp_path = temp_dir.join(format!(
            "td_att_{:x}",
            md5::compute(entry.source.as_bytes())
        ));
        {
            let mut reader = crate::android::saf::open_document_reader(&entry.source)?;
            let mut file = std::fs::File::create(&temp_path)
                .map_err(|e| format!("Create temp failed: {}", e))?;
            std::io::copy(&mut reader, &mut file)
                .map_err(|e| format!("Copy to temp failed: {}", e))?;
        }
        (temp_path, true)
    };

    #[cfg(not(target_os = "android"))]
    let (local_path, is_temp) = (std::path::PathBuf::from(&entry.source), false);

    // First pass: compute SHA-256 hash and file size
    let (sha256, file_size) = {
        let mut hasher = Sha256::new();
        let mut file = std::fs::File::open(&local_path)
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
        ((file_size as usize + chunk_size - 1) / chunk_size).max(1) as u32;

    // Send AttachmentChanged header
    mgr.send_to_peer_any(
        to_peer,
        &SyncMessage::AttachmentChanged {
            wiki_id: wiki_id.to_string(),
            filename: entry.rel_path.clone(),
            file_size,
            sha256,
            chunk_count,
        },
    )
    .await?;

    // Second pass: stream chunks
    let (tx, mut rx) = mpsc::channel::<String>(8);
    let read_path = local_path.clone();
    let read_handle = std::thread::spawn(move || -> Result<(), String> {
        let mut file = std::fs::File::open(&read_path)
            .map_err(|e| format!("Open failed: {}", e))?;
        let mut buf = vec![0u8; chunk_size];
        loop {
            let n = file
                .read(&mut buf)
                .map_err(|e| format!("Read error: {}", e))?;
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
    let fname = entry.rel_path.clone();
    let mut idx = 0u32;
    while let Some(b64) = rx.recv().await {
        mgr.send_to_peer_any(
            to_peer,
            &SyncMessage::AttachmentChunk {
                wiki_id: wid.clone(),
                filename: fname.clone(),
                chunk_index: idx,
                data_base64: b64,
            },
        )
        .await?;
        idx += 1;
        // Pace chunk sends to avoid saturating relay and leave bandwidth for tiddler sync
        tokio::time::sleep(std::time::Duration::from_millis(
            protocol::ATTACHMENT_CHUNK_DELAY_MS,
        ))
        .await;
    }

    // Handle empty files
    if idx == 0 {
        use base64::Engine;
        mgr.send_to_peer_any(
            to_peer,
            &SyncMessage::AttachmentChunk {
                wiki_id: wid.clone(),
                filename: fname.clone(),
                chunk_index: 0,
                data_base64: base64::engine::general_purpose::STANDARD.encode(b""),
            },
        )
        .await?;
    }

    // Wait for reader thread
    match read_handle.join() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            if is_temp {
                let _ = std::fs::remove_file(&local_path);
            }
            return Err(e);
        }
        Err(_) => {
            if is_temp {
                let _ = std::fs::remove_file(&local_path);
            }
            return Err("Reader thread panicked".to_string());
        }
    }

    if is_temp {
        let _ = std::fs::remove_file(&local_path);
    }

    eprintln!(
        "[LAN Sync] Sent attachment {} to peer ({} chunks)",
        entry.rel_path, idx
    );
    Ok(())
}

/// Collect attachment file metadata (not data) for a wiki.
/// Files are read individually later to avoid loading all into memory at once.
fn collect_attachment_entries(wiki_path: &str) -> Vec<AttachmentEntry> {
    let mut result = Vec::new();

    #[cfg(target_os = "android")]
    {
        // On Android, use SAF to find the attachments folder
        eprintln!("[LAN Sync] collect_attachment_entries for wiki_path: {}", wiki_path);
        match crate::android::saf::get_parent_uri(wiki_path) {
            Ok(parent_uri) => {
                eprintln!("[LAN Sync] Computed parent URI: {}", parent_uri);
                match crate::android::saf::find_subdirectory(&parent_uri, "attachments") {
                    Ok(Some(attachments_uri)) => {
                        eprintln!("[LAN Sync] Found attachments folder: {}", attachments_uri);
                        collect_saf_attachment_entries(&attachments_uri, "attachments", &mut result);
                    }
                    Ok(None) => {
                        eprintln!("[LAN Sync] No attachments folder found next to wiki");
                    }
                    Err(e) => {
                        eprintln!("[LAN Sync] Error looking for attachments folder: {}", e);
                    }
                }
            }
            Err(e) => {
                eprintln!("[LAN Sync] Failed to get parent URI for wiki {}: {}", wiki_path, e);
            }
        }
    }

    #[cfg(not(target_os = "android"))]
    {
        // On Desktop, use filesystem
        let wiki_file = std::path::Path::new(wiki_path);
        if let Some(parent) = wiki_file.parent() {
            let attachments_dir = parent.join("attachments");
            if attachments_dir.is_dir() {
                eprintln!("[LAN Sync] Found attachments folder: {}", attachments_dir.display());
                let mut file_list = Vec::new();
                collect_files_recursive(parent, &attachments_dir, &mut file_list);
                for (full_path, rel_path) in file_list {
                    result.push(AttachmentEntry {
                        rel_path,
                        source: full_path.to_string_lossy().to_string(),
                    });
                }
            }
        }
    }

    result
}

/// Recursively collect attachment file metadata from a SAF directory.
#[cfg(target_os = "android")]
fn collect_saf_attachment_entries(
    dir_uri: &str,
    prefix: &str,
    out: &mut Vec<AttachmentEntry>,
) {
    if let Ok(entries) = crate::android::saf::list_directory_entries(dir_uri) {
        for entry in entries {
            let rel_path = format!("{}/{}", prefix, entry.name);
            if entry.is_dir {
                collect_saf_attachment_entries(&entry.uri, &rel_path, out);
            } else {
                out.push(AttachmentEntry {
                    rel_path,
                    source: entry.uri,
                });
            }
        }
    }
}

/// Recursively collect attachment entries WITH file sizes from a SAF directory.
/// Returns (AttachmentEntry, size) tuples for use in Android periodic scanning.
#[cfg(target_os = "android")]
fn collect_saf_attachment_entries_with_size(
    dir_uri: &str,
    prefix: &str,
    out: &mut Vec<(AttachmentEntry, u64)>,
) {
    if let Ok(entries) = crate::android::saf::list_directory_entries(dir_uri) {
        for entry in entries {
            let rel_path = format!("{}/{}", prefix, entry.name);
            if entry.is_dir {
                collect_saf_attachment_entries_with_size(&entry.uri, &rel_path, out);
            } else {
                out.push((
                    AttachmentEntry {
                        rel_path,
                        source: entry.uri,
                    },
                    entry.size,
                ));
            }
        }
    }
}

/// Collect attachment entries with sizes for a wiki (Android only).
/// Returns Vec of (AttachmentEntry, size).
#[cfg(target_os = "android")]
fn collect_attachment_entries_with_size(wiki_path: &str) -> Vec<(AttachmentEntry, u64)> {
    let mut result = Vec::new();
    match crate::android::saf::get_parent_uri(wiki_path) {
        Ok(parent_uri) => {
            match crate::android::saf::find_subdirectory(&parent_uri, "attachments") {
                Ok(Some(attachments_uri)) => {
                    collect_saf_attachment_entries_with_size(
                        &attachments_uri,
                        "attachments",
                        &mut result,
                    );
                }
                Ok(None) => {}
                Err(e) => {
                    eprintln!(
                        "[LAN Sync] Error looking for attachments folder: {}",
                        e
                    );
                }
            }
        }
        Err(e) => {
            eprintln!(
                "[LAN Sync] Failed to get parent URI for wiki {}: {}",
                wiki_path, e
            );
        }
    }
    result
}

/// Validate that a relative path is safe and stays within a base directory.
/// Rejects path traversal (..), absolute paths, and symlink escapes.
/// Returns the validated joined path, or None if the path is unsafe.
fn validate_contained_path(
    base: &std::path::Path,
    rel_path: &str,
) -> Option<std::path::PathBuf> {
    // Normalize backslashes for cross-platform safety
    let normalized = rel_path.replace('\\', "/");
    let clean = normalized.strip_prefix("./").unwrap_or(&normalized);

    // Reject path traversal sequences
    for component in clean.split('/') {
        if component == ".." || component.is_empty() {
            eprintln!(
                "[LAN Sync] Security: Rejected path with traversal or empty component: {}",
                rel_path
            );
            return None;
        }
    }

    // Reject absolute paths
    if std::path::Path::new(clean).is_absolute() {
        eprintln!("[LAN Sync] Security: Rejected absolute path: {}", rel_path);
        return None;
    }

    let joined = base.join(clean);

    // Canonicalize and verify containment (catches symlink escapes)
    let canonical_base = dunce::canonicalize(base).unwrap_or_else(|_| base.to_path_buf());
    if let Ok(canonical) = dunce::canonicalize(&joined) {
        if !canonical.starts_with(&canonical_base) {
            eprintln!(
                "[LAN Sync] Security: Path escapes base dir: {} -> {}",
                rel_path,
                canonical.display()
            );
            return None;
        }
        Some(canonical)
    } else if let Some(parent) = joined.parent() {
        // File doesn't exist yet — canonicalize parent and check
        if let Ok(canonical_parent) = dunce::canonicalize(parent) {
            if !canonical_parent.starts_with(&canonical_base) {
                eprintln!(
                    "[LAN Sync] Security: Parent path escapes base dir: {} -> {}",
                    rel_path,
                    canonical_parent.display()
                );
                return None;
            }
        }
        Some(joined)
    } else {
        Some(joined)
    }
}

/// Recursively collect all files in a directory, returning (full_path, relative_path) pairs.
/// Skips symlinks that point outside the base directory to prevent data leakage.
fn collect_files_recursive(
    base: &std::path::Path,
    dir: &std::path::Path,
    out: &mut Vec<(std::path::PathBuf, String)>,
) {
    let canonical_base = dunce::canonicalize(base).unwrap_or_else(|_| base.to_path_buf());
    collect_files_recursive_inner(&canonical_base, dir, out);
}

fn collect_files_recursive_inner(
    canonical_base: &std::path::Path,
    dir: &std::path::Path,
    out: &mut Vec<(std::path::PathBuf, String)>,
) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();

            // Skip hidden files/directories (dotfiles: .git, .env, .DS_Store, etc.)
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with('.') {
                    continue;
                }
            }

            // Skip symlinks entirely — never follow or sync them
            if path.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false) {
                eprintln!(
                    "[LAN Sync] Security: Skipping symlink: {}",
                    path.display()
                );
                continue;
            }

            // Verify every path stays within the base
            if let Ok(canonical) = dunce::canonicalize(&path) {
                if !canonical.starts_with(canonical_base) {
                    eprintln!(
                        "[LAN Sync] Security: Skipping path escape: {} -> {}",
                        path.display(),
                        canonical.display()
                    );
                    continue;
                }
            }
            if path.is_dir() {
                collect_files_recursive_inner(canonical_base, &path, out);
            } else if path.is_file() {
                // Use canonical path for reliable strip_prefix
                let canonical = dunce::canonicalize(&path).unwrap_or_else(|_| path.clone());
                let rel = canonical
                    .strip_prefix(canonical_base)
                    .unwrap_or(&canonical)
                    .to_string_lossy()
                    .to_string();
                out.push((path, rel));
            }
        }
    }
}

/// Get the global sync manager
pub fn get_sync_manager() -> Option<Arc<SyncManager>> {
    SYNC_MANAGER.get().cloned()
}

/// Find which room a peer device is connected through (LAN or relay).
/// Used by lan_sync_link_wiki to auto-assign the wiki to the peer's room.
pub async fn find_peer_room(device_id: &str) -> Option<String> {
    let mgr = get_sync_manager()?;
    // Check LAN peer rooms
    if let Some(pc) = mgr.peers.read().await.get(device_id) {
        if let Some(room) = pc.auth_room_codes.first() {
            return Some(room.clone());
        }
    }
    // Check relay rooms
    if let Some(ref relay) = mgr.relay_manager {
        if let Some(room) = relay.find_device_room(device_id).await {
            return Some(room);
        }
    }
    None
}

/// Queue a sync-deactivate message to the Android bridge for a specific wiki.
/// Called from wiki_storage when sync is disabled for a wiki.
#[cfg(target_os = "android")]
pub fn queue_bridge_deactivate(sync_id: &str, wiki_path: &str) {
    if let Some(mgr) = get_sync_manager() {
        if let Ok(guard) = mgr.android_bridge.lock() {
            if let Some(ref bridge) = *guard {
                let payload = serde_json::json!({
                    "type": "sync-deactivate",
                    "wiki_path": wiki_path,
                });
                bridge.queue_change(sync_id, payload);
                eprintln!("[LAN Sync] Queued sync-deactivate to bridge for sync_id={}", sync_id);
            }
        }
    }
}

/// Get the collab WS port (for passing to child processes via env var)
#[cfg(not(target_os = "android"))]
pub fn get_collab_port() -> u16 {
    get_sync_manager()
        .map(|mgr| mgr.get_collab_ws_port())
        .unwrap_or(0)
}

/// Register the IPC client so wiki-process Tauri commands can route to the main process.
/// Called from lib.rs when running in wiki mode on desktop.
#[cfg(not(target_os = "android"))]
pub fn set_ipc_client_for_sync(client: Arc<std::sync::Mutex<Option<crate::ipc::IpcClient>>>) {
    let _ = IPC_CLIENT_FOR_SYNC.set(client);
}

// ── Tauri Commands ──────────────────────────────────────────────────────────

#[tauri::command]
pub async fn lan_sync_start(_app: tauri::AppHandle) -> Result<(), String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    mgr.start().await?;

    // Notify all open wiki windows that have sync enabled to start syncing.
    // This handles the case where wikis are already open when the user starts
    // the global LAN sync service.
    let entries = crate::wiki_storage::load_recent_files_from_disk(&_app);
    for entry in &entries {
        if entry.sync_enabled {
            if let Some(ref sync_id) = entry.sync_id {
                if !sync_id.is_empty() {
                    let _ = _app.emit("lan-sync-activate", serde_json::json!({
                        "wiki_path": entry.path,
                        "sync_id": sync_id,
                    }));
                    // Also send via IPC for wiki processes in separate OS processes
                    // (app.emit() only reaches webviews in the same process)
                    #[cfg(not(target_os = "android"))]
                    {
                        if let Some(server) = crate::GLOBAL_IPC_SERVER.get() {
                            let payload = serde_json::json!({
                                "type": "sync-activate",
                                "wiki_path": entry.path,
                                "sync_id": sync_id,
                            }).to_string();
                            server.send_lan_sync_to_all("*", &payload);
                        }
                    }
                    eprintln!("[LAN Sync] Global start: activating sync for wiki: {} (sync_id: {})", entry.path, sync_id);
                }
            }
        }
    }

    Ok(())
}

#[tauri::command]
pub async fn lan_sync_stop() -> Result<(), String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    mgr.stop().await;
    Ok(())
}

#[tauri::command]
pub async fn lan_sync_announce_username(user_name: String) -> Result<(), String> {
    eprintln!("[LAN Sync] lan_sync_announce_username called: '{}'", user_name);
    // Try sync manager first (main process)
    if let Some(mgr) = get_sync_manager() {
        eprintln!("[LAN Sync] announce_username: using sync manager directly");
        mgr.announce_username(user_name).await;
        return Ok(());
    }
    // Fall back to IPC (wiki process → main process)
    #[cfg(not(target_os = "android"))]
    {
        if let Some(ipc) = IPC_CLIENT_FOR_SYNC.get() {
            let mut guard = ipc.lock().unwrap();
            if let Some(ref mut client) = *guard {
                match client.send_lan_sync_announce_username(&user_name) {
                    Ok(()) => eprintln!("[LAN Sync] announce_username: sent via IPC"),
                    Err(e) => eprintln!("[LAN Sync] announce_username: IPC send failed: {}", e),
                }
            } else {
                eprintln!("[LAN Sync] announce_username: IPC client is None");
            }
        } else {
            eprintln!("[LAN Sync] announce_username: IPC_CLIENT_FOR_SYNC not set");
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn lan_sync_get_status() -> Result<SyncStatus, String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    Ok(mgr.get_status().await)
}

#[tauri::command]
pub async fn lan_sync_get_wiki_peers(wiki_id: String) -> Result<Vec<PeerInfo>, String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    Ok(mgr.get_wiki_peers(&wiki_id).await)
}

#[tauri::command]
pub fn lan_sync_tiddler_changed(
    wiki_id: String,
    title: String,
    tiddler_json: String,
) -> Result<(), String> {
    // Try sync manager first (main process)
    if let Some(mgr) = get_sync_manager() {
        mgr.notify_tiddler_changed(&wiki_id, &title, &tiddler_json);
        return Ok(());
    }
    // Fall back to IPC (wiki process → main process)
    #[cfg(not(target_os = "android"))]
    {
        if let Some(ipc) = IPC_CLIENT_FOR_SYNC.get() {
            let mut guard = ipc.lock().unwrap();
            if let Some(ref mut client) = *guard {
                match client.send_lan_sync_tiddler_changed(&wiki_id, &title, &tiddler_json) {
                    Ok(()) => {
                        eprintln!("[LAN Sync] IPC tiddler_changed sent: wiki={}, title={}", wiki_id, title);
                    }
                    Err(e) => {
                        eprintln!("[LAN Sync] IPC tiddler_changed failed: {} — IPC connection may be broken", e);
                    }
                }
            } else {
                eprintln!("[LAN Sync] IPC client is None in tiddler_changed");
            }
        } else {
            eprintln!("[LAN Sync] IPC_CLIENT_FOR_SYNC not set in tiddler_changed");
        }
    }
    Ok(())
}

#[tauri::command]
pub fn lan_sync_tiddler_deleted(wiki_id: String, title: String) -> Result<(), String> {
    if let Some(mgr) = get_sync_manager() {
        mgr.notify_tiddler_deleted(&wiki_id, &title);
        return Ok(());
    }
    #[cfg(not(target_os = "android"))]
    {
        if let Some(ipc) = IPC_CLIENT_FOR_SYNC.get() {
            let mut guard = ipc.lock().unwrap();
            if let Some(ref mut client) = *guard {
                let _ = client.send_lan_sync_tiddler_deleted(&wiki_id, &title);
            }
        }
    }
    Ok(())
}

/// Called by JS when a sync-enabled wiki window opens. Triggers catch-up sync
/// with all connected peers that have this wiki, so changes made while the
/// wiki was closed (or while the app was restarted) are applied.
#[tauri::command]
pub fn lan_sync_wiki_opened(wiki_id: String) -> Result<(), String> {
    eprintln!("[LAN Sync] lan_sync_wiki_opened called: {}", wiki_id);
    if let Some(mgr) = get_sync_manager() {
        eprintln!("[LAN Sync] Calling on_wiki_opened directly (main process)");
        let wiki_id_clone = wiki_id.clone();
        tokio::spawn(async move {
            mgr.on_wiki_opened(&wiki_id_clone).await;
        });
        return Ok(());
    }
    #[cfg(not(target_os = "android"))]
    {
        eprintln!("[LAN Sync] No sync manager, routing via IPC");
        if let Some(ipc) = IPC_CLIENT_FOR_SYNC.get() {
            let mut guard = ipc.lock().unwrap();
            if let Some(ref mut client) = *guard {
                eprintln!("[LAN Sync] Sending wiki_opened via IPC");
                let _ = client.send_lan_sync_wiki_opened(&wiki_id);
            } else {
                eprintln!("[LAN Sync] IPC client is None");
            }
        } else {
            eprintln!("[LAN Sync] IPC_CLIENT_FOR_SYNC not set");
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn lan_sync_get_available_wikis() -> Result<Vec<RemoteWikiInfo>, String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    Ok(mgr.get_available_remote_wikis().await)
}

#[tauri::command]
pub async fn lan_sync_request_wiki(
    wiki_id: String,
    from_device_id: String,
    target_dir: String,
    room_code: Option<String>,
) -> Result<(), String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    mgr.request_wiki_from_peer(&wiki_id, &from_device_id, &target_dir, room_code).await
}


/// Called by JS with tiddler fingerprints for diff-based sync.
/// Fingerprints are (title, modified) pairs for non-shadow tiddlers.
/// The peer compares and sends only tiddlers that differ.
#[tauri::command]
pub async fn lan_sync_send_fingerprints(
    wiki_id: String,
    to_device_id: String,
    fingerprints: Vec<protocol::TiddlerFingerprint>,
) -> Result<(), String> {
    if let Some(mgr) = get_sync_manager() {
        return mgr.send_tiddler_fingerprints(&wiki_id, &to_device_id, fingerprints).await;
    }
    // Fall back to IPC (wiki process → main process)
    #[cfg(not(target_os = "android"))]
    {
        if let Some(ipc) = IPC_CLIENT_FOR_SYNC.get() {
            let fingerprints_json = serde_json::to_string(&fingerprints).unwrap_or_default();
            let mut guard = ipc.lock().unwrap();
            if let Some(ref mut client) = *guard {
                if let Err(e) = client.send_lan_sync_fingerprints(&wiki_id, &to_device_id, &fingerprints_json) {
                    eprintln!("[LAN Sync] IPC send_fingerprints failed: {} — IPC connection may be broken", e);
                }
            } else {
                eprintln!("[LAN Sync] IPC client is None in send_fingerprints");
            }
        } else {
            eprintln!("[LAN Sync] IPC_CLIENT_FOR_SYNC not set in send_fingerprints");
        }
    }
    Ok(())
}

/// Called by JS with a batch of tiddlers for full sync.
/// JS gathers all tiddlers from a wiki window and sends them in batches.
/// Rust attaches vector clocks and forwards them to the specified peer.
#[tauri::command]
pub async fn lan_sync_send_full_sync(
    wiki_id: String,
    to_device_id: String,
    tiddlers: Vec<TiddlerBatch>,
    is_last_batch: bool,
) -> Result<(), String> {
    if let Some(mgr) = get_sync_manager() {
        return mgr.send_full_sync_batch(&wiki_id, &to_device_id, tiddlers, is_last_batch)
            .await;
    }
    // Fall back to IPC (wiki process → main process)
    #[cfg(not(target_os = "android"))]
    {
        eprintln!("[LAN Sync] lan_sync_send_full_sync: wiki_id={}, to={}, tiddlers={}, is_last={} (via IPC)", wiki_id, to_device_id, tiddlers.len(), is_last_batch);
        if let Some(ipc) = IPC_CLIENT_FOR_SYNC.get() {
            let tiddlers_json = serde_json::to_string(&tiddlers).unwrap_or_default();
            let mut guard = ipc.lock().unwrap();
            if let Some(ref mut client) = *guard {
                let _ = client.send_lan_sync_full_batch(&wiki_id, &to_device_id, &tiddlers_json, is_last_batch);
            } else {
                eprintln!("[LAN Sync] IPC client is None!");
            }
        } else {
            eprintln!("[LAN Sync] IPC_CLIENT_FOR_SYNC not initialized!");
        }
    }
    Ok(())
}

/// Broadcast our tiddler fingerprints to ALL connected peers sharing this wiki.
/// Called proactively by JS when sync activates — no event round-trip needed.
/// Each peer compares and sends back only tiddlers that differ.
#[tauri::command]
pub async fn lan_sync_broadcast_fingerprints(
    wiki_id: String,
    fingerprints: Vec<protocol::TiddlerFingerprint>,
) -> Result<(), String> {
    if let Some(mgr) = get_sync_manager() {
        return mgr.broadcast_tiddler_fingerprints(&wiki_id, fingerprints).await;
    }
    // Fall back to IPC (wiki process → main process)
    #[cfg(not(target_os = "android"))]
    {
        let fp_count = fingerprints.len();
        if let Some(ipc) = IPC_CLIENT_FOR_SYNC.get() {
            let fingerprints_json = serde_json::to_string(&fingerprints).unwrap_or_default();
            let mut guard = ipc.lock().unwrap();
            if let Some(ref mut client) = *guard {
                if let Err(e) = client.send_lan_sync_broadcast_fingerprints(&wiki_id, &fingerprints_json) {
                    eprintln!("[LAN Sync] IPC broadcast_fingerprints failed: {} — IPC connection may be broken", e);
                }
            } else {
                eprintln!("[LAN Sync] IPC client is None in broadcast_fingerprints");
            }
        } else {
            eprintln!("[LAN Sync] IPC_CLIENT_FOR_SYNC not set in broadcast_fingerprints");
        }
    }
    Ok(())
}

/// Broadcast updated WikiManifest to all connected peers.
/// Called when wiki sync is toggled or wiki list changes.
#[tauri::command]
pub async fn lan_sync_broadcast_manifest() -> Result<(), String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    mgr.broadcast_wiki_manifest().await;
    Ok(())
}

/// Poll for pending LAN sync messages from IPC (desktop wiki processes only).
/// Returns JSON strings that JS should parse and handle.
/// `wiki_id`: filters messages for this wiki. Messages with a different
/// `wiki_id` field in their JSON payload are re-queued so other wiki
/// windows can pick them up. Pass empty string to get only messages
/// without a `wiki_id` field (e.g. sync-activate/deactivate).
#[cfg(not(target_os = "android"))]
#[tauri::command]
pub fn lan_sync_poll_ipc(wiki_id: String) -> Vec<String> {
    let queue = IPC_SYNC_QUEUE.get_or_init(|| std::sync::Mutex::new(Vec::new()));
    let mut guard = queue.lock().unwrap();
    let all: Vec<String> = guard.drain(..).collect();
    if all.is_empty() {
        return vec![];
    }
    let mut matching = Vec::new();
    let mut requeue = Vec::new();
    for msg in all {
        // Parse just enough to check the wiki_id field
        let msg_wiki_id = serde_json::from_str::<serde_json::Value>(&msg)
            .ok()
            .and_then(|v| v.get("wiki_id").and_then(|w| w.as_str()).map(String::from));
        let keep = match (&msg_wiki_id, wiki_id.is_empty()) {
            // Message has no wiki_id → deliver to everyone (global message)
            (None, _) => true,
            // Caller wants global only (empty wiki_id) but message has one → re-queue
            (Some(_), true) => false,
            // Message wiki_id matches caller → deliver
            (Some(mid), false) if mid == &wiki_id => true,
            // Message wiki_id doesn't match → re-queue for the right wiki
            (Some(_), false) => false,
        };
        if keep {
            matching.push(msg);
        } else {
            requeue.push(msg);
        }
    }
    // Put non-matching messages back
    if !requeue.is_empty() {
        guard.extend(requeue);
    }
    if !matching.is_empty() {
        eprintln!("[LAN Sync] lan_sync_poll_ipc({}): JS drained {} messages", wiki_id, matching.len());
    }
    matching
}

/// No-op on Android (JS uses bridge polling instead).
#[cfg(target_os = "android")]
#[tauri::command]
pub fn lan_sync_poll_ipc() -> Vec<String> {
    Vec::new()
}

/// Load persisted deletion tombstones for a wiki (by sync_id).
/// Returns JSON string (empty object `{}` if none stored).
#[tauri::command]
pub async fn lan_sync_load_tombstones(
    app: tauri::AppHandle,
    wiki_id: String,
) -> Result<String, String> {
    let data_dir = crate::get_data_dir(&app)?;
    let tombstone_dir = data_dir.join("lan_sync_tombstones");
    let safe_name = wiki_id.replace(|c: char| !c.is_alphanumeric() && c != '-' && c != '_', "_");
    let file_path = tombstone_dir.join(format!("{}.json", safe_name));
    match tokio::fs::read_to_string(&file_path).await {
        Ok(content) => Ok(content),
        Err(_) => Ok("{}".to_string()),
    }
}

/// Save deletion tombstones for a wiki (by sync_id).
#[tauri::command]
pub async fn lan_sync_save_tombstones(
    app: tauri::AppHandle,
    wiki_id: String,
    tombstones_json: String,
) -> Result<(), String> {
    let data_dir = crate::get_data_dir(&app)?;
    let tombstone_dir = data_dir.join("lan_sync_tombstones");
    tokio::fs::create_dir_all(&tombstone_dir).await.map_err(|e| e.to_string())?;
    let safe_name = wiki_id.replace(|c: char| !c.is_alphanumeric() && c != '-' && c != '_', "_");
    let file_path = tombstone_dir.join(format!("{}.json", safe_name));
    tokio::fs::write(&file_path, tombstones_json).await.map_err(|e| e.to_string())?;
    Ok(())
}

// ── Collaborative editing Tauri commands ─────────────────────────────────

#[tauri::command]
pub fn lan_sync_collab_editing_started(wiki_id: String, tiddler_title: String) -> Result<(), String> {
    eprintln!("[Collab CMD] lan_sync_collab_editing_started: wiki={}, tiddler={}", wiki_id, tiddler_title);
    if let Some(mgr) = get_sync_manager() {
        mgr.notify_collab_editing_started(&wiki_id, &tiddler_title);
        return Ok(());
    }
    #[cfg(not(target_os = "android"))]
    {
        eprintln!("[Collab CMD] No sync manager, trying IPC client");
        if let Some(ipc) = IPC_CLIENT_FOR_SYNC.get() {
            let mut guard = ipc.lock().unwrap();
            if let Some(ref mut client) = *guard {
                eprintln!("[Collab CMD] Sending via IPC client");
                let _ = client.send_lan_sync_collab_editing_started(&wiki_id, &tiddler_title);
            } else {
                eprintln!("[Collab CMD] IPC client is None");
            }
        } else {
            eprintln!("[Collab CMD] IPC_CLIENT_FOR_SYNC not initialized");
        }
    }
    Ok(())
}

#[tauri::command]
pub fn lan_sync_collab_editing_stopped(wiki_id: String, tiddler_title: String) -> Result<(), String> {
    eprintln!("[Collab CMD] lan_sync_collab_editing_stopped: wiki={}, tiddler={}", wiki_id, tiddler_title);
    if let Some(mgr) = get_sync_manager() {
        mgr.notify_collab_editing_stopped(&wiki_id, &tiddler_title);
        return Ok(());
    }
    #[cfg(not(target_os = "android"))]
    {
        if let Some(ipc) = IPC_CLIENT_FOR_SYNC.get() {
            let mut guard = ipc.lock().unwrap();
            if let Some(ref mut client) = *guard {
                let _ = client.send_lan_sync_collab_editing_stopped(&wiki_id, &tiddler_title);
            }
        }
    }
    Ok(())
}

#[tauri::command]
pub fn lan_sync_collab_peer_saved(
    wiki_id: String,
    tiddler_title: String,
    saved_title: String,
) -> Result<(), String> {
    eprintln!("[Collab CMD] lan_sync_collab_peer_saved: wiki={}, tiddler={}, saved_as={}", wiki_id, tiddler_title, saved_title);
    if let Some(mgr) = get_sync_manager() {
        mgr.notify_collab_peer_saved(&wiki_id, &tiddler_title, &saved_title);
        return Ok(());
    }
    #[cfg(not(target_os = "android"))]
    {
        if let Some(ipc) = IPC_CLIENT_FOR_SYNC.get() {
            let mut guard = ipc.lock().unwrap();
            if let Some(ref mut client) = *guard {
                let _ = client.send_lan_sync_collab_peer_saved(&wiki_id, &tiddler_title, &saved_title);
            }
        }
    }
    Ok(())
}

#[tauri::command]
pub fn lan_sync_collab_update(
    wiki_id: String,
    tiddler_title: String,
    update_base64: String,
) -> Result<(), String> {
    eprintln!("[Collab CMD] lan_sync_collab_update: wiki={}, tiddler={}, len={}", wiki_id, tiddler_title, update_base64.len());
    if let Some(mgr) = get_sync_manager() {
        mgr.send_collab_update(&wiki_id, &tiddler_title, &update_base64);
        return Ok(());
    }
    #[cfg(not(target_os = "android"))]
    {
        eprintln!("[Collab CMD] No sync manager, trying IPC client for update");
        if let Some(ipc) = IPC_CLIENT_FOR_SYNC.get() {
            let mut guard = ipc.lock().unwrap();
            if let Some(ref mut client) = *guard {
                let _ = client.send_lan_sync_collab_update(&wiki_id, &tiddler_title, &update_base64);
            }
        }
    }
    Ok(())
}

#[tauri::command]
pub fn lan_sync_collab_awareness(
    wiki_id: String,
    tiddler_title: String,
    update_base64: String,
) -> Result<(), String> {
    eprintln!("[Collab CMD] lan_sync_collab_awareness: wiki={}, tiddler={}, len={}", wiki_id, tiddler_title, update_base64.len());
    if let Some(mgr) = get_sync_manager() {
        mgr.send_collab_awareness(&wiki_id, &tiddler_title, &update_base64);
        return Ok(());
    }
    #[cfg(not(target_os = "android"))]
    {
        if let Some(ipc) = IPC_CLIENT_FOR_SYNC.get() {
            let mut guard = ipc.lock().unwrap();
            if let Some(ref mut client) = *guard {
                let _ = client.send_lan_sync_collab_awareness(&wiki_id, &tiddler_title, &update_base64);
            }
        }
    }
    Ok(())
}

#[tauri::command]
pub fn lan_sync_get_remote_editors(
    wiki_id: String,
    tiddler_title: String,
) -> Result<Vec<serde_json::Value>, String> {
    if let Some(mgr) = get_sync_manager() {
        let editors = mgr.get_remote_editors(&wiki_id, &tiddler_title);
        let result: Vec<serde_json::Value> = editors
            .into_iter()
            .map(|(device_id, device_name, user_name)| {
                serde_json::json!({"deviceId": device_id, "deviceName": device_name, "userName": user_name})
            })
            .collect();
        return Ok(result);
    }
    Ok(Vec::new())
}

#[tauri::command]
pub fn lan_sync_get_collab_port() -> Result<u16, String> {
    if let Some(mgr) = get_sync_manager() {
        return Ok(mgr.get_collab_ws_port());
    }
    // Fallback for wiki child processes: read port from env var set by main process
    if let Ok(val) = std::env::var("COLLAB_WS_PORT") {
        if let Ok(port) = val.parse::<u16>() {
            return Ok(port);
        }
    }
    Ok(0)
}

// ── Relay sync Tauri commands ──────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RelaySyncStatus {
    pub relay_url: String,
    pub rooms: Vec<crate::relay_sync::RoomStatus>,
    // New generic auth fields
    pub username: Option<String>,
    pub user_id: Option<String>,
    pub auth_provider: Option<String>,
    pub authenticated: bool,
    // Legacy fields for backward compat with old landing pages
    pub github_login: Option<String>,
    pub github_id: Option<i64>,
    pub github_authenticated: bool,
}

#[tauri::command]
pub async fn relay_sync_get_status() -> Result<RelaySyncStatus, String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    if let Some(relay) = &mgr.relay_manager {
        let config = relay.get_config().await;
        let mut rooms = relay.get_rooms().await;
        let authenticated = !config.auth_token.is_empty();
        let local_device_id = mgr.pairing_manager.device_id();
        // For relay-disconnected rooms, clear relay peers (none expected anyway).
        // For relay-connected rooms, remove local device from relay peer list.
        for room in &mut rooms {
            if !room.connected {
                room.connected_peers.clear();
            } else {
                room.connected_peers.retain(|p| p.device_id != local_device_id);
            }
        }
        // Always merge LAN peers — rooms can be active via LAN even without relay.
        if let Some(ref server) = *mgr.server.read().await {
            for room in &mut rooms {
                let lan_peers = server.lan_peers_for_room(&room.room_code).await;
                for (id, name) in lan_peers {
                    if id != local_device_id && !room.connected_peers.iter().any(|p| p.device_id == id) {
                        room.connected_peers.push(crate::relay_sync::RoomPeerInfo {
                            device_id: id,
                            device_name: name,
                        });
                    }
                }
            }
        }
        // Extract legacy github_id from user_id if it's a github: prefixed ID
        let legacy_github_id = config.user_id.as_deref()
            .and_then(|uid| uid.strip_prefix("github:"))
            .and_then(|id| id.parse::<i64>().ok());
        Ok(RelaySyncStatus {
            relay_url: config.relay_url,
            rooms,
            username: config.username.clone(),
            user_id: config.user_id.clone(),
            auth_provider: config.auth_provider.clone(),
            authenticated,
            // Legacy fields
            github_login: config.username.clone(),
            github_id: legacy_github_id,
            github_authenticated: authenticated,
        })
    } else {
        Ok(RelaySyncStatus {
            relay_url: String::new(),
            rooms: vec![],
            username: None,
            user_id: None,
            auth_provider: None,
            authenticated: false,
            github_login: None,
            github_id: None,
            github_authenticated: false,
        })
    }
}

#[tauri::command]
pub async fn relay_sync_add_room(
    name: String,
    room_code: String,
    password: String,
    auto_connect: bool,
) -> Result<(), String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    if let Some(relay) = &mgr.relay_manager {
        let result = relay.add_room(name, room_code, password, auto_connect).await;
        // Refresh LAN room keys and discovery beacons
        mgr.update_room_keys().await;
        mgr.update_active_room_codes().await;
        // Auto-start LAN server for LAN-only discovery
        if result.is_ok() && mgr.server.read().await.is_none() {
            if let Err(e) = mgr.start().await {
                eprintln!("[LAN Sync] Auto-start LAN server failed: {}", e);
            }
            #[cfg(target_os = "android")]
            start_sync_foreground_service();
        }
        result
    } else {
        Err("Relay sync not available".to_string())
    }
}

#[tauri::command]
pub async fn relay_sync_remove_room(room_code: String) -> Result<(), String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    if let Some(relay) = &mgr.relay_manager {
        let result = relay.remove_room(&room_code).await;
        // Clear relay_room from any wikis still assigned to this room
        if let Some(app) = crate::get_global_app_handle() {
            crate::wiki_storage::clear_relay_room_for_code(&app, &room_code);
        }
        // Refresh LAN room keys and discovery beacons
        mgr.update_room_keys().await;
        mgr.update_active_room_codes().await;
        // Stop LAN server if no rooms remain
        if !relay.has_any_rooms().await {
            mgr.stop().await;
            #[cfg(target_os = "android")]
            stop_sync_foreground_service();
        }
        result
    } else {
        Err("Relay sync not available".to_string())
    }
}

#[tauri::command]
pub async fn relay_sync_connect_room(room_code: String) -> Result<(), String> {
    eprintln!("[LAN Sync] relay_sync_connect_room: {}", room_code);
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    if let Some(relay) = &mgr.relay_manager {
        // Try relay connection — log but don't fail if relay is unavailable
        // (LAN-only sync should still work)
        if let Err(e) = relay.connect_room(&room_code).await {
            eprintln!("[LAN Sync] Relay connect failed (LAN still works): {}", e);
        } else {
            eprintln!("[LAN Sync] relay.connect_room spawned task for {}", room_code);
        }
        // Mark room as manually activated so UI shows it as connected
        relay.activate_room(&room_code).await;
        relay.set_room_auto_connect(&room_code, true).await;
        // Refresh LAN room keys and discovery beacons
        mgr.update_room_keys().await;
        mgr.update_active_room_codes().await;
        // Always start LAN server + discovery if not running — enables
        // LAN-only sync even when relay connection fails (no internet)
        if mgr.server.read().await.is_none() {
            if let Err(e) = mgr.start().await {
                eprintln!("[LAN Sync] Auto-start LAN server failed: {}", e);
            }
        }
        // Trigger immediate beacon so peers discover us without waiting 2s
        mgr.force_discovery_beacon().await;
        // Re-check all connected peers against our updated room list.
        // The forced beacon tells the OTHER device about our new rooms, but
        // we also need to update our own peer_room_codes for already-connected
        // peers whose room hashes already included this room.
        if let Some(ref server) = *mgr.server.read().await {
            let our_rooms = mgr.active_room_codes.read()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let all_peer_hashes = mgr.peer_room_hashes.read().await;
            let mut any_updated = false;
            for (peer_id, hashes) in all_peer_hashes.iter() {
                let shared = protocol::select_all_shared_rooms_by_hash(&our_rooms, hashes);
                let old_codes = server.peer_room_codes(peer_id).await;
                let mut new_rooms = false;
                for r in &shared {
                    if !old_codes.contains(r) {
                        new_rooms = true;
                        break;
                    }
                }
                if new_rooms {
                    server.add_peer_room_codes(peer_id, &shared).await;
                    eprintln!("[LAN Sync] Added room codes for already-connected peer {}: {:?}",
                        &peer_id[..8.min(peer_id.len())], shared);
                    mgr.send_wiki_manifest_to_peer(peer_id).await;
                    any_updated = true;
                }
            }
            drop(all_peer_hashes);
            if any_updated {
                if let Some(app) = GLOBAL_APP_HANDLE.get() {
                    let _ = app.emit("lan-sync-peers-updated", serde_json::json!({}));
                }
                #[cfg(not(target_os = "android"))]
                mgr.push_peer_updates_to_ipc().await;
            }
        }
        // Start foreground service to keep process alive (Android only)
        #[cfg(target_os = "android")]
        start_sync_foreground_service();
        Ok(())
    } else {
        Err("Relay sync not available".to_string())
    }
}

#[tauri::command]
pub async fn relay_sync_disconnect_room(room_code: String) -> Result<(), String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    if let Some(relay) = &mgr.relay_manager {
        // Send RoomLeave to all LAN peers in this room BEFORE disconnecting,
        // so they can update their UI immediately instead of waiting for ping timeout.
        if let Some(ref server) = *mgr.server.read().await {
            let room_peers = server.lan_peers_for_room(&room_code).await;
            if !room_peers.is_empty() {
                let leave_msg = SyncMessage::RoomLeave {
                    room_code: room_code.clone(),
                };
                let peer_ids: Vec<String> = room_peers.into_iter().map(|(id, _)| id).collect();
                eprintln!("[LAN Sync] Sending RoomLeave({}) to {} peers", room_code, peer_ids.len());
                server.send_to_peers(&peer_ids, &leave_msg).await;
                // Brief delay to let the message flush before we tear down
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
        relay.disconnect_room(&room_code).await;
        relay.deactivate_room(&room_code).await;
        relay.set_room_auto_connect(&room_code, false).await;
        // Remove room from LAN peers and disconnect those with no remaining rooms
        if let Some(ref server) = *mgr.server.read().await {
            let disconnected = server.remove_room_from_peers(&room_code).await;
            for did in &disconnected {
                if let Ok(mut set) = mgr.connected_peer_ids.write() {
                    set.remove(did);
                }
                mgr.remote_wikis.write().await.remove(did);
            }
            if let Some(app) = GLOBAL_APP_HANDLE.get() {
                for did in &disconnected {
                    let _ = app.emit("lan-sync-peer-disconnected", serde_json::json!({
                        "device_id": did,
                    }));
                }
                if !disconnected.is_empty() {
                    let _ = app.emit("lan-sync-peers-updated", serde_json::json!({}));
                }
            }
        }
        // Refresh discovery beacons and trigger immediate beacon
        mgr.update_active_room_codes().await;
        mgr.update_room_keys().await;
        mgr.force_discovery_beacon().await;
        // Only stop LAN server if no rooms remain configured at all
        // (keep running for LAN-only discovery of other rooms)
        if !relay.has_any_rooms().await {
            mgr.stop().await;
            #[cfg(target_os = "android")]
            stop_sync_foreground_service();
        }
        Ok(())
    } else {
        Err("Relay sync not available".to_string())
    }
}

#[tauri::command]
pub async fn relay_sync_set_room_auto_connect(
    room_code: String,
    enabled: bool,
) -> Result<(), String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    if let Some(relay) = &mgr.relay_manager {
        relay.set_room_auto_connect(&room_code, enabled).await;
        mgr.update_active_room_codes().await;
        Ok(())
    } else {
        Err("Relay sync not available".to_string())
    }
}

#[tauri::command]
pub async fn relay_sync_set_room_password(
    room_code: String,
    password: String,
) -> Result<(), String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    if let Some(relay) = &mgr.relay_manager {
        let result = relay.set_room_password(&room_code, password).await;
        // Refresh LAN room keys since password changed
        mgr.update_room_keys().await;
        result
    } else {
        Err("Relay sync not available".to_string())
    }
}

#[tauri::command]
pub async fn relay_sync_set_room_name(
    room_code: String,
    name: String,
) -> Result<(), String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    if let Some(relay) = &mgr.relay_manager {
        relay.set_room_name(&room_code, name).await
    } else {
        Err("Relay sync not available".to_string())
    }
}

#[tauri::command]
pub async fn relay_sync_get_room_credentials(
    room_code: String,
) -> Result<serde_json::Value, String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    if let Some(relay) = &mgr.relay_manager {
        match relay.get_room_credentials(&room_code).await {
            Some((name, code, password)) => Ok(serde_json::json!({
                "name": name,
                "room_code": code,
                "password": password,
            })),
            None => Err(format!("Room '{}' not found", room_code)),
        }
    } else {
        Err("Relay sync not available".to_string())
    }
}

#[tauri::command]
pub async fn relay_sync_set_url(url: String) -> Result<(), String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    if let Some(relay) = &mgr.relay_manager {
        relay.set_relay_url(url).await;
        Ok(())
    } else {
        Err("Relay sync not available".to_string())
    }
}

#[tauri::command]
pub async fn relay_sync_generate_credentials() -> Result<serde_json::Value, String> {
    Ok(serde_json::json!({
        "room_code": crate::relay_sync::generate_room_code(),
        "password": crate::relay_sync::generate_room_password(),
    }))
}

// ── Authentication Commands ─────────────────────────────────────────

#[tauri::command]
pub async fn relay_sync_login(
    provider: String,
    client_id: String,
    auth_url: Option<String>,
    discovery_url: Option<String>,
    scope: Option<String>,
) -> Result<serde_json::Value, String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    if let Some(relay) = &mgr.relay_manager {
        let result = relay.login(
            &provider,
            &client_id,
            auth_url.as_deref(),
            discovery_url.as_deref(),
            scope.as_deref(),
        ).await?;
        Ok(serde_json::json!({
            "username": result.username,
            "user_id": result.user_id,
            "provider": result.provider,
            // Legacy fields
            "github_login": result.username,
        }))
    } else {
        Err("Relay sync not available".to_string())
    }
}

#[tauri::command]
pub async fn relay_sync_logout() -> Result<(), String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    if let Some(relay) = &mgr.relay_manager {
        // Disconnect all rooms first (tokens will be invalid after logout)
        relay.stop_all().await;
        relay.logout().await;
        Ok(())
    } else {
        Err("Relay sync not available".to_string())
    }
}

#[tauri::command]
pub async fn relay_sync_auth_status() -> Result<serde_json::Value, String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    if let Some(relay) = &mgr.relay_manager {
        let (username, user_id, provider, has_token) = relay.auth_status().await;
        Ok(serde_json::json!({
            "username": username,
            "user_id": user_id,
            "auth_provider": provider,
            "authenticated": has_token,
            // Legacy
            "github_login": username,
        }))
    } else {
        Ok(serde_json::json!({
            "username": null,
            "user_id": null,
            "auth_provider": null,
            "authenticated": false,
            "github_login": null,
        }))
    }
}

#[tauri::command]
pub async fn relay_sync_fetch_providers() -> Result<serde_json::Value, String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    if let Some(relay) = &mgr.relay_manager {
        let providers = relay.fetch_providers().await?;
        Ok(serde_json::to_value(providers).map_err(|e| format!("Serialize error: {}", e))?)
    } else {
        Err("Relay sync not available".to_string())
    }
}

// Legacy aliases — kept for backward compat with old landing pages
#[tauri::command]
pub async fn relay_sync_github_login() -> Result<serde_json::Value, String> {
    // Old-style GitHub-only login: fetch providers, find GitHub, and use it
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    if let Some(relay) = &mgr.relay_manager {
        let providers = relay.fetch_providers().await.unwrap_or_default();
        let github = providers.iter().find(|p| p.name == "github");
        let client_id = github.map(|p| p.client_id.as_str()).unwrap_or("");
        let result = relay.login("github", client_id, None, None, None).await?;
        Ok(serde_json::json!({
            "github_login": result.username,
            "github_id": result.user_id,
        }))
    } else {
        Err("Relay sync not available".to_string())
    }
}

#[tauri::command]
pub async fn relay_sync_github_logout() -> Result<(), String> {
    relay_sync_logout().await
}

#[tauri::command]
pub async fn relay_sync_github_status() -> Result<serde_json::Value, String> {
    relay_sync_auth_status().await
}

// ── Server-Side Room Management Commands ────────────────────────────

#[tauri::command]
pub async fn relay_sync_create_room(name: String, room_code: String) -> Result<serde_json::Value, String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    if let Some(relay) = &mgr.relay_manager {
        // Server receives hashed room code and no name (privacy)
        let _returned_code = relay.create_server_room(&room_code).await?;
        // Return local name — server doesn't store it
        Ok(serde_json::json!({
            "room_code": room_code,
            "name": name,
        }))
    } else {
        Err("Relay sync not available".to_string())
    }
}

#[tauri::command]
pub async fn relay_sync_delete_server_room(room_code: String) -> Result<(), String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    if let Some(relay) = &mgr.relay_manager {
        relay.delete_server_room(&room_code).await
    } else {
        Err("Relay sync not available".to_string())
    }
}

#[tauri::command]
pub async fn relay_sync_delete_server_room_by_hash(room_hash: String) -> Result<(), String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    if let Some(relay) = &mgr.relay_manager {
        relay.delete_server_room_by_hash(&room_hash).await
    } else {
        Err("Relay sync not available".to_string())
    }
}

#[tauri::command]
pub async fn relay_sync_add_member(
    room_code: String,
    username: Option<String>,
    provider: Option<String>,
    github_login: Option<String>,  // legacy param
    user_id: Option<String>,       // direct user_id for unblocking
) -> Result<(), String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    if let Some(relay) = &mgr.relay_manager {
        // Support both new (username) and legacy (github_login) params
        let name = username.or(github_login).ok_or("username is required")?;
        relay.add_room_member(&room_code, &name, provider.as_deref(), user_id.as_deref()).await
    } else {
        Err("Relay sync not available".to_string())
    }
}

#[tauri::command]
pub async fn relay_sync_remove_member(
    room_code: String,
    user_id: Option<String>,
    github_login: Option<String>,  // legacy param
) -> Result<(), String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    if let Some(relay) = &mgr.relay_manager {
        // Support both new (user_id) and legacy (github_login) params
        let id = user_id.or(github_login).ok_or("user_id is required")?;
        relay.remove_room_member(&room_code, &id).await
    } else {
        Err("Relay sync not available".to_string())
    }
}

#[tauri::command]
pub async fn relay_sync_list_members(room_code: String) -> Result<serde_json::Value, String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    if let Some(relay) = &mgr.relay_manager {
        let members = relay.list_room_members(&room_code).await?;
        Ok(serde_json::json!(members))
    } else {
        Err("Relay sync not available".to_string())
    }
}

#[tauri::command]
pub async fn relay_sync_list_server_rooms() -> Result<serde_json::Value, String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    if let Some(relay) = &mgr.relay_manager {
        let server_rooms = relay.list_server_rooms().await?;
        let local_rooms = relay.get_rooms().await;

        // Build hash → local room mapping
        let mut hash_to_local: HashMap<String, (String, String)> = HashMap::new();
        for room in &local_rooms {
            let hash = discovery::hash_room_code(&room.room_code);
            hash_to_local.insert(hash, (room.room_code.clone(), room.name.clone()));
        }

        // Annotate each server room with local match info and decrypted credentials
        let annotated: Vec<serde_json::Value> = server_rooms.iter().map(|sr| {
            let room_hash = sr["room_code"].as_str().unwrap_or("");
            let local = hash_to_local.get(room_hash);

            // Try to decrypt credentials for owned rooms (only works on same device)
            let decrypted = sr["encrypted_credentials"].as_str()
                .and_then(|enc| relay.decrypt_credentials(enc));

            serde_json::json!({
                "room_hash": room_hash,
                "role": sr["role"],
                "member_count": sr["member_count"],
                "owner_username": sr["owner_username"],
                "owner_provider": sr["owner_provider"],
                "local_room_code": local.map(|(code, _)| code.as_str()),
                "local_room_name": local.map(|(_, name)| name.as_str()),
                "decrypted_room_code": decrypted.as_ref().map(|(c, _)| c.as_str()),
                "decrypted_password": decrypted.as_ref().map(|(_, p)| p.as_str()),
            })
        }).collect();

        Ok(serde_json::json!(annotated))
    } else {
        Err("Relay sync not available".to_string())
    }
}

#[tauri::command]
pub async fn lan_sync_set_display_name(name: String) -> Result<(), String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    let clean = name.trim().to_string();
    let val = if clean.is_empty() { None } else { Some(clean) };
    mgr.pairing_manager.set_display_name(val);
    Ok(())
}

#[tauri::command]
pub async fn lan_sync_get_display_name_setting() -> Result<Option<String>, String> {
    let mgr = get_sync_manager().ok_or("Sync not initialized")?;
    Ok(mgr.pairing_manager.get_display_name_setting())
}

/// A tiddler in a full sync batch from JS
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TiddlerBatch {
    pub title: String,
    pub tiddler_json: String,
}

// ── Android foreground service for LAN sync ─────────────────────────────

/// Start the LanSyncService foreground service via JNI (Android only).
/// Uses Intent.setClassName() to avoid FindClass on app classes from native threads
/// (FindClass uses the system class loader on native threads, which can't find app classes).
#[cfg(target_os = "android")]
fn start_sync_foreground_service() {
    use crate::android::wiki_activity::get_java_vm;
    use jni::objects::{JValue, JObject};

    let vm = match get_java_vm() {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("[LAN Sync] Can't get JavaVM for foreground service: {}", e);
            return;
        }
    };

    let mut env = match vm.attach_current_thread() {
        Ok(env) => env,
        Err(e) => {
            eprintln!("[LAN Sync] Can't attach thread for foreground service: {}", e);
            return;
        }
    };

    // Get application context via ActivityThread.currentApplication()
    let context = match env.call_static_method(
        "android/app/ActivityThread",
        "currentApplication",
        "()Landroid/app/Application;",
        &[],
    ) {
        Ok(val) => match val.l() {
            Ok(obj) if !obj.is_null() => obj,
            _ => {
                eprintln!("[LAN Sync] Can't get app context for foreground service");
                return;
            }
        },
        Err(e) => {
            eprintln!("[LAN Sync] Failed to get app context: {}", e);
            return;
        }
    };

    // Create Intent and set component by class name string (avoids FindClass)
    let intent = match env.new_object("android/content/Intent", "()V", &[]) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[LAN Sync] Failed to create Intent: {}", e);
            return;
        }
    };

    let pkg = match env.call_method(&context, "getPackageName", "()Ljava/lang/String;", &[]) {
        Ok(val) => match val.l() {
            Ok(obj) => obj,
            Err(e) => {
                eprintln!("[LAN Sync] Failed to get package name: {}", e);
                return;
            }
        },
        Err(e) => {
            eprintln!("[LAN Sync] Failed to call getPackageName: {}", e);
            return;
        }
    };

    let cls_name = match env.new_string("com.burningtreec.tiddlydesktop_rs.LanSyncService") {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[LAN Sync] Failed to create class name string: {}", e);
            return;
        }
    };

    if let Err(e) = env.call_method(
        &intent,
        "setClassName",
        "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;",
        &[JValue::Object(&pkg), JValue::Object(&JObject::from(cls_name))],
    ) {
        eprintln!("[LAN Sync] Failed to set intent class name: {}", e);
        return;
    }

    // Start foreground service
    if let Err(e) = env.call_method(
        &context,
        "startForegroundService",
        "(Landroid/content/Intent;)Landroid/content/ComponentName;",
        &[JValue::Object(&intent)],
    ) {
        eprintln!("[LAN Sync] Failed to start foreground service: {}", e);
    } else {
        eprintln!("[LAN Sync] Foreground service started");
    }
}

/// Stop the LanSyncService foreground service via JNI (Android only).
/// Uses Intent.setClassName() to avoid FindClass on app classes from native threads.
#[cfg(target_os = "android")]
fn stop_sync_foreground_service() {
    use crate::android::wiki_activity::get_java_vm;
    use jni::objects::{JValue, JObject};

    let vm = match get_java_vm() {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("[LAN Sync] Can't get JavaVM for foreground service stop: {}", e);
            return;
        }
    };

    let mut env = match vm.attach_current_thread() {
        Ok(env) => env,
        Err(e) => {
            eprintln!("[LAN Sync] Can't attach thread for foreground service stop: {}", e);
            return;
        }
    };

    let context = match env.call_static_method(
        "android/app/ActivityThread",
        "currentApplication",
        "()Landroid/app/Application;",
        &[],
    ) {
        Ok(val) => match val.l() {
            Ok(obj) if !obj.is_null() => obj,
            _ => {
                eprintln!("[LAN Sync] Can't get app context for foreground service stop");
                return;
            }
        },
        Err(e) => {
            eprintln!("[LAN Sync] Failed to get app context: {}", e);
            return;
        }
    };

    // Create Intent and set component by class name string (avoids FindClass)
    let intent = match env.new_object("android/content/Intent", "()V", &[]) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("[LAN Sync] Failed to create Intent for stop: {}", e);
            return;
        }
    };

    let pkg = match env.call_method(&context, "getPackageName", "()Ljava/lang/String;", &[]) {
        Ok(val) => match val.l() {
            Ok(obj) => obj,
            Err(e) => {
                eprintln!("[LAN Sync] Failed to get package name: {}", e);
                return;
            }
        },
        Err(e) => {
            eprintln!("[LAN Sync] Failed to call getPackageName: {}", e);
            return;
        }
    };

    let cls_name = match env.new_string("com.burningtreec.tiddlydesktop_rs.LanSyncService") {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[LAN Sync] Failed to create class name string: {}", e);
            return;
        }
    };

    if let Err(e) = env.call_method(
        &intent,
        "setClassName",
        "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;",
        &[JValue::Object(&pkg), JValue::Object(&JObject::from(cls_name))],
    ) {
        eprintln!("[LAN Sync] Failed to set intent class name: {}", e);
        return;
    }

    // Stop the service
    if let Err(e) = env.call_method(
        &context,
        "stopService",
        "(Landroid/content/Intent;)Z",
        &[JValue::Object(&intent)],
    ) {
        eprintln!("[LAN Sync] Failed to stop foreground service: {}", e);
    } else {
        eprintln!("[LAN Sync] Foreground service stopped");
    }
}

/// Update the LanSyncService notification with the current peer count.
/// Sends an Intent with ACTION_UPDATE_PEERS to the service (same pattern as wiki open/close).
#[cfg(target_os = "android")]
fn update_sync_notification_peers(peer_count: i32) {
    use crate::android::wiki_activity::get_java_vm;
    use jni::objects::{JValue, JObject};

    let vm = match get_java_vm() {
        Ok(vm) => vm,
        Err(_) => return,
    };

    let mut env = match vm.attach_current_thread() {
        Ok(env) => env,
        Err(_) => return,
    };

    let context = match env.call_static_method(
        "android/app/ActivityThread",
        "currentApplication",
        "()Landroid/app/Application;",
        &[],
    ) {
        Ok(val) => match val.l() {
            Ok(obj) if !obj.is_null() => obj,
            _ => return,
        },
        Err(_) => return,
    };

    let intent = match env.new_object("android/content/Intent", "()V", &[]) {
        Ok(i) => i,
        Err(_) => return,
    };

    let pkg = match env.call_method(&context, "getPackageName", "()Ljava/lang/String;", &[]) {
        Ok(val) => match val.l() {
            Ok(obj) => obj,
            Err(_) => return,
        },
        Err(_) => return,
    };

    let cls_name = match env.new_string("com.burningtreec.tiddlydesktop_rs.LanSyncService") {
        Ok(s) => s,
        Err(_) => return,
    };

    if env.call_method(
        &intent,
        "setClassName",
        "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;",
        &[JValue::Object(&pkg), JValue::Object(&JObject::from(cls_name))],
    ).is_err() {
        return;
    }

    // Set action
    let action = match env.new_string("com.burningtreec.tiddlydesktop_rs.UPDATE_PEERS") {
        Ok(s) => s,
        Err(_) => return,
    };
    if env.call_method(
        &intent,
        "setAction",
        "(Ljava/lang/String;)Landroid/content/Intent;",
        &[JValue::Object(&JObject::from(action))],
    ).is_err() {
        return;
    }

    // Put peer count extra
    let key = match env.new_string("peer_count") {
        Ok(s) => s,
        Err(_) => return,
    };
    if env.call_method(
        &intent,
        "putExtra",
        "(Ljava/lang/String;I)Landroid/content/Intent;",
        &[JValue::Object(&JObject::from(key)), JValue::Int(peer_count)],
    ).is_err() {
        return;
    }

    // Start service with this intent (delivers to onStartCommand)
    let _ = env.call_method(
        &context,
        "startService",
        "(Landroid/content/Intent;)Landroid/content/ComponentName;",
        &[JValue::Object(&intent)],
    );
}
