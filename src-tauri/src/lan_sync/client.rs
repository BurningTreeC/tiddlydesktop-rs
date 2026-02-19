//! WebSocket client for LAN sync.
//!
//! Connects to a discovered peer's WebSocket server, performs pairing or
//! authentication, then enters encrypted sync mode.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{mpsc, RwLock};
use tokio_tungstenite::tungstenite::Message;
use futures_util::{SinkExt, StreamExt};

use super::server::{epoch_ms, PONG_TIMEOUT_SECS};

use super::pairing::PairingManager;
use super::protocol::*;
use super::server::{PeerConnection, ServerEvent, next_connection_id};

/// Connect to a peer's WebSocket server
pub async fn connect_to_peer(
    addr: &str,
    port: u16,
    pairing_manager: Arc<PairingManager>,
    peers: Arc<RwLock<HashMap<String, PeerConnection>>>,
    event_tx: mpsc::UnboundedSender<ServerEvent>,
    pin: Option<&str>,
) -> Result<(), String> {
    let url = format!("ws://{}:{}", addr, port);
    eprintln!("[LAN Sync] Connecting to {}", url);

    let (ws_stream, _) = tokio_tungstenite::connect_async_with_config(&url, Some(super::server::ws_config()), false)
        .await
        .map_err(|e| format!("WebSocket connect failed: {}", e))?;

    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    // Step 1: Send PairingInit
    let spake2_msg = if let Some(pin) = pin {
        // New pairing with PIN
        pairing_manager.start_pairing_as_enterer(pin)
    } else {
        // Reconnection to already-paired device — send empty SPAKE2 msg
        Vec::new()
    };

    let init_msg = PairingMessage::PairingInit {
        device_id: pairing_manager.device_id().to_string(),
        device_name: pairing_manager.device_name().to_string(),
        spake2_msg,
    };
    let init_json =
        serde_json::to_string(&init_msg).map_err(|e| format!("Serialize failed: {}", e))?;
    ws_sender
        .send(Message::Text(init_json.into()))
        .await
        .map_err(|e| format!("Send failed: {}", e))?;

    // Step 2: Handle pairing response (if new pairing)
    let (peer_device_id, peer_device_name, long_term_key) = if pin.is_some() {
        // Receive PairingResponse
        let response_frame = ws_receiver
            .next()
            .await
            .ok_or_else(|| "Connection closed during pairing".to_string())?
            .map_err(|e| format!("WebSocket error: {}", e))?;

        let response_text = match response_frame {
            Message::Text(t) => t.to_string(),
            _ => return Err("Expected text message for pairing response".to_string()),
        };

        let response: PairingMessage = serde_json::from_str(&response_text)
            .map_err(|e| format!("Invalid pairing response: {}", e))?;

        let (peer_id, peer_name, peer_spake2) = match response {
            PairingMessage::PairingResponse {
                device_id,
                device_name,
                spake2_msg,
            } => (device_id, device_name, spake2_msg),
            _ => return Err("Expected PairingResponse".to_string()),
        };

        // Process SPAKE2 and derive key
        let (_outbound, key) = pairing_manager.process_spake2_message(&peer_spake2)?;

        // Receive server's confirmation
        let server_confirm_frame = ws_receiver
            .next()
            .await
            .ok_or_else(|| "Connection closed during confirmation".to_string())?
            .map_err(|e| format!("WebSocket error: {}", e))?;

        let server_confirm_text = match server_confirm_frame {
            Message::Text(t) => t.to_string(),
            _ => return Err("Expected text for confirmation".to_string()),
        };

        let server_confirm: PairingMessage = serde_json::from_str(&server_confirm_text)
            .map_err(|e| format!("Invalid confirmation: {}", e))?;

        match server_confirm {
            PairingMessage::PairingConfirm { confirmation_hmac } => {
                if !pairing_manager.verify_confirmation(&key, &peer_id, &confirmation_hmac) {
                    return Err("Server confirmation failed - wrong PIN?".to_string());
                }
            }
            _ => return Err("Expected PairingConfirm from server".to_string()),
        }

        // Send our confirmation
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

        // Receive pairing result
        let result_frame = ws_receiver
            .next()
            .await
            .ok_or_else(|| "Connection closed waiting for pairing result".to_string())?
            .map_err(|e| format!("WebSocket error: {}", e))?;

        let result_text = match result_frame {
            Message::Text(t) => t.to_string(),
            _ => return Err("Expected text for pairing result".to_string()),
        };

        let result: PairingMessage = serde_json::from_str(&result_text)
            .map_err(|e| format!("Invalid pairing result: {}", e))?;

        match result {
            PairingMessage::PairingResult { success, message } => {
                if !success {
                    return Err(format!(
                        "Pairing failed: {}",
                        message.unwrap_or_default()
                    ));
                }
            }
            _ => return Err("Expected PairingResult".to_string()),
        }

        // Store pairing
        pairing_manager.complete_pairing(&peer_id, &peer_name, &key)?;

        let _ = event_tx.send(ServerEvent::PairingCompleted {
            device_id: peer_id.clone(),
            device_name: peer_name.clone(),
        });

        (peer_id, peer_name, key)
    } else {
        // Reconnection — we need the peer's identity from the next messages
        // The server will recognize us as paired and skip SPAKE2
        // Receive session nonce directly (server skips pairing exchange for paired devices)
        // For reconnection, we need to figure out the peer's identity
        // We'll get it from the subsequent message flow
        return Err("Reconnection requires knowing the peer device_id - use connect_to_paired_peer instead".to_string());
    };

    // Step 3: Receive session nonce and establish encrypted session
    let nonce_frame = ws_receiver
        .next()
        .await
        .ok_or_else(|| "Connection closed before session nonce".to_string())?
        .map_err(|e| format!("WebSocket error: {}", e))?;

    let session_nonce = match nonce_frame {
        Message::Binary(data) => data.to_vec(),
        _ => return Err("Expected binary message for session nonce".to_string()),
    };

    let cipher = SessionCipher::new(&long_term_key, &session_nonce)?;
    let send_cipher = SessionCipher::new(&long_term_key, &session_nonce)?;

    // High-priority channel for tiddler sync messages
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(super::server::PEER_CHANNEL_BOUND);
    // Low-priority channel for bulk data (attachment/wiki file chunks)
    let (bulk_tx, mut bulk_rx) = mpsc::channel::<Vec<u8>>(super::server::PEER_CHANNEL_BOUND);

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

    // Step 4: Message loop
    let peer_id = peer_device_id.clone();
    let peers_for_cleanup = peers.clone();
    let event_tx_for_cleanup = event_tx.clone();

    // Channel for forwarding Pong responses from inbound to outbound task
    let (pong_tx, mut pong_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    let last_pong = Arc::new(AtomicU64::new(epoch_ms()));
    let last_pong_for_inbound = last_pong.clone();

    let outbound_peer_id = peer_device_id.clone();
    let outbound = tokio::spawn(async move {
        let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(2));
        ping_interval.tick().await;
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

    let inbound_peer_id = peer_device_id.clone();
    let inbound = tokio::spawn(async move {
        while let Some(msg_result) = ws_receiver.next().await {
            match msg_result {
                Ok(Message::Binary(data)) => match decrypt_message(&cipher, &data) {
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
                },
                Ok(Message::Ping(data)) => {
                    let _ = pong_tx.send(data.to_vec());
                }
                Ok(Message::Pong(_)) => {
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

    tokio::select! {
        _ = outbound => {},
        _ = inbound => {},
    }

    // Cleanup — only remove if this connection is still the active one
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

/// Connect to an already-paired peer (reconnection)
pub async fn connect_to_paired_peer(
    addr: &str,
    port: u16,
    peer_device_id: &str,
    pairing_manager: Arc<PairingManager>,
    peers: Arc<RwLock<HashMap<String, PeerConnection>>>,
    event_tx: mpsc::UnboundedSender<ServerEvent>,
) -> Result<(), String> {
    let long_term_key = pairing_manager
        .get_shared_secret(peer_device_id)
        .ok_or_else(|| format!("No shared secret for device {}", peer_device_id))?;

    let url = format!("ws://{}:{}", addr, port);
    eprintln!(
        "[LAN Sync] Reconnecting to paired device {} at {}",
        peer_device_id, url
    );

    let (ws_stream, _) = tokio_tungstenite::connect_async_with_config(&url, Some(super::server::ws_config()), false)
        .await
        .map_err(|e| format!("WebSocket connect failed: {}", e))?;

    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    // Send PairingInit with empty SPAKE2 (signals reconnection)
    let init_msg = PairingMessage::PairingInit {
        device_id: pairing_manager.device_id().to_string(),
        device_name: pairing_manager.device_name().to_string(),
        spake2_msg: Vec::new(),
    };
    let init_json =
        serde_json::to_string(&init_msg).map_err(|e| format!("Serialize failed: {}", e))?;
    ws_sender
        .send(Message::Text(init_json.into()))
        .await
        .map_err(|e| format!("Send failed: {}", e))?;

    // Receive session nonce
    let nonce_frame = ws_receiver
        .next()
        .await
        .ok_or_else(|| "Connection closed before session nonce".to_string())?
        .map_err(|e| format!("WebSocket error: {}", e))?;

    let session_nonce = match nonce_frame {
        Message::Binary(data) => data.to_vec(),
        _ => return Err("Expected binary message for session nonce".to_string()),
    };

    let cipher = SessionCipher::new(&long_term_key, &session_nonce)?;
    let send_cipher = SessionCipher::new(&long_term_key, &session_nonce)?;

    // High-priority channel for tiddler sync messages
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(super::server::PEER_CHANNEL_BOUND);
    // Low-priority channel for bulk data (attachment/wiki file chunks)
    let (bulk_tx, mut bulk_rx) = mpsc::channel::<Vec<u8>>(super::server::PEER_CHANNEL_BOUND);

    // Get peer's device name from stored pairing
    let peer_device_name = pairing_manager
        .get_paired_devices()
        .iter()
        .find(|d| d.device_id == peer_device_id)
        .map(|d| d.device_name.clone())
        .unwrap_or_else(|| "Unknown".to_string());

    let conn_id = next_connection_id();
    {
        let mut peers_guard = peers.write().await;
        peers_guard.insert(
            peer_device_id.to_string(),
            PeerConnection {
                device_id: peer_device_id.to_string(),
                device_name: peer_device_name.clone(),
                connection_id: conn_id,
                tx,
                bulk_tx,
                cipher: send_cipher,
            },
        );
    }

    let _ = event_tx.send(ServerEvent::PeerConnected {
        device_id: peer_device_id.to_string(),
        device_name: peer_device_name,
    });

    let peer_id = peer_device_id.to_string();
    let peers_for_cleanup = peers.clone();
    let event_tx_for_cleanup = event_tx.clone();

    // Channel for forwarding Pong responses from inbound to outbound task
    let (pong_tx, mut pong_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    let last_pong = Arc::new(AtomicU64::new(epoch_ms()));
    let last_pong_for_inbound = last_pong.clone();

    let outbound_peer_id = peer_device_id.to_string();
    let outbound = tokio::spawn(async move {
        let mut ping_interval = tokio::time::interval(std::time::Duration::from_secs(2));
        ping_interval.tick().await;
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

    let inbound_peer_id = peer_device_id.to_string();
    let inbound = tokio::spawn(async move {
        while let Some(msg_result) = ws_receiver.next().await {
            match msg_result {
                Ok(Message::Binary(data)) => match decrypt_message(&cipher, &data) {
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
                },
                Ok(Message::Ping(data)) => {
                    let _ = pong_tx.send(data.to_vec());
                }
                Ok(Message::Pong(_)) => {
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

    tokio::select! {
        _ = outbound => {},
        _ = inbound => {},
    }

    // Cleanup — only remove if this connection is still the active one
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
