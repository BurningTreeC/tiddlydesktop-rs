//! Inter-Process Communication for multi-process wiki architecture
//!
//! The main process runs an IPC server that coordinates between wiki processes.
//! Each wiki file has a "wiki group" - the primary wiki window plus any tiddler windows.
//! Changes in one window are broadcast to all windows in the same group.
//!
//! ## Security
//!
//! IPC uses a shared secret token to authenticate clients. The token is generated
//! at app startup and must be provided by clients when registering. This prevents
//! other processes on localhost from connecting and spoofing messages.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use rand::Rng;

/// Default port for IPC server (main process)
pub const IPC_PORT: u16 = 45678;

/// Length of the authentication token in bytes
const AUTH_TOKEN_LENGTH: usize = 32;

/// Read timeout for IPC connections during initial handshake (prevents slow-loris)
/// After registration, the timeout is removed to allow long-lived idle connections
const IPC_HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Maximum concurrent IPC connections (one per wiki window, plus some headroom)
const MAX_IPC_CONNECTIONS: usize = 100;

/// Global connection counter for limiting concurrent connections
static ACTIVE_CONNECTIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Global authentication token (generated once at startup)
static AUTH_TOKEN: OnceLock<String> = OnceLock::new();

/// Generate and store the authentication token
/// Should be called once at app startup
pub fn init_auth_token() -> String {
    AUTH_TOKEN.get_or_init(|| {
        let mut rng = rand::rng();
        let token: String = (0..AUTH_TOKEN_LENGTH)
            .map(|_| {
                let idx = rng.random_range(0..62);
                let c = if idx < 10 {
                    (b'0' + idx) as char
                } else if idx < 36 {
                    (b'a' + idx - 10) as char
                } else {
                    (b'A' + idx - 36) as char
                };
                c
            })
            .collect();
        eprintln!("[IPC] Generated authentication token");
        token
    }).clone()
}

/// Environment variable name for passing auth token to child processes
pub const AUTH_TOKEN_ENV_VAR: &str = "TIDDLYDESKTOP_IPC_AUTH";

/// Get the authentication token
/// First checks the static (for main process), then environment variable (for spawned processes)
pub fn get_auth_token() -> Option<String> {
    // First try the static (set by main process)
    if let Some(token) = AUTH_TOKEN.get().cloned() {
        return Some(token);
    }

    // Fallback: check environment variable (for spawned wiki processes)
    std::env::var(AUTH_TOKEN_ENV_VAR).ok()
}

/// IPC message types
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum IpcMessage {
    /// Wiki process registering with the broker
    Register {
        wiki_path: String,
        pid: u32,
        is_tiddler_window: bool,
        tiddler_title: Option<String>,
        /// Authentication token (required for security)
        auth_token: String,
    },
    /// Wiki process unregistering (closing)
    Unregister {
        wiki_path: String,
        pid: u32,
    },
    /// Request to open a wiki (forwarded to main process)
    OpenWiki {
        path: String,
    },
    /// Request to open a tiddler in a new window
    OpenTiddlerWindow {
        wiki_path: String,
        tiddler_title: String,
        /// Optional startup tiddler for the window
        startup_tiddler: Option<String>,
    },
    /// Request to focus an existing wiki window
    FocusWiki {
        wiki_path: String,
    },
    /// Tiddler content changed - broadcast to wiki group
    TiddlerChanged {
        wiki_path: String,
        tiddler_title: String,
        tiddler_json: String,
        /// PID of sender (so it doesn't echo back)
        sender_pid: u32,
    },
    /// Tiddler deleted - broadcast to wiki group
    TiddlerDeleted {
        wiki_path: String,
        tiddler_title: String,
        sender_pid: u32,
    },
    /// Full wiki sync request (new window joining)
    RequestSync {
        wiki_path: String,
        requester_pid: u32,
    },
    /// Full wiki state (response to sync request)
    SyncState {
        wiki_path: String,
        /// JSON array of all tiddlers
        tiddlers_json: String,
    },
    /// Acknowledgment
    Ack {
        success: bool,
        message: Option<String>,
    },
    /// Update favicon for a wiki (sent from wiki process to main process)
    UpdateFavicon {
        wiki_path: String,
        favicon: Option<String>,
    },
    /// Ping/keepalive
    Ping,
    Pong,

    // ── LAN Sync IPC messages ────────────────────────────────────────

    /// Wiki process → main process: notify that a sync-enabled wiki window opened
    LanSyncWikiOpened {
        wiki_id: String,
    },
    /// Wiki process → main process: tiddler changed in a sync-enabled wiki
    LanSyncTiddlerChanged {
        wiki_id: String,
        title: String,
        tiddler_json: String,
    },
    /// Wiki process → main process: tiddler deleted in a sync-enabled wiki
    LanSyncTiddlerDeleted {
        wiki_id: String,
        title: String,
    },
    /// Wiki process → main process: batch of tiddlers for full sync dump
    LanSyncFullSyncBatch {
        wiki_id: String,
        to_device_id: String,
        tiddlers_json: String,
        is_last_batch: bool,
    },
    /// Wiki process → main process: tiddler fingerprints for diff-based sync
    LanSyncSendFingerprints {
        wiki_id: String,
        to_device_id: String,
        fingerprints_json: String,
    },
    /// Wiki process → main process: broadcast fingerprints to ALL peers sharing this wiki
    LanSyncBroadcastFingerprints {
        wiki_id: String,
        fingerprints_json: String,
    },

    /// Main process → wiki process: apply a remote tiddler change
    LanSyncApplyChange {
        wiki_id: String,
        payload_json: String,
    },

    // ── LAN Sync collaborative editing IPC messages ─────────────────

    /// Wiki process → main process: started editing a tiddler
    LanSyncCollabEditingStarted {
        wiki_id: String,
        tiddler_title: String,
    },
    /// Wiki process → main process: stopped editing a tiddler
    LanSyncCollabEditingStopped {
        wiki_id: String,
        tiddler_title: String,
    },
    /// Wiki process → main process: Yjs document update
    LanSyncCollabUpdate {
        wiki_id: String,
        tiddler_title: String,
        update_base64: String,
    },
    /// Wiki process → main process: Yjs awareness update
    LanSyncCollabAwareness {
        wiki_id: String,
        tiddler_title: String,
        update_base64: String,
    },
}

