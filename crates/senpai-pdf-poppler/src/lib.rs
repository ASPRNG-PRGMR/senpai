//! senpai-pdf-poppler
//!
//! The poppler-backed implementation of `senpai_core::PdfEngine`. This is
//! the *only* crate in the workspace that links poppler; everything else
//! depends on the trait, not on this crate directly (except at the app's
//! composition root, where a concrete backend has to be chosen).

mod ffi;

use cxx::UniquePtr;
use senpai_core::engine::{EngineError, PdfEngine, PdfEngineOpen};
use senpai_core::model::{DocumentMetadata, PageHandle, PageSize, RenderedPage};

pub struct PopplerEngine {
    // Keeps the source bytes alive for as long as the engine lives: poppler
    // reads directly out of this buffer (`load_from_raw_data`) without
    // copying it, so the buffer must outlive the document.
    _bytes_owner: Vec<u8>,
    doc: UniquePtr<ffi::ffi::PopplerDocument>,
}

// SAFETY: PopplerDocument is only ever touched through &self methods that
// poppler-cpp documents as safe for read-only access (rendering/text
// extraction are the only operations we expose). We do not expose any
// mutating operations across the bridge.
unsafe impl Send for PopplerEngine {}
unsafe impl Sync for PopplerEngine {}

impl PdfEngineOpen for PopplerEngine {
    fn open(bytes: &[u8]) -> Result<Self, EngineError> {
        // Own a copy so the buffer's address is stable for poppler's
        // lifetime regardless of what the caller does with their slice
        // afterwards. (senpai-core's mmap already avoids the *first* copy
        // from disk; this is the one deliberate copy at the engine
        // boundary, kept because poppler-cpp offers no borrowed-lifetime
        // API. Revisit if this shows up in the render-hot-path benchmark.)
        let owned = bytes.to_vec();
        // SAFETY: `owned` is a valid, live buffer of `owned.len()` bytes
        // for the duration of this call; poppler copies out of it
        // synchronously inside `open_document`.
        let doc = unsafe { ffi::ffi::open_document(owned.as_ptr(), owned.len()) };
        if doc.is_null() {
            return Err(EngineError::Open(
                "poppler failed to open document (corrupt, encrypted, or unsupported)".into(),
            ));
        }
        Ok(Self {
            _bytes_owner: owned,
            doc,
        })
    }
}

impl PdfEngine for PopplerEngine {
    fn metadata(&self) -> DocumentMetadata {
        let title = self.doc.get_title();
        let author = self.doc.get_author();
        DocumentMetadata {
            title: if title.is_empty() { None } else { Some(title) },
            author: if author.is_empty() {
                None
            } else {
                Some(author)
            },
            page_count: self.doc.page_count().max(0) as usize,
        }
    }

    fn page_size(&self, page: PageHandle) -> Result<PageSize, EngineError> {
        self.check_page(page)?;
        let s = self.doc.page_size(page.0 as i32);
        Ok(PageSize {
            width_pt: s.width_pt,
            height_pt: s.height_pt,
        })
    }

    fn render_page(&self, page: PageHandle, scale: f64) -> Result<RenderedPage, EngineError> {
        self.check_page(page)?;
        let img = self.doc.render_page(page.0 as i32, scale);
        let width = img.width();
        let height = img.height();
        if width == 0 || height == 0 {
            return Err(EngineError::Render(format!(
                "poppler returned an empty render for page {}",
                page.0
            )));
        }
        let len = img.len();
        // SAFETY: `img` outlives this block, and `data()` returns a pointer
        // valid for `len` bytes backed by `img`'s internal buffer for as
        // long as `img` itself is alive — which covers this entire copy.
        let ptr = unsafe { img.data() };
        let pixels = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
        Ok(RenderedPage {
            width_px: width,
            height_px: height,
            pixels,
        })
    }

    fn page_text(&self, page: PageHandle) -> Result<String, EngineError> {
        self.check_page(page)?;
        Ok(self.doc.page_text(page.0 as i32))
    }
}

impl PopplerEngine {
    fn check_page(&self, page: PageHandle) -> Result<(), EngineError> {
        let count = self.doc.page_count().max(0) as usize;
        if page.0 >= count {
            return Err(EngineError::InvalidPage(page));
        }
        Ok(())
    }
}
