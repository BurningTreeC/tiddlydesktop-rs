//! PIN-based device pairing via SPAKE2 password-authenticated key exchange.
//!
//! Flow:
//! 1. Device A generates 6-digit PIN and displays it
//! 2. User enters PIN on Device B
//! 3. Both run SPAKE2 exchange over unencrypted WebSocket
//! 4. Derive long-term shared secret via HKDF-SHA256
//! 5. Store pairing in paired_devices.json

use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};
use std::path::PathBuf;
use std::sync::Mutex;

// SPAKE2 identity constants — MUST be the same in start_a and start_b.
// id_a is always party A's identity, id_b is always party B's identity.
const SPAKE2_ID_A: &[u8] = b"tiddlydesktop-pin-enterer";
const SPAKE2_ID_B: &[u8] = b"tiddlydesktop-pin-displayer";

type HmacSha256 = Hmac<Sha256>;

/// A paired device record stored on disk
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairedDevice {
    pub device_id: String,
    pub device_name: String,
    /// SHA-256 hash of the long-term shared secret (for identification, not the secret itself)
    pub shared_secret_hash: String,
    /// The actual long-term shared secret (base64-encoded)
    pub shared_secret: String,
    /// When the pairing was established
    pub paired_at: String,
}

/// Persistent paired devices file
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PairedDevicesFile {
    pub devices: Vec<PairedDevice>,
}

/// State for an in-progress SPAKE2 pairing
pub struct PairingState {
    pub pin: String,
    pub spake2_state: Option<spake2::Spake2<Ed25519Group>>,
    pub role: PairingRole,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PairingRole {
    /// This device generated the PIN and is waiting for another device to enter it
    PinDisplayer,
    /// This device entered the PIN and is initiating connection
    PinEnterer,
}

/// Global pairing manager
pub struct PairingManager {
    /// This device's unique ID
    device_id: String,
    /// This device's display name
    device_name: String,
    /// Path to paired_devices.json
    storage_path: PathBuf,
    /// Currently paired devices (loaded from disk)
    paired_devices: Mutex<Vec<PairedDevice>>,
    /// In-progress pairing state (only one pairing at a time)
    active_pairing: Mutex<Option<PairingState>>,
}

impl PairingManager {
    pub fn new(device_id: String, device_name: String, data_dir: PathBuf) -> Self {
        let storage_path = data_dir.join("paired_devices.json");
        let paired_devices = Self::load_from_disk(&storage_path);
        Self {
            device_id,
            device_name,
            storage_path,
            paired_devices: Mutex::new(paired_devices),
            active_pairing: Mutex::new(None),
        }
    }

    /// Get this device's ID
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// Get this device's display name
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Generate a 6-digit PIN and prepare SPAKE2 state for displaying
    pub fn start_pairing_as_displayer(&self) -> String {
        let pin: String = format!("{:06}", rand::rng().random_range(0..1_000_000u32));

        let (spake2_state, _outbound_msg) = Spake2::<Ed25519Group>::start_b(
            &Password::new(pin.as_bytes()),
            &Identity::new(SPAKE2_ID_A),
            &Identity::new(SPAKE2_ID_B),
        );

        let mut active = self.active_pairing.lock().unwrap();
        *active = Some(PairingState {
            pin: pin.clone(),
            spake2_state: Some(spake2_state),
            role: PairingRole::PinDisplayer,
        });

        pin
    }

    /// Start pairing as the device that entered the PIN
    /// Returns the SPAKE2 outbound message to send to the displayer
    pub fn start_pairing_as_enterer(&self, pin: &str) -> Vec<u8> {
        let (spake2_state, outbound_msg) = Spake2::<Ed25519Group>::start_a(
            &Password::new(pin.as_bytes()),
            &Identity::new(SPAKE2_ID_A),
            &Identity::new(SPAKE2_ID_B),
        );

        let mut active = self.active_pairing.lock().unwrap();
        *active = Some(PairingState {
            pin: pin.to_string(),
            spake2_state: Some(spake2_state),
            role: PairingRole::PinEnterer,
        });

        outbound_msg
    }