/// A connected wiki process
#[allow(dead_code)]
struct WikiClient {
    write_stream: Arc<Mutex<TcpStream>>,
    wiki_path: String,
    pid: u32,
    is_tiddler_window: bool,
}

/// IPC Server state (runs in main process)
pub struct IpcServer {
    /// Connected clients grouped by wiki path
    wiki_groups: Arc<Mutex<HashMap<String, Vec<WikiClient>>>>,
    /// All clients by PID for quick lookup
    clients_by_pid: Arc<Mutex<HashMap<u32, Arc<Mutex<TcpStream>>>>>,
    /// Callback for opening wikis
    open_wiki_callback: Arc<Mutex<Option<Box<dyn Fn(String) + Send + 'static>>>>,
    /// Callback for opening tiddler windows
    open_tiddler_callback: Arc<Mutex<Option<Box<dyn Fn(String, String, Option<String>) + Send + 'static>>>>,
    /// Callback for updating wiki favicon
    update_favicon_callback: Arc<Mutex<Option<Box<dyn Fn(String, Option<String>) + Send + 'static>>>>,
    /// Callback for when a new wiki client registers (after authentication)
    register_callback: Arc<Mutex<Option<Box<dyn Fn(String) + Send + 'static>>>>,
    /// Authentication token for validating clients
    auth_token: String,
}

impl IpcServer {
    pub fn new() -> Self {
        // Get or initialize the auth token
        let token = init_auth_token();
        Self {
            wiki_groups: Arc::new(Mutex::new(HashMap::new())),
            clients_by_pid: Arc::new(Mutex::new(HashMap::new())),
            open_wiki_callback: Arc::new(Mutex::new(None)),
            open_tiddler_callback: Arc::new(Mutex::new(None)),
            update_favicon_callback: Arc::new(Mutex::new(None)),
            register_callback: Arc::new(Mutex::new(None)),
            auth_token: token,
        }
    }

    /// Send a LAN sync message to all connected wiki processes.
    /// Used by the main process to push inbound sync changes to wiki windows.
    /// Send a LAN sync message to all connected IPC clients.
    /// Returns the number of clients that received the message.
    pub fn send_lan_sync_to_all(&self, wiki_id: &str, payload_json: &str) -> usize {
        let msg = IpcMessage::LanSyncApplyChange {
            wiki_id: wiki_id.to_string(),
            payload_json: payload_json.to_string(),
        };
        if let Ok(json) = serde_json::to_string(&msg) {
            let mut broken_pids = Vec::new();
            let client_count;
            {
                let clients = self.clients_by_pid.lock().unwrap();
                client_count = clients.len();
                eprintln!("[IPC] send_lan_sync_to_all: wiki_id={}, {} clients, payload_len={}", wiki_id, client_count, payload_json.len());
                for (pid, stream_arc) in clients.iter() {
                    let mut s = stream_arc.lock().unwrap();
                    if let Err(e) = writeln!(s, "{}", json) {
                        eprintln!("[IPC] Failed to send LAN sync to pid {}: {}", pid, e);
                        broken_pids.push(*pid);
                        continue;
                    }
                    if let Err(e) = s.flush() {
                        eprintln!("[IPC] Failed to flush LAN sync to pid {}: {}", pid, e);
                        broken_pids.push(*pid);
                    }
                }
            }
            // Remove broken streams and their wiki group entries
            if !broken_pids.is_empty() {
                let mut clients = self.clients_by_pid.lock().unwrap();
                let mut groups = self.wiki_groups.lock().unwrap();
                for pid in &broken_pids {
                    clients.remove(pid);
                    // Also clean up wiki_groups
                    groups.retain(|_, group| {
                        group.retain(|c| c.pid != *pid);
                        !group.is_empty()
                    });
                }
                eprintln!("[IPC] Cleaned up {} broken client(s)", broken_pids.len());
            }
            let delivered = client_count - broken_pids.len();

            // Same-process fallback: On Linux/macOS, wiki windows run inside
            // the main process and never connect as TCP IPC clients. Push to
            // IPC_SYNC_QUEUE so JS can poll via lan_sync_poll_ipc.
            if delivered == 0 {
                crate::lan_sync::queue_lan_sync_ipc(payload_json.to_string());
            }

            return delivered;
        }
        0
    }

