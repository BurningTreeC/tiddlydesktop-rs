//! Relay Sync — cloud relay transport for cross-network sync.
//!
//! Works alongside LAN sync. The relay server forwards opaque encrypted blobs
//! between room members. All data is E2E encrypted with ChaCha20-Poly1305
//! (same as LAN sync).
//!
//! Architecture:
//! - Shared rooms: all devices join user-defined rooms (one WebSocket per room)
//! - Group key derived from password + room code (HKDF)
//! - Per-sender session ciphers prevent nonce reuse across senders
//! - `ServerEvent`s are emitted into the same channel as LAN sync

pub mod connection;
pub mod github_auth;

use crate::lan_sync::pairing::PairingManager;
use crate::lan_sync::protocol::{
    decrypt_message, encrypt_message, SessionCipher, SyncMessage,
};
use crate::lan_sync::server::ServerEvent;
use chacha20poly1305::{
    aead::Aead,
    ChaCha20Poly1305, Nonce,
};
use connection::{RelayFrame, RelayReceiver, RelaySender};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::Emitter;
use tokio::sync::{mpsc, RwLock};

/// Relay server URL (TLS via rustls + WebPKI roots)
const DEFAULT_RELAY_URL: &str = "wss://relay.tiddlydesktop-rs.com:8443";

/// Encrypted payloads larger than this are split into chunks (1.5 MB)
const CHUNK_THRESHOLD: usize = 1_500_000;

/// Size of each chunk (1 MB — well under the 2 MB server limit)
const CHUNK_SIZE: usize = 1_000_000;

/// Timeout for incomplete chunk reassembly (30 seconds)
const CHUNK_REASSEMBLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Key for reassembly buffer: (sender_device_id, message_id)
type ChunkKey = (String, [u8; 16]);

struct ChunkReassembly {
    chunks: Vec<Option<Vec<u8>>>,
    total_chunks: u16,
    received_count: u16,
    created_at: Instant,
}

/// Old relay URL (plain WebSocket, pre-TLS) — auto-migrated on config load
const OLD_RELAY_URL: &str = "ws://164.92.180.226:8443";

/// Derive a per-device app token for relay server authentication.
/// Uses HMAC-SHA256(device_key, label) truncated to 16 hex chars with a "tdr1-" prefix.
/// Each installation gets a unique, stable token that isn't hardcoded in the source.
fn derive_app_token(device_key: &[u8; 32]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(device_key)
        .expect("HMAC can take key of any size");
    mac.update(b"tiddlydesktop-relay-app-token");
    let result = mac.finalize().into_bytes();
    let hex: String = result[..8].iter().map(|b| format!("{:02x}", b)).collect();
    format!("tdr1-{}", hex)
}

/// Config file name
const RELAY_CONFIG_FILE: &str = "relay_sync_config.json";

/// Device encryption key file — protects passwords at rest
const DEVICE_KEY_FILE: &str = "relay_device_key";

// ── Device key for at-rest encryption ───────────────────────────────

/// Load or create the device encryption key.
///
/// The actual key is NEVER stored on disk. Instead, we store a random salt
/// and derive the key via HKDF from: machine fingerprint + salt.
/// The machine fingerprint comes from OS-level identifiers that aren't in
/// appdata (e.g. /etc/machine-id on Linux, MachineGuid on Windows,
/// IOPlatformUUID on macOS), so an attacker with only the appdata directory
/// cannot reconstruct the key.
fn load_or_create_device_key(data_dir: &std::path::Path) -> [u8; 32] {
    let salt_path = data_dir.join(DEVICE_KEY_FILE);
    let salt = if let Ok(bytes) = std::fs::read(&salt_path) {
        if bytes.len() == 32 {
            let mut s = [0u8; 32];
            s.copy_from_slice(&bytes);
            s
        } else {
            create_and_save_salt(&salt_path)
        }
    } else {
        create_and_save_salt(&salt_path)
    };
    derive_device_key(&salt)
}

/// Create a new random 32-byte salt and save it to disk.
fn create_and_save_salt(path: &std::path::Path) -> [u8; 32] {
    use rand::RngCore;
    let mut salt = [0u8; 32];
    rand::rng().fill_bytes(&mut salt);
    let _ = std::fs::write(path, &salt);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    salt
}

/// Derive the device encryption key from salt + machine fingerprint.
/// The fingerprint is gathered from OS-level sources outside appdata.
fn derive_device_key(salt: &[u8; 32]) -> [u8; 32] {
    let fingerprint = get_machine_fingerprint();
    let hk = Hkdf::<Sha256>::new(Some(salt), fingerprint.as_bytes());
    let mut key = [0u8; 32];
    hk.expand(b"tiddlydesktop-relay-device-key", &mut key)
        .expect("32 bytes is valid for HKDF-SHA256");
    key
}

/// Collect machine-specific entropy that isn't stored in appdata.
fn get_machine_fingerprint() -> String {
    let mut parts = Vec::new();

    // Linux: /etc/machine-id (unique per installation, survives reboots)
    #[cfg(target_os = "linux")]
    {
        if let Ok(id) = std::fs::read_to_string("/etc/machine-id") {
            parts.push(id.trim().to_string());
        }
        // Also include UID for multi-user systems
        #[cfg(unix)]
        {
            parts.push(format!("uid:{}", unsafe { libc::getuid() }));
        }
    }

    // macOS: IOPlatformUUID via ioreg
    #[cfg(target_os = "macos")]
    {
        if let Ok(output) = std::process::Command::new("ioreg")
            .args(["-rd1", "-c", "IOPlatformExpertDevice"])
            .output()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                if line.contains("IOPlatformUUID") {
                    if let Some(uuid) = line.split('"').nth(3) {
                        parts.push(uuid.to_string());
                    }
                }
            }
        }
        #[cfg(unix)]
        {
            parts.push(format!("uid:{}", unsafe { libc::getuid() }));
        }
    }

    // Windows: MachineGuid from registry
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        if let Ok(output) = std::process::Command::new("reg")
            .args(["query", r"HKLM\SOFTWARE\Microsoft\Cryptography", "/v", "MachineGuid"])
            .creation_flags(0x08000000) // CREATE_NO_WINDOW
            .output()
        {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines() {
                if line.contains("MachineGuid") {
                    if let Some(guid) = line.split_whitespace().last() {
                        parts.push(guid.to_string());
                    }
                }
            }
        }
        // Also include username
        if let Ok(user) = std::env::var("USERNAME") {
            parts.push(format!("user:{}", user));
        }
    }

    // Android: android_id or Build.SERIAL aren't easily accessible from Rust,
    // but the app-private directory is already sandboxed. Use a fallback.
    #[cfg(target_os = "android")]
    {
        // Android's app sandbox already protects appdata. Use package-specific path as entropy.
        parts.push("android-sandbox".to_string());
    }

    // Fallback if nothing was collected
    if parts.is_empty() {
        parts.push("tiddlydesktop-fallback".to_string());
    }

    parts.join("|")
}

/// Encrypt a password string → base64(nonce || ciphertext)
fn encrypt_password(device_key: &[u8; 32], plaintext: &str) -> String {
    use base64::Engine;
    use chacha20poly1305::KeyInit;
    use rand::RngCore;
    let cipher = ChaCha20Poly1305::new(device_key.into());
    let mut nonce_bytes = [0u8; 12];
    rand::rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher.encrypt(nonce, plaintext.as_bytes()).expect("encryption failed");
    let mut combined = Vec::with_capacity(12 + ciphertext.len());
    combined.extend_from_slice(&nonce_bytes);
    combined.extend_from_slice(&ciphertext);
    base64::engine::general_purpose::STANDARD.encode(&combined)
}

/// Decrypt a base64(nonce || ciphertext) → password string
fn decrypt_password(device_key: &[u8; 32], encrypted: &str) -> Option<String> {
    use base64::Engine;
    use chacha20poly1305::KeyInit;
    let combined = base64::engine::general_purpose::STANDARD.decode(encrypted).ok()?;
    if combined.len() < 13 {
        return None; // nonce (12) + at least 1 byte ciphertext+tag
    }
    let cipher = ChaCha20Poly1305::new(device_key.into());
    let nonce = Nonce::from_slice(&combined[..12]);
    let plaintext = cipher.decrypt(nonce, &combined[12..]).ok()?;
    String::from_utf8(plaintext).ok()
}

