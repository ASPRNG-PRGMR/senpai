# SENPAI

**S**treaming **E**ngine for **N**ative **P**DF **A**ccess & **I**ndexing

SENPAI is a fast, native, developer-oriented document reader — built to feel instant, even on huge files, and to stay out of your way.

It is **not** "another PDF reader." It's a systems-engineered document platform, starting with PDF, designed from day one for performance, low memory usage, and extensibility.

---

## Why SENPAI?

Most document readers get slower as files get bigger, ship bloated Electron shells, or bury simple reading behind unnecessary UI chrome. SENPAI takes the opposite approach:

- **Instant startup, no lag** — native rendering, no browser runtime
- **Handles huge documents smoothly** — memory-mapped file access, lazy object resolution, streaming parsing
- **Minimal, distraction-free UI** — no unnecessary animations, no visual clutter
- **Keyboard-first** — built for people who live in their tools, not their mouse
- **Fully usable with zero AI features enabled** — AI is an optional plugin, never a dependency
- **Open, extensible architecture** — a plugin system designed to grow without rewrites

## Core Philosophy

Performance is a design requirement, not an optimization pass. Every part of SENPAI — from the parser to the renderer to the plugin system — is built to be simple, fast, and maintainable rather than feature-heavy.

## Supported Formats

| Format | Status |
|---|---|
| PDF | First-class, highest priority |
| Markdown (.md) | Supported — reflowing text view, not paginated |
| DOCX | Planned |
| EPUB | Planned |
| Plain text | Planned |
| Excel | Under consideration |

## Key Features