    /// Process the peer's SPAKE2 message and derive the shared secret
    /// Returns (outbound_spake2_msg_if_displayer, long_term_key)
    pub fn process_spake2_message(
        &self,
        peer_msg: &[u8],
    ) -> Result<(Option<Vec<u8>>, Vec<u8>), String> {
        let mut active = self.active_pairing.lock().unwrap();
        let state = active
            .as_mut()
            .ok_or_else(|| "No active pairing".to_string())?;

        let spake2_state = state
            .spake2_state
            .take()
            .ok_or_else(|| "SPAKE2 state already consumed".to_string())?;

        // If we're the displayer, we need to generate our outbound message now
        let outbound_msg = if state.role == PairingRole::PinDisplayer {
            let (new_state, msg) = Spake2::<Ed25519Group>::start_b(
                &Password::new(state.pin.as_bytes()),
                &Identity::new(SPAKE2_ID_A),
                &Identity::new(SPAKE2_ID_B),
            );
            // We need to finish with the new state
            let shared_secret = new_state
                .finish(peer_msg)
                .map_err(|e| format!("SPAKE2 finish failed: {:?}", e))?;

            // Derive long-term key via HKDF
            let long_term_key = derive_long_term_key(&shared_secret)?;

            // Drop the old state (already consumed)
            drop(spake2_state);

            return Ok((Some(msg), long_term_key));
        } else {
            None
        };

        // Enterer: finish with the peer's message
        let shared_secret = spake2_state
            .finish(peer_msg)
            .map_err(|e| format!("SPAKE2 finish failed: {:?}", e))?;

        let long_term_key = derive_long_term_key(&shared_secret)?;
        Ok((outbound_msg, long_term_key))
    }

    /// Generate confirmation HMAC from the long-term key
    pub fn generate_confirmation(&self, long_term_key: &[u8]) -> Vec<u8> {
        let mut mac =
            HmacSha256::new_from_slice(long_term_key).expect("HMAC accepts any key length");
        mac.update(b"tiddlydesktop-pairing-confirm");
        mac.update(self.device_id.as_bytes());
        mac.finalize().into_bytes().to_vec()
    }

    /// Verify peer's confirmation HMAC
    pub fn verify_confirmation(
        &self,
        long_term_key: &[u8],
        peer_device_id: &str,
        confirmation: &[u8],
    ) -> bool {
        let mut mac =
            HmacSha256::new_from_slice(long_term_key).expect("HMAC accepts any key length");
        mac.update(b"tiddlydesktop-pairing-confirm");
        mac.update(peer_device_id.as_bytes());
        mac.verify_slice(confirmation).is_ok()
    }

    /// Complete pairing: store the paired device
    pub fn complete_pairing(
        &self,
        peer_device_id: &str,
        peer_device_name: &str,
        long_term_key: &[u8],
    ) -> Result<(), String> {
        // Clear active pairing state
        *self.active_pairing.lock().unwrap() = None;

        let secret_b64 =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, long_term_key);
        let secret_hash = format!("{:x}", md5::compute(long_term_key));

        let device = PairedDevice {
            device_id: peer_device_id.to_string(),
            device_name: peer_device_name.to_string(),
            shared_secret_hash: secret_hash,
            shared_secret: secret_b64,
            paired_at: chrono::Utc::now().to_rfc3339(),
        };

        let mut devices = self.paired_devices.lock().unwrap();
        // Remove any existing pairing for this device ID
        devices.retain(|d| d.device_id != peer_device_id);
        devices.push(device);

        self.save_to_disk(&devices)?;
        Ok(())
    }

    /// Cancel any in-progress pairing
    pub fn cancel_pairing(&self) {
        *self.active_pairing.lock().unwrap() = None;
    }

    /// Get the current PIN being displayed (if any)
    pub fn get_active_pin(&self) -> Option<String> {
        self.active_pairing
            .lock()
            .unwrap()
            .as_ref()
            .filter(|s| s.role == PairingRole::PinDisplayer)
            .map(|s| s.pin.clone())
    }

    /// Get the pairing role
    pub fn get_pairing_role(&self) -> Option<PairingRole> {
        self.active_pairing
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.role.clone())
    }

    /// Get all paired devices
    pub fn get_paired_devices(&self) -> Vec<PairedDevice> {
        self.paired_devices.lock().unwrap().clone()
    }

    /// Look up a paired device by ID and return its shared secret
    pub fn get_shared_secret(&self, device_id: &str) -> Option<Vec<u8>> {
        let devices = self.paired_devices.lock().unwrap();
        devices
            .iter()
            .find(|d| d.device_id == device_id)
            .and_then(|d| {
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &d.shared_secret)
                    .ok()
            })
    }

    /// Check if a device is paired
    pub fn is_paired(&self, device_id: &str) -> bool {
        self.paired_devices
            .lock()
            .unwrap()
            .iter()
            .any(|d| d.device_id == device_id)
    }

    /// Remove a paired device (unpair)
    pub fn unpair_device(&self, device_id: &str) -> Result<(), String> {
        let mut devices = self.paired_devices.lock().unwrap();
        devices.retain(|d| d.device_id != device_id);
        self.save_to_disk(&devices)
    }

    // ── Persistence ─────────────────────────────────────────────────────

    fn load_from_disk(path: &PathBuf) -> Vec<PairedDevice> {
        match std::fs::read_to_string(path) {
            Ok(content) => {
                serde_json::from_str::<PairedDevicesFile>(&content)
                    .map(|f| f.devices)
                    .unwrap_or_default()
            }
            Err(_) => Vec::new(),
        }
    }

    fn save_to_disk(&self, devices: &[PairedDevice]) -> Result<(), String> {
        let file = PairedDevicesFile {
            devices: devices.to_vec(),
        };
        let json = serde_json::to_string_pretty(&file)
            .map_err(|e| format!("Serialize failed: {}", e))?;
        if let Some(parent) = self.storage_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Create dir failed: {}", e))?;
        }
        std::fs::write(&self.storage_path, json)
            .map_err(|e| format!("Write failed: {}", e))?;
        Ok(())
    }
}