/// Save config to disk, encrypting all passwords before writing.
/// This is a sync function so it can be called from the constructor.
/// Returns an error if serialization or writing fails.
fn save_config_sync(config_path: &std::path::Path, device_key: &[u8; 32], config: &RelayConfig) -> Result<(), String> {
    // Clone config, encrypt passwords and tokens for serialization
    let mut disk_config = config.clone();
    for room in &mut disk_config.rooms {
        if !room.password.is_empty() {
            room.encrypted_password = Some(encrypt_password(device_key, &room.password));
        }
        // password has skip_serializing so it won't appear in output
    }
    // Encrypt auth token for disk storage
    if !disk_config.auth_token.is_empty() {
        disk_config.encrypted_auth_token = Some(encrypt_password(device_key, &disk_config.auth_token));
    }
    // auth_token has skip_serializing so it won't appear in output
    let json = serde_json::to_string_pretty(&disk_config)
        .map_err(|e| format!("Failed to serialize relay config: {}", e))?;
    std::fs::write(config_path, json)
        .map_err(|e| format!("Failed to write relay config to {}: {}", config_path.display(), e))
}

/// Normalize a relay URL: ensure it has a `wss://` scheme and `:8443` port.
/// Handles bare domains like `relay.example.com` → `wss://relay.example.com:8443`.
fn normalize_relay_url(url: &str) -> String {
    let url = url.trim();
    // Add scheme if missing
    let url = if url.starts_with("wss://") || url.starts_with("ws://") {
        url.to_string()
    } else {
        format!("wss://{}", url)
    };
    // Upgrade ws:// to wss://
    let url = if url.starts_with("ws://") {
        format!("wss://{}", &url[5..])
    } else {
        url
    };
    // Add default port if missing (no colon after the host)
    let after_scheme = &url[6..]; // skip "wss://"
    let has_port = after_scheme.split('/').next().unwrap_or("").contains(':');
    if !has_port {
        // Insert :8443 before the first / (path) or at the end
        if let Some(slash_pos) = after_scheme.find('/') {
            format!("wss://{}:8443{}", &after_scheme[..slash_pos], &after_scheme[slash_pos..])
        } else {
            format!("{}:8443", url)
        }
    } else {
        url
    }
}

// ── Config ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RelayConfig {
    pub relay_url: String,
    /// Legacy field — kept for backward compat with old config files.
    /// Ignored in the new rooms model (each room has its own auto_connect).
    #[serde(default)]
    pub auto_connect: bool,
    /// User-defined rooms
    #[serde(default)]
    pub rooms: Vec<RoomDefinition>,
    /// Auth token (in-memory only — never serialized to disk)
    #[serde(default, skip_serializing)]
    pub auth_token: String,
    /// Encrypted auth token (written to disk)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_auth_token: Option<String>,
    /// Auth provider name: "github", "gitlab", "oidc"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_provider: Option<String>,
    /// Username (for display)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// User ID (e.g. "github:12345", "gitlab:67890", "oidc:sub")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    // Legacy fields for deserializing old config files — never written back
    #[serde(default, skip_serializing, alias = "github_token")]
    _legacy_github_token: String,
    #[serde(default, skip_serializing, alias = "encrypted_github_token")]
    _legacy_encrypted_github_token: Option<String>,
    #[serde(default, skip_serializing, alias = "github_login")]
    _legacy_github_login: Option<String>,
    #[serde(default, skip_serializing, alias = "github_id")]
    _legacy_github_id: Option<i64>,
}

impl RelayConfig {
    /// Migrate legacy GitHub fields to the new generic auth fields.
    /// Called after deserialization to handle old config files.
    fn migrate_legacy_fields(&mut self) {
        // Migrate encrypted token
        if self.encrypted_auth_token.is_none() {
            if let Some(ref enc) = self._legacy_encrypted_github_token {
                self.encrypted_auth_token = Some(enc.clone());
            }
        }
        // Migrate in-memory token
        if self.auth_token.is_empty() && !self._legacy_github_token.is_empty() {
            self.auth_token = self._legacy_github_token.clone();
        }
        // Migrate username
        if self.username.is_none() {
            if let Some(ref login) = self._legacy_github_login {
                self.username = Some(login.clone());
            }
        }
        // Migrate user ID
        if self.user_id.is_none() {
            if let Some(id) = self._legacy_github_id {
                self.user_id = Some(format!("github:{}", id));
            }
        }
        // Default provider to github if we have a token but no provider
        if self.auth_provider.is_none() && (self.encrypted_auth_token.is_some() || !self.auth_token.is_empty()) {
            self.auth_provider = Some("github".to_string());
        }
        // Clear legacy fields
        self._legacy_github_token.clear();
        self._legacy_encrypted_github_token = None;
        self._legacy_github_login = None;
        self._legacy_github_id = None;
    }
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            relay_url: DEFAULT_RELAY_URL.to_string(),
            auto_connect: false,
            rooms: Vec::new(),
            auth_token: String::new(),
            encrypted_auth_token: None,
            auth_provider: None,
            username: None,
            user_id: None,
            _legacy_github_token: String::new(),
            _legacy_encrypted_github_token: None,
            _legacy_github_login: None,
            _legacy_github_id: None,
        }
    }
}

/// A user-defined relay room.
/// The `password` field is only held in memory — on disk it's stored encrypted
/// in `encrypted_password`. The `password` field is populated at load time by
/// decrypting `encrypted_password` with the device key.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RoomDefinition {
    pub name: String,
    pub room_code: String,
    /// In-memory only — never serialized to disk.
    #[serde(default, skip_serializing)]
    pub password: String,
    /// Encrypted password (base64 of nonce || ciphertext). Written to disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_password: Option<String>,
    #[serde(default)]
    pub auto_connect: bool,
}

// ── Room connection state ───────────────────────────────────────────

/// Active connection to a single room
struct RoomConnection {
    room_def: RoomDefinition,
    sender: RelaySender,
    encrypt_cipher: SessionCipher,
    our_session_nonce: [u8; 32],
    /// Per-sender decrypt ciphers (sender_device_id → cipher)
    decrypt_ciphers: HashMap<String, SessionCipher>,
    /// Previous ciphers kept briefly for messages in-flight during session rekeying
    old_decrypt_ciphers: HashMap<String, SessionCipher>,
    /// device_id → device_name for connected members
    member_names: HashMap<String, String>,
}

// ── Room status (returned to UI) ────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RoomStatus {
    pub name: String,
    pub room_code: String,
    pub password: String,
    pub auto_connect: bool,
    pub connected: bool,
    pub connected_peers: Vec<RoomPeerInfo>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RoomPeerInfo {
    pub device_id: String,
    pub device_name: String,
}

/// Generate a random 8-character code (alphanumeric, no ambiguous chars)
pub fn generate_room_code() -> String {
    use rand::Rng;
    let chars: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::rng();
    let mut code = String::with_capacity(8);
    for _ in 0..8 {
        code.push(chars[rng.random_range(0..chars.len())] as char);
    }
    code
}

/// Generate a random 8-character alphanumeric password
pub fn generate_room_password() -> String {
    use rand::Rng;
    let chars: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
    let mut rng = rand::rng();
    let mut pw = String::with_capacity(8);
    for _ in 0..8 {
        pw.push(chars[rng.random_range(0..chars.len())] as char);
    }
    pw
}

// ── RelaySyncManager ────────────────────────────────────────────────

pub struct RelaySyncManager {
    config: RwLock<RelayConfig>,
    config_path: PathBuf,
    device_key: [u8; 32],
    pairing_manager: Arc<PairingManager>,
    /// Channel to emit events into the main sync event loop
    event_tx: mpsc::UnboundedSender<ServerEvent>,
    /// Active room connections (room_code → connection)
    rooms: Arc<RwLock<HashMap<String, RoomConnection>>>,
    /// Per-room running flags (room_code → flag). Used to stop individual room tasks.
    room_running: Arc<RwLock<HashMap<String, Arc<AtomicBool>>>>,
    /// Rooms the user has manually activated (clicked "Connect" on).
    /// Used to report `connected = true` for LAN-only rooms where relay is unavailable.
    manually_activated: RwLock<HashSet<String>>,
}

