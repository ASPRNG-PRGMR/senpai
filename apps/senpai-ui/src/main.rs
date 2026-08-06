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
use std::sync::mpsc::{self, Receiver, Sender};
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
    let Some(path) = args.next() else {
        eprintln!("usage: senpai <path-to-pdf>");
        std::process::exit(1);
    };

    let file = LoadedFile::open(&path).unwrap_or_else(|e| {
        eprintln!("failed to open {path}: {e}");
        std::process::exit(1);
    });
    let engine = PopplerEngine::open(file.as_bytes()).unwrap_or_else(|e| {
        eprintln!("failed to parse {path}: {e}");
        std::process::exit(1);
    });
    drop(file);

    let page_count = engine.metadata().page_count;
    if page_count == 0 {
        eprintln!("document has no pages");
        std::process::exit(1);
    }

    let estimated_page_size = engine.page_size(PageHandle(0)).unwrap_or(PageSize {
        width_pt: 612.0,
        height_pt: 792.0,
    });

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
        Box::new(move |cc| {
            Ok(Box::new(SenpaiApp::new(
                engine,
                page_count,
                estimated_page_size,
                cc,
            )))
        }),
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
                    let outcome = render_or_cache_hit(
                        &engine,
                        &mut cache,
                        key,
                        request.page,
                        request.scale,
                    );

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
    page_count: usize,
    current_page: usize,
    scale: f64,
    estimated_page_size: PageSize,
    known_page_heights: Vec<Option<f32>>,

    shared: Shared,
    result_rx: Receiver<RenderResult>,
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
}

impl SenpaiApp {
    fn new(
        engine: PopplerEngine,
        page_count: usize,
        estimated_page_size: PageSize,
        cc: &eframe::CreationContext<'_>,
    ) -> Self {
        let engine = Arc::new(engine);
        let shared: Shared = Arc::new((
            Mutex::new(WorkerState {
                primary: None,
                prefetch: VecDeque::new(),
            }),
            Condvar::new(),
        ));
        let (tx, rx) = mpsc::channel();

        spawn_render_worker(engine, shared.clone(), tx, cc.egui_ctx.clone());

        Self {
            page_count,
            current_page: 0,
            scale: 1.5,
            estimated_page_size,
            known_page_heights: vec![None; page_count],
            shared,
            result_rx: rx,
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
        }
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
            let (lock, cvar) = &*self.shared;
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
        if page >= self.page_count {
            return;
        }
        let key = (page, self.scale.to_bits());
        if self.textures.contains(&key) {
            return;
        }
        let (lock, cvar) = &*self.shared;
        let mut guard = lock.lock().unwrap();
        push_prefetch_dedup(&mut guard, page, self.scale);
        cvar.notify_one();
    }

    fn clear_pending_prefetch(&mut self) {
        let (lock, _cvar) = &*self.shared;
        lock.lock().unwrap().prefetch.clear();
    }

    fn drain_results(&mut self, ctx: &egui::Context) {
        while let Ok(result) = self.result_rx.try_recv() {
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
                        self.known_page_heights[result.page.min(self.page_count - 1)] =
                            Some(rendered.height_px as f32);
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

            let (alt_f, alt_t, alt_m, alt_o, escape) = ctx.input(|i| {
                (
                    i.modifiers.alt && i.key_pressed(egui::Key::F),
                    i.modifiers.alt && i.key_pressed(egui::Key::T),
                    i.modifiers.alt && i.key_pressed(egui::Key::M),
                    i.modifiers.alt && i.key_pressed(egui::Key::O),
                    i.key_pressed(egui::Key::Escape),
                )
            });
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
        let bar_visible = !self.auto_hide_enabled || hovering_top || self.open_menu.is_some();

        self.drain_results(ctx);

        let bg = if self.dark_mode {
            egui::Color32::from_gray(30)
        } else {
            egui::Color32::from_gray(235)
        };

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
    fn show_paged_view(&mut self, ctx: &egui::Context, bg: egui::Color32) {
        let key = (self.current_page, self.scale.to_bits());
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(bg))
            .show(ctx, |ui| {
                if let Some(tex) = self.textures.get(&key) {
                    let avail = ui.available_size();
                    let tex_size = tex.size_vec2();
                    let fit_scale = (avail.x / tex_size.x).min(avail.y / tex_size.y);
                    let draw_size = tex_size * fit_scale.min(1.0).max(0.01);
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
                                        let fit = (avail_width / tex_size.x).min(1.0).max(0.01);
                                        ui.centered_and_justified(|ui| {
                                            ui.image((tex.id(), tex_size * fit));
                                        });
                                    } else {
                                        any_missing = true;
                                        ui.centered_and_justified(|ui| {
                                            ui.label(
                                                egui::RichText::new(format!(
                                                    "page {}",
                                                    page + 1
                                                ))
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
        let bar_width = (screen_width - 80.0).min(900.0).max(280.0);

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
                        if ui
                            .add(egui::Button::new(
                                egui::RichText::new("Open...").color(egui::Color32::WHITE),
                            ))
                            .clicked()
                        {
                            eprintln!("Open... clicked (file picker not wired yet)");
                            self.open_menu = None;
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
                        ui.label(egui::RichText::new("Theme").color(egui::Color32::GRAY).small());
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
                            egui::RichText::new("View mode").color(egui::Color32::GRAY).small(),
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
                            let label = if rebinding_prev { "press any key…" } else { "Rebind" };
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
                            let label = if rebinding_next { "press any key…" } else { "Rebind" };
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
