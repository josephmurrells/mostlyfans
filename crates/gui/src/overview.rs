use std::time::Instant;

use eframe::egui::{self, Id, RichText, vec2};
use fanforge_core::FanId;
use fanforge_core::state::ControlState;
use fanforge_core::telemetry::{FanReading, SensorReading};

use crate::format;
use crate::history::History;
use crate::theme::{self, DISPLAY, SPACE_M, SPACE_S, Tone};
use crate::widgets;

const TILE_COUNT: usize = 4;
const TILE_WIDTH: f32 = 190.0;
const FAN_CARD_WIDTH: f32 = 230.0;
const SPARK_HEIGHT: f32 = 36.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverviewAction {
    OpenFan(FanId),
    StartSetup,
    SkipSetup,
}

pub fn ordered_sensors(state: &ControlState) -> Vec<&SensorReading> {
    let critical = &state.config.safety.critical_sensor;
    let rank = |sensor: &SensorReading| {
        if &sensor.id == critical {
            0
        } else if sensor.id.as_str().starts_with("cpu/") {
            1
        } else {
            2
        }
    };
    let mut sensors: Vec<&SensorReading> = state.readings.sensors.iter().collect();
    sensors.sort_by_key(|sensor| rank(sensor));
    sensors
}

pub fn unnamed_fans(state: &ControlState, config: &fanforge_core::config::Config) -> Vec<FanId> {
    state
        .readings
        .fans
        .iter()
        .filter(|fan| format::connected(fan) && !format::is_named(config, &fan.id))
        .map(|fan| fan.id.clone())
        .collect()
}

pub fn show(
    ui: &mut egui::Ui,
    state: &ControlState,
    history: &History,
    live: bool,
    offer_setup: bool,
) -> Option<OverviewAction> {
    let mut action = None;
    let now = Instant::now();
    widgets::page_title(ui, "Overview", None);

    let unnamed = unnamed_fans(state, &state.config);
    if offer_setup && !unnamed.is_empty() {
        action = setup_card(ui, unnamed.len()).or(action);
        ui.add_space(SPACE_M);
    }

    widgets::section_heading(ui, "Temperatures");
    let sensors = ordered_sensors(state);
    let tiles: Vec<&SensorReading> = sensors.iter().take(TILE_COUNT).copied().collect();
    widgets::card_grid(ui, &tiles, TILE_WIDTH, |ui, sensor| {
        temperature_tile(ui, state, history, sensor, live, now);
    });
    if sensors.len() > TILE_COUNT {
        egui::CollapsingHeader::new(format!("All sensors ({})", sensors.len()))
            .id_salt("all-sensors")
            .show(ui, |ui| {
                egui::Grid::new("all-sensors-grid")
                    .num_columns(2)
                    .spacing(vec2(SPACE_M * 2.0, SPACE_S))
                    .show(ui, |ui| {
                        for sensor in &sensors {
                            ui.label(&sensor.label).on_hover_text(sensor.id.as_str());
                            ui.label(RichText::new(format::celsius(sensor.celsius)).strong());
                            ui.end_row();
                        }
                    });
            });
    }

    ui.add_space(SPACE_M);
    widgets::section_heading(ui, "Fans");
    if state.readings.fans.is_empty() {
        ui.label(widgets::muted(
            "No fan headers found. FanForge needs the PawnIO driver and a supported Super I/O chip.",
        ));
        return action;
    }
    let mut fans: Vec<&FanReading> = state.readings.fans.iter().collect();
    fans.sort_by_key(|fan| !format::connected(fan));
    widgets::card_grid(ui, &fans, FAN_CARD_WIDTH, |ui, fan| {
        if fan_card(ui, state, history, fan, live, now).clicked() {
            action = Some(OverviewAction::OpenFan(fan.id.clone()));
        }
    });
    action
}

fn setup_card(ui: &mut egui::Ui, count: usize) -> Option<OverviewAction> {
    let mut action = None;
    widgets::card(ui, |ui| {
        widgets::card_title(
            ui,
            "Set up your fans",
            Some(&format!(
                "{count} {} a fan connected but no name yet. Spin each one to find it, name it, and say whether it is a case fan.",
                if count == 1 {
                    "header has"
                } else {
                    "headers have"
                }
            )),
        );
        ui.horizontal(|ui| {
            if widgets::primary_button(ui, "Start setup").clicked() {
                action = Some(OverviewAction::StartSetup);
            }
            if ui.button("Skip for now").clicked() {
                action = Some(OverviewAction::SkipSetup);
            }
        });
    });
    action
}

