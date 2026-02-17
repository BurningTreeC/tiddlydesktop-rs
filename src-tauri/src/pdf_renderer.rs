/// PDFium-based PDF rendering module.
///
/// Provides handle-based API: open → render pages → close.
/// Selection uses PDFium's own text geometry — no font overlay needed.
/// Hit-testing, highlight rects, and text extraction all use the same
/// PdfRenderConfig as rendering, ensuring pixel-perfect coordinate alignment.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{Mutex, OnceLock, atomic::{AtomicU64, Ordering}};

use image::ImageFormat;
use pdfium_render::prelude::*;
use serde::Serialize;

/// Wrapper around Pdfium that implements Send + Sync.
///
/// SAFETY: pdfium-render's `thread_safe` feature serializes all PDFium FFI calls
/// through an internal mutex. We further protect our state with a Mutex on the
/// document map, so concurrent access is safe.
struct SendSyncPdfium(Pdfium);
unsafe impl Send for SendSyncPdfium {}
unsafe impl Sync for SendSyncPdfium {}

/// Global PDFium instance (loaded once at startup)
static PDFIUM: OnceLock<SendSyncPdfium> = OnceLock::new();

/// Wrapper around PdfDocument that implements Send + Sync.
/// See SendSyncPdfium safety comment — same reasoning applies.
struct SendSyncDoc(PdfDocument<'static>);
unsafe impl Send for SendSyncDoc {}
unsafe impl Sync for SendSyncDoc {}

/// Open PDF documents keyed by handle ID.
static DOCUMENTS: OnceLock<Mutex<HashMap<u64, SendSyncDoc>>> = OnceLock::new();

/// Monotonically increasing handle counter
static NEXT_HANDLE: AtomicU64 = AtomicU64::new(1);

fn documents() -> &'static Mutex<HashMap<u64, SendSyncDoc>> {
    DOCUMENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Build the same PdfRenderConfig used for rendering at a given width.
/// This ensures coordinate conversion uses the exact same transform chain.
fn render_config(width_px: u32) -> PdfRenderConfig {
    PdfRenderConfig::new()
        .set_target_width(width_px as Pixels)
        .set_clear_color(PdfColor::WHITE)
}

/// Page size in PDF points
#[derive(Serialize, Clone)]
pub struct PageSize {
    pub w: f32,
    pub h: f32,
}

/// Result of opening a PDF
#[derive(Serialize)]
pub struct PdfOpenResult {
    pub handle: u64,
    #[serde(rename = "pageCount")]
    pub page_count: u32,
    #[serde(rename = "pageSizes")]
    pub page_sizes: Vec<PageSize>,
}

/// Result of rendering a single page
#[derive(Serialize)]
pub struct PdfPageRenderResult {
    /// Base64-encoded PNG image
    #[serde(rename = "imageBase64")]
    pub image_base64: String,
    /// Flat array of character bounds in device pixels: [x1,y1,w1,h1, x2,y2,w2,h2, ...]
    /// Used for client-side hit-testing and highlight computation (zero round-trips during drag).
    #[serde(rename = "charBounds")]
    pub char_bounds: Vec<f32>,
}

/// A selection highlight rectangle in device pixels (top-left origin)
#[derive(Serialize, Clone)]
pub struct SelectionRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

/// Initialize PDFium by loading the dynamic library from the given path.
/// Call once at app startup. Safe to call multiple times (subsequent calls are no-ops).
pub fn init_pdfium(lib_path: &str) -> Result<(), String> {
    if PDFIUM.get().is_some() {
        return Ok(());
    }

    let bindings = Pdfium::bind_to_library(lib_path)
        .map_err(|e| format!("Failed to load PDFium library at {}: {:?}", lib_path, e))?;

    let pdfium = Pdfium::new(bindings);
    let _ = PDFIUM.set(SendSyncPdfium(pdfium));
    Ok(())
}

/// Check if PDFium has been initialized
pub fn is_initialized() -> bool {
    PDFIUM.get().is_some()
}

/// Open a PDF from raw bytes. Returns handle + page metadata.
pub fn pdf_open(bytes: Vec<u8>) -> Result<PdfOpenResult, String> {
    let pdfium = &PDFIUM.get().ok_or("PDFium not initialized")?.0;

    let doc = pdfium.load_pdf_from_byte_vec(bytes, None)
        .map_err(|e| format!("Failed to open PDF: {:?}", e))?;

    let page_count = doc.pages().len() as u32;

    let page_sizes: Vec<PageSize> = (0..page_count as u16).map(|i| {
        match doc.pages().page_size(i) {
            Ok(rect) => PageSize {
                w: rect.width().value,
                h: rect.height().value,
            },
            Err(_) => PageSize { w: 612.0, h: 792.0 }, // US Letter fallback
        }
    }).collect();

    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);

    // SAFETY: The PdfDocument borrows from the Pdfium instance which is in a static OnceLock
    // and will never be dropped during the program's lifetime. The byte vec is owned by the
    // document (load_pdf_from_byte_vec takes ownership). This transmute extends the lifetime
    // from the local scope to 'static, which is sound because the Pdfium instance is truly static.
    let doc: PdfDocument<'static> = unsafe { std::mem::transmute(doc) };

    documents().lock().unwrap().insert(handle, SendSyncDoc(doc));

    Ok(PdfOpenResult {
        handle,
        page_count,
        page_sizes,
    })
}

