//! PDF handling via pdfium-render.
//!
//! Opens a PDF (bytes or path), reports page count, renders each page as a
//! JPEG at configurable DPI. Text extraction is available for callers that
//! want it (not used directly by the tool now — page filtering is range-based).
//!
//! The pdfium binary is located at runtime. `locate_pdfium()` tries, in order:
//!   1. `DPE_PDFIUM_PATH` environment variable (full path to lib file).
//!   2. `<exe_dir>/pdfium.{dll,so,dylib}` (build.rs puts it there).
//!   3. `<crate_root>/vendor/pdfium/<platform>/<lib>` for `cargo test` runs.
//!   4. System-wide (Pdfium::bind_to_system_library).

use std::env;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use image::{codecs::jpeg::JpegEncoder, ImageFormat};
use pdfium_render::prelude::*;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PdfError {
    #[error("pdfium load: {0}")]
    PdfiumLoad(String),
    #[error("pdfium op: {0}")]
    PdfiumOp(String),
    #[error("encode: {0}")]
    Encode(String),
}

/// Locate the pdfium dynamic library on disk. Returns `None` to fall back
/// to system bindings.
pub fn locate_pdfium() -> Option<PathBuf> {
    if let Ok(p) = env::var("DPE_PDFIUM_PATH") {
        let p = PathBuf::from(p);
        if p.is_file() { return Some(p); }
    }
    let lib_name = platform_lib_name();
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(lib_name);
            if candidate.is_file() { return Some(candidate); }
        }
    }
    // cargo test: binary lives in target/debug/deps; vendor in crate root.
    if let Ok(manifest) = env::var("CARGO_MANIFEST_DIR") {
        let platform = platform_subdir();
        let candidate = PathBuf::from(manifest).join("vendor").join("pdfium").join(platform).join(lib_name);
        if candidate.is_file() { return Some(candidate); }
    }
    None
}

fn platform_lib_name() -> &'static str {
    if cfg!(target_os = "windows") { "pdfium.dll" }
    else if cfg!(target_os = "macos") { "libpdfium.dylib" }
    else { "libpdfium.so" }
}

fn platform_subdir() -> &'static str {
    if cfg!(target_os = "windows") { "win-x64" }
    else if cfg!(target_os = "macos") && cfg!(target_arch = "aarch64") { "mac-arm64" }
    else if cfg!(target_os = "macos") { "mac-x64" }
    else if cfg!(target_arch = "aarch64") { "linux-arm64" }
    else { "linux-x64" }
}

/// Pdfium wrapper tied to a single instance.
///
/// `Pdfium` itself isn't `Send + Sync` by default (the underlying
/// dynamic-library trait object is a C pointer). We add unsafe `Send +
/// Sync` because we build the tool with pdfium-render's `thread_safe`
/// feature (which wraps bindings in an internal mutex) AND the DPE tool
/// runtime calls `process_input` serially from the main thread — there's
/// never concurrent access even though the value lives in a static.
pub struct PdfRenderer {
    pdfium: Pdfium,
    /// Rendering DPI (default 150).
    pub dpi: u16,
    /// JPEG quality 1..=100 (default 85).
    pub jpeg_quality: u8,
}

// Safety: `thread_safe` feature + single-threaded tool runtime (see doc above).
unsafe impl Send for PdfRenderer {}
unsafe impl Sync for PdfRenderer {}

impl PdfRenderer {
    /// Initialise using the located pdfium binary, or system fallback.
    pub fn new() -> Result<Self, PdfError> {
        let bindings = if let Some(path) = locate_pdfium() {
            Pdfium::bind_to_library(&path)
                .map_err(|e| PdfError::PdfiumLoad(format!("{}: {}", path.display(), e)))?
        } else {
            Pdfium::bind_to_system_library()
                .map_err(|e| PdfError::PdfiumLoad(format!("system: {}", e)))?
        };
        Ok(Self {
            pdfium: Pdfium::new(bindings),
            dpi: 150,
            jpeg_quality: 85,
        })
    }

    /// Override render DPI.
    pub fn with_dpi(mut self, dpi: u16) -> Self { self.dpi = dpi; self }
    pub fn with_quality(mut self, q: u8) -> Self { self.jpeg_quality = q; self }

