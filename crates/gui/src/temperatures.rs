use std::collections::BTreeSet;
use std::time::Instant;

use eframe::egui::{self, RichText, vec2};
use fanforge_core::SensorId;
use fanforge_core::state::ControlState;
use fanforge_core::telemetry::SensorReading;

use crate::format;
use crate::history::History;
use crate::icons;
use crate::theme::{self, SPACE_M, SPACE_S};
use crate::widgets;

const SPARK_SIZE: egui::Vec2 = vec2(120.0, 22.0);
const LABEL_WIDTH: f32 = 220.0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemperaturesAction {
    Pin(SensorId),
    Unpin(SensorId),
    ResetOverview,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    Cpu,
    Motherboard,
    Gpu,
    Drives,
    Other,
}

impl Group {
    pub fn of(id: &SensorId) -> Self {
        let id = id.as_str();
        if id.starts_with("cpu/") {
            Self::Cpu
        } else if id.starts_with("board/") {
            Self::Motherboard
        } else if id.starts_with("gpu/") {
            Self::Gpu
        } else if id.starts_with("drive/") {
            Self::Drives
        } else {
            Self::Other
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Cpu => "CPU",
            Self::Motherboard => "Motherboard",
            Self::Gpu => "GPU",
            Self::Drives => "Drives",
            Self::Other => "Other",
        }
    }
}

pub struct TemperaturesPage<'a> {
    pub state: &'a ControlState,
    pub history: &'a History,
    pub pinned: &'a BTreeSet<SensorId>,
    pub customised: bool,
}

fn core_index(id: &SensorId) -> Option<usize> {
    id.as_str()
        .strip_prefix(fanforge_core::ids::CORE_SENSOR_PREFIX)
        .and_then(|index| index.parse().ok())
}

pub fn grouped(state: &ControlState) -> Vec<(Group, Vec<&SensorReading>)> {
    let mut groups: Vec<(Group, Vec<&SensorReading>)> = Vec::new();
    for sensor in &state.readings.sensors {
        let group = Group::of(&sensor.id);
        match groups.iter_mut().find(|(existing, _)| *existing == group) {
            Some((_, sensors)) => sensors.push(sensor),
            None => groups.push((group, vec![sensor])),
        }
    }
    groups.sort_by_key(|(group, _)| *group);
    for (_, sensors) in &mut groups {
        sensors.sort_by_key(|sensor| core_index(&sensor.id).map_or((0, 0), |index| (1, index)));
    }
    groups
}

pub fn used_by<'a>(state: &'a ControlState, id: &SensorId) -> Vec<&'a str> {
    state
        .config
        .curves
        .iter()
        .filter(|curve| curve.kind.sensor() == Some(id))
        .map(|curve| curve.name.as_str())
        .collect()
}

pub fn unconnected(state: &ControlState, history: &History, sensor: &SensorReading) -> bool {
    Group::of(&sensor.id) == Group::Motherboard
        && history.extremes(&sensor.id).is_none()
        && sensor.id != state.config.safety.critical_sensor
        && used_by(state, &sensor.id).is_empty()
}

pub fn show(ui: &mut egui::Ui, page: &TemperaturesPage<'_>) -> Option<TemperaturesAction> {
    let mut action = None;
    let now = Instant::now();
    ui.horizontal(|ui| {
        widgets::page_title(
            ui,
            "Temperatures",
            Some("Every sensor FanForge can read. Pin the ones you want on the Overview."),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
            if ui
                .add_enabled(
                    page.customised,
                    egui::Button::new("Reset Overview to automatic"),
                )
                .on_hover_text("Show the automatic selection of temperatures and every fan on the Overview again")
                .clicked()
            {
                action = Some(TemperaturesAction::ResetOverview);
            }
        });
    });
    ui.add_space(SPACE_M);
    let groups = grouped(page.state);
    if groups.is_empty() {
        ui.label(widgets::muted("No temperature sensors yet."));
        return action;
    }
    for (group, sensors) in groups {
        let (hidden, shown): (Vec<&SensorReading>, Vec<&SensorReading>) = sensors
            .into_iter()
            .partition(|sensor| unconnected(page.state, page.history, sensor));
        widgets::card(ui, |ui| {
            widgets::card_title(ui, group.title(), None);
            rows(ui, page, group, "shown", &shown, now, &mut action);
            if !hidden.is_empty() {
                egui::CollapsingHeader::new(format!("Unconnected ({})", hidden.len()))
                    .id_salt(("unconnected", group.title()))
                    .show(ui, |ui| {
                        ui.label(widgets::caption(
                            ui,
                            "These channels haven't given a usable reading since FanForge started; usually nothing is plugged in.",
                        ));
                        rows(ui, page, group, "hidden", &hidden, now, &mut action);
                    });
            }
        });
        ui.add_space(SPACE_S);
    }
    action
}

