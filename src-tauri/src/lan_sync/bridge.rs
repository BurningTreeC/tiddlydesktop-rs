//! Bridge between LAN sync and wiki processes.
//!
//! On desktop, this connects to the IPC system to forward changes between
//! wiki windows and the LAN sync module. On Android, it uses HTTP bridges
//! to the :wiki process.

use std::sync::Arc;
use tokio::sync::mpsc;

use super::conflict::{ConflictManager, ConflictResult};
use super::protocol::SyncMessage;
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
}

/// Messages from LAN sync to wiki processes
#[derive(Debug, Clone)]
pub enum SyncToWiki {
    /// Apply a tiddler change from a remote peer
    ApplyTiddlerChange {
        wiki_id: String,
        title: String,
        tiddler_json: String,
    },
    /// Apply a tiddler deletion from a remote peer
    ApplyTiddlerDeletion {
        wiki_id: String,
        title: String,
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
                        // Send to wiki first — only merge clock if send succeeds
                        if self.sync_to_wiki_tx.send(SyncToWiki::ApplyTiddlerChange {
                            wiki_id: wiki_id.clone(),
                            title: title.clone(),
                            tiddler_json,
                        }).is_ok() {
                            conflict_manager.accept_remote_change(&wiki_id, &title, &vector_clock);
                        } else {
                            eprintln!(
                                "[LAN Sync] Failed to send change for '{}' to wiki channel — clock not merged",
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
                        // Send to wiki first — only merge clock if send succeeds
                        if self.sync_to_wiki_tx.send(SyncToWiki::ApplyTiddlerChange {
                            wiki_id: wiki_id.clone(),
                            title: title.clone(),
                            tiddler_json,
                        }).is_ok() {
                            conflict_manager.accept_remote_change(&wiki_id, &title, &vector_clock);
                        } else {
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
                        conflict_manager.accept_remote_deletion(&wiki_id, &title, &vector_clock);
                        let _ = self.sync_to_wiki_tx.send(SyncToWiki::ApplyTiddlerDeletion {
                            wiki_id,
                            title,
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
                        conflict_manager.accept_remote_deletion(&wiki_id, &title, &vector_clock);
                        let _ = self.sync_to_wiki_tx.send(SyncToWiki::ApplyTiddlerDeletion {
                            wiki_id,
                            title,
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
                            if self.sync_to_wiki_tx.send(SyncToWiki::ApplyTiddlerChange {
                                wiki_id: wiki_id.clone(),
                                title: tiddler.title.clone(),
                                tiddler_json: tiddler.tiddler_json,
                            }).is_ok() {
                                conflict_manager.accept_remote_change(
                                    &wiki_id,
                                    &tiddler.title,
                                    &tiddler.vector_clock,
                                );
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
                            if self.sync_to_wiki_tx.send(SyncToWiki::ApplyTiddlerChange {
                                wiki_id: wiki_id.clone(),
                                title: tiddler.title.clone(),
                                tiddler_json: tiddler.tiddler_json,
                            }).is_ok() {
                                conflict_manager.accept_remote_change(
                                    &wiki_id,
                                    &tiddler.title,
                                    &tiddler.vector_clock,
                                );
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

                eprintln!("[LAN Sync] Broadcasting local change: '{}' in wiki {}", title, wiki_id);
                let clock = conflict_manager.record_local_change(&wiki_id, &title);
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();

                server
                    .broadcast(&SyncMessage::TiddlerChanged {
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

                let clock = conflict_manager.record_local_deletion(&wiki_id, &title);
                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();

                server
                    .broadcast(&SyncMessage::TiddlerDeleted {
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
                // Broadcast the FULL wiki manifest (all sync-enabled wikis),
                // not just the one that was opened. The peer's handle_wiki_manifest
                // replaces its entire record of our wikis, so a partial list would
                // cause it to forget about other shared wikis.
                if let Some(app) = super::GLOBAL_APP_HANDLE.get() {
                    let sync_wikis = crate::wiki_storage::get_sync_enabled_wikis(app);
                    let wikis: Vec<super::protocol::WikiInfo> = sync_wikis
                        .into_iter()
                        .map(|(sync_id, name, is_folder)| super::protocol::WikiInfo {
                            wiki_id: sync_id,
                            wiki_name: name,
                            is_folder,
                        })
                        .collect();
                    server
                        .broadcast(&SyncMessage::WikiManifest { wikis })
                        .await;
                }
            }
            WikiToSync::WikiClosed { wiki_id } => {
                eprintln!("[LAN Sync] Wiki closed: {}", wiki_id);
            }
        }
    }
}
