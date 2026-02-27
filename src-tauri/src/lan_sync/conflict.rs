//! Conflict detection and resolution using vector clocks.
//!
//! Each device maintains per-tiddler vector clocks in sync_state/{wiki_id}.json.
//! When a remote change arrives, we compare clocks to determine if it's a
//! fast-forward, or a true concurrent conflict.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::protocol::VectorClock;

/// Tiddler titles excluded from sync (per-device navigation/preferences state)
const EXCLUDED_TIDDLERS: &[&str] = &[
    "$:/StoryList",
    "$:/HistoryList",
    "$:/library/sjcl.js",
    "$:/Import",
    "$:/language",
    "$:/theme",
    "$:/palette",
    "$:/DefaultTiddlers",
    "$:/isEncrypted",
    "$:/view",
    "$:/layout",
    "$:/core",
];

/// Prefix for draft tiddlers (excluded from sync).
/// TiddlyWiki creates "Draft of 'title'", "Draft 2 of 'title'", "Draft 3 of 'title'", etc.
const DRAFT_PREFIX: &str = "Draft of '";
const DRAFT_N_PREFIX: &str = "Draft ";

/// Prefix for conflict tiddlers
pub const CONFLICT_PREFIX: &str = "$:/TiddlyDesktopRS/Conflicts/";

/// Prefixes for tiddlers excluded from sync (transient state, injected plugins)
const EXCLUDED_PREFIXES: &[&str] = &[
    "$:/state/",
    "$:/status/",
    "$:/temp/",
    "$:/config/",
    "$:/language/",
    "$:/plugins/tiddlydesktop-rs/",
    "$:/plugins/tiddlydesktop/",
    "$:/themes/tiddlywiki/vanilla/options/",
    "$:/themes/tiddlywiki/vanilla/metrics/",
];

/// A deletion tombstone
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tombstone {
    pub title: String,
    pub vector_clock: VectorClock,
    pub deleted_at: u64,
}

/// Per-wiki sync state
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WikiSyncState {
    /// Vector clocks for each tiddler
    pub tiddler_clocks: HashMap<String, VectorClock>,
    /// Deletion tombstones (pruned after 30 days)
    pub tombstones: Vec<Tombstone>,
}

/// Result of comparing a remote change against local state
#[derive(Debug)]
pub enum ConflictResult {
    /// Remote is strictly newer — apply it
    FastForward,
    /// Local is strictly newer — ignore remote
    LocalNewer,
    /// True concurrent conflict — apply last-write-wins, save loser as conflict tiddler
    Conflict,
    /// Both are equal — no action needed
    Equal,
}

/// Manages conflict detection for all wikis
pub struct ConflictManager {
    /// Our device ID for incrementing vector clocks
    device_id: String,
    /// Path to sync_state directory
    state_dir: PathBuf,
    /// In-memory sync state per wiki
    states: Mutex<HashMap<String, WikiSyncState>>,
    /// Wiki IDs with unsaved state changes (write failed or pending)
    dirty_wikis: Mutex<Vec<String>>,
}

impl ConflictManager {
    pub fn new(device_id: String, data_dir: &Path) -> Self {
        let state_dir = data_dir.join("sync_state");
        let _ = std::fs::create_dir_all(&state_dir);

        Self {
            device_id,
            state_dir,
            states: Mutex::new(HashMap::new()),
            dirty_wikis: Mutex::new(Vec::new()),
        }
    }

    /// Check if a tiddler title should be synced
    pub fn should_sync_tiddler(title: &str) -> bool {
        if EXCLUDED_TIDDLERS.contains(&title) {
            return false;
        }
        if title.starts_with(DRAFT_PREFIX) {
            return false;
        }
        // "Draft 2 of 'title'", "Draft 3 of 'title'", etc.
        if title.starts_with(DRAFT_N_PREFIX) {
            if let Some(rest) = title.strip_prefix(DRAFT_N_PREFIX) {
                // Check for "N of '" pattern where N is a number
                if let Some(of_pos) = rest.find(" of '") {
                    if rest[..of_pos].chars().all(|c| c.is_ascii_digit()) {
                        return false;
                    }
                }
            }
        }
        if title.starts_with(CONFLICT_PREFIX) {
            return false;
        }
        for prefix in EXCLUDED_PREFIXES {
            if title.starts_with(prefix) {
                return false;
            }
        }
        true
    }

