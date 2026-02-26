//! LAN Sync protocol message types and encryption/decryption.
//!
//! All messages are JSON-serialized. After pairing, every WebSocket frame is
//! encrypted with ChaCha20-Poly1305 using a per-session key derived from
//! the long-term shared secret via HKDF-SHA256.

use chacha20poly1305::{
    aead::{Aead, KeyInit},
    ChaCha20Poly1305, Nonce,
};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

/// Maximum chunk size for file transfer (256KB).
/// Balances per-message overhead against relay server memory pressure.
/// Each chunk is base64-encoded (~33% expansion), so wire size is ~341KB.
pub const ATTACHMENT_CHUNK_SIZE: usize = 256 * 1024;

/// Delay between sending attachment chunks (ms).
/// Prevents saturating the relay connection and leaves bandwidth for
/// tiddler sync messages on the same WebSocket.
pub const ATTACHMENT_CHUNK_DELAY_MS: u64 = 25;

/// Port range for LAN sync WebSocket server
pub const LAN_SYNC_PORT_START: u16 = 45700;
pub const LAN_SYNC_PORT_END: u16 = 45710;

// ── Room Auth Messages (cleartext, for LAN connections) ──────────────────────
// Uses SPAKE2 password-authenticated key exchange — never transmits room code
// or password material. Provides mutual authentication and forward secrecy.

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RoomAuthMessage {
    /// Client → Server: initiate SPAKE2 key exchange
    RoomAuthInit {
        device_id: String,
        device_name: String,
        /// Hash of room code (for routing only — same hash used in discovery)
        room_hash: String,
        /// Base64-encoded SPAKE2 message A (Ed25519 curve point)
        spake_msg: String,
    },
    /// Server → Client: SPAKE2 response + server key confirmation
    RoomAuthChallenge {
        device_id: String,
        device_name: String,
        /// Base64-encoded SPAKE2 message B (Ed25519 curve point)
        spake_msg: String,
        /// HMAC-SHA256(shared_secret, server_label)[..16] as hex
        key_confirm: String,
    },
    /// Client → Server: client key confirmation
    RoomAuthConfirm {
        /// HMAC-SHA256(shared_secret, client_label)[..16] as hex
        key_confirm: String,
    },
    /// Server rejects the auth
    RoomAuthReject {
        message: String,
    },
}

/// Find the first shared room code between our rooms and a peer's rooms.
/// Returns the first alphabetically-sorted shared room code.
pub fn select_shared_room(our_rooms: &[String], peer_rooms: &[String]) -> Option<String> {
    let mut shared: Vec<&String> = our_rooms.iter()
        .filter(|r| peer_rooms.contains(r))
        .collect();
    shared.sort();
    shared.first().map(|s| (*s).clone())
}

/// Find a shared room by comparing our room code hashes against a peer's hashes.
/// Hashes each of our room codes and checks if any match the peer's advertised hashes.
/// Returns the first alphabetically-sorted matching room code (ours, not the hash).
pub fn select_shared_room_by_hash(our_rooms: &[String], peer_room_hashes: &[String]) -> Option<String> {
    use crate::lan_sync::discovery::hash_room_code;
    let mut shared: Vec<&String> = our_rooms.iter()
        .filter(|r| {
            let our_hash = hash_room_code(r);
            peer_room_hashes.contains(&our_hash)
        })
        .collect();
    shared.sort();
    shared.first().map(|s| (*s).clone())
}

/// Find ALL shared rooms by comparing our room code hashes against a peer's hashes.
/// Same as `select_shared_room_by_hash` but returns all matches, not just the first.
pub fn select_all_shared_rooms_by_hash(our_rooms: &[String], peer_room_hashes: &[String]) -> Vec<String> {
    use crate::lan_sync::discovery::hash_room_code;
    let mut shared: Vec<String> = our_rooms.iter()
        .filter(|r| {
            let our_hash = hash_room_code(r);
            peer_room_hashes.contains(&our_hash)
        })
        .cloned()
        .collect();
    shared.sort();
    shared
}


