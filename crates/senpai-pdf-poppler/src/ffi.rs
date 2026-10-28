//! The `cxx` bridge to poppler-cpp, via `shim.h`/`shim.cpp`.
//!
//! This file — plus `shim.h`/`shim.cpp` — is the *only* place in the whole
//! workspace allowed to know poppler exists. `senpai-core` and everything
//! above it only sees `crate::PopplerEngine`, which implements the
//! backend-agnostic `PdfEngine` trait.

// `ffi::ffi` reads oddly, but the inner module name is the one cxx's own
// documentation and examples use, and it's what every cxx codebase looks
// like -- worth more here than satisfying the lint.
#[allow(clippy::module_inception)]
#[cxx::bridge(namespace = "senpai_poppler")]
pub mod ffi {
    struct PageSize {
        width_pt: f64,
        height_pt: f64,
    }

    unsafe extern "C++" {
        include!("shim.h");

        type PopplerDocument;
        type RenderedImage;

        fn page_count(self: &PopplerDocument) -> i32;
        fn get_title(self: &PopplerDocument) -> String;
        fn get_author(self: &PopplerDocument) -> String;
        fn page_size(self: &PopplerDocument, index: i32) -> PageSize;
        fn render_page(self: &PopplerDocument, index: i32, scale: f64) -> UniquePtr<RenderedImage>;
        fn page_text(self: &PopplerDocument, index: i32) -> String;

        fn width(self: &RenderedImage) -> u32;
        fn height(self: &RenderedImage) -> u32;
        unsafe fn data(self: &RenderedImage) -> *const u8;
        fn len(self: &RenderedImage) -> usize;

        unsafe fn open_document(data: *const u8, len: usize) -> UniquePtr<PopplerDocument>;
    }
}
