//! Android MulticastLock management for mDNS discovery.
//!
//! Android's Wi-Fi radio filters out multicast packets by default to save power.
//! We need to acquire a `WifiManager.MulticastLock` while LAN sync is active
//! so that mDNS discovery can receive service announcements from other devices.
//!
//! Requires `CHANGE_WIFI_MULTICAST_STATE` permission in AndroidManifest.xml.

use jni::objects::{JObject, JValue};
use jni::JNIEnv;
use std::sync::Mutex;

/// Stored MulticastLock global ref — kept alive while LAN sync is active
static MULTICAST_LOCK: Mutex<Option<jni::objects::GlobalRef>> = Mutex::new(None);

/// Acquire the Android MulticastLock (high-level, handles JNI setup internally).
/// Call this when starting LAN sync. Safe to call multiple times (no-op if already held).
pub fn acquire_lock() {
    // Check if already held
    if MULTICAST_LOCK.lock().unwrap().is_some() {
        eprintln!("[LAN Sync] MulticastLock already held");
        return;
    }

    let vm = match super::wiki_activity::get_java_vm() {
        Ok(vm) => vm,
        Err(e) => {
            eprintln!("[LAN Sync] Can't get JavaVM for MulticastLock: {}", e);
            return;
        }
    };

    let mut env = match vm.attach_current_thread() {
        Ok(env) => env,
        Err(e) => {
            eprintln!("[LAN Sync] Can't attach thread for MulticastLock: {}", e);
            return;
        }
    };

    // Get application context via ActivityThread.currentApplication()
    let app_context = match get_app_context(&mut env) {
        Ok(ctx) => ctx,
        Err(e) => {
            eprintln!("[LAN Sync] Can't get app context for MulticastLock: {}", e);
            return;
        }
    };

    match acquire_multicast_lock(&mut env, &app_context) {
        Ok(lock_ref) => {
            *MULTICAST_LOCK.lock().unwrap() = Some(lock_ref);
        }
        Err(e) => {
            eprintln!("[LAN Sync] Failed to acquire MulticastLock: {}", e);
        }
    }
}

/// Release the Android MulticastLock (high-level).
/// Call this when stopping LAN sync. Safe to call if not held.
pub fn release_lock() {
    let lock_ref = MULTICAST_LOCK.lock().unwrap().take();
    if let Some(ref lock) = lock_ref {
        let vm = match super::wiki_activity::get_java_vm() {
            Ok(vm) => vm,
            Err(e) => {
                eprintln!("[LAN Sync] Can't get JavaVM for MulticastLock release: {}", e);
                return;
            }
        };

        let mut env = match vm.attach_current_thread() {
            Ok(env) => env,
            Err(e) => {
                eprintln!("[LAN Sync] Can't attach thread for MulticastLock release: {}", e);
                return;
            }
        };

        if let Err(e) = release_multicast_lock(&mut env, lock) {
            eprintln!("[LAN Sync] Error releasing MulticastLock: {}", e);
        }
    }
}

/// Get the application context via ActivityThread.currentApplication()
fn get_app_context<'a>(env: &mut JNIEnv<'a>) -> Result<JObject<'a>, String> {
    let activity_thread_class = env.find_class("android/app/ActivityThread")
        .map_err(|e| format!("Failed to find ActivityThread: {}", e))?;
    env.call_static_method(
        &activity_thread_class,
        "currentApplication",
        "()Landroid/app/Application;",
        &[],
    )
    .map_err(|e| format!("Failed to get current application: {}", e))?
    .l()
    .map_err(|e| format!("Failed to convert: {}", e))
}

/// Acquire a multicast lock via JNI
/// Returns a global ref to the MulticastLock object that must be released later
pub fn acquire_multicast_lock(env: &mut JNIEnv, context: &JObject) -> Result<jni::objects::GlobalRef, String> {
    // Get WifiManager: context.getSystemService(Context.WIFI_SERVICE)
    let wifi_service = env
        .get_static_field(
            "android/content/Context",
            "WIFI_SERVICE",
            "Ljava/lang/String;",
        )
        .map_err(|e| format!("Failed to get WIFI_SERVICE: {}", e))?
        .l()
        .map_err(|e| format!("Failed to convert WIFI_SERVICE: {}", e))?;

    let wifi_manager = env
        .call_method(
            context,
            "getSystemService",
            "(Ljava/lang/String;)Ljava/lang/Object;",
            &[JValue::Object(&wifi_service)],
        )
        .map_err(|e| format!("Failed to get WifiManager: {}", e))?
        .l()
        .map_err(|e| format!("Failed to convert WifiManager: {}", e))?;

    // Create MulticastLock: wifiManager.createMulticastLock("tiddlydesktop-lan-sync")
    let lock_tag = env
        .new_string("tiddlydesktop-lan-sync")
        .map_err(|e| format!("Failed to create lock tag: {}", e))?;

    let multicast_lock = env
        .call_method(
            &wifi_manager,
            "createMulticastLock",
            "(Ljava/lang/String;)Landroid/net/wifi/WifiManager$MulticastLock;",
            &[JValue::Object(&lock_tag)],
        )
        .map_err(|e| format!("Failed to create MulticastLock: {}", e))?
        .l()
        .map_err(|e| format!("Failed to convert MulticastLock: {}", e))?;

    // Acquire the lock: multicastLock.acquire()
    env.call_method(&multicast_lock, "acquire", "()V", &[])
        .map_err(|e| format!("Failed to acquire MulticastLock: {}", e))?;

    // Create global ref so it survives the current JNI call
    let global_ref = env
        .new_global_ref(&multicast_lock)
        .map_err(|e| format!("Failed to create global ref: {}", e))?;

    eprintln!("[LAN Sync] MulticastLock acquired");
    Ok(global_ref)
}

/// Release a previously acquired multicast lock
pub fn release_multicast_lock(env: &mut JNIEnv, lock: &jni::objects::GlobalRef) -> Result<(), String> {
    let lock_obj = lock.as_obj();

    // Check if held: multicastLock.isHeld()
    let is_held = env
        .call_method(lock_obj, "isHeld", "()Z", &[])
        .map_err(|e| format!("Failed to check isHeld: {}", e))?
        .z()
        .map_err(|e| format!("Failed to convert isHeld: {}", e))?;

    if is_held {
        env.call_method(lock_obj, "release", "()V", &[])
            .map_err(|e| format!("Failed to release MulticastLock: {}", e))?;
        eprintln!("[LAN Sync] MulticastLock released");
    }

    Ok(())
}