    /// Get a reference to clients_by_pid for sending targeted messages
    pub fn clients_by_pid(&self) -> &Arc<Mutex<HashMap<u32, Arc<Mutex<TcpStream>>>>> {
        &self.clients_by_pid
    }

    /// Set callback for when a wiki open is requested
    pub fn on_open_wiki<F>(&self, callback: F)
    where
        F: Fn(String) + Send + 'static,
    {
        *self.open_wiki_callback.lock().unwrap() = Some(Box::new(callback));
    }

    /// Set callback for when a tiddler window open is requested
    pub fn on_open_tiddler<F>(&self, callback: F)
    where
        F: Fn(String, String, Option<String>) + Send + 'static,
    {
        *self.open_tiddler_callback.lock().unwrap() = Some(Box::new(callback));
    }

    /// Set callback for when a wiki favicon update is requested
    pub fn on_update_favicon<F>(&self, callback: F)
    where
        F: Fn(String, Option<String>) + Send + 'static,
    {
        *self.update_favicon_callback.lock().unwrap() = Some(Box::new(callback));
    }

    /// Set callback for when a new wiki client registers (after authentication)
    pub fn on_client_registered<F>(&self, callback: F)
    where
        F: Fn(String) + Send + 'static,
    {
        *self.register_callback.lock().unwrap() = Some(Box::new(callback));
    }

    /// Start the IPC server (blocks, run in separate thread)
    pub fn start(&self) -> std::io::Result<()> {
        let listener = TcpListener::bind(format!("127.0.0.1:{}", IPC_PORT))?;
        eprintln!("[IPC] Server listening on port {}", IPC_PORT);

        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    // Security: Check connection limit before accepting
                    let current = ACTIVE_CONNECTIONS.load(std::sync::atomic::Ordering::SeqCst);
                    if current >= MAX_IPC_CONNECTIONS {
                        eprintln!("[IPC] Security: Connection limit reached ({}), rejecting new connection", current);
                        drop(stream); // Close the connection
                        continue;
                    }

                    // Reduce write latency for IPC messages
                    let _ = stream.set_nodelay(true);

                    // Security: Set read timeout during handshake to prevent slow-loris attacks
                    // After authentication, timeout is removed to allow long-lived idle connections
                    if let Err(e) = stream.set_read_timeout(Some(IPC_HANDSHAKE_TIMEOUT)) {
                        eprintln!("[IPC] Warning: Failed to set read timeout: {}", e);
                    }

                    // Increment connection counter
                    ACTIVE_CONNECTIONS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

                    let wiki_groups = self.wiki_groups.clone();
                    let clients_by_pid = self.clients_by_pid.clone();
                    let open_wiki_cb = self.open_wiki_callback.clone();
                    let open_tiddler_cb = self.open_tiddler_callback.clone();
                    let update_favicon_cb = self.update_favicon_callback.clone();
                    let register_cb = self.register_callback.clone();
                    let auth_token = self.auth_token.clone();

                    thread::spawn(move || {
                        let result = handle_client(
                            stream,
                            wiki_groups,
                            clients_by_pid,
                            open_wiki_cb,
                            open_tiddler_cb,
                            update_favicon_cb,
                            register_cb,
                            auth_token,
                        );
                        // Always decrement connection counter when done
                        ACTIVE_CONNECTIONS.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                        if let Err(e) = result {
                            eprintln!("[IPC] Client handler error: {}", e);
                        }
                    });
                }
                Err(e) => {
                    eprintln!("[IPC] Accept error: {}", e);
                }
            }
        }
        Ok(())
    }

    /// Send a focus window request to all clients for a specific wiki
    pub fn send_focus_window(&self, wiki_path: &str) -> std::io::Result<()> {
        let msg = IpcMessage::FocusWiki {
            wiki_path: wiki_path.to_string(),
        };
        let json = serde_json::to_string(&msg)?;

        let groups = self.wiki_groups.lock().unwrap();
        if let Some(clients) = groups.get(wiki_path) {
            for client in clients {
                let mut s = client.write_stream.lock().unwrap();
                let _ = writeln!(s, "{}", json);
            }
        }
        Ok(())
    }
}

