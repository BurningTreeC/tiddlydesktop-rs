//! Device identity management for LAN sync.
//!
//! Provides stable device identification across sessions and reinstalls.
//! Uses MAC address on desktop, ANDROID_ID on Android.

use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::RwLock;

/// Hash a device name (hostname / Build.MODEL) so raw hostnames never leave
/// this module. Uses SHA-256 with domain separation, returns first 12 hex chars.
fn hash_device_name(raw: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"device_name:");
    hasher.update(raw.as_bytes());
    let result = hasher.finalize();
    result[..6]
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

/// Device identity manager — provides device_id and device_name for sync.
pub struct PairingManager {
    /// This device's unique ID
    device_id: String,
    /// This device's hashed hostname (fallback when no display_name is set)
    hashed_hostname: String,
    /// User-chosen display name (overrides hashed_hostname when set)
    display_name: RwLock<Option<String>>,
    /// Path to data directory (for persisting display_name)
    data_dir: PathBuf,
}

impl PairingManager {
    pub fn new(device_id: String, hashed_hostname: String, display_name: Option<String>, data_dir: PathBuf) -> Self {
        Self {
            device_id,
            hashed_hostname,
            display_name: RwLock::new(display_name),
            data_dir,
        }
    }

    /// Get this device's ID
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// Get this device's display name: custom name if set, else hashed hostname.
    pub fn device_name(&self) -> String {
        let guard = self.display_name.read().unwrap();
        if let Some(ref name) = *guard {
            if !name.is_empty() {
                return name.clone();
            }
        }
        self.hashed_hostname.clone()
    }

    /// Set a custom display name. Pass None or empty to revert to hashed hostname.
    /// Persists the change to device_identity.json.
    pub fn set_display_name(&self, name: Option<String>) {
        let clean = name.and_then(|n| {
            let trimmed = n.trim().to_string();
            if trimmed.is_empty() { None } else { Some(trimmed) }
        });
        *self.display_name.write().unwrap() = clean.clone();

        // Persist to disk
        let path = self.data_dir.join("device_identity.json");
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(mut identity) = serde_json::from_str::<DeviceIdentity>(&content) {
                identity.display_name = clean;
                if let Ok(json) = serde_json::to_string_pretty(&identity) {
                    let _ = std::fs::write(&path, json);
                }
            }
        }
    }

    /// Get the current custom display name setting (for pre-populating the UI input).
    /// Returns None if no custom name is set (user sees hashed hostname).
    pub fn get_display_name_setting(&self) -> Option<String> {
        self.display_name.read().unwrap().clone()
    }
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
    #[serde(default)]
    pub display_name: Option<String>,
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
    use std::os::windows::process::CommandExt;
    let output = std::process::Command::new("getmac")
        .args(["/FO", "CSV", "/NH"])
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
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

/// Hash a raw hardware identifier (MAC address, ANDROID_ID) with SHA-256 so the
/// raw value never leaves this module. Returns the first 16 hex chars (64 bits)
/// which is enough for unique device identification without exposing the original.
fn hash_hardware_id(raw: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(raw.as_bytes());
    let result = hasher.finalize();
    // First 8 bytes → 16 hex chars
    result[..8]
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
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
///
/// The device_name field is always the hashed hostname (never the raw hostname).
/// The display_name field is preserved from disk if it exists.
pub fn load_or_create_device_identity(data_dir: &std::path::Path) -> DeviceIdentity {
    let path = data_dir.join("device_identity.json");
    let stable_id = get_stable_device_id();
    let raw_name = get_device_name();
    let hashed_name = hash_device_name(&raw_name);

    // Try to load existing display_name from disk
    let existing_display_name = std::fs::read_to_string(&path)
        .ok()
        .and_then(|content| serde_json::from_str::<DeviceIdentity>(&content).ok())
        .and_then(|id| id.display_name);

    // If we have a stable hardware ID, hash it so the raw MAC/ANDROID_ID
    // never leaves this module, then use the hash as the device_id.
    if let Some(ref stable) = stable_id {
        let hashed = hash_hardware_id(stable);
        let identity = DeviceIdentity {
            device_id: hashed.clone(),
            device_name: hashed_name.clone(),
            display_name: existing_display_name,
        };
        // Save/update on disk
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(&identity) {
            let _ = std::fs::write(&path, json);
        }
        eprintln!("[LAN Sync] Device ID (stable, hashed): {}", hashed);
        return identity;
    }

    // No stable ID available — fall back to persisted random UUID
    if let Ok(content) = std::fs::read_to_string(&path) {
        if let Ok(mut identity) = serde_json::from_str::<DeviceIdentity>(&content) {
            identity.device_name = hashed_name;
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
        device_name: hashed_name,
        display_name: None,
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
