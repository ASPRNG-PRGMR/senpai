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
    └── cli/                    # senpai-cli — the composition root that
        └── src/                # picks poppler as the concrete PdfEngine
            └── main.rs         # backend; current dev/benchmark entry point
```

A UI crate (`apps/ui` or similar) and future backends (e.g. `crates/engine-native`) slot into this same structure without touching existing crates.

## Status

SENPAI is under active early development. The workspace skeleton is up and the poppler-backed `PdfEngine` is working end to end: opening a document, reading metadata, and reading page geometry all run through the CLI today. Actual pixel rendering, scrolling/zoom UI, search, annotations, and plugins are next.

## Philosophy on AI

AI is an enhancement, not the product. Every AI capability is implemented as an optional plugin on top of the core reader — the core itself never depends on AI to function.

## Contributing

SENPAI is open source and intended to be a serious, long-lived project rather than a demo. Contributions that align with the performance-first, minimal-dependency philosophy above are welcome. (Contribution guide coming soon.)