fn handle_client(
    stream: TcpStream,
    wiki_groups: Arc<Mutex<HashMap<String, Vec<WikiClient>>>>,
    clients_by_pid: Arc<Mutex<HashMap<u32, Arc<Mutex<TcpStream>>>>>,
    open_wiki_cb: Arc<Mutex<Option<Box<dyn Fn(String) + Send + 'static>>>>,
    open_tiddler_cb: Arc<Mutex<Option<Box<dyn Fn(String, String, Option<String>) + Send + 'static>>>>,
    update_favicon_cb: Arc<Mutex<Option<Box<dyn Fn(String, Option<String>) + Send + 'static>>>>,
    register_cb: Arc<Mutex<Option<Box<dyn Fn(String) + Send + 'static>>>>,
    expected_auth_token: String,
) -> std::io::Result<()> {
    let peer_addr = stream.peer_addr()?;
    eprintln!("[IPC] New connection from {}", peer_addr);

    // ONLY clone — used exclusively by BufReader for reading.
    // The original stream is wrapped in Arc<Mutex<>> for ALL writes, ensuring
    // serialization across threads. This fixes Windows/macOS where try_clone()
    // (WSADuplicateSocketW / dup) creates handles that don't serialize writes.
    let reader_clone = stream.try_clone()?;
    let write_stream = Arc::new(Mutex::new(stream));
    let mut reader = BufReader::new(reader_clone);
    let mut client_wiki_path: Option<String> = None;
    let mut client_pid: Option<u32> = None;
    let mut client_authenticated = false;

    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {
                // Connection closed
                eprintln!("[IPC] Connection closed from {}", peer_addr);
                break;
            }
            Ok(_) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }

                match serde_json::from_str::<IpcMessage>(line) {
                    Ok(msg) => {
                        match &msg {
                            IpcMessage::Register { wiki_path, pid, is_tiddler_window, auth_token, .. } => {
                                // Security: Validate authentication token
                                if auth_token != &expected_auth_token {
                                    eprintln!("[IPC] Security: Invalid auth token from pid={}, rejecting", pid);
                                    let ack = IpcMessage::Ack {
                                        success: false,
                                        message: Some("Invalid authentication token".to_string()),
                                    };
                                    let mut ws = write_stream.lock().unwrap();
                                    let _ = writeln!(ws, "{}", serde_json::to_string(&ack)?);
                                    // Close connection on auth failure
                                    break;
                                }

                                eprintln!("[IPC] Register: wiki={}, pid={}, tiddler_window={} (authenticated)",
                                    wiki_path, pid, is_tiddler_window);

                                client_authenticated = true;

                                // Remove read timeout after successful auth to allow long-lived idle connections.
                                // CRITICAL: Must clear on BOTH the original stream AND the reader's clone.
                                // On Windows, WSADuplicateSocketW creates a new socket descriptor and
                                // SO_RCVTIMEO set on one handle may not propagate to the other (unlike
                                // Linux where dup() shares the underlying socket object). If the reader's
                                // clone keeps the 30s handshake timeout, the server disconnects the client
                                // after 30s of inactivity — breaking all IPC communication silently.
                                {
                                    let ws = write_stream.lock().unwrap();
                                    if let Err(e) = ws.set_read_timeout(None) {
                                        eprintln!("[IPC] Warning: Failed to clear read timeout on original: {}", e);
                                    }
                                }
                                if let Err(e) = reader.get_mut().set_read_timeout(None) {
                                    eprintln!("[IPC] Warning: Failed to clear read timeout on reader: {}", e);
                                }
                                client_wiki_path = Some(wiki_path.clone());
                                client_pid = Some(*pid);

                                // Add to wiki group
                                let mut groups = wiki_groups.lock().unwrap();
                                let group = groups.entry(wiki_path.clone()).or_insert_with(Vec::new);
                                group.push(WikiClient {
                                    write_stream: Arc::clone(&write_stream),
                                    wiki_path: wiki_path.clone(),
                                    pid: *pid,
                                    is_tiddler_window: *is_tiddler_window,
                                });

                                // Track by PID
                                clients_by_pid.lock().unwrap().insert(*pid, Arc::clone(&write_stream));

                                // Send ack
                                let ack = IpcMessage::Ack { success: true, message: None };
                                {
                                    let mut ws = write_stream.lock().unwrap();
                                    let _ = writeln!(ws, "{}", serde_json::to_string(&ack)?);
                                }

                                // Notify callback that a new client registered
                                if let Some(ref cb) = *register_cb.lock().unwrap() {
                                    cb(wiki_path.clone());
                                }
                            }

                            IpcMessage::Unregister { wiki_path, pid } => {
                                if !client_authenticated {
                                    eprintln!("[IPC] Security: Unauthenticated Unregister attempt, ignoring");
                                    continue;
                                }
                                eprintln!("[IPC] Unregister: wiki={}, pid={}", wiki_path, pid);

                                // Remove from wiki group
                                let mut groups = wiki_groups.lock().unwrap();
                                if let Some(group) = groups.get_mut(wiki_path) {
                                    group.retain(|c| c.pid != *pid);
                                    if group.is_empty() {
                                        groups.remove(wiki_path);
                                    }
                                }

                                // Remove from PID tracking
                                clients_by_pid.lock().unwrap().remove(pid);
                            }

                            IpcMessage::OpenWiki { path } => {
                                if !client_authenticated {
                                    eprintln!("[IPC] Security: Unauthenticated OpenWiki attempt, ignoring");
                                    continue;
                                }
                                eprintln!("[IPC] OpenWiki request: {}", path);
                                if let Some(ref cb) = *open_wiki_cb.lock().unwrap() {
                                    cb(path.clone());
                                }
                                let ack = IpcMessage::Ack { success: true, message: None };
                                let mut ws = write_stream.lock().unwrap();
                                let _ = writeln!(ws, "{}", serde_json::to_string(&ack)?);
                            }

                            IpcMessage::OpenTiddlerWindow { wiki_path, tiddler_title, startup_tiddler } => {
                                if !client_authenticated {
                                    eprintln!("[IPC] Security: Unauthenticated OpenTiddlerWindow attempt, ignoring");
                                    continue;
                                }
                                eprintln!("[IPC] OpenTiddlerWindow request: wiki={}, tiddler={}",
                                    wiki_path, tiddler_title);
                                if let Some(ref cb) = *open_tiddler_cb.lock().unwrap() {
                                    cb(wiki_path.clone(), tiddler_title.clone(), startup_tiddler.clone());
                                }
                                let ack = IpcMessage::Ack { success: true, message: None };
                                let mut ws = write_stream.lock().unwrap();
                                let _ = writeln!(ws, "{}", serde_json::to_string(&ack)?);
                            }

                            IpcMessage::TiddlerChanged { wiki_path, sender_pid, .. } => {
                                if !client_authenticated {
                                    eprintln!("[IPC] Security: Unauthenticated TiddlerChanged attempt, ignoring");
                                    continue;
                                }
                                // Broadcast to all other clients in the same wiki group
                                let groups = wiki_groups.lock().unwrap();
                                if let Some(clients) = groups.get(wiki_path) {
                                    for client in clients {
                                        if client.pid != *sender_pid {
                                            if let Ok(json) = serde_json::to_string(&msg) {
                                                let mut s = client.write_stream.lock().unwrap();
                                                let _ = writeln!(s, "{}", json);
                                            }
                                        }
                                    }
                                }
                            }

                            IpcMessage::TiddlerDeleted { wiki_path, sender_pid, .. } => {
                                if !client_authenticated {
                                    eprintln!("[IPC] Security: Unauthenticated TiddlerDeleted attempt, ignoring");
                                    continue;
                                }
                                // Broadcast to all other clients in the same wiki group
                                let groups = wiki_groups.lock().unwrap();
                                if let Some(clients) = groups.get(wiki_path) {
                                    for client in clients {
                                        if client.pid != *sender_pid {
                                            if let Ok(json) = serde_json::to_string(&msg) {
                                                let mut s = client.write_stream.lock().unwrap();
                                                let _ = writeln!(s, "{}", json);
                                            }
                                        }
                                    }
                                }
                            }

                            IpcMessage::RequestSync { wiki_path, requester_pid } => {
                                if !client_authenticated {
                                    eprintln!("[IPC] Security: Unauthenticated RequestSync attempt, ignoring");
                                    continue;
                                }
                                eprintln!("[IPC] SyncRequest from pid {} for {}", requester_pid, wiki_path);
                                // Find the primary (non-tiddler) window for this wiki and ask it to send state
                                let groups = wiki_groups.lock().unwrap();
                                if let Some(clients) = groups.get(wiki_path) {
                                    for client in clients {
                                        if !client.is_tiddler_window && client.pid != *requester_pid {
                                            // Ask this client to send sync state
                                            if let Ok(json) = serde_json::to_string(&msg) {
                                                let mut s = client.write_stream.lock().unwrap();
                                                let _ = writeln!(s, "{}", json);
                                            }
                                            break;
                                        }
                                    }
                                }
                            }

                            IpcMessage::SyncState { wiki_path, .. } => {
                                if !client_authenticated {
                                    eprintln!("[IPC] Security: Unauthenticated SyncState attempt, ignoring");
                                    continue;
                                }
                                // Forward to all tiddler windows that need sync
                                let groups = wiki_groups.lock().unwrap();
                                if let Some(clients) = groups.get(wiki_path) {
                                    for client in clients {
                                        if client.is_tiddler_window {
                                            if let Ok(json) = serde_json::to_string(&msg) {
                                                let mut s = client.write_stream.lock().unwrap();
                                                let _ = writeln!(s, "{}", json);
                                            }
                                        }
                                    }
                                }
                            }

                            IpcMessage::UpdateFavicon { wiki_path, favicon } => {
                                if !client_authenticated {
                                    eprintln!("[IPC] Security: Unauthenticated UpdateFavicon attempt, ignoring");
                                    continue;
                                }
                                eprintln!("[IPC] UpdateFavicon request: wiki={}", wiki_path);
                                if let Some(ref cb) = *update_favicon_cb.lock().unwrap() {
                                    cb(wiki_path.clone(), favicon.clone());
                                }
                                let ack = IpcMessage::Ack { success: true, message: None };
                                let mut ws = write_stream.lock().unwrap();
                                let _ = writeln!(ws, "{}", serde_json::to_string(&ack)?);
                            }

                            IpcMessage::Ping => {
                                let pong = IpcMessage::Pong;
                                let mut ws = write_stream.lock().unwrap();
                                let _ = writeln!(ws, "{}", serde_json::to_string(&pong)?);
                            }

                            // ── LAN Sync: wiki process → main process ─────────
                            IpcMessage::LanSyncWikiOpened { wiki_id } => {
                                if !client_authenticated {
                                    continue;
                                }
                                #[cfg(not(target_os = "android"))]
                                {
                                    if let Some(mgr) = crate::lan_sync::get_sync_manager() {
                                        let mgr = mgr.clone();
                                        let wiki_id_owned = wiki_id.clone();
                                        tauri::async_runtime::spawn(async move {
                                            mgr.on_wiki_opened(&wiki_id_owned).await;
                                        });
                                    }
                                }
                            }

                            IpcMessage::LanSyncTiddlerChanged { wiki_id, title, tiddler_json } => {
                                if !client_authenticated {
                                    continue;
                                }
                                #[cfg(not(target_os = "android"))]
                                {
                                    if let Some(mgr) = crate::lan_sync::get_sync_manager() {
                                        mgr.notify_tiddler_changed(wiki_id, title, tiddler_json);
                                    }
                                }
                            }

                            IpcMessage::LanSyncTiddlerDeleted { wiki_id, title } => {
                                if !client_authenticated {
                                    continue;
                                }
                                #[cfg(not(target_os = "android"))]
                                {
                                    if let Some(mgr) = crate::lan_sync::get_sync_manager() {
                                        mgr.notify_tiddler_deleted(wiki_id, title);
                                    }
                                }
                            }

                            IpcMessage::LanSyncFullSyncBatch { wiki_id, to_device_id, tiddlers_json, is_last_batch } => {
                                if !client_authenticated {
                                    continue;
                                }
                                #[cfg(not(target_os = "android"))]
                                {
                                    if let Some(mgr) = crate::lan_sync::get_sync_manager() {
                                        let tiddlers: Vec<crate::lan_sync::TiddlerBatch> =
                                            serde_json::from_str(tiddlers_json).unwrap_or_default();
                                        let mgr = mgr.clone();
                                        let wiki_id_owned = wiki_id.clone();
                                        let to_device_id_owned = to_device_id.clone();
                                        let is_last = *is_last_batch;
                                        tauri::async_runtime::spawn(async move {
                                            if let Err(e) = mgr
                                                .send_full_sync_batch(
                                                    &wiki_id_owned,
                                                    &to_device_id_owned,
                                                    tiddlers,
                                                    is_last,
                                                )
                                                .await
                                            {
                                                eprintln!(
                                                    "[IPC] Full sync batch error: {}",
                                                    e
                                                );
                                            }
                                        });
                                    }
                                }
                            }

                            IpcMessage::LanSyncSendFingerprints { wiki_id, to_device_id, fingerprints_json } => {
                                if !client_authenticated {
                                    continue;
                                }
                                #[cfg(not(target_os = "android"))]
                                {
                                    if let Some(mgr) = crate::lan_sync::get_sync_manager() {
                                        let fingerprints: Vec<crate::lan_sync::protocol::TiddlerFingerprint> =
                                            serde_json::from_str(fingerprints_json).unwrap_or_default();
                                        let mgr = mgr.clone();
                                        let wiki_id_owned = wiki_id.clone();
                                        let to_device_id_owned = to_device_id.clone();
                                        tauri::async_runtime::spawn(async move {
                                            if let Err(e) = mgr
                                                .send_tiddler_fingerprints(
                                                    &wiki_id_owned,
                                                    &to_device_id_owned,
                                                    fingerprints,
                                                )
                                                .await
                                            {
                                                eprintln!(
                                                    "[IPC] Send fingerprints error: {}",
                                                    e
                                                );
                                            }
                                        });
                                    }
                                }
                            }

                            IpcMessage::LanSyncBroadcastFingerprints { wiki_id, fingerprints_json } => {
                                if !client_authenticated {
                                    continue;
                                }
                                #[cfg(not(target_os = "android"))]
                                {
                                    if let Some(mgr) = crate::lan_sync::get_sync_manager() {
                                        let fingerprints: Vec<crate::lan_sync::protocol::TiddlerFingerprint> =
                                            serde_json::from_str(fingerprints_json).unwrap_or_default();
                                        let mgr = mgr.clone();
                                        let wiki_id_owned = wiki_id.clone();
                                        tauri::async_runtime::spawn(async move {
                                            if let Err(e) = mgr
                                                .broadcast_tiddler_fingerprints(
                                                    &wiki_id_owned,
                                                    fingerprints,
                                                )
                                                .await
                                            {
                                                eprintln!(
                                                    "[IPC] Broadcast fingerprints error: {}",
                                                    e
                                                );
                                            }
                                        });
                                    }
                                }
                            }

                            // ── LAN Sync collaborative editing ──────────────
                            IpcMessage::LanSyncCollabEditingStarted { wiki_id, tiddler_title } => {
                                if !client_authenticated {
                                    continue;
                                }
                                #[cfg(not(target_os = "android"))]
                                {
                                    if let Some(mgr) = crate::lan_sync::get_sync_manager() {
                                        mgr.notify_collab_editing_started(wiki_id, tiddler_title);
                                    }
                                }
                            }

                            IpcMessage::LanSyncCollabEditingStopped { wiki_id, tiddler_title } => {
                                if !client_authenticated {
                                    continue;
                                }
                                #[cfg(not(target_os = "android"))]
                                {
                                    if let Some(mgr) = crate::lan_sync::get_sync_manager() {
                                        mgr.notify_collab_editing_stopped(wiki_id, tiddler_title);
                                    }
                                }
                            }

                            IpcMessage::LanSyncCollabUpdate { wiki_id, tiddler_title, update_base64 } => {
                                if !client_authenticated {
                                    continue;
                                }
                                #[cfg(not(target_os = "android"))]
                                {
                                    if let Some(mgr) = crate::lan_sync::get_sync_manager() {
                                        mgr.send_collab_update(wiki_id, tiddler_title, update_base64);
                                    }
                                }
                            }

                            IpcMessage::LanSyncCollabAwareness { wiki_id, tiddler_title, update_base64 } => {
                                if !client_authenticated {
                                    continue;
                                }
                                #[cfg(not(target_os = "android"))]
                                {
                                    if let Some(mgr) = crate::lan_sync::get_sync_manager() {
                                        mgr.send_collab_awareness(wiki_id, tiddler_title, update_base64);
                                    }
                                }
                            }

                            _ => {}
                        }
                    }
                    Err(e) => {
                        eprintln!("[IPC] Parse error: {} for line: {}", e, line);
                    }
                }
            }
            Err(e) => {
                // Check if this is a timeout error
                if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut {
                    // Timeout during handshake (before auth) - disconnect
                    if !client_authenticated {
                        eprintln!("[IPC] Handshake timeout from {}, disconnecting", peer_addr);
                        break;
                    }
                    // After auth, timeouts shouldn't happen (timeout is cleared), but if they do, continue
                    continue;
                }
                eprintln!("[IPC] Read error: {}", e);
                break;
            }
        }
    }

    // Clean up on disconnect
    if let (Some(wiki_path), Some(pid)) = (client_wiki_path, client_pid) {
        let mut groups = wiki_groups.lock().unwrap();
        if let Some(group) = groups.get_mut(&wiki_path) {
            group.retain(|c| c.pid != pid);
            if group.is_empty() {
                groups.remove(&wiki_path);
            }
        }
        clients_by_pid.lock().unwrap().remove(&pid);
        eprintln!("[IPC] Cleaned up client: wiki={}, pid={}", wiki_path, pid);
    }

    Ok(())
}