/// Render a single page as PNG.
/// `page_num` is 0-based. `width_px` is the target render width in pixels.
pub fn pdf_render_page(handle: u64, page_num: u32, width_px: u32) -> Result<PdfPageRenderResult, String> {
    let docs = documents().lock().unwrap();
    let wrapper = docs.get(&handle).ok_or("Invalid PDF handle")?;
    let doc = &wrapper.0;

    let page = doc.pages().get(page_num as u16)
        .map_err(|e| format!("Failed to get page {}: {:?}", page_num, e))?;

    let config = render_config(width_px);

    let bitmap = page.render_with_config(&config)
        .map_err(|e| format!("Failed to render page {}: {:?}", page_num, e))?;

    let dynamic_image = bitmap.as_image();

    let mut png_buf = Cursor::new(Vec::new());
    dynamic_image.write_to(&mut png_buf, ImageFormat::Png)
        .map_err(|e| format!("Failed to encode PNG: {:?}", e))?;

    let image_base64 = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        png_buf.into_inner(),
    );

    // Extract character bounds in device pixels for client-side hit-testing.
    // Using loose_bounds for slightly larger rects (easier to click, fewer gaps in highlights).
    let mut char_bounds = Vec::new();
    if let Ok(text) = page.text() {
        for ch in text.chars().iter() {
            if let Ok(bounds) = ch.loose_bounds() {
                // Convert corners from PDF points to device pixels
                let (px_left, px_top) = page.points_to_pixels(bounds.left(), bounds.top(), &config)
                    .unwrap_or((0, 0));
                let (px_right, px_bottom) = page.points_to_pixels(bounds.right(), bounds.bottom(), &config)
                    .unwrap_or((0, 0));
                let x = px_left.min(px_right) as f32;
                let y = px_top.min(px_bottom) as f32;
                let w = (px_left - px_right).unsigned_abs() as f32;
                let h = (px_top - px_bottom).unsigned_abs() as f32;
                char_bounds.push(x);
                char_bounds.push(y);
                char_bounds.push(w);
                char_bounds.push(h);
            } else {
                // Placeholder for chars without bounds (e.g. control chars)
                char_bounds.push(0.0);
                char_bounds.push(0.0);
                char_bounds.push(0.0);
                char_bounds.push(0.0);
            }
        }
    }

    Ok(PdfPageRenderResult {
        image_base64,
        char_bounds,
    })
}

/// Close a PDF document and release its handle.
pub fn pdf_close(handle: u64) {
    documents().lock().unwrap().remove(&handle);
}

