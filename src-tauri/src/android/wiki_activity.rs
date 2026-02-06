//! Wiki Activity launcher for opening wikis in separate Android app instances.
//!
//! This module provides JNI bindings to launch WikiActivity.
//! WikiActivity is a standalone WebView activity (not Tauri-based) that opens
//! individual wikis in separate Android tasks (visible in recent apps).

#![cfg(target_os = "android")]

use std::sync::OnceLock;
use jni::JavaVM;
use jni::objects::{GlobalRef, JObject, JClass};

/// Cached JavaVM reference
static JAVA_VM: OnceLock<JavaVM> = OnceLock::new();

/// Cached ClassLoader reference (needed to find app classes from native threads)
static CLASS_LOADER: OnceLock<GlobalRef> = OnceLock::new();

/// Store the JavaVM for later use. Called during app initialization.
pub fn set_java_vm(vm: JavaVM) {
    let _ = JAVA_VM.set(vm);
}

/// Get the cached JavaVM reference.
pub fn get_java_vm() -> Result<&'static JavaVM, String> {
    JAVA_VM.get().ok_or_else(|| "JavaVM not initialized. Call set_java_vm first.".to_string())
}

/// Get or initialize the application's ClassLoader.
fn get_class_loader<'a>(env: &mut jni::JNIEnv<'a>) -> Result<JObject<'a>, String> {
    // Check if we already have a cached ClassLoader
    if let Some(cached) = CLASS_LOADER.get() {
        // Create a new local reference from the global reference
        let local = env.new_local_ref(cached.as_obj())
            .map_err(|e| format!("Failed to create local ref from cached ClassLoader: {}", e))?;
        return Ok(local);
    }

    eprintln!("[WikiActivity] Initializing ClassLoader from application context");

    // Get the application via ActivityThread.currentApplication()
    let activity_thread_class = env.find_class("android/app/ActivityThread")
        .map_err(|e| format!("Failed to find ActivityThread: {}", e))?;

    let app = env.call_static_method(
        &activity_thread_class,
        "currentApplication",
        "()Landroid/app/Application;",
        &[],
    ).map_err(|e| format!("Failed to get currentApplication: {}", e))?
        .l().map_err(|e| format!("Failed to convert application: {}", e))?;

    // Get the ClassLoader from the application
    let class_loader = env.call_method(
        &app,
        "getClassLoader",
        "()Ljava/lang/ClassLoader;",
        &[],
    ).map_err(|e| format!("Failed to get ClassLoader: {}", e))?
        .l().map_err(|e| format!("Failed to convert ClassLoader: {}", e))?;

    // Create a global reference to cache it
    let global_ref = env.new_global_ref(&class_loader)
        .map_err(|e| format!("Failed to create global ref: {}", e))?;

    // Try to cache it (ignore if already set by another thread)
    let _ = CLASS_LOADER.set(global_ref);

    // Return a local reference (the caller's env owns this)
    Ok(class_loader)
}

/// Find a class using the application's ClassLoader.
/// This is necessary because env.find_class() from an attached native thread
/// uses the system classloader which doesn't have access to app classes.
fn find_app_class<'a>(env: &mut jni::JNIEnv<'a>, class_name: &str) -> Result<JClass<'a>, String> {
    // First try the standard way (works if called from the main thread)
    if let Ok(class) = env.find_class(class_name) {
        return Ok(class);
    }

    // Clear any exception from the failed find_class
    if env.exception_check().unwrap_or(false) {
        let _ = env.exception_clear();
    }

    eprintln!("[WikiActivity] Standard find_class failed for {}, trying ClassLoader", class_name);

    // Get the application's ClassLoader
    let class_loader = get_class_loader(env)?;

    // Convert class name from JNI format (com/example/Class) to Java format (com.example.Class)
    let java_class_name = class_name.replace('/', ".");
    let class_name_jstring = env.new_string(&java_class_name)
        .map_err(|e| format!("Failed to create class name string: {}", e))?;

    // Call classLoader.loadClass(className)
    let class_obj = env.call_method(
        &class_loader,
        "loadClass",
        "(Ljava/lang/String;)Ljava/lang/Class;",
        &[(&class_name_jstring).into()],
    ).map_err(|e| format!("Failed to call loadClass: {}", e))?
        .l().map_err(|e| format!("Failed to convert class object: {}", e))?;

    // Convert JObject to JClass
    Ok(JClass::from(class_obj))
}

