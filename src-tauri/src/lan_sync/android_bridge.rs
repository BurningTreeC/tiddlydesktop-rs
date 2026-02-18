//! Android bridge HTTP server for cross-process LAN sync communication.
//!
//! On Android, wiki windows run in the `:wiki` process while LAN sync runs in the
//! main Tauri process. This bridge provides an HTTP API on localhost that the `:wiki`
//! process can call to send/receive tiddler changes.
//!
//! Outbound (wiki → sync): POST endpoints feed changes into the wiki_tx channel
//! Inbound (sync → wiki): Changes are queued per wiki_id for JS polling via GET

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::Manager;
use tokio::sync::mpsc;

use super::bridge::WikiToSync;
use super::GLOBAL_APP_HANDLE;

/// Maximum body size for bridge requests.
/// Full sync batches can be very large (32 tiddlers with embedded images = 8+ MB),
/// so 64 MB gives plenty of headroom.
const MAX_BODY_SIZE: usize = 64 * 1024 * 1024;

/// The Android bridge server
pub struct AndroidBridge {
    port: u16,
    shutdown: Arc<AtomicBool>,
    /// Pending inbound changes per wiki_id for JS to poll
    pending: Arc<Mutex<HashMap<String, VecDeque<serde_json::Value>>>>,
}

impl AndroidBridge {
    /// Start the bridge HTTP server on a random localhost port.
    /// Returns the bridge with the actual port.
    pub fn start(wiki_tx: mpsc::UnboundedSender<WikiToSync>) -> Result<Self, String> {
        let server = tiny_http::Server::http("127.0.0.1:0")
            .map_err(|e| format!("Failed to start bridge server: {}", e))?;

        let port = server
            .server_addr()
            .to_ip()
            .map(|a| a.port())
            .unwrap_or(0);

        if port == 0 {
            return Err("Bridge server bound to port 0".to_string());
        }

        let pending: Arc<Mutex<HashMap<String, VecDeque<serde_json::Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let shutdown = Arc::new(AtomicBool::new(false));

        // Write port to file so WikiActivity in :wiki process can read it
        if let Some(app) = GLOBAL_APP_HANDLE.get() {
            if let Ok(data_dir) = app.path().app_data_dir() {
                let port_file = data_dir.join("sync_bridge_port");
                let _ = std::fs::write(&port_file, port.to_string());
                eprintln!("[Android Bridge] Wrote port {} to {:?}", port, port_file);
            }
        }

        // Spawn the request handler thread — server is moved into the thread
        let pending_clone = pending.clone();
        let shutdown_clone = shutdown.clone();

        std::thread::Builder::new()
            .name("android-bridge".into())
            .spawn(move || {
                run_bridge_server(server, wiki_tx, pending_clone, shutdown_clone);
            })
            .map_err(|e| format!("Failed to spawn bridge thread: {}", e))?;

        eprintln!("[Android Bridge] Started on port {}", port);

        Ok(AndroidBridge {
            port,
            shutdown,
            pending,
        })
    }

    /// Get the bridge port
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Queue an inbound change for a wiki (called from the emit task)
    pub fn queue_change(&self, wiki_id: &str, payload: serde_json::Value) {
        if let Ok(mut map) = self.pending.lock() {
            map.entry(wiki_id.to_string())
                .or_insert_with(VecDeque::new)
                .push_back(payload);
        }
    }

    /// Stop the bridge server
    pub fn stop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // Send a dummy request to unblock recv_timeout faster
        let _ = std::net::TcpStream::connect(format!("127.0.0.1:{}", self.port));
        eprintln!("[Android Bridge] Stopped");

        // Remove port file
        if let Some(app) = GLOBAL_APP_HANDLE.get() {
            if let Ok(data_dir) = app.path().app_data_dir() {
                let _ = std::fs::remove_file(data_dir.join("sync_bridge_port"));
            }
        }
    }
}

