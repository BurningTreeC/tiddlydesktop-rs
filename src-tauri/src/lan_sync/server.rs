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

use super::pairing::PairingManager;
use super::protocol::*;

/// Max buffered encrypted messages per peer (backpressure for large transfers).
/// 16 × ~1.4MB ≈ 22MB max buffered per peer — sufficient pipeline depth
/// for sustained throughput without excessive memory use.
pub const PEER_CHANNEL_BOUND: usize = 16;

/// If no pong is received within this duration after a ping, consider the connection dead.
pub const PONG_TIMEOUT_SECS: u64 = 60;

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
    /// Pairing request from a new device
    PairingRequested {
        device_id: String,
        device_name: String,
    },
    /// Pairing completed successfully
    PairingCompleted {
        device_id: String,
        device_name: String,
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
}

impl SyncServer {
    /// Start the WebSocket server, trying ports in the configured range.
    /// The `peers` map is shared with client-initiated connections so both
    /// inbound and outbound peers are tracked in the same place.
    pub async fn start(
        pairing_manager: Arc<PairingManager>,
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

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok((stream, addr)) => {
                                eprintln!("[LAN Sync] New connection from {}", addr);
                                let pairing_mgr = pairing_manager.clone();
                                let peers = peers_clone.clone();
                                let event_tx = event_tx_clone.clone();

                                tokio::spawn(async move {
                                    if let Err(e) =
                                        handle_connection(stream, pairing_mgr, peers, event_tx).await
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
        })
    }

    /// Get the port the server is listening on
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Send an encrypted sync message to a specific peer.
    /// Uses a bounded channel so the caller blocks when the WebSocket
    /// can't keep up — this is critical for large file transfers.
    pub async fn send_to_peer(
        &self,
        device_id: &str,
        msg: &SyncMessage,
    ) -> Result<(), String> {
        let is_bulk = msg.is_bulk_data();
        let (tx, encrypted) = {
            let mut peers = self.peers.write().await;
            let peer = peers
                .get_mut(device_id)
                .ok_or_else(|| format!("Peer {} not connected", device_id))?;
            let encrypted = encrypt_message(&mut peer.cipher, msg)?;
            let channel = if is_bulk { peer.bulk_tx.clone() } else { peer.tx.clone() };
            (channel, encrypted)
        }; // write lock dropped — safe to await
        tx.send(encrypted)
            .await
            .map_err(|_| "Peer channel closed".to_string())
    }

    /// Broadcast an encrypted sync message to all connected peers
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
    }

