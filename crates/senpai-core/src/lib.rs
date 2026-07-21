//! senpai-core
//!
//! The engine-agnostic core of SENPAI: file loading, the document model,
//! and the `PdfEngine` trait that every backend (poppler today, a native
//! engine long-term) must implement.
//!
//! Nothing outside this crate defines what a "document" or "page" is.
//! Nothing outside `senpai-pdf-poppler` is allowed to call poppler directly.

pub mod engine;
pub mod file_loader;
pub mod model;

pub use engine::{EngineError, PdfEngine};
pub use file_loader::LoadedFile;
pub use model::{DocumentMetadata, PageHandle, PageSize, RenderedPage};