/// Start the foreground service to keep the app alive in background.
/// Call this when starting a wiki server.
/// This is best-effort - if it fails, we continue without the service.
pub fn start_foreground_service() -> Result<(), String> {
    eprintln!("[WikiActivity] start_foreground_service called");

    let vm = get_java_vm()?;
    eprintln!("[WikiActivity] Got JavaVM for foreground service");

    let mut env = vm.attach_current_thread()
        .map_err(|e| format!("Failed to attach thread: {}", e))?;
    eprintln!("[WikiActivity] Attached thread for foreground service");

    // Get application context
    let activity_thread_class = env.find_class("android/app/ActivityThread")
        .map_err(|e| format!("Failed to find ActivityThread: {}", e))?;
    eprintln!("[WikiActivity] Found ActivityThread class");

    let app_context = env.call_static_method(
        &activity_thread_class,
        "currentApplication",
        "()Landroid/app/Application;",
        &[],
    ).map_err(|e| format!("Failed to get current application: {}", e))?
        .l().map_err(|e| format!("Failed to convert: {}", e))?;
    eprintln!("[WikiActivity] Got application context");

    // Call WikiServerService.startService(context)
    let service_class = match find_app_class(&mut env, "com/burningtreec/tiddlydesktop_rs/WikiServerService") {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[WikiActivity] WikiServerService class not found: {}", e);
            // Clear any exception so we can continue
            if env.exception_check().unwrap_or(false) {
                let _ = env.exception_clear();
            }
            return Err(format!("WikiServerService class not found: {}", e));
        }
    };
    eprintln!("[WikiActivity] Found WikiServerService class");

    match env.call_static_method(
        &service_class,
        "startService",
        "(Landroid/content/Context;)V",
        &[(&app_context).into()],
    ) {
        Ok(_) => {
            eprintln!("[WikiActivity] Started foreground service successfully");
        }
        Err(e) => {
            eprintln!("[WikiActivity] Failed to call startService: {}", e);
            // Clear any exception so we can continue
            if env.exception_check().unwrap_or(false) {
                let _ = env.exception_clear();
            }
            return Err(format!("Failed to start foreground service: {}", e));
        }
    }

    Ok(())
}

/// Notify that a wiki was closed. Stops the foreground service when no wikis are open.
pub fn wiki_closed() -> Result<(), String> {
    let vm = get_java_vm()?;
    let mut env = vm.attach_current_thread()
        .map_err(|e| format!("Failed to attach thread: {}", e))?;

    // Get application context
    let activity_thread_class = env.find_class("android/app/ActivityThread")
        .map_err(|e| format!("Failed to find ActivityThread: {}", e))?;

    let app_context = env.call_static_method(
        &activity_thread_class,
        "currentApplication",
        "()Landroid/app/Application;",
        &[],
    ).map_err(|e| format!("Failed to get current application: {}", e))?
        .l().map_err(|e| format!("Failed to convert: {}", e))?;

    // Call WikiServerService.wikiClosed(context)
    let service_class = find_app_class(&mut env, "com/burningtreec/tiddlydesktop_rs/WikiServerService")
        .map_err(|e| format!("Failed to find WikiServerService: {}", e))?;

    env.call_static_method(
        &service_class,
        "wikiClosed",
        "(Landroid/content/Context;)V",
        &[(&app_context).into()],
    ).map_err(|e| format!("Failed to notify wiki closed: {}", e))?;

    eprintln!("[WikiActivity] Notified wiki closed");
    Ok(())
}