This is the target feature set — what SENPAI is being built toward. For what actually works today, see [Status](#status) below.

- Native, tile-based rendering with background page decoding and render caching
- Fast full-document search with an on-disk search index
- Annotation support
- Session restore and workspace management
- Command palette and fully keyboard-driven navigation
- A plugin system for extending the reader — including optional AI features like summarization, Q&A, and note generation — without touching the core

## Built With

SENPAI's core is written in **Rust** for safety and performance in parsing, indexing, and plugin management, paired with **C++** for mature, battle-tested rendering and font-shaping libraries where it makes sense to reuse them rather than reinvent them.

The PDF engine itself is currently backed by **poppler**, the same proven engine behind Evince, wrapped behind an internal `PdfEngine` trait so the backend can be swapped for a native SENPAI engine down the line without touching the rest of the codebase.

## Project Structure

SENPAI is a Rust workspace of small, single-purpose crates. Each crate has one job; nothing outside `pdf-poppler` is allowed to know poppler exists.

```
senpai/
├── Cargo.toml                      # workspace root
├── fetch-mingw64-sysroot.sh        # resolves + downloads the mingw64
│                                   # poppler sysroot for cross-builds
├── package-windows.sh              # builds the Windows release zip and
│                                   # derives its DLL set from the exe
├── installer.nsi                   # NSIS installer script
├── .github/workflows/ci.yml        # fmt, clippy, tests, cross-build
│
├── crates/
│   ├── senpai-core/                # engine-agnostic document model, mmap
│   │   └── src/                    # file loading, the PdfEngine trait
│   │       ├── lib.rs
│   │       ├── file_loader.rs
│   │       ├── model.rs
│   │       └── engine.rs
│   │
│   ├── senpai-pdf-poppler/         # the poppler backend; the only crate
│   │   ├── build.rs                # that links poppler
│   │   ├── src/
│   │   │   ├── lib.rs              # Rust: implements PdfEngine
│   │   │   ├── ffi.rs              # Rust: the cxx bridge definition
│   │   │   ├── shim.h              # C++: RAII wrappers over poppler-cpp
│   │   │   └── shim.cpp
│   │   └── tests/
│   │       └── engine.rs           # against an in-memory minimal PDF
│   │
│   └── senpai-render/              # tiling, caching (upcoming)
│       └── src/
│           └── lib.rs
│
└── apps/
    ├── senpai-cli/                 # the composition root that picks
    │   ├── src/                    # poppler as the concrete PdfEngine
    │   │   └── main.rs             # backend; info/render/bench commands
    │   └── tests/
    │       └── cli.rs
    │
    ├── senpai-ui/                  # the windowed reader (eframe/egui +
    │   ├── build.rs                # wgpu). No side panel, no menu bar —
    │   ├── senpai.ico              # one floating top bar, hidden until
    │   └── src/                    # hovered or opened via Alt+F/T/M/O
    │       └── main.rs
    │
    └── win-test/                   # throwaway poppler-free crate used to
        └── src/                    # prove the winit/wgpu/egui stack
            └── main.rs             # cross-builds and runs on Windows
```

Future backends (e.g. `crates/engine-native`) slot into this same structure without touching existing crates.

## Status

SENPAI is under active early development. What's working right now:

- **Opening documents**: memory-mapped file loading, poppler-backed parsing, metadata and page geometry — all running through the `PdfEngine` trait, verified against real PDFs.
- **Rendering**: pages render correctly through poppler and have been checked pixel-for-pixel against Evince's output.
- **A minimal windowed reader** (`senpai-ui`): opens a PDF into a native window with no side panel and no dedicated menu bar — just the current page, centered and scaled to fit. The only chrome is a single floating top bar (Files / Tools / Modes / Options) that stays hidden by default and reveals itself when you hover the top of the window or trigger a menu with `Alt+F` / `Alt+T` / `Alt+M` / `Alt+O`; auto-hide can be turned off entirely from Options. Modes includes a working Light/Night toggle, a view-mode switch, and rebindable page-navigation keys. Keyboard page navigation (arrow keys, `+`/`-`, space) works regardless of whether the bar is visible.
- **Opening files**: Files → Open, or `Ctrl+O`, brings up a real native file dialog, and the reader can be launched with no arguments at all — it lands on an empty state that prompts for a document. You can also drag a file onto the window, or pick one from the recent-documents list in the Files menu, which persists between runs and also decides where the dialog opens, so a browse starts where the last one ended rather than at the drive root. The dialog runs on its own thread rather than blocking the UI, and switching documents retires the previous render worker so each open releases the old document's render cache and engine handle instead of accumulating them.

  On Windows the dialog is the shell's own Common Item Dialog — the Explorer picker — and on Linux it is whatever chooser the desktop provides, through the XDG portal. Running the Windows build under Wine is the exception: Wine supplies its own, much plainer implementation of that dialog, and nothing on the application side can replace it.
- **Markdown**: `.md` files open in a reflowing reading column rather than as pages, which is why the Supported Formats table above qualifies it. Headings, emphasis, inline and fenced code, nested and ordered lists, task lists, block quotes, tables, rules and links are all rendered; zoom changes the type size and the text re-wraps, and the page-navigation keys move by a screenful. Deliberate limits: raw HTML is dropped rather than shown as source, images are replaced by a named placeholder, and hard line breaks are treated as spaces. Emphasis is carried by contrast rather than weight, because the bundled font has no bold face — for the same reason, characters outside its coverage (CJK, and some symbols such as `→`) render as a placeholder box.
- **Background rendering**: page rendering runs on a dedicated background thread instead of blocking the UI — page turns and zoom no longer freeze the window while poppler works. Rapid page-flipping doesn't queue up or flash through intermediate pages: only the most recently requested page/scale ever gets rendered, and an in-flight render that gets superseded is never shown when it lands (it is still kept in the cache — see below). A plain-text "rendering…" indicator (not a pill — the only pill in the app is the top bar) appears while a render is in flight.
- **Render cache + prefetching**: an LRU cache (30 entries) sits inside the background worker, keyed on `(page, scale)`. Revisiting a page you've already viewed at the same zoom is a cache hit instead of a re-render — measured at ~38µs versus ~86ms for a real render, roughly 2000x faster. Whenever the worker has no real request waiting, it opportunistically pre-renders neighbouring pages, so page-turns are frequently already cached by the time you ask for them. Confirmed in actual use, not just the isolated benchmark: page turns and revisits both feel meaningfully faster than the background-rendering-only version before this. Note the cache is bounded by *entry count*, not bytes: at the default 1.5x zoom a letter page is ~4MB, so the worker cache is ~125MB and the UI's matching texture cache roughly the same again, but both scale with the square of the zoom level. A byte-budgeted eviction policy is the obvious next step.
- **Tests**: 54 across the workspace, covering the poppler engine, the CLI, the Markdown parser, and the parts of the UI whose rules can be checked without a window.
- **Prefetch tuning**: measured against a real 982-page book (19.8MB, *Operating System Concepts*, 8th ed.) — render cost was remarkably consistent across the whole document (66–86.5ms, avg 77.8ms across 20 sampled pages spanning page 0 to 931), confirming no single pathologically slow page was responsible for an earlier page-skipping issue during fast page-turns. The real cause was that prefetch couldn't keep up with page-turns faster than ~78ms apart. A fixed depth-3-forward queue was tried first and dropped: the worker queued neighbours of whatever page it had just *finished*, which lags the reader's real position under fast navigation. What's in the code now is a dynamic ±1 window recomputed from the actual current page every frame, driven by the UI thread rather than the worker — never anchored to a stale position, and the queue is cleared on zoom so superseded work doesn't block live requests.
- **Stability fixes found on real hardware**: the wgpu backend is pinned to Vulkan at startup — wgpu's default backend selection picked a path that crashed inside AMD's OpenGL driver (`atio6axx.dll`, access violation) on a real Windows machine; confirmed root cause, not a guess. Two texture-lifetime bugs are also fixed: LRU-evicted textures are now dropped at the *start* of the next frame rather than mid-frame, and a render result never overwrites an existing texture for the same `(page, scale)` key — together these were the `"texture has been destroyed"` panic. Superseded renders are still cached rather than thrown away, so navigating back to a page you just left doesn't re-render it. Separately, the poppler shim no longer aborts the process on non-ASCII text: it was handing Latin-1 bytes to a `rust::String` constructor that throws on invalid UTF-8, and with `panic = "abort"` a single accented character in a title, author, or page of text killed the reader.
- **Benchmark harness**: `senpai bench <pdf>` gives repeatable numbers — fixed page sampling, fixed iteration count, and p50/p95 percentiles alongside min/avg/max, because scroll latency is a tail-latency problem and the p95 frame is the one you actually notice. `--json` makes runs comparable across commits. What it measured, over three very different documents, redirected the roadmap: **document size is nearly free, page content is what costs.** A 1278-page/31MB textbook opens in 8ms, while a single image-heavy page costs ~88ms to render against ~13ms for a text page — so the remaining performance work belongs in per-page tiling, not in document-level lazy loading.
- **Tests and CI**: GitHub Actions runs `cargo fmt --check`, `clippy -D warnings`, the test suite and a release build on Linux, plus the Windows cross-build and packaging end to end. The engine is tested against a minimal PDF assembled in memory — readable bytes with a real xref table rather than a committed binary blob — covering render dimensions, text extraction, non-ASCII decoding, and the paths that must return an error instead of crashing.

Not yet built: annotation tools (present in the UI but disabled), search, session state, and the plugin system. Continuous scrolling exists behind a **Continuous Scroll (experimental)** switch in Modes but is known broken and is not the default — paged navigation is. Rendering is still whole-page, not tiled.

**Platform support**: developed on Linux. The full reader — including PDF rendering, not just the windowing stack — now cross-compiles for Windows via `mingw-w64` and runs correctly under Wine with real documents. `poppler-cpp` and its full dependency chain (cairo/freetype/harfbuzz/glib/image codecs, etc.) are cross-built from prebuilt MSYS2 mingw64 packages, resolved and fetched by a script rather than assembled by hand, so this is reproducible rather than a one-off. `package-windows.sh` produces the shippable zip, and the DLL set in it is **derived, not hand-maintained**: the script reads `senpai.exe`'s PE import table with `objdump -p` and walks it transitively against the sysroot, bundling only names that resolve there — anything that doesn't (kernel32, user32, d3d12, vulkan-1) ships with Windows or the GPU driver. That derived set came out identical to the list below, which was originally established the slow way by deleting DLLs one at a time until the app broke:

```
libbrotlicommon.dll  libglib-2.0-0.dll  liblcms2-2.dll               libopenjp2-7.dll      libsharpyuv-0.dll    libzstd.dll
libbrotlidec.dll     libgraphite2.dll   libLerc.dll                  libpcre2-8-0.dll      libssh2-1.dll        nss3.dll
libbz2-1.dll         libharfbuzz-0.dll  liblzma-5.dll                libplc4.dll           libssl-3-x64.dll     nssutil3.dll
libcrypto-3-x64.dll  libiconv-2.dll     libnghttp2-14.dll            libplds4.dll          libstdc++-6.dll      smime3.dll
libcurl-4.dll        libidn2-0.dll      libnghttp3-9.dll             libpng16-16.dll       libtiff-6.dll        zlib1.dll
libdeflate.dll       libintl-8.dll      libngtcp2-16.dll             libpoppler-162.dll    libunistring-5.dll
libfreetype-6.dll    libjbig-0.dll      libngtcp2_crypto_ossl-0.dll  libpoppler-cpp-3.dll  libwebp-7.dll
libgcc_s_seh-1.dll   libjpeg-8.dll      libnspr4.dll                 libpsl-5.dll          libwinpthread-1.dll
```

Notably absent: `libcairo-2.dll` and `libfontconfig-1.dll` weren't touched during this test, suggesting poppler-cpp's rendering path here doesn't route through cairo and didn't need system font fallback — plausible since the test document likely has embedded fonts. This list is derived from a single test document; a PDF relying on non-embedded fonts or a different poppler render path could pull in libraries not covered here, so it shouldn't be treated as exhaustive until verified against a more varied set of PDFs.

Remaining before this is a real release artifact: verifying in a genuinely clean Wine prefix (or a real clean Windows machine) rather than one that's accumulated state, and testing against more varied PDFs to confirm the DLL list holds.

## Windows Build (cross-compiled from Linux)

1. Install `mingw-w64` and add the Rust target: `rustup target add x86_64-pc-windows-gnu`
2. Fetch the mingw64 poppler sysroot: `./fetch-mingw64-sysroot.sh` (resolves poppler's full dependency tree from MSYS2's package database and downloads/extracts everything into `mingw64-sysroot/`, gitignored — rerun any time poppler updates rather than maintaining a hand-written dependency list)
3. Build: `SENPAI_MINGW_SYSROOT=$(pwd)/mingw64-sysroot cargo build -p senpai-ui --target x86_64-pc-windows-gnu`
4. Package: `./package-windows.sh` — builds release, derives the DLL set from the exe's import table, and writes `target/package/senpai-<version>-windows-x64.zip`. That zip is the shippable artifact, not the bare exe. (`--debug` packages the debug build instead.)
5. Optional installer: `makensis installer.nsi` — wraps the staging directory from step 4 into `SenpaiSetup.exe`. The installer reads that same directory rather than a separately maintained one, so the zip and the installer always ship an identical bundle.

## Philosophy on AI

AI is an enhancement, not the product. Every AI capability is implemented as an optional plugin on top of the core reader — the core itself never depends on AI to function.
