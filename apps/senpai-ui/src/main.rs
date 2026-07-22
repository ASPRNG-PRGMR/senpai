//! senpai-ui — the first real windowed prototype.
//!
//! Deliberately minimal: no side panel, no top/menu bar. The page fills
//! the window; the only chrome is a small set of floating "pill" controls
//! that can be hidden entirely (press `H`) so the document is the only
//! thing on screen when you want it to be.
//!
//! This is a UI-shape milestone, not a performance one — rendering still
//! happens synchronously on open/page-change (see the ~111ms/page number
//! from the CLI benchmarks). Moving render calls off the main thread is
//! tracked separately and will slot in underneath this same UI without
//! changing how it looks.

use eframe::egui;
use senpai_core::engine::{PdfEngine, PdfEngineOpen};
use senpai_core::file_loader::LoadedFile;
use senpai_core::model::PageHandle;
use senpai_pdf_poppler::PopplerEngine;
use std::env;

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
    // `engine` owns its own copy of the document bytes internally, so
    // `file`'s mmap doesn't need to outlive this point.
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
        Box::new(move |_cc| Ok(Box::new(SenpaiApp::new(engine, page_count)))),
    )
}

struct SenpaiApp {
    engine: PopplerEngine,
    page_count: usize,
    current_page: usize,
    scale: f64,

    texture: Option<egui::TextureHandle>,
    texture_for: Option<(usize, u64)>, // (page index, scale bits) the texture matches

    ui_visible: bool,
}

impl SenpaiApp {
    fn new(engine: PopplerEngine, page_count: usize) -> Self {
        Self {
            engine,
            page_count,
            current_page: 0,
            scale: 1.5,
            texture: None,
            texture_for: None,
            ui_visible: true,
        }
    }

    /// Re-renders the current page into a GPU texture only if the page
    /// index or scale actually changed since the last frame — this is
    /// the one guard standing between "fine" and "re-rendering at
    /// ~110ms every single frame," which would make the UI unusable.
    fn ensure_texture(&mut self, ctx: &egui::Context) {
        let want = (self.current_page, self.scale.to_bits());
        if self.texture_for == Some(want) {
            return;
        }

        match self
            .engine
            .render_page(PageHandle(self.current_page), self.scale)
        {
            Ok(rendered) => {
                let image = egui::ColorImage::from_rgba_unmultiplied(
                    [rendered.width_px as usize, rendered.height_px as usize],
                    &rendered.pixels,
                );
                let handle = ctx.load_texture("page", image, egui::TextureOptions::LINEAR);
                self.texture = Some(handle);
                self.texture_for = Some(want);
            }
            Err(e) => {
                eprintln!("render failed for page {}: {e}", self.current_page);
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
}

impl eframe::App for SenpaiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let (want_next, want_prev, want_zoom_in, want_zoom_out, want_toggle_ui) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::ArrowRight) || i.key_pressed(egui::Key::Space),
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::Plus) || i.key_pressed(egui::Key::Equals),
                i.key_pressed(egui::Key::Minus),
                i.key_pressed(egui::Key::H),
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
        if want_toggle_ui {
            self.ui_visible = !self.ui_visible;
        }

        self.ensure_texture(ctx);

        // --- the page itself: one CentralPanel, zero frame/border, no
        // side or top panels anywhere in this app ---
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(egui::Color32::from_gray(30)))
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
            });

        if self.ui_visible {
            self.show_pill_toolbar(ctx);
        }
        self.show_visibility_toggle(ctx);
    }
}

impl SenpaiApp {
    /// The main floating pill: page nav + zoom, anchored to the bottom
    /// center of the window, rounded, semi-transparent, no title bar —
    /// deliberately not a docked panel of any kind.
    fn show_pill_toolbar(&mut self, ctx: &egui::Context) {
        let pill_frame = egui::Frame::none()
            .fill(egui::Color32::from_black_alpha(210))
            .rounding(egui::Rounding::same(20.0))
            .inner_margin(egui::Margin::symmetric(16.0, 10.0));

        egui::Area::new(egui::Id::new("pill_toolbar"))
            .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -20.0))
            .show(ctx, |ui| {
                pill_frame.show(ui, |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("◀").clicked() {
                            self.prev_page();
                        }
                        ui.label(
                            egui::RichText::new(format!(
                                "{} / {}",
                                self.current_page + 1,
                                self.page_count
                            ))
                            .color(egui::Color32::WHITE),
                        );
                        if ui.button("▶").clicked() {
                            self.next_page();
                        }
                        ui.separator();
                        if ui.button("−").clicked() {
                            self.zoom_out();
                        }
                        ui.label(
                            egui::RichText::new(format!("{:.0}%", self.scale / 1.5 * 100.0))
                                .color(egui::Color32::WHITE),
                        );
                        if ui.button("+").clicked() {
                            self.zoom_in();
                        }
                    });
                });
            });
    }

    /// A tiny, always-present pill in the corner that toggles the main
    /// toolbar — this is the "hideable" part of the brief. Same visual
    /// language as the main pill so it doesn't read as a stray button.
    fn show_visibility_toggle(&mut self, ctx: &egui::Context) {
        let pill_frame = egui::Frame::none()
            .fill(egui::Color32::from_black_alpha(180))
            .rounding(egui::Rounding::same(14.0))
            .inner_margin(egui::Margin::symmetric(10.0, 6.0));

        egui::Area::new(egui::Id::new("visibility_toggle"))
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-16.0, 16.0))
            .show(ctx, |ui| {
                pill_frame.show(ui, |ui| {
                    let label = if self.ui_visible { "hide (H)" } else { "show (H)" };
                    if ui
                        .add(egui::Button::new(
                            egui::RichText::new(label).color(egui::Color32::WHITE),
                        ))
                        .clicked()
                    {
                        self.ui_visible = !self.ui_visible;
                    }
                });
            });
    }
}
