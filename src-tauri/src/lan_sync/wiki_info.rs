//! tiddlywiki.info parsing, merging, and plugin availability checking for LAN sync.
//!
//! When folder wikis sync across devices, tiddlywiki.info changes (plugin/theme/language
//! additions) are propagated. This module handles:
//! - Parsing tiddlywiki.info JSON (extracting plugins/themes/languages arrays)
//! - Merging two tiddlywiki.info files (union of arrays, local's other fields preserved)
//! - Checking if a plugin is available (bundled, synced, or wiki-local)
//! - Computing SHA-256 manifests for plugin directories

use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::protocol::AttachmentFileInfo;

/// Parsed tiddlywiki.info (only the fields we care about for sync)
#[derive(Debug, Clone)]
pub struct WikiInfo {
    pub plugins: Vec<String>,
    pub themes: Vec<String>,
    pub languages: Vec<String>,
    /// Raw JSON Value for preserving other fields
    pub raw: serde_json::Value,
}

impl WikiInfo {
    /// Parse tiddlywiki.info from JSON string
    pub fn parse(json_str: &str) -> Result<Self, String> {
        let raw: serde_json::Value =
            serde_json::from_str(json_str).map_err(|e| format!("Invalid JSON: {}", e))?;
        let plugins = extract_string_array(&raw, "plugins");
        let themes = extract_string_array(&raw, "themes");
        let languages = extract_string_array(&raw, "languages");
        Ok(WikiInfo {
            plugins,
            themes,
            languages,
            raw,
        })
    }

    /// Serialize back to JSON string (pretty-printed)
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(&self.raw)
            .map_err(|e| format!("Failed to serialize: {}", e))
    }
}

/// Extract a string array from a JSON object
fn extract_string_array(value: &serde_json::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// Merge two WikiInfo: union of plugins/themes/languages, local's other fields preserved.
/// Returns a new WikiInfo with the merged result.
pub fn merge_wiki_info(local: &WikiInfo, remote: &WikiInfo) -> WikiInfo {
    let mut result = local.clone();

    // Merge plugins
    let merged_plugins = union_strings(&local.plugins, &remote.plugins);
    result.plugins = merged_plugins.clone();
    result.raw["plugins"] = serde_json::Value::Array(
        merged_plugins
            .iter()
            .map(|s| serde_json::Value::String(s.clone()))
            .collect(),
    );

    // Merge themes
    let merged_themes = union_strings(&local.themes, &remote.themes);
    result.themes = merged_themes.clone();
    result.raw["themes"] = serde_json::Value::Array(
        merged_themes
            .iter()
            .map(|s| serde_json::Value::String(s.clone()))
            .collect(),
    );

    // Merge languages
    let merged_languages = union_strings(&local.languages, &remote.languages);
    result.languages = merged_languages.clone();
    result.raw["languages"] = serde_json::Value::Array(
        merged_languages
            .iter()
            .map(|s| serde_json::Value::String(s.clone()))
            .collect(),
    );

    result
}

/// Union two string lists, preserving order (local first, then new items from remote)
fn union_strings(local: &[String], remote: &[String]) -> Vec<String> {
    let mut result = local.to_vec();
    let existing: HashSet<&str> = local.iter().map(|s| s.as_str()).collect();
    for item in remote {
        if !existing.contains(item.as_str()) {
            result.push(item.clone());
        }
    }
    result
}

/// Compute SHA-256 hash of a string
pub fn hash_content(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Check if a plugin is available in bundled resources.
/// Plugin names are like "tiddlywiki/markdown" which maps to
/// `{resources_dir}/plugins/tiddlywiki/markdown/`.
pub fn is_bundled_plugin(resources_dir: &Path, plugin_name: &str) -> bool {
    resources_dir.join("plugins").join(plugin_name).is_dir()
}

/// Check if a plugin is available in bundled themes.
/// Theme names are like "tiddlywiki/vanilla" which maps to
/// `{resources_dir}/themes/tiddlywiki/vanilla/`.
pub fn is_bundled_theme(resources_dir: &Path, theme_name: &str) -> bool {
    resources_dir.join("themes").join(theme_name).is_dir()
}

/// Check if a language is available in bundled resources.
/// Language names are like "en-US" which maps to `{resources_dir}/languages/en-US/`.
pub fn is_bundled_language(resources_dir: &Path, language_name: &str) -> bool {
    resources_dir.join("languages").join(language_name).is_dir()
}

/// Check if a plugin/theme/language is available in the synced plugins directory.
/// Plugin names like "tiddlywiki/markdown" map to `{synced_dir}/plugins/tiddlywiki/markdown/`.
/// Theme names map to `{synced_dir}/themes/...`, languages to `{synced_dir}/languages/...`.
pub fn is_synced_item(synced_dir: &Path, category: &str, name: &str) -> bool {
    synced_dir.join(category).join(name).is_dir()
}

/// Find plugin directory on source device (for sending).
/// Checks wiki folder first, then synced plugins, then extra paths
/// (TIDDLYWIKI_PLUGIN_PATH / custom plugin path), then bundled.
pub fn find_item_dir(
    wiki_folder: &Path,
    resources_dir: &Path,
    synced_dir: &Path,
    extra_plugin_dirs: &[PathBuf],
    category: &str,
    name: &str,
) -> Option<PathBuf> {
    // 1. Check wiki folder (e.g. {wiki}/plugins/my-plugin/)
    let wiki_local = wiki_folder.join(category).join(name);
    if wiki_local.is_dir() {
        return Some(wiki_local);
    }

    // 2. Check synced plugins dir
    let synced = synced_dir.join(category).join(name);
    if synced.is_dir() {
        return Some(synced);
    }

    // 3. Check extra plugin paths (TIDDLYWIKI_PLUGIN_PATH, custom plugin dir)
    for extra_dir in extra_plugin_dirs {
        let candidate = extra_dir.join(name);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }

    // 4. Check bundled resources
    let bundled = resources_dir.join(category).join(name);
    if bundled.is_dir() {
        return Some(bundled);
    }

    None
}

/// Compute SHA-256 manifest for a plugin/theme/language directory.
/// Returns list of (relative_path, sha256_hex, file_size) for each file.
pub fn item_dir_manifest(item_dir: &Path) -> Vec<AttachmentFileInfo> {
    let mut files = Vec::new();
    collect_files_recursive(item_dir, item_dir, &mut files);
    files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    files
}

fn collect_files_recursive(base: &Path, current: &Path, files: &mut Vec<AttachmentFileInfo>) {
    let entries = match std::fs::read_dir(current) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files_recursive(base, &path, files);
        } else if path.is_file() {
            let rel_path = match path.strip_prefix(base) {
                Ok(r) => r.to_string_lossy().to_string(),
                Err(_) => continue,
            };
            let metadata = match std::fs::metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            let hash = match hash_file(&path) {
                Some(h) => h,
                None => continue,
            };
            files.push(AttachmentFileInfo {
                rel_path,
                sha256_hex: hash,
                file_size: metadata.len(),
            });
        }
    }
}

