//! Phase 0/1/2 demonstrator and benchmark harness (see ROADMAP.md).
//!
//! Three subcommands, all against the `PdfEngine` trait:
//! - `info` — mmap + open a document, print metadata and page geometry
//! - `render` — render one page out to a `.ppm` as a visual correctness
//!   check (verified pixel-identical to Evince's output)
//! - `bench` — the repeatable version of what used to be ad-hoc timing
//!   inline in `info`: fixed page sampling, fixed iteration count,
//!   percentile stats, optional JSON for diffing runs over time
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
use std::time::{Duration, Instant};

const USAGE: &str = "\
usage:
  senpai info   <pdf>
  senpai render <pdf> [out.ppm] [scale]
  senpai bench  <pdf> [--scale S] [--pages N] [--iters N] [--json]
  senpai        <pdf>                     (same as `info`)

bench options:
  --scale S    render scale; 1.0 = 72 DPI, 2.0 = 144 DPI   (default 2.0)
  --pages N    how many pages to sample, spread evenly      (default 20)
  --iters N    render each sampled page N times             (default 3)
  --json       emit machine-readable results instead of a table
";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let Some(first) = args.first() else {
        eprint!("{USAGE}");
        return ExitCode::FAILURE;
    };

    match first.as_str() {
        "info" => cmd_info(&args[1..]),
        "render" => cmd_render(&args[1..]),
        "bench" => cmd_bench(&args[1..]),
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        // No subcommand: treat the first argument as a path, so the old
        // `senpai <pdf>` invocation keeps working.
        _ => cmd_info(&args),
    }
}

/// mmap + open + metadata, timed. Shared by every subcommand because the
/// open path is cheap enough (~4ms on the reference document) that timing
/// it on every run costs nothing and catches regressions for free.
struct Opened {
    file: LoadedFile,
    engine: PopplerEngine,
    t_mmap: Duration,
    t_open: Duration,
}

fn open_timed(path: &str) -> Result<Opened, String> {
    let t0 = Instant::now();
    let file = LoadedFile::open(path).map_err(|e| format!("failed to open {path}: {e}"))?;
    let t_mmap = t0.elapsed();
    let engine =
        PopplerEngine::open(file.as_bytes()).map_err(|e| format!("failed to parse {path}: {e}"))?;
    let t_open = t0.elapsed();
    Ok(Opened {
        file,
        engine,
        t_mmap,
        t_open,
    })
}

fn cmd_info(args: &[String]) -> ExitCode {
    let Some(path) = args.first() else {
        eprint!("{USAGE}");
        return ExitCode::FAILURE;
    };

    let opened = match open_timed(path) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let meta = opened.engine.metadata();

    println!("file:        {}", opened.file.path().display());
    println!("size:        {} bytes", opened.file.len());
    println!("pages:       {}", meta.page_count);
    println!("title:       {}", meta.title.as_deref().unwrap_or("(none)"));
    println!(
        "author:      {}",
        meta.author.as_deref().unwrap_or("(none)")
    );
    println!("mmap time:   {:?}", opened.t_mmap);
    println!("open time:   {:?} (total, including mmap)", opened.t_open);

    if meta.page_count == 0 {
        eprintln!("document has no pages");
        return ExitCode::SUCCESS;
    }
    match opened.engine.page_size(PageHandle(0)) {
        Ok(size) => println!(
            "page 1 size: {:.1} x {:.1} pt",
            size.width_pt, size.height_pt
        ),
        Err(e) => eprintln!("warning: could not read page 1 size: {e}"),
    }
    ExitCode::SUCCESS
}

