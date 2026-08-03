//! Phase 0/1 demonstrator (see ROADMAP.md):
//! open a PDF via the poppler backend, mmap it, log page count + metadata,
//! and render page 1 out to a .ppm file as a visual correctness check —
//! the last step before any windowing/UI code gets written.
//!
//! This is deliberately the *only* place in the workspace that imports
//! `senpai-pdf-poppler` directly — it's the composition root that picks a
//! concrete `PdfEngine` backend. Everything below it only knows the trait.

use senpai_core::engine::{PdfEngine, PdfEngineOpen};
use senpai_core::file_loader::LoadedFile;
use senpai_core::model::PageHandle;
use senpai_pdf_poppler::PopplerEngine;
use std::env;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: senpai <path-to-pdf> [output.ppm] [scale]");
        return ExitCode::FAILURE;
    };
    let out_path = args
        .next()
        .unwrap_or_else(|| default_output_path(&path));
    let scale: f64 = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2.0); // 2.0 = 144 DPI, sharp enough to actually judge quality

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

    if meta.page_count == 0 {
        eprintln!("document has no pages, nothing to render");
        return ExitCode::SUCCESS;
    }

    match engine.page_size(PageHandle(0)) {
        Ok(size) => println!("page 1 size: {:.1} x {:.1} pt", size.width_pt, size.height_pt),
        Err(e) => eprintln!("warning: could not read page 1 size: {e}"),
    }

    let sample_count = meta.page_count.min(20); // sample up to 20 pages, spread across the doc
    let step = (meta.page_count / sample_count).max(1);

    let mut times = Vec::new();
    for i in 0..sample_count {
        let page_idx = i * step;
        let t = std::time::Instant::now();

        match engine.render_page(PageHandle(page_idx), 2.0) {
            Ok(r) => {
                let elapsed = t.elapsed();
                println!(
                    "page {page_idx:>4}: {:>5}x{:<5} in {elapsed:?}",
                    r.width_px,
                    r.height_px
                );
                times.push(elapsed);
            }
            Err(e) => eprintln!("page {page_idx} failed: {e}"),
        }
    }

    if !times.is_empty() {
        let total: std::time::Duration = times.iter().sum();
        let avg = total / times.len() as u32;
        let max = times.iter().max().unwrap();
        let min = times.iter().min().unwrap();

        println!(
            "\nrender stats over {} pages: min={:?} avg={:?} max={:?}",
            times.len(),
            min,
            avg,
            max
        );
    }
    ExitCode::SUCCESS
}

    

fn default_output_path(input: &str) -> String {
    let stem = Path::new(input)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("page1");
    format!("{stem}_page1.ppm")
}

/// Writes a binary PPM (P6): a dead-simple, uncompressed image format that
/// needs zero extra dependencies to produce, and that most image viewers
/// (or `convert`/ImageMagick, or Python's Pillow) can open directly.
///
/// PPM has no alpha channel, so we drop it here — fine for a correctness
/// check; SENPAI's own renderer will handle alpha/compositing properly
/// once tiling/caching (Phase 1/2) land.
fn write_ppm(path: &str, width: u32, height: u32, rgba: &[u8]) -> io::Result<()> {
    let file = File::create(path)?;
    let mut w = BufWriter::new(file);
    write!(w, "P6\n{width} {height}\n255\n")?;
    for chunk in rgba.chunks_exact(4) {
        w.write_all(&chunk[0..3])?; // R, G, B — skip A
    }
    w.flush()
}
