use eframe::egui::{self, ComboBox, DragValue, RichText};
use fanforge_core::config::{Config, FanMode};
use fanforge_core::curve::{CurveDef, CurveKind, CurvePoint, MixOp, Tuning};
use fanforge_core::state::ControlState;
use fanforge_core::{CurveId, SensorId};

use crate::curve_plot::{self, Live};
use crate::format;

#[derive(Debug, Default)]
pub struct CurveEditor {
    selected: Option<CurveId>,
    dragging: Option<usize>,
}

impl CurveEditor {
    pub fn show(&mut self, ui: &mut egui::Ui, state: &ControlState, draft: &mut Config) -> bool {
        let mut changed = false;
        if self
            .selected
            .as_ref()
            .is_none_or(|id| !draft.curves.iter().any(|curve| &curve.id == id))
        {
            self.selected = draft.curves.first().map(|curve| curve.id.clone());
        }
        egui::Panel::left("curve-list")
            .resizable(true)
            .default_size(200.0)
            .show(ui, |ui| {
                changed |= self.list(ui, state, draft);
            });
        egui::CentralPanel::default().show(ui, |ui| {
            let Some(index) = self
                .selected
                .as_ref()
                .and_then(|id| draft.curves.iter().position(|curve| &curve.id == id))
            else {
                ui.label("Create a curve to control fans from temperatures.");
                return;
            };
            let others: Vec<(CurveId, String)> = draft
                .curves
                .iter()
                .filter(|curve| curve.id != draft.curves[index].id)
                .map(|curve| (curve.id.clone(), curve.name.clone()))
                .collect();
            egui::ScrollArea::vertical().show(ui, |ui| {
                changed |= self.editor(ui, state, &mut draft.curves[index], &others);
            });
        });
        changed
    }

    fn list(&mut self, ui: &mut egui::Ui, state: &ControlState, draft: &mut Config) -> bool {
        let mut changed = false;
        ui.heading("Curves");
        for curve in &draft.curves {
            if ui
                .selectable_label(self.selected.as_ref() == Some(&curve.id), &curve.name)
                .clicked()
            {
                self.selected = Some(curve.id.clone());
                self.dragging = None;
            }
        }
        ui.separator();
        let sensor = default_sensor(state);
        let templates = [
            (
                "+ Graph",
                CurveKind::Graph {
                    sensor: sensor.clone(),
                    points: vec![
                        CurvePoint::new(30.0, 30.0),
                        CurvePoint::new(50.0, 40.0),
                        CurvePoint::new(70.0, 70.0),
                        CurvePoint::new(85.0, 100.0),
                    ],
                },
            ),
            (
                "+ Linear",
                CurveKind::Linear {
                    sensor,
                    start: CurvePoint::new(40.0, 30.0),
                    end: CurvePoint::new(80.0, 100.0),
                },
            ),
            ("+ Flat", CurveKind::Flat { duty: 50.0 }),
        ];
        ui.horizontal_wrapped(|ui| {
            for (label, kind) in templates {
                if ui.button(label).clicked() {
                    self.add(draft, kind);
                    changed = true;
                }
            }
            if ui
                .add_enabled(!draft.curves.is_empty(), egui::Button::new("+ Mix"))
                .clicked()
            {
                let curves = draft
                    .curves
                    .iter()
                    .take(2)
                    .map(|curve| curve.id.clone())
                    .collect();
                self.add(
                    draft,
                    CurveKind::Mix {
                        op: MixOp::Max,
                        curves,
                    },
                );
                changed = true;
            }
        });
        if let Some(selected) = self.selected.clone() {
            let users = curve_users(draft, &selected);
            let delete = ui.add_enabled(users.is_empty(), egui::Button::new("Delete selected"));
            let delete = if users.is_empty() {
                delete
            } else {
                delete.on_disabled_hover_text(format!("Used by: {}", users.join(", ")))
            };
            if delete.clicked() {
                draft.curves.retain(|curve| curve.id != selected);
                self.selected = None;
                changed = true;
            }
        }
        changed
    }

    fn add(&mut self, draft: &mut Config, kind: CurveKind) {
        let id = (1..)
            .map(|n| CurveId::new(format!("curve-{n}")))
            .find(|id| !draft.curves.iter().any(|curve| &curve.id == id))
            .unwrap_or_else(|| CurveId::new("curve"));
        let name = format!("Curve {}", draft.curves.len() + 1);
        draft.curves.push(CurveDef {
            id: id.clone(),
            name,
            kind,
            tuning: Tuning::default(),
        });
        self.selected = Some(id);
        self.dragging = None;
    }