    /// Record a local tiddler change and return the updated vector clock
    /// Get the current vector clock for a tiddler without incrementing it.
    /// Used when relay-routing a change that was already recorded by the bridge.
    pub fn get_clock(&self, wiki_id: &str, title: &str) -> VectorClock {
        let states = self.states.lock().unwrap();
        states.get(wiki_id)
            .and_then(|s| s.tiddler_clocks.get(title))
            .cloned()
            .unwrap_or_else(VectorClock::new)
    }

    pub fn record_local_change(&self, wiki_id: &str, title: &str) -> VectorClock {
        let mut states = self.states.lock().unwrap();
        let state = states.entry(wiki_id.to_string()).or_default();

        let clock = state
            .tiddler_clocks
            .entry(title.to_string())
            .or_insert_with(VectorClock::new);
        clock.increment(&self.device_id);

        let result = clock.clone();
        self.save_state_async(wiki_id, state);
        result
    }

    /// Record a local deletion and return the updated vector clock + add tombstone
    pub fn record_local_deletion(&self, wiki_id: &str, title: &str) -> VectorClock {
        let mut states = self.states.lock().unwrap();
        let state = states.entry(wiki_id.to_string()).or_default();

        let clock = state
            .tiddler_clocks
            .entry(title.to_string())
            .or_insert_with(VectorClock::new);
        clock.increment(&self.device_id);
        let result = clock.clone();

        // Add tombstone
        state.tombstones.push(Tombstone {
            title: title.to_string(),
            vector_clock: result.clone(),
            deleted_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        });

        self.save_state_async(wiki_id, state);
        result
    }

    /// Check a remote change against local state
    pub fn check_remote_change(
        &self,
        wiki_id: &str,
        title: &str,
        remote_clock: &VectorClock,
    ) -> ConflictResult {
        let states = self.states.lock().unwrap();
        let state = match states.get(wiki_id) {
            Some(s) => s,
            None => return ConflictResult::FastForward, // No local state = always accept
        };

        let local_clock = match state.tiddler_clocks.get(title) {
            Some(c) => c,
            None => return ConflictResult::FastForward, // Never seen this tiddler = accept
        };

        if remote_clock == local_clock {
            ConflictResult::Equal
        } else if remote_clock.dominates(local_clock) {
            ConflictResult::FastForward
        } else if local_clock.dominates(remote_clock) {
            ConflictResult::LocalNewer
        } else {
            ConflictResult::Conflict
        }
    }

    /// Accept a remote change: merge the remote clock into our local state
    pub fn accept_remote_change(
        &self,
        wiki_id: &str,
        title: &str,
        remote_clock: &VectorClock,
    ) {
        let mut states = self.states.lock().unwrap();
        let state = states.entry(wiki_id.to_string()).or_default();

        let clock = state
            .tiddler_clocks
            .entry(title.to_string())
            .or_insert_with(VectorClock::new);
        clock.merge(remote_clock);

        self.save_state_async(wiki_id, state);
    }

    /// Accept a remote deletion
    pub fn accept_remote_deletion(
        &self,
        wiki_id: &str,
        title: &str,
        remote_clock: &VectorClock,
    ) {
        let mut states = self.states.lock().unwrap();
        let state = states.entry(wiki_id.to_string()).or_default();

        let clock = state
            .tiddler_clocks
            .entry(title.to_string())
            .or_insert_with(VectorClock::new);
        clock.merge(remote_clock);

        // Add tombstone
        state.tombstones.push(Tombstone {
            title: title.to_string(),
            vector_clock: remote_clock.clone(),
            deleted_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        });

        self.save_state_async(wiki_id, state);
    }

    /// Check if a tiddler has a deletion tombstone that is newer than the given clock
    pub fn is_deleted(&self, wiki_id: &str, title: &str, clock: &VectorClock) -> bool {
        let states = self.states.lock().unwrap();
        if let Some(state) = states.get(wiki_id) {
            for tombstone in &state.tombstones {
                if tombstone.title == title && tombstone.vector_clock.dominates(clock) {
                    return true;
                }
            }
        }
        false
    }

