//! WebSocket client for LAN sync.
//!
//! Connects to a discovered peer's WebSocket server using room-based
//! authentication, then enters encrypted sync mode.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{mpsc, RwLock};
use tokio_tungstenite::tungstenite::Message;
use futures_util::{SinkExt, StreamExt};

use super::server::{epoch_ms, PONG_TIMEOUT_SECS};

use super::protocol::*;
use super::server::{PeerConnection, ServerEvent, next_connection_id};

/// Connect to a peer's WebSocket server using room-based authentication.
/// The group_key is derived from the shared room password + room code.
pub async fn connect_to_room_peer(
    addr: &str,
    port: u16,
    peer_device_id: &str,
    our_device_id: &str,
    our_device_name: &str,
    room_code: &str,
    group_key: &[u8; 32],
    peers: Arc<RwLock<HashMap<String, PeerConnection>>>,
    event_tx: mpsc::UnboundedSender<ServerEvent>,
) -> Result<(), String> {
    let url = format!("ws://{}:{}", addr, port);
    eprintln!(
        "[LAN Sync] Connecting to room peer {} at {} (room {})",
        peer_device_id, url, room_code
    );

    let (ws_stream, _) = tokio_tungstenite::connect_async_with_config(&url, Some(super::server::ws_config()), false)
        .await
        .map_err(|e| format!("WebSocket connect failed: {}", e))?;

    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    // Step 1: Send RoomAuthInit with room token proof
    let room_token = crate::relay_sync::RelaySyncManager::derive_room_token_from_key(group_key);
    let init_msg = RoomAuthMessage::RoomAuthInit {
        device_id: our_device_id.to_string(),
        device_name: our_device_name.to_string(),
        room_code: room_code.to_string(),
        room_token,
    };
    let init_json = serde_json::to_string(&init_msg)
        .map_err(|e| format!("Serialize failed: {}", e))?;
    ws_sender.send(Message::Text(init_json.into()))
        .await
        .map_err(|e| format!("Send failed: {}", e))?;

    // Step 2: Receive RoomAuthAccept or RoomAuthReject
    let response_frame = ws_receiver
        .next()
        .await
        .ok_or_else(|| "Connection closed during room auth".to_string())?
        .map_err(|e| format!("WebSocket error: {}", e))?;

    let response_text = match response_frame {
        Message::Text(t) => t.to_string(),
        _ => return Err("Expected text message for room auth response".to_string()),
    };

    let response: RoomAuthMessage = serde_json::from_str(&response_text)
        .map_err(|e| format!("Invalid room auth response: {}", e))?;

    let (peer_id_confirmed, peer_name_confirmed) = match response {
        RoomAuthMessage::RoomAuthAccept { device_id, device_name } => {
            eprintln!("[LAN Sync] Room auth accepted by {} ({})", device_name, device_id);
            (device_id, device_name)
        }
        RoomAuthMessage::RoomAuthReject { message } => {
            return Err(format!("Room auth rejected: {}", message));
        }
        _ => return Err("Expected RoomAuthAccept or RoomAuthReject".to_string()),
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

    let cipher = SessionCipher::new(group_key, &session_nonce)?;
    let send_cipher = SessionCipher::new(group_key, &session_nonce)?;

    // High-priority channel for tiddler sync messages
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(super::server::PEER_CHANNEL_BOUND);
    // Low-priority channel for bulk data (attachment/wiki file chunks)
    let (bulk_tx, mut bulk_rx) = mpsc::channel::<Vec<u8>>(super::server::PEER_CHANNEL_BOUND);

    let conn_id = next_connection_id();
    {
        let mut peers_guard = peers.write().await;
        peers_guard.insert(
            peer_id_confirmed.clone(),
            PeerConnection {
                device_id: peer_id_confirmed.clone(),
                device_name: peer_name_confirmed.clone(),
                connection_id: conn_id,
                tx,
                bulk_tx,
                cipher: send_cipher,
                auth_room_code: Some(room_code.to_string()),
                user_name: None,
            },
        );
    }

    let _ = event_tx.send(ServerEvent::PeerConnected {
        device_id: peer_id_confirmed.clone(),
        device_name: peer_name_confirmed.clone(),
    });

    let peer_id = peer_id_confirmed.clone();
    let peers_for_cleanup = peers.clone();
    let event_tx_for_cleanup = event_tx.clone();

    // Channel for forwarding Pong responses from inbound to outbound task
    let (pong_tx, mut pong_rx) = mpsc::unbounded_channel::<Vec<u8>>();

    let last_pong = Arc::new(AtomicU64::new(epoch_ms()));
    let last_pong_for_inbound = last_pong.clone();

    let outbound_peer_id = peer_id_confirmed.clone();
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

    let inbound_peer_id = peer_id_confirmed.clone();
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