/// IPC Client (runs in wiki processes)
pub struct IpcClient {
    stream: Option<TcpStream>,
    wiki_path: String,
    is_tiddler_window: bool,
    tiddler_title: Option<String>,
    auth_token: String,
}

impl IpcClient {
    pub fn new(wiki_path: String, is_tiddler_window: bool, tiddler_title: Option<String>, auth_token: String) -> Self {
        Self {
            stream: None,
            wiki_path,
            is_tiddler_window,
            tiddler_title,
            auth_token,
        }
    }

    /// Connect to the IPC server
    pub fn connect(&mut self) -> std::io::Result<()> {
        let stream = TcpStream::connect(format!("127.0.0.1:{}", IPC_PORT))?;
        stream.set_nodelay(true)?;
        self.stream = Some(stream);

        // Register with the server (includes auth token)
        let pid = std::process::id();
        let msg = IpcMessage::Register {
            wiki_path: self.wiki_path.clone(),
            pid,
            is_tiddler_window: self.is_tiddler_window,
            tiddler_title: self.tiddler_title.clone(),
            auth_token: self.auth_token.clone(),
        };
        self.send(&msg)?;

        eprintln!("[IPC Client] Connected and registered: wiki={}, pid={}", self.wiki_path, pid);
        Ok(())
    }