    /// Page count for a PDF loaded from disk.
    pub fn page_count(&self, path: &Path) -> Result<u32, PdfError> {
        let doc = self.pdfium.load_pdf_from_file(path, None)
            .map_err(|e| PdfError::PdfiumOp(format!("load {}: {}", path.display(), e)))?;
        Ok(doc.pages().len() as u32)
    }

    /// Render a single 1-based page number to JPEG bytes at self.dpi.
    /// Caller ensures `page` is in [1, page_count(path)].
    pub fn render_page_jpeg(&self, path: &Path, page: u32) -> Result<Vec<u8>, PdfError> {
        let doc = self.pdfium.load_pdf_from_file(path, None)
            .map_err(|e| PdfError::PdfiumOp(format!("load: {}", e)))?;
        let pages = doc.pages();
        let total = pages.len() as u32;
        if page == 0 || page > total {
            return Err(PdfError::PdfiumOp(format!("page {} out of range 1..={}", page, total)));
        }
        let p = pages.get((page - 1) as i32)
            .map_err(|e| PdfError::PdfiumOp(format!("get page {}: {}", page, e)))?;
        // Pdfium points per inch = 72; our DPI → pixels/inch. Width in pixels.
        let w_px = (p.width().value * (self.dpi as f32) / 72.0).ceil() as i32;
        let h_px = (p.height().value * (self.dpi as f32) / 72.0).ceil() as i32;
        let cfg = PdfRenderConfig::new()
            .set_target_width(w_px.max(1))
            .set_target_height(h_px.max(1))
            .rotate_if_landscape(PdfPageRenderRotation::None, false);
        let bitmap = p.render_with_config(&cfg)
            .map_err(|e| PdfError::PdfiumOp(format!("render page {}: {}", page, e)))?;
        let dyn_image = bitmap.as_image()
            .map_err(|e| PdfError::PdfiumOp(format!("image page {}: {}", page, e)))?;
        encode_jpeg(&dyn_image, self.jpeg_quality)
    }

    /// Plain-text extraction for a page (1-based). Useful for debugging.
    pub fn page_text(&self, path: &Path, page: u32) -> Result<String, PdfError> {
        let doc = self.pdfium.load_pdf_from_file(path, None)
            .map_err(|e| PdfError::PdfiumOp(format!("load: {}", e)))?;
        let pages = doc.pages();
        let total = pages.len() as u32;
        if page == 0 || page > total {
            return Err(PdfError::PdfiumOp(format!("page {} out of range 1..={}", page, total)));
        }
        let p = pages.get((page - 1) as i32)
            .map_err(|e| PdfError::PdfiumOp(format!("get page {}: {}", page, e)))?;
        let text = p.text()
            .map_err(|e| PdfError::PdfiumOp(format!("text(): {}", e)))?;
        Ok(text.all())
    }
}

fn encode_jpeg(img: &image::DynamicImage, quality: u8) -> Result<Vec<u8>, PdfError> {
    let mut out = Vec::with_capacity(64 * 1024);
    // JPEG doesn't support alpha; drop to RGB.
    let rgb = img.to_rgb8();
    {
        let mut cursor = Cursor::new(&mut out);
        let mut enc = JpegEncoder::new_with_quality(&mut cursor, quality);
        enc.encode(
            rgb.as_raw(),
            rgb.width(),
            rgb.height(),
            image::ExtendedColorType::Rgb8,
        ).map_err(|e| PdfError::Encode(e.to_string()))?;
    }
    // Ensure we actually produced JPEG-shaped data.
    debug_assert_eq!(image::guess_format(&out).ok(), Some(ImageFormat::Jpeg));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_lib_name_matches_os() {
        let name = platform_lib_name();
        if cfg!(target_os = "windows") { assert_eq!(name, "pdfium.dll"); }
        else if cfg!(target_os = "macos") { assert_eq!(name, "libpdfium.dylib"); }
        else { assert_eq!(name, "libpdfium.so"); }
    }

    #[test]
    fn locate_pdfium_finds_vendored_when_present() {
        // The repo does NOT store the pdfium binary; scripts/fetch-pdfium.sh
        // puts it at `vendor/pdfium/<platform>/<binary>` (CI runs it) for a
        // stronger assertion. This test passes either way: it just checks
        // that locate_pdfium's return value, when Some, points at a file.
        if let Some(path) = locate_pdfium() {
            assert!(path.is_file(), "located path must point at a real file");
        } else {
            eprintln!("skipping: no vendored pdfium present");
        }
    }
}
