use eframe::egui::{self, RichText};
use fanforge_core::protocol::ServiceState;

use crate::format;

pub fn show(ui: &mut egui::Ui, state: &ServiceState) {
    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.heading("Temperatures");
        egui::Grid::new("sensors")
            .striped(true)
            .num_columns(3)
            .show(ui, |ui| {
                for sensor in &state.readings.sensors {
                    ui.label(&sensor.label);
                    ui.label(RichText::new(format::celsius(sensor.celsius)).strong());
                    ui.label(RichText::new(sensor.id.as_str()).color(format::MUTED));
                    ui.end_row();
                }
            });
        ui.add_space(12.0);
        ui.heading("Fans");
        egui::Grid::new("fan-readings")
            .striped(true)
            .num_columns(4)
            .show(ui, |ui| {
                for fan in &state.readings.fans {
                    ui.label(format::fan_name(&state.config, state, &fan.id));
                    ui.label(RichText::new(format::rpm(fan)).strong());
                    ui.label(format::duty(fan.duty));
                    ui.label(RichText::new(fan.id.as_str()).color(format::MUTED));
                    ui.end_row();
                }
            });
    });
}