fn temperature_tile(
    ui: &mut egui::Ui,
    state: &ControlState,
    history: &History,
    sensor: &SensorReading,
    live: bool,
    now: Instant,
) {
    let palette = theme::of(ui);
    widgets::card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(widgets::caption(ui, &sensor.label))
                .on_hover_text(sensor.id.as_str());
            if sensor.id == state.config.safety.critical_sensor {
                ui.label(widgets::caption(ui, "· safety sensor"));
            }
        });
        let color = if live {
            palette.text
        } else {
            palette.text_muted
        };
        ui.label(widgets::value(format::whole_celsius(sensor.celsius), DISPLAY).color(color));
        let points = history.temperature(&sensor.id, now);
        let (low, high) = points
            .iter()
            .fold((f32::MAX, f32::MIN), |(low, high), (_, value)| {
                (low.min(*value), high.max(*value))
            });
        let middle = (low + high) / 2.0;
        let half = ((high - low) / 2.0).max(5.0) + 1.0;
        widgets::sparkline(
            ui,
            vec2(ui.available_width(), SPARK_HEIGHT),
            &points,
            (middle - half, middle + half),
            palette.accent,
        );
        if !live {
            ui.label(widgets::caption(ui, "last known"));
        }
    });
}

fn fan_card(
    ui: &mut egui::Ui,
    state: &ControlState,
    history: &History,
    fan: &FanReading,
    live: bool,
    now: Instant,
) -> egui::Response {
    let name = format::fan_name(&state.config, state, &fan.id);
    let connected = format::connected(fan);
    let status = state.fans.iter().find(|status| status.id == fan.id);
    let settings = state.config.settings(&fan.id);
    let limits = state.limits.get(&fan.id).copied().unwrap_or_default();
    let floor = format::effective_floor(limits, &settings, state.config.calibration_for(&fan.id));
    let mode = state.config.mode(&fan.id);
    widgets::clickable_card(ui, Id::unique(("fan-card", fan.id.as_str())), &name, |ui| {
        let palette = theme::of(ui);
        ui.horizontal(|ui| {
            ui.label(RichText::new(&name).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !live {
                    widgets::status(ui, Tone::Neutral, "BIOS control");
                } else if let Some(status) = status {
                    let (text, tone) = format::fan_state(&status.state);
                    widgets::status(ui, tone, &text);
                }
            });
        });
        let rpm = if connected {
            format::rpm(fan)
        } else {
            "No fan".to_owned()
        };
        let color = if live && connected {
            palette.text
        } else {
            palette.text_muted
        };
        ui.label(widgets::value(rpm, DISPLAY).color(color));
        let points = history.rpm(&fan.id, now);
        let peak = points.iter().map(|(_, rpm)| *rpm).fold(0.0, f32::max);
        widgets::sparkline(
            ui,
            vec2(ui.available_width(), SPARK_HEIGHT),
            &points,
            (0.0, (peak * 1.15).max(500.0)),
            palette.accent,
        );
        ui.add_space(SPACE_S);
        let duty = fan.duty.map(|duty| duty.percent());
        widgets::duty_bar(ui, ui.available_width(), duty, Some(floor.percent));
        ui.horizontal(|ui| {
            widgets::chip(ui, &format::mode_summary(&state.config, state, &mode));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(widgets::caption(ui, format::duty(fan.duty)));
            });
        });
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;
    use fanforge_core::SensorId;

    #[test]
    fn the_safety_sensor_comes_first_and_the_order_is_stable() {
        let mut state = fixtures::state();
        state.config.safety.critical_sensor = SensorId::new("board/temp2");
        state.readings.sensors[1].celsius = Some(90.0);
        let order: Vec<&str> = ordered_sensors(&state)
            .iter()
            .map(|sensor| sensor.id.as_str())
            .collect();
        assert_eq!(order, vec!["board/temp2", "cpu/tctl", "board/temp1"]);
    }

    #[test]
    fn only_connected_unnamed_fans_need_setup() {
        let state = fixtures::state();
        assert_eq!(unnamed_fans(&state, &state.config), vec![fixtures::fan(1)]);
    }
}
