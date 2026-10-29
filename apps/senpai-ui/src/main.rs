//! senpai-ui -- first release candidate.
//!
//! This build includes:
//!   - Forced Vulkan backend (fixes a hard crash inside AMD's OpenGL
//!     driver, atio6axx.dll, hit via wgpu's default backend selection
//!     on at least one real Windows machine -- confirmed root cause,
//!     not a workaround)
//!   - Paged mode: dynamic current-page +/-1 prefetch, recomputed every
//!     frame from the actual current position (replaces the old
//!     worker-side "queue neighbors after finishing a render" logic,
//!     which could anchor prefetch to a stale position under fast
//!     navigation)
//!   - Stale (superseded) render results are still cached, never
//!     discarded -- only "is this the current display" is gated by
//!     generation, so navigating back to a recently-left page doesn't
//!     force a re-render
//!   - Fixed a texture double-free panic (wgpu "texture has been
//!     destroyed"): drain_results no longer overwrites an existing
//!     cached texture for a (page, scale) key that's already present
//!   - LRU-evicted textures are dropped at the START of the next frame
//!     (pending_evictions), never mid-frame, avoiding a race against
//!     that frame's paint submission
//!   - Worker state (primary request + prefetch queue) unified behind
//!     ONE Mutex+Condvar, enabling true zero-CPU blocking with no
//!     polling, and atomic dedup/clearing
//!   - A real file picker (Files -> Open, or Ctrl+O), so the reader is
//!     no longer limited to the document named on the command line.
//!     Opening a document retires the previous render worker (dropping
//!     its cache and its engine handle with it) and spawns a fresh one,
//!     keeping the invariant that exactly one thread ever touches any
//!     given poppler document.
//!
//! The chrome's look lives in `theme.rs`: capsule surfaces, 120-150ms
//! state transitions and a segmented switcher, following omen-space's
//! design vocabulary. Page *content* is deliberately never animated --
//! see the note at the top of that module.
//!
//! Known broken, NOT default (Paged is the startup mode): Continuous
//! Scroll mode. Deferred to after this release.

mod config;
mod keybinds;
mod markdown;
mod recent;
mod theme;

use eframe::egui;
use keybinds::{NavKeys, NavTarget, Preset};
use lru::LruCache;
use senpai_core::engine::{PdfEngine, PdfEngineOpen};
use senpai_core::file_loader::LoadedFile;
use senpai_core::model::{PageHandle, PageSize, RenderedPage};
use senpai_pdf_poppler::PopplerEngine;
use std::collections::VecDeque;
use std::env;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use theme::{Pill, PillStyle};