fn cmd_render(args: &[String]) -> ExitCode {
    let Some(path) = args.first() else {
        eprint!("{USAGE}");
        return ExitCode::FAILURE;
    };
    let out_path = args
        .get(1)
        .cloned()
        .unwrap_or_else(|| default_output_path(path));
    // 2.0 = 144 DPI, sharp enough to actually judge quality.
    let scale: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(2.0);

    let opened = match open_timed(path) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    if opened.engine.metadata().page_count == 0 {
        eprintln!("document has no pages, nothing to render");
        return ExitCode::FAILURE;
    }

    let t = Instant::now();
    let rendered = match opened.engine.render_page(PageHandle(0), scale) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("render failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    let elapsed = t.elapsed();

    if let Err(e) = write_ppm(
        &out_path,
        rendered.width_px,
        rendered.height_px,
        &rendered.pixels,
    ) {
        eprintln!("failed to write {out_path}: {e}");
        return ExitCode::FAILURE;
    }
    println!(
        "wrote {out_path}: {}x{} at scale {scale} in {elapsed:?}",
        rendered.width_px, rendered.height_px
    );
    ExitCode::SUCCESS
}

struct BenchOpts {
    path: String,
    scale: f64,
    pages: usize,
    iters: usize,
    json: bool,
}

fn parse_bench_opts(args: &[String]) -> Result<BenchOpts, String> {
    let mut path = None;
    let mut scale = 2.0;
    let mut pages = 20;
    let mut iters = 3;
    let mut json = false;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let mut value = |name: &str| -> Result<String, String> {
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "--scale" => scale = value("--scale")?.parse().map_err(|_| "bad --scale")?,
            "--pages" => pages = value("--pages")?.parse().map_err(|_| "bad --pages")?,
            "--iters" => iters = value("--iters")?.parse().map_err(|_| "bad --iters")?,
            "--json" => json = true,
            other if other.starts_with('-') => return Err(format!("unknown option {other}")),
            other => path = Some(other.to_string()),
        }
        i += 1;
    }

    Ok(BenchOpts {
        path: path.ok_or("bench needs a path to a pdf")?,
        scale,
        pages: pages.max(1),
        iters: iters.max(1),
        json,
    })
}