/// Derive a 32-byte long-term key from the SPAKE2 shared secret
fn derive_long_term_key(shared_secret: &[u8]) -> Result<Vec<u8>, String> {
    let hk = Hkdf::<Sha256>::new(None, shared_secret);
    let mut key = vec![0u8; 32];
    hk.expand(b"tiddlydesktop-lan-sync-long-term-key", &mut key)
        .map_err(|e| format!("HKDF expand failed: {}", e))?;
    Ok(key)
}

/// Generate a random UUID v4 — used for wiki sync IDs and as device ID fallback
pub fn generate_random_id() -> String {
    let mut rng = rand::rng();
    let bytes: [u8; 16] = rng.random();
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        u16::from_be_bytes([bytes[4], bytes[5]]),
        u16::from_be_bytes([bytes[6], bytes[7]]) & 0x0FFF,
        (u16::from_be_bytes([bytes[8], bytes[9]]) & 0x3FFF) | 0x8000,
        u64::from_be_bytes([0, 0, bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]])
    )
}

/// Device identity stored on disk
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceIdentity {
    pub device_id: String,
    pub device_name: String,
}

// ── Stable device ID (MAC address on desktop, ANDROID_ID on Android) ────

/// Get the MAC address of the primary network interface (Linux).
#[cfg(target_os = "linux")]
fn get_mac_address() -> Option<String> {
    let net_dir = std::fs::read_dir("/sys/class/net/").ok()?;
    // Collect and sort for deterministic ordering
    let mut entries: Vec<_> = net_dir.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().to_string();
        // Skip loopback, virtual, container, and VPN interfaces
        if name == "lo"
            || name.starts_with("veth")
            || name.starts_with("docker")
            || name.starts_with("br-")
            || name.starts_with("virbr")
            || name.starts_with("tun")
            || name.starts_with("tap")
        {
            continue;
        }
        if let Ok(mac) = std::fs::read_to_string(entry.path().join("address")) {
            let mac = mac.trim().to_uppercase();
            if !mac.is_empty() && mac != "00:00:00:00:00:00" {
                return Some(mac);
            }
        }
    }
    None
}

/// Get the MAC address of the primary network interface (macOS).
#[cfg(target_os = "macos")]
fn get_mac_address() -> Option<String> {
    // en0 is the built-in WiFi/Ethernet on macOS
    let output = std::process::Command::new("ifconfig")
        .arg("en0")
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(mac) = line.strip_prefix("ether ") {
            let mac = mac.trim().to_uppercase();
            if !mac.is_empty() && mac != "00:00:00:00:00:00" {
                return Some(mac);
            }
        }
    }
    None
}

/// Get the MAC address of the primary network interface (Windows).
#[cfg(target_os = "windows")]
fn get_mac_address() -> Option<String> {
    let output = std::process::Command::new("getmac")
        .args(["/FO", "CSV", "/NH"])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        // Format: "AA-BB-CC-DD-EE-FF","...","...","..."
        let parts: Vec<&str> = line.split(',').collect();
        if let Some(mac) = parts.first() {
            let mac = mac.trim_matches('"').replace('-', ":").to_uppercase();
            if mac.len() == 17 && mac != "00:00:00:00:00:00" {
                return Some(mac);
            }
        }
    }
    None
}

