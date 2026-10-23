use eframe::egui::{self, ComboBox, DragValue, Id, Margin, RichText, vec2};
use mostlyfans_core::config::{
    CPU_SENSOR, Config, FanMode, MAX_CURVE_POINTS, MAX_CURVES, MAX_NAME_LEN,
};
use mostlyfans_core::curve::{CurveDef, CurveKind, CurvePoint, MixOp, Tuning};
use mostlyfans_core::state::ControlState;
use mostlyfans_core::{CurveId, SensorId};

use crate::curve_plot::{self, FloorMark, Interaction, Live, PointLimits, Refusal};
use crate::format;
use crate::icons;
use crate::theme::{self, PAGE, SPACE_M, SPACE_S};
use crate::widgets::{self, Segment};

const LIST_WIDTH: f32 = 250.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Smoothing {
    Fast,
    Smooth,
    Gentle,
    Custom,
}

impl Smoothing {
    pub fn tuning(self) -> Option<Tuning> {
        match self {
            Self::Fast => Some(Tuning {
                hysteresis_c: 1.0,
                response_secs: 0.5,
                step_up_per_sec: 50.0,
                step_down_per_sec: 20.0,
            }),
            Self::Smooth => Some(Tuning::default()),
            Self::Gentle => Some(Tuning {
                hysteresis_c: 3.0,
                response_secs: 6.0,
                step_up_per_sec: 10.0,
                step_down_per_sec: 3.0,
            }),
            Self::Custom => None,
        }
    }

    pub fn of(tuning: &Tuning) -> Self {
        [Self::Fast, Self::Smooth, Self::Gentle]
            .into_iter()
            .find(|preset| preset.tuning().as_ref() == Some(tuning))
            .unwrap_or(Self::Custom)
    }

    fn describe(self) -> &'static str {
        match self {
            Self::Fast => "Reacts within a second. Fans change speed often.",
            Self::Smooth => "Follows temperature over a couple of seconds.",
            Self::Gentle => "Slow, quiet changes. Short spikes are ignored.",
            Self::Custom => "Your own values, set under Advanced.",
        }
    }
}

