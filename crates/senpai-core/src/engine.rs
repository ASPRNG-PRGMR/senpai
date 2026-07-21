//! The `PdfEngine` trait.
//!
//! This is the single boundary between "how we read a PDF today" (poppler)
//! and "how we might read one in the future" (a native SENPAI engine).
//!
//! Rule from the roadmap: no code outside a `senpai-pdf-*` crate may depend
//! on a specific backend's types. `senpai-core`, `senpai-render`, and
//! `senpai-ui` only ever talk to this trait.

use crate::model::{DocumentMetadata, PageHandle, PageSize, RenderedPage};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("failed to open document: {0}")]
    Open(String),
    #[error("invalid page handle: {0:?}")]
    InvalidPage(PageHandle),
    #[error("render failed: {0}")]
    Render(String),
    #[error("backend error: {0}")]
    Backend(String),
}

/// A document opened by some backend engine.
///
/// Implementations own whatever backend-specific state they need (a
/// poppler document pointer today; a native parsed object graph, someday).
/// Nothing about that state is visible outside the implementing crate.
pub trait PdfEngine {
    /// Cheap: page count and top-level metadata only. Must not eagerly
    /// resolve or decode page content per the performance budget.
    fn metadata(&self) -> DocumentMetadata;

    /// Page dimensions, without rendering it. Used for layout before any
    /// pixels are needed.
    fn page_size(&self, page: PageHandle) -> Result<PageSize, EngineError>;

    /// Renders a full page at the given scale (1.0 = 72 DPI, matching PDF
    /// points 1:1; callers scale up for higher DPI displays).
    ///
    /// Tile-based/partial rendering is a `senpai-render` concern layered
    /// on top of this — the engine only needs to support "render this page
    /// at this scale," not know about tiling or caching.
    fn render_page(&self, page: PageHandle, scale: f64) -> Result<RenderedPage, EngineError>;

    /// Plain-text extraction for a page, needed later by search indexing.
    /// Returns an empty string rather than erroring for pages with no
    /// extractable text (e.g. pure image scans without OCR).
    fn page_text(&self, page: PageHandle) -> Result<String, EngineError>;
}

/// Opens a document and returns a boxed engine. Each backend crate
/// (`senpai-pdf-poppler`, and later `senpai-engine-native`) provides its
/// own free function with this shape; there is deliberately no factory/
/// registry here yet — callers pick the backend explicitly until there's
/// a second one worth choosing between at runtime.
pub trait PdfEngineOpen: Sized {
    fn open(bytes: &[u8]) -> Result<Self, EngineError>;
}
