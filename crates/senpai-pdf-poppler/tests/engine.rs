//! Backend tests for the poppler engine.
//!
//! These build a minimal, valid PDF in memory rather than committing a
//! binary fixture: the bytes under test stay readable and reviewable, and
//! CI needs nothing but the workspace itself to run them.

use senpai_core::engine::{EngineError, PdfEngine, PdfEngineOpen};
use senpai_core::model::PageHandle;
use senpai_pdf_poppler::PopplerEngine;

const PAGE_W: f64 = 200.0;
const PAGE_H: f64 = 100.0;

/// Assembles a one-page PDF with a real xref table (byte offsets computed
/// as the objects are emitted, so the file is structurally valid rather
/// than relying on poppler's damaged-file reconstruction path).
///
/// `title_utf16be_hex` is the document title as a hex-encoded UTF-16BE
/// string *including* the BOM — the only way to put non-ASCII text in a
/// PDF info dictionary, and the shape that matters for the text-decoding
/// regression test below.
fn minimal_pdf(title_utf16be_hex: &str, page_text: &str) -> Vec<u8> {
    let content = format!(
        "BT /F1 12 Tf 10 40 Td ({}) Tj ET\n",
        page_text
            .replace('\\', r"\\")
            .replace('(', r"\(")
            .replace(')', r"\)")
    );

    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {PAGE_W} {PAGE_H}] \
             /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>"
        ),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        format!(
            "<< /Length {} >>\nstream\n{content}endstream",
            content.len()
        ),
        format!("<< /Title <{title_utf16be_hex}> /Author (SENPAI test suite) >>"),
    ];

    let mut out: Vec<u8> = b"%PDF-1.7\n".to_vec();
    let mut offsets = Vec::with_capacity(objects.len());
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", i + 1).as_bytes());
    }

    let xref_at = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R /Info {} 0 R >>\nstartxref\n{xref_at}\n%%EOF\n",
            objects.len() + 1,
            objects.len()
        )
        .as_bytes(),
    );
    out
}

/// "Résumé ✓" — one Latin-1-representable accented char, one that isn't.
const TITLE_NON_ASCII: &str = "FEFF005200E900730075006D00E900202713";

fn open_test_doc() -> PopplerEngine {
    let bytes = minimal_pdf(TITLE_NON_ASCII, "hello senpai");
    PopplerEngine::open(&bytes).expect("minimal pdf should open")
}

#[test]
fn opens_and_reports_metadata() {
    let engine = open_test_doc();
    let meta = engine.metadata();
    assert_eq!(meta.page_count, 1);
    assert_eq!(meta.author.as_deref(), Some("SENPAI test suite"));
}

/// Regression test: the shim used to hand poppler's `to_latin1()` bytes
/// straight to `rust::String`, whose constructor throws on invalid UTF-8.
/// Under `panic = "abort"` that throw killed the process outright, so a
/// single accented character in a title, author, or page of text was
/// enough to take the reader down. Must now decode, not abort.
#[test]
fn non_ascii_text_decodes_instead_of_aborting() {
    let engine = open_test_doc();
    let title = engine.metadata().title.expect("title should be present");
    assert!(
        title.contains('é'),
        "title did not round-trip as UTF-8: {title:?}"
    );
    assert!(title.contains('✓'), "non-Latin-1 character lost: {title:?}");
}

#[test]
fn reports_declared_page_size() {
    let engine = open_test_doc();
    let size = engine.page_size(PageHandle(0)).expect("page 0 size");
    assert_eq!(size.width_pt, PAGE_W);
    assert_eq!(size.height_pt, PAGE_H);
}

#[test]
fn renders_page_at_scale() {
    let engine = open_test_doc();
    for (scale, expect_w, expect_h) in [(1.0, 200, 100), (2.0, 400, 200)] {
        let page = engine
            .render_page(PageHandle(0), scale)
            .unwrap_or_else(|e| panic!("render at {scale}x failed: {e}"));
        assert_eq!((page.width_px, page.height_px), (expect_w, expect_h));
        assert_eq!(
            page.pixels.len(),
            page.expected_len(),
            "RGBA buffer length must match width*height*4"
        );
    }
}

#[test]
fn extracts_page_text() {
    let engine = open_test_doc();
    let text = engine.page_text(PageHandle(0)).expect("page text");
    assert!(text.contains("hello senpai"), "extracted text was {text:?}");
}

#[test]
fn out_of_range_page_is_an_error_not_a_crash() {
    let engine = open_test_doc();
    for call in [
        engine.page_size(PageHandle(1)).map(|_| ()),
        engine.render_page(PageHandle(1), 1.0).map(|_| ()),
        engine.page_text(PageHandle(1)).map(|_| ()),
    ] {
        assert!(matches!(call, Err(EngineError::InvalidPage(PageHandle(1)))));
    }
}

#[test]
fn garbage_bytes_are_an_error_not_a_crash() {
    let err = PopplerEngine::open(b"not a pdf at all, not even close");
    assert!(matches!(err, Err(EngineError::Open(_))));
}