fn rows(
    ui: &mut egui::Ui,
    page: &TemperaturesPage<'_>,
    group: Group,
    part: &str,
    sensors: &[&SensorReading],
    now: Instant,
    action: &mut Option<TemperaturesAction>,
) {
    if sensors.is_empty() {
        return;
    }
    egui::Grid::new(("temperatures", group.title(), part))
        .num_columns(5)
        .spacing(vec2(SPACE_M * 1.5, SPACE_S))
        .min_col_width(0.0)
        .show(ui, |ui| {
            for sensor in sensors {
                if let Some(next) = row(ui, page, sensor, now) {
                    *action = Some(next);
                }
                ui.end_row();
            }
        });
}

fn row(
    ui: &mut egui::Ui,
    page: &TemperaturesPage<'_>,
    sensor: &SensorReading,
    now: Instant,
) -> Option<TemperaturesAction> {
    let palette = theme::of(ui);
    let state = page.state;
    ui.vertical(|ui| {
        ui.set_min_width(LABEL_WIDTH);
        ui.label(RichText::new(&sensor.label).strong())
            .on_hover_text(sensor.id.as_str());
        ui.horizontal_wrapped(|ui| {
            if sensor.id == state.config.safety.critical_sensor {
                widgets::chip(ui, "Safety sensor");
            }
            if sensor.id.is_display_only() {
                widgets::chip(ui, "Display only")
                    .on_hover_text("Per-core readings are for display. Fans and the critical override use the CPU temperature.");
            }
            for curve in used_by(state, &sensor.id) {
                widgets::chip(ui, &format!("Used by {curve}"));
            }
        });
    });
    ui.label(widgets::value(
        format::celsius(sensor.celsius),
        theme::SECTION,
    ));
    let extremes = page.history.extremes(&sensor.id).map_or_else(
        || "min – · max –".to_owned(),
        |(low, high)| format!("min {low:.0} · max {high:.0} °C"),
    );
    ui.label(widgets::caption(ui, extremes));
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
        SPARK_SIZE,
        &points,
        (middle - half, middle + half),
        palette.accent,
    );
    let pinned = page.pinned.contains(&sensor.id);
    let (icon, text, hint) = if pinned {
        (icons::UNPIN, "Pinned", "Remove from the Overview")
    } else {
        (icons::PIN, "Pin", "Show on the Overview")
    };
    let clicked = ui
        .selectable_label(pinned, widgets::with_icon(icon, text))
        .on_hover_text(hint)
        .clicked();
    clicked.then(|| {
        if pinned {
            TemperaturesAction::Unpin(sensor.id.clone())
        } else {
            TemperaturesAction::Pin(sensor.id.clone())
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::fixtures;
    use fanforge_core::curve::{CurveDef, CurveKind, CurvePoint, Tuning};

    fn sensor(id: &str, celsius: Option<f32>) -> SensorReading {
        SensorReading {
            id: SensorId::new(id),
            label: id.to_owned(),
            celsius,
        }
    }

    #[test]
    fn sensors_group_by_source_and_cores_sort_numerically() {
        let mut state = fixtures::state();
        state.readings.sensors = vec![
            sensor("cpu/package", Some(50.0)),
            sensor("board/temp1", Some(30.0)),
            sensor("cpu/core0", Some(45.0)),
            sensor("cpu/core10", Some(45.0)),
            sensor("cpu/core2", Some(45.0)),
            sensor("drive/S1", Some(35.0)),
            sensor("gpu/nvidia/x", Some(40.0)),
            sensor("misc", Some(20.0)),
        ];
        let groups: Vec<(Group, Vec<&str>)> = grouped(&state)
            .into_iter()
            .map(|(group, sensors)| {
                (
                    group,
                    sensors.iter().map(|sensor| sensor.id.as_str()).collect(),
                )
            })
            .collect();
        assert_eq!(
            groups,
            vec![
                (
                    Group::Cpu,
                    vec!["cpu/package", "cpu/core0", "cpu/core2", "cpu/core10"]
                ),
                (Group::Motherboard, vec!["board/temp1"]),
                (Group::Gpu, vec!["gpu/nvidia/x"]),
                (Group::Drives, vec!["drive/S1"]),
                (Group::Other, vec!["misc"]),
            ]
        );
    }

    #[test]
    fn dead_board_channels_are_unconnected_unless_something_uses_them() {
        let mut state = fixtures::state();
        state.readings.sensors = vec![
            sensor("board/temp1", Some(30.0)),
            sensor("board/temp2", None),
            sensor("board/temp3", Some(-55.0)),
            sensor("board/temp4", None),
            sensor("drive/S1", None),
        ];
        state.config.safety.critical_sensor = SensorId::new("board/temp4");
        state.config.curves.push(CurveDef {
            id: "c".into(),
            name: "Case".into(),
            kind: CurveKind::Graph {
                sensor: SensorId::new("board/temp3"),
                points: vec![CurvePoint::new(30.0, 30.0)],
            },
            tuning: Tuning::default(),
        });
        let state = Arc::new(state);
        let mut history = History::default();
        history.record(&state, Instant::now());
        let hidden: Vec<&str> = state
            .readings
            .sensors
            .iter()
            .filter(|sensor| unconnected(&state, &history, sensor))
            .map(|sensor| sensor.id.as_str())
            .collect();
        assert_eq!(hidden, vec!["board/temp2"]);
        assert_eq!(used_by(&state, &SensorId::new("board/temp3")), vec!["Case"]);
    }
}
