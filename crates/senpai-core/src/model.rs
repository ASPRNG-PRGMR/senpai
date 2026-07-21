//! Engine-agnostic document model.
//!
//! These types are what `senpai-render` and `senpai-ui` are allowed to see.
//! They carry no poppler (or any backend-specific) types across the
//! boundary — that's the whole point of the `PdfEngine` trait.

/// Opaque handle to a page within an open document. Backends decide what
/// this maps to internally (an index, a resolved object, etc).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PageHandle(pub usize);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PageSize {
    /// Page width in points (1/72 inch), matching the PDF coordinate space.
    pub width_pt: f64,
    /// Page height in points (1/72 inch).
    pub height_pt: f64,
}

#[derive(Debug, Clone, Default)]
pub struct DocumentMetadata {
    pub title: Option<String>,
    pub author: Option<String>,
    pub page_count: usize,
}

/// A rasterized page (or tile of a page), handed back by an engine's
/// render call. Pixel format is fixed at RGBA8 for now to keep the
/// renderer simple; revisit if a GPU-backed path wants something else.
pub struct RenderedPage {
    pub width_px: u32,
    pub height_px: u32,
    /// RGBA8, row-major, no padding between rows.
    pub pixels: Vec<u8>,
}

impl RenderedPage {
    pub fn expected_len(&self) -> usize {
        self.width_px as usize * self.height_px as usize * 4
    }
}
