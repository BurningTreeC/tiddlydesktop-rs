// SAF (Storage Access Framework) plugin for Android
// This provides directory listing support for content:// URIs on Android

use serde::{Deserialize, Serialize};
use tauri::{
    plugin::{Builder, TauriPlugin},
    Manager, Wry,
};

#[cfg(target_os = "android")]
use tauri::plugin::PluginHandle;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ListDirectoryRequest {
    uri: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SafEntry {
    name: String,
    #[serde(rename = "documentId")]
    document_id: String,
    #[serde(rename = "isFile")]
    is_file: bool,
    #[serde(rename = "mimeType")]
    mime_type: String,
    uri: String,
}

#[derive(Debug, Deserialize)]
struct ListDirectoryResponse {
    success: bool,
    entries: Vec<SafEntry>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FileExistsRequest {
    parent_uri: String,
    file_name: String,
}

#[derive(Debug, Deserialize)]
struct FileExistsResponse {
    success: bool,
    exists: bool,
    error: Option<String>,
}

/// Entry returned from SAF directory listing
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectoryEntry {
    pub name: String,
    pub is_file: bool,
    pub uri: String,
}

/// SAF plugin state for Android - wraps the Kotlin SafPlugin
#[cfg(target_os = "android")]
pub struct SafPluginState {
    handle: PluginHandle<Wry>,
}

#[cfg(not(target_os = "android"))]
pub struct SafPluginState;

#[cfg(target_os = "android")]
impl SafPluginState {
    /// List directory contents using SAF (content:// URIs)
    pub fn list_directory(&self, uri: &str) -> Result<Vec<DirectoryEntry>, String> {
        let response: ListDirectoryResponse = self
            .handle
            .run_mobile_plugin("listDirectory", ListDirectoryRequest { uri: uri.to_string() })
            .map_err(|e| format!("Failed to call SAF plugin: {}", e))?;

        if !response.success {
            return Err(response.error.unwrap_or_else(|| "Unknown SAF error".to_string()));
        }

        Ok(response
            .entries
            .into_iter()
            .map(|e| DirectoryEntry {
                name: e.name,
                is_file: e.is_file,
                uri: e.uri,
            })
            .collect())
    }

    /// List subdirectory contents using SAF (content:// URIs)
    pub fn list_subdirectory(&self, uri: &str) -> Result<Vec<DirectoryEntry>, String> {
        let response: ListDirectoryResponse = self
            .handle
            .run_mobile_plugin("listSubdirectory", ListDirectoryRequest { uri: uri.to_string() })
            .map_err(|e| format!("Failed to call SAF plugin: {}", e))?;

        if !response.success {
            return Err(response.error.unwrap_or_else(|| "Unknown SAF error".to_string()));
        }

        Ok(response
            .entries
            .into_iter()
            .map(|e| DirectoryEntry {
                name: e.name,
                is_file: e.is_file,
                uri: e.uri,
            })
            .collect())
    }

    /// Check if a file exists in a directory using SAF
    pub fn file_exists(&self, parent_uri: &str, file_name: &str) -> Result<bool, String> {
        let response: FileExistsResponse = self
            .handle
            .run_mobile_plugin(
                "fileExistsInDirectory",
                FileExistsRequest {
                    parent_uri: parent_uri.to_string(),
                    file_name: file_name.to_string(),
                },
            )
            .map_err(|e| format!("Failed to call SAF plugin: {}", e))?;

        if !response.success {
            return Err(response.error.unwrap_or_else(|| "Unknown SAF error".to_string()));
        }

        Ok(response.exists)
    }
}

#[cfg(not(target_os = "android"))]
impl SafPluginState {
    /// List directory contents - on non-Android platforms, this returns an error
    pub fn list_directory(&self, _uri: &str) -> Result<Vec<DirectoryEntry>, String> {
        Err("SAF is only available on Android".to_string())
    }

    /// List subdirectory contents - on non-Android platforms, this returns an error
    pub fn list_subdirectory(&self, _uri: &str) -> Result<Vec<DirectoryEntry>, String> {
        Err("SAF is only available on Android".to_string())
    }

    /// Check if a file exists - on non-Android platforms, this returns an error
    pub fn file_exists(&self, _parent_uri: &str, _file_name: &str) -> Result<bool, String> {
        Err("SAF is only available on Android".to_string())
    }
}

/// Tauri command to list directory contents via SAF
#[tauri::command]
pub fn saf_list_directory(
    app: tauri::AppHandle,
    uri: String,
) -> Result<Vec<DirectoryEntry>, String> {
    let state = app.state::<SafPluginState>();
    state.list_directory(&uri)
}

/// Tauri command to list subdirectory contents via SAF
#[tauri::command]
pub fn saf_list_subdirectory(
    app: tauri::AppHandle,
    uri: String,
) -> Result<Vec<DirectoryEntry>, String> {
    let state = app.state::<SafPluginState>();
    state.list_subdirectory(&uri)
}

/// Tauri command to check if a file exists in a directory via SAF
#[tauri::command]
pub fn saf_file_exists(
    app: tauri::AppHandle,
    parent_uri: String,
    file_name: String,
) -> Result<bool, String> {
    let state = app.state::<SafPluginState>();
    state.file_exists(&parent_uri, &file_name)
}

/// Initialize the SAF plugin
pub fn init() -> TauriPlugin<Wry> {
    Builder::new("saf")
        .invoke_handler(tauri::generate_handler![
            saf_list_directory,
            saf_list_subdirectory,
            saf_file_exists
        ])
        .setup(|app, _api| {
            #[cfg(target_os = "android")]
            {
                let handle = _api
                    .register_android_plugin("com.simon.tiddlydesktop_rs", "SafPlugin")
                    .map_err(|e| format!("Failed to register SAF plugin: {}", e))?;
                app.manage(SafPluginState { handle });
            }
            #[cfg(not(target_os = "android"))]
            {
                app.manage(SafPluginState);
            }
            Ok(())
        })
        .build()
}

/// Extension trait for easy access to SAF plugin state
pub trait SafExt {
    fn saf(&self) -> &SafPluginState;
}

impl<T: Manager<Wry>> SafExt for T {
    fn saf(&self) -> &SafPluginState {
        self.state::<SafPluginState>().inner()
    }
}
