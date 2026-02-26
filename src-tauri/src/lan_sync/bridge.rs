//! Bridge between LAN sync and wiki processes.
//!
//! On desktop, this connects to the IPC system to forward changes between
//! wiki windows and the LAN sync module. On Android, it uses HTTP bridges
//! to the :wiki process.

use std::sync::Arc;
use tokio::sync::mpsc;

use super::conflict::{ConflictManager, ConflictResult};
use super::protocol::{SyncMessage, VectorClock};
use super::server::SyncServer;

/// Messages from wiki processes to the LAN sync module
#[derive(Debug, Clone)]
pub enum WikiToSync {
    /// A tiddler was changed in a wiki window
    TiddlerChanged {
        wiki_id: String,
        title: String,
        tiddler_json: String,
    },
    /// A tiddler was deleted in a wiki window
    TiddlerDeleted {
        wiki_id: String,
        title: String,
    },
    /// A wiki was opened (send manifest to peers)
    WikiOpened {
        wiki_id: String,
        wiki_name: String,
        is_folder: bool,
    },
    /// A wiki was closed
    WikiClosed {
        wiki_id: String,
    },

    // ── Collaborative editing ────────────────────────────────────────

    /// Local device started editing a tiddler
    CollabEditingStarted {
        wiki_id: String,
        tiddler_title: String,
        device_id: String,
        device_name: String,
    },
    /// Local device stopped editing a tiddler
    CollabEditingStopped {
        wiki_id: String,
        tiddler_title: String,
        device_id: String,
    },
    /// Outbound Yjs document update
    CollabUpdate {
        wiki_id: String,
        tiddler_title: String,
        update_base64: String,
    },
    /// Outbound Yjs awareness update
    CollabAwareness {
        wiki_id: String,
        tiddler_title: String,
        update_base64: String,
    },
    /// Local device saved a tiddler being collaboratively edited
    CollabPeerSaved {
        wiki_id: String,
        tiddler_title: String,
        saved_title: String,
        device_id: String,
        device_name: String,
    },
}

/// Messages from LAN sync to wiki processes
#[derive(Debug, Clone)]
pub enum SyncToWiki {
    /// Apply a tiddler change from a remote peer
    ApplyTiddlerChange {
        wiki_id: String,
        title: String,
        tiddler_json: String,
        /// Vector clock to merge after confirmed IPC delivery (None = already merged)
        vector_clock: Option<VectorClock>,
    },
    /// Apply a tiddler deletion from a remote peer
    ApplyTiddlerDeletion {
        wiki_id: String,
        title: String,
        /// Vector clock to merge after confirmed IPC delivery (None = already merged)
        vector_clock: Option<VectorClock>,
    },
    /// A conflict was detected — save the losing version
    SaveConflict {
        wiki_id: String,
        title: String,
        conflict_tiddler_json: String,
    },
}

/// The sync bridge processes events from the server and wiki processes
pub struct SyncBridge {
    /// Channel to receive wiki process changes
    pub wiki_tx: mpsc::UnboundedSender<WikiToSync>,
    /// Channel to send changes to wiki processes
    pub sync_to_wiki_rx: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<SyncToWiki>>>,
    sync_to_wiki_tx: mpsc::UnboundedSender<SyncToWiki>,
}

impl SyncBridge {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<WikiToSync>) {
        let (wiki_tx, wiki_rx) = mpsc::unbounded_channel();
        let (sync_to_wiki_tx, sync_to_wiki_rx) = mpsc::unbounded_channel();