#[derive(Debug, Default)]
pub struct CurveEditor {
    selected: Option<CurveId>,
    plot: Interaction,
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
            self.plot = Interaction::default();
        }
        egui::Panel::left("curve-list")
            .resizable(false)
            .exact_size(LIST_WIDTH)
            .frame(egui::Frame::new().inner_margin(Margin {
                right: SPACE_M as i8,
                ..Margin::ZERO
            }))
            .show(ui, |ui| {
                changed |= self.list(ui, state, draft);
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().inner_margin(Margin {
                left: SPACE_M as i8,
                ..Margin::ZERO
            }))
            .show(ui, |ui| {
                let Some(index) = self
                    .selected
                    .as_ref()
                    .and_then(|id| draft.curves.iter().position(|curve| &curve.id == id))
                else {
                    ui.add_space(SPACE_M * 3.0);
                    ui.vertical_centered(|ui| {
                        ui.label(widgets::value("No curves yet", theme::SECTION));
                        ui.label(widgets::muted(
                            "A curve sets fan speed from a temperature. Create one with New curve.",
                        ));
                    });
                    return;
                };
                egui::ScrollArea::vertical().show(ui, |ui| {
                    if self.editor(ui, state, draft, index) {
                        self.plot.refused = None;
                        changed = true;
                    }
                });
            });
        changed
    }

    fn list(&mut self, ui: &mut egui::Ui, state: &ControlState, draft: &mut Config) -> bool {
        let mut changed = false;
        ui.horizontal(|ui| {
            ui.label(widgets::value("Curves", PAGE));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                changed |= self.new_curve_menu(ui, state, draft);
            });
        });
        ui.add_space(SPACE_S);
        egui::ScrollArea::vertical().show(ui, |ui| {
            for curve in &draft.curves {
                let selected = self.selected.as_ref() == Some(&curve.id);
                let users = curve_users(draft, state, &curve.id).len();
                let response = widgets::list_item(
                    ui,
                    Id::unique(("curve-row", curve.id.as_str())),
                    selected,
                    &curve.name,
                    |ui| {
                        ui.horizontal(|ui| {
                            curve_plot::thumbnail(ui, curve, vec2(44.0, 30.0));
                            ui.vertical(|ui| {
                                ui.label(RichText::new(&curve.name).strong());
                                let usage = match users {
                                    0 => "Not used".to_owned(),
                                    1 => "Used by 1 fan or curve".to_owned(),
                                    count => format!("Used by {count} fans or curves"),
                                };
                                ui.label(widgets::caption(ui, usage));
                            });
                        });
                    },
                );
                if response.clicked() {
                    self.selected = Some(curve.id.clone());
                    self.plot = Interaction::default();
                }
            }
        });
        changed
    }

    fn new_curve_menu(
        &mut self,
        ui: &mut egui::Ui,
        state: &ControlState,
        draft: &mut Config,
    ) -> bool {
        let mut changed = false;
        let full = draft.curves.len() >= MAX_CURVES;
        ui.add_enabled_ui(!full, |ui| {
            ui.menu_button(widgets::with_icon(icons::ADD, "New curve"), |ui| {
                ui.set_min_width(280.0);
                let sensor = default_sensor(state);
                let templates = [
                    (
                        "Custom points",
                        "Drag points to shape the curve.",
                        "Custom",
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
                        "Straight line",
                        "Rises evenly between two temperatures.",
                        "Linear",
                        CurveKind::Linear {
                            sensor,
                            start: CurvePoint::new(40.0, 30.0),
                            end: CurvePoint::new(80.0, 100.0),
                        },
                    ),
                    (
                        "Fixed speed",
                        "The same speed at every temperature.",
                        "Fixed",
                        CurveKind::Flat { duty: 50.0 },
                    ),
                ];
                for (title, description, name, kind) in templates {
                    if menu_entry(ui, title, description, true) {
                        self.add(draft, name, kind);
                        changed = true;
                        ui.close();
                    }
                }
                let can_mix = !draft.curves.is_empty();
                if menu_entry(
                    ui,
                    "Combine curves",
                    if can_mix {
                        "The highest, lowest or average of other curves."
                    } else {
                        "Create another curve first."
                    },
                    can_mix,
                ) {
                    let curves = draft
                        .curves
                        .iter()
                        .take(2)
                        .map(|curve| curve.id.clone())
                        .collect();
                    self.add(
                        draft,
                        "Combined",
                        CurveKind::Mix {
                            op: MixOp::Max,
                            curves,
                        },
                    );
                    changed = true;
                    ui.close();
                }
            });
        });
        changed
    }

    fn add(&mut self, draft: &mut Config, base: &str, kind: CurveKind) {
        let id = (1..)
            .map(|n| CurveId::new(format!("curve-{n}")))
            .find(|id| !draft.curves.iter().any(|curve| &curve.id == id))
            .unwrap_or_else(|| CurveId::new("curve"));
        let name = (1..)
            .map(|n| format!("{base} {n}"))
            .find(|name| !draft.curves.iter().any(|curve| &curve.name == name))
            .unwrap_or_else(|| base.to_owned());
        draft.curves.push(CurveDef {
            id: id.clone(),
            name,
            kind,
            tuning: Tuning::default(),
        });
        self.selected = Some(id);
        self.plot = Interaction::default();
    }

    fn editor(
        &mut self,
        ui: &mut egui::Ui,
        state: &ControlState,
        draft: &mut Config,
        index: usize,
    ) -> bool {
        let mut changed = false;
        let id = draft.curves[index].id.clone();
        let users = curve_users(draft, state, &id);
        let others: Vec<(CurveId, String)> = draft
            .curves
            .iter()
            .filter(|curve| curve.id != id)
            .map(|curve| (curve.id.clone(), curve.name.clone()))
            .collect();
        let floor = floor_mark(draft, state, &id);
        let mut delete = false;

        let curve = &mut draft.curves[index];
        ui.horizontal(|ui| {
            changed |= ui
                .add(
                    egui::TextEdit::singleline(&mut curve.name)
                        .id(Id::unique(("curve-name", id.as_str())))
                        .font(theme::semibold(PAGE))
                        .char_limit(MAX_NAME_LEN)
                        .desired_width(320.0)
                        .frame(egui::Frame::NONE),
                )
                .on_hover_text("Click to rename")
                .changed();
            ui.label(RichText::new(icons::RENAME).color(theme::of(ui).text_muted));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                delete = widgets::confirm(
                    ui,
                    Id::unique(("delete-curve", id.as_str())),
                    &widgets::with_icon(icons::DELETE, "Delete"),
                    "Delete this curve?",
                    users.is_empty(),
                );
            });
        });
        if curve.name.trim().is_empty() {
            ui.label(
                RichText::new(format!("{}  A curve needs a name", icons::WARNING))
                    .color(theme::of(ui).warn),
            );
        }
        if users.is_empty() {
            ui.label(widgets::muted("Not used by any fan yet."));
        } else {
            ui.label(widgets::muted(format!("Used by {}", users.join(", "))));
        }
        ui.add_space(SPACE_M);

        widgets::card(ui, |ui| {
            widgets::card_title(ui, "Shape", None);
            if let Some(sensor) = sensor_of(&mut curve.kind) {
                changed |= sensor_picker(ui, state, &id, sensor);
                ui.add_space(SPACE_S);
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
                    changed |= self.plot(ui, points, PointSet::Graph, &live, floor.as_ref());
                    changed |= point_table(ui, &id, points);
                }
                CurveKind::Linear { start, end, .. } => {
                    let mut points = vec![*start, *end];
                    if self.plot(ui, &mut points, PointSet::Line, &live, floor.as_ref())
                        && points.len() == 2
                    {
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
                        let label = ui.label("Speed");
                        changed |= ui
                            .add(
                                egui::Slider::new(duty, 0.0..=100.0)
                                    .suffix("%")
                                    .fixed_decimals(0)
                                    .clamping(egui::SliderClamping::Edits),
                            )
                            .labelled_by(label.id)
                            .changed();
                    });
                }
                CurveKind::Mix { op, curves } => changed |= mix_editor(ui, op, curves, &others),
            }
        });
        ui.add_space(SPACE_M);
        widgets::card(ui, |ui| {
            changed |= smoothing_editor(ui, &id, &mut curve.tuning);
        });
        if delete {
            draft.curves.retain(|curve| curve.id != id);
            self.selected = None;
            changed = true;
        }
        changed
    }

    fn plot(
        &mut self,
        ui: &mut egui::Ui,
        points: &mut Vec<CurvePoint>,
        set: PointSet,
        live: &Live,
        floor: Option<&FloorMark>,
    ) -> bool {
        let limits = set.limits();
        let changed = curve_plot::show(ui, points, &mut self.plot, limits, live, floor);
        let refusal = self.plot.refused.filter(|refusal| match refusal {
            Refusal::Remove => points.len() <= limits.min,
            Refusal::Add => points.len() >= limits.max,
        });
        if let Some(refusal) = refusal {
            ui.label(
                RichText::new(format!("{}  {}", icons::WARNING, set.refusal(refusal)))
                    .color(theme::of(ui).warn),
            );
        }
        ui.label(widgets::caption(ui, set.caption()));
        changed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PointSet {
    Graph,
    Line,
}

impl PointSet {
    fn limits(self) -> PointLimits {
        match self {
            Self::Graph => PointLimits {
                min: 1,
                max: MAX_CURVE_POINTS,
            },
            Self::Line => PointLimits { min: 2, max: 2 },
        }
    }

    fn caption(self) -> &'static str {
        match self {
            Self::Graph => {
                "Drag a point to move it; hold Shift to snap to 5° and 5% steps. Double-click empty space to add a point. Right-click or double-click a point to remove it."
            }
            Self::Line => "Drag a point to move it; hold Shift to snap to 5° and 5% steps.",
        }
    }

    fn refusal(self, refusal: Refusal) -> String {
        match (self, refusal) {
            (Self::Graph, Refusal::Remove) => "A curve needs at least one point".to_owned(),
            (Self::Graph, Refusal::Add) => {
                format!("A curve can have at most {MAX_CURVE_POINTS} points")
            }
            (Self::Line, _) => "A straight line always has two points".to_owned(),
        }
    }
}

