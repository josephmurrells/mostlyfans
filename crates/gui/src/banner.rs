use eframe::egui::{self, Color32};

pub fn frame<R>(ui: &mut egui::Ui, color: Color32, contents: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.18))
        .stroke(egui::Stroke::new(1.0, color))
        .corner_radius(4.0)
        .inner_margin(6.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            contents(ui)
        })
        .inner
}

pub fn show(ui: &mut egui::Ui, color: Color32, text: &str) {
    frame(ui, color, |ui| ui.colored_label(color, text));
}

pub fn with_action(ui: &mut egui::Ui, color: Color32, text: &str, action: &str) -> bool {
    frame(ui, color, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(color, text);
            ui.button(action).clicked()
        })
        .inner
    })
}
