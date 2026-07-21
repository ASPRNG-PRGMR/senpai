//! senpai-render
//!
//! Tile-based rendering, page caching, and (later) GPU compositing sit
//! here, layered on top of any `PdfEngine`. This crate is intentionally
//! near-empty right now — Phase 0's job is proving the `PdfEngine` +
//! poppler wiring works end to end (see `senpai-cli`). Real tiling and
//! caching land in Phase 1/2 per ROADMAP.md.

use senpai_core::model::PageHandle;

/// A single tile of a rendered page. Real tiling (splitting a page into
/// fixed-size chunks for partial/background decode) is Phase 2 work; for
/// now a "tile" is just the whole page, so the rest of the pipeline
/// (cache, UI) can be built against a stable shape early.
pub struct Tile {
    pub page: PageHandle,
    pub scale: f64,
}