/// Hit-test: find the character index at a pixel position.
/// `pixel_x`, `pixel_y` are relative to the rendered bitmap at `render_width`.
/// Returns the char index (>= 0) or -1 if no character found.
pub fn pdf_char_at_pos(handle: u64, page_num: u32, pixel_x: i32, pixel_y: i32, render_width: u32) -> Result<i32, String> {
    let docs = documents().lock().unwrap();
    let wrapper = docs.get(&handle).ok_or("Invalid PDF handle")?;
    let doc = &wrapper.0;

    let page = doc.pages().get(page_num as u16)
        .map_err(|e| format!("Failed to get page {}: {:?}", page_num, e))?;

    let config = render_config(render_width);

    // Convert pixel coordinates to PDF page points using the same config as rendering
    let (x_pt, y_pt) = page.pixels_to_points(pixel_x as Pixels, pixel_y as Pixels, &config)
        .map_err(|e| format!("pixels_to_points failed: {:?}", e))?;

    let text = page.text()
        .map_err(|e| format!("Failed to get page text: {:?}", e))?;
    let chars = text.chars();

    // Use get_char_near_point with a tolerance of 5 PDF points (~1.8mm)
    let tolerance = PdfPoints::new(5.0);
    match chars.get_char_near_point(x_pt, tolerance, y_pt, tolerance) {
        Some(ch) => Ok(ch.index() as i32),
        None => Ok(-1),
    }
}

/// Get selection highlight rectangles for a character range.
/// Returns rectangles in device pixels (top-left origin), using segments_subset
/// which merges characters on the same line into single rectangles.
pub fn pdf_selection_rects(handle: u64, page_num: u32, start_idx: u32, end_idx: u32, render_width: u32) -> Result<Vec<SelectionRect>, String> {
    let docs = documents().lock().unwrap();
    let wrapper = docs.get(&handle).ok_or("Invalid PDF handle")?;
    let doc = &wrapper.0;

    let page = doc.pages().get(page_num as u16)
        .map_err(|e| format!("Failed to get page {}: {:?}", page_num, e))?;

    let config = render_config(render_width);

    let text = page.text()
        .map_err(|e| format!("Failed to get page text: {:?}", e))?;

    let start = start_idx.min(end_idx) as usize;
    let end = start_idx.max(end_idx) as usize;
    let count = end - start + 1;

    let segments = text.segments_subset(start, count);
    let mut rects = Vec::new();

    for i in 0..segments.len() {
        if let Ok(segment) = segments.get(i) {
            let bounds = segment.bounds();
            // Convert the four corners from PDF points to device pixels
            let (px_left, px_top) = page.points_to_pixels(bounds.left(), bounds.top(), &config)
                .unwrap_or((0, 0));
            let (px_right, px_bottom) = page.points_to_pixels(bounds.right(), bounds.bottom(), &config)
                .unwrap_or((0, 0));

            // points_to_pixels returns top-left origin coordinates
            let x = px_left.min(px_right) as f32;
            let y = px_top.min(px_bottom) as f32;
            let w = (px_left - px_right).unsigned_abs() as f32;
            let h = (px_top - px_bottom).unsigned_abs() as f32;

            if w > 0.0 && h > 0.0 {
                rects.push(SelectionRect { x, y, w, h });
            }
        }
    }

    Ok(rects)
}

/// Extract text for a character range.
pub fn pdf_get_text(handle: u64, page_num: u32, start_idx: u32, end_idx: u32) -> Result<String, String> {
    let docs = documents().lock().unwrap();
    let wrapper = docs.get(&handle).ok_or("Invalid PDF handle")?;
    let doc = &wrapper.0;

    let page = doc.pages().get(page_num as u16)
        .map_err(|e| format!("Failed to get page {}: {:?}", page_num, e))?;

    let text = page.text()
        .map_err(|e| format!("Failed to get page text: {:?}", e))?;

    let start = start_idx.min(end_idx) as usize;
    let end = start_idx.max(end_idx) as usize;

    let result: String = text.chars().iter()
        .filter(|ch| {
            let idx = ch.index();
            idx >= start && idx <= end
        })
        .filter_map(|ch| ch.unicode_string())
        .collect();

    Ok(result)
}

/// Get total character count for a page.
pub fn pdf_char_count(handle: u64, page_num: u32) -> Result<u32, String> {
    let docs = documents().lock().unwrap();
    let wrapper = docs.get(&handle).ok_or("Invalid PDF handle")?;
    let doc = &wrapper.0;

    let page = doc.pages().get(page_num as u16)
        .map_err(|e| format!("Failed to get page {}: {:?}", page_num, e))?;

    let text = page.text()
        .map_err(|e| format!("Failed to get page text: {:?}", e))?;

    Ok(text.chars().len() as u32)
}