// ── Sync Phase Messages (encrypted after room auth) ──────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum SyncMessage {
    /// Announce which wikis this device has available for sync
    WikiManifest {
        wikis: Vec<WikiInfo>,
    },
    /// A tiddler was created or modified
    TiddlerChanged {
        wiki_id: String,
        title: String,
        tiddler_json: String,
        vector_clock: VectorClock,
        timestamp: u64,
    },
    /// A tiddler was deleted
    TiddlerDeleted {
        wiki_id: String,
        title: String,
        vector_clock: VectorClock,
        timestamp: u64,
    },
    /// Request a full sync of a wiki (sent on initial connection)
    RequestFullSync {
        wiki_id: String,
        /// Our known vector clocks so the peer can send only what we're missing
        known_clocks: std::collections::HashMap<String, VectorClock>,
    },
    /// Batch of tiddlers for full sync
    FullSyncBatch {
        wiki_id: String,
        tiddlers: Vec<SyncTiddler>,
        is_last_batch: bool,
    },
    /// An attachment file was added or modified
    AttachmentChanged {
        wiki_id: String,
        filename: String,
        file_size: u64,
        #[serde(with = "base64_bytes")]
        sha256: Vec<u8>,
        chunk_count: u32,
    },
    /// A chunk of attachment data
    AttachmentChunk {
        wiki_id: String,
        filename: String,
        chunk_index: u32,
        data_base64: String,
    },
    /// An attachment file was deleted
    AttachmentDeleted {
        wiki_id: String,
        filename: String,
    },
    /// Notify peer of a conflict
    ConflictNotification {
        wiki_id: String,
        title: String,
    },
    /// Request a wiki file transfer. Includes a list of files we already have
    /// (with SHA-256 hashes) so the sender can skip unchanged files.
    RequestWikiFile {
        wiki_id: String,
        /// Files we already have: (relative_path, sha256_hex).
        /// Sender should skip files whose hash matches.
        #[serde(default)]
        have_files: Vec<AttachmentFileInfo>,
    },
    /// A chunk of wiki file data (response to RequestWikiFile)
    WikiFileChunk {
        wiki_id: String,
        wiki_name: String,
        is_folder: bool,
        /// For folder wikis: relative path within the folder (e.g. "tiddlywiki.info", "tiddlers/foo.tid")
        /// For single-file wikis: the filename (e.g. "mywiki.html")
        filename: String,
        chunk_index: u32,
        chunk_count: u32,
        data_base64: String,
    },
    /// Signals the end of a wiki file transfer
    WikiFileComplete {
        wiki_id: String,
        wiki_name: String,
        is_folder: bool,
    },
    /// Attachment manifest — sent after WikiManifest for shared wikis.
    /// Lists all files in the attachments directory with their SHA-256 hashes
    /// so the peer can detect missing or outdated files after an interrupted sync.
    AttachmentManifest {
        wiki_id: String,
        /// (relative_path, sha256_hex) pairs
        files: Vec<AttachmentFileInfo>,
    },
    /// Request specific attachment files that are missing or outdated
    RequestAttachments {
        wiki_id: String,
        /// Relative paths of files that need to be (re-)sent
        files: Vec<String>,
    },
    /// Lightweight tiddler fingerprints for diff-based sync.
    /// Sent instead of a full dump — the receiver compares with local state
    /// and only sends back tiddlers that are missing or newer.
    TiddlerFingerprints {
        wiki_id: String,
        from_device_id: String,
        /// (title, modified_timestamp) pairs for all non-shadow tiddlers
        fingerprints: Vec<TiddlerFingerprint>,
        /// True when these fingerprints are a reciprocal reply.
        /// Receiver should compare and send diffs but NOT reply with
        /// its own fingerprints (prevents infinite ping-pong).
        #[serde(default)]
        is_reply: bool,
    },
    /// Request a peer to send their tiddler fingerprints for a specific wiki.
    /// Sent when a wiki window is about to open and needs catch-up sync.
    /// The peer responds with TiddlerFingerprints so we can compare locally.
    RequestFingerprints {
        wiki_id: String,
    },
    /// tiddlywiki.info content broadcast (folder wikis only).
    /// Sent on wiki open and peer connect for folder wikis.
    WikiInfoChanged {
        wiki_id: String,
        /// Full JSON content of tiddlywiki.info
        content_json: String,
        /// SHA-256 hash of the content
        content_hash: String,
        /// File modification timestamp (ms since epoch)
        timestamp: u64,
    },
    /// Request tiddlywiki.info content from a peer
    WikiInfoRequest {
        wiki_id: String,
    },
    /// Plugin directory file manifest (for non-bundled plugins)
    PluginManifest {
        wiki_id: String,
        plugin_name: String,
        /// List of (relative_path, sha256_hash, file_size) for each file in the plugin
        files: Vec<AttachmentFileInfo>,
        /// Version string from plugin.info (for update direction comparison)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
    /// Request specific plugin files
    RequestPluginFiles {
        wiki_id: String,
        plugin_name: String,
        /// Relative paths of files needed
        needed_files: Vec<String>,
    },
    /// Chunked plugin file transfer
    PluginFileChunk {
        wiki_id: String,
        plugin_name: String,
        rel_path: String,
        chunk_index: u32,
        chunk_count: u32,
        data_base64: String,
    },
    /// All plugin files for a plugin have been sent
    PluginFilesComplete {
        wiki_id: String,
        plugin_name: String,
    },
    // ── Collaborative editing (Yjs transport layer) ────────────────────

    /// A device started editing a tiddler (for awareness display)
    EditingStarted {
        wiki_id: String,
        tiddler_title: String,
        device_id: String,
        device_name: String,
    },
    /// A device stopped editing a tiddler
    EditingStopped {
        wiki_id: String,
        tiddler_title: String,
        device_id: String,
    },
    /// Opaque Yjs document update (base64-encoded binary)
    CollabUpdate {
        wiki_id: String,
        tiddler_title: String,
        update_base64: String,
    },
    /// Opaque Yjs awareness update (base64-encoded binary)
    CollabAwareness {
        wiki_id: String,
        tiddler_title: String,
        update_base64: String,
    },

    /// A peer saved a tiddler that was being collaboratively edited
    PeerSaved {
        wiki_id: String,
        tiddler_title: String,
        saved_title: String,
        device_id: String,
        device_name: String,
    },

    /// Announce this device's TiddlyWiki username to peers
    UserNameAnnounce {
        user_name: String,
    },

    /// Keepalive
    Ping,
    Pong,
}

