//! Encoding utilities for drag-drop content
//!
//! Handles detection and conversion of various text encodings commonly
//! found in clipboard/drag-drop data across platforms.
//!
//! Also provides parsers for browser-specific custom clipboard formats:
//! - Chromium's `chromium/x-web-custom-data` (Pickle format)
//! - Firefox's `application/x-moz-custom-clipdata`

use std::collections::HashMap;

/// Decode string data with proper encoding detection
/// Handles UTF-8, UTF-16 LE/BE (with or without BOM), and falls back to lossy UTF-8
pub fn decode_string(data: &[u8]) -> String {
    if data.is_empty() {
        return String::new();
    }

    // Find the actual end of the string (first null terminator, accounting for encoding)
    let data = if data.len() >= 2 {
        // Check for UTF-16 BOM or pattern
        if data[0] == 0xFF && data[1] == 0xFE {
            // UTF-16 LE BOM - find double-null terminator
            let end = find_utf16_null(&data[2..]).map(|i| i + 2).unwrap_or(data.len());
            &data[..end]
        } else if data[0] == 0xFE && data[1] == 0xFF {
            // UTF-16 BE BOM - find double-null terminator
            let end = find_utf16_null(&data[2..]).map(|i| i + 2).unwrap_or(data.len());
            &data[..end]
        } else if looks_like_utf16_le(data) {
            // UTF-16 LE without BOM (common on Windows)
            let end = find_utf16_null(data).unwrap_or(data.len());
            &data[..end]
        } else {
            // UTF-8 or ASCII - find single null
            let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
            &data[..end]
        }
    } else {
        let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
        &data[..end]
    };

    // Check for UTF-8 BOM
    let data = if data.len() >= 3 && data[0] == 0xEF && data[1] == 0xBB && data[2] == 0xBF {
        &data[3..]
    } else {
        data
    };

    // Try UTF-16 LE with BOM
    if data.len() >= 2 && data[0] == 0xFF && data[1] == 0xFE {
        if let Some(text) = decode_utf16_le(&data[2..]) {
            return text;
        }
    }

    // Try UTF-16 BE with BOM
    if data.len() >= 2 && data[0] == 0xFE && data[1] == 0xFF {
        if let Some(text) = decode_utf16_be(&data[2..]) {
            return text;
        }
    }

    // Check if it looks like UTF-16 LE without BOM (every other byte is 0 for ASCII range)
    if looks_like_utf16_le(data) {
        if let Some(text) = decode_utf16_le(data) {
            return text;
        }
    }

    // Try UTF-8
    if let Ok(text) = std::str::from_utf8(data) {
        return text.to_string();
    }

    // Fallback: lossy UTF-8 conversion
    String::from_utf8_lossy(data).to_string()
}

/// Check if data looks like UTF-16 LE (ASCII characters have 0x00 as second byte)
fn looks_like_utf16_le(data: &[u8]) -> bool {
    if data.len() < 4 {
        return false;
    }
    // Check first few characters - if they're ASCII range with null high bytes, it's likely UTF-16 LE
    let mut null_count = 0;
    let mut check_count = 0;
    for chunk in data.chunks(2).take(10) {
        if chunk.len() == 2 {
            check_count += 1;
            if chunk[1] == 0 && chunk[0] != 0 && chunk[0] < 128 {
                null_count += 1;
            }
        }
    }
    check_count > 0 && null_count > check_count / 2
}

/// Find UTF-16 null terminator (two consecutive zero bytes on even boundary)
fn find_utf16_null(data: &[u8]) -> Option<usize> {
    for (i, chunk) in data.chunks(2).enumerate() {
        if chunk.len() == 2 && chunk[0] == 0 && chunk[1] == 0 {
            return Some(i * 2);
        }
    }
    None
}

