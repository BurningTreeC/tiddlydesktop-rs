//! JavaScript initialization scripts for wiki windows
//!
//! This module contains the JavaScript code that gets injected into wiki windows
//! to provide TiddlyDesktop functionality:
//! - Native dialog replacements (alert, confirm, prompt)
//! - Drag-drop handling and synthetic event creation
//! - External attachments support
//! - File import functionality
//! - Clipboard operations
//! - TiddlyWiki integration hooks
//!
//! The JavaScript is organized into semantic modules:
//! - main.js: Entry point and namespace setup
//! - core.js: Initialization guard, modal UI, confirm override
//! - window.js: Window close handler with unsaved changes check
//! - filesystem.js: httpRequest override, path resolution, media interceptor
//! - drag_drop.js: External attachments, file drops, content drags, paste, import hooks
//! - session_auth.js: Session authentication URL management
//! - internal_drag.js: Internal TiddlyWiki drag-and-drop polyfill
//! - sync.js: Window handlers, cross-window tiddler synchronization

/// Media controls CSS stylesheet (included inline because WebKitGTK doesn't load
/// CSS from custom URI schemes like tdlib:// via <link> tags)
const MEDIA_CONTROLS_CSS: &str = include_str!("../resources/tdlib/media-controls.css");

/// Combined initialization script from all modules (concatenated at compile time).
/// Each module is wrapped in try-catch so one failing script can't prevent subsequent
/// scripts from executing (critical for LAN sync which loads near the end).
const COMBINED_INIT_SCRIPT: &str = concat!(
    // Error reporter — logs to Rust stderr via js_log when available
    "window.__tdInitErr=function(n,e){var m='[TD init] '+n+' error: '+(e&&e.message||e);if(window.__TAURI__&&window.__TAURI__.core&&window.__TAURI__.core.invoke){window.__TAURI__.core.invoke('js_log',{message:m}).catch(function(){})}console.error(m)};\n",
    "try{\n", include_str!("init_script/main.js"),
    "\n}catch(_e){window.__tdInitErr('main.js',_e)}\n",
    "try{\n", include_str!("init_script/core.js"),
    "\n}catch(_e){window.__tdInitErr('core.js',_e)}\n",
    "try{\n", include_str!("init_script/window.js"),
    "\n}catch(_e){window.__tdInitErr('window.js',_e)}\n",
    "try{\n", include_str!("init_script/filesystem.js"),
    "\n}catch(_e){window.__tdInitErr('filesystem.js',_e)}\n",
    "try{\n", include_str!("init_script/drag_drop.js"),
    "\n}catch(_e){window.__tdInitErr('drag_drop.js',_e)}\n",
    "try{\n", include_str!("init_script/session_auth.js"),
    "\n}catch(_e){window.__tdInitErr('session_auth.js',_e)}\n",
    "try{\n", include_str!("init_script/internal_drag.js"),
    "\n}catch(_e){window.__tdInitErr('internal_drag.js',_e)}\n",
    "try{\n", include_str!("init_script/sync.js"),
    "\n}catch(_e){window.__tdInitErr('sync.js',_e)}\n",
    "try{\n", include_str!("init_script/media.js"),
    "\n}catch(_e){window.__tdInitErr('media.js',_e)}\n",
    "try{\n", include_str!("init_script/title_sync.js"),
    "\n}catch(_e){window.__tdInitErr('title_sync.js',_e)}\n",
    "try{\n", include_str!("init_script/favicon_sync.js"),
    "\n}catch(_e){window.__tdInitErr('favicon_sync.js',_e)}\n",
    "try{\n", include_str!("init_script/lan_sync.js"),
    "\n}catch(_e){window.__tdInitErr('lan_sync.js',_e)}\n",
    "try{\n", include_str!("init_script/conflict_ui.js"),
    "\n}catch(_e){window.__tdInitErr('conflict_ui.js',_e)}\n",
    "try{\n", include_str!("init_script/peer_status.js"),
    "\n}catch(_e){window.__tdInitErr('peer_status.js',_e)}\n",
);

/// Full JavaScript initialization script for wiki windows - sets all necessary variables early
/// This ensures __WIKI_PATH__, __WINDOW_LABEL__, and __IS_MAIN_WIKI__ are available before
/// setupExternalAttachments runs, avoiding race conditions with protocol handler injection.
pub fn get_wiki_init_script(wiki_path: &str, window_label: &str, is_main_wiki: bool) -> String {
    get_wiki_init_script_with_language(wiki_path, window_label, is_main_wiki, None)
}

/// Full JavaScript initialization script with optional language override
/// Uses serde_json for safe string escaping to prevent injection attacks
pub fn get_wiki_init_script_with_language(wiki_path: &str, window_label: &str, is_main_wiki: bool, language: Option<&str>) -> String {
    // Use serde_json::to_string for proper JSON escaping - this handles all edge cases
    // including backslashes, quotes, newlines, unicode, etc.
    let wiki_path_json = serde_json::to_string(wiki_path).unwrap_or_else(|_| "\"\"".to_string());
    let window_label_json = serde_json::to_string(window_label).unwrap_or_else(|_| "\"\"".to_string());
    let lang_line = match language {
        Some(lang) => {
            let lang_json = serde_json::to_string(lang).unwrap_or_else(|_| "\"\"".to_string());
            format!("window.__TIDDLYDESKTOP_LANGUAGE__ = {};", lang_json)
        }
        None => String::new(),
    };
    let mut script = format!(
        r#"
    window.__WIKI_PATH__ = {};
    window.__WINDOW_LABEL__ = {};
    window.__IS_MAIN_WIKI__ = {};
    {}
    "#,
        wiki_path_json,
        window_label_json,
        is_main_wiki,
        lang_line
    );
    // Detect folder wiki mode: wiki path doesn't end with .html/.htm and isn't the main wiki
    let is_folder_wiki = !is_main_wiki
        && !wiki_path.ends_with(".html")
        && !wiki_path.ends_with(".htm");

    if is_folder_wiki {
        // Folder wikis load from HTTP origin — tdasset:// custom scheme is blocked.
        // Route all file access through the localhost media server on all platforms.
        script.push_str("window.__TD_FOLDER_WIKI__ = true;\n");
        script.push_str("window.__TD_MEDIA_SERVER__ = true;\n");
    } else {
        // Single-file wikis: Linux needs media server for GStreamer playback.
        // Windows/macOS use tdasset:// directly (their media engines handle custom schemes).
        #[cfg(target_os = "linux")]
        script.push_str("window.__TD_MEDIA_SERVER__ = true;\n");
    }

    // Include media controls CSS as a global variable for media.js to inject as <style>.
    // WebKitGTK doesn't load CSS from custom URI schemes via <link> tags.
    if !is_main_wiki {
        let css_json = serde_json::to_string(MEDIA_CONTROLS_CSS).unwrap_or_else(|_| "\"\"".to_string());
        script.push_str(&format!("window.__MEDIA_CONTROLS_CSS__ = {};\n", css_json));
    }
    script.push_str(get_dialog_init_script());
    script
}

/// Get the main initialization script for dialog handling and TiddlyDesktop integration
pub fn get_dialog_init_script() -> &'static str {
    COMBINED_INIT_SCRIPT
}