fn main() -> eframe::Result<()> {
    // Force Vulkan explicitly. wgpu's default backend selection on at
    // least one real Windows/AMD machine picked a path that crashes
    // inside atio6axx.dll (AMD's OpenGL driver) with an access
    // violation (0xC0000005). Manually setting WGPU_BACKEND=vulkan
    // confirmed this fixes it -- doing it here means the end user never
    // has to know or set anything.
    env::set_var("WGPU_BACKEND", "vulkan");

    let mut args = env::args().skip(1);
    let initial_path = match args.next().as_deref() {
        Some("-h") | Some("--help") => {
            println!(
                "usage: senpai [path-to-pdf]    (or start with no file and use Files -> Open)"
            );
            return Ok(());
        }
        // A path is now optional: with no argument the reader starts on
        // its empty state and waits for Files -> Open (Ctrl+O).
        other => other.map(PathBuf::from),
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("senpai")
            .with_inner_size([1000.0, 750.0]),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };

    eframe::run_native(
        "senpai",
        options,
        Box::new(move |cc| Ok(Box::new(SenpaiApp::new(cc, initial_path)))),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Menu {
    Files,
    Tools,
    Modes,
    Options,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Paged,
    Continuous,
}

const CACHE_CAPACITY: usize = 30;
/// US Letter, used only to lay out rows for pages whose real height
/// hasn't been rendered (and therefore measured) yet.
const FALLBACK_PAGE_SIZE: PageSize = PageSize {
    width_pt: 612.0,
    height_pt: 792.0,
};
const HOVER_ZONE_HEIGHT: f32 = 40.0;
/// How long a render must stay pending before the "rendering…" pill is
/// shown at all. A cache hit resolves in microseconds; flashing a spinner
/// for a frame makes an instant action look like a slow one.
const RENDER_HINT_DELAY: f32 = 0.18;
const CONTINUOUS_BUFFER_PAGES: usize = 1;
/// How many recent documents the Files menu lists. The dropdown is a
/// fixed 330px wide and shares that space with the Open button and the
/// current document's details; more than this and the menu stops being
/// something you can read at a glance.
/// How long the lone page capsule takes to leave. Shorter than the
/// bar's own fade so it is out of the way before the bar is fully there.
const PAGE_PILL_FADE: f32 = 0.09;
/// Width to reserve for the page counter.
///
/// Measured on the widest text it will ever hold rather than on what it
/// holds now: sized to the current number, the whole right-hand group
/// would shift sideways on the turn from page 9 to page 10.
fn page_counter_width(ctx: &egui::Context, page_count: usize) -> f32 {
    let widest = format!("{page_count} / {page_count}");
    ctx.fonts(|f| {
        f.layout_no_wrap(
            widest,
            egui::FontId::proportional(theme::COUNTER_FONT),
            egui::Color32::WHITE,
        )
    })
    .size()
    .x
}

/// Largest text file either text view will open. See `read_text`.
const MAX_TEXT_BYTES: usize = 512 * 1024;

/// Settles who owns the zoom shortcuts.
///
/// egui binds Ctrl +, Ctrl = and Ctrl - to a zoom of its own -- the
/// whole interface, chrome included -- and has it on by default. The
/// reader's `+` and `-` are checked without looking at modifiers, so
/// Ctrl and a plus key was firing both at once: the document scaled and
/// the top bar grew with it. Two zooms answering one key is not a
/// feature, it is a collision, and the one that should win is obvious.
/// A reader zooms the thing being read.
///
/// So egui's is turned off and Ctrl + / Ctrl - fall through to the
/// document, the same as the bare keys. The bar now stays the size it
/// was drawn at.
fn configure_context(ctx: &egui::Context) {
    ctx.options_mut(|o| o.zoom_with_keyboard = false);
}

/// The zoom level as a percentage, where the level a document opens at
/// is 100%.
///
/// `scale` is a renderer figure -- the multiplier handed to poppler,
/// 1.0 being 72 DPI -- and means nothing to a reader. What a reader
/// wants to know is how far from "as opened" they have got, which is
/// what this reports. A free function rather than a method so the
/// arithmetic can be checked without an app.
fn zoom_percent(scale: f64) -> u32 {
    (scale / DEFAULT_SCALE * 100.0).round() as u32
}

/// How long the lone capsule stays up after a page turn. Long enough to
/// read a four-digit number, short enough not to become scenery.
const PAGE_FLASH: f64 = 1.4;
/// Starting zoom, and the 100% the bar reports.
///
/// 1.8 rather than 1.5: a page opened at the old default wanted one
/// press of `+` before it was comfortable to read, which is a poor place
/// to call 100%. The step the keys take is 1.2, so this is exactly that
/// one press -- what used to read 120% is now where a document starts.
///
/// It does both jobs, which is what keeps them consistent: pages render
/// at this scale, and for text it is the divisor that turns the same
/// number into a type-size multiplier. Moving it therefore opens PDFs
/// larger while leaving text exactly where it was, since text is sized
/// relative to this and both halves moved together.
const DEFAULT_SCALE: f64 = 1.8;
const RECENT_IN_MENU: usize = 6;
/// Budget for an entry's label, in characters. The dropdown's pills do
/// not wrap or truncate on their own -- a long name would simply push
/// the pill wider than the card.
const RECENT_NAME_CHARS: usize = 34;

/// What `open_document` resolved a path to. Boxed on the PDF side
/// because an engine plus its metadata is far larger than a parsed
/// Markdown document, and a lopsided enum would make every move of it
/// pay for the bigger arm.
enum DocumentContent {
    Pdf(Box<(PopplerEngine, usize, PageSize)>),
    Markdown(markdown::Document),
    Plain(String),
}

/// What the reader is currently showing, when it is not a PDF.
///
/// One slot rather than a field per kind: they are mutually exclusive,
/// and a pair of `Option`s invites the state where both are set.
enum TextDoc {
    Markdown(markdown::Document),
    Plain(String),
}

fn is_plain_text(path: &Path) -> bool {
    path.extension().is_some_and(|e| {
        let e = e.to_string_lossy().to_lowercase();
        e == "txt" || e == "text"
    })
}

fn is_markdown(path: &Path) -> bool {
    path.extension().is_some_and(|e| {
        let e = e.to_string_lossy().to_lowercase();
        e == "md" || e == "markdown" || e == "mdown" || e == "mkd"
    })
}

/// Reads a text file whole, as lossy UTF-8.
///
/// Read whole rather than mapped, unlike a PDF: both text views walk the
/// entire file anyway. Invalid bytes are replaced rather than rejected
/// -- a stray byte in a mostly-readable file should not stop it opening.
///
/// The size limit is real rather than defensive. Neither text view is
/// virtualised: every block, or the whole plain-text galley, is laid out
/// for each frame it is on screen. That is fine for documents and notes
/// and hopeless for a log, so the limit refuses the file with a reason
/// instead of hanging the window on it.
fn read_text(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("could not open: {e}"))?;
    if bytes.len() > MAX_TEXT_BYTES {
        return Err(format!(
            "{:.1}MB of text; the limit is {}KB, because the text views lay the whole \
             file out on every frame rather than only the part on screen",
            bytes.len() as f64 / 1_000_000.0,
            MAX_TEXT_BYTES / 1024
        ));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Picks which of several dropped files to open.
///
/// By precedence rather than by position: a PDF first, then Markdown,
/// then plain text.
///
/// Order matters once more than one kind is recognised. A drop carrying
/// a paper and the notes beside it should open the paper, whichever the
/// file manager happened to list first -- and `.txt` being the broadest
/// kind, it is the one most likely to be along for the ride rather than
/// the thing meant.
///
/// Failing all three the first file is taken anyway rather than the drop
/// being ignored: a PDF with no extension is a real thing, and when it
/// genuinely is not one, letting the loader fail says which file and why
/// -- which silently doing nothing does not.
fn pick_dropped_document(paths: &[PathBuf]) -> Option<&Path> {
    paths
        .iter()
        .find(|p| is_pdf(p))
        .or_else(|| paths.iter().find(|p| is_markdown(p)))
        .or_else(|| paths.iter().find(|p| is_plain_text(p)))
        .or_else(|| paths.first())
        .map(PathBuf::as_path)
}

fn is_pdf(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
}

/// Shortens `text` to `max` characters by dropping its middle.
///
/// The tail is kept rather than cut because that is where a file name
/// carries its distinguishing part -- an edition, a chapter number, the
/// extension -- while the head is often a shared prefix.
fn elide(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_owned();
    }
    // Below this there is no middle left to drop and the result would be
    // mostly ellipsis; plain truncation is more honest.
    if max < 6 {
        return chars[..max].iter().collect();
    }
    let tail = max / 3;
    let head = max - tail - 1;
    let mut out: String = chars[..head].iter().collect();
    out.push('\u{2026}');
    out.extend(&chars[chars.len() - tail..]);
    out
}

enum RenderKind {
    Primary { generation: u64 },
    Prefetch,
}

struct PendingRequest {
    page: usize,
    scale: f64,
    generation: u64,
}

struct RenderResult {
    page: usize,
    scale: f64,
    kind: RenderKind,
    outcome: Result<Arc<RenderedPage>, String>,
}

/// All state the worker needs, behind ONE lock -- so a check of "is
/// there work" and a push of new work always happen atomically, and
/// the worker can do a true zero-CPU blocking wait with no polling.
struct WorkerState {
    primary: Option<PendingRequest>,
    prefetch: VecDeque<(usize, f64)>,
    /// Set when this worker's document has been replaced (Files -> Open).
    /// The worker returns at the top of its next loop iteration, dropping
    /// its render cache and its `Arc<PopplerEngine>` as it unwinds -- so
    /// switching documents doesn't leak a thread, nor the previous
    /// document's cached pages, per open.
    shutdown: bool,
}

type Shared = Arc<(Mutex<WorkerState>, Condvar)>;

enum WorkItem {
    Primary(PendingRequest),
    Prefetch(usize, f64),
}

fn push_prefetch_dedup(state: &mut WorkerState, page: usize, scale: f64) {
    let scale_bits = scale.to_bits();
    let already_queued = state
        .prefetch
        .iter()
        .any(|&(p, s)| p == page && s.to_bits() == scale_bits);
    if !already_queued {
        state.prefetch.push_back((page, scale));
    }
}

fn spawn_render_worker(
    engine: Arc<PopplerEngine>,
    shared: Shared,
    result_tx: Sender<RenderResult>,
    ctx: egui::Context,
) {
    thread::spawn(move || {
        let mut cache: LruCache<(usize, u64), Arc<RenderedPage>> =
            LruCache::new(NonZeroUsize::new(CACHE_CAPACITY).unwrap());

        loop {
            let work = {
                let (lock, cvar) = &*shared;
                let mut guard = lock.lock().unwrap();
                loop {
                    if guard.shutdown {
                        return;
                    }
                    if let Some(req) = guard.primary.take() {
                        break WorkItem::Primary(req);
                    }
                    if let Some((page, scale)) = guard.prefetch.pop_front() {
                        break WorkItem::Prefetch(page, scale);
                    }
                    guard = cvar.wait(guard).unwrap();
                }
            };

            match work {
                WorkItem::Primary(request) => {
                    let key = (request.page, request.scale.to_bits());
                    let outcome =
                        render_or_cache_hit(&engine, &mut cache, key, request.page, request.scale);

                    let _ = result_tx.send(RenderResult {
                        page: request.page,
                        scale: request.scale,
                        kind: RenderKind::Primary {
                            generation: request.generation,
                        },
                        outcome,
                    });
                    ctx.request_repaint();

                    // NOTE: deliberately no worker-side neighbor
                    // prefetch here anymore. Paged mode's +/-1 lookahead
                    // is now driven entirely by the UI thread, computed
                    // fresh from the actual current page every frame
                    // (see SenpaiApp's ViewMode::Paged arm in update()).
                    // The old version queued neighbors of "whatever page
                    // the worker just finished," which could lag behind
                    // the user's real position under fast navigation.
                }
                WorkItem::Prefetch(page, scale) => {
                    let key = (page, scale.to_bits());
                    let outcome = render_or_cache_hit(&engine, &mut cache, key, page, scale);

                    let queue_now_empty = {
                        let (lock, _cvar) = &*shared;
                        lock.lock().unwrap().prefetch.is_empty()
                    };

                    let _ = result_tx.send(RenderResult {
                        page,
                        scale,
                        kind: RenderKind::Prefetch,
                        outcome,
                    });
                    if queue_now_empty {
                        ctx.request_repaint();
                    }
                }
            }
        }
    });
}

fn render_or_cache_hit(
    engine: &PopplerEngine,
    cache: &mut LruCache<(usize, u64), Arc<RenderedPage>>,
    key: (usize, u64),
    page: usize,
    scale: f64,
) -> Result<Arc<RenderedPage>, String> {
    if let Some(cached) = cache.get(&key) {
        return Ok(cached.clone());
    }
    match engine.render_page(PageHandle(page), scale) {
        Ok(rendered) => {
            let arc = Arc::new(rendered);
            cache.put(key, arc.clone());
            Ok(arc)
        }
        Err(e) => Err(e.to_string()),
    }
}

struct SenpaiApp {
    /// 0 means "no document open" -- the startup state when senpai is
    /// launched without a path, and the state between a failed open and
    /// the next successful one.
    page_count: usize,
    doc_path: Option<PathBuf>,
    current_page: usize,
    scale: f64,
    estimated_page_size: PageSize,
    known_page_heights: Vec<Option<f32>>,

    /// `None` until a document is open: the worker (and therefore the
    /// channel back from it) belongs to one specific document.
    shared: Option<Shared>,
    result_rx: Option<Receiver<RenderResult>>,
    next_generation: u64,
    requested_primary: Option<(usize, u64, u64)>,

    textures: LruCache<(usize, u64), egui::TextureHandle>,
    pending_evictions: Vec<egui::TextureHandle>,
    render_pending: bool,
    continuous_render_pending: bool,

    dark_mode: bool,
    auto_hide_enabled: bool,
    view_mode: ViewMode,
    nav_keys: NavKeys,
    rebinding: Option<NavTarget>,
    /// A key the reader tried to assign that is spoken for already, kept
    /// only long enough to say so.
    rebind_rejected: Option<egui::Key>,
    open_menu: Option<Menu>,
    /// Where the bar and the open dropdown were painted last frame, used
    /// to decide whether a click landed on the chrome or outside it.
    /// Last frame's geometry is accurate enough: neither moves once the
    /// open animation has settled.
    bar_rect: egui::Rect,
    menu_rect: egui::Rect,
    /// The menu currently being *painted*, which outlives `open_menu` for
    /// the length of the close animation. Without it the dropdown stops
    /// being drawn the instant `open_menu` clears, so it faded in and then
    /// vanished -- the fade-out was computed but never used.
    menu_painting: Option<Menu>,

    /// Result channel for an in-flight file dialog. The dialog itself
    /// runs on its own thread -- blocking the UI thread on it would stall
    /// painting for as long as the user browses, which the performance
    /// budget rules out.
    pending_open: Option<Receiver<Option<PathBuf>>>,
    open_error: Option<String>,
    /// Documents opened before, newest first. Also decides where the
    /// file dialog starts -- see the note at the top of `recent.rs`.
    recent: recent::Recent,
    /// The document when a text file is open, Markdown or plain.
    /// Mutually exclusive with a PDF: installing either clears the other.
    text: Option<TextDoc>,
    /// Pixels the Markdown view should scroll on the next frame, from a
    /// page-navigation key. Applied inside the scroll area, which is the
    /// only place the offset can be changed.
    md_scroll: f32,
    /// Name shown on the drag-and-drop overlay. Kept after the pointer
    /// leaves so the card still reads correctly while it fades out --
    /// clearing it with the hover would blank the text mid-fade.
    drop_label: Option<String>,

    /// When the current render request started, for `RENDER_HINT_DELAY`.
    render_started: Option<f64>,
    /// Page the counter last reported, so a turn can be noticed.
    last_shown_page: usize,
    /// When the lone capsule should stop being shown. Only meaningful
    /// while the bar is hidden; with the bar up the number is in it.
    page_flash_until: f64,
}

/// Whether a pointer press should close the open menu.
///
/// Presses on the bar are excluded so the menu button keeps its own
/// toggle, and so switching straight to another menu works: without that
/// exclusion a click on a menu button would be dismissed here and then
/// re-opened by the button, leaving it stuck open. Presses inside the
/// dropdown are excluded because that is where its controls live.
fn should_dismiss_menu(
    menu_open: bool,
    press: Option<egui::Pos2>,
    bar_rect: egui::Rect,
    menu_rect: egui::Rect,
) -> bool {
    match press {
        Some(pos) if menu_open => !bar_rect.contains(pos) && !menu_rect.contains(pos),
        _ => false,
    }
}

/// mmap + parse + read the cheap metadata, all before anything is swapped
/// into the app. Done on the caller's thread, which is sound because the
/// document isn't shared with a worker yet: the "only the render thread
/// touches an open document" invariant starts the moment the worker is
/// spawned, not before. Measured at ~11ms for a 1278-page/31MB document,
/// well inside one frame's budget for a user-initiated action.
fn load_document(path: &Path) -> Result<(PopplerEngine, usize, PageSize), String> {
    let file = LoadedFile::open(path).map_err(|e| format!("could not open: {e}"))?;
    let engine =
        PopplerEngine::open(file.as_bytes()).map_err(|e| format!("could not parse: {e}"))?;
    drop(file);

    let page_count = engine.metadata().page_count;
    if page_count == 0 {
        return Err("document has no pages".to_string());
    }
    let size = engine
        .page_size(PageHandle(0))
        .unwrap_or(FALLBACK_PAGE_SIZE);
    Ok((engine, page_count, size))
}

impl SenpaiApp {
    fn new(cc: &eframe::CreationContext<'_>, initial_path: Option<PathBuf>) -> Self {
        let mut app = Self {
            page_count: 0,
            doc_path: None,
            current_page: 0,
            scale: DEFAULT_SCALE,
            estimated_page_size: FALLBACK_PAGE_SIZE,
            known_page_heights: Vec::new(),
            shared: None,
            result_rx: None,
            next_generation: 0,
            requested_primary: None,
            textures: LruCache::new(NonZeroUsize::new(CACHE_CAPACITY).unwrap()),
            pending_evictions: Vec::new(),
            render_pending: false,
            continuous_render_pending: false,
            dark_mode: true,
            auto_hide_enabled: true,
            view_mode: ViewMode::Paged,
            nav_keys: NavKeys::load(),
            rebinding: None,
            rebind_rejected: None,
            open_menu: None,
            bar_rect: egui::Rect::NOTHING,
            menu_rect: egui::Rect::NOTHING,
            menu_painting: None,
            pending_open: None,
            open_error: None,
            recent: recent::Recent::load(),
            text: None,
            md_scroll: 0.0,
            drop_label: None,
            render_started: None,
            last_shown_page: 0,
            page_flash_until: 0.0,
        };
        configure_context(&cc.egui_ctx);
        theme::install(&cc.egui_ctx, app.dark_mode);
        if let Some(path) = initial_path {
            app.open_document(&cc.egui_ctx, &path);
            // A bad path on the command line is still worth reporting on
            // stderr for CLI use; the window stays up on its empty state
            // rather than exiting, so the user can just pick another file.
            if let Some(err) = &app.open_error {
                eprintln!("{err}");
            }
        }
        app
    }

    /// Spawns the file dialog on its own thread and remembers the channel
    /// it will answer on; `poll_pending_open` picks the answer up on a
    /// later frame. Re-entrant calls are ignored so a second Ctrl+O can't
    /// stack a second dialog.
    fn begin_open_dialog(&mut self, ctx: &egui::Context) {
        if self.pending_open.is_some() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        // Where the dialog lands matters more here than it would on a
        // desktop picker with a usable sidebar: start beside the open
        // document, else beside the last one opened, else somewhere a
        // document plausibly lives. Anything else drops the user at the
        // drive root with a lot of clicking ahead of them.
        let start_dir = self
            .doc_path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .filter(|dir| dir.is_dir())
            .or_else(|| self.recent.start_dir())
            .or_else(recent::default_start_dir);
        let ctx = ctx.clone();
        thread::spawn(move || {
            let mut dialog = rfd::FileDialog::new()
                .set_title("Open a document")
                // The combined filter goes first because it is the
                // default selection, and "anything this reader opens" is
                // what someone browsing actually wants to see.
                .add_filter(
                    "Documents",
                    &["pdf", "md", "markdown", "mdown", "mkd", "txt", "text"],
                )
                .add_filter("PDF document", &["pdf"])
                .add_filter("Markdown", &["md", "markdown", "mdown", "mkd"])
                .add_filter("Plain text", &["txt", "text"]);
            if let Some(dir) = start_dir {
                dialog = dialog.set_directory(dir);
            }
            let _ = tx.send(dialog.pick_file());
            ctx.request_repaint();
        });
        self.pending_open = Some(rx);
    }

    fn poll_pending_open(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.pending_open else {
            return;
        };
        match rx.try_recv() {
            Ok(Some(path)) => {
                self.pending_open = None;
                self.open_document(ctx, &path);
            }
            // User cancelled, or the dialog thread died without answering.
            Ok(None) | Err(TryRecvError::Disconnected) => self.pending_open = None,
            Err(TryRecvError::Empty) => {}
        }
    }

    /// Opens a file dragged onto the window, and paints the overlay that
    /// says so while a drag is in progress.
    ///
    /// Both halves live here because they read the same input: egui
    /// reports an in-flight drag as `hovered_files` and a completed one
    /// as `dropped_files`, and the overlay is simply the first of those
    /// made visible.
    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let (hovering, hovered_name, dropped) = ctx.input(|i| {
            let hovering = !i.raw.hovered_files.is_empty();
            // A path is not always attached -- some platforms only name
            // the file on the drop itself -- so the card falls back to
            // its title alone rather than showing an empty line.
            let name = i
                .raw
                .hovered_files
                .iter()
                .find_map(|f| f.path.as_deref())
                .and_then(|p| p.file_name())
                .map(|n| elide(&n.to_string_lossy(), RECENT_NAME_CHARS));
            let dropped: Vec<PathBuf> = i
                .raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect();
            (hovering, name, dropped)
        });
        if hovering {
            self.drop_label = hovered_name;
        }

        let t = ctx.animate_bool_with_time(egui::Id::new("drop_fade"), hovering, theme::T_PANEL);
        theme::drop_overlay(ctx, t, self.drop_label.as_deref());

        if let Some(path) = pick_dropped_document(&dropped) {
            let path = path.to_path_buf();
            // A drop is a deliberate "open this now"; a menu left open
            // over the top of it would just be in the way.
            self.close_menu();
            self.open_document(ctx, &path);
        }
    }

    /// Opens whatever the path points at, picking the reader by
    /// extension.
    ///
    /// Extension rather than content sniffing: a `.md` file is plain
    /// text and has no magic number to find, so the name is the only
    /// thing that distinguishes it from any other text file. A PDF is
    /// the default because that is what this reader is for, and
    /// poppler's own error is a better answer for a mislabelled file
    /// than a guess would be.
    fn open_document(&mut self, ctx: &egui::Context, path: &Path) {
        let result = if is_markdown(path) {
            read_text(path).map(|t| DocumentContent::Markdown(markdown::parse(&t)))
        } else if is_plain_text(path) {
            read_text(path).map(DocumentContent::Plain)
        } else {
            load_document(path).map(|(engine, page_count, size)| {
                DocumentContent::Pdf(Box::new((engine, page_count, size)))
            })
        };
        match result {
            Ok(DocumentContent::Pdf(boxed)) => {
                let (engine, page_count, size) = *boxed;
                self.install_document(ctx, path, engine, page_count, size);
                self.open_error = None;
                self.recent.record(path);
            }
            Ok(DocumentContent::Markdown(doc)) => {
                self.install_text(ctx, path, TextDoc::Markdown(doc));
                self.open_error = None;
                self.recent.record(path);
            }
            Ok(DocumentContent::Plain(text)) => {
                self.install_text(ctx, path, TextDoc::Plain(text));
                self.open_error = None;
                self.recent.record(path);
            }
            Err(e) => {
                self.open_error = Some(format!("{}: {e}", path.display()));
            }
        }
    }

    /// Swaps in a text document of either kind, retiring any PDF worker
    /// first.
    ///
    /// Shares `retire_worker` with the PDF path rather than duplicating
    /// it: leaving a worker running against a document nobody is looking
    /// at would keep its engine handle -- and its memory -- alive for as
    /// long as the app ran.
    fn install_text(&mut self, ctx: &egui::Context, path: &Path, doc: TextDoc) {
        self.retire_worker();
        self.page_count = 0;
        self.current_page = 0;
        self.known_page_heights.clear();
        self.md_scroll = 0.0;
        self.text = Some(doc);
        self.doc_path = Some(path.to_path_buf());
        self.set_window_title(ctx, path);
    }

    /// Retires the previous document's worker and swaps in a new one.
    ///
    /// Order matters: the old worker is told to shut down *before* the new
    /// one is spawned, so the two never render the same document, and the
    /// old document's engine handle is dropped on the thread that owned it.
    fn install_document(
        &mut self,
        ctx: &egui::Context,
        path: &Path,
        engine: PopplerEngine,
        page_count: usize,
        estimated_page_size: PageSize,
    ) {
        self.retire_worker();
        self.text = None;

        let shared: Shared = Arc::new((
            Mutex::new(WorkerState {
                primary: None,
                prefetch: VecDeque::new(),
                shutdown: false,
            }),
            Condvar::new(),
        ));
        let (tx, rx) = mpsc::channel();
        spawn_render_worker(Arc::new(engine), shared.clone(), tx, ctx.clone());

        self.shared = Some(shared);
        self.result_rx = Some(rx);
        self.page_count = page_count;
        self.doc_path = Some(path.to_path_buf());
        self.current_page = 0;
        self.estimated_page_size = estimated_page_size;
        self.known_page_heights = vec![None; page_count];
        // Flashes the count once on open, so how long the document is
        // can be seen without having to turn a page to find out.
        self.last_shown_page = usize::MAX;
        self.next_generation = 0;
        self.requested_primary = None;
        self.render_pending = false;
        self.continuous_render_pending = false;

        self.set_window_title(ctx, path);
    }

    /// Shuts down the render worker and releases the textures it filled.
    ///
    /// Order matters: the old worker is told to stop *before* a new one
    /// is spawned, so the two never render the same document, and the
    /// old engine handle is dropped on the thread that owned it.
    fn retire_worker(&mut self) {
        if let Some(old) = self.shared.take() {
            let (lock, cvar) = &*old;
            let mut guard = lock.lock().unwrap();
            guard.shutdown = true;
            guard.primary = None;
            guard.prefetch.clear();
            cvar.notify_all();
        }
        // Dropping the receiver makes any result the old worker still
        // sends a no-op, so a late render can't land on the new document.
        self.result_rx = None;

        // The old document's textures go through pending_evictions rather
        // than being dropped here: this runs mid-update(), and dropping a
        // texture in the same frame that may already have queued paint
        // commands referencing it is exactly the wgpu "texture has been
        // destroyed" race that pending_evictions exists to avoid.
        while let Some((_key, handle)) = self.textures.pop_lru() {
            self.pending_evictions.push(handle);
        }
    }

    fn set_window_title(&self, ctx: &egui::Context, path: &Path) {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("{name} — senpai")));
    }

    /// Whether anything is open at all, in either reader.
    fn has_document(&self) -> bool {
        self.page_count > 0 || self.text.is_some()
    }

    fn insert_texture(&mut self, key: (usize, u64), handle: egui::TextureHandle) {
        if self.textures.len() >= self.textures.cap().get() && !self.textures.contains(&key) {
            if let Some((_evicted_key, evicted_handle)) = self.textures.pop_lru() {
                self.pending_evictions.push(evicted_handle);
            }
        }
        self.textures.put(key, handle);
    }

    fn request_primary(&mut self, page: usize) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        let want = (page, self.scale.to_bits());
        let already_current = self.textures.contains(&want);
        let already_requested = self
            .requested_primary
            .map(|(p, s, _)| (p, s) == want)
            .unwrap_or(false);
        if already_current || already_requested {
            return;
        }

        self.next_generation += 1;
        let generation = self.next_generation;
        {
            let (lock, cvar) = &*shared;
            let mut guard = lock.lock().unwrap();
            guard.primary = Some(PendingRequest {
                page: want.0,
                scale: self.scale,
                generation,
            });
            cvar.notify_one();
        }
        self.requested_primary = Some((want.0, want.1, generation));
        self.render_pending = true;
        self.render_started = None;
    }

    /// Used both by Paged mode's +/-1 lookahead (called with a window of
    /// just current_page-1 and current_page+1, recomputed every frame)
    /// and by Continuous mode's visible-range window.
    fn request_window_page(&mut self, page: usize) {
        let Some(shared) = self.shared.clone() else {
            return;
        };
        if page >= self.page_count {
            return;
        }
        let key = (page, self.scale.to_bits());
        if self.textures.contains(&key) {
            return;
        }
        let (lock, cvar) = &*shared;
        let mut guard = lock.lock().unwrap();
        push_prefetch_dedup(&mut guard, page, self.scale);
        cvar.notify_one();
    }

    /// Single exit for the menu, so an armed key-rebind can't be left
    /// listening behind a dropdown that is no longer on screen.
    fn close_menu(&mut self) {
        self.open_menu = None;
        self.rebinding = None;
    }

    fn clear_pending_prefetch(&mut self) {
        if let Some(shared) = &self.shared {
            let (lock, _cvar) = &**shared;
            lock.lock().unwrap().prefetch.clear();
        }
    }

    fn drain_results(&mut self, ctx: &egui::Context) {
        // Collected first so the loop body is free to mutate `self`
        // (texture cache, page heights) while the receiver stays put.
        let mut results = Vec::new();
        if let Some(rx) = &self.result_rx {
            while let Ok(result) = rx.try_recv() {
                results.push(result);
            }
        }

        for result in results {
            let key = (result.page, result.scale.to_bits());

            if let RenderKind::Primary { generation } = result.kind {
                let is_latest = self
                    .requested_primary
                    .map(|(_, _, g)| g == generation)
                    .unwrap_or(false);
                if is_latest {
                    self.render_pending = false;
                }
                // No `continue` here: even a superseded render is real,
                // correct work. Caching it costs nothing extra (display
                // is separately gated by whether it matches the CURRENT
                // page/scale) and means navigating back to a recently-
                // left page doesn't force a re-render.
            }

            match result.outcome {
                Ok(rendered) => {
                    // Guard against overwriting an existing texture for
                    // this exact key -- LruCache::put on an existing key
                    // silently drops the old TextureHandle synchronously,
                    // bypassing pending_evictions entirely. This was the
                    // actual cause of the "texture has been destroyed"
                    // wgpu panic.
                    if !self.textures.contains(&key) {
                        if let Some(slot) = self.known_page_heights.get_mut(result.page) {
                            *slot = Some(rendered.height_px as f32);
                        }
                        let image = egui::ColorImage::from_rgba_unmultiplied(
                            [rendered.width_px as usize, rendered.height_px as usize],
                            &rendered.pixels,
                        );
                        let handle = ctx.load_texture(
                            format!("page-{}", result.page),
                            image,
                            egui::TextureOptions::LINEAR,
                        );
                        self.insert_texture(key, handle);
                    }
                }
                Err(e) => {
                    eprintln!("render failed for page {}: {e}", result.page);
                }
            }
        }
    }

    fn next_page(&mut self) {
        if self.current_page + 1 < self.page_count {
            self.current_page += 1;
        }
    }
    fn prev_page(&mut self) {
        self.current_page = self.current_page.saturating_sub(1);
    }
    fn zoom_in(&mut self) {
        self.scale = (self.scale * 1.2).min(6.0);
        self.clear_pending_prefetch();
    }
    fn zoom_out(&mut self) {
        self.scale = (self.scale / 1.2).max(0.2);
        self.clear_pending_prefetch();
    }

    /// Back to the level a document opens at -- the 100% the bar reports.
    ///
    /// The ladder is multiplicative, so stepping back by hand means
    /// landing on 104% or 96% and not being able to tell which. A reset
    /// is the only way to actually return to where you started.
    fn reset_zoom(&mut self) {
        self.scale = DEFAULT_SCALE;
        self.clear_pending_prefetch();
    }

    fn set_view_mode(&mut self, mode: ViewMode) {
        if self.view_mode != mode {
            self.view_mode = mode;
            self.clear_pending_prefetch();
        }
    }

    fn set_dark(&mut self, ctx: &egui::Context, dark: bool) {
        self.dark_mode = dark;
        theme::install(ctx, dark);
    }

    fn estimated_row_height(&self, page: usize) -> f32 {
        if let Some(h) = self.known_page_heights.get(page).copied().flatten() {
            return h;
        }
        (self.estimated_page_size.height_pt * self.scale) as f32
    }
}

