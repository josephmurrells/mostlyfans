use std::collections::BTreeSet;
use std::time::Instant;

use eframe::egui::{self, Id, RichText, vec2};
use fanforge_core::state::ControlState;
use fanforge_core::telemetry::{FanReading, SensorReading};
use fanforge_core::{FanId, FanKind, SensorId};

use crate::format;
use crate::history::History;
use crate::icons;
use crate::theme::{self, DISPLAY, SPACE_M, SPACE_S, Tone};
use crate::widgets;

pub const TILE_COUNT: usize = 4;
const TILE_WIDTH: f32 = 190.0;
const FAN_CARD_WIDTH: f32 = 230.0;
const SPARK_HEIGHT: f32 = 36.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverviewAction {
    OpenFan(FanId),
    OpenFans,
    OpenTemperatures,
    UnpinSensor(SensorId),
    UnpinFan(FanId),
    StartSetup,
    SkipSetup,
}

pub struct OverviewPage<'a> {
    pub state: &'a ControlState,
    pub history: &'a History,
    pub live: bool,
    pub offer_setup: bool,
    pub sensors: &'a [SensorId],
    pub fans: &'a [FanId],
}

fn category(sensor: &SensorReading) -> usize {
    const PREFIXES: [&str; 3] = ["cpu/", "gpu/", "drive/"];
    PREFIXES
        .iter()
        .position(|prefix| sensor.id.as_str().starts_with(prefix))
        .unwrap_or(PREFIXES.len())
}

pub fn ordered_sensors(state: &ControlState) -> Vec<&SensorReading> {
    let critical = &state.config.safety.critical_sensor;
    let mut leaders: BTreeSet<usize> = state
        .readings
        .sensors
        .iter()
        .filter(|sensor| &sensor.id == critical)
        .map(category)
        .collect();
    let mut ranked: Vec<((usize, usize), &SensorReading)> = state
        .readings
        .sensors
        .iter()
        .filter(|sensor| !sensor.id.is_display_only())
        .map(|sensor| {
            let group = category(sensor);
            let tier = if &sensor.id == critical {
                0
            } else if leaders.insert(group) {
                1
            } else {
                2
            };
            ((tier, group), sensor)
        })
        .collect();
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, sensor)| sensor).collect()
}

pub fn ordered_fans(state: &ControlState) -> Vec<&FanReading> {
    let mut fans: Vec<&FanReading> = state.readings.fans.iter().collect();
    fans.sort_by_key(|fan| !format::connected(fan));
    fans
}

pub fn unnamed_fans(state: &ControlState, config: &fanforge_core::config::Config) -> Vec<FanId> {
    state
        .readings
        .fans
        .iter()
        .filter(|fan| fan.id.kind() == FanKind::Header && !fan.monitor_only)
        .filter(|fan| format::connected(fan) && !format::is_named(config, &fan.id))
        .map(|fan| fan.id.clone())
        .collect()
}

enum Tile<'a, T, R> {
    Present(&'a R),
    Absent(&'a T),
}

fn tiles<'a, T: PartialEq, R>(
    ids: &'a [T],
    readings: &'a [R],
    id_of: impl Fn(&R) -> &T,
) -> Vec<Tile<'a, T, R>> {
    ids.iter()
        .map(|id| {
            readings
                .iter()
                .find(|reading| id_of(reading) == id)
                .map_or(Tile::Absent(id), Tile::Present)
        })
        .collect()
}