    /// Get list of connected peer device IDs
    pub async fn connected_peers(&self) -> Vec<(String, String)> {
        let peers = self.peers.read().await;
        peers
            .iter()
            .map(|(id, p)| (id.clone(), p.device_name.clone()))
            .collect()
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
    pairing_manager: Arc<PairingManager>,
    peers: Arc<RwLock<HashMap<String, PeerConnection>>>,
    event_tx: mpsc::UnboundedSender<ServerEvent>,
) -> Result<(), String> {
    let ws_stream = tokio_tungstenite::accept_async_with_config(stream, Some(ws_config()))
        .await
        .map_err(|e| format!("WebSocket handshake failed: {}", e))?;

    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    // Step 1: Receive PairingInit or auth with existing pairing
    let first_msg = ws_receiver
        .next()
        .await
        .ok_or_else(|| "Connection closed before pairing".to_string())?
        .map_err(|e| format!("WebSocket error: {}", e))?;

    let first_text = match first_msg {
        Message::Text(t) => t.to_string(),
        _ => return Err("Expected text message for pairing".to_string()),
    };

    let pairing_msg: PairingMessage = serde_json::from_str(&first_text)
        .map_err(|e| format!("Invalid pairing message: {}", e))?;

    let (peer_device_id, peer_device_name, long_term_key) = match pairing_msg {
        PairingMessage::PairingInit {
            device_id,
            device_name,
            spake2_msg,
        } => {
            // Check if this device is already paired
            if let Some(key) = pairing_manager.get_shared_secret(&device_id) {
                // Already paired — this is a reconnection auth
                // Verify by generating session with known key
                eprintln!(
                    "[LAN Sync] Reconnection from paired device: {} ({})",
                    device_name, device_id
                );
                (device_id, device_name, key)
            } else {
                // New device — need PIN pairing
                eprintln!(
                    "[LAN Sync] New pairing request from: {} ({})",
                    device_name, device_id
                );

                // Notify the sync manager about the pairing request
                let _ = event_tx.send(ServerEvent::PairingRequested {
                    device_id: device_id.clone(),
                    device_name: device_name.clone(),
                });

                // Process SPAKE2
                let (outbound, key) =
                    pairing_manager.process_spake2_message(&spake2_msg)?;

                // Send our SPAKE2 response
                let response = PairingMessage::PairingResponse {
                    device_id: pairing_manager.device_id().to_string(),
                    device_name: pairing_manager.device_name().to_string(),
                    spake2_msg: outbound.unwrap_or_default(),
                };
                let response_json = serde_json::to_string(&response)
                    .map_err(|e| format!("Serialize failed: {}", e))?;
                ws_sender
                    .send(Message::Text(response_json.into()))
                    .await
                    .map_err(|e| format!("Send failed: {}", e))?;

                // Exchange confirmation HMACs
                let our_confirm = pairing_manager.generate_confirmation(&key);
                let confirm_msg = PairingMessage::PairingConfirm {
                    confirmation_hmac: our_confirm,
                };
                let confirm_json = serde_json::to_string(&confirm_msg)
                    .map_err(|e| format!("Serialize failed: {}", e))?;
                ws_sender
                    .send(Message::Text(confirm_json.into()))
                    .await
                    .map_err(|e| format!("Send failed: {}", e))?;

                // Receive peer's confirmation
                let confirm_frame = ws_receiver
                    .next()
                    .await
                    .ok_or_else(|| "Connection closed during confirmation".to_string())?
                    .map_err(|e| format!("WebSocket error: {}", e))?;

                let confirm_text = match confirm_frame {
                    Message::Text(t) => t.to_string(),
                    _ => return Err("Expected text message for confirmation".to_string()),
                };

                let peer_confirm: PairingMessage = serde_json::from_str(&confirm_text)
                    .map_err(|e| format!("Invalid confirmation: {}", e))?;

                match peer_confirm {
                    PairingMessage::PairingConfirm { confirmation_hmac } => {
                        if !pairing_manager.verify_confirmation(
                            &key,
                            &device_id,
                            &confirmation_hmac,
                        ) {
                            let result = PairingMessage::PairingResult {
                                success: false,
                                message: Some("Confirmation failed - wrong PIN?".to_string()),
                            };
                            let _ = ws_sender
                                .send(Message::Text(
                                    serde_json::to_string(&result).unwrap().into(),
                                ))
                                .await;
                            return Err("Pairing confirmation failed".to_string());
                        }
                    }
                    _ => return Err("Expected PairingConfirm message".to_string()),
                }

                // Pairing successful — store it
                pairing_manager.complete_pairing(&device_id, &device_name, &key)?;

                let result = PairingMessage::PairingResult {
                    success: true,
                    message: None,
                };
                let _ = ws_sender
                    .send(Message::Text(
                        serde_json::to_string(&result).unwrap().into(),
                    ))
                    .await;

                let _ = event_tx.send(ServerEvent::PairingCompleted {
                    device_id: device_id.clone(),
                    device_name: device_name.clone(),
                });

                (device_id, device_name, key)
            }
        }
        _ => return Err("Expected PairingInit message".to_string()),
    };

    // Step 2: Establish encrypted session
    let session_nonce: [u8; 32] = rand::random();
    // Send session nonce as binary frame
    ws_sender
        .send(Message::Binary(session_nonce.to_vec().into()))
        .await
        .map_err(|e| format!("Send session nonce failed: {}", e))?;

    let cipher = SessionCipher::new(&long_term_key, &session_nonce)?;
    let send_cipher = SessionCipher::new(&long_term_key, &session_nonce)?;

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
        let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(20));
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