/// On Android, get Settings.Secure.ANDROID_ID via JNI.
/// This is stable across app reinstalls (tied to signing key + user + device).
/// Real WiFi MAC is inaccessible to apps since Android 10.
#[cfg(target_os = "android")]
fn get_android_stable_id() -> Option<String> {
    use crate::android::wiki_activity::get_java_vm;

    let vm = get_java_vm().ok()?;
    let mut env = vm.attach_current_thread().ok()?;

    // Get application context via ActivityThread.currentApplication()
    let activity_thread_class = env.find_class("android/app/ActivityThread").ok()?;
    let context = env
        .call_static_method(
            &activity_thread_class,
            "currentApplication",
            "()Landroid/app/Application;",
            &[],
        )
        .ok()?
        .l()
        .ok()?;

    if context.is_null() {
        return None;
    }

    // Call context.getContentResolver()
    let resolver = env
        .call_method(&context, "getContentResolver", "()Landroid/content/ContentResolver;", &[])
        .ok()?
        .l()
        .ok()?;

    // Call Settings.Secure.getString(resolver, "android_id")
    let key = env.new_string("android_id").ok()?;
    let android_id = env
        .call_static_method(
            "android/provider/Settings$Secure",
            "getString",
            "(Landroid/content/ContentResolver;Ljava/lang/String;)Ljava/lang/String;",
            &[
                jni::objects::JValueGen::Object(&resolver),
                jni::objects::JValueGen::Object(&key.into()),
            ],
        )
        .ok()?
        .l()
        .ok()?;

    if android_id.is_null() {
        return None;
    }

    let id_str: String = env.get_string((&android_id).into()).ok()?.into();
    if id_str.is_empty() {
        return None;
    }

    Some(id_str)
}

/// Get a stable device identifier: MAC address on desktop, ANDROID_ID on Android.
/// Returns None if no stable ID can be obtained (falls back to random UUID).
fn get_stable_device_id() -> Option<String> {
    #[cfg(target_os = "android")]
    {
        get_android_stable_id()
    }
    #[cfg(not(target_os = "android"))]
    {
        get_mac_address()
    }
}

// ── Device name ─────────────────────────────────────────────────────────

/// Get the device name for this platform
fn get_device_name() -> String {
    #[cfg(target_os = "android")]
    {
        if let Some(name) = get_android_device_name() {
            return name;
        }
        "Android Device".to_string()
    }
    #[cfg(not(target_os = "android"))]
    {
        hostname::get()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_else(|_| "Unknown Device".to_string())
    }
}

/// Get device name on Android via JNI (Build.MODEL)
#[cfg(target_os = "android")]
fn get_android_device_name() -> Option<String> {
    use crate::android::wiki_activity::get_java_vm;

    let vm = get_java_vm().ok()?;
    let mut env = vm.attach_current_thread().ok()?;

    let model = env
        .get_static_field("android/os/Build", "MODEL", "Ljava/lang/String;")
        .ok()?
        .l()
        .ok()?;

    if model.is_null() {
        return None;
    }

    let model_str: String = env.get_string((&model).into()).ok()?.into();
    if model_str.is_empty() {
        return None;
    }

    Some(model_str)
}

// ── Load / create identity ──────────────────────────────────────────────

/// Load or create device identity.
/// Uses a stable hardware ID (MAC address on desktop, ANDROID_ID on Android)
/// so the device_id survives app data clears and reinstalls. Falls back to
/// a persisted random UUID only if no stable ID is available.
pub fn load_or_create_device_identity(data_dir: &std::path::Path) -> DeviceIdentity {
    let path = data_dir.join("device_identity.json");
    let stable_id = get_stable_device_id();
    let device_name = get_device_name();

    // If we have a stable hardware ID, always use it (even if file says something different)
    if let Some(ref stable) = stable_id {
        let identity = DeviceIdentity {
            device_id: stable.clone(),
            device_name: device_name.clone(),
        };
        // Save/update on disk
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(&identity) {
            let _ = std::fs::write(&path, json);
        }
        eprintln!("[LAN Sync] Device ID (stable): {}", stable);
        return identity;
    }

    // No stable ID available — fall back to persisted random UUID
    if let Ok(content) = std::fs::read_to_string(&path) {
        if let Ok(mut identity) = serde_json::from_str::<DeviceIdentity>(&content) {
            identity.device_name = device_name;
            if let Ok(json) = serde_json::to_string_pretty(&identity) {
                let _ = std::fs::write(&path, json);
            }
            eprintln!("[LAN Sync] Device ID (persisted): {}", identity.device_id);
            return identity;
        }
    }

    // Create new random identity as last resort
    let identity = DeviceIdentity {
        device_id: generate_random_id(),
        device_name: device_name,
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(&identity) {
        let _ = std::fs::write(&path, json);
    }
    eprintln!("[LAN Sync] Device ID (random fallback): {}", identity.device_id);
    identity
}