impl SyncMessage {
    /// Returns true for bulk-data messages (attachments, wiki file transfers)
    /// that should use the low-priority channel so they don't block tiddler sync.
    pub fn is_bulk_data(&self) -> bool {
        matches!(
            self,
            SyncMessage::AttachmentChunk { .. }
                | SyncMessage::AttachmentChanged { .. }
                | SyncMessage::AttachmentDeleted { .. }
                | SyncMessage::AttachmentManifest { .. }
                | SyncMessage::RequestAttachments { .. }
                | SyncMessage::WikiFileChunk { .. }
                | SyncMessage::WikiFileComplete { .. }
                | SyncMessage::RequestWikiFile { .. }
                | SyncMessage::PluginFileChunk { .. }
                | SyncMessage::PluginManifest { .. }
                | SyncMessage::RequestPluginFiles { .. }
                | SyncMessage::PluginFilesComplete { .. }
        )
    }
}

/// Lightweight fingerprint of a tiddler for diff-based sync
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TiddlerFingerprint {
    pub title: String,
    pub modified: String,
    /// If true, this is a deletion tombstone — the tiddler was intentionally
    /// deleted and peers should delete their copy if it is older.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted: Option<bool>,
}

/// Info about a single file in an attachment manifest
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentFileInfo {
    pub rel_path: String,
    pub sha256_hex: String,
    pub file_size: u64,
}

/// Information about a wiki available for sync
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiInfo {
    pub wiki_id: String,
    pub wiki_name: String,
    pub is_folder: bool,
}

/// A tiddler being sent in a full sync batch
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncTiddler {
    pub title: String,
    pub tiddler_json: String,
    pub vector_clock: VectorClock,
}

