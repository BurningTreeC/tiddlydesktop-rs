//! WebSocket server for LAN sync.
//!
//! Listens on a port in the 45700-45710 range and accepts connections from
//! paired devices. Each connection goes through pairing (if not already paired)
//! then enters encrypted sync mode.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, watch, RwLock};
use tokio_tungstenite::tungstenite::{Message, protocol::WebSocketConfig};
use futures_util::{SinkExt, StreamExt};

use super::protocol::*;
use crate::relay_sync::RelaySyncManager;

/// Max buffered encrypted messages per peer (backpressure for large transfers).
/// 16 × ~1.4MB ≈ 22MB max buffered per peer — sufficient pipeline depth
/// for sustained throughput without excessive memory use.
pub const PEER_CHANNEL_BOUND: usize = 16;

/// If no pong is received within this duration after a ping, consider the connection dead.
pub const PONG_TIMEOUT_SECS: u64 = 6;

/// WebSocket configuration with large write buffer for sync batches.
/// Default max_write_buffer_size is 512KB which is too small for FullSyncBatch
/// messages (2-4MB with embedded images). 16MB handles any realistic batch.
pub fn ws_config() -> WebSocketConfig {
    WebSocketConfig {
        max_write_buffer_size: 16 * 1024 * 1024,
        max_message_size: Some(64 * 1024 * 1024),
        max_frame_size: Some(16 * 1024 * 1024),
        ..Default::default()
    }
}

/// Get current epoch milliseconds for pong tracking
pub fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Global connection ID counter — each new connection gets a unique ID
/// so cleanup can distinguish stale connections from current ones.
static NEXT_CONNECTION_ID: AtomicU64 = AtomicU64::new(1);

/// Allocate a new unique connection ID
pub fn next_connection_id() -> u64 {
    NEXT_CONNECTION_ID.fetch_add(1, Ordering::Relaxed)
}

/// A connected peer
pub struct PeerConnection {
    pub device_id: String,
    pub device_name: String,
    /// Unique ID for this connection instance — used to prevent stale cleanup
    /// from removing a newer replacement connection for the same peer.
    pub connection_id: u64,
    /// High-priority channel for tiddler sync messages (fingerprints, FullSyncBatch, etc.)
    pub tx: mpsc::Sender<Vec<u8>>,
    /// Low-priority channel for bulk data (attachment chunks, wiki file chunks).
    /// The outbound task drains `tx` first (biased select) so tiddler changes
    /// are never blocked by large attachment transfers.
    pub bulk_tx: mpsc::Sender<Vec<u8>>,
    /// Encryption state for this connection
    pub cipher: SessionCipher,
    /// Room codes this peer shares with us (for LAN connections).
    /// A peer may share multiple rooms with us — each room can have different wikis.
    pub auth_room_codes: Vec<String>,
    /// TiddlyWiki username of this peer (from UserNameAnnounce)
    pub user_name: Option<String>,
}

/// Events emitted by the server to be handled by the sync manager
#[derive(Debug)]
pub enum ServerEvent {
    /// A new peer connected and completed pairing/auth
    PeerConnected {
        device_id: String,
        device_name: String,
    },
    /// A peer disconnected
    PeerDisconnected {
        device_id: String,
    },
    /// Received an encrypted sync message from a peer
    SyncMessageReceived {
        from_device_id: String,
        message: SyncMessage,
    },
}

/// The LAN sync WebSocket server
pub struct SyncServer {
    /// Port the server is listening on
    port: u16,
    /// Connected peers (shared with client connections)
    peers: Arc<RwLock<HashMap<String, PeerConnection>>>,
    /// Channel for server events
    event_tx: mpsc::UnboundedSender<ServerEvent>,
    /// Shutdown signal sender — drop to stop the accept loop
    _shutdown_tx: watch::Sender<bool>,
    /// Optional relay transport — sends fall through to relay if peer not on LAN
    relay: RwLock<Option<Arc<RelaySyncManager>>>,
}