impl RelaySyncManager {
    pub fn new(
        data_dir: &std::path::Path,
        pairing_manager: Arc<PairingManager>,
        event_tx: mpsc::UnboundedSender<ServerEvent>,
    ) -> Self {
        let device_key = load_or_create_device_key(data_dir);
        let config_path = data_dir.join(RELAY_CONFIG_FILE);
        let mut config: RelayConfig = if config_path.exists() {
            match std::fs::read_to_string(&config_path) {
                Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
                Err(_) => RelayConfig::default(),
            }
        } else {
            RelayConfig::default()
        };

        // Migrate old ws:// relay URL to wss://
        let mut needs_save = false;
        if config.relay_url == OLD_RELAY_URL {
            eprintln!("[Relay] Migrating relay URL from ws:// to wss://");
            config.relay_url = DEFAULT_RELAY_URL.to_string();
            needs_save = true;
        }

        // Normalize relay URL: ensure it has a wss:// scheme and :8443 port
        config.relay_url = normalize_relay_url(&config.relay_url);
        if config.relay_url != DEFAULT_RELAY_URL {
            eprintln!("[Relay] Normalized relay URL to: {}", config.relay_url);
        }

        // Decrypt passwords from encrypted_password, or migrate plain-text passwords
        for room in &mut config.rooms {
            if let Some(ref enc) = room.encrypted_password {
                // Decrypt into in-memory password field
                if let Some(pw) = decrypt_password(&device_key, enc) {
                    room.password = pw;
                } else {
                    eprintln!("[Relay] Warning: failed to decrypt password for room {}", room.room_code);
                }
            } else if !room.password.is_empty() {
                // Migration: old config with plain-text password — encrypt on next save
                needs_save = true;
            }
        }

        // Migrate legacy GitHub fields to new generic auth fields
        config.migrate_legacy_fields();

        // Decrypt auth token
        if let Some(ref enc) = config.encrypted_auth_token {
            if let Some(token) = decrypt_password(&device_key, enc) {
                config.auth_token = token;
            } else {
                eprintln!("[Relay] Warning: failed to decrypt auth token");
                config.encrypted_auth_token = None;
                config.username = None;
                config.user_id = None;
                config.auth_provider = None;
                needs_save = true;
            }
        }

        let mgr = Self {
            config: RwLock::new(config),
            config_path,
            device_key,
            pairing_manager,
            event_tx,
            rooms: Arc::new(RwLock::new(HashMap::new())),
            room_running: Arc::new(RwLock::new(HashMap::new())),
            manually_activated: RwLock::new(HashSet::new()),
        };

        // If we need to migrate plain-text passwords, encrypt and save now
        if needs_save {
            let config_path = mgr.config_path.clone();
            let device_key = mgr.device_key;
            let config = mgr.config.blocking_read().clone();
            if let Err(e) = save_config_sync(&config_path, &device_key, &config) {
                eprintln!("[Relay] Warning: Failed to save migrated config: {}", e);
            }
        }

        mgr
    }

    /// Save config to disk (passwords are encrypted before writing)
    async fn save_config(&self) {
        let config = self.config.read().await.clone();
        if let Err(e) = save_config_sync(&self.config_path, &self.device_key, &config) {
            eprintln!("[Relay] Failed to save config: {}", e);
        }
    }

    /// Persist current config to disk and verify the write succeeded.
    /// Used at startup to ensure on-disk state matches in-memory state before sync begins.
    pub async fn persist_and_verify_config(&self) -> Result<(), String> {
        let config = self.config.read().await.clone();
        save_config_sync(&self.config_path, &self.device_key, &config)
    }

    // ── Key derivation for shared rooms ──────────────────────────────

    /// Derive a 32-byte group key from password and room code.
    /// All room members with the same password + code derive the same key.
    pub fn derive_group_key(password: &str, room_code: &str) -> [u8; 32] {
        let hk = Hkdf::<Sha256>::new(Some(room_code.as_bytes()), password.as_bytes());
        let mut key = [0u8; 32];
        hk.expand(b"tiddlydesktop-relay-group-key", &mut key)
            .expect("32 bytes is valid for HKDF-SHA256");
        key
    }

    /// Derive a room token from the group key (for server authentication).
    /// HMAC-SHA256(group_key, "relay-room-token"), hex first 16 bytes.
    pub fn derive_room_token_from_key(group_key: &[u8; 32]) -> String {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(group_key).expect("HMAC can take key of any size");
        mac.update(b"relay-room-token");
        let result = mac.finalize().into_bytes();
        result[..16]
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect()
    }

    // ── Room management ─────────────────────────────────────────────

    /// Add a new room to the config
    pub async fn add_room(
        &self,
        name: String,
        room_code: String,
        password: String,
        auto_connect: bool,
    ) -> Result<(), String> {
        {
            let mut config = self.config.write().await;
            // Check for duplicate room code
            if config.rooms.iter().any(|r| r.room_code == room_code) {
                return Err(format!("Room with code '{}' already exists", room_code));
            }
            config.rooms.push(RoomDefinition {
                name,
                room_code,
                password,
                encrypted_password: None, // will be set on save_config
                auto_connect,
            });
        }
        self.save_config().await;
        Ok(())
    }

    /// Remove a room from config, disconnect if connected
    pub async fn remove_room(&self, room_code: &str) -> Result<(), String> {
        // Disconnect first if connected
        self.disconnect_room(room_code).await;

        {
            let mut config = self.config.write().await;
            config.rooms.retain(|r| r.room_code != room_code);
        }
        self.save_config().await;
        Ok(())
    }

    /// Connect to a specific room
    pub async fn connect_room(&self, room_code: &str) -> Result<(), String> {
        // Check if already connected
        if self.rooms.read().await.contains_key(room_code) {
            return Ok(());
        }

        // Check if a task is already running (e.g., in reconnect backoff)
        if let Some(flag) = self.room_running.read().await.get(room_code) {
            if flag.load(Ordering::Relaxed) {
                eprintln!("[Relay] Room {} already has a running task, skipping duplicate spawn", room_code);
                return Ok(());
            }
        }

        // Find room definition
        let config = self.config.read().await.clone();
        let room_def = config
            .rooms
            .iter()
            .find(|r| r.room_code == room_code)
            .ok_or_else(|| format!("Room '{}' not found in config", room_code))?
            .clone();

        // Auth token is required for relay connection
        if config.auth_token.is_empty() {
            return Err("Authentication required. Please sign in first.".to_string());
        }

        let provider = config.auth_provider.clone().unwrap_or_else(|| "github".to_string());
        self.spawn_room_task(config.relay_url.clone(), room_def, config.auth_token.clone(), provider).await;
        Ok(())
    }

    /// Disconnect from a specific room
    pub async fn disconnect_room(&self, room_code: &str) {
        // Set the running flag to false to stop the room task
        {
            let running_map = self.room_running.read().await;
            if let Some(flag) = running_map.get(room_code) {
                flag.store(false, Ordering::Relaxed);
            }
        }

        // Close the WebSocket connection and remove the room
        let mut rooms = self.rooms.write().await;
        if let Some(room) = rooms.remove(room_code) {
            eprintln!("[Relay] Disconnecting from room {}", room_code);
            // Emit PeerDisconnected for all members
            for (device_id, _) in &room.member_names {
                let _ = self.event_tx.send(ServerEvent::PeerDisconnected {
                    device_id: device_id.clone(),
                });
            }
            room.sender.close().await;
        }

        // Clean up running flag
        self.room_running.write().await.remove(room_code);
    }

    /// Mark a room as manually activated (for LAN-only connect when relay is unavailable).
    pub async fn activate_room(&self, room_code: &str) {
        self.manually_activated.write().await.insert(room_code.to_string());
    }

    /// Remove manual activation flag (user clicked Disconnect).
    pub async fn deactivate_room(&self, room_code: &str) {
        self.manually_activated.write().await.remove(room_code);
    }

    /// Connect all auto_connect rooms
    pub async fn start_all(&self) -> Result<(), String> {
        let config = self.config.read().await.clone();
        eprintln!("[Relay] Starting auto-connect rooms (url: {})", config.relay_url);

        // Activate all auto_connect rooms so they appear in LAN discovery
        // beacons and show as "connected" in the UI. This is safe because
        // disconnect sets auto_connect=false, so only rooms the user
        // intentionally left connected will be re-activated.
        for room_def in &config.rooms {
            if room_def.auto_connect {
                self.activate_room(&room_def.room_code).await;
            }
        }

        // Auth token required for relay — skip relay if not authenticated
        // (LAN sync still works via activate_room above)
        if config.auth_token.is_empty() {
            eprintln!("[Relay] Skipping relay auto-connect — no auth token (LAN sync still works)");
            return Ok(());
        }

        let provider = config.auth_provider.clone().unwrap_or_else(|| "github".to_string());
        for room_def in &config.rooms {
            if room_def.auto_connect {
                if !self.rooms.read().await.contains_key(&room_def.room_code) {
                    self.spawn_room_task(config.relay_url.clone(), room_def.clone(), config.auth_token.clone(), provider.clone()).await;
                }
            }
        }
        Ok(())
    }

    /// Disconnect all rooms
    pub async fn stop_all(&self) {
        let room_codes: Vec<String> = self.rooms.read().await.keys().cloned().collect();
        for code in room_codes {
            self.disconnect_room(&code).await;
        }
        eprintln!("[Relay] All rooms disconnected");
    }

    /// Set auto-connect for a specific room
    pub async fn set_room_auto_connect(&self, room_code: &str, auto_connect: bool) {
        {
            let mut config = self.config.write().await;
            if let Some(room) = config.rooms.iter_mut().find(|r| r.room_code == room_code) {
                room.auto_connect = auto_connect;
            }
        }
        self.save_config().await;
    }