impl eframe::App for SenpaiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pending_evictions.clear();

        // Click anywhere off the chrome to dismiss an open menu. Checked
        // before the bar is drawn, against last frame's geometry, so a
        // press on a menu button still reaches that button.
        let press = ctx.input(|i| {
            i.pointer
                .any_pressed()
                .then(|| i.pointer.interact_pos())
                .flatten()
        });
        if should_dismiss_menu(
            self.open_menu.is_some(),
            press,
            self.bar_rect,
            self.menu_rect,
        ) {
            self.close_menu();
        }

        self.handle_dropped_files(ctx);

        if let Some(target) = self.rebinding {
            let (escape, captured) = ctx.input(|i| {
                let escape = i.key_pressed(egui::Key::Escape);
                let captured = i.events.iter().find_map(|e| match e {
                    egui::Event::Key {
                        key, pressed: true, ..
                    } => Some(*key),
                    _ => None,
                });
                (escape, captured)
            });
            if escape {
                self.rebinding = None;
            } else if let Some(key) = captured {
                if keybinds::is_assignable(key) {
                    self.nav_keys.bind(target, key);
                    self.nav_keys.save();
                    self.rebind_rejected = None;
                } else {
                    // Refused rather than silently ignored: pressing a
                    // key and having nothing happen reads as the feature
                    // being broken.
                    self.rebind_rejected = Some(key);
                }
                self.rebinding = None;
            }
        } else {
            let keys = self.nav_keys;
            let (nav, want_zoom_in, want_zoom_out, want_zoom_reset) = ctx.input(|i| {
                // A direction fires on its built-in keys or on whatever
                // the reader has bound to it. Built-ins are never taken
                // away, so a binding can only add a way to move.
                let asked = |target: NavTarget| {
                    target.builtins().iter().any(|k| i.key_pressed(*k))
                        || keys.get(target).is_some_and(|k| i.key_pressed(k))
                };
                (
                    [
                        asked(NavTarget::Left),
                        asked(NavTarget::Right),
                        asked(NavTarget::Up),
                        asked(NavTarget::Down),
                    ],
                    // Modifiers deliberately unchecked: Ctrl and a plus
                    // key means the same thing here as the plus key on
                    // its own, now that egui is not also listening for
                    // it. See `configure_context`.
                    i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals),
                    i.key_pressed(egui::Key::Minus),
                    // This one does want the modifier: `0` alone is a
                    // key someone could bind a direction to, and it is a
                    // digit a reader might simply type.
                    i.modifiers.command && i.key_pressed(egui::Key::Num0),
                )
            });
            let [want_left, want_right, want_up, want_down] = nav;
            if self.text.is_some() {
                // Left and right keep meaning "move on by one
                // screenful" in a format that has no pages. Slightly
                // less than a full screen, so a line or two carries over
                // and the eye has somewhere to land. Up and down are the
                // finer movement: a few lines, the way an arrow key
                // behaves in anything else that scrolls.
                let screen = ctx.screen_rect().height();
                if want_right {
                    self.md_scroll += screen * 0.85;
                }
                if want_left {
                    self.md_scroll -= screen * 0.85;
                }
                if want_down {
                    self.md_scroll += screen * 0.2;
                }
                if want_up {
                    self.md_scroll -= screen * 0.2;
                }
            } else if self.view_mode == ViewMode::Paged {
                // A page is fitted to the window, so there is nothing
                // for up and down to scroll. Rather than have them do
                // nothing, they turn pages too -- which is what every
                // other reader does once a page fits.
                if want_right || want_down {
                    self.next_page();
                }
                if want_left || want_up {
                    self.prev_page();
                }
            }
            if want_zoom_in {
                self.zoom_in();
            }
            if want_zoom_out {
                self.zoom_out();
            }
            if want_zoom_reset {
                self.reset_zoom();
            }

            let (alt_f, alt_t, alt_m, alt_o, ctrl_o, escape) = ctx.input(|i| {
                (
                    i.modifiers.alt && i.key_pressed(egui::Key::F),
                    i.modifiers.alt && i.key_pressed(egui::Key::T),
                    i.modifiers.alt && i.key_pressed(egui::Key::M),
                    i.modifiers.alt && i.key_pressed(egui::Key::O),
                    i.modifiers.command && i.key_pressed(egui::Key::O),
                    i.key_pressed(egui::Key::Escape),
                )
            });
            if ctrl_o {
                self.begin_open_dialog(ctx);
            }
            if alt_f {
                self.open_menu = Some(Menu::Files);
            }
            if alt_t {
                self.open_menu = Some(Menu::Tools);
            }
            if alt_m {
                self.open_menu = Some(Menu::Modes);
            }
            if alt_o {
                self.open_menu = Some(Menu::Options);
            }
            if escape {
                self.close_menu();
            }
        }

        let hovering_top = ctx.input(|i| {
            i.pointer
                .hover_pos()
                .map(|p| p.y <= HOVER_ZONE_HEIGHT)
                .unwrap_or(false)
        });
        // With no document open the bar is pinned: auto-hiding the only
        // route to Files -> Open would leave the window looking inert.
        let bar_visible = !self.auto_hide_enabled
            || hovering_top
            || self.open_menu.is_some()
            || !self.has_document();

        self.poll_pending_open(ctx);
        self.drain_results(ctx);

        let bg = if self.dark_mode {
            egui::Color32::from_gray(30)
        } else {
            egui::Color32::from_gray(235)
        };

        // The bar fades rather than popping: it appears and disappears on
        // nothing more than pointer position, and a hard cut there reads
        // as a glitch. Keep painting it until the fade has finished.
        let bar_t =
            ctx.animate_bool_with_time(egui::Id::new("bar_fade"), bar_visible, theme::T_PANEL);

        if !self.has_document() {
            self.show_empty_state(ctx, bg);
            self.show_top_bar(ctx, bar_t);
            return;
        }

        if self.text.is_some() {
            self.show_text_view(ctx, bg);
            self.show_top_bar(ctx, bar_t);
            return;
        }

        match self.view_mode {
            ViewMode::Paged => {
                self.request_primary(self.current_page);
                // Dynamic 1-page lookahead/lookbehind, recomputed fresh
                // from the ACTUAL current page every single frame --
                // never anchored to a stale position.
                if self.current_page + 1 < self.page_count {
                    self.request_window_page(self.current_page + 1);
                }
                if self.current_page > 0 {
                    self.request_window_page(self.current_page - 1);
                }
                self.show_paged_view(ctx, bg);
            }
            ViewMode::Continuous => {
                self.show_continuous_view(ctx, bg);
            }
        }

        self.show_status_pills(ctx);
        self.show_top_bar(ctx, bar_t);
        self.show_page_counter(ctx, bar_t);
    }
}