    fn editor(
        &mut self,
        ui: &mut egui::Ui,
        state: &ControlState,
        curve: &mut CurveDef,
        others: &[(CurveId, String)],
    ) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label("Name");
            changed |= ui.text_edit_singleline(&mut curve.name).changed();
        });
        if let Some(sensor) = sensor_of(&mut curve.kind) {
            changed |= sensor_picker(ui, state, sensor);
        }
        let live = Live {
            temp: curve
                .kind
                .sensor()
                .and_then(|sensor| state.readings.temperature(sensor)),
            output: live_output(state, &curve.id),
        };
        match &mut curve.kind {
            CurveKind::Graph { points, .. } => {
                changed |= self.plot(ui, points, 1, &live);
                changed |= point_table(ui, points);
            }
            CurveKind::Linear { start, end, .. } => {
                let mut points = vec![*start, *end];
                if self.plot(ui, &mut points, 2, &live) && points.len() == 2 {
                    *start = points[0];
                    *end = points[1];
                    changed = true;
                }
                ui.horizontal(|ui| {
                    ui.label("From");
                    changed |= point_editor(ui, start);
                    ui.label("to");
                    changed |= point_editor(ui, end);
                });
            }
            CurveKind::Flat { duty } => {
                ui.horizontal(|ui| {
                    ui.label("Duty");
                    changed |= ui
                        .add(egui::Slider::new(duty, 0.0..=100.0).suffix("%"))
                        .changed();
                });
            }
            CurveKind::Mix { op, curves } => changed |= mix_editor(ui, op, curves, others),
        }
        ui.separator();
        changed |= tuning_editor(ui, &mut curve.tuning);
        changed
    }

    fn plot(
        &mut self,
        ui: &mut egui::Ui,
        points: &mut Vec<CurvePoint>,
        min_points: usize,
        live: &Live,
    ) -> bool {
        let changed = curve_plot::show(ui, points, &mut self.dragging, min_points, live);
        ui.label(
            RichText::new("Drag points to edit. Double-click empty space to add a point, or a point to remove it.")
                .color(format::MUTED),
        );
        changed
    }
}

fn sensor_of(kind: &mut CurveKind) -> Option<&mut SensorId> {
    match kind {
        CurveKind::Graph { sensor, .. } | CurveKind::Linear { sensor, .. } => Some(sensor),
        CurveKind::Flat { .. } | CurveKind::Mix { .. } => None,
    }
}

fn default_sensor(state: &ControlState) -> SensorId {
    state
        .readings
        .sensors
        .iter()
        .find(|sensor| sensor.id.as_str() == "cpu/tctl")
        .or_else(|| state.readings.sensors.first())
        .map_or_else(|| SensorId::new("cpu/tctl"), |sensor| sensor.id.clone())
}

fn sensor_picker(ui: &mut egui::Ui, state: &ControlState, sensor: &mut SensorId) -> bool {
    let mut changed = false;
    let selected = state
        .readings
        .sensors
        .iter()
        .find(|candidate| &candidate.id == sensor)
        .map_or_else(|| sensor.to_string(), |candidate| candidate.label.clone());
    ui.horizontal(|ui| {
        ui.label("Sensor");
        ComboBox::from_id_salt("curve-sensor")
            .selected_text(selected)
            .show_ui(ui, |ui| {
                for candidate in &state.readings.sensors {
                    let label = format!(
                        "{}  ({})",
                        candidate.label,
                        format::celsius(candidate.celsius)
                    );
                    if ui
                        .selectable_label(&candidate.id == sensor, label)
                        .clicked()
                        && &candidate.id != sensor
                    {
                        *sensor = candidate.id.clone();
                        changed = true;
                    }
                }
            });
        let reading = state.readings.temperature(sensor);
        ui.label(format::celsius(reading));
    });
    changed
}

fn live_output(state: &ControlState, curve: &CurveId) -> Option<f32> {
    let modes = state.config.active_modes()?;
    let fan = modes
        .iter()
        .find(|(_, mode)| matches!(mode, FanMode::Curve { curve: used } if used == curve))
        .map(|(fan, _)| fan)?;
    state
        .fans
        .iter()
        .find(|status| &status.id == fan)
        .and_then(|status| status.output)
        .map(|duty| duty.percent())
}

