//! Encoding utilities for drag-drop content
//!
//! Handles detection and conversion of various text encodings commonly
//! found in clipboard/drag-drop data across platforms.

#![allow(dead_code)]

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