impl SenpaiApp {
    /// The two transient capsules: a render hint at bottom-left and a page
    /// counter at bottom-centre.
    ///
    /// Both are time-gated rather than bound directly to state. The render
    /// hint waits out `RENDER_HINT_DELAY` so a cache hit -- microseconds --
    /// never flashes a spinner, and the page counter lingers briefly after
    /// a turn instead of sitting on screen permanently, since in a reader
    /// it is reassurance, not chrome.
    fn show_status_pills(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);

        let pending = match self.view_mode {
            ViewMode::Paged => self.render_pending,
            ViewMode::Continuous => self.continuous_render_pending,
        };
        if pending {
            if self.render_started.is_none() {
                self.render_started = Some(now);
            }
        } else {
            self.render_started = None;
        }
        let show_hint = self
            .render_started
            .map(|t0| (now - t0) as f32 >= RENDER_HINT_DELAY)
            .unwrap_or(false);
        if pending && !show_hint {
            // Wake up once the delay has elapsed, or a render that
            // finishes nothing else would repaint for leaves the hint
            // permanently un-shown.
            ctx.request_repaint_after(std::time::Duration::from_secs_f32(RENDER_HINT_DELAY));
        }
        let hint_t =
            ctx.animate_bool_with_time(egui::Id::new("render_hint"), show_hint, theme::T_PANEL);
        theme::status_pill(
            ctx,
            "render_hint_pill",
            egui::Align2::LEFT_BOTTOM,
            egui::vec2(16.0, -16.0),
            hint_t,
            "rendering…",
        );
    }

    /// The page counter, in the one place the eye is already going to
    /// look for controls.
    ///
    /// It used to be a capsule at the bottom of the window. That put the
    /// one number a reader checks constantly in the corner nothing else
    /// lives in, so it moved up here, where the controls already are.
    ///
    /// This is the lone capsule only. With the bar up the number is
    /// drawn by the bar itself, at its right-hand end -- see
    /// `show_top_bar`.
    ///
    /// Three states between them:
    ///
    /// - bar up: part of the bar. The bar is already on screen, and a
    ///   readout costs nothing once it is;
    /// - bar hidden, a page just turned: this capsule, centred, which
    ///   leaves again after [`PAGE_FLASH`];
    /// - bar hidden, nothing happening: nothing at all. Someone reading
    ///   a page should be looking at the page, not at a number that has
    ///   not changed since they last looked.
    ///
    /// Centred rather than standing where the bar's counter stands. The
    /// two are different things: one is a readout among controls, the
    /// other is the only thing on screen, and the only thing on screen
    /// belongs in the middle of it.
    fn show_page_counter(&mut self, ctx: &egui::Context, bar_t: f32) {
        if self.page_count == 0 {
            return;
        }
        let now = ctx.input(|i| i.time);
        if self.current_page != self.last_shown_page {
            self.last_shown_page = self.current_page;
            self.page_flash_until = now + PAGE_FLASH;
        }
        let flashing = now < self.page_flash_until;
        if flashing {
            // Nothing else is necessarily animating while a page is just
            // sitting there, and without this the frame that retires the
            // capsule never arrives -- it would stay up until some
            // unrelated repaint happened to come along.
            ctx.request_repaint_after(std::time::Duration::from_secs_f32(0.1));
        }
        // Faster out than the bar is in, so the lone capsule is clearly
        // leaving rather than competing with what replaces it.
        let alone_t = ctx.animate_bool_with_time(
            egui::Id::new("page_alone"),
            bar_t <= 0.001 && flashing,
            PAGE_PILL_FADE,
        );
        // The same arrival the bar makes: tucked up against the edge at
        // 2, settling at 12.
        let slide = 2.0 + 10.0 * egui::emath::easing::cubic_out(alone_t);
        theme::page_capsule(
            ctx,
            slide,
            &format!("{} / {}", self.current_page + 1, self.page_count),
            alone_t,
        );
    }

    /// Shown when no document is open -- on launch without a path, or
    /// after a failed open. Deliberately just a prompt and a button: the
    /// reader has nothing to be fast about yet.
    fn show_empty_state(&mut self, ctx: &egui::Context, bg: egui::Color32) {
        let dim = if self.dark_mode {
            egui::Color32::from_gray(140)
        } else {
            egui::Color32::from_gray(110)
        };
        let strong = if self.dark_mode {
            egui::Color32::from_gray(205)
        } else {
            egui::Color32::from_gray(70)
        };
        // Eased in on first paint, so launching into the empty state is a
        // fade rather than a snap from blank to populated.
        let intro = ctx.animate_bool_with_time(egui::Id::new("empty_intro"), true, 0.35);

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(bg))
            .show(ctx, |ui| {
                ui.set_opacity(intro);
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.32);
                    ui.label(egui::RichText::new("senpai").color(dim).size(13.0));
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new("No document open")
                            .color(strong)
                            .size(22.0),
                    );
                    ui.add_space(20.0);

                    let waiting = self.pending_open.is_some();
                    let clicked = Pill::new(if waiting { "Choosing…" } else { "Open a PDF" })
                        .style(PillStyle::Accent)
                        .enabled(!waiting)
                        .min_width(150.0)
                        .show(ui)
                        .clicked();
                    if clicked {
                        self.begin_open_dialog(ctx);
                    }

                    ui.add_space(12.0);
                    ui.label(
                        egui::RichText::new("Drop a PDF here, or press Ctrl+O")
                            .color(dim)
                            .size(11.5),
                    );
                    if let Some(err) = &self.open_error {
                        ui.add_space(22.0);
                        ui.label(
                            egui::RichText::new(err)
                                .color(egui::Color32::from_rgb(224, 120, 120))
                                .size(11.5),
                        );
                    }
                });
            });
    }

    /// The text reader -- Markdown or plain: one scrolling, centred
    /// column.
    ///
    /// No paging, no render cache, no prefetch -- see the note at the top
    /// of `markdown.rs` for why text does not go through the page
    /// pipeline the rest of the reader is built on.
    fn show_text_view(&mut self, ctx: &egui::Context, bg: egui::Color32) {
        let zoom = (self.scale / DEFAULT_SCALE) as f32;
        let dark = self.dark_mode;
        let scroll = std::mem::take(&mut self.md_scroll);
        let Some(doc) = &self.text else {
            return;
        };
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(bg))
            .show(ctx, |ui| {
                // Measured out here, on the panel, and not inside the
                // scroll area. Inside, `available_width` is the width of
                // the *content*, which egui grows to fit whatever is in
                // it -- so one item too wide for the column widened the
                // column, which widened the paragraphs, which pushed the
                // text off the window. The panel is the window, and the
                // window is what the column should answer to.
                let viewport = ui.available_width();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let width = markdown::column_width(viewport);
                    let gutter = ((viewport - width) / 2.0).max(0.0);
                    ui.horizontal_top(|ui| {
                        // Zeroed, or egui inserts its default item gap
                        // between the spacer and the column and pushes
                        // the text that far past the right edge.
                        ui.spacing_mut().item_spacing.x = 0.0;
                        ui.add_space(gutter);
                        ui.vertical(|ui| {
                            ui.set_width(width);
                            ui.add_space(30.0 * zoom);
                            match doc {
                                TextDoc::Markdown(doc) => markdown::show(ui, doc, zoom, dark),
                                TextDoc::Plain(text) => markdown::show_plain(ui, text, zoom, dark),
                            }
                            // Room to scroll the last line clear of the
                            // bottom edge, the way a page has a margin.
                            ui.add_space(60.0 * zoom);
                        });
                    });
                    if scroll != 0.0 {
                        ui.scroll_with_delta(egui::vec2(0.0, -scroll));
                    }
                });
            });
    }

    fn show_paged_view(&mut self, ctx: &egui::Context, bg: egui::Color32) {
        let key = (self.current_page, self.scale.to_bits());
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(bg))
            .show(ctx, |ui| {
                if let Some(tex) = self.textures.get(&key) {
                    let avail = ui.available_size();
                    let tex_size = tex.size_vec2();
                    let fit_scale = (avail.x / tex_size.x).min(avail.y / tex_size.y);
                    let draw_size = tex_size * fit_scale.clamp(0.01, 1.0);
                    ui.centered_and_justified(|ui| {
                        ui.image((tex.id(), draw_size));
                    });
                }
            });
    }

    /// Continuous scroll -- KNOWN BROKEN, not the default view mode.
    /// Deferred to a follow-up; left in place so it's opt-in-only and
    /// doesn't block shipping Paged mode, which is working.
    fn show_continuous_view(&mut self, ctx: &egui::Context, bg: egui::Color32) {
        let dark = self.dark_mode;
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(bg))
            .show(ctx, |ui| {
                let avail_width = ui.available_width();

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show_viewport(ui, |ui, viewport| {
                        let spacing = 8.0;
                        let mut y = 0.0f32;
                        let mut page_tops: Vec<f32> = Vec::with_capacity(self.page_count);
                        let mut visible_pages = Vec::new();

                        for page in 0..self.page_count {
                            let h = self.estimated_row_height(page);
                            let row_top = y;
                            let row_bottom = y + h;
                            page_tops.push(row_top);

                            let is_near_viewport = row_bottom >= viewport.min.y - 400.0
                                && row_top <= viewport.max.y + 400.0;
                            if is_near_viewport {
                                visible_pages.push(page);
                            }
                            y = row_bottom + spacing;
                        }

                        ui.set_height(y);

                        let mut any_missing = false;
                        for &page in &visible_pages {
                            let h = self.estimated_row_height(page);
                            let row_top = page_tops[page];

                            ui.allocate_new_ui(
                                egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(
                                    egui::pos2(0.0, row_top),
                                    egui::vec2(avail_width, h),
                                )),
                                |ui| {
                                    let key = (page, self.scale.to_bits());
                                    if let Some(tex) = self.textures.get(&key) {
                                        let tex_size = tex.size_vec2();
                                        let fit = (avail_width / tex_size.x).clamp(0.01, 1.0);
                                        ui.centered_and_justified(|ui| {
                                            ui.image((tex.id(), tex_size * fit));
                                        });
                                    } else {
                                        any_missing = true;
                                        // A soft placeholder card, so an
                                        // un-rendered page reads as "this
                                        // page exists, pixels are coming"
                                        // rather than as a gap.
                                        let area = ui.max_rect();
                                        let card = area.shrink2(egui::vec2(
                                            (area.width() * 0.12).min(90.0),
                                            6.0,
                                        ));
                                        ui.painter().rect_filled(
                                            card,
                                            egui::Rounding::same(theme::R_CARD),
                                            if dark {
                                                egui::Color32::from_gray(44)
                                            } else {
                                                egui::Color32::from_gray(222)
                                            },
                                        );
                                        ui.painter().text(
                                            card.center(),
                                            egui::Align2::CENTER_CENTER,
                                            format!("{}", page + 1),
                                            egui::FontId::proportional(13.0),
                                            egui::Color32::from_gray(if dark { 110 } else { 150 }),
                                        );
                                    }
                                },
                            );
                        }

                        if let (Some(&first), Some(&last)) =
                            (visible_pages.first(), visible_pages.last())
                        {
                            let start = first.saturating_sub(CONTINUOUS_BUFFER_PAGES);
                            let end = (last + CONTINUOUS_BUFFER_PAGES)
                                .min(self.page_count.saturating_sub(1));
                            for page in start..=end {
                                self.request_window_page(page);
                            }
                        }

                        self.continuous_render_pending = any_missing;
                    });
            });
    }

    /// The top bar. `fade` is 0..1 from the caller's animation, so the bar
    /// can slide/fade in and out instead of popping on hover.
    fn show_top_bar(&mut self, ctx: &egui::Context, fade: f32) {
        if fade <= 0.001 {
            // Fully hidden: don't paint, and crucially don't register the
            // bar's click targets, or an invisible bar would still eat
            // clicks meant for the page.
            self.close_menu();
            self.menu_painting = None;
            self.bar_rect = egui::Rect::NOTHING;
            self.menu_rect = egui::Rect::NOTHING;
            return;
        }

        let screen_width = ctx.screen_rect().width();
        // Narrower than the window by a clear margin, and capped well
        // short of a wide monitor: a bar that runs most of the way
        // across reads as a toolbar, and the whole point of this one is
        // that it does not.
        let bar_width = (screen_width - 140.0).clamp(300.0, 720.0);

        // Slides down into place as it fades. A few pixels of travel is
        // what separates "appeared" from "arrived". Note the direction:
        // it starts tucked up against the edge (2) and settles at its
        // resting offset (12), not the other way round.
        let slide = 2.0 + 10.0 * egui::emath::easing::cubic_out(fade);

        let bar = egui::Area::new(egui::Id::new("top_bar"))
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, slide))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                ui.set_opacity(fade);
                ui.set_width(bar_width);
                theme::chrome_frame(theme::R_PILL)
                    .inner_margin(egui::Margin::symmetric(
                        theme::BAR_MARGIN_X,
                        theme::BAR_MARGIN_Y,
                    ))
                    .show(ui, |ui| {
                        ui.set_width(bar_width - 2.0 * theme::BAR_MARGIN_X);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            self.menu_button(ui, "Files", Menu::Files);
                            self.menu_button(ui, "Tools", Menu::Tools);
                            self.menu_button(ui, "Modes", Menu::Modes);
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    // Three buttons on the left, one
                                    // button and two readouts on the
                                    // right. Reading leftwards from the
                                    // edge: Options, the zoom, then the
                                    // page. Both readouts are set
                                    // quieter than the buttons, being
                                    // things to read rather than press.
                                    self.menu_button(ui, "Options", Menu::Options);
                                    if self.has_document() {
                                        ui.add_space(10.0);
                                        ui.label(
                                            egui::RichText::new(format!(
                                                "{}%",
                                                zoom_percent(self.scale)
                                            ))
                                            .color(theme::text_faint())
                                            .size(theme::COUNTER_FONT - 1.0),
                                        );
                                    }
                                    if self.page_count > 0 {
                                        ui.add_space(14.0);
                                        theme::counter_label(
                                            ui,
                                            &format!(
                                                "{} / {}",
                                                self.current_page + 1,
                                                self.page_count
                                            ),
                                            page_counter_width(ctx, self.page_count),
                                        );
                                    }
                                },
                            );
                        });
                    });
            });
        self.bar_rect = bar.response.rect;

        // Painted from `menu_painting`, not `open_menu`, so closing
        // animates symmetrically with opening instead of cutting out.
        if self.open_menu.is_some() {
            self.menu_painting = self.open_menu;
        } else {
            // Nothing open: forget the dropdown's old rect, or a click
            // where it used to be would silently count as "on the menu".
            self.menu_rect = egui::Rect::NOTHING;
        }
        let open_t = ctx.animate_bool_with_time(
            egui::Id::new("menu_fade"),
            self.open_menu.is_some(),
            theme::T_PANEL,
        );
        if open_t <= 0.001 {
            self.menu_painting = None;
        } else if let Some(menu) = self.menu_painting {
            // A menu on its way out must not take clicks: it is still
            // visible, but it is no longer the open menu.
            let closing = self.open_menu.is_none();
            self.show_menu_dropdown(ctx, menu, bar_width, open_t * fade, closing);
        }
    }

    fn menu_button(&mut self, ui: &mut egui::Ui, label: &str, kind: Menu) {
        let selected = self.open_menu == Some(kind);
        if Pill::new(label)
            .selected(selected)
            .lead()
            .show(ui)
            .clicked()
        {
            if selected {
                self.close_menu();
            } else {
                self.open_menu = Some(kind);
            }
        }
    }

    fn show_menu_dropdown(
        &mut self,
        ctx: &egui::Context,
        menu: Menu,
        bar_width: f32,
        fade: f32,
        closing: bool,
    ) {
        if fade <= 0.001 {
            return;
        }
        let slide = 62.0 + 6.0 * egui::emath::easing::cubic_out(fade);

        let dropdown = egui::Area::new(egui::Id::new("menu_dropdown"))
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, slide))
            .order(egui::Order::Foreground)
            .interactable(!closing)
            .show(ctx, |ui| {
                ui.set_opacity(fade);
                // Fixed width, set *inside* the frame: letting each menu
                // size to its own content made the card visibly resize
                // when switching between them.
                let panel_width = bar_width.min(330.0);
                theme::chrome_frame(theme::R_CARD)
                    .inner_margin(egui::Margin::symmetric(14.0, 12.0))
                    .show(ui, |ui| {
                        ui.set_width(panel_width);
                        ui.spacing_mut().item_spacing.y = 7.0;
                        match menu {
                            Menu::Files => self.menu_files(ui, ctx),
                            Menu::Tools => self.menu_tools(ui),
                            Menu::Modes => self.menu_modes(ui, ctx),
                            Menu::Options => self.menu_options(ui),
                        }
                    });
            });
        // A dropdown on its way out must stop counting as "inside the
        // menu", or a click during the fade would be swallowed.
        self.menu_rect = if closing {
            egui::Rect::NOTHING
        } else {
            dropdown.response.rect
        };
    }

    fn menu_files(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        theme::section_label(ui, "Document");
        let waiting = self.pending_open.is_some();
        let clicked = Pill::new(if waiting {
            "Dialog open…"
        } else {
            "Open…   Ctrl+O"
        })
        .style(PillStyle::Accent)
        .enabled(!waiting)
        .min_width(ui.available_width())
        .show(ui)
        .clicked();
        if clicked {
            self.begin_open_dialog(ctx);
            self.close_menu();
        }

        if let Some(path) = &self.doc_path {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.display().to_string());
            ui.add_space(2.0);
            ui.label(
                egui::RichText::new(name)
                    .color(theme::text_dim())
                    .size(11.5),
            );
            let detail = match &self.text {
                // A text file has no page count to report, so say what
                // it is instead -- the view behaves differently and the
                // menu is where that is explained.
                Some(TextDoc::Markdown(doc)) => match &doc.title {
                    Some(title) => format!("Markdown — {title}"),
                    None => "Markdown".to_owned(),
                },
                Some(TextDoc::Plain(text)) => {
                    format!("Plain text — {} lines", text.lines().count())
                }
                None => format!("{} pages", self.page_count),
            };
            ui.label(
                egui::RichText::new(elide(&detail, RECENT_NAME_CHARS))
                    .color(theme::text_faint())
                    .size(10.5),
            );
        }
        self.recent_section(ui, ctx);

        if let Some(err) = &self.open_error {
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(err)
                    .color(egui::Color32::from_rgb(224, 120, 120))
                    .size(11.0),
            );
        }
    }

    /// The point of this list: it is the way back to a document that
    /// does not involve the file dialog at all.
    fn recent_section(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let current = self.doc_path.clone();
        let entries: Vec<PathBuf> = self
            .recent
            .paths()
            .iter()
            .filter(|p| current.as_deref() != Some(p.as_path()))
            .take(RECENT_IN_MENU)
            .cloned()
            .collect();
        if entries.is_empty() {
            return;
        }

        ui.add_space(4.0);
        theme::divider(ui);
        theme::section_label(ui, "Recent");

        // The click is applied after the loop: opening a document borrows
        // all of `self`, and the loop is reading `self.recent`.
        let mut chosen = None;
        for path in entries {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.display().to_string());
            let hit = Pill::new(&elide(&name, RECENT_NAME_CHARS))
                .style(PillStyle::Ghost)
                .small()
                .align_left()
                .min_width(ui.available_width())
                .show(ui)
                // The full path, because two documents can share a file
                // name and the elided label can hide which is which.
                .on_hover_text(path.display().to_string());
            if hit.clicked() {
                chosen = Some(path);
            }
        }
        if let Some(path) = chosen {
            self.open_document(ctx, &path);
            self.close_menu();
        }
    }

    fn menu_tools(&mut self, ui: &mut egui::Ui) {
        theme::section_label(ui, "Annotation");
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
            for label in ["Highlight", "Note", "Draw"] {
                Pill::new(label).enabled(false).show(ui);
            }
        });
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new("Not built yet — Phase 3 in the roadmap.")
                .color(theme::text_faint())
                .size(10.5),
        );
    }

    fn menu_modes(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        theme::section_label(ui, "Theme");
        let dark = self.dark_mode;
        if let Some(i) = theme::segmented(ui, "theme_seg", &["Light", "Night"], dark as usize) {
            self.set_dark(ctx, i == 1);
        }

        theme::divider(ui);
        theme::section_label(ui, "View mode");
        if self.text.is_some() {
            // Offering a switch that does nothing is worse than not
            // offering it: Markdown reflows to the window by nature, so
            // neither mode has anything to apply to.
            ui.label(
                egui::RichText::new("Text reflows to the window. View modes apply to PDFs.")
                    .color(theme::text_faint())
                    .size(10.5),
            );
        } else {
            let current = (self.view_mode == ViewMode::Continuous) as usize;
            if let Some(i) = theme::segmented(ui, "view_seg", &["Paged", "Continuous"], current) {
                self.set_view_mode(if i == 1 {
                    ViewMode::Continuous
                } else {
                    ViewMode::Paged
                });
            }
            if self.view_mode == ViewMode::Continuous {
                ui.label(
                    egui::RichText::new("Experimental — known broken.")
                        .color(egui::Color32::from_rgb(224, 170, 110))
                        .size(10.5),
                );
            }
        }
    }

    fn rebind_row(&mut self, ui: &mut egui::Ui, target: NavTarget) {
        let listening = self.rebinding == Some(target);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(target.label())
                    .color(theme::text())
                    .size(12.5),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let caption = match (listening, self.nav_keys.get(target)) {
                    (true, _) => "press a key…".to_owned(),
                    (false, Some(key)) => key.name().to_owned(),
                    // Not "none": nothing is unbound here, the built-in
                    // keys are simply all there is.
                    (false, None) => "—".to_owned(),
                };
                let style = if listening {
                    PillStyle::Selected
                } else {
                    PillStyle::Quiet
                };
                if Pill::new(&caption)
                    .style(style)
                    .small()
                    .min_width(104.0)
                    .show(ui)
                    .clicked()
                {
                    self.rebind_rejected = None;
                    self.rebinding = if listening { None } else { Some(target) };
                }
            });
        });
    }

    fn menu_options(&mut self, ui: &mut egui::Ui) {
        theme::section_label(ui, "Top bar");
        let mut auto_hide = self.auto_hide_enabled;
        if theme::switch_row(ui, "Auto-hide", &mut auto_hide).changed() {
            self.auto_hide_enabled = auto_hide;
        }
        ui.label(
            egui::RichText::new("Reveal by hovering the top edge, or Alt+F / T / M / O.")
                .color(theme::text_faint())
                .size(10.5),
        );

        theme::divider(ui);
        theme::section_label(ui, "Navigation keys");
        ui.label(
            egui::RichText::new(
                "Arrows, Page Up/Down and Space always work. Anything set here is extra.",
            )
            .color(theme::text_faint())
            .size(10.5),
        );
        ui.add_space(3.0);
        for target in NavTarget::ALL {
            self.rebind_row(ui, target);
        }

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 5.0;
            for preset in [Preset::Vim, Preset::Gamer] {
                if Pill::new(preset.label())
                    .selected(self.nav_keys.matches(preset))
                    .small()
                    .show(ui)
                    .clicked()
                {
                    self.nav_keys.apply(preset);
                    self.nav_keys.save();
                    self.rebinding = None;
                    self.rebind_rejected = None;
                }
            }
            if Pill::new("Clear")
                .style(PillStyle::Quiet)
                .small()
                .show(ui)
                .clicked()
            {
                self.nav_keys.clear();
                self.nav_keys.save();
                self.rebinding = None;
                self.rebind_rejected = None;
            }
        });

        if self.rebinding.is_some() {
            ui.label(
                egui::RichText::new("Press any key, or Escape to cancel.")
                    .color(theme::text_faint())
                    .size(10.5),
            );
        } else if let Some(key) = self.rebind_rejected {
            ui.label(
                egui::RichText::new(format!("{} is always bound — pick another.", key.name()))
                    .color(egui::Color32::from_rgb(224, 170, 110))
                    .size(10.5),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        configure_context, elide, is_markdown, is_plain_text, pick_dropped_document,
        should_dismiss_menu, zoom_percent, DEFAULT_SCALE, RECENT_NAME_CHARS,
    };
    use eframe::egui::{self, pos2, Rect};
    use std::path::{Path, PathBuf};

    fn bar() -> Rect {
        Rect::from_min_max(pos2(50.0, 12.0), pos2(950.0, 60.0))
    }
    fn menu() -> Rect {
        Rect::from_min_max(pos2(320.0, 68.0), pos2(680.0, 300.0))
    }

    #[test]
    fn click_on_the_page_dismisses() {
        assert!(should_dismiss_menu(
            true,
            Some(pos2(500.0, 500.0)),
            bar(),
            menu()
        ));
    }

    /// The bar is excluded so a menu button keeps its own toggle, and so
    /// switching straight to another menu works. Dismissing here would
    /// close the menu and let the button immediately reopen it.
    #[test]
    fn click_on_the_bar_does_not_dismiss() {
        assert!(!should_dismiss_menu(
            true,
            Some(pos2(90.0, 34.0)),
            bar(),
            menu()
        ));
    }

    #[test]
    fn click_inside_the_dropdown_does_not_dismiss() {
        assert!(!should_dismiss_menu(
            true,
            Some(pos2(500.0, 150.0)),
            bar(),
            menu()
        ));
    }

    #[test]
    fn nothing_happens_without_a_press() {
        assert!(!should_dismiss_menu(true, None, bar(), menu()));
    }

    #[test]
    fn no_open_menu_means_nothing_to_dismiss() {
        assert!(!should_dismiss_menu(
            false,
            Some(pos2(500.0, 500.0)),
            bar(),
            menu()
        ));
    }

    /// Guards the reset paths: with the rects cleared, any press counts as
    /// outside, so a stale rect can never swallow a dismissing click.
    #[test]
    fn cleared_rects_still_dismiss() {
        assert!(should_dismiss_menu(
            true,
            Some(pos2(500.0, 150.0)),
            Rect::NOTHING,
            Rect::NOTHING
        ));
    }

    #[test]
    fn elide_leaves_a_short_name_alone() {
        assert_eq!(elide("notes.pdf", 34), "notes.pdf");
        // Exactly at the budget is still untouched.
        assert_eq!(elide("abcdefghij", 10), "abcdefghij");
    }

    #[test]
    fn elide_keeps_the_budget_and_both_ends() {
        let name = "Operating System Concepts, Tenth Edition.pdf";
        let out = elide(name, RECENT_NAME_CHARS);
        assert_eq!(out.chars().count(), RECENT_NAME_CHARS);
        assert!(out.starts_with("Operating"));
        assert!(out.ends_with(".pdf"));
        assert!(out.contains('\u{2026}'));
    }

    /// The label is sliced by character, not by byte: a non-ASCII name
    /// must not panic on a multi-byte boundary.
    #[test]
    fn elide_handles_multibyte_names() {
        let name = "\u{e9}tude sur les syst\u{e8}mes d'exploitation modernes.pdf";
        let out = elide(name, 20);
        assert_eq!(out.chars().count(), 20);
    }

    #[test]
    fn elide_falls_back_to_truncation_when_there_is_no_middle() {
        assert_eq!(elide("abcdefg", 3), "abc");
    }

    fn paths(items: &[&str]) -> Vec<PathBuf> {
        items.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn nothing_dropped_opens_nothing() {
        assert_eq!(pick_dropped_document(&[]), None);
    }

    #[test]
    fn a_pdf_wins_over_whatever_else_came_with_it() {
        let dropped = paths(&["/notes.txt", "/book.pdf", "/other.pdf"]);
        assert_eq!(
            pick_dropped_document(&dropped),
            Some(Path::new("/book.pdf"))
        );
    }

    /// Dragging from a file manager can produce an upper-case extension,
    /// and on Windows the case carries no meaning at all.
    #[test]
    fn the_extension_match_ignores_case() {
        let dropped = paths(&["/notes.txt", "/BOOK.PDF"]);
        assert_eq!(
            pick_dropped_document(&dropped),
            Some(Path::new("/BOOK.PDF"))
        );
    }

    /// Not a refusal: the loader gets to say what is wrong with it, by
    /// name. An extensionless PDF opens by this path too.
    #[test]
    fn without_a_known_type_the_first_file_is_tried_anyway() {
        let dropped = paths(&["/scan", "/notes.dat"]);
        assert_eq!(pick_dropped_document(&dropped), Some(Path::new("/scan")));
    }

    /// Whichever order the file manager hands them over in.
    #[test]
    fn the_known_kinds_are_preferred_in_order() {
        let all = ["/a.txt", "/b.md", "/c.pdf"];
        for rotation in 0..all.len() {
            let mut names = all;
            names.rotate_left(rotation);
            let dropped = paths(&names);
            assert_eq!(
                pick_dropped_document(&dropped),
                Some(Path::new("/c.pdf")),
                "order {names:?}"
            );
        }
        // With no PDF among them, Markdown comes before plain text.
        let dropped = paths(&["/a.txt", "/b.md"]);
        assert_eq!(pick_dropped_document(&dropped), Some(Path::new("/b.md")));
    }

    #[test]
    fn a_dropped_markdown_file_is_opened_too() {
        let dropped = paths(&["/notes.txt", "/README.md"]);
        assert_eq!(
            pick_dropped_document(&dropped),
            Some(Path::new("/README.md"))
        );
    }

    /// Pins the collision this removed: egui ships a whole-interface
    /// zoom on the same keys, so Ctrl and a plus key scaled the document
    /// *and* grew the top bar around it.
    #[test]
    fn egui_s_own_zoom_shortcuts_are_handed_over_to_the_document() {
        let ctx = egui::Context::default();
        assert!(
            ctx.options(|o| o.zoom_with_keyboard),
            "egui has this on by default -- if that changes, so does the reason for this"
        );
        configure_context(&ctx);
        assert!(!ctx.options(|o| o.zoom_with_keyboard));
    }

    #[test]
    fn a_freshly_opened_document_reads_as_one_hundred_percent() {
        assert_eq!(zoom_percent(DEFAULT_SCALE), 100);
    }

    #[test]
    fn zoom_is_reported_relative_to_that_rather_than_as_the_raw_scale() {
        assert_eq!(zoom_percent(DEFAULT_SCALE * 2.0), 200);
        assert_eq!(zoom_percent(DEFAULT_SCALE * 0.5), 50);
        // The 1.2x step the zoom keys take, rounded to whole percent.
        assert_eq!(zoom_percent(DEFAULT_SCALE * 1.2), 120);
        assert_eq!(zoom_percent(DEFAULT_SCALE * 1.2 * 1.2), 144);
    }

    #[test]
    fn markdown_is_recognised_by_its_usual_extensions_in_any_case() {
        for name in ["a.md", "a.MD", "a.markdown", "a.mdown", "a.mkd"] {
            assert!(is_markdown(Path::new(name)), "{name} should be markdown");
        }
        for name in ["a.pdf", "a.txt", "a", "a.mdx"] {
            assert!(!is_markdown(Path::new(name)), "{name} should not be");
        }
    }

    #[test]
    fn plain_text_is_recognised_by_its_extension_in_any_case() {
        for name in ["notes.txt", "NOTES.TXT", "a.text"] {
            assert!(
                is_plain_text(Path::new(name)),
                "{name} should be plain text"
            );
        }
        for name in ["a.md", "a.pdf", "a", "a.texts"] {
            assert!(!is_plain_text(Path::new(name)), "{name} should not be");
        }
    }

    /// Plain text is not Markdown: a `.txt` whose author wrote a row of
    /// dashes meant dashes, so the two must not be confused by extension.
    #[test]
    fn the_two_text_kinds_do_not_overlap() {
        for name in ["a.md", "a.markdown", "a.txt", "a.text"] {
            let p = Path::new(name);
            assert!(
                is_markdown(p) != is_plain_text(p),
                "{name} should be exactly one of the two"
            );
        }
    }

    #[test]
    fn a_dropped_text_file_is_opened_too() {
        let dropped = paths(&["/image.png", "/notes.txt"]);
        assert_eq!(
            pick_dropped_document(&dropped),
            Some(Path::new("/notes.txt"))
        );
    }
}