pub fn show(ui: &mut egui::Ui, page: &OverviewPage<'_>) -> Option<OverviewAction> {
    let mut action = None;
    let now = Instant::now();
    let state = page.state;
    let settled = page.history.settled(now);
    widgets::page_title(ui, "Overview", None);

    let unnamed = unnamed_fans(state, &state.config);
    if page.offer_setup && !unnamed.is_empty() {
        action = setup_card(ui, unnamed.len()).or(action);
        ui.add_space(SPACE_M);
    }

    ui.horizontal(|ui| {
        widgets::section_heading(ui, "Temperatures");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.link("All temperatures").clicked() {
                action = Some(OverviewAction::OpenTemperatures);
            }
        });
    });
    let sensor_tiles = tiles(page.sensors, &state.readings.sensors, |sensor| &sensor.id);
    if sensor_tiles.is_empty() {
        ui.horizontal(|ui| {
            ui.label(widgets::muted("No temperatures pinned."));
            if ui.link("Pin some on the Temperatures page").clicked() {
                action = Some(OverviewAction::OpenTemperatures);
            }
        });
    }
    widgets::card_grid(ui, &sensor_tiles, TILE_WIDTH, |ui, tile| {
        let next = match tile {
            Tile::Present(sensor) => temperature_tile(ui, page, sensor, now),
            Tile::Absent(id) => absent_tile(ui, id.as_str(), settled && page.live)
                .then(|| OverviewAction::UnpinSensor((*id).clone())),
        };
        if next.is_some() {
            action = next;
        }
    });

    ui.add_space(SPACE_M);
    widgets::section_heading(ui, "Fans");
    if state.readings.fans.is_empty() && page.fans.is_empty() {
        ui.label(widgets::muted(
            "No fan headers found. FanForge needs the PawnIO driver and a supported Super I/O chip.",
        ));
        return action;
    }
    let fan_tiles = tiles(page.fans, &state.readings.fans, |fan| &fan.id);
    if fan_tiles.is_empty() {
        ui.horizontal(|ui| {
            ui.label(widgets::muted("No fans pinned."));
            if ui.link("Pin some on the Fans page").clicked() {
                action = Some(OverviewAction::OpenFans);
            }
        });
    }
    widgets::card_grid(ui, &fan_tiles, FAN_CARD_WIDTH, |ui, tile| {
        let next = match tile {
            Tile::Present(fan) => fan_card(ui, page, fan, now),
            Tile::Absent(id) => absent_tile(ui, id.as_str(), settled && page.live)
                .then(|| OverviewAction::UnpinFan((*id).clone())),
        };
        if next.is_some() {
            action = next;
        }
    });
    action
}

fn unpin_menu(response: &egui::Response, unpin: OverviewAction) -> Option<OverviewAction> {
    let mut action = None;
    response.context_menu(|ui| {
        if ui
            .button(widgets::with_icon(icons::UNPIN, "Unpin from Overview"))
            .clicked()
        {
            action = Some(unpin.clone());
            ui.close();
        }
    });
    action
}

fn absent_tile(ui: &mut egui::Ui, id: &str, missing: bool) -> bool {
    let mut unpin = false;
    widgets::card(ui, |ui| {
        ui.label(widgets::caption(ui, id));
        if missing {
            ui.label(widgets::value("Missing", theme::SECTION).weak());
            ui.label(widgets::caption(
                ui,
                "Pinned, but FanForge can't find it any more.",
            ));
            unpin = ui
                .button(widgets::with_icon(icons::UNPIN, "Unpin"))
                .clicked();
        } else {
            ui.label(widgets::value("–", DISPLAY).weak());
            ui.label(widgets::caption(ui, "waiting for a reading"));
            unpin = ui
                .small_button(widgets::with_icon(icons::UNPIN, "Unpin"))
                .clicked();
        }
    });
    unpin
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
    page: &OverviewPage<'_>,
    sensor: &SensorReading,
    now: Instant,
) -> Option<OverviewAction> {
    let state = page.state;
    let response = widgets::clickable_card(
        ui,
        Id::unique(("temperature-tile", sensor.id.as_str())),
        &sensor.label,
        |ui| {
            let palette = theme::of(ui);
            ui.horizontal(|ui| {
                ui.label(widgets::caption(ui, &sensor.label))
                    .on_hover_text(sensor.id.as_str());
                if sensor.id == state.config.safety.critical_sensor {
                    ui.label(widgets::caption(ui, "· safety sensor"));
                }
            });
            let color = if page.live {
                palette.text
            } else {
                palette.text_muted
            };
            ui.label(widgets::value(format::whole_celsius(sensor.celsius), DISPLAY).color(color));
            let points = page.history.temperature(&sensor.id, now);
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
            if !page.live {
                ui.label(widgets::caption(ui, "last known"));
            }
        },
    );
    let unpin = unpin_menu(&response, OverviewAction::UnpinSensor(sensor.id.clone()));
    if response.clicked() {
        return Some(OverviewAction::OpenTemperatures);
    }
    unpin
}

