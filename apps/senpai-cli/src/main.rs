//! Phase 0 milestone demonstrator (see ROADMAP.md):
//! open a PDF via the poppler backend, mmap it, log page count + metadata.
//!
//! This is deliberately the *only* place in the workspace that imports
//! `senpai-pdf-poppler` directly — it's the composition root that picks a
//! concrete `PdfEngine` backend. Everything below it only knows the trait.

use senpai_core::engine::{PdfEngine, PdfEngineOpen};
use senpai_core::file_loader::LoadedFile;
use senpai_core::model::PageHandle;
use senpai_pdf_poppler::PopplerEngine;
use std::env;
use std::process::ExitCode;
use std::time::Instant;

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: senpai <path-to-pdf>");
        return ExitCode::FAILURE;
    };

    let t0 = Instant::now();

    let file = match LoadedFile::open(&path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("failed to open {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let t_mmap = t0.elapsed();

    let engine = match PopplerEngine::open(file.as_bytes()) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("failed to parse {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let t_open = t0.elapsed();

    let meta = engine.metadata();
    println!("file:        {}", file.path().display());
    println!("size:        {} bytes", file.len());
    println!("pages:       {}", meta.page_count);
    println!("title:       {}", meta.title.as_deref().unwrap_or("(none)"));
    println!("author:      {}", meta.author.as_deref().unwrap_or("(none)"));
    println!("mmap time:   {t_mmap:?}");
    println!("open time:   {t_open:?} (total, including mmap)");

    if meta.page_count > 0 {
        match engine.page_size(PageHandle(0)) {
            Ok(size) => println!(
                "page 1 size: {:.1} x {:.1} pt",
                size.width_pt, size.height_pt
            ),
            Err(e) => eprintln!("warning: could not read page 1 size: {e}"),
        }
    }

    ExitCode::SUCCESS
}
