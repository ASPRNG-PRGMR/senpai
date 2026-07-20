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

## Status

SENPAI is under active early development. The core reader (open, scroll, zoom PDFs at speed) is the current focus, ahead of search, annotations, and plugins.

## Philosophy on AI

AI is an enhancement, not the product. Every AI capability is implemented as an optional plugin on top of the core reader — the core itself never depends on AI to function.

## Contributing

SENPAI is open source and intended to be a serious, long-lived project rather than a demo. Contributions that align with the performance-first, minimal-dependency philosophy above are welcome. (Contribution guide coming soon.)