fn fan_card(
    ui: &mut egui::Ui,
    page: &OverviewPage<'_>,
    fan: &FanReading,
    now: Instant,
) -> Option<OverviewAction> {
    let state = page.state;
    let history = page.history;
    let live = page.live;
    let name = format::fan_name(&state.config, state, &fan.id);
    let connected = format::connected(fan);
    let status = state.fans.iter().find(|status| status.id == fan.id);
    let settings = state.config.settings(&fan.id);
    let limits = state.limits.get(&fan.id).copied().unwrap_or_default();
    let floor = format::effective_floor(limits, &settings, state.config.calibration_for(&fan.id));
    let mode = state.config.mode(&fan.id);
    let response =
        widgets::clickable_card(ui, Id::unique(("fan-card", fan.id.as_str())), &name, |ui| {
            let palette = theme::of(ui);
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if !live {
                        widgets::status(ui, Tone::Neutral, "BIOS control");
                    } else if let Some(status) = status {
                        let (text, tone) = format::fan_state(&status.state);
                        widgets::status(ui, tone, &text);
                    }
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add(egui::Label::new(RichText::new(&name).strong()).wrap());
                    });
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
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(widgets::caption(ui, format::duty(fan.duty)));
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                        widgets::chip(
                            ui,
                            &format::mode_summary(&state.config, state, &fan.id, &mode),
                        );
                    });
                });
            });
        });
    let unpin = unpin_menu(&response, OverviewAction::UnpinFan(fan.id.clone()));
    if response.clicked() {
        return Some(OverviewAction::OpenFan(fan.id.clone()));
    }
    unpin
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    #[test]
    fn the_safety_sensor_comes_first_and_the_order_is_stable() {
        let mut state = fixtures::state();
        state.config.safety.critical_sensor = SensorId::new("board/temp2");
        state.readings.sensors[1].celsius = Some(90.0);
        let order: Vec<&str> = ordered_sensors(&state)
            .iter()
            .map(|sensor| sensor.id.as_str())
            .collect();
        assert_eq!(order, vec!["board/temp2", "cpu/package", "board/temp1"]);
    }

    #[test]
    fn each_category_leads_with_one_tile() {
        let mut state = fixtures::state();
        let extra = |id: &str| SensorReading {
            id: SensorId::new(id),
            label: id.to_owned(),
            celsius: Some(40.0),
        };
        state.readings.sensors = vec![
            extra("cpu/package"),
            extra("cpu/tdie"),
            extra("cpu/ccd1"),
            extra("board/temp1"),
            extra("drive/S1"),
            extra("gpu/nvidia/gpu-1"),
            extra("drive/S2"),
            extra("gpu/nvidia/gpu-2"),
        ];
        let order: Vec<&str> = ordered_sensors(&state)
            .iter()
            .map(|sensor| sensor.id.as_str())
            .collect();
        assert_eq!(
            order,
            vec![
                "cpu/package",
                "gpu/nvidia/gpu-1",
                "drive/S1",
                "board/temp1",
                "cpu/tdie",
                "cpu/ccd1",
                "gpu/nvidia/gpu-2",
                "drive/S2",
            ]
        );
    }

    #[test]
    fn only_connected_unnamed_fans_need_setup() {
        let state = fixtures::state();
        assert_eq!(unnamed_fans(&state, &state.config), vec![fixtures::fan(1)]);
    }
}