/// Hash a file with SHA-256
fn hash_file(path: &Path) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    Some(format!("{:x}", hasher.finalize()))
}

/// Get the list of new items (in remote but not in local) for a specific category
pub fn new_items(local: &[String], remote: &[String]) -> Vec<String> {
    let existing: HashSet<&str> = local.iter().map(|s| s.as_str()).collect();
    remote
        .iter()
        .filter(|item| !existing.contains(item.as_str()))
        .cloned()
        .collect()
}

/// Get the list of shared items (in both local and remote) for a specific category
pub fn shared_items(local: &[String], remote: &[String]) -> Vec<String> {
    let existing: HashSet<&str> = local.iter().map(|s| s.as_str()).collect();
    remote
        .iter()
        .filter(|item| existing.contains(item.as_str()))
        .cloned()
        .collect()
}

/// Read the version string from a plugin.info file in a plugin directory.
/// Returns None if plugin.info doesn't exist or has no version field.
pub fn read_plugin_version(item_dir: &Path) -> Option<String> {
    let info_path = item_dir.join("plugin.info");
    let content = std::fs::read_to_string(&info_path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;
    json.get("version")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Compare two version strings (e.g. "0.0.3" vs "0.0.2").
/// Returns true if `a` is newer than `b`.
/// Falls back to lexicographic comparison if not purely numeric segments.
pub fn version_is_newer(a: &str, b: &str) -> bool {
    let a_parts: Vec<&str> = a.split('.').collect();
    let b_parts: Vec<&str> = b.split('.').collect();
    let max_len = std::cmp::max(a_parts.len(), b_parts.len());
    for i in 0..max_len {
        let a_seg = a_parts.get(i).copied().unwrap_or("0");
        let b_seg = b_parts.get(i).copied().unwrap_or("0");
        // Try numeric comparison first
        match (a_seg.parse::<u64>(), b_seg.parse::<u64>()) {
            (Ok(an), Ok(bn)) => {
                if an != bn {
                    return an > bn;
                }
            }
            _ => {
                // Fall back to lexicographic
                if a_seg != b_seg {
                    return a_seg > b_seg;
                }
            }
        }
    }
    false // equal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merge_union() {
        let local = WikiInfo::parse(r#"{"plugins":["tiddlywiki/tiddlyweb","tiddlywiki/filesystem"],"themes":["tiddlywiki/vanilla"],"languages":[]}"#).unwrap();
        let remote = WikiInfo::parse(r#"{"plugins":["tiddlywiki/tiddlyweb","tiddlywiki/markdown"],"themes":["tiddlywiki/vanilla","tiddlywiki/snowwhite"],"languages":["de-DE"]}"#).unwrap();
        let merged = merge_wiki_info(&local, &remote);
        assert_eq!(merged.plugins, vec!["tiddlywiki/tiddlyweb", "tiddlywiki/filesystem", "tiddlywiki/markdown"]);
        assert_eq!(merged.themes, vec!["tiddlywiki/vanilla", "tiddlywiki/snowwhite"]);
        assert_eq!(merged.languages, vec!["de-DE"]);
    }

    #[test]
    fn test_hash_content() {
        let hash = hash_content("hello");
        assert_eq!(hash, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
    }
}