/// Launch a new WikiActivity to open a wiki in a separate app instance,
/// or bring an existing instance to the foreground if the wiki is already open.
///
/// WikiActivity is a standalone Android WebView that loads the wiki URL directly.
/// Each wiki appears as a separate entry in Android's recent apps.
///
/// For single-file wikis: WikiActivity starts its own HTTP server in the :wiki process.
/// For folder wikis: Uses Node.js server URL from main process.
///
/// # Arguments
/// * `wiki_path` - The content:// URI or file path of the wiki (JSON for SAF)
/// * `wiki_title` - Display name for the wiki (shown in recent apps)
/// * `is_folder` - Whether this is a folder wiki
/// * `server_url` - For folder wikis: Node.js server URL
/// * `backups_enabled` - Whether to create backups on save
/// * `backup_count` - Max backups to keep (0 = unlimited)
pub fn launch_wiki_activity(
    wiki_path: &str,
    wiki_title: &str,
    is_folder: bool,
    server_url: Option<&str>,
    backups_enabled: bool,
    backup_count: u32,
) -> Result<(), String> {
    eprintln!("[WikiActivity] launch_wiki_activity called:");
    eprintln!("[WikiActivity]   wiki_path: {}", wiki_path);
    eprintln!("[WikiActivity]   wiki_title: {}", wiki_title);
    eprintln!("[WikiActivity]   is_folder: {}", is_folder);
    eprintln!("[WikiActivity]   server_url: {:?}", server_url);

    let vm = get_java_vm()?;
    eprintln!("[WikiActivity] Got JavaVM");

    let mut env = vm.attach_current_thread()
        .map_err(|e| format!("Failed to attach thread: {}", e))?;
    eprintln!("[WikiActivity] Attached thread");

    // Get the current activity
    let activity = match get_current_activity(&mut env) {
        Ok(a) => {
            eprintln!("[WikiActivity] Got current activity");
            a
        }
        Err(e) => {
            eprintln!("[WikiActivity] Failed to get current activity: {}", e);
            return Err(e);
        }
    };

    // Check if this wiki is already open
    match try_bring_wiki_to_front(&mut env, &activity, wiki_path) {
        Ok(true) => {
            eprintln!("[WikiActivity] Wiki already open, brought to front: {}", wiki_title);
            return Ok(());
        }
        Ok(false) => {
            eprintln!("[WikiActivity] Wiki not already open, will launch new activity");
        }
        Err(e) => {
            eprintln!("[WikiActivity] Error checking if wiki open: {}, continuing anyway", e);
        }
    }

    // Create Intent for WikiActivity
    eprintln!("[WikiActivity] Finding Intent class...");
    let intent_class = env.find_class("android/content/Intent")
        .map_err(|e| format!("Failed to find Intent class: {}", e))?;
    eprintln!("[WikiActivity] Found Intent class");

    eprintln!("[WikiActivity] Finding WikiActivity class...");
    let wiki_activity_class = find_app_class(&mut env, "com/burningtreec/tiddlydesktop_rs/WikiActivity")
        .map_err(|e| format!("Failed to find WikiActivity class: {}", e))?;
    eprintln!("[WikiActivity] Found WikiActivity class");

    let intent = env.new_object(
        &intent_class,
        "(Landroid/content/Context;Ljava/lang/Class;)V",
        &[
            (&activity).into(),
            (&wiki_activity_class).into(),
        ],
    ).map_err(|e| format!("Failed to create Intent: {}", e))?;

    // Put extras
    let extra_wiki_path = env.new_string("wiki_path")
        .map_err(|e| format!("Failed to create string: {}", e))?;
    let extra_wiki_title = env.new_string("wiki_title")
        .map_err(|e| format!("Failed to create string: {}", e))?;
    let extra_is_folder = env.new_string("is_folder")
        .map_err(|e| format!("Failed to create string: {}", e))?;

    let value_path = env.new_string(wiki_path)
        .map_err(|e| format!("Failed to create string: {}", e))?;
    let value_title = env.new_string(wiki_title)
        .map_err(|e| format!("Failed to create string: {}", e))?;

    // intent.putExtra(key, value)
    env.call_method(
        &intent,
        "putExtra",
        "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;",
        &[(&extra_wiki_path).into(), (&value_path).into()],
    ).map_err(|e| format!("Failed to putExtra wiki_path: {}", e))?;

    env.call_method(
        &intent,
        "putExtra",
        "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;",
        &[(&extra_wiki_title).into(), (&value_title).into()],
    ).map_err(|e| format!("Failed to putExtra wiki_title: {}", e))?;

    env.call_method(
        &intent,
        "putExtra",
        "(Ljava/lang/String;Z)Landroid/content/Intent;",
        &[(&extra_is_folder).into(), jni::objects::JValue::Bool(is_folder as u8)],
    ).map_err(|e| format!("Failed to putExtra is_folder: {}", e))?;

    // Add server URL for folder wikis
    if let Some(url) = server_url {
        let extra_wiki_url = env.new_string("wiki_url")
            .map_err(|e| format!("Failed to create string: {}", e))?;
        let value_wiki_url = env.new_string(url)
            .map_err(|e| format!("Failed to create string: {}", e))?;
        env.call_method(
            &intent,
            "putExtra",
            "(Ljava/lang/String;Ljava/lang/String;)Landroid/content/Intent;",
            &[(&extra_wiki_url).into(), (&value_wiki_url).into()],
        ).map_err(|e| format!("Failed to putExtra wiki_url: {}", e))?;
    }

    // Add backup settings
    let extra_backups_enabled = env.new_string("backups_enabled")
        .map_err(|e| format!("Failed to create string: {}", e))?;
    let extra_backup_count = env.new_string("backup_count")
        .map_err(|e| format!("Failed to create string: {}", e))?;

    env.call_method(
        &intent,
        "putExtra",
        "(Ljava/lang/String;Z)Landroid/content/Intent;",
        &[(&extra_backups_enabled).into(), jni::objects::JValue::Bool(backups_enabled as u8)],
    ).map_err(|e| format!("Failed to putExtra backups_enabled: {}", e))?;

    env.call_method(
        &intent,
        "putExtra",
        "(Ljava/lang/String;I)Landroid/content/Intent;",
        &[(&extra_backup_count).into(), jni::objects::JValue::Int(backup_count as i32)],
    ).map_err(|e| format!("Failed to putExtra backup_count: {}", e))?;

    // Start the activity
    eprintln!("[WikiActivity] Calling startActivity...");
    env.call_method(
        &activity,
        "startActivity",
        "(Landroid/content/Intent;)V",
        &[(&intent).into()],
    ).map_err(|e| format!("Failed to start WikiActivity: {}", e))?;

    eprintln!("[WikiActivity] startActivity returned successfully");
    eprintln!("[WikiActivity] Launched WikiActivity for: {}", wiki_title);
    Ok(())
}