fn cmd_bench(args: &[String]) -> ExitCode {
    let opts = match parse_bench_opts(args) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}\n");
            eprint!("{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    let opened = match open_timed(&opts.path) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let meta = opened.engine.metadata();
    if meta.page_count == 0 {
        eprintln!("document has no pages, nothing to benchmark");
        return ExitCode::FAILURE;
    }

    // Time-to-first-page: the number the performance budget actually
    // cares about -- process-visible latency from "user asked" to
    // "pixels exist for the first page."
    let t = Instant::now();
    let first = opened.engine.render_page(PageHandle(0), opts.scale);
    let t_first_page = opened.t_open + t.elapsed();
    if let Err(e) = &first {
        eprintln!("page 1 render failed: {e}");
        return ExitCode::FAILURE;
    }
    drop(first);

    // Deterministic, evenly spread page sampling, so two runs over the
    // same document compare like for like.
    let sample_count = opts.pages.min(meta.page_count);
    let step = (meta.page_count / sample_count).max(1);
    let sampled: Vec<usize> = (0..sample_count).map(|i| i * step).collect();

    let mut render_times = Vec::with_capacity(sampled.len() * opts.iters);
    let mut text_times = Vec::with_capacity(sampled.len());
    let mut largest_page_bytes = 0usize;
    let mut failures = 0usize;

    for &page in &sampled {
        for _ in 0..opts.iters {
            let t = Instant::now();
            match opened.engine.render_page(PageHandle(page), opts.scale) {
                Ok(r) => {
                    render_times.push(t.elapsed());
                    largest_page_bytes = largest_page_bytes.max(r.expected_len());
                }
                Err(e) => {
                    failures += 1;
                    eprintln!("page {page} render failed: {e}");
                }
            }
        }
        let t = Instant::now();
        if opened.engine.page_text(PageHandle(page)).is_ok() {
            text_times.push(t.elapsed());
        }
    }

    let render = Stats::from(&mut render_times);
    let text = Stats::from(&mut text_times);
    let rss = resident_set_bytes();

    if opts.json {
        print_bench_json(
            &opts,
            &opened,
            &meta.page_count,
            t_first_page,
            &render,
            &text,
            rss,
            largest_page_bytes,
            failures,
        );
    } else {
        println!("document:          {}", opened.file.path().display());
        println!("size:              {} bytes", opened.file.len());
        println!("pages:             {}", meta.page_count);
        println!(
            "scale:             {} ({:.0} DPI)",
            opts.scale,
            opts.scale * 72.0
        );
        println!(
            "sampled:           {} page(s), {} iteration(s) each",
            sampled.len(),
            opts.iters
        );
        println!();
        println!("mmap:              {:?}", opened.t_mmap);
        println!("open (w/ mmap):    {:?}", opened.t_open);
        println!(
            "time-to-first-page:{:>12?}  (open + page 1 render)",
            t_first_page
        );
        println!();
        match &render {
            Some(render) => println!("page render:       {render}"),
            None => println!("page render:       (no successful renders)"),
        }
        if let Some(text) = &text {
            println!("page text extract: {text}");
        }
        println!();
        println!(
            "peak page buffer:  {:.1} MiB (largest single RGBA page at this scale)",
            largest_page_bytes as f64 / (1024.0 * 1024.0)
        );
        match rss {
            Some(bytes) => println!(
                "resident memory:   {:.1} MiB",
                bytes as f64 / (1024.0 * 1024.0)
            ),
            None => println!("resident memory:   (unavailable on this platform)"),
        }
        if failures > 0 {
            println!("\n{failures} render(s) FAILED -- numbers above exclude them");
        }
    }

    if failures > 0 {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

#[allow(clippy::too_many_arguments)]
fn print_bench_json(
    opts: &BenchOpts,
    opened: &Opened,
    page_count: &usize,
    t_first_page: Duration,
    render: &Option<Stats>,
    text: &Option<Stats>,
    rss: Option<usize>,
    largest_page_bytes: usize,
    failures: usize,
) {
    // Hand-rolled rather than pulling in serde: the performance budget's
    // minimal-dependency rule applies to tooling too, and this is a dozen
    // flat fields.
    fn us(d: Duration) -> String {
        format!("{:.3}", d.as_secs_f64() * 1e6)
    }
    fn stats_json(s: &Option<Stats>) -> String {
        match s {
            Some(s) => format!(
                "{{\"n\":{},\"min_us\":{},\"p50_us\":{},\"p95_us\":{},\"max_us\":{},\"mean_us\":{}}}",
                s.n,
                us(s.min),
                us(s.p50),
                us(s.p95),
                us(s.max),
                us(s.mean)
            ),
            None => "null".to_string(),
        }
    }
    println!("{{");
    println!(
        "  \"path\": {:?},",
        opened.file.path().display().to_string()
    );
    println!("  \"file_bytes\": {},", opened.file.len());
    println!("  \"page_count\": {page_count},");
    println!("  \"scale\": {},", opts.scale);
    println!("  \"iters\": {},", opts.iters);
    println!("  \"mmap_us\": {},", us(opened.t_mmap));
    println!("  \"open_us\": {},", us(opened.t_open));
    println!("  \"time_to_first_page_us\": {},", us(t_first_page));
    println!("  \"render\": {},", stats_json(render));
    println!("  \"page_text\": {},", stats_json(text));
    println!("  \"largest_page_bytes\": {largest_page_bytes},");
    match rss {
        Some(b) => println!("  \"resident_bytes\": {b},"),
        None => println!("  \"resident_bytes\": null,"),
    }
    println!("  \"failures\": {failures}");
    println!("}}");
}

struct Stats {
    n: usize,
    min: Duration,
    p50: Duration,
    p95: Duration,
    max: Duration,
    mean: Duration,
}

impl Stats {
    /// Sorts in place and summarizes. Percentiles rather than just
    /// min/avg/max because scroll latency is a tail-latency problem: the
    /// p95 frame is the one the user actually notices.
    fn from(samples: &mut [Duration]) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }
        samples.sort_unstable();
        let n = samples.len();
        let total: Duration = samples.iter().sum();
        Some(Self {
            n,
            min: samples[0],
            p50: samples[percentile_index(n, 50.0)],
            p95: samples[percentile_index(n, 95.0)],
            max: samples[n - 1],
            mean: total / n as u32,
        })
    }
}

fn percentile_index(n: usize, pct: f64) -> usize {
    let idx = (pct / 100.0 * n as f64).ceil() as usize;
    idx.saturating_sub(1).min(n - 1)
}

impl std::fmt::Display for Stats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "n={} min={:?} p50={:?} p95={:?} max={:?} mean={:?}",
            self.n, self.min, self.p50, self.p95, self.max, self.mean
        )
    }
}

/// Resident set size, for the "memory footprint" leg of the benchmark.
/// Linux-only via /proc; deliberately not worth a crate dependency, and
/// deliberately not fatal when unavailable.
#[cfg(target_os = "linux")]
fn resident_set_bytes() -> Option<usize> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let resident_pages: usize = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(resident_pages * 4096)
}

#[cfg(not(target_os = "linux"))]
fn resident_set_bytes() -> Option<usize> {
    None
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
/// once tiling lands.
fn write_ppm(path: &str, width: u32, height: u32, rgba: &[u8]) -> io::Result<()> {
    let file = File::create(path)?;
    let mut w = BufWriter::new(file);
    write!(w, "P6\n{width} {height}\n255\n")?;
    for chunk in rgba.as_chunks::<4>().0 {
        w.write_all(&chunk[0..3])?; // R, G, B — skip A
    }
    w.flush()
}
