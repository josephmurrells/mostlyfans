use eframe::egui::{self, ComboBox, DragValue, RichText};
use fanforge_core::config::{Config, MAX_INTERVAL_MS, MIN_INTERVAL_MS, Profile};
use fanforge_core::protocol::ServiceState;

use crate::format;

#[derive(Debug, Default)]
pub struct SettingsView {
    new_profile: String,
}

impl SettingsView {
    pub fn show(&mut self, ui: &mut egui::Ui, state: &ServiceState, draft: &mut Config) -> bool {
        let mut changed = false;
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("Profiles");
            changed |= self.profiles(ui, draft);
            ui.separator();
            ui.heading("Control loop");
            ui.horizontal(|ui| {
                ui.label("Update interval");
                changed |= ui
                    .add(
                        DragValue::new(&mut draft.interval_ms)
                            .range(MIN_INTERVAL_MS..=MAX_INTERVAL_MS)
                            .speed(10.0)
                            .suffix(" ms"),
                    )
                    .changed();
            });
            ui.separator();
            ui.heading("Safety");
            changed |= safety(ui, state, draft);
            ui.separator();
            ui.heading("Board");
            board(ui, state);
        });
        changed
    }

    fn profiles(&mut self, ui: &mut egui::Ui, draft: &mut Config) -> bool {
        let mut changed = false;
        let names: Vec<String> = draft.profiles.keys().cloned().collect();
        let mut remove = None;
        for name in &names {
            ui.horizontal(|ui| {
                changed |= ui
                    .radio_value(&mut draft.active_profile, name.clone(), name)
                    .changed();
                let removable = name != &draft.active_profile && names.len() > 1;
                if ui
                    .add_enabled(removable, egui::Button::new("Delete"))
                    .clicked()
                {
                    remove = Some(name.clone());
                }
            });
        }
        if let Some(name) = remove {
            draft.profiles.remove(&name);
            changed = true;
        }
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.new_profile).hint_text("New profile name"));
            let name = self.new_profile.trim().to_owned();
            let valid = !name.is_empty() && !draft.profiles.contains_key(&name);
            if ui
                .add_enabled(valid, egui::Button::new("Add (copy of active)"))
                .clicked()
            {
                let copy = draft
                    .profiles
                    .get(&draft.active_profile)
                    .cloned()
                    .unwrap_or_else(Profile::default);
                draft.profiles.insert(name, copy);
                self.new_profile.clear();
                changed = true;
            }
        });
        changed
    }
}

fn safety(ui: &mut egui::Ui, state: &ServiceState, draft: &mut Config) -> bool {
    let mut changed = false;
    let safety = &mut draft.safety;
    egui::Grid::new("safety").num_columns(2).show(ui, |ui| {
        ui.label("Critical sensor");
        let selected = state
            .readings
            .sensors
            .iter()
            .find(|sensor| sensor.id == safety.critical_sensor)
            .map_or_else(
                || safety.critical_sensor.to_string(),
                |sensor| sensor.label.clone(),
            );
        ComboBox::from_id_salt("critical-sensor")
            .selected_text(selected)
            .show_ui(ui, |ui| {
                for sensor in &state.readings.sensors {
                    if ui
                        .selectable_label(sensor.id == safety.critical_sensor, &sensor.label)
                        .clicked()
                    {
                        safety.critical_sensor = sensor.id.clone();
                        changed = true;
                    }
                }
            });
        ui.end_row();

        ui.label("All fans to 100% at");
        changed |= ui
            .add(
                DragValue::new(&mut safety.critical_c)
                    .range(40.0..=110.0)
                    .speed(0.5)
                    .suffix(" °C"),
            )
            .changed();
        ui.end_row();

        ui.label("Release below");
        let max_release = safety.critical_c - 1.0;
        changed |= ui
            .add(
                DragValue::new(&mut safety.release_c)
                    .range(0.0..=max_release)
                    .speed(0.5)
                    .suffix(" °C"),
            )
            .changed();
        ui.end_row();

        ui.label("for");
        changed |= ui
            .add(
                DragValue::new(&mut safety.release_hold_secs)
                    .range(0.0..=600.0)
                    .speed(0.5)
                    .suffix(" s"),
            )
            .changed();
        ui.end_row();

        ui.label("Sensor failures before 100%");
        changed |= ui
            .add(DragValue::new(&mut safety.sensor_failure_ticks).range(1..=60))
            .changed();
        ui.end_row();
    });
    changed
}

fn board(ui: &mut egui::Ui, state: &ServiceState) {
    let (text, color) = format::hardware(&state.hardware);
    ui.colored_label(color, text);
    if let Some(board) = &state.board {
        ui.label(format!(
            "{} {}",
            board.identity.manufacturer, board.identity.product
        ));
        ui.label(format!(
            "Profile: {}  ·  Super I/O {:#06x}",
            board.profile_name, board.chip_id
        ));
        let verified = if board.verified {
            "verified"
        } else {
            "unverified (provisional header names)"
        };
        ui.label(RichText::new(verified).color(if board.verified {
            format::OK
        } else {
            format::WARN
        }));
    }
    ui.label(RichText::new(format!("Service version {}", state.version)).color(format::MUTED));
}
