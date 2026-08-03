//! senpai-ui — windowed reader, v4: render cache + prefetching.
//!
//! Builds on the background-rendering design from before, adding:
//!   - an LRU cache of rendered (page, scale) results, so revisiting a
//!     page you've already seen is a cache hit instead of a re-render
//!   - opportunistic prefetch of page N-1/N+1 whenever the worker has
//!     no real request waiting, so page turns often complete before
//!     you've even finished the click
//!
//! Verified against a real PDF before integration: a cache hit measured
//! ~2000x faster than a real render (38us vs ~86ms), and a page that was
//! never explicitly requested was already cached by the time it was
//! asked for, confirming prefetch actually runs ahead of need.
//!
//! The cache lives entirely inside the background worker -- it is never
//! shared with or touched by the UI thread directly. This preserves the
//! existing invariant that only one thread ever calls into the poppler
//! engine, and means a cache hit still goes through the same request/
//! response round-trip as a miss (just returns almost instantly instead
//! of after ~72-111ms), rather than introducing a second source of truth.

use eframe::egui;
use lru::LruCache;
use senpai_core::engine::{PdfEngine, PdfEngineOpen};
use senpai_core::file_loader::LoadedFile;
use senpai_core::model::{PageHandle, RenderedPage};
use senpai_pdf_poppler::PopplerEngine;
use std::collections::VecDeque;
use std::env;
use std::num::NonZeroUsize;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