/// Vector clock for conflict detection
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VectorClock {
    /// Maps device_id → counter
    pub clocks: std::collections::HashMap<String, u64>,
}

impl VectorClock {
    pub fn new() -> Self {
        Self {
            clocks: std::collections::HashMap::new(),
        }
    }

    /// Increment this device's counter
    pub fn increment(&mut self, device_id: &str) {
        let counter = self.clocks.entry(device_id.to_string()).or_insert(0);
        *counter += 1;
    }

    /// Check if self is strictly newer than other (self dominates)
    pub fn dominates(&self, other: &VectorClock) -> bool {
        // self dominates if every entry in other is <= self,
        // and at least one entry in self is > other
        let mut dominated = false;
        for (id, &other_val) in &other.clocks {
            let self_val = self.clocks.get(id).copied().unwrap_or(0);
            if self_val < other_val {
                return false;
            }
            if self_val > other_val {
                dominated = true;
            }
        }
        // Also check entries in self that aren't in other
        if !dominated {
            for (id, &self_val) in &self.clocks {
                if !other.clocks.contains_key(id) && self_val > 0 {
                    dominated = true;
                    break;
                }
            }
        }
        dominated
    }

    /// Merge another vector clock into this one (take max of each entry)
    pub fn merge(&mut self, other: &VectorClock) {
        for (id, &val) in &other.clocks {
            let entry = self.clocks.entry(id.clone()).or_insert(0);
            if val > *entry {
                *entry = val;
            }
        }
    }

    /// Check if two vector clocks are concurrent (neither dominates)
    pub fn is_concurrent_with(&self, other: &VectorClock) -> bool {
        !self.dominates(other) && !other.dominates(self) && self != other
    }
}

impl PartialEq for VectorClock {
    fn eq(&self, other: &Self) -> bool {
        // Equal if all entries match (treating missing as 0)
        let all_keys: std::collections::HashSet<&String> =
            self.clocks.keys().chain(other.clocks.keys()).collect();
        for key in all_keys {
            let a = self.clocks.get(key).copied().unwrap_or(0);
            let b = other.clocks.get(key).copied().unwrap_or(0);
            if a != b {
                return false;
            }
        }
        true
    }
}

// ── Encryption ──────────────────────────────────────────────────────────────

/// Per-connection encryption state
#[derive(Clone)]
pub struct SessionCipher {
    cipher: ChaCha20Poly1305,
    /// Counter for outgoing nonces (avoids nonce reuse)
    send_counter: u64,
}

impl SessionCipher {
    /// Derive a session key from the long-term shared secret and a random nonce
    pub fn new(long_term_key: &[u8], session_nonce: &[u8]) -> Result<Self, String> {
        let hk = Hkdf::<Sha256>::new(Some(session_nonce), long_term_key);
        let mut session_key = [0u8; 32];
        hk.expand(b"tiddlydesktop-lan-sync-session-key", &mut session_key)
            .map_err(|e| format!("HKDF expand failed: {}", e))?;

        let cipher = ChaCha20Poly1305::new_from_slice(&session_key)
            .map_err(|e| format!("ChaCha20 init failed: {}", e))?;

        Ok(Self {
            cipher,
            send_counter: 0,
        })
    }

    /// Derive a session cipher directly from a SPAKE2 shared secret.
    /// Uses HKDF-SHA256 with a fixed salt (no random nonce needed — SPAKE2
    /// already provides a unique shared secret per session).
    pub fn from_spake2_secret(shared_secret: &[u8]) -> Result<Self, String> {
        let hk = Hkdf::<Sha256>::new(Some(b"tiddlydesktop-spake2-session"), shared_secret);
        let mut session_key = [0u8; 32];
        hk.expand(b"tiddlydesktop-lan-sync-session-key", &mut session_key)
            .map_err(|e| format!("HKDF expand failed: {}", e))?;

        let cipher = ChaCha20Poly1305::new_from_slice(&session_key)
            .map_err(|e| format!("ChaCha20 init failed: {}", e))?;

        Ok(Self {
            cipher,
            send_counter: 0,
        })
    }

