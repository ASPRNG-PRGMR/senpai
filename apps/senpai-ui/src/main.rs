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

mod recent;
mod theme;

use eframe::egui;
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum NavTarget {
    Prev,
    Next,
}

struct NavKeys {
    prev: egui::Key,
    next: egui::Key,
}

impl Default for NavKeys {
    fn default() -> Self {
        Self {
            prev: egui::Key::ArrowLeft,
            next: egui::Key::ArrowRight,
        }
    }
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
const RECENT_IN_MENU: usize = 6;
/// Budget for an entry's label, in characters. The dropdown's pills do
/// not wrap or truncate on their own -- a long name would simply push
/// the pill wider than the card.
const RECENT_NAME_CHARS: usize = 34;

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

    /// When the current render request started, for `RENDER_HINT_DELAY`.
    render_started: Option<f64>,
    /// Page the counter pill last showed, so it can re-fade on a change.
    last_shown_page: usize,
    page_pill_until: f64,
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
            scale: 1.5,
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
            nav_keys: NavKeys::default(),
            rebinding: None,
            open_menu: None,
            bar_rect: egui::Rect::NOTHING,
            menu_rect: egui::Rect::NOTHING,
            menu_painting: None,
            pending_open: None,
            open_error: None,
            recent: recent::Recent::load(),
            render_started: None,
            last_shown_page: 0,
            page_pill_until: 0.0,
        };
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
                .set_title("Open a PDF")
                .add_filter("PDF document", &["pdf"]);
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

    fn open_document(&mut self, ctx: &egui::Context, path: &Path) {
        match load_document(path) {
            Ok((engine, page_count, size)) => {
                self.install_document(ctx, path, engine, page_count, size);
                self.open_error = None;
                self.recent.record(path);
            }
            Err(e) => {
                self.open_error = Some(format!("{}: {e}", path.display()));
            }
        }
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
        // Forces the counter pill to flash once on open, so the page
        // count is visible without having to turn a page first.
        self.last_shown_page = usize::MAX;
        self.estimated_page_size = estimated_page_size;
        self.known_page_heights = vec![None; page_count];
        self.next_generation = 0;
        self.requested_primary = None;
        self.render_pending = false;
        self.continuous_render_pending = false;

        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("{name} — senpai")));
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
                match target {
                    NavTarget::Prev => self.nav_keys.prev = key,
                    NavTarget::Next => self.nav_keys.next = key,
                }
                self.rebinding = None;
            }
        } else {
            let (want_next, want_prev, want_zoom_in, want_zoom_out) = ctx.input(|i| {
                (
                    i.key_pressed(self.nav_keys.next) || i.key_pressed(egui::Key::Space),
                    i.key_pressed(self.nav_keys.prev),
                    i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals),
                    i.key_pressed(egui::Key::Minus),
                )
            });
            if self.view_mode == ViewMode::Paged {
                if want_next {
                    self.next_page();
                }
                if want_prev {
                    self.prev_page();
                }
            }
            if want_zoom_in {
                self.zoom_in();
            }
            if want_zoom_out {
                self.zoom_out();
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
            || self.page_count == 0;

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

        if self.page_count == 0 {
            self.show_empty_state(ctx, bg);
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

        if self.current_page != self.last_shown_page {
            self.last_shown_page = self.current_page;
            self.page_pill_until = now + 1.4;
        }
        let page_visible = now < self.page_pill_until;
        if page_visible {
            ctx.request_repaint_after(std::time::Duration::from_secs_f32(0.1));
        }
        let page_t =
            ctx.animate_bool_with_time(egui::Id::new("page_pill"), page_visible, theme::T_PANEL);
        theme::status_pill(
            ctx,
            "page_counter_pill",
            egui::Align2::CENTER_BOTTOM,
            egui::vec2(0.0, -16.0),
            page_t,
            &format!("{} / {}", self.current_page + 1, self.page_count),
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
                        egui::RichText::new("Ctrl+O, or the Files menu")
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
        let bar_width = (screen_width - 80.0).clamp(280.0, 900.0);

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
                    .inner_margin(egui::Margin::symmetric(10.0, 7.0))
                    .show(ui, |ui| {
                        ui.set_width(bar_width - 20.0);
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            self.menu_button(ui, "Files", Menu::Files);
                            self.menu_button(ui, "Tools", Menu::Tools);
                            self.menu_button(ui, "Modes", Menu::Modes);
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    self.menu_button(ui, "Options", Menu::Options);
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
        if Pill::new(label).selected(selected).show(ui).clicked() {
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
            ui.label(egui::RichText::new(name).color(theme::TEXT_DIM).size(11.5));
            ui.label(
                egui::RichText::new(format!("{} pages", self.page_count))
                    .color(theme::TEXT_FAINT)
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
                .color(theme::TEXT_FAINT)
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

        theme::divider(ui);
        theme::section_label(ui, "Page navigation keys");
        self.rebind_row(ui, "Previous", NavTarget::Prev);
        self.rebind_row(ui, "Next", NavTarget::Next);
        if self.rebinding.is_some() {
            ui.label(
                egui::RichText::new("Press any key, or Escape to cancel.")
                    .color(theme::TEXT_FAINT)
                    .size(10.5),
            );
        }
    }

    fn rebind_row(&mut self, ui: &mut egui::Ui, label: &str, target: NavTarget) {
        let key = match target {
            NavTarget::Prev => self.nav_keys.prev,
            NavTarget::Next => self.nav_keys.next,
        };
        let listening = self.rebinding == Some(target);
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(label).color(theme::TEXT).size(12.5));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let caption = if listening {
                    "press a key…".to_string()
                } else {
                    format!("{key:?}")
                };
                let style = if listening {
                    PillStyle::Selected
                } else {
                    PillStyle::Quiet
                };
                if Pill::new(&caption)
                    .style(style)
                    .small()
                    .min_width(98.0)
                    .show(ui)
                    .clicked()
                {
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
                .color(theme::TEXT_FAINT)
                .size(10.5),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{elide, should_dismiss_menu, RECENT_NAME_CHARS};
    use eframe::egui::{pos2, Rect};

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
}
