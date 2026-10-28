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
//! Known broken, NOT default (Paged is the startup mode): Continuous
//! Scroll mode. Deferred to after this release.

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
const CONTINUOUS_BUFFER_PAGES: usize = 1;

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

    /// Result channel for an in-flight file dialog. The dialog itself
    /// runs on its own thread -- blocking the UI thread on it would stall
    /// painting for as long as the user browses, which the performance
    /// budget rules out.
    pending_open: Option<Receiver<Option<PathBuf>>>,
    open_error: Option<String>,
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
            pending_open: None,
            open_error: None,
        };
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
        let start_dir = self
            .doc_path
            .as_ref()
            .and_then(|p| p.parent().map(Path::to_path_buf));
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
        ctx.set_visuals(if dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });
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
                self.open_menu = None;
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

        if self.page_count == 0 {
            self.show_empty_state(ctx, bg);
            if bar_visible {
                self.show_top_bar(ctx);
            }
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

        if bar_visible {
            self.show_top_bar(ctx);
        }
    }
}

impl SenpaiApp {
    /// Shown when no document is open -- on launch without a path, or
    /// after a failed open. Deliberately just a prompt and a button: the
    /// reader has nothing to be fast about yet.
    fn show_empty_state(&mut self, ctx: &egui::Context, bg: egui::Color32) {
        let dim = if self.dark_mode {
            egui::Color32::from_gray(140)
        } else {
            egui::Color32::from_gray(110)
        };
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(bg))
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() * 0.35);
                    ui.label(
                        egui::RichText::new("no document open")
                            .color(dim)
                            .size(20.0),
                    );
                    ui.add_space(12.0);
                    let waiting = self.pending_open.is_some();
                    let label = if waiting {
                        "choosing…"
                    } else {
                        "Open a PDF…"
                    };
                    if ui.add_enabled(!waiting, egui::Button::new(label)).clicked() {
                        self.begin_open_dialog(ctx);
                    }
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new("Ctrl+O, or Files → Open")
                            .color(dim)
                            .small(),
                    );
                    if let Some(err) = &self.open_error {
                        ui.add_space(18.0);
                        ui.label(
                            egui::RichText::new(err)
                                .color(egui::Color32::from_rgb(220, 110, 110))
                                .small(),
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

                if self.render_pending {
                    ui.put(
                        egui::Rect::from_min_size(
                            egui::pos2(12.0, ui.clip_rect().height() - 28.0),
                            egui::vec2(120.0, 20.0),
                        ),
                        egui::Label::new(
                            egui::RichText::new("rendering…")
                                .color(egui::Color32::from_gray(160))
                                .small(),
                        ),
                    );
                }
            });
    }

    /// Continuous scroll -- KNOWN BROKEN, not the default view mode.
    /// Deferred to a follow-up; left in place so it's opt-in-only and
    /// doesn't block shipping Paged mode, which is working.
    fn show_continuous_view(&mut self, ctx: &egui::Context, bg: egui::Color32) {
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
                                        ui.centered_and_justified(|ui| {
                                            ui.label(
                                                egui::RichText::new(format!("page {}", page + 1))
                                                    .color(egui::Color32::from_gray(100)),
                                            );
                                        });
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

                if self.continuous_render_pending {
                    ui.put(
                        egui::Rect::from_min_size(
                            egui::pos2(12.0, ui.clip_rect().height() - 28.0),
                            egui::vec2(120.0, 20.0),
                        ),
                        egui::Label::new(
                            egui::RichText::new("rendering…")
                                .color(egui::Color32::from_gray(160))
                                .small(),
                        ),
                    );
                }
            });
    }

    fn show_top_bar(&mut self, ctx: &egui::Context) {
        let bar_frame = egui::Frame::none()
            .fill(egui::Color32::from_black_alpha(230))
            .rounding(egui::Rounding::same(18.0))
            .inner_margin(egui::Margin::symmetric(20.0, 10.0));

        let screen_width = ctx.screen_rect().width();
        let bar_width = (screen_width - 80.0).clamp(280.0, 900.0);

        egui::Area::new(egui::Id::new("top_bar"))
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 12.0))
            .show(ctx, |ui| {
                ui.set_width(bar_width);
                bar_frame.show(ui, |ui| {
                    ui.set_width(bar_width - 40.0);
                    ui.horizontal(|ui| {
                        self.menu_button(ui, "Files", Menu::Files);
                        self.menu_button(ui, "Tools", Menu::Tools);
                        self.menu_button(ui, "Modes", Menu::Modes);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            self.menu_button(ui, "Options", Menu::Options);
                        });
                    });
                });
            });

        if let Some(menu) = self.open_menu {
            self.show_menu_dropdown(ctx, menu, bar_width);
        }
    }

    fn menu_button(&mut self, ui: &mut egui::Ui, label: &str, kind: Menu) {
        let selected = self.open_menu == Some(kind);
        let text = egui::RichText::new(label).color(egui::Color32::WHITE);
        let button = egui::Button::new(text).fill(if selected {
            egui::Color32::from_white_alpha(30)
        } else {
            egui::Color32::TRANSPARENT
        });
        if ui.add(button).clicked() {
            self.open_menu = if selected { None } else { Some(kind) };
        }
    }

    fn show_menu_dropdown(&mut self, ctx: &egui::Context, menu: Menu, bar_width: f32) {
        let frame = egui::Frame::none()
            .fill(egui::Color32::from_black_alpha(235))
            .rounding(egui::Rounding::same(14.0))
            .inner_margin(egui::Margin::symmetric(16.0, 12.0));

        egui::Area::new(egui::Id::new("menu_dropdown"))
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 68.0))
            .show(ctx, |ui| {
                ui.set_width(bar_width.min(340.0));
                frame.show(ui, |ui| match menu {
                    Menu::Files => {
                        let waiting = self.pending_open.is_some();
                        let label = if waiting {
                            "Open... (dialog open)"
                        } else {
                            "Open...   Ctrl+O"
                        };
                        if ui
                            .add_enabled(
                                !waiting,
                                egui::Button::new(
                                    egui::RichText::new(label).color(egui::Color32::WHITE),
                                ),
                            )
                            .clicked()
                        {
                            self.begin_open_dialog(ctx);
                            self.open_menu = None;
                        }
                        if let Some(path) = &self.doc_path {
                            ui.add_space(8.0);
                            ui.label(
                                egui::RichText::new(path.display().to_string())
                                    .color(egui::Color32::GRAY)
                                    .small(),
                            );
                        }
                        if let Some(err) = &self.open_error {
                            ui.add_space(8.0);
                            ui.label(
                                egui::RichText::new(err)
                                    .color(egui::Color32::from_rgb(220, 110, 110))
                                    .small(),
                            );
                        }
                    }
                    Menu::Tools => {
                        ui.label(
                            egui::RichText::new("Annotation tools (coming soon)")
                                .color(egui::Color32::GRAY),
                        );
                        ui.add_enabled(
                            false,
                            egui::Button::new(
                                egui::RichText::new("Highlight").color(egui::Color32::GRAY),
                            ),
                        );
                        ui.add_enabled(
                            false,
                            egui::Button::new(
                                egui::RichText::new("Note").color(egui::Color32::GRAY),
                            ),
                        );
                        ui.add_enabled(
                            false,
                            egui::Button::new(
                                egui::RichText::new("Draw").color(egui::Color32::GRAY),
                            ),
                        );
                    }
                    Menu::Modes => {
                        ui.label(
                            egui::RichText::new("Theme")
                                .color(egui::Color32::GRAY)
                                .small(),
                        );
                        ui.horizontal(|ui| {
                            if ui
                                .add(egui::Button::new(
                                    egui::RichText::new("Light").color(egui::Color32::WHITE),
                                ))
                                .clicked()
                            {
                                self.set_dark(ctx, false);
                            }
                            if ui
                                .add(egui::Button::new(
                                    egui::RichText::new("Night").color(egui::Color32::WHITE),
                                ))
                                .clicked()
                            {
                                self.set_dark(ctx, true);
                            }
                        });

                        ui.add_space(10.0);
                        ui.label(
                            egui::RichText::new("View mode")
                                .color(egui::Color32::GRAY)
                                .small(),
                        );
                        ui.horizontal(|ui| {
                            let paged_selected = self.view_mode == ViewMode::Paged;
                            let paged_btn = egui::Button::new(
                                egui::RichText::new("Paged").color(egui::Color32::WHITE),
                            )
                            .fill(if paged_selected {
                                egui::Color32::from_white_alpha(30)
                            } else {
                                egui::Color32::TRANSPARENT
                            });
                            if ui.add(paged_btn).clicked() {
                                self.set_view_mode(ViewMode::Paged);
                            }

                            let cont_selected = self.view_mode == ViewMode::Continuous;
                            let cont_btn = egui::Button::new(
                                egui::RichText::new("Continuous Scroll (experimental)")
                                    .color(egui::Color32::WHITE),
                            )
                            .fill(if cont_selected {
                                egui::Color32::from_white_alpha(30)
                            } else {
                                egui::Color32::TRANSPARENT
                            });
                            if ui.add(cont_btn).clicked() {
                                self.set_view_mode(ViewMode::Continuous);
                            }
                        });

                        ui.add_space(10.0);
                        ui.label(
                            egui::RichText::new("Page navigation keys (Paged mode)")
                                .color(egui::Color32::GRAY)
                                .small(),
                        );
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(format!("Prev: {:?}", self.nav_keys.prev))
                                    .color(egui::Color32::WHITE),
                            );
                            let rebinding_prev = self.rebinding == Some(NavTarget::Prev);
                            let label = if rebinding_prev {
                                "press any key…"
                            } else {
                                "Rebind"
                            };
                            if ui.button(label).clicked() {
                                self.rebinding = Some(NavTarget::Prev);
                            }
                        });
                        ui.horizontal(|ui| {
                            ui.label(
                                egui::RichText::new(format!("Next: {:?}", self.nav_keys.next))
                                    .color(egui::Color32::WHITE),
                            );
                            let rebinding_next = self.rebinding == Some(NavTarget::Next);
                            let label = if rebinding_next {
                                "press any key…"
                            } else {
                                "Rebind"
                            };
                            if ui.button(label).clicked() {
                                self.rebinding = Some(NavTarget::Next);
                            }
                        });
                        if self.rebinding.is_some() {
                            ui.label(
                                egui::RichText::new("Press Escape to cancel")
                                    .color(egui::Color32::from_gray(150))
                                    .small(),
                            );
                        }
                    }
                    Menu::Options => {
                        let mut auto_hide = self.auto_hide_enabled;
                        if ui.checkbox(&mut auto_hide, "Enable auto-hide").changed() {
                            self.auto_hide_enabled = auto_hide;
                        }
                    }
                });
            });
    }
}