    /// Encrypt a message for sending
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, String> {
        let nonce_bytes = self.send_counter.to_le_bytes();
        self.send_counter = self.send_counter.checked_add(1)
            .ok_or("Session nonce counter overflow — connection must be rekeyed")?;

        // ChaCha20-Poly1305 nonce is 12 bytes, we use 8 bytes of counter + 4 zero bytes
        let mut nonce_arr = [0u8; 12];
        nonce_arr[..8].copy_from_slice(&nonce_bytes);
        let nonce = Nonce::from(nonce_arr);

        let ciphertext = self
            .cipher
            .encrypt(&nonce, plaintext)
            .map_err(|e| format!("Encryption failed: {}", e))?;

        // Prepend the 8-byte nonce counter so receiver knows which nonce to use
        let mut frame = Vec::with_capacity(8 + ciphertext.len());
        frame.extend_from_slice(&nonce_bytes);
        frame.extend_from_slice(&ciphertext);
        Ok(frame)
    }

    /// Decrypt a received message
    pub fn decrypt(&self, frame: &[u8]) -> Result<Vec<u8>, String> {
        if frame.len() < 8 {
            return Err("Frame too short".to_string());
        }
        let nonce_bytes = &frame[..8];
        let ciphertext = &frame[8..];

        let mut nonce_arr = [0u8; 12];
        nonce_arr[..8].copy_from_slice(nonce_bytes);
        let nonce = Nonce::from(nonce_arr);

        self.cipher
            .decrypt(&nonce, ciphertext)
            .map_err(|e| format!("Decryption failed: {}", e))
    }
}

// ── SPAKE2 Key Confirmation ──────────────────────────────────────────────────

const SPAKE2_SERVER_CONFIRM_LABEL: &[u8] = b"tiddlydesktop-spake2-server-confirm";
const SPAKE2_CLIENT_CONFIRM_LABEL: &[u8] = b"tiddlydesktop-spake2-client-confirm";

/// Compute a key confirmation tag: HMAC-SHA256(shared_secret, label)[..16] as hex.
/// Different labels for server vs client ensure both sides prove knowledge independently.
pub fn spake2_key_confirm(shared_secret: &[u8], label: &[u8]) -> String {
    use hmac::Mac;
    type HmacSha256 = hmac::Hmac<Sha256>;
    let mut mac = <HmacSha256 as Mac>::new_from_slice(shared_secret)
        .expect("HMAC can take key of any size");
    Mac::update(&mut mac, label);
    let result = mac.finalize().into_bytes();
    result[..16].iter().map(|b| format!("{:02x}", b)).collect()
}

/// Compute server-side key confirmation tag
pub fn spake2_server_confirm(shared_secret: &[u8]) -> String {
    spake2_key_confirm(shared_secret, SPAKE2_SERVER_CONFIRM_LABEL)
}

/// Compute client-side key confirmation tag
pub fn spake2_client_confirm(shared_secret: &[u8]) -> String {
    spake2_key_confirm(shared_secret, SPAKE2_CLIENT_CONFIRM_LABEL)
}

/// Encrypt a SyncMessage for sending over WebSocket
pub fn encrypt_message(cipher: &mut SessionCipher, msg: &SyncMessage) -> Result<Vec<u8>, String> {
    let json = serde_json::to_string(msg).map_err(|e| format!("Serialize failed: {}", e))?;
    cipher.encrypt(json.as_bytes())
}

/// Decrypt a WebSocket frame into a SyncMessage
pub fn decrypt_message(cipher: &SessionCipher, frame: &[u8]) -> Result<SyncMessage, String> {
    let plaintext = cipher.decrypt(frame)?;
    let json_str =
        std::str::from_utf8(&plaintext).map_err(|e| format!("UTF-8 decode failed: {}", e))?;
    serde_json::from_str(json_str).map_err(|e| format!("Deserialize failed: {}", e))
}

// ── Base64 serde helper for Vec<u8> fields ──────────────────────────────────

mod base64_bytes {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &Vec<u8>, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(deserializer)?;
        STANDARD.decode(&s).map_err(serde::de::Error::custom)
    }
}