impl SyncServer {
    /// Start the WebSocket server, trying ports in the configured range.
    /// The `peers` map is shared with client-initiated connections so both
    /// inbound and outbound peers are tracked in the same place.
    /// `room_keys` maps room_code → group_key for room-based authentication.
    pub async fn start(
        room_keys: Arc<RwLock<HashMap<String, [u8; 32]>>>,
        our_device_id: String,
        our_device_name: String,
        event_tx: mpsc::UnboundedSender<ServerEvent>,
        peers: Arc<RwLock<HashMap<String, PeerConnection>>>,
    ) -> Result<Self, String> {
        let mut port = LAN_SYNC_PORT_START;
        let listener = loop {
            match TcpListener::bind(format!("0.0.0.0:{}", port)).await {
                Ok(listener) => break listener,
                Err(_) if port < LAN_SYNC_PORT_END => {
                    port += 1;
                }
                Err(e) => {
                    return Err(format!(
                        "Failed to bind to any port in {}-{}: {}",
                        LAN_SYNC_PORT_START, LAN_SYNC_PORT_END, e
                    ));
                }
            }
        };

        eprintln!("[LAN Sync] Server listening on port {}", port);

        let peers_clone = peers.clone();
        let event_tx_clone = event_tx.clone();

        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

        let room_keys_clone = room_keys.clone();
        let our_id = our_device_id.clone();
        let our_name = our_device_name.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok((stream, addr)) => {
                                eprintln!("[LAN Sync] New connection from {}", addr);
                                let rk = room_keys_clone.clone();
                                let peers = peers_clone.clone();
                                let event_tx = event_tx_clone.clone();
                                let did = our_id.clone();
                                let dname = our_name.clone();

                                tokio::spawn(async move {
                                    if let Err(e) =
                                        handle_connection(stream, rk, &did, &dname, peers, event_tx).await
                                    {
                                        eprintln!("[LAN Sync] Connection error from {}: {}", addr, e);
                                    }
                                });
                            }
                            Err(e) => {
                                eprintln!("[LAN Sync] Accept error: {}", e);
                            }
                        }
                    }
                    _ = shutdown_rx.changed() => {
                        eprintln!("[LAN Sync] Server accept loop shutting down");
                        break;
                    }
                }
            }
        });

        Ok(Self {
            port,
            peers,
            event_tx,
            _shutdown_tx: shutdown_tx,
            relay: RwLock::new(None),
        })
    }

    /// Get the port the server is listening on
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Set the relay manager for transparent relay fallback.
    /// When set, send operations try LAN first, then fall through to relay.
    pub async fn set_relay_manager(&self, relay: Arc<RelaySyncManager>) {
        *self.relay.write().await = Some(relay);
    }

    /// Send an encrypted sync message to a specific peer.
    /// Tries LAN first, falls through to relay if not connected via LAN.
    pub async fn send_to_peer(
        &self,
        device_id: &str,
        msg: &SyncMessage,
    ) -> Result<(), String> {
        // Try LAN first
        let lan_result = {
            let is_bulk = msg.is_bulk_data();
            let mut peers = self.peers.write().await;
            if let Some(peer) = peers.get_mut(device_id) {
                let encrypted = encrypt_message(&mut peer.cipher, msg)?;
                let channel = if is_bulk { peer.bulk_tx.clone() } else { peer.tx.clone() };
                drop(peers);
                Some(channel.send(encrypted).await.map_err(|_| "Peer channel closed".to_string()))
            } else {
                None
            }
        };

        if let Some(result) = lan_result {
            return result;
        }

        // Fall through to relay
        if let Some(relay) = self.relay.read().await.as_ref() {
            if relay.has_peer(device_id).await {
                return relay.send_to_peer(device_id, msg).await;
            }
        }

        Err(format!("Peer {} not connected via LAN or relay", device_id))
    }

    /// Send an encrypted sync message to specific peers only (filtered by device IDs).
    /// Routes each peer through LAN or relay transparently.
    pub async fn send_to_peers(&self, device_ids: &[String], msg: &SyncMessage) {
        if device_ids.is_empty() {
            return;
        }

        // Send to LAN peers and collect IDs that weren't found on LAN
        let mut relay_targets = Vec::new();
        let is_bulk = msg.is_bulk_data();
        let lan_sends: Vec<(String, mpsc::Sender<Vec<u8>>, Vec<u8>)> = {
            let mut peers = self.peers.write().await;
            let mut sends = Vec::new();
            for device_id in device_ids {
                if let Some(peer) = peers.get_mut(device_id.as_str()) {
                    match encrypt_message(&mut peer.cipher, msg) {
                        Ok(encrypted) => {
                            let channel = if is_bulk { peer.bulk_tx.clone() } else { peer.tx.clone() };
                            sends.push((device_id.clone(), channel, encrypted));
                        }
                        Err(e) => {
                            eprintln!(
                                "[LAN Sync] Failed to encrypt message for peer {}: {}",
                                device_id, e
                            );
                        }
                    }
                } else {
                    relay_targets.push(device_id.clone());
                }
            }
            sends
        }; // write lock dropped

        for (device_id, tx, encrypted) in lan_sends {
            if tx.send(encrypted).await.is_err() {
                eprintln!(
                    "[LAN Sync] Failed to send to peer {} (channel closed)",
                    device_id
                );
            }
        }

        // Send remaining peers through relay
        if !relay_targets.is_empty() {
            if let Some(relay) = self.relay.read().await.as_ref() {
                relay.send_to_peers(&relay_targets, msg).await;
            }
        }
    }

    /// Broadcast an encrypted sync message to all connected peers (LAN + relay)
    pub async fn broadcast(&self, msg: &SyncMessage) {
        let is_bulk = msg.is_bulk_data();
        let msg_desc = match msg {
            SyncMessage::TiddlerChanged { title, .. } => format!("TiddlerChanged({})", title),
            SyncMessage::TiddlerDeleted { title, .. } => format!("TiddlerDeleted({})", title),
            SyncMessage::FullSyncBatch { tiddlers, is_last_batch, .. } => format!("FullSyncBatch({} tiddlers, last={})", tiddlers.len(), is_last_batch),
            _ => String::new(),
        };
        let sends: Vec<(String, mpsc::Sender<Vec<u8>>, Vec<u8>)> = {
            let mut peers = self.peers.write().await;
            if !msg_desc.is_empty() && !peers.is_empty() {
                let peer_ids: Vec<&String> = peers.keys().collect();
                eprintln!("[LAN Sync] broadcast {} to {} peers: {:?}", msg_desc, peers.len(), peer_ids);
            } else if !msg_desc.is_empty() {
                eprintln!("[LAN Sync] broadcast {} — no connected peers!", msg_desc);
            }
            peers
                .iter_mut()
                .filter_map(|(device_id, peer)| {
                    match encrypt_message(&mut peer.cipher, msg) {
                        Ok(encrypted) => {
                            let channel = if is_bulk { peer.bulk_tx.clone() } else { peer.tx.clone() };
                            Some((device_id.clone(), channel, encrypted))
                        }
                        Err(e) => {
                            eprintln!(
                                "[LAN Sync] Failed to encrypt message for peer {}: {}",
                                device_id, e
                            );
                            None
                        }
                    }
                })
                .collect()
        }; // write lock dropped
        for (device_id, tx, encrypted) in sends {
            if tx.send(encrypted).await.is_err() {
                eprintln!(
                    "[LAN Sync] Failed to send to peer {} (channel closed)",
                    device_id
                );
            }
        }

        // Also broadcast to relay peers
        if let Some(relay) = self.relay.read().await.as_ref() {
            let relay_peers = relay.connected_peers().await;
            let relay_ids: Vec<String> = relay_peers.into_iter().map(|(id, _)| id).collect();
            if !relay_ids.is_empty() {
                relay.send_to_peers(&relay_ids, msg).await;
            }
        }
    }

    /// Get list of LAN-only connected peer device IDs
    pub async fn lan_connected_peers(&self) -> Vec<(String, String)> {
        let peers = self.peers.read().await;
        peers
            .iter()
            .map(|(id, p)| (id.clone(), p.device_name.clone()))
            .collect()
    }

    /// Get list of LAN-only connected peers with user_name
    pub async fn lan_connected_peers_full(&self) -> Vec<(String, String, Option<String>)> {
        let peers = self.peers.read().await;
        peers
            .iter()
            .map(|(id, p)| (id.clone(), p.device_name.clone(), p.user_name.clone()))
            .collect()
    }

    /// Set user_name for a specific peer
    pub async fn set_peer_user_name(&self, device_id: &str, user_name: String) {
        let mut peers = self.peers.write().await;
        if let Some(peer) = peers.get_mut(device_id) {
            peer.user_name = Some(user_name);
        }
    }

    /// Get all peer device IDs in a given room (LAN + relay).
    pub async fn peers_for_room(&self, room_code: &str) -> Vec<String> {
        let mut result = Vec::new();
        // LAN peers with matching auth_room_code
        {
            let peers = self.peers.read().await;
            for (id, p) in peers.iter() {
                if p.auth_room_codes.iter().any(|rc| rc == room_code) {
                    result.push(id.clone());
                }
            }
        }
        // Relay peers in this room
        if let Some(relay) = self.relay.read().await.as_ref() {
            let relay_members = relay.get_room_members(room_code).await;
            for id in relay_members {
                if !result.contains(&id) {
                    result.push(id);
                }
            }
        }
        result
    }

    /// Get LAN peers in a specific room with their device names.
    pub async fn lan_peers_for_room(&self, room_code: &str) -> Vec<(String, String)> {
        let peers = self.peers.read().await;
        peers.iter()
            .filter(|(_, p)| p.auth_room_codes.iter().any(|rc| rc == room_code))
            .map(|(id, p)| (id.clone(), p.device_name.clone()))
            .collect()
    }

    /// Get all room codes a LAN peer shares with us.
    pub async fn peer_room_codes(&self, device_id: &str) -> Vec<String> {
        self.peers.read().await
            .get(device_id)
            .map(|pc| pc.auth_room_codes.clone())
            .unwrap_or_default()
    }

    /// Add room codes to a peer's auth_room_codes (deduplicating).
    pub async fn add_peer_room_codes(&self, device_id: &str, new_codes: &[String]) {
        let mut peers = self.peers.write().await;
        if let Some(peer) = peers.get_mut(device_id) {
            for code in new_codes {
                if !peer.auth_room_codes.contains(code) {
                    peer.auth_room_codes.push(code.clone());
                }
            }
        }
    }

    /// Get list of connected peer device IDs (LAN + relay)
    pub async fn connected_peers(&self) -> Vec<(String, String, Option<String>)> {
        let mut all: Vec<(String, String, Option<String>)> = {
            let peers = self.peers.read().await;
            peers
                .iter()
                .map(|(id, p)| (id.clone(), p.device_name.clone(), p.user_name.clone()))
                .collect()
        };

        // Include relay peers (dedup by device_id, prefer LAN)
        if let Some(relay) = self.relay.read().await.as_ref() {
            let relay_peers = relay.connected_peers().await;
            for (id, name) in relay_peers {
                if !all.iter().any(|(aid, _, _)| aid == &id) {
                    all.push((id, name, None));
                }
            }
        }

        all
    }

    /// Disconnect a specific peer
    pub async fn disconnect_peer(&self, device_id: &str) {
        let mut peers = self.peers.write().await;
        if peers.remove(device_id).is_some() {
            eprintln!("[LAN Sync] Disconnected peer {}", device_id);
        }
    }

    /// Gracefully close all peer connections by dropping senders
    /// (which causes outbound tasks to exit and send Close frames).
    /// Waits briefly for connections to close cleanly.
    pub async fn close_all_peers(&self) {
        let peer_ids: Vec<String> = {
            let peers = self.peers.read().await;
            peers.keys().cloned().collect()
        };
        if peer_ids.is_empty() {
            return;
        }
        eprintln!("[LAN Sync] Closing {} peer connections gracefully", peer_ids.len());
        // Drop all peer entries — this closes the tx channels,
        // causing outbound tasks to break out of their loops
        self.peers.write().await.clear();
        // Give outbound tasks a moment to finish and close WebSocket cleanly
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// Handle a single WebSocket connection (server side)
async fn handle_connection(
    stream: tokio::net::TcpStream,
    room_keys: Arc<RwLock<HashMap<String, [u8; 32]>>>,
    our_device_id: &str,
    our_device_name: &str,
    peers: Arc<RwLock<HashMap<String, PeerConnection>>>,
    event_tx: mpsc::UnboundedSender<ServerEvent>,
) -> Result<(), String> {
    let ws_stream = tokio_tungstenite::accept_async_with_config(stream, Some(ws_config()))
        .await
        .map_err(|e| format!("WebSocket handshake failed: {}", e))?;

    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    // Step 1: Receive RoomAuthInit with SPAKE2 message A
    let first_msg = ws_receiver
        .next()
        .await
        .ok_or_else(|| "Connection closed before auth".to_string())?
        .map_err(|e| format!("WebSocket error: {}", e))?;

    let first_text = match first_msg {
        Message::Text(t) => t.to_string(),
        _ => return Err("Expected text message for room auth".to_string()),
    };

    let auth_msg: RoomAuthMessage = serde_json::from_str(&first_text)
        .map_err(|e| format!("Invalid room auth message: {}", e))?;

    let (peer_device_id, peer_device_name, auth_room_code, shared_secret) = match auth_msg {
        RoomAuthMessage::RoomAuthInit {
            device_id,
            device_name,
            room_hash,
            spake_msg,
        } => {
            use base64::{engine::general_purpose::STANDARD, Engine};
            use spake2::{Ed25519Group, Identity, Password, Spake2};

            // Look up group_key by hashing each room code to find match
            let keys = room_keys.read().await;
            let mut found: Option<(String, [u8; 32])> = None;
            for (code, key) in keys.iter() {
                if crate::lan_sync::discovery::hash_room_code(code) == room_hash {
                    found = Some((code.clone(), *key));
                    break;
                }
            }
            drop(keys);

            let (matched_room_code, group_key) = match found {
                Some(pair) => pair,
                None => {
                    let reject = RoomAuthMessage::RoomAuthReject {
                        message: "Unknown room".to_string(),
                    };
                    let _ = ws_sender.send(Message::Text(
                        serde_json::to_string(&reject).unwrap().into(),
                    )).await;
                    return Err("No room matches the provided hash".to_string());
                }
            };

            // Decode client's SPAKE2 message A
            let msg_a_bytes = STANDARD.decode(&spake_msg)
                .map_err(|e| format!("Invalid base64 in spake_msg: {}", e))?;

            // Start SPAKE2 side B with group_key as password
            let (spake_state, msg_b) = Spake2::<Ed25519Group>::start_b(
                &Password::new(&group_key),
                &Identity::new(b"client"),
                &Identity::new(b"server"),
            );

            // Complete SPAKE2 with client's message A → shared secret
            let shared_secret = spake_state.finish(&msg_a_bytes)
                .map_err(|e| format!("SPAKE2 finish failed: {:?}", e))?;

            // Compute server key confirmation and send challenge
            let server_confirm = spake2_server_confirm(&shared_secret);

            let challenge = RoomAuthMessage::RoomAuthChallenge {
                device_id: our_device_id.to_string(),
                device_name: our_device_name.to_string(),
                spake_msg: STANDARD.encode(&msg_b),
                key_confirm: server_confirm,
            };
            let challenge_json = serde_json::to_string(&challenge)
                .map_err(|e| format!("Serialize failed: {}", e))?;
            ws_sender.send(Message::Text(challenge_json.into()))
                .await
                .map_err(|e| format!("Send failed: {}", e))?;

            // Step 2: Receive RoomAuthConfirm from client
            let confirm_frame = ws_receiver
                .next()
                .await
                .ok_or_else(|| "Connection closed before key confirmation".to_string())?
                .map_err(|e| format!("WebSocket error: {}", e))?;

            let confirm_text = match confirm_frame {
                Message::Text(t) => t.to_string(),
                _ => return Err("Expected text message for key confirmation".to_string()),
            };

            let confirm_msg: RoomAuthMessage = serde_json::from_str(&confirm_text)
                .map_err(|e| format!("Invalid key confirmation message: {}", e))?;

            match confirm_msg {
                RoomAuthMessage::RoomAuthConfirm { key_confirm } => {
                    let expected_client_confirm = spake2_client_confirm(&shared_secret);
                    if key_confirm != expected_client_confirm {
                        let reject = RoomAuthMessage::RoomAuthReject {
                            message: "Key confirmation failed".to_string(),
                        };
                        let _ = ws_sender.send(Message::Text(
                            serde_json::to_string(&reject).unwrap().into(),
                        )).await;
                        return Err("Client key confirmation mismatch".to_string());
                    }
                }
                _ => return Err("Expected RoomAuthConfirm message".to_string()),
            }

            eprintln!(
                "[LAN Sync] SPAKE2 auth completed: {} ({}) for room {}",
                device_name, device_id, matched_room_code
            );

            (device_id, device_name, matched_room_code, shared_secret)
        }
        _ => return Err("Expected RoomAuthInit message".to_string()),
    };

    // Step 3: Derive session cipher from SPAKE2 shared secret
    let cipher = SessionCipher::from_spake2_secret(&shared_secret)?;
    let send_cipher = SessionCipher::from_spake2_secret(&shared_secret)?;

    // High-priority channel for tiddler sync messages
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(PEER_CHANNEL_BOUND);
    // Low-priority channel for bulk data (attachment/wiki file chunks)
    let (bulk_tx, mut bulk_rx) = mpsc::channel::<Vec<u8>>(PEER_CHANNEL_BOUND);

    // Register the peer with a unique connection ID
    let conn_id = next_connection_id();
    {
        let mut peers_guard = peers.write().await;
        peers_guard.insert(
            peer_device_id.clone(),
            PeerConnection {
                device_id: peer_device_id.clone(),
                device_name: peer_device_name.clone(),
                connection_id: conn_id,
                tx,
                bulk_tx,
                cipher: send_cipher,
                auth_room_codes: vec![auth_room_code],
                user_name: None,
            },
        );
    }

    let _ = event_tx.send(ServerEvent::PeerConnected {
        device_id: peer_device_id.clone(),
        device_name: peer_device_name.clone(),
    });

    // Step 3: Message loop — handle inbound and outbound concurrently
    let peer_id = peer_device_id.clone();
    let peers_for_cleanup = peers.clone();
    let event_tx_for_cleanup = event_tx.clone();

    // Channel for forwarding Pong responses from inbound to outbound task
    // (split WebSocket streams do NOT auto-respond to Pings)
    let (pong_tx, mut pong_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    // Shared timestamp for last pong received (epoch ms) — used by outbound task
    // to detect dead connections when pongs stop arriving
    let last_pong = Arc::new(AtomicU64::new(epoch_ms()));
    let last_pong_for_inbound = last_pong.clone();

    // Outbound task: send encrypted messages + periodic pings + pong responses.
    // Uses biased select with connection-health messages (pong, ping) at highest
    // priority so they are never starved by sustained data traffic. Tiddler sync
    // messages come next, then bulk attachment data at lowest priority.
    let outbound_peer_id = peer_device_id.clone();
    let outbound = tokio::spawn(async move {
        let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(2));
        ping_interval.tick().await; // skip immediate first tick
        loop {
            tokio::select! {
                biased;
                // 1. Pong responses — highest priority to prevent remote timeout
                Some(pong_data) = pong_rx.recv() => {
                    if ws_sender.send(Message::Pong(pong_data.into())).await.is_err() {
                        break;
                    }
                }
                // 2. Ping keepalive — detect dead connections
                _ = ping_interval.tick() => {
                    let last = last_pong.load(Ordering::Relaxed);
                    let now = epoch_ms();
                    if now.saturating_sub(last) > PONG_TIMEOUT_SECS * 1000 {
                        eprintln!(
                            "[LAN Sync] Pong timeout for peer {} ({}s since last pong) — closing",
                            outbound_peer_id,
                            now.saturating_sub(last) / 1000
                        );
                        break;
                    }
                    if ws_sender.send(Message::Ping(vec![].into())).await.is_err() {
                        break;
                    }
                }
                // 3. High-priority tiddler sync messages
                Some(encrypted) = rx.recv() => {
                    let len = encrypted.len();
                    if let Err(e) = ws_sender.send(Message::Binary(encrypted.into())).await {
                        eprintln!("[LAN Sync] WebSocket send failed for {} ({} bytes): {}", outbound_peer_id, len, e);
                        break;
                    }
                }
                // 4. Low-priority bulk data (attachments, wiki file transfers)
                Some(encrypted) = bulk_rx.recv() => {
                    let len = encrypted.len();
                    if let Err(e) = ws_sender.send(Message::Binary(encrypted.into())).await {
                        eprintln!("[LAN Sync] WebSocket send failed for {} (bulk, {} bytes): {}", outbound_peer_id, len, e);
                        break;
                    }
                }
                else => break,
            }
        }
    });

    // Inbound task: receive and decrypt messages
    let inbound_peer_id = peer_device_id.clone();
    let inbound = tokio::spawn(async move {
        while let Some(msg_result) = ws_receiver.next().await {
            match msg_result {
                Ok(Message::Binary(data)) => {
                    match decrypt_message(&cipher, &data) {
                        Ok(sync_msg) => {
                            let _ = event_tx.send(ServerEvent::SyncMessageReceived {
                                from_device_id: inbound_peer_id.clone(),
                                message: sync_msg,
                            });
                        }
                        Err(e) => {
                            eprintln!(
                                "[LAN Sync] Decrypt error from {}: {}",
                                inbound_peer_id, e
                            );
                        }
                    }
                }
                Ok(Message::Ping(data)) => {
                    // Forward to outbound task for Pong response
                    let _ = pong_tx.send(data.to_vec());
                }
                Ok(Message::Pong(_)) => {
                    // Keepalive response received — update timestamp
                    last_pong_for_inbound.store(epoch_ms(), Ordering::Relaxed);
                }
                Ok(Message::Close(_)) => break,
                Err(e) => {
                    eprintln!("[LAN Sync] WebSocket error from {}: {}", inbound_peer_id, e);
                    break;
                }
                _ => {}
            }
        }
    });

    // Wait for either task to finish (connection close)
    tokio::select! {
        _ = outbound => {},
        _ = inbound => {},
    }

    // Cleanup — only remove if this connection is still the active one.
    // A newer connection may have replaced us in the peers map; removing
    // that would break the live connection.
    let should_emit_disconnect;
    {
        let mut peers_guard = peers_for_cleanup.write().await;
        if peers_guard.get(&peer_id).map(|p| p.connection_id) == Some(conn_id) {
            peers_guard.remove(&peer_id);
            should_emit_disconnect = true;
        } else {
            eprintln!(
                "[LAN Sync] Skipping cleanup for {} (conn_id={}) — superseded by newer connection",
                peer_id, conn_id
            );
            should_emit_disconnect = false;
        }
    }
    if should_emit_disconnect {
        let _ = event_tx_for_cleanup.send(ServerEvent::PeerDisconnected {
            device_id: peer_id,
        });
    }

    Ok(())
}
