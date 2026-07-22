//! senpai-ui — windowed reader, v3: background rendering.
//!
//! Rendering now happens on a dedicated background thread instead of
//! blocking the UI thread. The two threads coordinate through a
//! "latest wins" shared slot (Mutex + Condvar) for requests -- if the
//! user flips pages faster than rendering can keep up, only the most
//! recently requested page/scale ever actually gets rendered, nothing
//! queues up and falls behind -- plus an mpsc channel carrying results
//! back, tagged with a generation number so stale/superseded results
//! are silently discarded rather than flashing an old page onto screen.
//!
//! Verified with a standalone test (real PDF, real background thread)
//! before wiring into the UI: see commit history for that check.
//!
//! Important invariant: after startup, the UI/main thread never touches
//! `engine` directly again -- only the background thread calls
//! `render_page`. poppler-cpp's thread-safety story for a single shared
//! `document` is not well-documented, so rather than relying on
//! internal locking we simply never access it from two threads at once.

use eframe::egui;
use senpai_core::engine::{PdfEngine, PdfEngineOpen};
use senpai_core::file_loader::LoadedFile;
use senpai_core::model::{PageHandle, RenderedPage};
use senpai_pdf_poppler::PopplerEngine;
use std::env;
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

/// A render request in flight to the background thread. Writing a new
/// one into the shared slot overwrites whatever was there -- "latest
/// wins," no queue, no backlog.
struct PendingRequest {
    page: usize,
    scale: f64,
    generation: u64,
}

/// What comes back from the background thread. Tagged with the same
/// generation so the UI thread can tell a fresh result from a stale one
/// that got superseded while it was still rendering.
struct RenderResult {
    generation: u64,
    page: usize,
    scale: f64,
    outcome: Result<RenderedPage, String>,
}

type SharedRequest = Arc<(Mutex<Option<PendingRequest>>, Condvar)>;

fn spawn_render_worker(
    engine: Arc<PopplerEngine>,
    shared: SharedRequest,
    result_tx: Sender<RenderResult>,
    ctx: egui::Context,
) {
    thread::spawn(move || loop {
        let request = {
            let (lock, cvar) = &*shared;
            let mut guard = lock.lock().unwrap();
            while guard.is_none() {
                guard = cvar.wait(guard).unwrap();
            }
            guard.take().unwrap()
        };

        let outcome = engine
            .render_page(PageHandle(request.page), request.scale)
            .map_err(|e| e.to_string());

        let _ = result_tx.send(RenderResult {
            generation: request.generation,
            page: request.page,
            scale: request.scale,
            outcome,
        });
        // Wake the UI thread up so it processes the result promptly
        // instead of waiting for the next input event.
        ctx.request_repaint();
    });
}

struct SenpaiApp {
    page_count: usize,
    current_page: usize,
    scale: f64,

    shared_request: SharedRequest,
    result_rx: Receiver<RenderResult>,
    next_generation: u64,
    /// (page, scale-bits, generation) of the most recent request we sent
    /// -- lets us avoid re-sending an identical request every frame
    /// while one is already in flight.
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

    /// Sends a new render request if the desired (page, scale) isn't
    /// already the current texture or already in flight. Never blocks.
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

    /// Drains any results waiting on the channel. Discards anything that
    /// isn't the latest generation we asked for -- that's a render that
    /// got superseded by a faster page-flip before it finished.
    fn drain_results(&mut self, ctx: &egui::Context) {
        while let Ok(result) = self.result_rx.try_recv() {
            let is_latest = self
                .requested
                .map(|(_, _, gen)| gen == result.generation)
                .unwrap_or(false);
            if !is_latest {
                continue; // stale, discard
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

                // Subtle, plain-text-only indicator -- deliberately not a
                // pill/frame, since the only floating pill in this app is
                // the top bar.
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
