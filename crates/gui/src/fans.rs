use eframe::egui::{self, ComboBox, RichText, Slider};
use fanforge_core::config::{Config, FanMode};
use fanforge_core::state::ControlState;
use fanforge_core::{Duty, FanId};

use crate::connection::{Connection, Request};
use crate::format;

pub fn show(
    ui: &mut egui::Ui,
    state: &ControlState,
    draft: &mut Config,
    connection: &Connection,
) -> bool {
    let mut changed = false;
    if state.readings.fans.is_empty() {
        ui.label("No fans detected yet.");
        return false;
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        for reading in &state.readings.fans {
            ui.group(|ui| {
                ui.set_width(ui.available_width());
                changed |= fan_card(ui, state, draft, connection, &reading.id);
            });
            ui.add_space(6.0);
        }
    });
    changed
}

fn fan_card(
    ui: &mut egui::Ui,
    state: &ControlState,
    draft: &mut Config,
    connection: &Connection,
    fan: &FanId,
) -> bool {
    let mut changed = false;
    let Some(reading) = state.readings.fan(fan) else {
        return false;
    };
    let status = state.fans.iter().find(|status| &status.id == fan);
    let limits = state.limits.get(fan).copied().unwrap_or_default();

    ui.horizontal(|ui| {
        ui.label(RichText::new(format::fan_name(draft, state, fan)).heading());
        ui.label(RichText::new(format!("({})", reading.label)).color(format::MUTED));
        ui.separator();
        ui.label(RichText::new(format::rpm(reading)).strong());
        ui.separator();
        ui.label(format!("duty {}", format::duty(reading.duty)));
        if let Some(status) = status {
            let (text, color) = format::fan_state(&status.state);
            ui.separator();
            ui.colored_label(color, text);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .button("Identify")
                .on_hover_text("Spin this fan at 100% for 3 seconds")
                .clicked()
            {
                connection.send(Request::Identify(fan.clone()));
            }
        });
    });

    let other_fans: Vec<FanId> = state
        .readings
        .fans
        .iter()
        .map(|other| other.id.clone())
        .filter(|other| other != fan)
        .collect();
    let Some(profile) = draft.profiles.get(&draft.active_profile) else {
        return false;
    };
    let mut mode = profile.fans.get(fan).cloned().unwrap_or_default();
    let mut edited_settings = draft.settings(fan);
    let min_duty = edited_settings.min_duty.percent();
    let curves = draft.curves.clone();
    let names: Vec<(FanId, String)> = other_fans
        .iter()
        .map(|id| (id.clone(), format::fan_name(draft, state, id)))
        .collect();

    ui.horizontal(|ui| {
        ui.label("Mode");
        ComboBox::from_id_salt(("mode", fan.as_str()))
            .selected_text(format::mode_name(&mode))
            .show_ui(ui, |ui| {
                let mut choose = |ui: &mut egui::Ui, candidate: FanMode, enabled: bool| {
                    let selected =
                        std::mem::discriminant(&candidate) == std::mem::discriminant(&mode);
                    let label = format::mode_name(&candidate);
                    if ui
                        .add_enabled(enabled, egui::Button::selectable(selected, label))
                        .clicked()
                        && !selected
                    {
                        mode = candidate;
                        changed = true;
                    }
                };
                choose(ui, FanMode::Auto, true);
                let current = reading.duty.unwrap_or(Duty::new(50.0));
                choose(ui, FanMode::Manual { duty: current }, true);
                let first_curve = curves.first().map(|curve| curve.id.clone());
                choose(
                    ui,
                    FanMode::Curve {
                        curve: first_curve.clone().unwrap_or_else(|| "".into()),
                    },
                    first_curve.is_some(),
                );
                let first_fan = other_fans.first().cloned();
                choose(
                    ui,
                    FanMode::Sync {
                        fan: first_fan.clone().unwrap_or_else(|| "".into()),
                    },
                    first_fan.is_some(),
                );
            });

        match &mut mode {
            FanMode::Auto => {
                ui.label(
                    RichText::new("The motherboard BIOS controls this fan.").color(format::MUTED),
                );
            }
            FanMode::Manual { duty } => {
                let mut percent = duty.percent();
                if ui
                    .add(Slider::new(&mut percent, 0.0..=100.0).suffix("%"))
                    .changed()
                {
                    *duty = Duty::new(percent);
                    changed = true;
                }
                let floor = limits.floor.percent().max(min_duty);
                if percent < floor {
                    ui.colored_label(format::WARN, format!("held at the {floor:.0}% floor"));
                }
            }
            FanMode::Curve { curve } => {
                let selected = curves
                    .iter()
                    .find(|candidate| &candidate.id == curve)
                    .map_or_else(|| curve.to_string(), |candidate| candidate.name.clone());
                ComboBox::from_id_salt(("curve", fan.as_str()))
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        for candidate in &curves {
                            if ui
                                .selectable_label(&candidate.id == curve, &candidate.name)
                                .clicked()
                                && &candidate.id != curve
                            {
                                *curve = candidate.id.clone();
                                changed = true;
                            }
                        }
                    });
            }
            FanMode::Sync { fan: leader } => {
                let selected = names
                    .iter()
                    .find(|(id, _)| id == leader)
                    .map_or_else(|| leader.to_string(), |(_, name)| name.clone());
                ComboBox::from_id_salt(("sync", fan.as_str()))
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        for (id, name) in &names {
                            if ui.selectable_label(id == leader, name).clicked() && id != leader {
                                *leader = id.clone();
                                changed = true;
                            }
                        }
                    });
            }
        }
    });

    if changed && let Some(profile) = draft.profiles.get_mut(&draft.active_profile) {
        profile.fans.insert(fan.clone(), mode);
    }

    let mut settings_changed = false;
    egui::CollapsingHeader::new("Fan settings")
        .id_salt(("settings", fan.as_str()))
        .show(ui, |ui| {
            let settings = &mut edited_settings;
            ui.horizontal(|ui| {
                ui.label("Name");
                let mut name = settings.name.clone().unwrap_or_default();
                if ui
                    .add(egui::TextEdit::singleline(&mut name).hint_text(&reading.label))
                    .changed()
                {
                    settings.name = (!name.trim().is_empty()).then_some(name);
                    settings_changed = true;
                }
            });
            ui.horizontal(|ui| {
                ui.label("Minimum duty");
                let mut percent = settings.min_duty.percent();
                if ui
                    .add(Slider::new(&mut percent, 0.0..=100.0).suffix("%"))
                    .changed()
                {
                    settings.min_duty = Duty::new(percent);
                    settings_changed = true;
                }
                ui.label(
                    RichText::new(format!("hardware floor {}", limits.floor)).color(format::MUTED),
                );
            });
            ui.horizontal(|ui| {
                let response = ui.add_enabled(
                    limits.stop_permitted,
                    egui::Checkbox::new(
                        &mut settings.allow_stop,
                        "Allow 0 RPM when the curve asks for 0%",
                    ),
                );
                settings_changed |= response.changed();
                if !limits.stop_permitted {
                    response.on_disabled_hover_text(
                        "This header must keep spinning (CPU fan or unverified board profile)",
                    );
                }
            });
            ui.horizontal(|ui| {
                ui.label("Spin-up duty");
                let mut percent = settings.spin_up_duty.percent();
                if ui
                    .add(Slider::new(&mut percent, 0.0..=100.0).suffix("%"))
                    .changed()
                {
                    settings.spin_up_duty = Duty::new(percent);
                    settings_changed = true;
                }
            });
        });
    if settings_changed {
        draft.fans.insert(fan.clone(), edited_settings);
        changed = true;
    }
    changed
}