    /// Update a room's password (disconnects if connected, since keys change)
    pub async fn set_room_password(&self, room_code: &str, password: String) -> Result<(), String> {
        // Disconnect first — the old group key is no longer valid
        self.disconnect_room(room_code).await;

        {
            let mut config = self.config.write().await;
            if let Some(room) = config.rooms.iter_mut().find(|r| r.room_code == room_code) {
                room.password = password;
            } else {
                return Err(format!("Room '{}' not found", room_code));
            }
        }
        self.save_config().await;
        Ok(())
    }

    /// Update a room's display name
    pub async fn set_room_name(&self, room_code: &str, name: String) -> Result<(), String> {
        {
            let mut config = self.config.write().await;
            if let Some(room) = config.rooms.iter_mut().find(|r| r.room_code == room_code) {
                room.name = name;
            } else {
                return Err(format!("Room '{}' not found", room_code));
            }
        }
        self.save_config().await;
        Ok(())
    }

    /// Get credentials for a specific room (for sharing with others)
    pub async fn get_room_credentials(&self, room_code: &str) -> Option<(String, String, String)> {
        let config = self.config.read().await;
        config.rooms.iter()
            .find(|r| r.room_code == room_code)
            .map(|r| (r.name.clone(), r.room_code.clone(), r.password.clone()))
    }

    // ── Spawn a long-lived task for one room ────────────────────────

