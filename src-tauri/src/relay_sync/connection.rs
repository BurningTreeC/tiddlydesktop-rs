//! WebSocket client connection to the relay server.

use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message;

/// Frames received from the relay
#[derive(Debug)]
pub enum RelayFrame {
    /// Opaque binary data (encrypted SyncMessage or session_init)
    Binary(Vec<u8>),
    /// Text control message from relay server (members, member_joined, member_left)
    Control(String),
    /// Server ping received (resets receive timeout — no data to process)
    Heartbeat,
}

/// Sender half — used by RelaySyncManager to send encrypted frames
pub struct RelaySender {
    ws_tx: Arc<Mutex<Option<futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >>>>,
}

impl RelaySender {
    /// Send a binary frame to the relay room
    pub async fn send_binary(&self, data: Vec<u8>) -> Result<(), String> {
        let mut guard = self.ws_tx.lock().await;
        if let Some(ref mut tx) = *guard {
            tx.send(Message::Binary(data.into()))
                .await
                .map_err(|e| format!("Send failed: {}", e))
        } else {
            Err("Connection closed".to_string())
        }
    }

    /// Send a text frame to the relay room (for pairing JSON messages)
    pub async fn send_text(&self, text: String) -> Result<(), String> {
        let mut guard = self.ws_tx.lock().await;
        if let Some(ref mut tx) = *guard {
            tx.send(Message::Text(text.into()))
                .await
                .map_err(|e| format!("Send failed: {}", e))
        } else {
            Err("Connection closed".to_string())
        }
    }

    /// Close the connection
    pub async fn close(&self) {
        let mut guard = self.ws_tx.lock().await;
        if let Some(mut tx) = guard.take() {
            let _ = tx.send(Message::Close(None)).await;
        }
    }
}

/// Receiver half — consumed by the receive loop task
pub struct RelayReceiver {
    rx: mpsc::UnboundedReceiver<RelayFrame>,
}

impl RelayReceiver {
    pub async fn recv(&mut self) -> Option<RelayFrame> {
        self.rx.recv().await
    }
}

/// Connect to a relay room.
///
/// Uses Bearer token for server authentication.
/// Room token (derived from E2E password) is still sent in the join message for
/// end-to-end verification.
///
/// Returns a sender (for outbound) and receiver (for inbound).
pub async fn connect(
    url: &str,
    device_id: &str,
    auth_token: &str,
    auth_provider: &str,
    room_token: Option<&str>,
) -> Result<(RelaySender, RelayReceiver), String> {
    // Build WebSocket request with Bearer token + provider header
    let request = http::Request::builder()
        .uri(url)
        .header("Authorization", format!("Bearer {}", auth_token))
        .header("X-Auth-Provider", auth_provider)
        .header("Host", extract_host(url))
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            tokio_tungstenite::tungstenite::handshake::client::generate_key(),
        )
        .body(())
        .map_err(|e| format!("Failed to build request: {}", e))?;

    let (ws_stream, _response) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|e| format!("WebSocket connection failed: {}", e))?;

    let (ws_tx, mut ws_rx) = ws_stream.split();
    let ws_tx = Arc::new(Mutex::new(Some(ws_tx)));

    // Send join message with room token (for E2E key verification)
    {
        let mut guard = ws_tx.lock().await;
        if let Some(ref mut tx) = *guard {
            let mut join_msg = serde_json::json!({
                "type": "join",
                "deviceId": device_id
            });
            if let Some(token) = room_token {
                join_msg["roomToken"] = serde_json::Value::String(token.to_string());
            }
            tx.send(Message::Text(join_msg.to_string().into()))
                .await
                .map_err(|e| format!("Failed to send join: {}", e))?;
        }
    }

    // Create frame channel
    let (frame_tx, frame_rx) = mpsc::unbounded_channel();

    // Spawn receive task that forwards WebSocket frames to the channel
    tokio::spawn(async move {
        while let Some(msg_result) = ws_rx.next().await {
            match msg_result {
                Ok(Message::Binary(data)) => {
                    if frame_tx.send(RelayFrame::Binary(data.to_vec())).is_err() {
                        break;
                    }
                }
                Ok(Message::Text(text)) => {
                    if frame_tx
                        .send(RelayFrame::Control(text.to_string()))
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(Message::Close(_)) => break,
                Ok(Message::Ping(_) | Message::Pong(_)) => {
                    // Forward as heartbeat so receive timeout resets
                    if frame_tx.send(RelayFrame::Heartbeat).is_err() {
                        break;
                    }
                }
                Ok(Message::Frame(_)) => {}
                Err(e) => {
                    eprintln!("[Relay] WebSocket receive error: {}", e);
                    break;
                }
            }
        }
        // Channel drops naturally when this task ends, signaling receiver
    });

    Ok((
        RelaySender { ws_tx },
        RelayReceiver { rx: frame_rx },
    ))
}

fn extract_host(url: &str) -> String {
    url.trim_start_matches("ws://")
        .trim_start_matches("wss://")
        .split('/')
        .next()
        .unwrap_or("localhost")
        .to_string()
}