        (
            Self {
                wiki_tx,
                sync_to_wiki_rx: Arc::new(tokio::sync::Mutex::new(sync_to_wiki_rx)),
                sync_to_wiki_tx,
            },
            wiki_rx,
        )
    }

    /// Process a sync message received from a remote peer.
    /// Returns (is_last_batch, applied_count) for FullSyncBatch messages,
    /// (false, 0) for all others.
    pub fn handle_remote_message(
        &self,
        from_device_id: &str,
        message: SyncMessage,
        conflict_manager: &ConflictManager,
    ) -> (bool, u32) {
        match message {
            SyncMessage::TiddlerChanged {
                wiki_id,
                title,
                tiddler_json,
                vector_clock,
                timestamp: _,
            } => {
                if !ConflictManager::should_sync_tiddler(&title) {
                    return (false, 0);
                }

                // Check if this tiddler was deleted locally
                if conflict_manager.is_deleted(&wiki_id, &title, &vector_clock) {
                    eprintln!(
                        "[LAN Sync] Ignoring change for deleted tiddler: {}",
                        title
                    );
                    return (false, 0);
                }

                match conflict_manager.check_remote_change(&wiki_id, &title, &vector_clock) {
                    ConflictResult::FastForward => {
                        eprintln!("[LAN Sync] Applying remote change (FastForward): '{}' from {}", title, from_device_id);
                        // Defer clock merge until confirmed IPC delivery to JS.
                        // Pass the vector clock through the channel so emit_to_wiki
                        // can merge it after successful delivery.
                        if self.sync_to_wiki_tx.send(SyncToWiki::ApplyTiddlerChange {
                            wiki_id: wiki_id.clone(),
                            title: title.clone(),
                            tiddler_json,
                            vector_clock: Some(vector_clock),
                        }).is_err() {
                            eprintln!(
                                "[LAN Sync] Failed to send change for '{}' to wiki channel",
                                title
                            );
                        }
                    }
                    ConflictResult::LocalNewer => {
                        eprintln!(
                            "[LAN Sync] Local is newer for tiddler '{}', ignoring remote from {}",
                            title, from_device_id
                        );
                    }
                    ConflictResult::Equal => {
                        // No action needed
                    }
                    ConflictResult::Conflict => {
                        eprintln!(
                            "[LAN Sync] Conflict detected for tiddler '{}' from {}",
                            title, from_device_id
                        );

                        // Signal the conflict to the JS side so it can save the local version
                        let _ = self.sync_to_wiki_tx.send(SyncToWiki::SaveConflict {
                            wiki_id: wiki_id.clone(),
                            title: title.clone(),
                            conflict_tiddler_json: String::new(), // JS side has the local version
                        });

                        // Last-write-wins: apply the remote change
                        // Defer clock merge until confirmed IPC delivery
                        if self.sync_to_wiki_tx.send(SyncToWiki::ApplyTiddlerChange {
                            wiki_id: wiki_id.clone(),
                            title: title.clone(),
                            tiddler_json,
                            vector_clock: Some(vector_clock),
                        }).is_err() {
                            eprintln!(
                                "[LAN Sync] Failed to send conflict change for '{}' to wiki channel",
                                title
                            );
                        }
                    }
                }
                (false, 0)
            }
            SyncMessage::TiddlerDeleted {
                wiki_id,
                title,
                vector_clock,
                timestamp: _,
            } => {
                if !ConflictManager::should_sync_tiddler(&title) {
                    return (false, 0);
                }

                match conflict_manager.check_remote_change(&wiki_id, &title, &vector_clock) {
                    ConflictResult::FastForward => {
                        // Defer clock merge until confirmed IPC delivery
                        let _ = self.sync_to_wiki_tx.send(SyncToWiki::ApplyTiddlerDeletion {
                            wiki_id,
                            title,
                            vector_clock: Some(vector_clock),
                        });
                    }
                    ConflictResult::Conflict => {
                        // Concurrent edit vs delete: save local version as conflict tiddler
                        // before applying the deletion, so local edits aren't silently lost
                        eprintln!(
                            "[LAN Sync] Conflict: tiddler '{}' deleted remotely but edited locally — saving conflict",
                            title
                        );
                        let _ = self.sync_to_wiki_tx.send(SyncToWiki::SaveConflict {
                            wiki_id: wiki_id.clone(),
                            title: title.clone(),
                            conflict_tiddler_json: String::new(), // JS side has the local version
                        });
                        // Defer clock merge until confirmed IPC delivery
                        let _ = self.sync_to_wiki_tx.send(SyncToWiki::ApplyTiddlerDeletion {
                            wiki_id,
                            title,
                            vector_clock: Some(vector_clock),
                        });
                    }
                    ConflictResult::LocalNewer => {
                        eprintln!(
                            "[LAN Sync] Local is newer for deleted tiddler '{}', ignoring",
                            title
                        );
                    }
                    ConflictResult::Equal => {}
                }
                (false, 0)
            }
            SyncMessage::FullSyncBatch {
                wiki_id,
                tiddlers,
                is_last_batch,
            } => {
                let batch_count = tiddlers.len();
                let mut applied = 0u32;
                let mut skipped_filter = 0u32;
                let mut skipped_equal = 0u32;
                let mut skipped_local_newer = 0u32;
                let mut conflicts = 0u32;
                for tiddler in tiddlers {
                    if !ConflictManager::should_sync_tiddler(&tiddler.title) {
                        skipped_filter += 1;
                        continue;
                    }

                    match conflict_manager.check_remote_change(
                        &wiki_id,
                        &tiddler.title,
                        &tiddler.vector_clock,
                    ) {
                        ConflictResult::FastForward => {
                            // Defer clock merge until confirmed IPC delivery
                            if self.sync_to_wiki_tx.send(SyncToWiki::ApplyTiddlerChange {
                                wiki_id: wiki_id.clone(),
                                title: tiddler.title.clone(),
                                tiddler_json: tiddler.tiddler_json,
                                vector_clock: Some(tiddler.vector_clock),
                            }).is_ok() {
                                applied += 1;
                            }
                        }
                        ConflictResult::Conflict => {
                            conflicts += 1;
                            // Both sides edited this tiddler while offline.
                            // Last-write-wins: apply remote, save local as conflict tiddler.
                            eprintln!(
                                "[LAN Sync] Full sync conflict for tiddler '{}' from {}",
                                tiddler.title, from_device_id
                            );
                            let _ = self.sync_to_wiki_tx.send(SyncToWiki::SaveConflict {
                                wiki_id: wiki_id.clone(),
                                title: tiddler.title.clone(),
                                conflict_tiddler_json: String::new(),
                            });
                            // Defer clock merge until confirmed IPC delivery
                            if self.sync_to_wiki_tx.send(SyncToWiki::ApplyTiddlerChange {
                                wiki_id: wiki_id.clone(),
                                title: tiddler.title.clone(),
                                tiddler_json: tiddler.tiddler_json,
                                vector_clock: Some(tiddler.vector_clock),
                            }).is_ok() {
                                applied += 1;
                            }
                        }
                        ConflictResult::LocalNewer => {
                            skipped_local_newer += 1;
                            eprintln!(
                                "[LAN Sync] FullSyncBatch: skipped '{}' (LocalNewer)",
                                tiddler.title
                            );
                        }
                        ConflictResult::Equal => {
                            skipped_equal += 1;
                        }
                    }
                }
                eprintln!(
                    "[LAN Sync] FullSyncBatch: {} in batch, {} applied, {} filtered, {} equal, {} local-newer, {} conflicts, is_last={}",
                    batch_count, applied, skipped_filter, skipped_equal, skipped_local_newer, conflicts, is_last_batch
                );

                if is_last_batch {
                    eprintln!("[LAN Sync] Full sync completed for wiki {}", wiki_id);
                }
                (is_last_batch, applied)
            }
            SyncMessage::Ping => {
                // Pong is handled at the server level
                (false, 0)
            }
            _ => {
                // Other message types handled elsewhere (attachments, manifest, etc.)
                (false, 0)
            }
        }
    }

    /// Handle a local wiki change: record in conflict manager and broadcast to peers
    pub async fn handle_local_change(
        &self,
        change: WikiToSync,
        conflict_manager: &ConflictManager,
        server: &SyncServer,
    ) {
        match change {
            WikiToSync::TiddlerChanged {
                wiki_id,
                title,
                tiddler_json,
            } => {
                if !ConflictManager::should_sync_tiddler(&title) {
                    return;
                }

                // Get peers for this wiki via room membership
                let peers = if let Some(app) = super::GLOBAL_APP_HANDLE.get() {
                    if let Some(room_code) = crate::wiki_storage::get_wiki_relay_room_by_sync_id(app, &wiki_id) {
                        server.peers_for_room(&room_code).await
                    } else {
                        return;
                    }
                } else {
                    return;
                };
                if peers.is_empty() {
                    return;
                }

                eprintln!("[LAN Sync] Broadcasting local change: '{}' in wiki {} to {} peers", title, wiki_id, peers.len());
                let clock = conflict_manager.record_local_change(&wiki_id, &title);
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();

                server
                    .send_to_peers(&peers, &SyncMessage::TiddlerChanged {
                        wiki_id,
                        title,
                        tiddler_json,
                        vector_clock: clock,
                        timestamp,
                    })
                    .await;
            }
            WikiToSync::TiddlerDeleted { wiki_id, title } => {
                if !ConflictManager::should_sync_tiddler(&title) {
                    return;
                }

                // Get peers for this wiki via room membership
                let peers = if let Some(app) = super::GLOBAL_APP_HANDLE.get() {
                    if let Some(room_code) = crate::wiki_storage::get_wiki_relay_room_by_sync_id(app, &wiki_id) {
                        server.peers_for_room(&room_code).await
                    } else {
                        return;
                    }
                } else {
                    return;
                };
                if peers.is_empty() {
                    return;
                }

                let clock = conflict_manager.record_local_deletion(&wiki_id, &title);
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();

                server
                    .send_to_peers(&peers, &SyncMessage::TiddlerDeleted {
                        wiki_id,
                        title,
                        vector_clock: clock,
                        timestamp,
                    })
                    .await;
            }
            WikiToSync::WikiOpened {
                wiki_id,
                ..
            } => {
                conflict_manager.load_wiki_state(&wiki_id);
                // Send per-peer filtered WikiManifest to each connected peer
                // (each peer only sees wikis assigned to the room they share with us)
                if let Some(app) = super::GLOBAL_APP_HANDLE.get() {
                    let lan_peers = server.lan_connected_peers().await;
                    for (peer_id, _) in &lan_peers {
                        let room_codes = server.peer_room_codes(peer_id).await;
                        // Union wikis from all shared rooms
                        let mut seen_wiki_ids = std::collections::HashSet::new();
                        let mut all_sync_wikis = Vec::new();
                        for rc in &room_codes {
                            for wiki in crate::wiki_storage::get_sync_wikis_for_room(app, rc) {
                                if seen_wiki_ids.insert(wiki.0.clone()) {
                                    all_sync_wikis.push(wiki);
                                }
                            }
                        }
                        let wikis: Vec<super::protocol::WikiInfo> = all_sync_wikis
                            .into_iter()
                            .map(|(sync_id, name, is_folder)| super::protocol::WikiInfo {
                                wiki_id: sync_id,
                                wiki_name: name,
                                is_folder,
                            })
                            .collect();
                        let _ = server.send_to_peer(peer_id, &SyncMessage::WikiManifest { wikis }).await;
                    }
                }
            }
            WikiToSync::WikiClosed { wiki_id } => {
                eprintln!("[LAN Sync] Wiki closed: {}", wiki_id);
            }

            // ── Collaborative editing ────────────────────────────────────
            WikiToSync::CollabEditingStarted {
                wiki_id,
                tiddler_title,
                device_id,
                device_name,
            } => {
                let peers = if let Some(app) = super::GLOBAL_APP_HANDLE.get() {
                    if let Some(room_code) = crate::wiki_storage::get_wiki_relay_room_by_sync_id(app, &wiki_id) {
                        server.peers_for_room(&room_code).await
                    } else { vec![] }
                } else { vec![] };
                if !peers.is_empty() {
                    eprintln!("[Collab] OUTBOUND broadcast EditingStarted: wiki={}, tiddler={}", wiki_id, tiddler_title);
                    server
                        .send_to_peers(&peers, &SyncMessage::EditingStarted {
                            wiki_id,
                            tiddler_title,
                            device_id,
                            device_name,
                        })
                        .await;
                }
            }
            WikiToSync::CollabEditingStopped {
                wiki_id,
                tiddler_title,
                device_id,
            } => {
                let peers = if let Some(app) = super::GLOBAL_APP_HANDLE.get() {
                    if let Some(room_code) = crate::wiki_storage::get_wiki_relay_room_by_sync_id(app, &wiki_id) {
                        server.peers_for_room(&room_code).await
                    } else { vec![] }
                } else { vec![] };
                if !peers.is_empty() {
                    eprintln!("[Collab] OUTBOUND broadcast EditingStopped: wiki={}, tiddler={}", wiki_id, tiddler_title);
                    server
                        .send_to_peers(&peers, &SyncMessage::EditingStopped {
                            wiki_id,
                            tiddler_title,
                            device_id,
                        })
                        .await;
                }
            }
            WikiToSync::CollabUpdate {
                wiki_id,
                tiddler_title,
                update_base64,
            } => {
                let peers = if let Some(app) = super::GLOBAL_APP_HANDLE.get() {
                    if let Some(room_code) = crate::wiki_storage::get_wiki_relay_room_by_sync_id(app, &wiki_id) {
                        server.peers_for_room(&room_code).await
                    } else { vec![] }
                } else { vec![] };
                if !peers.is_empty() {
                    eprintln!("[Collab] OUTBOUND broadcast CollabUpdate: wiki={}, tiddler={}, len={}", wiki_id, tiddler_title, update_base64.len());
                    server
                        .send_to_peers(&peers, &SyncMessage::CollabUpdate {
                            wiki_id,
                            tiddler_title,
                            update_base64,
                        })
                        .await;
                }
            }
            WikiToSync::CollabAwareness {
                wiki_id,
                tiddler_title,
                update_base64,
            } => {
                // Get the peer we recently received awareness from (if any) to avoid echo-back
                let echo_peer = if let Some(mgr) = super::get_sync_manager() {
                    mgr.get_awareness_echo_peer(&wiki_id, &tiddler_title)
                } else {
                    None
                };
                let mut peers = if let Some(app) = super::GLOBAL_APP_HANDLE.get() {
                    if let Some(room_code) = crate::wiki_storage::get_wiki_relay_room_by_sync_id(app, &wiki_id) {
                        server.peers_for_room(&room_code).await
                    } else { vec![] }
                } else { vec![] };
                // Exclude the peer that sent us this awareness (suppress echo-back)
                if let Some(ref exclude_id) = echo_peer {
                    peers.retain(|p| p != exclude_id);
                }
                if !peers.is_empty() {
                    eprintln!("[Collab] OUTBOUND broadcast CollabAwareness: wiki={}, tiddler={}, len={}", wiki_id, tiddler_title, update_base64.len());
                    server
                        .send_to_peers(&peers, &SyncMessage::CollabAwareness {
                            wiki_id,
                            tiddler_title,
                            update_base64,
                        })
                        .await;
                }
            }
            WikiToSync::CollabPeerSaved {
                wiki_id,
                tiddler_title,
                saved_title,
                device_id,
                device_name,
            } => {
                let peers = if let Some(app) = super::GLOBAL_APP_HANDLE.get() {
                    if let Some(room_code) = crate::wiki_storage::get_wiki_relay_room_by_sync_id(app, &wiki_id) {
                        server.peers_for_room(&room_code).await
                    } else { vec![] }
                } else { vec![] };
                if !peers.is_empty() {
                    eprintln!("[Collab] OUTBOUND broadcast PeerSaved: wiki={}, tiddler={}, saved_as={}", wiki_id, tiddler_title, saved_title);
                    server
                        .send_to_peers(&peers, &SyncMessage::PeerSaved {
                            wiki_id,
                            tiddler_title,
                            saved_title,
                            device_id,
                            device_name,
                        })
                        .await;
                }
            }
        }
    }
}