impl Drop for AndroidBridge {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Read request body as string (with size limit)
fn read_body(request: &mut tiny_http::Request) -> Option<String> {
    let content_length = request.body_length().unwrap_or(0);
    if content_length > MAX_BODY_SIZE {
        eprintln!("[Android Bridge] Body too large: {} bytes (max {})", content_length, MAX_BODY_SIZE);
        return None;
    }
    let mut body = String::new();
    request
        .as_reader()
        .take(MAX_BODY_SIZE as u64)
        .read_to_string(&mut body)
        .ok()?;
    Some(body)
}

/// Add CORS headers to a response
fn cors_response(data: &str, status: u16) -> tiny_http::Response<std::io::Cursor<Vec<u8>>> {
    let bytes = data.as_bytes().to_vec();
    tiny_http::Response::new(
        tiny_http::StatusCode(status),
        vec![
            tiny_http::Header::from_bytes("Access-Control-Allow-Origin", "*").unwrap(),
            tiny_http::Header::from_bytes("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
                .unwrap(),
            tiny_http::Header::from_bytes("Access-Control-Allow-Headers", "Content-Type").unwrap(),
            tiny_http::Header::from_bytes("Content-Type", "application/json").unwrap(),
        ],
        std::io::Cursor::new(bytes),
        Some(data.len()),
        None,
    )
}

/// Main request handler loop
fn run_bridge_server(
    server: tiny_http::Server,
    wiki_tx: mpsc::UnboundedSender<WikiToSync>,
    pending: Arc<Mutex<HashMap<String, VecDeque<serde_json::Value>>>>,
    shutdown: Arc<AtomicBool>,
) {
    loop {
        if shutdown.load(Ordering::Relaxed) {
            break;
        }

        // Use recv_timeout so we can check the shutdown flag periodically
        let request = match server.recv_timeout(Duration::from_millis(500)) {
            Ok(Some(req)) => req,
            Ok(None) => continue,  // timeout, loop back to check shutdown
            Err(_) => break,       // server error, exit
        };

        let mut request = request;
        let url = request.url().to_string();
        let method = request.method().to_string();

        // Handle CORS preflight
        if method == "OPTIONS" {
            let _ = request.respond(cors_response("", 204));
            continue;
        }

        match (method.as_str(), url.as_str()) {
            // ── Outbound: wiki process → sync module ──────────────────

            ("POST", "/_bridge/tiddler-changed") => {
                if let Some(body) = read_body(&mut request) {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
                        let wiki_id = json["wiki_id"].as_str().unwrap_or("").to_string();
                        let title = json["title"].as_str().unwrap_or("").to_string();
                        let tiddler_json =
                            json["tiddler_json"].as_str().unwrap_or("").to_string();
                        if !wiki_id.is_empty() && !title.is_empty() {
                            let _ = wiki_tx.send(WikiToSync::TiddlerChanged {
                                wiki_id,
                                title,
                                tiddler_json,
                            });
                        }
                    }
                }
                let _ = request.respond(cors_response("{\"ok\":true}", 200));
            }

            ("POST", "/_bridge/tiddler-deleted") => {
                if let Some(body) = read_body(&mut request) {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
                        let wiki_id = json["wiki_id"].as_str().unwrap_or("").to_string();
                        let title = json["title"].as_str().unwrap_or("").to_string();
                        if !wiki_id.is_empty() && !title.is_empty() {
                            let _ =
                                wiki_tx.send(WikiToSync::TiddlerDeleted { wiki_id, title });
                        }
                    }
                }
                let _ = request.respond(cors_response("{\"ok\":true}", 200));
            }

            ("POST", "/_bridge/wiki-opened") => {
                if let Some(body) = read_body(&mut request) {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
                        let wiki_id = json["wiki_id"].as_str().unwrap_or("").to_string();
                        if !wiki_id.is_empty() {
                            // Get wiki info from recent files
                            if let Some(app) = GLOBAL_APP_HANDLE.get() {
                                let wikis =
                                    crate::wiki_storage::get_sync_enabled_wikis(app);
                                if let Some((_, wiki_name, is_folder)) =
                                    wikis.iter().find(|(id, _, _)| id == &wiki_id)
                                {
                                    let _ = wiki_tx.send(WikiToSync::WikiOpened {
                                        wiki_id,
                                        wiki_name: wiki_name.clone(),
                                        is_folder: *is_folder,
                                    });
                                }
                            }
                        }
                    }
                }
                let _ = request.respond(cors_response("{\"ok\":true}", 200));
            }

            ("POST", "/_bridge/full-sync-batch") => {
                if let Some(body) = read_body(&mut request) {
                    eprintln!("[Android Bridge] full-sync-batch: body_len={}", body.len());
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
                        let wiki_id = json["wiki_id"].as_str().unwrap_or("").to_string();
                        let to_device_id =
                            json["to_device_id"].as_str().unwrap_or("").to_string();
                        let is_last_batch =
                            json["is_last_batch"].as_bool().unwrap_or(false);

                        let tiddlers: Vec<super::TiddlerBatch> = json["tiddlers"]
                            .as_array()
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|t| {
                                        Some(super::TiddlerBatch {
                                            title: t["title"].as_str()?.to_string(),
                                            tiddler_json: t["tiddler_json"]
                                                .as_str()?
                                                .to_string(),
                                        })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();

                        eprintln!("[Android Bridge] full-sync-batch: wiki_id={}, to_device_id={}, tiddlers={}, is_last={}", wiki_id, to_device_id, tiddlers.len(), is_last_batch);
                        if !wiki_id.is_empty() && !to_device_id.is_empty() {
                            if let Some(mgr) = super::get_sync_manager() {
                                let mgr = mgr.clone();
                                tauri::async_runtime::spawn(async move {
                                    if let Err(e) = mgr
                                        .send_full_sync_batch(
                                            &wiki_id,
                                            &to_device_id,
                                            tiddlers,
                                            is_last_batch,
                                        )
                                        .await
                                    {
                                        eprintln!(
                                            "[Android Bridge] Full sync batch error: {}",
                                            e
                                        );
                                    }
                                });
                            }
                        }
                    }
                }
                let _ = request.respond(cors_response("{\"ok\":true}", 200));
            }

            ("POST", "/_bridge/send-fingerprints") => {
                if let Some(body) = read_body(&mut request) {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
                        let wiki_id = json["wiki_id"].as_str().unwrap_or("").to_string();
                        let to_device_id =
                            json["to_device_id"].as_str().unwrap_or("").to_string();

                        let fingerprints: Vec<super::protocol::TiddlerFingerprint> = json
                            ["fingerprints"]
                            .as_array()
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|f| {
                                        Some(super::protocol::TiddlerFingerprint {
                                            title: f["title"].as_str()?.to_string(),
                                            modified: f["modified"]
                                                .as_str()
                                                .unwrap_or("")
                                                .to_string(),
                                            deleted: f["deleted"].as_bool().filter(|&b| b),
                                        })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();

                        if !wiki_id.is_empty() && !to_device_id.is_empty() {
                            if let Some(mgr) = super::get_sync_manager() {
                                let mgr = mgr.clone();
                                tauri::async_runtime::spawn(async move {
                                    if let Err(e) = mgr
                                        .send_tiddler_fingerprints(
                                            &wiki_id,
                                            &to_device_id,
                                            fingerprints,
                                        )
                                        .await
                                    {
                                        eprintln!(
                                            "[Android Bridge] Send fingerprints error: {}",
                                            e
                                        );
                                    }
                                });
                            }
                        }
                    }
                }
                let _ = request.respond(cors_response("{\"ok\":true}", 200));
            }

            ("POST", "/_bridge/broadcast-fingerprints") => {
                if let Some(body) = read_body(&mut request) {
                    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&body) {
                        let wiki_id = json["wiki_id"].as_str().unwrap_or("").to_string();

                        let fingerprints: Vec<super::protocol::TiddlerFingerprint> = json
                            ["fingerprints"]
                            .as_array()
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|f| {
                                        Some(super::protocol::TiddlerFingerprint {
                                            title: f["title"].as_str()?.to_string(),
                                            modified: f["modified"]
                                                .as_str()
                                                .unwrap_or("")
                                                .to_string(),
                                            deleted: f["deleted"].as_bool().filter(|&b| b),
                                        })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();

                        if !wiki_id.is_empty() {
                            if let Some(mgr) = super::get_sync_manager() {
                                let mgr = mgr.clone();
                                tauri::async_runtime::spawn(async move {
                                    if let Err(e) = mgr
                                        .broadcast_tiddler_fingerprints(
                                            &wiki_id,
                                            fingerprints,
                                        )
                                        .await
                                    {
                                        eprintln!(
                                            "[Android Bridge] Broadcast fingerprints error: {}",
                                            e
                                        );
                                    }
                                });
                            }
                        }
                    }
                }
                let _ = request.respond(cors_response("{\"ok\":true}", 200));
            }

            // ── Queries ───────────────────────────────────────────────

            ("GET", url) if url.starts_with("/_bridge/sync-id?") => {
                let path = url
                    .strip_prefix("/_bridge/sync-id?path=")
                    .unwrap_or("")
                    .to_string();
                let path = urlencoding::decode(&path)
                    .unwrap_or_else(|_| path.clone().into())
                    .to_string();

                let sync_id = if let Some(app) = GLOBAL_APP_HANDLE.get() {
                    crate::wiki_storage::get_wiki_sync_id(app.clone(), path)
                } else {
                    String::new()
                };

                let resp = serde_json::json!({ "sync_id": sync_id }).to_string();
                let _ = request.respond(cors_response(&resp, 200));
            }

            // ── Inbound: poll for changes to apply in wiki ────────────

            ("GET", url) if url.starts_with("/_bridge/poll?") => {
                let wiki_id = url
                    .strip_prefix("/_bridge/poll?wiki_id=")
                    .unwrap_or("")
                    .to_string();
                let wiki_id = urlencoding::decode(&wiki_id)
                    .unwrap_or_else(|_| wiki_id.clone().into())
                    .to_string();

                let changes: Vec<serde_json::Value> = if let Ok(mut map) = pending.lock() {
                    if let Some(queue) = map.get_mut(&wiki_id) {
                        queue.drain(..).collect()
                    } else {
                        vec![]
                    }
                } else {
                    vec![]
                };

                let resp =
                    serde_json::to_string(&changes).unwrap_or_else(|_| "[]".to_string());
                let _ = request.respond(cors_response(&resp, 200));
            }

            _ => {
                let _ = request.respond(cors_response("{\"error\":\"not found\"}", 404));
            }
        }
    }

    eprintln!("[Android Bridge] Server thread exited");
}
