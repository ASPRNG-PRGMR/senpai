//! senpai-ui — windowed reader, v2 UI.
//!
//! Exactly one floating pill: a wide top bar (Files / Tools / Modes /
//! Options) that stays hidden unless the user hovers the top of the
//! window or triggers a menu via an Alt+<letter> shortcut. Auto-hide can
//! be disabled entirely from the Options menu. No other floating chrome
//! exists anywhere in this app.

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

#[derive(Clone, Copy, PartialEq, Eq)]
enum Menu {
    Files,
    Tools,
    Modes,
    Options,
}

struct SenpaiApp {
    engine: PopplerEngine,
    page_count: usize,
    current_page: usize,
    scale: f64,

    texture: Option<egui::TextureHandle>,
    texture_for: Option<(usize, u64)>,

    dark_mode: bool,
    auto_hide_enabled: bool,
    open_menu: Option<Menu>,
}

/// Height, in logical points, of the invisible strip at the top of the
/// window that reveals the bar on hover. Deliberately taller than the
/// bar's own drawn height so the cursor doesn't need to land pixel-
/// perfectly on the (currently invisible) pill to trigger it.
const HOVER_ZONE_HEIGHT: f32 = 40.0;

impl SenpaiApp {
    fn new(engine: PopplerEngine, page_count: usize) -> Self {
        Self {
            engine,
            page_count,
            current_page: 0,
            scale: 1.5,
            texture: None,
            texture_for: None,
            dark_mode: true,
            auto_hide_enabled: true,
            open_menu: None,
        }
    }

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
            Err(e) => eprintln!("render failed for page {}: {e}", self.current_page),
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
        // --- page navigation / zoom keyboard shortcuts (unrelated to the bar) ---
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

        // --- bar-opening shortcuts: Alt+F / Alt+T / Alt+M / Alt+O ---
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

        // --- hover-to-reveal detection ---
        let hovering_top = ctx.input(|i| {
            i.pointer
                .hover_pos()
                .map(|p| p.y <= HOVER_ZONE_HEIGHT)
                .unwrap_or(false)
        });

        let bar_visible = !self.auto_hide_enabled || hovering_top || self.open_menu.is_some();

        self.ensure_texture(ctx);

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
            });

        if bar_visible {
            self.show_top_bar(ctx);
        }
    }
}

impl SenpaiApp {
    /// The one and only floating pill: a wide bar across the top of the
    /// window, Files / Tools / Modes / Options. No other pill exists in
    /// this app.
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
                            // TODO: wire to a real file picker (e.g. the
                            // `rfd` crate) once this shape is confirmed.
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
                        if ui
                            .checkbox(&mut auto_hide, "Enable auto-hide")
                            .changed()
                        {
                            self.auto_hide_enabled = auto_hide;
                        }
                    }
                });
            });
    }
}