    /// Get a clone of the stream for the listener thread
    /// Returns None if not connected
    pub fn get_listener_stream(&self) -> Option<TcpStream> {
        self.stream.as_ref().and_then(|s| s.try_clone().ok())
    }

    /// Send a message to the server
    pub fn send(&mut self, msg: &IpcMessage) -> std::io::Result<()> {
        if let Some(ref mut stream) = self.stream {
            let json = serde_json::to_string(msg)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            writeln!(stream, "{}", json)?;
            stream.flush()?;
        }
        Ok(())
    }

    /// Notify that a tiddler changed
    pub fn notify_tiddler_changed(&mut self, tiddler_title: &str, tiddler_json: &str) -> std::io::Result<()> {
        let msg = IpcMessage::TiddlerChanged {
            wiki_path: self.wiki_path.clone(),
            tiddler_title: tiddler_title.to_string(),
            tiddler_json: tiddler_json.to_string(),
            sender_pid: std::process::id(),
        };
        self.send(&msg)
    }

    /// Notify that a tiddler was deleted
    pub fn notify_tiddler_deleted(&mut self, tiddler_title: &str) -> std::io::Result<()> {
        let msg = IpcMessage::TiddlerDeleted {
            wiki_path: self.wiki_path.clone(),
            tiddler_title: tiddler_title.to_string(),
            sender_pid: std::process::id(),
        };
        self.send(&msg)
    }