/// Try to bring an existing wiki instance to the foreground.
/// Returns true if the wiki was already open and brought to front.
fn try_bring_wiki_to_front<'a>(
    env: &mut jni::JNIEnv<'a>,
    activity: &jni::objects::JObject<'a>,
    wiki_path: &str,
) -> Result<bool, String> {
    eprintln!("[WikiActivity] try_bring_wiki_to_front: {}", wiki_path);

    let wiki_activity_class = find_app_class(env, "com/burningtreec/tiddlydesktop_rs/WikiActivity")
        .map_err(|e| format!("Failed to find WikiActivity class: {}", e))?;

    // Create the wiki path string
    let wiki_path_jstring = env.new_string(wiki_path)
        .map_err(|e| format!("Failed to create string: {}", e))?;

    // First try to find and bring to front using the AppTask scanning method
    // This works even if the in-memory map was cleared
    let brought_to_front = env.call_static_method(
        &wiki_activity_class,
        "bringWikiToFront",
        "(Landroid/content/Context;Ljava/lang/String;)Z",
        &[activity.into(), (&wiki_path_jstring).into()],
    ).map_err(|e| format!("Failed to call bringWikiToFront: {}", e))?
        .z().map_err(|e| format!("Failed to get boolean result: {}", e))?;

    eprintln!("[WikiActivity] try_bring_wiki_to_front result: {}", brought_to_front);
    Ok(brought_to_front)
}

