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

Not yet built: scrolling (currently single-page-at-a-time), file picker (Files → Open is stubbed), annotation tools (present in the UI but disabled), search, session state, and the plugin system.

**Platform support**: developed on Linux. The windowing/rendering stack (`winit` + `wgpu` + `egui`) has been verified to build and run on Windows too — cross-compiled via `mingw-w64` and tested under Wine — confirming the UI architecture itself is portable. PDF rendering is not yet part of that verification: `poppler-cpp` (the current PDF backend) isn't cross-built for Windows yet, so the full reader with real documents hasn't run there. A throwaway `apps/win-test` crate (no poppler dependency) is what's used to validate the windowing stack on Windows; it's not part of the shipped app.

## Philosophy on AI

AI is an enhancement, not the product. Every AI capability is implemented as an optional plugin on top of the core reader — the core itself never depends on AI to function.
