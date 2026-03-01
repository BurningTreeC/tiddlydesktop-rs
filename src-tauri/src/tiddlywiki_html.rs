//! TiddlyWiki HTML manipulation functions
//!
//! This module handles extraction and injection of content in TiddlyWiki HTML files:
//! - Tiddler extraction from JSON and div formats
//! - Tiddler injection into TiddlyWiki HTML
//! - Favicon extraction from various TiddlyWiki formats
//! - Favicon extraction from wiki folders

use std::path::PathBuf;
use crate::utils;

/// Extract a tiddler's text content from TiddlyWiki HTML
/// Supports both JSON format (TW 5.2+) and div format (older)
pub fn extract_tiddler_from_html(html: &str, tiddler_title: &str) -> Option<String> {
    // TiddlyWiki stores tiddlers in multiple formats. Saved/modified tiddlers appear at the
    // END of the tiddler store as single-escaped JSON. Plugin-embedded tiddlers appear
    // earlier as double-escaped JSON. We need to find the LAST occurrence (most recent save).

    // First try single-escaped JSON format (saved tiddlers at end of file)
    // Format: {"title":"$:/TiddlyDesktop/WikiList","type":"application/json","text":"[...]"}
    let single_escaped_search = format!(r#"{{"title":"{}""#, tiddler_title);

    // Find the LAST occurrence (most recently saved version)
    if let Some(start_idx) = html.rfind(&single_escaped_search) {
        let after_title = &html[start_idx..std::cmp::min(start_idx + 2_000_000, html.len())];
        // Look for "text":" pattern (single-escaped)
        let text_pattern = r#""text":""#;
        if let Some(text_start) = after_title.find(text_pattern) {
            let text_content_start = text_start + 8; // length of "text":" (8 chars)
            if text_content_start < after_title.len() {
                let remaining = &after_title[text_content_start..];
                // Find closing " that's not escaped with backslash
                let mut end_pos = 0;
                let bytes = remaining.as_bytes();
                while end_pos < bytes.len() {
                    if bytes[end_pos] == b'"' {
                        // Check if escaped
                        let mut backslash_count = 0;
                        let mut check_pos = end_pos;
                        while check_pos > 0 && bytes[check_pos - 1] == b'\\' {
                            backslash_count += 1;
                            check_pos -= 1;
                        }
                        // If even number of backslashes, quote is not escaped
                        if backslash_count % 2 == 0 {
                            break;
                        }
                    }
                    end_pos += 1;
                }
                if end_pos < remaining.len() {
                    let text = &remaining[..end_pos];
                    // Unescape single-escaped JSON
                    let unescaped = text
                        .replace("\\n", "\n")
                        .replace("\\t", "\t")
                        .replace("\\r", "\r")
                        .replace("\\\"", "\"")
                        .replace("\\\\", "\\");
                    return Some(unescaped);
                }
            }
        }
    }

    // Try double-escaped JSON format (inside plugin bundles)
    // Format: \"$:/Title\":{\"title\":\"...\",\"text\":\"value\",...}
    let escaped_search = format!(r#"\"{}\":{{"#, tiddler_title);

    // Search from end to find the last (most recent) occurrence
    if let Some(start_idx) = html.rfind(&escaped_search) {
        let after_title = &html[start_idx..std::cmp::min(start_idx + 2_000_000, html.len())];
        let text_pattern = r#"\"text\":\""#;
        if let Some(text_start) = after_title.find(text_pattern) {
            let text_content_start = text_start + 11; // length of \"text\":\" (11 chars)
            if text_content_start < after_title.len() {
                let remaining = &after_title[text_content_start..];
                // Find the closing \" - need to skip escaped backslashes
                let mut end_pos = 0;
                let bytes = remaining.as_bytes();
                while end_pos < bytes.len().saturating_sub(1) {
                    if bytes[end_pos] == b'\\' && bytes[end_pos + 1] == b'"' {
                        // Check if this backslash is escaped (preceded by \\)
                        if end_pos >= 2 && bytes[end_pos - 1] == b'\\' && bytes[end_pos - 2] == b'\\' {
                            // This is \\\\" - the backslash is escaped, so \" is the real end
                            break;
                        } else if end_pos >= 1 && bytes[end_pos - 1] == b'\\' {
                            // This is \\" - skip it (escaped quote inside string)
                            end_pos += 2;
                            continue;
                        }
                        // Found unescaped \"
                        break;
                    }
                    end_pos += 1;
                }
                if end_pos < remaining.len() {
                    let text = &remaining[..end_pos];
                    // Unescape double-escaped JSON (embedded in JS string)
                    let unescaped = text
                        .replace("\\\\n", "\n")
                        .replace("\\\\t", "\t")
                        .replace("\\\\r", "\r")
                        .replace("\\\\\\\\", "\\");
                    return Some(unescaped);
                }
            }
        }
    }

    // Fallback to div format (older TiddlyWiki)
    let escaped_title = regex::escape(tiddler_title);
    let pattern = format!(
        r#"<div[^>]*\stitle="{}"[^>]*>([\s\S]*?)</div>"#,
        escaped_title
    );
    let re = regex::Regex::new(&pattern).ok()?;
    let caps = re.captures(html)?;
    let content = caps.get(1)?.as_str();
    // Decode HTML entities
    Some(utils::html_decode(content))
}

/// Inject or replace a tiddler in TiddlyWiki HTML
/// Works with modern TiddlyWiki JSON store format
/// Returns Err if no tiddler store marker is found in the HTML
pub fn inject_tiddler_into_html(html: &str, tiddler_title: &str, tiddler_type: &str, content: &str) -> Result<String, String> {
    // Modern TiddlyWiki (5.2+) uses JSON store in a script tag
    // Format: <script class="tiddlywiki-tiddler-store" type="application/json">[{...}]</script>

    // Use serde_json for proper JSON escaping of all string values
    // This handles all edge cases: quotes, backslashes, newlines, unicode, etc.
    let tiddler_obj = serde_json::json!({
        "title": tiddler_title,
        "type": tiddler_type,
        "text": content
    });
    let new_tiddler = serde_json::to_string(&tiddler_obj).unwrap_or_else(|_| {
        // Fallback to empty object if serialization fails (should never happen)
        "{}".to_string()
    });

    // Find the tiddler store - look for the LAST one (TW can have multiple stores)
    // The store ends with ]</script>
    let store_end = r#"]</script>"#;

    if let Some(end_pos) = html.rfind(store_end) {
        // Insert the new tiddler before the closing ]
        let mut result = String::with_capacity(html.len() + new_tiddler.len() + 10);
        result.push_str(&html[..end_pos]);
        result.push(',');
        result.push_str(&new_tiddler);
        result.push_str(&html[end_pos..]);
        return Ok(result);
    }

    // Fallback to div format for older TiddlyWiki
    // HTML-encode all values for safe attribute/content insertion
    let encoded_title = utils::html_encode(tiddler_title);
    let encoded_type = utils::html_encode(tiddler_type);
    let encoded_content = utils::html_encode(content);
    let new_div = format!(
        r#"<div title="{}" type="{}">{}</div>"#,
        encoded_title, encoded_type, encoded_content
    );

    let store_end_markers = [
        "</div><!--~~ Library modules ~~-->",
        r#"</div><script"#,
    ];

    for marker in &store_end_markers {
        if let Some(pos) = html.find(marker) {
            let mut result = String::with_capacity(html.len() + new_div.len() + 1);
            result.push_str(&html[..pos]);
            result.push_str(&new_div);
            result.push('\n');
            result.push_str(&html[pos..]);
            return Ok(result);
        }
    }

    // No store marker found - return error instead of silently returning unchanged HTML
    Err(format!("No tiddler store marker found in HTML for injecting '{}'", tiddler_title))
}

/// Extract all tiddlers from all JSON tiddler stores in TiddlyWiki HTML.
/// Finds all `<script class="tiddlywiki-tiddler-store" type="application/json">` tags,
/// parses each JSON array, and returns a flat Vec of all tiddler objects.
pub fn extract_all_tiddlers_from_html(html: &str) -> Vec<serde_json::Value> {
    let mut tiddlers = Vec::new();
    let store_start_marker = r#"<script class="tiddlywiki-tiddler-store" type="application/json">"#;
    let store_end_marker = "</script>";

    let mut search_pos = 0;
    while let Some(start_rel) = html[search_pos..].find(store_start_marker) {
        let content_start = search_pos + start_rel + store_start_marker.len();
        if let Some(end_rel) = html[content_start..].find(store_end_marker) {
            let json_str = &html[content_start..content_start + end_rel];
            match serde_json::from_str::<serde_json::Value>(json_str) {
                Ok(serde_json::Value::Array(arr)) => {
                    tiddlers.extend(arr);
                }
                Ok(_) => {
                    eprintln!("[TiddlyDesktop] Warning: tiddler store is not a JSON array");
                }
                Err(e) => {
                    eprintln!("[TiddlyDesktop] Warning: failed to parse tiddler store JSON: {}", e);
                }
            }
            search_pos = content_start + end_rel + store_end_marker.len();
        } else {
            break;
        }
    }
    tiddlers
}

/// Check if a tiddler title is a system/plugin tiddler that should be updated from bundled.
/// User-data tiddlers (wiki list, config, etc.) are NOT considered system tiddlers.
fn is_bundled_system_tiddler(title: &str) -> bool {
    title.starts_with("$:/plugins/")
        || title.starts_with("$:/boot/")
        || title == "$:/core"
        || title.starts_with("$:/themes/")
        || title.starts_with("$:/info/")
        || title.starts_with("$:/language/")
        || title.starts_with("$:/languages/")
        || title == "$:/TiddlyDesktop/AppVersion"
}

/// Build merged HTML by injecting user data tiddlers from the old wiki into
/// a fresh copy of the bundled HTML.
///
/// Strategy: keep the bundled HTML completely intact (preserving all system/plugin
/// tiddlers, boot scripts, and HTML structure). Extract only user-data tiddlers
/// from the old HTML and inject them as a NEW tiddler store right after the last
/// bundled store (so they override bundled defaults via TiddlyWiki's last-wins
/// processing order). This avoids re-serializing large plugin blobs and prevents
/// escaping issues (e.g. </script> inside JSON breaking the enclosing <script> tag).
pub fn build_merged_html(old_html: &str, bundled_html: &str) -> Result<String, String> {
    let old_tiddlers = extract_all_tiddlers_from_html(old_html);

    if old_tiddlers.is_empty() {
        eprintln!("[TiddlyDesktop] Migration: no tiddlers found in old wiki, using bundled as-is");
        return Ok(bundled_html.to_string());
    }

    // Collect titles from bundled HTML (only need titles, not full parse)
    let bundled_tiddlers = extract_all_tiddlers_from_html(bundled_html);
    let bundled_titles: std::collections::HashSet<String> = bundled_tiddlers.iter()
        .filter_map(|t| t.get("title").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .collect();

    // Keep only user-data tiddlers from old HTML (skip system/plugin tiddlers
    // which are already up-to-date in the bundled HTML)
    let user_tiddlers: Vec<&serde_json::Value> = old_tiddlers.iter()
        .filter(|t| {
            if let Some(title) = t.get("title").and_then(|v| v.as_str()) {
                !is_bundled_system_tiddler(title)
            } else {
                false
            }
        })
        .collect();

    eprintln!("[TiddlyDesktop] Migration: {} tiddlers in old wiki ({} user data, {} system/plugin skipped), {} in bundled",
        old_tiddlers.len(), user_tiddlers.len(), old_tiddlers.len() - user_tiddlers.len(), bundled_titles.len());

    if user_tiddlers.is_empty() {
        eprintln!("[TiddlyDesktop] Migration: no user data to preserve, using bundled as-is");
        return Ok(bundled_html.to_string());
    }

    // Serialize user tiddlers as JSON array
    let user_json = serde_json::to_string(&user_tiddlers)
        .map_err(|e| format!("Failed to serialize user tiddlers: {}", e))?;

    // CRITICAL: Escape </ as <\/ to prevent breaking the enclosing <script> tag.
    // TiddlyWiki does this natively; serde_json does not escape forward slashes.
    // <\/ is valid JSON (just an escaped forward slash), so parsers handle it fine.
    let user_json = user_json.replace("</", "<\\/");

    // Build the new tiddler store tag
    let injection = format!(
        "<script class=\"tiddlywiki-tiddler-store\" type=\"application/json\">{}</script>",
        user_json
    );

    // Find injection point: right after the last bundled tiddler store's </script>.
    // Must be BEFORE boot scripts so the DOM elements exist when boot.js reads them.
    let store_start_marker = r#"<script class="tiddlywiki-tiddler-store" type="application/json">"#;
    let last_store_start = bundled_html.rfind(store_start_marker)
        .ok_or("No tiddler store found in bundled HTML")?;
    let after_last_store = &bundled_html[last_store_start..];
    let store_close = after_last_store.find("</script>")
        .ok_or("Unterminated tiddler store in bundled HTML")?;
    let inject_pos = last_store_start + store_close + "</script>".len();

    // Build result: bundled HTML with user data store injected
    let mut result = String::with_capacity(bundled_html.len() + injection.len() + 1);
    result.push_str(&bundled_html[..inject_pos]);
    result.push('\n');
    result.push_str(&injection);
    result.push_str(&bundled_html[inject_pos..]);

    eprintln!("[TiddlyDesktop] Migration: injected {} user tiddlers ({} bytes) into bundled HTML",
        user_tiddlers.len(), user_json.len());

    Ok(result)
}

/// Extract favicon from the $:/favicon.ico tiddler in TiddlyWiki HTML
/// The tiddler contains base64-encoded image data with a type field
pub fn extract_favicon_from_tiddler(html: &str) -> Option<String> {
    // Helper to unescape JSON string content
    fn unescape_json_string(s: &str) -> String {
        let mut result = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next() {
                    Some('n') => {} // Skip newlines in base64
                    Some('r') => {} // Skip carriage returns
                    Some('t') => {} // Skip tabs
                    Some('/') => result.push('/'),
                    Some('\\') => result.push('\\'),
                    Some('"') => result.push('"'),
                    Some(other) => {
                        // Keep other escapes as-is (shouldn't happen in base64)
                        result.push('\\');
                        result.push(other);
                    }
                    None => result.push('\\'),
                }
            } else if c == '\n' || c == '\r' || c == '\t' || c == ' ' {
                // Skip whitespace in base64 (some encoders add line breaks)
                continue;
            } else {
                result.push(c);
            }
        }
        result
    }

    // Helper to extract a JSON string field value (handles escaping)
    fn extract_json_field(content: &str, field: &str, escaped: bool) -> Option<String> {
        let patterns: Vec<String> = if escaped {
            vec![
                format!(r#"\"{}\":\""#, field),
                format!(r#"\"{}\": \""#, field),
            ]
        } else {
            vec![
                format!(r#""{}":""#, field),
                format!(r#""{}" : ""#, field),
                format!(r#""{}\": \""#, field), // Mixed format sometimes seen
            ]
        };

        for pattern in &patterns {
            if let Some(start) = content.find(pattern.as_str()) {
                let after = &content[start + pattern.len()..];
                // Find closing quote (handle escaped quotes)
                let mut pos = 0;
                let bytes = after.as_bytes();
                while pos < bytes.len() {
                    if escaped {
                        // Look for \"
                        if pos + 1 < bytes.len() && bytes[pos] == b'\\' && bytes[pos + 1] == b'"' {
                            // Check if this backslash is itself escaped
                            let mut bs_count = 0;
                            let mut check = pos;
                            while check > 0 && bytes[check - 1] == b'\\' {
                                bs_count += 1;
                                check -= 1;
                            }
                            if bs_count % 2 == 0 {
                                let raw = &after[..pos];
                                return Some(unescape_json_string(raw));
                            }
                        }
                        pos += 1;
                    } else {
                        if bytes[pos] == b'"' {
                            // Check for escaped quote
                            let mut bs_count = 0;
                            let mut check = pos;
                            while check > 0 && bytes[check - 1] == b'\\' {
                                bs_count += 1;
                                check -= 1;
                            }
                            if bs_count % 2 == 0 {
                                let raw = &after[..pos];
                                return Some(unescape_json_string(raw));
                            }
                        }
                        pos += 1;
                    }
                }
            }
        }
        None
    }

    // Helper to check if content looks like base64 image data
    fn looks_like_base64_image(s: &str) -> bool {
        if s.len() < 20 { return false; }
        // PNG starts with iVBOR, GIF with R0lGO, JPEG with /9j/, ICO varies
        // Valid base64 chars only
        let first_chars: String = s.chars().take(20).collect();
        first_chars.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
            && (s.starts_with("iVBOR")   // PNG
                || s.starts_with("R0lGO") // GIF
                || s.starts_with("/9j/")  // JPEG
                || s.starts_with("AAAB")  // ICO often
                || s.len() > 100)         // Or just long enough to be real data
    }

    // Strategy 1: Look for tiddler in JSON array format (modern TiddlyWiki 5.2+)
    let title_patterns = [
        r#""title":"$:/favicon.ico""#,
        r#""title": "$:/favicon.ico""#,
    ];

    for title_pattern in &title_patterns {
        let mut search_pos = 0;
        while let Some(rel_idx) = html[search_pos..].find(title_pattern) {
            let title_idx = search_pos + rel_idx;
            search_pos = title_idx + title_pattern.len();

            eprintln!("[TiddlyDesktop] Favicon: Strategy 1 - checking pattern '{}' at index {}", title_pattern, title_idx);

            // Find the END of this tiddler object first
            let after_title = &html[title_idx..];
            let mut obj_end_rel = 0;
            let mut in_string = false;
            let mut escape_next = false;
            let mut brace_depth = 1;

            for (i, c) in after_title.char_indices() {
                if escape_next {
                    escape_next = false;
                    continue;
                }
                match c {
                    '\\' if in_string => escape_next = true,
                    '"' => in_string = !in_string,
                    '{' if !in_string => brace_depth += 1,
                    '}' if !in_string => {
                        brace_depth -= 1;
                        if brace_depth == 0 {
                            obj_end_rel = i + 1;
                            break;
                        }
                    }
                    _ => {}
                }
                if i > 500_000 { break; }
            }

            if obj_end_rel == 0 { continue; }

            let obj_end_abs = title_idx + obj_end_rel;

            // Scan BACKWARDS from title to find the opening {
            let search_back_limit = title_idx.min(1_000_000);
            let search_back_start = title_idx.saturating_sub(search_back_limit);
            let before_title = &html[search_back_start..title_idx];

            let mut obj_start_rel: Option<usize> = None;
            let bytes = before_title.as_bytes();
            let mut i = bytes.len();
            let mut brace_depth = 1;
            let mut in_string = false;

            while i > 0 {
                i -= 1;
                let c = bytes[i];

                if in_string && i > 0 {
                    let mut bs_count = 0;
                    let mut j = i;
                    while j > 0 && bytes[j - 1] == b'\\' {
                        bs_count += 1;
                        j -= 1;
                    }
                    if bs_count % 2 == 1 { continue; }
                }

                match c {
                    b'"' => in_string = !in_string,
                    b'}' if !in_string => brace_depth += 1,
                    b'{' if !in_string => {
                        brace_depth -= 1;
                        if brace_depth == 0 {
                            obj_start_rel = Some(i);
                            break;
                        }
                    }
                    _ => {}
                }
            }

            let obj_start = match obj_start_rel {
                Some(rel) => search_back_start + rel,
                None => continue,
            };

            let tiddler_json = &html[obj_start..obj_end_abs];
            eprintln!("[TiddlyDesktop] Favicon: extracted tiddler JSON ({} bytes)", tiddler_json.len());

            if let Some(text_content) = extract_json_field(tiddler_json, "text", false) {
                if looks_like_base64_image(&text_content) {
                    let mime_type = extract_json_field(tiddler_json, "type", false)
                        .unwrap_or_else(|| "image/x-icon".to_string());
                    let data_uri = format!("data:{};base64,{}", mime_type, text_content);
                    eprintln!("[TiddlyDesktop] Favicon: SUCCESS - type={}, base64_len={}", mime_type, text_content.len());
                    return Some(data_uri);
                }
            }
        }
    }

    // Strategy 2: Double-escaped format (inside plugin bundles or old wikis)
    let escaped_patterns = [
        r#"\"$:/favicon.ico\":{"#,
        r#"\"title\":\"$:/favicon.ico\""#,
    ];

    for pattern in &escaped_patterns {
        let mut search_pos = 0;
        while let Some(rel_idx) = html[search_pos..].find(pattern) {
            let title_idx = search_pos + rel_idx;
            search_pos = title_idx + pattern.len();

            let after_title = &html[title_idx..];
            let mut obj_end_rel = 0;
            let mut brace_depth = 1;
            let bytes = after_title.as_bytes();
            let mut i = 0;

            while i < bytes.len() {
                match bytes[i] {
                    b'\\' => i += 2,
                    b'{' => { brace_depth += 1; i += 1; }
                    b'}' => {
                        brace_depth -= 1;
                        if brace_depth == 0 { obj_end_rel = i + 1; break; }
                        i += 1;
                    }
                    _ => i += 1,
                }
                if i > 500_000 { break; }
            }

            if obj_end_rel == 0 { continue; }

            let obj_end_abs = title_idx + obj_end_rel;

            let search_back_limit = title_idx.min(1_000_000);
            let search_back_start = title_idx.saturating_sub(search_back_limit);
            let before_title = &html[search_back_start..title_idx];

            let mut obj_start_rel: Option<usize> = None;
            let bytes = before_title.as_bytes();
            let mut i = bytes.len();
            let mut brace_depth = 1;

            while i > 0 {
                i -= 1;
                if i > 0 && bytes[i - 1] == b'\\' { continue; }
                match bytes[i] {
                    b'}' => brace_depth += 1,
                    b'{' => {
                        brace_depth -= 1;
                        if brace_depth == 0 { obj_start_rel = Some(i); break; }
                    }
                    _ => {}
                }
            }

            let obj_start = match obj_start_rel {
                Some(rel) => search_back_start + rel,
                None => continue,
            };

            let tiddler_content = &html[obj_start..obj_end_abs];

            if let Some(text_content) = extract_json_field(tiddler_content, "text", true) {
                if looks_like_base64_image(&text_content) {
                    let mime_type = extract_json_field(tiddler_content, "type", true)
                        .unwrap_or_else(|| "image/x-icon".to_string());
                    eprintln!("[TiddlyDesktop] Favicon: Strategy 2 SUCCESS");
                    return Some(format!("data:{};base64,{}", mime_type, text_content));
                }
            }
        }
    }

    // Strategy 3: Debug logging
    let has_unescaped = html.contains(r#""$:/favicon.ico""#);
    let has_escaped = html.contains(r#"\"$:/favicon.ico\""#);

    if has_unescaped || has_escaped {
        eprintln!("[TiddlyDesktop] Favicon: tiddler reference found but extraction failed");
        if html.contains(r#""_canonical_uri""#) || html.contains(r#"\"_canonical_uri\""#) {
            eprintln!("[TiddlyDesktop] Favicon: has _canonical_uri (external reference)");
        }
    }

    None
}

/// Extract favicon from wiki HTML content
/// First tries the <link> tag in <head>, then falls back to $:/favicon.ico tiddler
pub fn extract_favicon(content: &str) -> Option<String> {
    // First try: Look for favicon link with data URI in the head section
    let head_end = content.find("</head>")
        .or_else(|| content.find("</HEAD>"))
        .unwrap_or(content.len().min(500_000));
    let search_content = &content[..head_end];

    // Find favicon link elements
    for pattern in &["<link", "<LINK"] {
        let mut search_pos = 0;
        while let Some(link_start) = search_content[search_pos..].find(pattern) {
            let abs_start = search_pos + link_start;
            if let Some(link_end) = search_content[abs_start..].find('>') {
                let link_tag = &search_content[abs_start..abs_start + link_end + 1];
                let link_tag_lower = link_tag.to_lowercase();

                if (link_tag_lower.contains("icon") || link_tag_lower.contains("faviconlink"))
                    && link_tag_lower.contains("href=")
                {
                    if let Some(href_start) = link_tag.to_lowercase().find("href=") {
                        let after_href = &link_tag[href_start + 5..];
                        let quote_char = after_href.chars().next();
                        if let Some(q) = quote_char {
                            if q == '"' || q == '\'' {
                                if let Some(href_end) = after_href[1..].find(q) {
                                    let href = &after_href[1..href_end + 1];
                                    if href.starts_with("data:image") {
                                        return Some(href.to_string());
                                    }
                                }
                            }
                        }
                    }
                }
                search_pos = abs_start + link_end + 1;
            } else {
                break;
            }
        }
    }

    // Second try: Extract from $:/favicon.ico tiddler
    extract_favicon_from_tiddler(content)
}

/// Extract favicon from a wiki folder by reading the favicon file
pub async fn extract_favicon_from_folder(wiki_path: &PathBuf) -> Option<String> {
    use base64::{engine::general_purpose::STANDARD, Engine};

    let tiddlers_path = wiki_path.join("tiddlers");

    // TiddlyWiki stores $:/favicon.ico as $__favicon.ico.EXT
    let favicon_patterns = [
        ("$__favicon.ico.png", "image/png"),
        ("$__favicon.ico.jpg", "image/jpeg"),
        ("$__favicon.ico.jpeg", "image/jpeg"),
        ("$__favicon.ico.gif", "image/gif"),
        ("$__favicon.ico.ico", "image/x-icon"),
        ("$__favicon.ico", "image/x-icon"),
        ("favicon.ico", "image/x-icon"),
        ("favicon.png", "image/png"),
    ];

    for (filename, mime_type) in &favicon_patterns {
        let favicon_path = tiddlers_path.join(filename);
        if let Ok(data) = tokio::fs::read(&favicon_path).await {
            let base64_data = STANDARD.encode(&data);
            return Some(format!("data:{};base64,{}", mime_type, base64_data));
        }
    }

    // Also check for .tid file format
    let tid_patterns = [
        "$__favicon.ico.png.tid",
        "$__favicon.ico.tid",
    ];

    for tid_filename in &tid_patterns {
        let tid_path = tiddlers_path.join(tid_filename);
        if let Ok(content) = tokio::fs::read_to_string(&tid_path).await {
            if let Some(blank_pos) = content.find("\n\n") {
                let text_content = content[blank_pos + 2..].trim();
                if !text_content.is_empty() {
                    let mime_type = if content.contains("type: image/png") {
                        "image/png"
                    } else if content.contains("type: image/jpeg") {
                        "image/jpeg"
                    } else {
                        "image/png"
                    };
                    return Some(format!("data:{};base64,{}", mime_type, text_content));
                }
            }
        }
    }

    None
}