/// Decode UTF-16 LE data to String
fn decode_utf16_le(data: &[u8]) -> Option<String> {
    if data.len() % 2 != 0 {
        return None;
    }
    let u16_data: Vec<u16> = data
        .chunks(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .take_while(|&c| c != 0)
        .collect();
    String::from_utf16(&u16_data).ok()
}

/// Decode UTF-16 BE data to String
fn decode_utf16_be(data: &[u8]) -> Option<String> {
    if data.len() % 2 != 0 {
        return None;
    }
    let u16_data: Vec<u16> = data
        .chunks(2)
        .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
        .take_while(|&c| c != 0)
        .collect();
    String::from_utf16(&u16_data).ok()
}

/// Decode UTF-16LE bytes to a String (lossy)
pub fn decode_utf16le(data: &[u8]) -> String {
    if data.len() < 2 {
        return String::new();
    }

    let u16_vec: Vec<u16> = data
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();

    String::from_utf16_lossy(&u16_vec)
}

/// Parse Firefox's `application/x-moz-custom-clipdata` format
///
/// Format (all big-endian):
/// - 4 bytes: number of entries
/// - For each entry:
///   - 4 bytes big-endian: length of MIME type in bytes (UTF-16LE)
///   - MIME type as UTF-16LE
///   - 4 bytes big-endian: length of data in bytes (UTF-16LE)
///   - Data as UTF-16LE
pub fn parse_moz_custom_clipdata(data: &[u8]) -> Option<HashMap<String, String>> {
    if data.len() < 8 {
        return None;
    }

    let mut result = HashMap::new();
    let mut offset = 0;

    // Read number of entries (4 bytes big-endian)
    let num_entries = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    offset += 4;

    eprintln!(
        "[TiddlyDesktop] DragDrop: Mozilla clipdata: {} entries",
        num_entries
    );

    for i in 0..num_entries {
        if offset + 4 > data.len() {
            eprintln!(
                "[TiddlyDesktop] DragDrop: Mozilla clipdata: truncated at entry {} (mime type length)",
                i
            );
            break;
        }

        // Read MIME type length (4 bytes big-endian, in bytes)
        let mime_len = u32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        offset += 4;

        if offset + mime_len > data.len() {
            eprintln!(
                "[TiddlyDesktop] DragDrop: Mozilla clipdata: truncated at entry {} (mime type data)",
                i
            );
            break;
        }

        // Read MIME type as UTF-16LE
        let mime_bytes = &data[offset..offset + mime_len];
        let mime_type = decode_utf16le(mime_bytes);
        offset += mime_len;

        if offset + 4 > data.len() {
            eprintln!(
                "[TiddlyDesktop] DragDrop: Mozilla clipdata: truncated at entry {} (content length)",
                i
            );
            break;
        }

        // Read content length (4 bytes big-endian, in bytes)
        let content_len = u32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        offset += 4;

        if offset + content_len > data.len() {
            eprintln!(
                "[TiddlyDesktop] DragDrop: Mozilla clipdata: truncated at entry {} (content data, need {} have {})",
                i, content_len, data.len() - offset
            );
            // Try to read what we can
            let available = data.len() - offset;
            let content_bytes = &data[offset..offset + available];
            let content = decode_utf16le(content_bytes);
            if !mime_type.is_empty() && !content.is_empty() {
                result.insert(mime_type, content);
            }
            break;
        }

        // Read content as UTF-16LE
        let content_bytes = &data[offset..offset + content_len];
        let content = decode_utf16le(content_bytes);
        offset += content_len;

        eprintln!(
            "[TiddlyDesktop] DragDrop: Mozilla clipdata entry {}: {} = {} bytes -> {} chars",
            i,
            mime_type,
            content_len,
            content.len()
        );

        if !mime_type.is_empty() {
            result.insert(mime_type, content);
        }
    }

    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}

/// Parse Chrome's `chromium/x-web-custom-data` format (Pickle)
///
/// Format (all little-endian):
/// - 4 bytes: payload size
/// - 4 bytes: number of entries
/// - For each entry:
///   - 4 bytes: MIME type length (in chars, not bytes)
///   - MIME type as UTF-16LE (padded to 4-byte boundary)
///   - 4 bytes: data length (in chars, not bytes)
///   - Data as UTF-16LE (padded to 4-byte boundary)
pub fn parse_chromium_custom_data(data: &[u8]) -> Option<HashMap<String, String>> {
    if data.len() < 12 {
        return None;
    }

    let mut result = HashMap::new();
    let mut offset = 0;

    // Skip payload size (4 bytes) - this is the Pickle header
    offset += 4;

    // Read number of entries (4 bytes little-endian)
    if offset + 4 > data.len() {
        return None;
    }
    let num_entries = u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ]) as usize;
    offset += 4;

    eprintln!(
        "[TiddlyDesktop] DragDrop: Chrome clipdata: {} entries",
        num_entries
    );

    for i in 0..num_entries {
        if offset + 4 > data.len() {
            break;
        }

        // Read MIME type length (in UTF-16 chars)
        let mime_char_len = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        offset += 4;

        let mime_byte_len = mime_char_len * 2;
        if offset + mime_byte_len > data.len() {
            break;
        }

        // Read MIME type as UTF-16LE
        let mime_bytes = &data[offset..offset + mime_byte_len];
        let mime_type = decode_utf16le(mime_bytes);
        offset += mime_byte_len;

        // Align to 4-byte boundary
        let padding = (4 - (mime_byte_len % 4)) % 4;
        offset += padding;

        if offset + 4 > data.len() {
            break;
        }

        // Read content length (in UTF-16 chars)
        let content_char_len = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        offset += 4;

        let content_byte_len = content_char_len * 2;
        if offset + content_byte_len > data.len() {
            // Try to read what we can
            let available = data.len() - offset;
            let content_bytes = &data[offset..offset + available];
            let content = decode_utf16le(content_bytes);
            if !mime_type.is_empty() && !content.is_empty() {
                result.insert(mime_type, content);
            }
            break;
        }

        // Read content as UTF-16LE
        let content_bytes = &data[offset..offset + content_byte_len];
        let content = decode_utf16le(content_bytes);
        offset += content_byte_len;

        // Align to 4-byte boundary
        let padding = (4 - (content_byte_len % 4)) % 4;
        offset += padding;

        eprintln!(
            "[TiddlyDesktop] DragDrop: Chrome clipdata entry {}: {} = {} chars",
            i,
            mime_type,
            content.len()
        );

        if !mime_type.is_empty() {
            result.insert(mime_type, content);
        }
    }

    if result.is_empty() {
        None
    } else {
        Some(result)
    }
}
