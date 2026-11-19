//! Integration test exercising pdfium against a real PDF fixture.
//!
//! Skipped gracefully when:
//!   - pdfium is not locatable, OR
//!   - the fixture file `tests/fixtures/sample.pdf` is absent.
//!
//! NOTE: the repo does NOT ship a sample fixture (the previous one came from
//! a non-redistributable source). Drop any small public-domain PDF at
//! `tests/fixtures/sample.pdf` to opt into these checks locally; CI does
//! not run them. Test is preserved so contributors with their own fixture
//! can validate end-to-end pdfium integration without touching tool source.

use std::path::PathBuf;

use combycode_dpe_tool_doc_converter::pdf::{locate_pdfium, PdfRenderer};

fn sample_pdf() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample.pdf")
}

fn skip_unless_ready() -> bool {
    if locate_pdfium().is_none() {
        eprintln!("skipping: pdfium not found");
        return true;
    }
    if !sample_pdf().exists() {
        eprintln!("skipping: tests/fixtures/sample.pdf not present");
        return true;
    }
    false
}

#[test]
fn page_count_on_real_pdf() {
    if skip_unless_ready() { return; }
    let r = PdfRenderer::new().expect("pdfium init");
    let n = r.page_count(&sample_pdf()).expect("page_count");
    assert!(n >= 1, "expected at least one page, got {n}");
}

#[test]
fn render_first_page_jpeg() {
    if skip_unless_ready() { return; }
    let r = PdfRenderer::new().expect("pdfium init").with_dpi(120).with_quality(80);
    let bytes = r.render_page_jpeg(&sample_pdf(), 1).expect("render");
    // JPEG magic: FF D8 FF
    assert!(bytes.len() > 1000, "jpeg too small: {}", bytes.len());
    assert_eq!(&bytes[..3], &[0xFF, 0xD8, 0xFF], "not a JPEG header");
}

#[test]
fn render_page_out_of_range_errors() {
    if skip_unless_ready() { return; }
    let r = PdfRenderer::new().unwrap();
    let err = r.render_page_jpeg(&sample_pdf(), 9999).unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("out of range"), "unexpected err: {msg}");
}

#[test]
fn extract_page_text_nonempty() {
    if skip_unless_ready() { return; }
    let r = PdfRenderer::new().unwrap();
    let text = r.page_text(&sample_pdf(), 1).expect("page_text");
    assert!(text.len() > 50, "expected readable text, got {} chars", text.len());
}