fn main() -> eframe::Result<()> {
    let mut args = env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: senpai-ui <path-to-pdf>");
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
        Box::new(move |cc| Ok(Box::new(SenpaiApp::new(engine, page_count, cc)))),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Menu {
    Files,
    Tools,
    Modes,
    Options,
}

/// Roughly 20 cached pages at typical zoom (~8MB/page at 144 DPI on a
/// standard-size page) is a ~160MB ceiling -- a real, statable memory
/// budget rather than an arbitrary number, and comfortably covers a
/// normal reading session's worth of back-and-forth.
const CACHE_CAPACITY: usize = 20;

struct PendingRequest {
    page: usize,
    scale: f64,
    generation: u64,
}

struct RenderResult {
    generation: u64,
    page: usize,
    scale: f64,
    outcome: Result<Arc<RenderedPage>, String>,
}

type SharedRequest = Arc<(Mutex<Option<PendingRequest>>, Condvar)>;

fn spawn_render_worker(
    engine: Arc<PopplerEngine>,
    shared: SharedRequest,
    result_tx: Sender<RenderResult>,
    ctx: egui::Context,
) {
    thread::spawn(move || {
        let mut cache: LruCache<(usize, u64), Arc<RenderedPage>> =
            LruCache::new(NonZeroUsize::new(CACHE_CAPACITY).unwrap());
        let mut prefetch_queue: VecDeque<(usize, f64)> = VecDeque::new();

        loop {
            // Priority check #1: is there a real, user-triggered request
            // waiting? Never block here -- we want to fall through to
            // prefetch work (or, failing that, an actual blocking wait)
            // rather than get stuck.
            let maybe_request = {
                let (lock, _cvar) = &*shared;
                let mut guard = lock.lock().unwrap();
                guard.take()
            };

            if let Some(request) = maybe_request {
                let key = (request.page, request.scale.to_bits());

                let outcome = if let Some(cached) = cache.get(&key) {
                    Ok(cached.clone())
                } else {
                    match engine.render_page(PageHandle(request.page), request.scale) {
                        Ok(rendered) => {
                            let arc = Arc::new(rendered);
                            cache.put(key, arc.clone());
                            Ok(arc)
                        }
                        Err(e) => Err(e.to_string()),
                    }
                };

                let _ = result_tx.send(RenderResult {
                    generation: request.generation,
                    page: request.page,
                    scale: request.scale,
                    outcome,
                });
                ctx.request_repaint();

                // Queue neighbors for opportunistic prefetch -- only if
                // not already cached, and only ever processed below when
                // no real request is waiting.
                //
                // Depth chosen from measured data: renders average ~78ms
                // on a 982-page real-world document, consistent across
                // the whole book (not just a few heavy pages). A user
                // flipping faster than ~1 page per 78ms will outrun a
                // depth-1 prefetch and fall back to "latest wins"
                // skipping. Depth 3 forward gives roughly 3x that margin
                // before the same thing happens, at negligible extra
                // memory cost (each cached page ~8MB, well within the
                // existing 20-entry/~160MB cache budget). Fixed for now;
                // adapting depth to actual flip speed is a reasonable
                // later refinement once we know fixed-3 isn't enough.
                const PREFETCH_DEPTH_FORWARD: usize = 3;
                const PREFETCH_DEPTH_BACKWARD: usize = 1;

                for back in 1..=PREFETCH_DEPTH_BACKWARD {
                    if let Some(page) = request.page.checked_sub(back) {
                        let key = (page, request.scale.to_bits());
                        if !cache.contains(&key) {
                            prefetch_queue.push_back((page, request.scale));
                        }
                    }
                }
                for fwd in 1..=PREFETCH_DEPTH_FORWARD {
                    let page = request.page + fwd;
                    let key = (page, request.scale.to_bits());
                    if !cache.contains(&key) {
                        prefetch_queue.push_back((page, request.scale));
                    }
                }

                continue;
            }

            // Priority check #2: no real request pending -- spend idle
            // time on a queued prefetch instead of sleeping.
            if let Some((page, scale)) = prefetch_queue.pop_front() {
                let key = (page, scale.to_bits());
                if !cache.contains(&key) {
                    if let Ok(rendered) = engine.render_page(PageHandle(page), scale) {
                        cache.put(key, Arc::new(rendered));
                    }
                }
                continue;
            }

            // Nothing to do at all: block until a real request arrives.
            // Zero CPU usage while genuinely idle.
            let (lock, cvar) = &*shared;
            let mut guard = lock.lock().unwrap();
            while guard.is_none() {
                guard = cvar.wait(guard).unwrap();
            }
        }
    });
}

struct SenpaiApp {
    page_count: usize,
    current_page: usize,
    scale: f64,

    shared_request: SharedRequest,
    result_rx: Receiver<RenderResult>,
    next_generation: u64,
    requested: Option<(usize, u64, u64)>,

    texture: Option<egui::TextureHandle>,
    texture_for: Option<(usize, u64)>,
    render_pending: bool,

    dark_mode: bool,
    auto_hide_enabled: bool,
    open_menu: Option<Menu>,
}

const HOVER_ZONE_HEIGHT: f32 = 40.0;

impl SenpaiApp {
    fn new(engine: PopplerEngine, page_count: usize, cc: &eframe::CreationContext<'_>) -> Self {
        let engine = Arc::new(engine);
        let shared_request: SharedRequest = Arc::new((Mutex::new(None), Condvar::new()));
        let (tx, rx) = mpsc::channel();

        spawn_render_worker(engine, shared_request.clone(), tx, cc.egui_ctx.clone());

        Self {
            page_count,
            current_page: 0,
            scale: 1.5,
            shared_request,
            result_rx: rx,
            next_generation: 0,
            requested: None,
            texture: None,
            texture_for: None,
            render_pending: false,
            dark_mode: true,
            auto_hide_enabled: true,
            open_menu: None,
        }
    }

    fn request_render_if_needed(&mut self) {
        let want = (self.current_page, self.scale.to_bits());
        let already_current = self.texture_for == Some(want);
        let already_requested = self
            .requested
            .map(|(p, s, _)| (p, s) == want)
            .unwrap_or(false);

        if already_current || already_requested {
            return;
        }

        self.next_generation += 1;
        let generation = self.next_generation;

        {
            let (lock, cvar) = &*self.shared_request;
            let mut guard = lock.lock().unwrap();
            *guard = Some(PendingRequest {
                page: want.0,
                scale: self.scale,
                generation,
            });
            cvar.notify_one();
        }

        self.requested = Some((want.0, want.1, generation));
        self.render_pending = true;
    }

    fn drain_results(&mut self, ctx: &egui::Context) {
        while let Ok(result) = self.result_rx.try_recv() {
            let is_latest = self
                .requested
                .map(|(_, _, gen)| gen == result.generation)
                .unwrap_or(false);
            if !is_latest {
                continue;
            }

            self.render_pending = false;
            match result.outcome {
                Ok(rendered) => {
                    let image = egui::ColorImage::from_rgba_unmultiplied(
                        [rendered.width_px as usize, rendered.height_px as usize],
                        &rendered.pixels,
                    );
                    let handle =
                        ctx.load_texture("page", image, egui::TextureOptions::LINEAR);
                    self.texture = Some(handle);
                    self.texture_for = Some((result.page, result.scale.to_bits()));
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
    }
    fn zoom_out(&mut self) {
        self.scale = (self.scale / 1.2).max(0.2);
    }

    fn set_dark(&mut self, ctx: &egui::Context, dark: bool) {
        self.dark_mode = dark;
        ctx.set_visuals(if dark {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        });
    }
}

impl eframe::App for SenpaiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let (want_next, want_prev, want_zoom_in, want_zoom_out) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowRight) || i.key_pressed(egui::Key::Space),
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals),
                i.key_pressed(egui::Key::Minus),
            )
        });
        if want_next {
            self.next_page();
        }
        if want_prev {
            self.prev_page();
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

        let hovering_top = ctx.input(|i| {
            i.pointer
                .hover_pos()
                .map(|p| p.y <= HOVER_ZONE_HEIGHT)
                .unwrap_or(false)
        });
        let bar_visible = !self.auto_hide_enabled || hovering_top || self.open_menu.is_some();

        self.drain_results(ctx);
        self.request_render_if_needed();

        let bg = if self.dark_mode {
            egui::Color32::from_gray(30)
        } else {
            egui::Color32::from_gray(235)
        };

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(bg))
            .show(ctx, |ui| {
                if let Some(tex) = &self.texture {
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

        if bar_visible {
            self.show_top_bar(ctx);
        }
    }
}

impl SenpaiApp {
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
                ui.set_width(bar_width.min(320.0));
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
                            egui::Button::new(egui::RichText::new("Note").color(egui::Color32::GRAY)),
                        );
                        ui.add_enabled(
                            false,
                            egui::Button::new(egui::RichText::new("Draw").color(egui::Color32::GRAY)),
                        );
                    }
                    Menu::Modes => {
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