fn point_editor(ui: &mut egui::Ui, point: &mut CurvePoint) -> bool {
    let temp = ui.add(
        DragValue::new(&mut point.temp)
            .range(0.0..=125.0)
            .speed(0.5)
            .suffix(" °C"),
    );
    let duty = ui.add(
        DragValue::new(&mut point.duty)
            .range(0.0..=100.0)
            .speed(0.5)
            .suffix(" %"),
    );
    temp.changed() || duty.changed()
}

fn point_table(ui: &mut egui::Ui, points: &mut Vec<CurvePoint>) -> bool {
    let mut changed = false;
    let mut remove = None;
    egui::CollapsingHeader::new("Points")
        .default_open(false)
        .show(ui, |ui| {
            egui::Grid::new("curve-points")
                .striped(true)
                .show(ui, |ui| {
                    let can_remove = points.len() > 1;
                    for (index, point) in points.iter_mut().enumerate() {
                        changed |= point_editor(ui, point);
                        if ui
                            .add_enabled(can_remove, egui::Button::new("Remove"))
                            .clicked()
                        {
                            remove = Some(index);
                        }
                        ui.end_row();
                    }
                });
            if ui.button("Add point").clicked() {
                let last = points
                    .last()
                    .copied()
                    .unwrap_or(CurvePoint::new(50.0, 50.0));
                points.push(CurvePoint::new((last.temp + 10.0).min(125.0), last.duty));
                changed = true;
            }
        });
    if let Some(index) = remove {
        points.remove(index);
        changed = true;
    }
    changed
}

fn mix_editor(
    ui: &mut egui::Ui,
    op: &mut MixOp,
    curves: &mut Vec<CurveId>,
    others: &[(CurveId, String)],
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label("Combine with");
        for (candidate, label) in [
            (MixOp::Max, "Maximum"),
            (MixOp::Min, "Minimum"),
            (MixOp::Average, "Average"),
        ] {
            changed |= ui.radio_value(op, candidate, label).changed();
        }
    });
    for (id, name) in others {
        let mut included = curves.contains(id);
        if ui.checkbox(&mut included, name).changed() {
            if included {
                curves.push(id.clone());
            } else {
                curves.retain(|existing| existing != id);
            }
            changed = true;
        }
    }
    if curves.is_empty() {
        ui.colored_label(format::WARN, "Select at least one curve.");
    }
    changed
}

fn tuning_editor(ui: &mut egui::Ui, tuning: &mut Tuning) -> bool {
    let mut changed = false;
    egui::Grid::new("curve-tuning")
        .num_columns(2)
        .show(ui, |ui| {
            let mut row = |ui: &mut egui::Ui,
                           label: &str,
                           value: &mut f32,
                           max: f32,
                           suffix: &str,
                           hint: &str| {
                ui.label(label).on_hover_text(hint);
                changed |= ui
                    .add(
                        DragValue::new(value)
                            .range(0.0..=max)
                            .speed(0.1)
                            .suffix(suffix),
                    )
                    .changed();
                ui.end_row();
            };
            row(
                ui,
                "Hysteresis",
                &mut tuning.hysteresis_c,
                20.0,
                " °C",
                "Ignore temperature drops smaller than this",
            );
            row(
                ui,
                "Response time",
                &mut tuning.response_secs,
                60.0,
                " s",
                "Smoothing time constant; 0 reacts instantly",
            );
            row(
                ui,
                "Max ramp up",
                &mut tuning.step_up_per_sec,
                100.0,
                " %/s",
                "0 means unlimited",
            );
            row(
                ui,
                "Max ramp down",
                &mut tuning.step_down_per_sec,
                100.0,
                " %/s",
                "0 means unlimited",
            );
        });
    changed
}

fn curve_users(config: &Config, curve: &CurveId) -> Vec<String> {
    let mut users: Vec<String> = config
        .profiles
        .iter()
        .flat_map(|(profile, settings)| {
            settings
                .fans
                .iter()
                .filter(|(_, mode)| matches!(mode, FanMode::Curve { curve: used } if used == curve))
                .map(move |(fan, _)| format!("{fan} in {profile}"))
        })
        .collect();
    users.extend(
        config
            .curves
            .iter()
            .filter(|other| other.kind.references().contains(curve))
            .map(|other| other.name.clone()),
    );
    users
}