    /// Get all known tiddler clocks for a wiki (for RequestFullSync)
    pub fn get_known_clocks(&self, wiki_id: &str) -> HashMap<String, VectorClock> {
        let states = self.states.lock().unwrap();
        states
            .get(wiki_id)
            .map(|s| s.tiddler_clocks.clone())
            .unwrap_or_default()
    }

    /// Load sync state from disk for a wiki.
    /// If the state file is corrupt, logs a warning and starts with empty state
    /// (which will trigger a full sync with peers on next connection).
    pub fn load_wiki_state(&self, wiki_id: &str) {
        let path = self.state_dir.join(format!("{}.json", wiki_id));
        if !path.exists() {
            return;
        }
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                match serde_json::from_str::<WikiSyncState>(&content) {
                    Ok(state) => {
                        let mut states = self.states.lock().unwrap();
                        states.insert(wiki_id.to_string(), state);
                    }
                    Err(e) => {
                        eprintln!(
                            "[LAN Sync] Corrupt sync state for wiki {} — starting fresh (will full-sync): {}",
                            wiki_id, e
                        );
                        // Remove corrupt file
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "[LAN Sync] Failed to read sync state for wiki {}: {}",
                    wiki_id, e
                );
            }
        }
    }

    /// Prune tombstones older than 30 days
    pub fn prune_tombstones(&self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let thirty_days = 30 * 24 * 60 * 60;

        let mut states = self.states.lock().unwrap();
        for (wiki_id, state) in states.iter_mut() {
            let before = state.tombstones.len();
            state.tombstones.retain(|t| now - t.deleted_at < thirty_days);
            let pruned = before - state.tombstones.len();
            if pruned > 0 {
                eprintln!(
                    "[LAN Sync] Pruned {} tombstones for wiki {}",
                    pruned, wiki_id
                );
            }
        }
    }

    // ── Persistence ─────────────────────────────────────────────────────

    fn save_state_async(&self, wiki_id: &str, state: &WikiSyncState) {
        let path = self.state_dir.join(format!("{}.json", wiki_id));
        match serde_json::to_string(state) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, &json) {
                    eprintln!(
                        "[LAN Sync] Failed to save sync state for wiki {}: {} — will retry on next change",
                        wiki_id, e
                    );
                    // Track as dirty so we retry on next save
                    if let Ok(mut dirty) = self.dirty_wikis.lock() {
                        if !dirty.contains(&wiki_id.to_string()) {
                            dirty.push(wiki_id.to_string());
                        }
                    }
                } else {
                    // Clear dirty flag on success
                    if let Ok(mut dirty) = self.dirty_wikis.lock() {
                        dirty.retain(|id| id != wiki_id);
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "[LAN Sync] Failed to serialize sync state for wiki {}: {}",
                    wiki_id, e
                );
            }
        }
    }

    /// Retry saving state for wikis that previously failed to write.
    /// Called periodically (e.g., during tombstone pruning).
    pub fn flush_dirty_states(&self) {
        let dirty_ids: Vec<String> = {
            let dirty = self.dirty_wikis.lock().unwrap();
            dirty.clone()
        };
        if dirty_ids.is_empty() {
            return;
        }
        let states = self.states.lock().unwrap();
        for wiki_id in &dirty_ids {
            if let Some(state) = states.get(wiki_id) {
                let path = self.state_dir.join(format!("{}.json", wiki_id));
                if let Ok(json) = serde_json::to_string(state) {
                    if std::fs::write(&path, &json).is_ok() {
                        eprintln!("[LAN Sync] Successfully retried saving state for wiki {}", wiki_id);
                    }
                }
            }
        }
        drop(states);
        // Clear the ones that succeeded
        if let Ok(mut dirty) = self.dirty_wikis.lock() {
            dirty.retain(|id| {
                let path = self.state_dir.join(format!("{}.json", id));
                !path.exists()
            });
        }
    }
}
