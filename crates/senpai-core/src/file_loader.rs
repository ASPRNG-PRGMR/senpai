//! Memory-mapped file access.
//!
//! Per the roadmap's performance budget: the hot path never reads a whole
//! document into memory up front. `LoadedFile` hands out a read-only mmap
//! view; engines are expected to resolve only the bytes they actually need
//! for the page(s) currently being decoded.

use memmap2::Mmap;
use std::fs::File;
use std::io;
use std::path::Path;

pub struct LoadedFile {
    mmap: Mmap,
    path: std::path::PathBuf,
}

impl LoadedFile {
    /// Opens `path` and memory-maps it. Does not parse or validate PDF
    /// structure — that is the engine's job, and only for the parts of the
    /// document actually needed.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path)?;
        // Safety: the mapped file is treated as read-only for the lifetime
        // of `LoadedFile`. External mutation of the underlying file while
        // mapped is a known, accepted risk of mmap-based readers (same
        // tradeoff poppler/Evince make).
        let mmap = unsafe { Mmap::map(&file)? };
        Ok(Self { mmap, path })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.mmap
    }

    pub fn len(&self) -> usize {
        self.mmap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.mmap.is_empty()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}
