# SENPAI

**S**treaming **E**ngine for **N**ative **P**DF **A**ccess & **I**ndexing

SENPAI is a fast, native, developer-oriented document reader — built to feel instant, even on huge files, and to stay out of your way.

It is **not** "another PDF reader." It's a systems-engineered document platform, starting with PDF and Markdown, designed from day one for performance, low memory usage, and extensibility.

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
| Markdown (.md) | Supported |
| DOCX | Planned |
| EPUB | Planned |
| Plain text | Planned |
| Excel | Under consideration |

## Key Features

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
├── Cargo.toml                  # workspace root
├── crates/
│   ├── core/                   # senpai-core — engine-agnostic document model,
│   │   │                       # mmap file loading, the PdfEngine trait
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── file_loader.rs
│   │       ├── model.rs
│   │       └── engine.rs
│   │
│   ├── pdf-poppler/            # senpai-pdf-poppler — the poppler backend;
│   │   │                       # the only crate that links poppler
│   │   ├── build.rs
│   │   ├── src/
│   │   │   ├── lib.rs          # Rust: implements PdfEngine
│   │   │   └── ffi.rs          # Rust: the cxx bridge definition
│   │   └── cpp/                # C++, kept physically separate from Rust
│   │       ├── shim.h
│   │       └── shim.cpp
│   │
│   └── render/                 # senpai-render — tiling, caching (upcoming)
│       └── src/
│           └── lib.rs
│
└── apps/
    ├── cli/                    # senpai-cli — the composition root that
    │   └── src/                # picks poppler as the concrete PdfEngine
    │       └── main.rs         # backend; dev/benchmark entry point
    │
    └── ui/                     # senpai-ui — the windowed reader (eframe/
        └── src/                # egui + wgpu). No side panel, no menu bar —
            └── main.rs         # one floating top bar, hidden until hovered
                                 # or opened via Alt+F/T/M/O
```

Future backends (e.g. `crates/engine-native`) slot into this same structure without touching existing crates.

## Status

SENPAI is under active early development. What's working right now:

- **Opening documents**: memory-mapped file loading, poppler-backed parsing, metadata and page geometry — all running through the `PdfEngine` trait, verified against real PDFs.
- **Rendering**: pages render correctly through poppler and have been checked pixel-for-pixel against Evince's output.
- **A minimal windowed reader** (`senpai-ui`): opens a PDF into a native window with no side panel and no dedicated menu bar — just the current page, centered and scaled to fit. The only chrome is a single floating top bar (Files / Tools / Modes / Options) that stays hidden by default and reveals itself when you hover the top of the window or trigger a menu with `Alt+F` / `Alt+T` / `Alt+M` / `Alt+O`; auto-hide can be turned off entirely from Options. Modes includes a working Light/Night toggle. Keyboard page navigation (arrow keys, `+`/`-`, space) works regardless of whether the bar is visible.
- **Background rendering**: page rendering runs on a dedicated background thread instead of blocking the UI — page turns and zoom no longer freeze the window while poppler works. Rapid page-flipping doesn't queue up or flash through intermediate pages: only the most recently requested page/scale ever gets rendered, and any in-flight render that gets superseded is discarded when it completes rather than shown. A plain-text "rendering…" indicator (not a pill — the only pill in the app is the top bar) appears while a render is in flight.
- **Render cache + prefetching**: an LRU cache (20 entries, ~160MB budget at typical zoom) sits inside the background worker, keyed on `(page, scale)`. Revisiting a page you've already viewed at the same zoom is a cache hit instead of a re-render — measured at ~38µs versus ~86ms for a real render, roughly 2000x faster. Whenever the worker has no real request waiting, it opportunistically pre-renders the previous/next page in the background, so forward page-turns are frequently already cached by the time you ask for them. Confirmed in actual use, not just the isolated benchmark: page turns and revisits both feel meaningfully faster than the background-rendering-only version before this.
- **Prefetch depth tuning**: measured against a real 982-page book (19.8MB, *Operating System Concepts*, 8th ed.) — render cost was remarkably consistent across the whole document (66–86.5ms, avg 77.8ms across 20 sampled pages spanning page 0 to 931), confirming no single pathologically slow page was responsible for an earlier page-skipping issue during fast page-turns. The real cause: single-neighbor (depth-1) prefetch couldn't keep up with page-turns faster than ~78ms apart. Widened to depth-3 forward / depth-1 backward; confirmed in practice on the same 982-page document that this eliminates the page-skipping.

Not yet built: scrolling (currently single-page-at-a-time), file picker (Files → Open is stubbed), annotation tools (present in the UI but disabled), search, session state, and the plugin system.

**Platform support**: developed on Linux. The full reader — including PDF rendering, not just the windowing stack — now cross-compiles for Windows via `mingw-w64` and runs correctly under Wine with real documents. `poppler-cpp` and its full dependency chain (cairo/freetype/harfbuzz/glib/image codecs, etc.) are cross-built from prebuilt MSYS2 mingw64 packages, resolved and fetched by a script rather than assembled by hand, so this is reproducible rather than a one-off. The confirmed minimal set of runtime DLLs that must ship alongside `senpai-ui.exe` (derived by testing — removing a DLL and confirming the app fails without it — not just by guessing):

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
4. Bundle: copy `senpai-ui.exe` plus the DLL list above (from `mingw64-sysroot/mingw64/bin/`) into one folder — that folder is the shippable artifact, not the bare exe

## Philosophy on AI

AI is an enhancement, not the product. Every AI capability is implemented as an optional plugin on top of the core reader — the core itself never depends on AI to function.