    /// Request to open a tiddler window
    pub fn request_open_tiddler(&mut self, tiddler_title: &str, startup_tiddler: Option<&str>) -> std::io::Result<()> {
        let msg = IpcMessage::OpenTiddlerWindow {
            wiki_path: self.wiki_path.clone(),
            tiddler_title: tiddler_title.to_string(),
            startup_tiddler: startup_tiddler.map(|s| s.to_string()),
        };
        self.send(&msg)
    }

    /// Request full wiki sync (for new tiddler windows)
    pub fn request_sync(&mut self) -> std::io::Result<()> {
        let msg = IpcMessage::RequestSync {
            wiki_path: self.wiki_path.clone(),
            requester_pid: std::process::id(),
        };
        self.send(&msg)
    }

    /// Send full wiki state (response to sync request)
    pub fn send_sync_state(&mut self, tiddlers_json: &str) -> std::io::Result<()> {
        let msg = IpcMessage::SyncState {
            wiki_path: self.wiki_path.clone(),
            tiddlers_json: tiddlers_json.to_string(),
        };
        self.send(&msg)
    }

    /// Send favicon update to main process
    pub fn send_update_favicon(&mut self, wiki_path: &str, favicon: Option<String>) -> std::io::Result<()> {
        let msg = IpcMessage::UpdateFavicon {
            wiki_path: wiki_path.to_string(),
            favicon,
        };
        self.send(&msg)
    }

    // ── LAN Sync helpers ─────────────────────────────────────────────

    /// Notify main process that a sync-enabled wiki window opened
    pub fn send_lan_sync_wiki_opened(&mut self, wiki_id: &str) -> std::io::Result<()> {
        self.send(&IpcMessage::LanSyncWikiOpened {
            wiki_id: wiki_id.to_string(),
        })
    }