/// Get the current Android Activity via reflection.
fn get_current_activity<'a>(env: &mut jni::JNIEnv<'a>) -> Result<jni::objects::JObject<'a>, String> {
    // ActivityThread.currentActivityThread().mActivities
    let activity_thread_class = env.find_class("android/app/ActivityThread")
        .map_err(|e| format!("Failed to find ActivityThread: {}", e))?;

    let current_thread = env.call_static_method(
        &activity_thread_class,
        "currentActivityThread",
        "()Landroid/app/ActivityThread;",
        &[],
    ).map_err(|e| format!("Failed to get currentActivityThread: {}", e))?
        .l().map_err(|e| format!("Failed to convert to object: {}", e))?;

    // Get mActivities map
    let activities_field = env.get_field(
        &current_thread,
        "mActivities",
        "Landroid/util/ArrayMap;",
    ).map_err(|e| format!("Failed to get mActivities: {}", e))?
        .l().map_err(|e| format!("Failed to convert: {}", e))?;

    // Get values collection
    let values = env.call_method(
        &activities_field,
        "values",
        "()Ljava/util/Collection;",
        &[],
    ).map_err(|e| format!("Failed to get values: {}", e))?
        .l().map_err(|e| format!("Failed to convert: {}", e))?;

    // Convert to array
    let values_array = env.call_method(
        &values,
        "toArray",
        "()[Ljava/lang/Object;",
        &[],
    ).map_err(|e| format!("Failed to toArray: {}", e))?
        .l().map_err(|e| format!("Failed to convert: {}", e))?;

    let array_obj: jni::objects::JObjectArray = values_array.into();
    let length = env.get_array_length(&array_obj)
        .map_err(|e| format!("Failed to get array length: {}", e))?;

    if length == 0 {
        return Err("No activities found".to_string());
    }

    // Get the first (current) activity record
    let activity_record = env.get_object_array_element(&array_obj, 0)
        .map_err(|e| format!("Failed to get array element: {}", e))?;

    // Get the activity from the record
    let activity = env.get_field(
        &activity_record,
        "activity",
        "Landroid/app/Activity;",
    ).map_err(|e| format!("Failed to get activity field: {}", e))?
        .l().map_err(|e| format!("Failed to convert: {}", e))?;

    Ok(activity)
}

/// Parse a hex color string (e.g., "#FFFFFF" or "FFFFFF") to an Android color int.
fn parse_color_to_int(color: &str) -> Result<i32, String> {
    let hex = color.trim_start_matches('#');

    if hex.len() != 6 && hex.len() != 8 {
        return Err(format!("Invalid color format: {}", color));
    }

    // Parse as ARGB (add full alpha if only RGB provided)
    let argb = if hex.len() == 6 {
        format!("FF{}", hex)
    } else {
        hex.to_string()
    };

    u32::from_str_radix(&argb, 16)
        .map(|c| c as i32)
        .map_err(|e| format!("Failed to parse color {}: {}", color, e))
}

/// Determine if a color is light (for setting light/dark status bar icons).
fn is_light_color(color: &str) -> bool {
    let hex = color.trim_start_matches('#');
    if hex.len() < 6 {
        return false;
    }

    // Parse RGB components
    let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(0) as f32;
    let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(0) as f32;
    let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(0) as f32;

    // Calculate relative luminance
    let luminance = (0.299 * r + 0.587 * g + 0.114 * b) / 255.0;
    luminance > 0.5
}

