use eframe::egui;

fn main() -> eframe::Result<()> {
    eframe::run_native(
        "senpai win-test",
        eframe::NativeOptions::default(),
        Box::new(|_cc| Ok(Box::new(App))),
    )
}

struct App;

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(egui::Color32::from_gray(30)))
            .show(ctx, |ui| {
                ui.centered_and_justified(|ui| {
                    ui.label(egui::RichText::new("hello from windows").size(24.0));
                });
            });

        egui::Area::new(egui::Id::new("pill"))
            .anchor(egui::Align2::CENTER_BOTTOM, egui::vec2(0.0, -20.0))
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(egui::Color32::from_black_alpha(210))
                    .rounding(egui::Rounding::same(20.0))
                    .inner_margin(egui::Margin::symmetric(16.0, 10.0))
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new("a pill").color(egui::Color32::WHITE));
                    });
            });
    }
}