    async fn spawn_room_task(&self, relay_url: String, room_def: RoomDefinition, auth_token: String, auth_provider: String) {
        let event_tx = self.event_tx.clone();
        let rooms = self.rooms.clone();
        let room_running = self.room_running.clone();
        let my_device_id = self.pairing_manager.device_id().to_string();
        let my_device_name = self.pairing_manager.device_name().to_string();
        let room_code = room_def.room_code.clone();

        // Create a per-room running flag — insert SYNCHRONOUSLY before spawning the task
        // to prevent duplicate tasks from being spawned by concurrent connect_room calls.
        let running = Arc::new(AtomicBool::new(true));
        self.room_running.write().await.insert(room_code.clone(), running.clone());

        let running_for_task = running.clone();

        tokio::spawn(async move {
            let mut delay = std::time::Duration::from_secs(2);
            let max_delay = std::time::Duration::from_secs(30);
            let mut first_connect = true;

            let group_key = Self::derive_group_key(&room_def.password, &room_def.room_code);
            let room_token = Self::derive_room_token_from_key(&group_key);

            eprintln!(
                "[Relay] Room {} password len={}, token={}",
                room_code,
                room_def.password.len(),
                &room_token[..8]
            );

            loop {
                if !running_for_task.load(Ordering::Relaxed) {
                    break;
                }

                // Backoff wait (skip on first connect)
                if !first_connect {
                    tokio::time::sleep(delay).await;
                    if !running_for_task.load(Ordering::Relaxed) {
                        break;
                    }
                }
                first_connect = false;

                // Try to connect
                let room_hash = crate::lan_sync::discovery::hash_room_code(&room_code);
                let url = format!("{}/room/{}", relay_url, room_hash);
                let (sender, receiver) = match connection::connect(
                    &url,
                    &my_device_id,
                    &auth_token,
                    &auth_provider,
                    Some(&room_token),
                )
                .await
                {
                    Ok(pair) => pair,
                    Err(e) => {
                        eprintln!(
                            "[Relay] Connect to room {} ({}) failed: {}",
                            room_def.name, room_code, e
                        );
                        delay = (delay * 2).min(max_delay);
                        continue;
                    }
                };

                // Generate session nonce and create encrypt cipher
                let mut session_nonce = [0u8; 32];
                rand::Fill::fill(&mut session_nonce, &mut rand::rng());
                let encrypt_cipher = match SessionCipher::new(&group_key, &session_nonce) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("[Relay] Cipher creation failed: {}", e);
                        delay = (delay * 2).min(max_delay);
                        continue;
                    }
                };

                // Send session_init: [0x01][32 bytes nonce][device_id UTF-8]
                let mut init_frame = Vec::with_capacity(1 + 32 + my_device_id.len());
                init_frame.push(0x01);
                init_frame.extend_from_slice(&session_nonce);
                init_frame.extend_from_slice(my_device_id.as_bytes());
                if let Err(e) = sender.send_binary(init_frame).await {
                    eprintln!("[Relay] Failed to send session_init: {}", e);
                    delay = (delay * 2).min(max_delay);
                    continue;
                }

                eprintln!(
                    "[Relay] Connected to room {} ({})",
                    room_def.name, room_code
                );

                // Store the room connection
                {
                    let conn = RoomConnection {
                        room_def: room_def.clone(),
                        sender,
                        encrypt_cipher,
                        our_session_nonce: session_nonce,
                        decrypt_ciphers: HashMap::new(),
                        old_decrypt_ciphers: HashMap::new(),
                        member_names: HashMap::new(),
                    };
                    rooms.write().await.insert(room_code.clone(), conn);
                }

                // Notify UI that the room connected
                if let Some(app) = crate::GLOBAL_APP_HANDLE.get() {
                    let _ = app.emit("relay-room-connected", serde_json::json!({
                        "room_code": room_code
                    }));
                }

                // Broadcast wiki manifest so existing peers in this room learn our wikis
                if let Some(mgr) = crate::lan_sync::get_sync_manager() {
                    let mgr = mgr.clone();
                    tokio::spawn(async move {
                        // Small delay so session_init handshakes with existing room members complete first
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        mgr.broadcast_wiki_manifest().await;
                    });
                }

                // Reset backoff on successful connect
                delay = std::time::Duration::from_secs(2);

                // Run receive loop (blocks until disconnect)
                Self::room_receive_loop(
                    receiver,
                    &room_code,
                    &group_key,
                    &my_device_id,
                    &my_device_name,
                    &event_tx,
                    &rooms,
                    &running_for_task,
                )
                .await;

                // Disconnected — clean up
                eprintln!("[Relay] Disconnected from room {} ({})", room_def.name, room_code);
                {
                    let mut rooms_guard = rooms.write().await;
                    if let Some(room) = rooms_guard.remove(&room_code) {
                        for (device_id, _) in &room.member_names {
                            let _ = event_tx.send(ServerEvent::PeerDisconnected {
                                device_id: device_id.clone(),
                            });
                        }
                    }
                }

                // Notify UI that the room disconnected
                if let Some(app) = crate::GLOBAL_APP_HANDLE.get() {
                    let _ = app.emit("relay-room-disconnected", serde_json::json!({
                        "room_code": room_code
                    }));
                }

                // Loop will continue with backoff to reconnect
            }
        });
    }

    /// Receive loop for a room — handles session inits, data, and control messages.
    /// Returns when the connection is closed.
    async fn room_receive_loop(
        mut receiver: RelayReceiver,
        room_code: &str,
        group_key: &[u8; 32],
        my_device_id: &str,
        my_device_name: &str,
        event_tx: &mpsc::UnboundedSender<ServerEvent>,
        rooms: &Arc<RwLock<HashMap<String, RoomConnection>>>,
        running: &Arc<AtomicBool>,
    ) {
        // Reassembly buffer for chunked messages (0x03 frames)
        let mut reassembly_buffers: HashMap<ChunkKey, ChunkReassembly> = HashMap::new();

        // Receive timeout: server pings every 30s, so if nothing arrives
        // within 90s the connection is likely dead (Android Doze, NAT timeout, etc.)
        const RECV_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

        loop {
            // Prune expired reassembly buffers
            reassembly_buffers.retain(|_key, entry| {
                entry.created_at.elapsed() < CHUNK_REASSEMBLY_TIMEOUT
            });

            let frame = tokio::time::timeout(RECV_TIMEOUT, receiver.recv()).await;
            let frame = match frame {
                Ok(f) => f,
                Err(_) => {
                    // Timeout — no frames received in 90s, connection presumed dead
                    eprintln!(
                        "[Relay] Room {}: receive timeout ({}s), disconnecting",
                        room_code,
                        RECV_TIMEOUT.as_secs()
                    );
                    return;
                }
            };

            match frame {
                Some(RelayFrame::Binary(data)) => {
                    if data.is_empty() {
                        continue;
                    }

                    match data[0] {
                        // Session init: [0x01][32 bytes nonce][device_id UTF-8]
                        0x01 => {
                            if data.len() <= 33 {
                                continue;
                            }
                            let nonce = &data[1..33];
                            let from_device = String::from_utf8_lossy(&data[33..]).to_string();

                            // Don't process our own session_init
                            if from_device == my_device_id {
                                continue;
                            }

                            match SessionCipher::new(group_key, nonce) {
                                Ok(cipher) => {
                                    eprintln!(
                                        "[Relay] Room {}: received session init from {}",
                                        room_code,
                                        &from_device[..8.min(from_device.len())]
                                    );
                                    // Prepare reciprocal init frame outside the write lock
                                    // to avoid holding the lock during async send_binary
                                    let mut reciprocal_init: Option<Vec<u8>> = None;
                                    {
                                        let mut rooms_guard = rooms.write().await;
                                        if let Some(room) = rooms_guard.get_mut(room_code) {
                                            let is_new = !room.decrypt_ciphers.contains_key(&from_device);
                                            // Save old cipher for in-flight message decryption during rekey
                                            if let Some(old_cipher) = room.decrypt_ciphers.get(&from_device) {
                                                room.old_decrypt_ciphers.insert(from_device.clone(), old_cipher.clone());
                                            }
                                            room.decrypt_ciphers.insert(from_device.clone(), cipher);
                                            // Store the device name from the device_id for now
                                            // (will be updated when we receive their device_name via a sync message)
                                            if is_new {
                                                room.member_names
                                                    .entry(from_device.clone())
                                                    .or_insert_with(|| {
                                                        from_device[..8.min(from_device.len())].to_string()
                                                    });
                                                // Emit PeerConnected for the new member
                                                let name = room.member_names.get(&from_device)
                                                    .cloned()
                                                    .unwrap_or_default();
                                                let _ = event_tx.send(ServerEvent::PeerConnected {
                                                    device_id: from_device.clone(),
                                                    device_name: name,
                                                });
                                                // Prepare reciprocal session_init (sent after lock release)
                                                let mut init_frame =
                                                    Vec::with_capacity(1 + 32 + my_device_id.len());
                                                init_frame.push(0x01);
                                                init_frame
                                                    .extend_from_slice(&room.our_session_nonce);
                                                init_frame.extend_from_slice(my_device_id.as_bytes());
                                                reciprocal_init = Some(init_frame);
                                            }
                                        }
                                    } // Write lock released here
                                    // Send reciprocal session_init outside the write lock
                                    if let Some(init_frame) = reciprocal_init {
                                        let rooms_guard = rooms.read().await;
                                        if let Some(room) = rooms_guard.get(room_code) {
                                            let _ = room.sender.send_binary(init_frame).await;
                                        }
                                    }
                                }
                                Err(e) => {
                                    eprintln!(
                                        "[Relay] Room {}: failed to create decrypt cipher: {}",
                                        room_code, e
                                    );
                                }
                            }
                        }

                        // Data message: [0x02][2-byte sender_id_len LE][sender_id UTF-8]
                        //               [1-byte mode: 0x00=broadcast, 0x01=targeted]
                        //               [if targeted: 2-byte recipient_len LE][recipient_id UTF-8]
                        //               [encrypted payload (nonce counter + ciphertext)]
                        0x02 => {
                            if data.len() < 4 {
                                continue;
                            }
                            let sender_id_len =
                                u16::from_le_bytes([data[1], data[2]]) as usize;
                            if data.len() < 3 + sender_id_len + 1 {
                                continue;
                            }
                            let sender_id =
                                String::from_utf8_lossy(&data[3..3 + sender_id_len]).to_string();

                            // Don't process our own messages
                            if sender_id == my_device_id {
                                continue;
                            }

                            let mode_offset = 3 + sender_id_len;
                            let mode = data[mode_offset];
                            let payload_offset;

                            if mode == 0x01 {
                                // Targeted message — check if we're the recipient
                                if data.len() < mode_offset + 3 {
                                    continue;
                                }
                                let recipient_len = u16::from_le_bytes([
                                    data[mode_offset + 1],
                                    data[mode_offset + 2],
                                ]) as usize;
                                if data.len() < mode_offset + 3 + recipient_len {
                                    continue;
                                }
                                let recipient_id = String::from_utf8_lossy(
                                    &data[mode_offset + 3..mode_offset + 3 + recipient_len],
                                );
                                if recipient_id != my_device_id {
                                    continue; // Not for us
                                }
                                payload_offset = mode_offset + 3 + recipient_len;
                            } else {
                                // Broadcast
                                payload_offset = mode_offset + 1;
                            }

                            if payload_offset >= data.len() {
                                continue;
                            }
                            let encrypted_payload = &data[payload_offset..];

                            // Decrypt using sender's cipher (fall back to old cipher for in-flight messages during rekey)
                            let rooms_guard = rooms.read().await;
                            if let Some(room) = rooms_guard.get(room_code) {
                                if let Some(cipher) = room.decrypt_ciphers.get(&sender_id) {
                                    match decrypt_message(cipher, encrypted_payload) {
                                        Ok(message) => {
                                            let _ = event_tx.send(
                                                ServerEvent::SyncMessageReceived {
                                                    from_device_id: sender_id.clone(),
                                                    message,
                                                },
                                            );
                                        }
                                        Err(_) => {
                                            // Try old cipher (message may have been encrypted before rekey)
                                            if let Some(old_cipher) = room.old_decrypt_ciphers.get(&sender_id) {
                                                match decrypt_message(old_cipher, encrypted_payload) {
                                                    Ok(message) => {
                                                        eprintln!(
                                                            "[Relay] Room {}: decrypted with old cipher from {}",
                                                            room_code,
                                                            &sender_id[..8.min(sender_id.len())]
                                                        );
                                                        let _ = event_tx.send(
                                                            ServerEvent::SyncMessageReceived {
                                                                from_device_id: sender_id.clone(),
                                                                message,
                                                            },
                                                        );
                                                    }
                                                    Err(e2) => {
                                                        eprintln!(
                                                            "[Relay] Room {}: decrypt failed from {} (both ciphers): {}",
                                                            room_code,
                                                            &sender_id[..8.min(sender_id.len())],
                                                            e2
                                                        );
                                                    }
                                                }
                                            } else {
                                                eprintln!(
                                                    "[Relay] Room {}: decrypt failed from {} (no old cipher)",
                                                    room_code,
                                                    &sender_id[..8.min(sender_id.len())]
                                                );
                                            }
                                        }
                                    }
                                }
                                // If no cipher for sender, silently drop
                            }
                        }

                        // Chunk frame: [0x03][2-byte sender_id_len LE][sender_id UTF-8]
                        //              [1-byte mode: 0x00=broadcast, 0x01=targeted]
                        //              [if targeted: 2-byte recipient_len LE][recipient_id UTF-8]
                        //              [16-byte message_id][2-byte chunk_index LE][2-byte total_chunks LE]
                        //              [chunk payload]
                        0x03 => {
                            if data.len() < 4 {
                                continue;
                            }
                            let sender_id_len =
                                u16::from_le_bytes([data[1], data[2]]) as usize;
                            if data.len() < 3 + sender_id_len + 1 {
                                continue;
                            }
                            let sender_id =
                                String::from_utf8_lossy(&data[3..3 + sender_id_len]).to_string();

                            if sender_id == my_device_id {
                                continue;
                            }

                            let mode_offset = 3 + sender_id_len;
                            let mode = data[mode_offset];
                            let meta_offset;

                            if mode == 0x01 {
                                // Targeted message — check if we're the recipient
                                if data.len() < mode_offset + 3 {
                                    continue;
                                }
                                let recipient_len = u16::from_le_bytes([
                                    data[mode_offset + 1],
                                    data[mode_offset + 2],
                                ]) as usize;
                                if data.len() < mode_offset + 3 + recipient_len {
                                    continue;
                                }
                                let recipient_id = String::from_utf8_lossy(
                                    &data[mode_offset + 3..mode_offset + 3 + recipient_len],
                                );
                                if recipient_id != my_device_id {
                                    continue;
                                }
                                meta_offset = mode_offset + 3 + recipient_len;
                            } else {
                                meta_offset = mode_offset + 1;
                            }

                            // Parse chunk metadata: message_id (16) + chunk_index (2) + total_chunks (2) = 20 bytes
                            if data.len() < meta_offset + 20 {
                                continue;
                            }
                            let mut message_id = [0u8; 16];
                            message_id.copy_from_slice(&data[meta_offset..meta_offset + 16]);
                            let chunk_index = u16::from_le_bytes([
                                data[meta_offset + 16],
                                data[meta_offset + 17],
                            ]);
                            let total_chunks = u16::from_le_bytes([
                                data[meta_offset + 18],
                                data[meta_offset + 19],
                            ]);
                            let chunk_payload = &data[meta_offset + 20..];

                            if total_chunks == 0 || chunk_index >= total_chunks {
                                continue;
                            }

                            let key: ChunkKey = (sender_id.clone(), message_id);
                            let entry = reassembly_buffers
                                .entry(key)
                                .or_insert_with(|| ChunkReassembly {
                                    chunks: vec![None; total_chunks as usize],
                                    total_chunks,
                                    received_count: 0,
                                    created_at: Instant::now(),
                                });

                            // Validate total_chunks consistency
                            if entry.total_chunks != total_chunks {
                                continue;
                            }

                            let idx = chunk_index as usize;
                            if idx < entry.chunks.len() && entry.chunks[idx].is_none() {
                                entry.chunks[idx] = Some(chunk_payload.to_vec());
                                entry.received_count += 1;
                            }

                            // Check if all chunks received
                            if entry.received_count == entry.total_chunks {
                                // Reassemble the full encrypted payload
                                let key = (sender_id.clone(), message_id);
                                if let Some(completed) = reassembly_buffers.remove(&key) {
                                    let total_len: usize = completed
                                        .chunks
                                        .iter()
                                        .map(|c| c.as_ref().map_or(0, |v| v.len()))
                                        .sum();
                                    let mut encrypted_payload = Vec::with_capacity(total_len);
                                    for chunk in &completed.chunks {
                                        if let Some(data) = chunk {
                                            encrypted_payload.extend_from_slice(data);
                                        }
                                    }

                                    eprintln!(
                                        "[Relay] Room {}: reassembled {} chunks ({} bytes) from {}",
                                        room_code,
                                        completed.total_chunks,
                                        encrypted_payload.len(),
                                        &sender_id[..8.min(sender_id.len())]
                                    );

                                    // Decrypt (same logic as 0x02 handler)
                                    let rooms_guard = rooms.read().await;
                                    if let Some(room) = rooms_guard.get(room_code) {
                                        if let Some(cipher) = room.decrypt_ciphers.get(&sender_id) {
                                            match decrypt_message(cipher, &encrypted_payload) {
                                                Ok(message) => {
                                                    let _ = event_tx.send(
                                                        ServerEvent::SyncMessageReceived {
                                                            from_device_id: sender_id.clone(),
                                                            message,
                                                        },
                                                    );
                                                }
                                                Err(_) => {
                                                    if let Some(old_cipher) =
                                                        room.old_decrypt_ciphers.get(&sender_id)
                                                    {
                                                        match decrypt_message(
                                                            old_cipher,
                                                            &encrypted_payload,
                                                        ) {
                                                            Ok(message) => {
                                                                let _ = event_tx.send(
                                                                    ServerEvent::SyncMessageReceived {
                                                                        from_device_id: sender_id
                                                                            .clone(),
                                                                        message,
                                                                    },
                                                                );
                                                            }
                                                            Err(e2) => {
                                                                eprintln!(
                                                                    "[Relay] Room {}: chunked decrypt failed from {} (both ciphers): {}",
                                                                    room_code,
                                                                    &sender_id[..8.min(sender_id.len())],
                                                                    e2
                                                                );
                                                            }
                                                        }
                                                    } else {
                                                        eprintln!(
                                                            "[Relay] Room {}: chunked decrypt failed from {} (no old cipher)",
                                                            room_code,
                                                            &sender_id[..8.min(sender_id.len())]
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        _ => {
                            // Unknown frame type, ignore
                        }
                    }
                }
                Some(RelayFrame::Control(msg)) => {
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&msg) {
                        match parsed.get("type").and_then(|t| t.as_str()) {
                            Some("member_joined") => {
                                // New member joined — re-send our session_init so they
                                // can create a decrypt cipher for us
                                let member_id = parsed
                                    .get("deviceId")
                                    .and_then(|d| d.as_str())
                                    .unwrap_or("");
                                if !member_id.is_empty() && member_id != my_device_id {
                                    eprintln!(
                                        "[Relay] Room {}: member joined: {}",
                                        room_code,
                                        &member_id[..8.min(member_id.len())]
                                    );
                                    // Re-send session_init
                                    let rooms_guard = rooms.read().await;
                                    if let Some(room) = rooms_guard.get(room_code) {
                                        let mut init_frame =
                                            Vec::with_capacity(1 + 32 + my_device_id.len());
                                        init_frame.push(0x01);
                                        init_frame
                                            .extend_from_slice(&room.our_session_nonce);
                                        init_frame.extend_from_slice(my_device_id.as_bytes());
                                        let _ = room.sender.send_binary(init_frame).await;
                                    }
                                }
                            }
                            Some("member_left") => {
                                let member_id = parsed
                                    .get("deviceId")
                                    .and_then(|d| d.as_str())
                                    .unwrap_or("");
                                if !member_id.is_empty() && member_id != my_device_id {
                                    eprintln!(
                                        "[Relay] Room {}: member left: {}",
                                        room_code,
                                        &member_id[..8.min(member_id.len())]
                                    );
                                    let mut rooms_guard = rooms.write().await;
                                    if let Some(room) = rooms_guard.get_mut(room_code) {
                                        room.decrypt_ciphers.remove(member_id);
                                        room.member_names.remove(member_id);
                                    }
                                    let _ = event_tx.send(ServerEvent::PeerDisconnected {
                                        device_id: member_id.to_string(),
                                    });
                                }
                            }
                            Some("members") => {
                                // Initial member list — just log
                                let members = parsed
                                    .get("members")
                                    .and_then(|m| m.as_array())
                                    .map(|arr| arr.len())
                                    .unwrap_or(0);
                                eprintln!(
                                    "[Relay] Room {}: {} existing members",
                                    room_code, members
                                );
                            }
                            Some("error") => {
                                let err_msg = parsed
                                    .get("message")
                                    .and_then(|m| m.as_str())
                                    .unwrap_or("unknown");
                                eprintln!("[Relay] Room {}: server error: {}", room_code, err_msg);
                                return;
                            }
                            _ => {} // ignore other control messages
                        }
                    }
                }
                Some(RelayFrame::Heartbeat) => {
                    // Server ping received — timeout already reset by loop iteration
                    continue;
                }
                None => {
                    // Connection closed
                    return;
                }
            }
        }
    }

    // ── Public API for sending ──────────────────────────────────────

    /// Check if a device is connected via any relay room
    pub async fn has_peer(&self, device_id: &str) -> bool {
        let rooms = self.rooms.read().await;
        rooms.values().any(|r| r.decrypt_ciphers.contains_key(device_id))
    }

    /// Send an encrypted message to a specific relay peer (targeted).
    /// Finds which room has this device and sends through it.
    /// Large payloads are automatically chunked to fit under the server's message size limit.
    pub async fn send_to_peer(&self, device_id: &str, msg: &SyncMessage) -> Result<(), String> {
        let (sender, encrypted) = {
            let mut rooms = self.rooms.write().await;
            let mut found = None;
            for (_room_code, room) in rooms.iter_mut() {
                if room.decrypt_ciphers.contains_key(device_id) {
                    let encrypted = encrypt_message(&mut room.encrypt_cipher, msg)?;
                    found = Some((room.sender.clone(), encrypted));
                    break;
                }
            }
            found.ok_or_else(|| format!("Relay peer {} not connected in any room", device_id))?
        };
        // Lock released — safe to await
        let my_id = self.pairing_manager.device_id().to_string();
        send_maybe_chunked(&sender, &my_id, Some(device_id), encrypted).await
    }

    /// Send to multiple relay peers (broadcast per room)
    pub async fn send_to_peers(&self, device_ids: &[String], msg: &SyncMessage) {
        let my_device_id = self.pairing_manager.device_id().to_string();

        // Encrypt under lock, collect (sender, encrypted) pairs, then release lock before sending
        let sends: Vec<(RelaySender, Vec<u8>, String)> = {
            let mut rooms = self.rooms.write().await;

            // Group target device_ids by room
            let mut room_targets: HashMap<String, Vec<String>> = HashMap::new();
            for device_id in device_ids {
                for (room_code, room) in rooms.iter() {
                    if room.decrypt_ciphers.contains_key(device_id.as_str()) {
                        room_targets
                            .entry(room_code.clone())
                            .or_default()
                            .push(device_id.clone());
                        break;
                    }
                }
            }

            let mut result = Vec::new();
            for (room_code, _targets) in &room_targets {
                if let Some(room) = rooms.get_mut(room_code) {
                    match encrypt_message(&mut room.encrypt_cipher, msg) {
                        Ok(encrypted) => {
                            result.push((room.sender.clone(), encrypted, room_code.clone()));
                        }
                        Err(e) => {
                            eprintln!("[Relay] Encrypt for room {} failed: {}", room_code, e);
                        }
                    }
                }
            }
            result
        };
        // Lock released — safe to await
        for (sender, encrypted, room_code) in sends {
            if let Err(e) = send_maybe_chunked(&sender, &my_device_id, None, encrypted).await {
                eprintln!("[Relay] Send to room {} failed: {}", room_code, e);
            }
        }
    }

    /// Send a broadcast message to a specific room.
    /// Large payloads are automatically chunked.
    pub async fn send_to_room(&self, room_code: &str, msg: &SyncMessage) -> Result<(), String> {
        let (sender, encrypted) = {
            let mut rooms = self.rooms.write().await;
            let room = rooms
                .get_mut(room_code)
                .ok_or_else(|| format!("Room {} not connected", room_code))?;

            // No other peers in the room — skip encryption and send
            if room.member_names.is_empty() {
                return Ok(());
            }

            let encrypted = encrypt_message(&mut room.encrypt_cipher, msg)?;
            (room.sender.clone(), encrypted)
        };
        // Lock released — safe to await
        let my_device_id = self.pairing_manager.device_id().to_string();
        send_maybe_chunked(&sender, &my_device_id, None, encrypted).await
    }

    /// Send a message to a room, but skip peers that are already reachable via LAN.
    /// If all room members are in the exclusion set, the send is skipped entirely.
    /// If no members are excluded, falls back to a broadcast (same as send_to_room).
    pub async fn send_to_room_excluding(
        &self,
        room_code: &str,
        msg: &SyncMessage,
        exclude_device_ids: &std::collections::HashSet<String>,
    ) -> Result<(), String> {
        let my_device_id = self.pairing_manager.device_id().to_string();

        // Encrypt and extract send info under lock, then release before async sends
        let (sender, encrypted, relay_only_targets, is_broadcast) = {
            let mut rooms = self.rooms.write().await;
            let room = rooms
                .get_mut(room_code)
                .ok_or_else(|| format!("Room {} not connected", room_code))?;

            // Figure out which room members need the relay copy
            let relay_only_targets: Vec<String> = room
                .member_names
                .keys()
                .filter(|id| *id != &my_device_id && !exclude_device_ids.contains(id.as_str()))
                .cloned()
                .collect();

            if relay_only_targets.is_empty() {
                // All room members are reachable via LAN — skip relay entirely
                return Ok(());
            }

            let encrypted = encrypt_message(&mut room.encrypt_cipher, msg)?;

            // Check if we can use a broadcast (no exclusions apply)
            let total_other_members = room.member_names.keys().filter(|id| *id != &my_device_id).count();
            let is_broadcast = relay_only_targets.len() == total_other_members;

            (room.sender.clone(), encrypted, relay_only_targets, is_broadcast)
        };
        // Lock released — safe to await

        if is_broadcast {
            // No exclusions — broadcast to whole room
            return send_maybe_chunked(&sender, &my_device_id, None, encrypted).await;
        }

        // Send targeted frames to each relay-only peer (reuse same encrypted bytes)
        for target_id in &relay_only_targets {
            if let Err(e) =
                send_maybe_chunked(&sender, &my_device_id, Some(target_id), encrypted.clone())
                    .await
            {
                eprintln!("[Relay] Targeted send to {} failed: {}", target_id, e);
            }
        }
        Ok(())
    }

    /// Get list of connected relay peers across all rooms (deduped)
    pub async fn connected_peers(&self) -> Vec<(String, String)> {
        let rooms = self.rooms.read().await;
        let mut seen = HashMap::new();
        for room in rooms.values() {
            for (device_id, name) in &room.member_names {
                seen.entry(device_id.clone())
                    .or_insert_with(|| name.clone());
            }
        }
        seen.into_iter().collect()
    }

    /// Get the relay config
    pub async fn get_config(&self) -> RelayConfig {
        self.config.read().await.clone()
    }

    /// Try to decrypt an encrypted credentials blob (from server room list).
    /// Returns `Some((room_code, password))` if decryption succeeds (same device),
    /// or `None` if it was encrypted by a different device key.
    pub fn decrypt_credentials(&self, encrypted: &str) -> Option<(String, String)> {
        let plaintext = decrypt_password(&self.device_key, encrypted)?;
        let (code, password) = plaintext.split_once(':')?;
        Some((code.to_string(), password.to_string()))
    }

    /// Update the relay URL
    pub async fn set_relay_url(&self, url: String) {
        self.config.write().await.relay_url = normalize_relay_url(&url);
        self.save_config().await;
    }

    /// Check if any room is connected
    pub fn is_running(&self) -> bool {
        // We'll check the rooms map directly via a synchronous check isn't possible
        // with async RwLock. Instead, return true if any room_running flag is set.
        // For backward compat, this returns true if ANY room is active.
        // Callers that need accurate status should use `any_room_connected()`.
        true // Will be checked via get_rooms / any_room_connected instead
    }

    /// Check if any room is currently connected
    pub async fn any_room_connected(&self) -> bool {
        !self.rooms.read().await.is_empty()
    }

    /// Get status for all rooms
    pub async fn get_rooms(&self) -> Vec<RoomStatus> {
        let config = self.config.read().await;
        let rooms = self.rooms.read().await;
        let activated = self.manually_activated.read().await;

        let result: Vec<RoomStatus> = config
            .rooms
            .iter()
            .map(|def| {
                let connected_room = rooms.get(&def.room_code);
                let is_activated = activated.contains(&def.room_code);
                let peer_count = connected_room.map(|r| r.member_names.len()).unwrap_or(0);
                eprintln!(
                    "[Relay] get_rooms: {} relay_connected={} activated={} members={}",
                    &def.room_code[..8.min(def.room_code.len())],
                    connected_room.is_some(),
                    is_activated,
                    peer_count
                );
                RoomStatus {
                    name: def.name.clone(),
                    room_code: def.room_code.clone(),
                    password: def.password.clone(),
                    auto_connect: def.auto_connect,
                    connected: connected_room.is_some() || is_activated,
                    connected_peers: connected_room
                        .map(|r| {
                            r.member_names
                                .iter()
                                .map(|(id, name)| RoomPeerInfo {
                                    device_id: id.clone(),
                                    device_name: name.clone(),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                }
            })
            .collect();
        result
    }

    /// Get connected room codes (for filtering wiki manifests)
    pub async fn get_connected_room_codes(&self) -> Vec<String> {
        self.rooms.read().await.keys().cloned().collect()
    }

    /// Check if any rooms are configured (regardless of connection status)
    pub async fn has_any_rooms(&self) -> bool {
        !self.config.read().await.rooms.is_empty()
    }

    /// Get all device IDs connected in a specific room
    pub async fn get_room_members(&self, room_code: &str) -> Vec<String> {
        let rooms = self.rooms.read().await;
        rooms
            .get(room_code)
            .map(|r| r.member_names.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Find which room a device is connected in (returns first match)
    pub async fn find_device_room(&self, device_id: &str) -> Option<String> {
        let rooms = self.rooms.read().await;
        for (room_code, room) in rooms.iter() {
            if room.decrypt_ciphers.contains_key(device_id) {
                return Some(room_code.clone());
            }
        }
        None
    }

    /// Find all rooms a device is connected in
    pub async fn find_all_device_rooms(&self, device_id: &str) -> Vec<String> {
        let rooms = self.rooms.read().await;
        rooms.iter()
            .filter(|(_, room)| room.decrypt_ciphers.contains_key(device_id))
            .map(|(room_code, _)| room_code.clone())
            .collect()
    }

    // ── Authentication ─────────────────────────────────────────────

    /// Start OAuth login flow for a given provider
    pub async fn login(
        &self,
        provider: &str,
        client_id: &str,
        auth_url: Option<&str>,
        discovery_url: Option<&str>,
        scope: Option<&str>,
    ) -> Result<github_auth::AuthResult, String> {
        let relay_url = self.config.read().await.relay_url.clone();
        let result = github_auth::start_auth_flow(
            &relay_url, provider, client_id, auth_url, discovery_url, scope,
        ).await?;

        // Store the token in config
        {
            let mut config = self.config.write().await;
            config.auth_token = result.access_token.clone();
            config.auth_provider = Some(result.provider.clone());
            config.username = Some(result.username.clone());
            config.user_id = Some(result.user_id.clone());
        }
        self.save_config().await;
        Ok(result)
    }

    /// Log out (clear stored auth token and user info)
    pub async fn logout(&self) {
        {
            let mut config = self.config.write().await;
            config.auth_token.clear();
            config.encrypted_auth_token = None;
            config.auth_provider = None;
            config.username = None;
            config.user_id = None;
        }
        self.save_config().await;
    }

    /// Get auth status: (username, user_id, provider, has_token)
    pub async fn auth_status(&self) -> (Option<String>, Option<String>, Option<String>, bool) {
        let config = self.config.read().await;
        let has_token = !config.auth_token.is_empty();
        (config.username.clone(), config.user_id.clone(), config.auth_provider.clone(), has_token)
    }

    /// Fetch available auth providers from the relay server
    pub async fn fetch_providers(&self) -> Result<Vec<github_auth::ProviderInfo>, String> {
        let relay_url = self.config.read().await.relay_url.clone();
        github_auth::fetch_providers(&relay_url).await
    }

    // ── Server-side room management API ──────────────────────────────

    /// Generic relay API call helper
    async fn relay_api<T: serde::de::DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<T, String> {
        let config = self.config.read().await;
        if config.auth_token.is_empty() {
            return Err("Authentication required".to_string());
        }
        let api_base = github_auth::relay_ws_to_https(&config.relay_url);
        let token = config.auth_token.clone();
        let provider = config.auth_provider.clone().unwrap_or_else(|| "github".to_string());
        drop(config);

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| format!("HTTP client error: {}", e))?;

        let mut req = client.request(method, format!("{}{}", api_base, path))
            .header("Authorization", format!("Bearer {}", token))
            .header("X-Auth-Provider", &provider);

        if let Some(body) = body {
            req = req.json(body);
        }

        let resp = req.send().await
            .map_err(|e| format!("API request failed: {}", e))?;

        if resp.status() == reqwest::StatusCode::NO_CONTENT {
            // For 204 responses, return empty JSON
            return serde_json::from_str("null")
                .map_err(|e| format!("Unexpected response: {}", e));
        }

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("API error ({}): {}", status, body));
        }

        resp.json::<T>().await
            .map_err(|e| format!("Invalid API response: {}", e))
    }

    /// Register a room on the server (with existing room_code from local creation).
    /// Sends a hashed room code — the server never sees the raw code or room name.
    pub async fn create_server_room(&self, room_code: &str) -> Result<String, String> {
        let room_hash = crate::lan_sync::discovery::hash_room_code(room_code);

        // Encrypt room credentials so the owner can re-join from this device
        let encrypted_credentials = {
            let config = self.config.read().await;
            config.rooms.iter()
                .find(|r| r.room_code == room_code)
                .map(|r| {
                    let plaintext = format!("{}:{}", r.room_code, r.password);
                    encrypt_password(&self.device_key, &plaintext)
                })
        };

        let body = serde_json::json!({
            "room_code": room_hash,
            "encrypted_credentials": encrypted_credentials,
        });
        let result: serde_json::Value = self.relay_api(
            reqwest::Method::POST,
            "/api/rooms",
            Some(&body),
        ).await?;
        let returned_code = result["room_code"].as_str()
            .ok_or("No room_code in response")?
            .to_string();
        Ok(returned_code)
    }

    /// Delete a room on the server (owner only). Sends hashed room code.
    pub async fn delete_server_room(&self, room_code: &str) -> Result<(), String> {
        let room_hash = crate::lan_sync::discovery::hash_room_code(room_code);
        let _: serde_json::Value = self.relay_api(
            reqwest::Method::DELETE,
            &format!("/api/rooms/{}", room_hash),
            None,
        ).await?;
        Ok(())
    }

    /// Delete a server room by its hash directly (owner only).
    pub async fn delete_server_room_by_hash(&self, room_hash: &str) -> Result<(), String> {
        let _: serde_json::Value = self.relay_api(
            reqwest::Method::DELETE,
            &format!("/api/rooms/{}", room_hash),
            None,
        ).await?;
        Ok(())
    }

    /// Add a member to a server room (owner only). Sends hashed room code.
    pub async fn add_room_member(&self, room_code: &str, username: &str, provider: Option<&str>) -> Result<(), String> {
        let room_hash = crate::lan_sync::discovery::hash_room_code(room_code);
        let provider = provider.unwrap_or("github");
        let body = serde_json::json!({"username": username, "provider": provider, "github_login": username});
        let _: serde_json::Value = self.relay_api(
            reqwest::Method::POST,
            &format!("/api/rooms/{}/members", room_hash),
            Some(&body),
        ).await?;
        Ok(())
    }

    /// Remove a member from a server room (owner only). Sends hashed room code.
    pub async fn remove_room_member(&self, room_code: &str, user_id: &str) -> Result<(), String> {
        let room_hash = crate::lan_sync::discovery::hash_room_code(room_code);
        let _: serde_json::Value = self.relay_api(
            reqwest::Method::DELETE,
            &format!("/api/rooms/{}/members/{}", room_hash, urlencoding::encode(user_id)),
            None,
        ).await?;
        Ok(())
    }

    /// List members of a server room. Sends hashed room code.
    pub async fn list_room_members(&self, room_code: &str) -> Result<Vec<serde_json::Value>, String> {
        let room_hash = crate::lan_sync::discovery::hash_room_code(room_code);
        self.relay_api(
            reqwest::Method::GET,
            &format!("/api/rooms/{}/members", room_hash),
            None,
        ).await
    }

    /// List server rooms the user owns or is a member of.
    /// Returns hashed room codes — caller matches against local rooms.
    pub async fn list_server_rooms(&self) -> Result<Vec<serde_json::Value>, String> {
        self.relay_api(
            reqwest::Method::GET,
            "/api/rooms",
            None,
        ).await
    }
}

// ── Frame building helpers ──────────────────────────────────────────

/// Build a data frame with the new format:
/// [0x02][2-byte sender_id_len LE][sender_id UTF-8]
/// [1-byte mode: 0x00=broadcast, 0x01=targeted]
/// [if targeted: 2-byte recipient_len LE][recipient_id UTF-8]
/// [encrypted payload]
fn build_data_frame(
    sender_id: &str,
    target_device_id: Option<&str>,
    encrypted_payload: &[u8],
) -> Vec<u8> {
    let sender_bytes = sender_id.as_bytes();
    let sender_len = sender_bytes.len() as u16;

    let mut frame = Vec::with_capacity(
        1 + 2 + sender_bytes.len() + 1 + encrypted_payload.len() + 32, // extra for optional recipient
    );

    frame.push(0x02);
    frame.extend_from_slice(&sender_len.to_le_bytes());
    frame.extend_from_slice(sender_bytes);

    if let Some(recipient) = target_device_id {
        frame.push(0x01); // targeted
        let recipient_bytes = recipient.as_bytes();
        let recipient_len = recipient_bytes.len() as u16;
        frame.extend_from_slice(&recipient_len.to_le_bytes());
        frame.extend_from_slice(recipient_bytes);
    } else {
        frame.push(0x00); // broadcast
    }

    frame.extend_from_slice(encrypted_payload);
    frame
}

/// Build a chunk frame:
/// [0x03][2-byte sender_id_len LE][sender_id UTF-8]
/// [1-byte mode: 0x00=broadcast, 0x01=targeted]
/// [if targeted: 2-byte recipient_len LE][recipient_id UTF-8]
/// [16-byte message_id]
/// [2-byte chunk_index LE]
/// [2-byte total_chunks LE]
/// [chunk payload]
fn build_chunk_frame(
    sender_id: &str,
    target_device_id: Option<&str>,
    message_id: &[u8; 16],
    chunk_index: u16,
    total_chunks: u16,
    chunk_payload: &[u8],
) -> Vec<u8> {
    let sender_bytes = sender_id.as_bytes();
    let sender_len = sender_bytes.len() as u16;

    let mut frame = Vec::with_capacity(
        1 + 2 + sender_bytes.len() + 1 + 16 + 4 + chunk_payload.len() + 32,
    );

    frame.push(0x03);
    frame.extend_from_slice(&sender_len.to_le_bytes());
    frame.extend_from_slice(sender_bytes);

    if let Some(recipient) = target_device_id {
        frame.push(0x01); // targeted
        let recipient_bytes = recipient.as_bytes();
        let recipient_len = recipient_bytes.len() as u16;
        frame.extend_from_slice(&recipient_len.to_le_bytes());
        frame.extend_from_slice(recipient_bytes);
    } else {
        frame.push(0x00); // broadcast
    }

    frame.extend_from_slice(message_id);
    frame.extend_from_slice(&chunk_index.to_le_bytes());
    frame.extend_from_slice(&total_chunks.to_le_bytes());
    frame.extend_from_slice(chunk_payload);
    frame
}

/// Send an encrypted payload, chunking it if it exceeds CHUNK_THRESHOLD.
/// Small payloads are sent as a single 0x02 data frame.
/// Large payloads are split into 0x03 chunk frames.
async fn send_maybe_chunked(
    sender: &RelaySender,
    sender_id: &str,
    target: Option<&str>,
    encrypted: Vec<u8>,
) -> Result<(), String> {
    if encrypted.len() <= CHUNK_THRESHOLD {
        let frame = build_data_frame(sender_id, target, &encrypted);
        return sender.send_binary(frame).await;
    }

    // Split into chunks
    let message_id: [u8; 16] = rand::random();
    let chunk_slices: Vec<&[u8]> = encrypted.chunks(CHUNK_SIZE).collect();
    let total_chunks = chunk_slices.len() as u16;

    eprintln!(
        "[Relay] Chunking {} byte payload into {} chunks",
        encrypted.len(),
        total_chunks
    );

    for (i, chunk_data) in chunk_slices.iter().enumerate() {
        let frame = build_chunk_frame(
            sender_id,
            target,
            &message_id,
            i as u16,
            total_chunks,
            chunk_data,
        );
        sender.send_binary(frame).await?;
    }
    Ok(())
}