fn menu_entry(ui: &mut egui::Ui, title: &str, description: &str, enabled: bool) -> bool {
    let text = format!("{title}\n{description}");
    let response = ui.add_enabled(
        enabled,
        egui::Button::new(RichText::new(text))
            .frame(false)
            .min_size(vec2(260.0, 0.0)),
    );
    response.clicked()
}

fn floor_mark(config: &Config, state: &ControlState, curve: &CurveId) -> Option<FloorMark> {
    let modes = config.active_modes()?;
    modes
        .iter()
        .filter(|(_, mode)| matches!(mode, FanMode::Curve { curve: used } if used == curve))
        .map(|(fan, _)| {
            let limits = state.limits.get(fan).copied().unwrap_or_default();
            let floor = format::effective_floor(
                limits,
                &config.settings(fan),
                state.config.calibration_for(fan),
            );
            (format::fan_name(config, state, fan), floor.percent)
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(name, percent)| FloorMark {
            percent,
            label: format!("{name} never goes below {percent:.0}%"),
        })
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
        .find(|sensor| sensor.id.as_str() == CPU_SENSOR)
        .or_else(|| {
            state
                .readings
                .sensors
                .iter()
                .find(|sensor| !sensor.id.is_display_only())
        })
        .map_or_else(|| SensorId::new(CPU_SENSOR), |sensor| sensor.id.clone())
}

fn sensor_picker(
    ui: &mut egui::Ui,
    state: &ControlState,
    curve: &CurveId,
    sensor: &mut SensorId,
) -> bool {
    let mut changed = false;
    let selected = state
        .readings
        .sensors
        .iter()
        .find(|candidate| &candidate.id == sensor)
        .map_or_else(|| sensor.to_string(), |candidate| candidate.label.clone());
    ui.horizontal(|ui| {
        let label = ui.label("Temperature from");
        ComboBox::from_id_salt(("curve-sensor", curve.as_str()))
            .selected_text(selected)
            .width(200.0)
            .show_ui(ui, |ui| {
                for candidate in state
                    .readings
                    .sensors
                    .iter()
                    .filter(|candidate| !candidate.id.is_display_only())
                {
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
            })
            .response
            .labelled_by(label.id);
        ui.label(widgets::muted(format::celsius(
            state.readings.temperature(sensor),
        )));
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
    let temp = ui
        .add(
            DragValue::new(&mut point.temp)
                .range(0.0..=125.0)
                .speed(0.5)
                .suffix(" °C"),
        )
        .on_hover_text("Temperature");
    let duty = ui
        .add(
            DragValue::new(&mut point.duty)
                .range(0.0..=100.0)
                .speed(0.5)
                .suffix(" %"),
        )
        .on_hover_text("Fan speed");
    temp.changed() || duty.changed()
}

fn point_table(ui: &mut egui::Ui, curve: &CurveId, points: &mut Vec<CurvePoint>) -> bool {
    let mut changed = false;
    let mut remove = None;
    ui.add_space(SPACE_S);
    let can_remove = points.len() > 1;
    egui::Grid::new(("curve-points", curve.as_str()))
        .num_columns(3)
        .spacing(vec2(SPACE_S, SPACE_S))
        .show(ui, |ui| {
            ui.label(widgets::caption(ui, "Temperature"));
            ui.label(widgets::caption(ui, "Speed"));
            ui.label("");
            ui.end_row();
            for (index, point) in points.iter_mut().enumerate() {
                changed |= point_editor(ui, point);
                if ui
                    .add_enabled(can_remove, egui::Button::new(icons::CLOSE).small())
                    .on_hover_text("Remove this point")
                    .clicked()
                {
                    remove = Some(index);
                }
                ui.end_row();
            }
        });
    if ui
        .add_enabled(
            points.len() < MAX_CURVE_POINTS,
            egui::Button::new(widgets::with_icon(icons::ADD, "Add point")),
        )
        .clicked()
    {
        let last = points
            .iter()
            .max_by(|a, b| a.temp.total_cmp(&b.temp))
            .copied()
            .unwrap_or(CurvePoint::new(50.0, 50.0));
        points.push(CurvePoint::new((last.temp + 10.0).min(125.0), last.duty));
        changed = true;
    }
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
    ui.label("Use the");
    changed |= widgets::segmented(
        ui,
        op,
        &[
            Segment::new(MixOp::Max, "Highest"),
            Segment::new(MixOp::Min, "Lowest"),
            Segment::new(MixOp::Average, "Average"),
        ],
    );
    ui.label("of these curves:");
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
        ui.label(
            RichText::new(format!("{}  Select at least one curve.", icons::WARNING))
                .color(theme::of(ui).warn),
        );
    }
    changed
}

fn smoothing_editor(ui: &mut egui::Ui, id: &CurveId, tuning: &mut Tuning) -> bool {
    let mut changed = false;
    widgets::card_title(
        ui,
        "Smoothing",
        Some("How quickly fans follow temperature changes."),
    );
    let current = Smoothing::of(tuning);
    let mut chosen = current;
    widgets::segmented(
        ui,
        &mut chosen,
        &[
            Segment::new(Smoothing::Fast, "Fast"),
            Segment::new(Smoothing::Smooth, "Smooth"),
            Segment::new(Smoothing::Gentle, "Gentle"),
            Segment::new(Smoothing::Custom, "Custom").disabled_because(
                (current != Smoothing::Custom).then_some("Change a value under Advanced"),
            ),
        ],
    );
    if chosen != current
        && let Some(preset) = chosen.tuning()
    {
        *tuning = preset;
        changed = true;
    }
    ui.label(widgets::muted(Smoothing::of(tuning).describe()));
    ui.label(widgets::caption(
        ui,
        "Critical temperature and Identify always go straight to full speed, whatever the smoothing.",
    ));
    ui.add_space(SPACE_S);
    egui::CollapsingHeader::new("Advanced")
        .id_salt(("smoothing-advanced", id.as_str()))
        .show(ui, |ui| {
            egui::Grid::new(("curve-tuning", id.as_str()))
                .num_columns(3)
                .spacing(vec2(SPACE_M, SPACE_S))
                .show(ui, |ui| {
                    let mut row = |ui: &mut egui::Ui,
                                   label: &str,
                                   value: &mut f32,
                                   max: f32,
                                   suffix: &str,
                                   hint: &str| {
                        let label = ui.label(label);
                        changed |= ui
                            .add(
                                DragValue::new(value)
                                    .range(0.0..=max)
                                    .speed(0.1)
                                    .suffix(suffix),
                            )
                            .labelled_by(label.id)
                            .changed();
                        ui.label(widgets::caption(ui, hint));
                        ui.end_row();
                    };
                    row(
                        ui,
                        "Hysteresis",
                        &mut tuning.hysteresis_c,
                        20.0,
                        " °C",
                        "Ignore temperature drops smaller than this.",
                    );
                    row(
                        ui,
                        "Response time",
                        &mut tuning.response_secs,
                        60.0,
                        " s",
                        "Averages the temperature over this time; 0 reacts instantly.",
                    );
                    row(
                        ui,
                        "Max ramp up",
                        &mut tuning.step_up_per_sec,
                        100.0,
                        " %/s",
                        "Fastest speed increase; 0 means unlimited.",
                    );
                    row(
                        ui,
                        "Max ramp down",
                        &mut tuning.step_down_per_sec,
                        100.0,
                        " %/s",
                        "Fastest speed decrease; 0 means unlimited.",
                    );
                });
        });
    changed
}

fn curve_users(config: &Config, state: &ControlState, curve: &CurveId) -> Vec<String> {
    let mut users: Vec<String> = config
        .profiles
        .iter()
        .flat_map(|(profile, settings)| {
            settings
                .fans
                .iter()
                .filter(|(_, mode)| matches!(mode, FanMode::Curve { curve: used } if used == curve))
                .map(move |(fan, _)| {
                    format!("{} in {profile}", format::fan_name(config, state, fan))
                })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    #[test]
    fn presets_round_trip_and_anything_else_is_custom() {
        for preset in [Smoothing::Fast, Smoothing::Smooth, Smoothing::Gentle] {
            let tuning = preset.tuning().unwrap();
            assert_eq!(Smoothing::of(&tuning), preset);
        }
        assert_eq!(Smoothing::of(&Tuning::default()), Smoothing::Smooth);
        let custom = Tuning {
            hysteresis_c: 4.5,
            ..Tuning::default()
        };
        assert_eq!(Smoothing::of(&custom), Smoothing::Custom);
        assert_eq!(Smoothing::Custom.tuning(), None);
    }

    #[test]
    fn the_floor_mark_uses_the_strictest_fan_on_the_curve() {
        let state = fixtures::state();
        let mut config = state.config.clone();
        let mark = floor_mark(&config, &state, &CurveId::new("curve-1")).unwrap();
        assert_eq!(mark.percent, 30.0);
        assert_eq!(mark.label, "CPU cooler never goes below 30%");
        config
            .profiles
            .get_mut(&config.active_profile)
            .unwrap()
            .fans
            .clear();
        assert!(floor_mark(&config, &state, &CurveId::new("curve-1")).is_none());
    }

    #[test]
    fn curve_users_name_the_fan_and_profile() {
        let state = fixtures::state();
        let users = curve_users(&state.config, &state, &CurveId::new("curve-1"));
        assert_eq!(users, vec!["CPU cooler in Balanced"]);
    }

    #[test]
    fn point_sets_match_the_config_limits_and_explain_refusals() {
        let graph = PointSet::Graph.limits();
        assert_eq!((graph.min, graph.max), (1, MAX_CURVE_POINTS));
        let line = PointSet::Line.limits();
        assert_eq!((line.min, line.max), (2, 2));
        assert_eq!(
            PointSet::Graph.refusal(Refusal::Add),
            format!("A curve can have at most {MAX_CURVE_POINTS} points")
        );
        assert_eq!(
            PointSet::Line.refusal(Refusal::Remove),
            "A straight line always has two points"
        );
        assert!(!PointSet::Line.caption().contains("Double-click"));
    }
}