/// Set the Android status bar and navigation bar colors.
///
/// # Arguments
/// * `status_bar_color` - Hex color for the status bar (e.g., "#FFFFFF")
/// * `nav_bar_color` - Hex color for the navigation bar (e.g., "#FFFFFF")
/// * `foreground_color` - Optional foreground color to determine icon contrast
pub fn set_system_bar_colors(status_bar_color: &str, nav_bar_color: &str, foreground_color: Option<&str>) -> Result<(), String> {
    eprintln!("[WikiActivity] set_system_bar_colors: status={}, nav={}, fg={:?}", status_bar_color, nav_bar_color, foreground_color);

    let status_color_int = parse_color_to_int(status_bar_color)?;
    let nav_color_int = parse_color_to_int(nav_bar_color)?;

    // Use foreground color to determine if icons should be dark (foreground is dark) or light (foreground is light)
    // If foreground is dark (#333), we want dark icons → set LIGHT_STATUS_BARS
    // If foreground is light (#fff), we want light icons → don't set LIGHT_STATUS_BARS
    let (status_is_light, nav_is_light) = if let Some(fg) = foreground_color {
        // Use foreground color to determine icon style
        // Dark foreground = light background = use dark icons (set LIGHT flag)
        let fg_is_dark = !is_light_color(fg);
        (fg_is_dark, fg_is_dark)
    } else {
        // Fallback to background luminance calculation
        (is_light_color(status_bar_color), is_light_color(nav_bar_color))
    };

    let vm = get_java_vm()?;
    let mut env = vm.attach_current_thread()
        .map_err(|e| format!("Failed to attach thread: {}", e))?;

    // Get the current activity
    let activity = get_current_activity(&mut env)?;

    // Get the Window from the activity
    let window = env.call_method(
        &activity,
        "getWindow",
        "()Landroid/view/Window;",
        &[],
    ).map_err(|e| format!("Failed to get window: {}", e))?
        .l().map_err(|e| format!("Failed to convert window: {}", e))?;

    // Set status bar color
    env.call_method(
        &window,
        "setStatusBarColor",
        "(I)V",
        &[jni::objects::JValue::Int(status_color_int)],
    ).map_err(|e| format!("Failed to set status bar color: {}", e))?;

    // Set navigation bar color
    env.call_method(
        &window,
        "setNavigationBarColor",
        "(I)V",
        &[jni::objects::JValue::Int(nav_color_int)],
    ).map_err(|e| format!("Failed to set navigation bar color: {}", e))?;

    // Get the decor view for setting light/dark status bar
    let decor_view = env.call_method(
        &window,
        "getDecorView",
        "()Landroid/view/View;",
        &[],
    ).map_err(|e| format!("Failed to get decor view: {}", e))?
        .l().map_err(|e| format!("Failed to convert decor view: {}", e))?;

    // Get WindowInsetsController (API 30+) or fall back to systemUiVisibility
    // Try the modern API first
    let insets_controller_result = env.call_method(
        &window,
        "getInsetsController",
        "()Landroid/view/WindowInsetsController;",
        &[],
    );

    if let Ok(controller_value) = insets_controller_result {
        if let Ok(controller) = controller_value.l() {
            if !controller.is_null() {
                // Use WindowInsetsController (API 30+)
                // APPEARANCE_LIGHT_STATUS_BARS = 8
                // APPEARANCE_LIGHT_NAVIGATION_BARS = 16
                let mut appearance: i32 = 0;
                if status_is_light {
                    appearance |= 8; // APPEARANCE_LIGHT_STATUS_BARS
                }
                if nav_is_light {
                    appearance |= 16; // APPEARANCE_LIGHT_NAVIGATION_BARS
                }

                let mask = 8 | 16; // Both flags

                let _ = env.call_method(
                    &controller,
                    "setSystemBarsAppearance",
                    "(II)V",
                    &[
                        jni::objects::JValue::Int(appearance),
                        jni::objects::JValue::Int(mask),
                    ],
                );

                eprintln!("[WikiActivity] Set system bar colors via InsetsController");
                return Ok(());
            }
        }
    }

    // Fall back to older API (deprecated but still works)
    let current_flags = env.call_method(
        &decor_view,
        "getSystemUiVisibility",
        "()I",
        &[],
    ).map_err(|e| format!("Failed to get systemUiVisibility: {}", e))?
        .i().map_err(|e| format!("Failed to convert flags: {}", e))?;

    // SYSTEM_UI_FLAG_LIGHT_STATUS_BAR = 0x00002000 (8192)
    // SYSTEM_UI_FLAG_LIGHT_NAVIGATION_BAR = 0x00000010 (16) - actually it's 0x10 = 16 for nav bar light
    // Wait, let me check the correct values:
    // SYSTEM_UI_FLAG_LIGHT_STATUS_BAR = 8192 (0x2000)
    // SYSTEM_UI_FLAG_LIGHT_NAVIGATION_BAR = 16 (0x10)

    let light_status_flag: i32 = 0x2000;
    let light_nav_flag: i32 = 0x10;

    let mut new_flags = current_flags;

    if status_is_light {
        new_flags |= light_status_flag;
    } else {
        new_flags &= !light_status_flag;
    }

    if nav_is_light {
        new_flags |= light_nav_flag;
    } else {
        new_flags &= !light_nav_flag;
    }

    env.call_method(
        &decor_view,
        "setSystemUiVisibility",
        "(I)V",
        &[jni::objects::JValue::Int(new_flags)],
    ).map_err(|e| format!("Failed to set systemUiVisibility: {}", e))?;

    eprintln!("[WikiActivity] Set system bar colors via systemUiVisibility");
    Ok(())
}