    /// Notify main process of a tiddler change for LAN sync
    pub fn send_lan_sync_tiddler_changed(&mut self, wiki_id: &str, title: &str, tiddler_json: &str) -> std::io::Result<()> {
        self.send(&IpcMessage::LanSyncTiddlerChanged {
            wiki_id: wiki_id.to_string(),
            title: title.to_string(),
            tiddler_json: tiddler_json.to_string(),
        })
    }

    /// Notify main process of a tiddler deletion for LAN sync
    pub fn send_lan_sync_tiddler_deleted(&mut self, wiki_id: &str, title: &str) -> std::io::Result<()> {
        self.send(&IpcMessage::LanSyncTiddlerDeleted {
            wiki_id: wiki_id.to_string(),
            title: title.to_string(),
        })
    }

    /// Send a batch of tiddlers for full sync dump
    pub fn send_lan_sync_full_batch(&mut self, wiki_id: &str, to_device_id: &str, tiddlers_json: &str, is_last_batch: bool) -> std::io::Result<()> {
        self.send(&IpcMessage::LanSyncFullSyncBatch {
            wiki_id: wiki_id.to_string(),
            to_device_id: to_device_id.to_string(),
            tiddlers_json: tiddlers_json.to_string(),
            is_last_batch,
        })
    }

    /// Send tiddler fingerprints for diff-based sync
    pub fn send_lan_sync_fingerprints(&mut self, wiki_id: &str, to_device_id: &str, fingerprints_json: &str) -> std::io::Result<()> {
        self.send(&IpcMessage::LanSyncSendFingerprints {
            wiki_id: wiki_id.to_string(),
            to_device_id: to_device_id.to_string(),
            fingerprints_json: fingerprints_json.to_string(),
        })
    }

    /// Broadcast tiddler fingerprints to all connected peers sharing this wiki
    pub fn send_lan_sync_broadcast_fingerprints(&mut self, wiki_id: &str, fingerprints_json: &str) -> std::io::Result<()> {
        self.send(&IpcMessage::LanSyncBroadcastFingerprints {
            wiki_id: wiki_id.to_string(),
            fingerprints_json: fingerprints_json.to_string(),
        })
    }

    // ── Collaborative editing IPC helpers ────────────────────────────

    /// Notify main process that we started editing a tiddler
    pub fn send_lan_sync_collab_editing_started(&mut self, wiki_id: &str, tiddler_title: &str) -> std::io::Result<()> {
        self.send(&IpcMessage::LanSyncCollabEditingStarted {
            wiki_id: wiki_id.to_string(),
            tiddler_title: tiddler_title.to_string(),
        })
    }

    /// Notify main process that we stopped editing a tiddler
    pub fn send_lan_sync_collab_editing_stopped(&mut self, wiki_id: &str, tiddler_title: &str) -> std::io::Result<()> {
        self.send(&IpcMessage::LanSyncCollabEditingStopped {
            wiki_id: wiki_id.to_string(),
            tiddler_title: tiddler_title.to_string(),
        })
    }

    /// Send a Yjs document update to main process for LAN sync
    pub fn send_lan_sync_collab_update(&mut self, wiki_id: &str, tiddler_title: &str, update_base64: &str) -> std::io::Result<()> {
        self.send(&IpcMessage::LanSyncCollabUpdate {
            wiki_id: wiki_id.to_string(),
            tiddler_title: tiddler_title.to_string(),
            update_base64: update_base64.to_string(),
        })
    }

    /// Send a Yjs awareness update to main process for LAN sync
    pub fn send_lan_sync_collab_awareness(&mut self, wiki_id: &str, tiddler_title: &str, update_base64: &str) -> std::io::Result<()> {
        self.send(&IpcMessage::LanSyncCollabAwareness {
            wiki_id: wiki_id.to_string(),
            tiddler_title: tiddler_title.to_string(),
            update_base64: update_base64.to_string(),
        })
    }
}

impl Drop for IpcClient {
    fn drop(&mut self) {
        // Unregister on drop
        let msg = IpcMessage::Unregister {
            wiki_path: self.wiki_path.clone(),
            pid: std::process::id(),
        };
        let _ = self.send(&msg);
    }
}

/// Try to connect to existing IPC server, returns None if server not running or no auth token
pub fn try_connect(wiki_path: &str, is_tiddler_window: bool, tiddler_title: Option<String>) -> Option<IpcClient> {
    // Get the auth token (must have been initialized by the server)
    let auth_token = get_auth_token()?;

    let mut client = IpcClient::new(wiki_path.to_string(), is_tiddler_window, tiddler_title, auth_token);
    match client.connect() {
        Ok(_) => Some(client),
        Err(_) => None,
    }
}

/// Run a listener loop on a stream (blocking, for use in a separate thread)
/// This allows wiki processes to receive messages from the IPC server
pub fn run_listener<F>(stream: TcpStream, mut callback: F)
where
    F: FnMut(IpcMessage),
{
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {
                eprintln!("[IPC Listener] Server disconnected");
                break;
            }
            Ok(_) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<IpcMessage>(line) {
                    Ok(msg) => callback(msg),
                    Err(e) => eprintln!("[IPC Listener] Parse error: {}", e),
                }
            }
            Err(e) => {
                eprintln!("[IPC Listener] Read error (IPC connection lost, LAN sync IPC broken): {}", e);
                break;
            }
        }
    }
}
