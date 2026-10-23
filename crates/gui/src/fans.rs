use eframe::egui::{self, ComboBox, Id, Margin, RichText, Slider, SliderClamping, Stroke};
use fanforge_core::calibration::{
    CalibrationOutcome, CalibrationProgress, FanCalibration, MARGIN, Phase,
};
use fanforge_core::config::{Config, FanMode, FanSettings, MAX_NAME_LEN};
use fanforge_core::engine::FanState;
use fanforge_core::state::ControlState;
use fanforge_core::telemetry::FanReading;
use fanforge_core::{Duty, FanId, FanKind};

use crate::connection::{ControlLink, Request};
use crate::curve_plot::{self, Live};
use crate::format;
use crate::icons;
use crate::overview::unnamed_fans;
use crate::theme::{self, PAGE, SPACE_M, SPACE_S, Tone};
use crate::widgets::{self, Segment};

const LIST_WIDTH: f32 = 250.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModeKind {
    Bios,
    Fixed,
    Curve,
    Follow,
}

impl ModeKind {
    fn of(mode: &FanMode) -> Self {
        match mode {
            FanMode::Auto => Self::Bios,
            FanMode::Manual { .. } => Self::Fixed,
            FanMode::Curve { .. } => Self::Curve,
            FanMode::Sync { .. } => Self::Follow,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderType {
    CpuOrPump,
    Case,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    Board,
    SecondChip,
    Gpu,
}

impl Group {
    fn of(fan: &FanId) -> Self {
        match fan.kind() {
            FanKind::Gpu => Self::Gpu,
            FanKind::Header if fan.header_channel().is_some_and(|(chip, _)| chip > 0) => {
                Self::SecondChip
            }
            FanKind::Header => Self::Board,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Board => "Motherboard",
            Self::SecondChip => "Motherboard (second chip)",
            Self::Gpu => "Graphics card",
        }
    }
}

const SECOND_CHIP_NOTE: &str = "FanForge reads these fans but doesn't control them yet. On some Gigabyte boards their speeds may read low or 0.";

pub fn absent_gpu_fans(config: &Config, state: &ControlState) -> Vec<FanId> {
    if state.gpus.fans.is_empty() {
        return Vec::new();
    }
    let mut absent: Vec<FanId> = config
        .fans
        .keys()
        .chain(
            config
                .profiles
                .values()
                .flat_map(|profile| profile.fans.keys()),
        )
        .filter(|fan| fan.kind() == FanKind::Gpu && state.readings.fan(fan).is_none())
        .cloned()
        .collect();
    absent.sort();
    absent.dedup();
    absent
}

pub fn forget(config: &mut Config, fan: &FanId) {
    config.fans.remove(fan);
    config.calibration.remove(fan);
    for profile in config.profiles.values_mut() {
        profile.fans.remove(fan);
        for mode in profile.fans.values_mut() {
            if matches!(mode, FanMode::Sync { fan: leader } if leader == fan) {
                *mode = FanMode::Auto;
            }
        }
    }
}

impl HeaderType {
    pub fn of(settings: &FanSettings) -> Self {
        if settings.case_fan {
            Self::Case
        } else {
            Self::CpuOrPump
        }
    }

    pub fn apply(self, settings: &mut FanSettings) {
        settings.case_fan = self == Self::Case;
        settings.allow_stop &= settings.case_fan;
    }
}

pub struct FanPage<'a> {
    pub state: &'a ControlState,
    pub draft: &'a mut Config,
    pub link: &'a dyn ControlLink,
    pub identify_block: Option<&'a str>,
    pub gpu_identify_block: Option<&'a str>,
    pub setup_skipped: bool,
    pub pinned: &'a [FanId],
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FansOutcome {
    pub changed: bool,
    pub skip_setup: bool,
    pub resume_setup: bool,
    pub pin: Option<(FanId, bool)>,
}

#[derive(Debug, Default)]
pub struct FansView {
    selected: Option<FanId>,
    confirm_case: Option<FanId>,
}

impl FansView {
    pub fn select(&mut self, fan: FanId) {
        self.selected = Some(fan);
        self.confirm_case = None;
    }

    pub fn show(&mut self, ui: &mut egui::Ui, page: FanPage<'_>) -> FansOutcome {
        let mut outcome = FansOutcome::default();
        let state = page.state;
        if state.readings.fans.is_empty() {
            widgets::page_title(ui, "Fans", None);
            ui.label(widgets::muted("No fan headers found yet."));
            return outcome;
        }
        if self
            .selected
            .as_ref()
            .is_none_or(|fan| state.readings.fan(fan).is_none())
        {
            self.selected = state.readings.fans.first().map(|fan| fan.id.clone());
        }
        let unnamed = unnamed_fans(state, &state.config);
        egui::Panel::left("fan-list")
            .resizable(false)
            .exact_size(LIST_WIDTH)
            .frame(egui::Frame::new().inner_margin(Margin {
                right: SPACE_M as i8,
                ..Margin::ZERO
            }))
            .show(ui, |ui| {
                widgets::page_title(ui, "Fans", None);
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let grouped = state
                        .readings
                        .fans
                        .iter()
                        .any(|reading| Group::of(&reading.id) != Group::Board);
                    let mut group = None;
                    for reading in &state.readings.fans {
                        let kind = Group::of(&reading.id);
                        if grouped && group != Some(kind) {
                            if group.is_some() {
                                ui.add_space(SPACE_S);
                            }
                            ui.label(widgets::caption(ui, kind.name()));
                            if kind == Group::SecondChip {
                                ui.label(widgets::caption(ui, SECOND_CHIP_NOTE));
                            }
                            group = Some(kind);
                        }
                        self.list_row(ui, page.draft, state, reading);
                    }
                    let absent = absent_gpu_fans(page.draft, state);
                    if !absent.is_empty() {
                        ui.add_space(SPACE_S);
                        ui.label(widgets::caption(ui, "Not present"));
                    }
                    for fan in absent {
                        ui.horizontal(|ui| {
                            ui.label(widgets::muted(format::fan_name(page.draft, state, &fan)));
                            if ui
                                .small_button("Forget")
                                .on_hover_text(
                                    "Remove this fan's settings; its graphics card is gone",
                                )
                                .clicked()
                            {
                                forget(page.draft, &fan);
                                outcome.changed = true;
                            }
                        });
                    }
                    if page.setup_skipped && !unnamed.is_empty() {
                        ui.add_space(SPACE_S);
                        if ui.link("Show the setup guide").clicked() {
                            outcome.resume_setup = true;
                        }
                    }
                });
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().inner_margin(Margin {
                left: SPACE_M as i8,
                ..Margin::ZERO
            }))
            .show(ui, |ui| {
                let Some(fan) = self.selected.clone() else {
                    return;
                };
                egui::ScrollArea::vertical().show(ui, |ui| {
                    let detail = self.detail(ui, &page, &fan, &unnamed);
                    outcome.skip_setup |= detail.skip_setup;
                    if let Some(pinned) = detail.pin {
                        outcome.pin = Some((fan.clone(), pinned));
                    }
                    if let Some(next) = detail.next {
                        self.select(next);
                    }
                    if let Some(settings) = detail.settings {
                        page.draft.fans.insert(fan.clone(), settings);
                        outcome.changed = true;
                    }
                    if let Some(mode) = detail.mode
                        && let Some(profile) =
                            page.draft.profiles.get_mut(&page.draft.active_profile)
                    {
                        profile.fans.insert(fan.clone(), mode);
                        outcome.changed = true;
                    }
                });
            });
        outcome
    }

    fn list_row(
        &mut self,
        ui: &mut egui::Ui,
        draft: &Config,
        state: &ControlState,
        reading: &FanReading,
    ) {
        let name = format::fan_name(draft, state, &reading.id);
        let selected = self.selected.as_ref() == Some(&reading.id);
        let connected = format::connected(reading);
        let subtitle = if let Some(reason) = &reading.unavailable {
            format!("Not available: {reason}")
        } else if reading.monitor_only {
            format::rpm(reading)
        } else if !connected {
            "No fan connected".to_owned()
        } else if reading.id.kind() == FanKind::Header
            && !format::is_named(&state.config, &reading.id)
        {
            format!("{} · not set up", format::rpm(reading))
        } else {
            format::rpm(reading)
        };
        let response = widgets::list_item(
            ui,
            Id::unique(("fan-row", reading.id.as_str())),
            selected,
            &name,
            |ui| {
                let palette = theme::of(ui);
                let color = if connected {
                    palette.text
                } else {
                    palette.text_muted
                };
                ui.label(RichText::new(&name).strong().color(color));
                ui.label(widgets::caption(ui, subtitle));
                if connected {
                    widgets::duty_bar(
                        ui,
                        ui.available_width(),
                        reading.duty.map(Duty::percent),
                        None,
                    );
                }
            },
        );
        if response.clicked() {
            self.select(reading.id.clone());
        }
    }

    fn detail(
        &mut self,
        ui: &mut egui::Ui,
        page: &FanPage<'_>,
        fan: &FanId,
        unnamed: &[FanId],
    ) -> Detail {
        let mut detail = Detail::default();
        let state = page.state;
        let draft = &*page.draft;
        let Some(reading) = state.readings.fan(fan) else {
            return detail;
        };
        let status = state.fans.iter().find(|status| &status.id == fan);
        let likely_cpu = state
            .board
            .as_ref()
            .is_some_and(|board| board.likely_cpu_fan.as_ref() == Some(fan));
        let settings = draft.settings(fan);
        let mut edited = settings.clone();
        let mut touched = false;
        let hardware_limits = state.limits.get(fan).copied().unwrap_or_default();
        let gpu = fan.kind() == FanKind::Gpu;
        let block = if gpu {
            page.gpu_identify_block
        } else {
            page.identify_block
        };

        ui.horizontal(|ui| {
            let mut name = edited.name.clone().unwrap_or_default();
            let edit = ui.add(
                egui::TextEdit::singleline(&mut name)
                    .id(Id::unique(("fan-name", fan.as_str())))
                    .hint_text(&reading.label)
                    .font(theme::semibold(PAGE))
                    .char_limit(MAX_NAME_LEN)
                    .desired_width(320.0)
                    .frame(egui::Frame::NONE),
            );
            let edit = edit.on_hover_text("Click to rename");
            if edit.changed() {
                edited.name = (!name.trim().is_empty()).then_some(name);
                touched = true;
            }
            ui.label(RichText::new(icons::RENAME).color(theme::of(ui).text_muted));
            if !reading.monitor_only {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    identify(ui, page, fan, block, status.map(|status| &status.state));
                });
            }
        });
        let mut subtitle = reading.label.clone();
        if likely_cpu {
            subtitle.push_str(" · usually the CPU fan");
        }
        if gpu {
            subtitle.push_str(&format!(" · minimum {} (driver)", hardware_limits.floor));
        }
        ui.label(widgets::muted(subtitle));
        if let Some(reason) = block.filter(|_| !reading.monitor_only) {
            ui.label(widgets::caption(
                ui,
                format!("Identify is unavailable: {reason}."),
            ));
        }
        ui.add_space(SPACE_S);
        ui.horizontal(|ui| {
            if let Some(status) = status {
                let (text, tone) = format::fan_state(&status.state);
                widgets::pill(ui, tone, &text);
            }
            ui.label(RichText::new(format::rpm(reading)).strong());
            ui.label(widgets::muted(format!(
                "duty {}",
                format::duty(reading.duty)
            )));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let pinned = page.pinned.contains(fan);
                let (icon, text, hint) = if pinned {
                    (
                        icons::UNPIN,
                        "On Overview",
                        "Remove this fan from the Overview",
                    )
                } else {
                    (
                        icons::PIN,
                        "Pin to Overview",
                        "Show this fan on the Overview",
                    )
                };
                if ui
                    .selectable_label(pinned, widgets::with_icon(icon, text))
                    .on_hover_text(hint)
                    .clicked()
                {
                    detail.pin = Some(!pinned);
                }
            });
        });
        match status.map(|status| &status.state) {
            Some(FanState::Faulted(reason)) => {
                let blocked = if gpu {
                    state.gpus.monitor_only
                } else {
                    state.monitor_only
                };
                ui.add_space(SPACE_S);
                ui.label(widgets::caption(ui, crate::alerts::sentence(reason)));
                if ui
                    .add_enabled(!blocked, egui::Button::new("Try again"))
                    .on_hover_text("Give the fan back to FanForge and check it again")
                    .clicked()
                {
                    page.link.send(Request::RetryFan(fan.clone()));
                }
            }
            Some(FanState::LeaderReleased(reason)) => {
                ui.label(widgets::caption(ui, crate::alerts::sentence(reason)));
            }
            _ => {}
        }
        if gpu && state.gpus.monitor_only && !state.gpus.conflicts.is_empty() {
            ui.label(widgets::caption(
                ui,
                format!(
                    "{} is running, so FanForge isn't controlling the graphics card fans.",
                    state.gpus.conflicts.join(", ")
                ),
            ));
        }
        ui.add_space(SPACE_M);

        if reading.monitor_only {
            widgets::card(ui, |ui| {
                widgets::card_title(ui, "Control", Some("What sets this fan's speed."));
                ui.label(widgets::muted(SECOND_CHIP_NOTE));
            });
            if touched {
                detail.settings = Some(edited);
            }
            return detail;
        }

        if !page.setup_skipped && unnamed.contains(fan) {
            setup_strip(ui, fan, unnamed, &mut detail);
            ui.add_space(SPACE_M);
        }

        let mode = draft.mode(fan);
        if let Some(changed) = control_card(ui, page, fan, reading, &mode, &settings) {
            detail.mode = Some(changed);
        }
        ui.add_space(SPACE_M);

        if !gpu {
            touched |= self.header_card(
                ui,
                fan,
                likely_cpu,
                hardware_limits,
                state.config.calibration_for(fan),
                &mut edited,
            );
            ui.add_space(SPACE_M);
            calibration_card(ui, page, fan, reading);
            ui.add_space(SPACE_M);
        }
        touched |= advanced_card(ui, fan, hardware_limits, &mut edited);
        if touched {
            detail.settings = Some(edited);
        }
        detail
    }

    fn header_card(
        &mut self,
        ui: &mut egui::Ui,
        fan: &FanId,
        likely_cpu: bool,
        limits: fanforge_core::board::FanLimits,
        calibration: Option<&FanCalibration>,
        settings: &mut FanSettings,
    ) -> bool {
        let mut changed = false;
        widgets::card(ui, |ui| {
            widgets::card_title(
                ui,
                "Header type",
                Some("Decides how slow this header may run. A guess never relaxes safety."),
            );
            let current = HeaderType::of(settings);
            let mut chosen = current;
            widgets::segmented(
                ui,
                &mut chosen,
                &[
                    Segment::new(HeaderType::CpuOrPump, "CPU fan or pump"),
                    Segment::new(HeaderType::Case, "Case fan"),
                ],
            );
            if chosen != current {
                if chosen == HeaderType::Case && likely_cpu {
                    self.confirm_case = Some(fan.clone());
                } else {
                    chosen.apply(settings);
                    self.confirm_case = None;
                    changed = true;
                }
            }
            if self.confirm_case.as_ref() == Some(fan) {
                ui.add_space(SPACE_S);
                let palette = theme::of(ui);
                egui::Frame::new()
                    .fill(palette.fill(Tone::Warn))
                    .stroke(Stroke::new(1.0, palette.warn.gamma_multiply(0.45)))
                    .corner_radius(theme::CONTROL_RADIUS)
                    .inner_margin(Margin::same(SPACE_M as i8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(
                            RichText::new(format!(
                                "{}  This header is usually the CPU fan. Treat it as a case fan? Its floor drops to {} and it may be allowed to stop.",
                                icons::WARNING,
                                limits.for_header(true).floor
                            ))
                            .color(palette.text),
                        );
                        ui.horizontal(|ui| {
                            if widgets::danger_button(ui, "Yes, it is a case fan").clicked() {
                                HeaderType::Case.apply(settings);
                                self.confirm_case = None;
                                changed = true;
                            }
                            if ui.button("Cancel").clicked() {
                                self.confirm_case = None;
                            }
                        });
                    });
            }
            let description = match HeaderType::of(settings) {
                HeaderType::CpuOrPump => {
                    format!("Never runs below {} and never stops.", limits.floor)
                }
                HeaderType::Case => format!(
                    "May run down to {}, and stop at 0% if you allow it under Advanced.",
                    limits.for_header(true).floor
                ),
            };
            ui.add_space(SPACE_S);
            ui.label(widgets::muted(description));
            if settings.case_fan && likely_cpu {
                ui.label(
                    RichText::new(format!(
                        "{}  This header is usually the CPU fan",
                        icons::WARNING
                    ))
                    .color(theme::of(ui).warn),
                );
            }
            ui.label(widgets::caption(
                ui,
                format::describe_floor(format::effective_floor(limits, settings, calibration)),
            ));
        });
        changed
    }
}

#[derive(Default)]
struct Detail {
    skip_setup: bool,
    next: Option<FanId>,
    settings: Option<FanSettings>,
    mode: Option<FanMode>,
    pin: Option<bool>,
}

fn calibrating<'a>(page: &'a FanPage<'_>, fan: &FanId) -> Option<&'a CalibrationProgress> {
    page.state
        .calibration
        .active
        .as_ref()
        .filter(|progress| &progress.fan == fan)
}

fn identify(
    ui: &mut egui::Ui,
    page: &FanPage<'_>,
    fan: &FanId,
    block: Option<&str>,
    state: Option<&FanState>,
) {
    if matches!(state, Some(FanState::Identifying)) {
        widgets::spinner_label(ui, "Spinning…");
        return;
    }
    let block = block.or_else(|| calibrating(page, fan).map(|_| "This fan is being calibrated"));
    let button = ui.add_enabled(
        block.is_none(),
        egui::Button::new(widgets::with_icon(icons::IDENTIFY, "Identify")),
    );
    let button = match block {
        Some(reason) => button.on_disabled_hover_text(reason),
        None => button.on_hover_text("Spin this fan at 100% for 3 seconds"),
    };
    if button.clicked() {
        page.link.send(Request::Identify(fan.clone()));
    }
}

fn phase_text(phase: Phase) -> &'static str {
    match phase {
        Phase::StepDown => "Stepping down",
        Phase::Rest => "Letting it stop",
        Phase::FindStart => "Finding the start duty",
    }
}

fn calibration_card(ui: &mut egui::Ui, page: &FanPage<'_>, fan: &FanId, reading: &FanReading) {
    let state = page.state;
    widgets::card(ui, |ui| {
        widgets::card_title(
            ui,
            "Calibration",
            Some(
                "Finds where this fan stops and starts, and raises its floor to match. It never lowers a floor.",
            ),
        );
        if let Some(progress) = calibrating(page, fan) {
            ui.horizontal(|ui| {
                widgets::spinner_label(
                    ui,
                    &format!(
                        "{} · {} · {}",
                        phase_text(progress.phase),
                        progress.duty,
                        progress
                            .rpm
                            .map_or_else(|| "no signal".to_owned(), |rpm| format!("{rpm} rpm"))
                    ),
                );
                if ui.button("Cancel calibration").clicked() {
                    page.link.send(Request::CancelCalibration);
                }
            });
            ui.label(widgets::caption(
                ui,
                format!(
                    "{} steps measured · {:.0} s",
                    progress.points.len(),
                    progress.elapsed.as_secs_f32()
                ),
            ));
        } else {
            let other = state.calibration.active.as_ref().map(|progress| {
                format!(
                    "{} is being calibrated",
                    format::fan_name(&state.config, state, &progress.fan)
                )
            });
            let block = page
                .identify_block
                .map(str::to_owned)
                .or(other)
                .or_else(|| {
                    reading
                        .rpm
                        .is_none()
                        .then(|| "This header has no RPM signal".to_owned())
                });
            let button = ui.add_enabled(block.is_none(), egui::Button::new("Calibrate"));
            let button = match &block {
                Some(reason) => button.on_disabled_hover_text(reason),
                None => button.on_hover_text("Measure where this fan stops and starts"),
            };
            if button.clicked() {
                page.link.send(Request::Calibrate(fan.clone()));
            }
            if let Some(reason) = &block {
                ui.label(widgets::caption(
                    ui,
                    format!("Calibration is unavailable: {reason}."),
                ));
            }
            ui.label(widgets::caption(
                ui,
                "Steps down from 100% in 5% steps of at least 3 s each. It stays at or above the header floor, and only goes lower, down to 0%, on a case fan allowed to stop. It stops early if a temperature reaches 75 °C.",
            ));
        }
        if let Some(report) = state
            .calibration
            .last
            .as_ref()
            .filter(|report| &report.fan == fan)
        {
            let palette = theme::of(ui);
            let (text, color) = match &report.outcome {
                CalibrationOutcome::Completed(_) => ("Calibration finished".to_owned(), palette.ok),
                CalibrationOutcome::Aborted(reason) => {
                    (format!("Calibration stopped: {reason}"), palette.warn)
                }
                CalibrationOutcome::NotRestarting => (
                    "The fan did not restart, even at 100%. Check that it is connected and working."
                        .to_owned(),
                    palette.danger,
                ),
                CalibrationOutcome::Cancelled => {
                    ("Calibration cancelled".to_owned(), palette.text_muted)
                }
            };
            ui.add_space(SPACE_S);
            ui.label(RichText::new(text).color(color));
        }
        if let Some(result) = state.config.calibration_for(fan) {
            ui.add_space(SPACE_S);
            calibration_result(ui, fan, result);
        }
    });
}

fn calibration_result(ui: &mut egui::Ui, fan: &FanId, result: &FanCalibration) {
    let stop = result.stop_duty.map_or_else(
        || format!("Keeps spinning down to {}", result.lowest_tested),
        |duty| format!("Stops at {duty}"),
    );
    let start = result
        .start_duty
        .map(|duty| format!(" · starts at {duty}"))
        .unwrap_or_default();
    ui.label(RichText::new(format!("{stop}{start}")).strong());
    let raise = match result.floor() {
        Some(floor) => {
            format!("Floor raised to at least {floor} ({MARGIN:.0} points above the stop duty).")
        }
        None => "The header floor is unchanged.".to_owned(),
    };
    ui.label(widgets::muted(raise));
    if result.not_restarting {
        ui.label(
            RichText::new(format!(
                "{}  It did not restart, even at 100%. Check that it is connected and working, then calibrate again.",
                icons::WARNING
            ))
            .color(theme::of(ui).danger),
        );
    }
    if result.points.is_empty() {
        return;
    }
    egui::CollapsingHeader::new("Measured speeds")
        .id_salt(("calibration-points", fan.as_str()))
        .show(ui, |ui| {
            egui::Grid::new(("calibration-grid", fan.as_str()))
                .num_columns(2)
                .striped(true)
                .show(ui, |ui| {
                    ui.label(widgets::muted("Duty"));
                    ui.label(widgets::muted("Speed"));
                    ui.end_row();
                    for point in &result.points {
                        ui.label(point.duty.to_string());
                        ui.label(format!("{} rpm", point.rpm));
                        ui.end_row();
                    }
                });
        });
}

fn setup_strip(ui: &mut egui::Ui, fan: &FanId, unnamed: &[FanId], detail: &mut Detail) {
    let palette = theme::of(ui);
    egui::Frame::new()
        .fill(palette.fill(Tone::Accent))
        .stroke(Stroke::new(1.0, palette.accent.gamma_multiply(0.45)))
        .corner_radius(theme::CARD_RADIUS)
        .inner_margin(Margin::same(SPACE_M as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let position = unnamed.iter().position(|id| id == fan).unwrap_or(0) + 1;
            ui.label(
                RichText::new(format!("Fan setup · {position} of {}", unnamed.len()))
                    .strong()
                    .color(palette.text),
            );
            for step in [
                "1. Click Identify: this fan spins at full speed for 3 seconds.",
                "2. Give it a name you will recognise.",
                "3. Choose its header type below, then apply your changes.",
            ] {
                ui.label(widgets::muted(step));
            }
            ui.horizontal(|ui| {
                let next = unnamed
                    .iter()
                    .cycle()
                    .skip_while(|id| *id != fan)
                    .nth(1)
                    .filter(|next| *next != fan);
                if let Some(next) = next
                    && ui.button("Next fan").clicked()
                {
                    detail.next = Some(next.clone());
                }
                if ui.button("Skip setup").clicked() {
                    detail.skip_setup = true;
                }
            });
        });
}

pub fn follow_candidates(
    draft: &Config,
    state: &ControlState,
    fan: &FanId,
) -> Vec<(FanId, String)> {
    state
        .readings
        .fans
        .iter()
        .filter(|other| &other.id != fan && other.id.kind() == fan.kind() && !other.monitor_only)
        .map(|other| (other.id.clone(), format::fan_name(draft, state, &other.id)))
        .collect()
}

fn control_card(
    ui: &mut egui::Ui,
    page: &FanPage<'_>,
    fan: &FanId,
    reading: &FanReading,
    mode: &FanMode,
    settings: &FanSettings,
) -> Option<FanMode> {
    let state = page.state;
    let draft = &*page.draft;
    let mut result = None;
    widgets::card(ui, |ui| {
        widgets::card_title(ui, "Control", Some("What sets this fan's speed."));
        let others = follow_candidates(draft, state, fan);
        let current = ModeKind::of(mode);
        let mut kind = current;
        widgets::segmented(
            ui,
            &mut kind,
            &[
                Segment::new(ModeKind::Bios, format::auto_name(fan)),
                Segment::new(ModeKind::Fixed, "Fixed"),
                Segment::new(ModeKind::Curve, "Curve").disabled_because(
                    draft
                        .curves
                        .is_empty()
                        .then_some("Create a curve on the Curves page first"),
                ),
                Segment::new(ModeKind::Follow, "Follow").disabled_because(
                    others
                        .is_empty()
                        .then_some("There is no other fan to follow"),
                ),
            ],
        );
        let mut mode = mode.clone();
        if kind != current {
            mode = match kind {
                ModeKind::Bios => FanMode::Auto,
                ModeKind::Fixed => FanMode::Manual {
                    duty: reading.duty.unwrap_or(Duty::new(50.0)),
                },
                ModeKind::Curve => FanMode::Curve {
                    curve: draft
                        .curves
                        .first()
                        .map(|curve| curve.id.clone())
                        .unwrap_or_else(|| "".into()),
                },
                ModeKind::Follow => FanMode::Sync {
                    fan: others
                        .first()
                        .map(|(id, _)| id.clone())
                        .unwrap_or_else(|| "".into()),
                },
            };
            result = Some(mode.clone());
        }
        ui.add_space(SPACE_S);
        let limits = state.limits.get(fan).copied().unwrap_or_default();
        let floor = format::effective_floor(limits, settings, state.config.calibration_for(fan));
        match &mut mode {
            FanMode::Auto => {
                ui.label(widgets::muted(match fan.kind() {
                    FanKind::Header => {
                        "The motherboard BIOS controls this fan, as if FanForge were not running."
                    }
                    FanKind::Gpu => {
                        "The graphics driver controls this fan with its own curve, including 0 RPM at idle, as if FanForge were not running."
                    }
                }));
            }
            FanMode::Manual { duty } => {
                let mut percent = duty.percent();
                let slider = ui.add(
                    Slider::new(&mut percent, 0.0..=100.0)
                        .suffix("%")
                        .fixed_decimals(0)
                        .clamping(SliderClamping::Edits)
                        .text("Speed"),
                );
                if slider.changed() {
                    *duty = Duty::new(percent);
                    result = Some(mode.clone());
                }
                if percent < floor.percent && !(floor.may_stop && percent == 0.0) {
                    ui.label(
                        RichText::new(format!(
                            "{}  Held at the {:.0}% floor",
                            icons::WARNING,
                            floor.percent
                        ))
                        .color(theme::of(ui).warn),
                    );
                } else {
                    ui.label(widgets::caption(ui, format::describe_floor(floor)));
                }
            }
            FanMode::Curve { curve } => {
                let selected = draft
                    .curves
                    .iter()
                    .find(|candidate| &candidate.id == curve)
                    .map_or_else(|| curve.to_string(), |candidate| candidate.name.clone());
                ui.horizontal(|ui| {
                    let label = ui.label("Curve");
                    ComboBox::from_id_salt(("curve", fan.as_str()))
                        .selected_text(selected)
                        .show_ui(ui, |ui| {
                            for candidate in &draft.curves {
                                if ui
                                    .selectable_label(&candidate.id == curve, &candidate.name)
                                    .clicked()
                                    && &candidate.id != curve
                                {
                                    *curve = candidate.id.clone();
                                    result = Some(FanMode::Curve {
                                        curve: candidate.id.clone(),
                                    });
                                }
                            }
                        })
                        .response
                        .labelled_by(label.id);
                });
                if let Some(definition) =
                    draft.curves.iter().find(|candidate| &candidate.id == curve)
                {
                    let live = Live {
                        temp: definition
                            .kind
                            .sensor()
                            .and_then(|sensor| state.readings.temperature(sensor)),
                        output: state
                            .fans
                            .iter()
                            .find(|status| &status.id == fan)
                            .and_then(|status| status.output)
                            .map(Duty::percent),
                    };
                    curve_plot::preview(ui, definition, &live, Some(floor.percent));
                }
                ui.label(widgets::caption(ui, format::describe_floor(floor)));
            }
            FanMode::Sync { fan: leader } => {
                let selected = others
                    .iter()
                    .find(|(id, _)| id == leader)
                    .map_or_else(|| leader.to_string(), |(_, name)| name.clone());
                ui.horizontal(|ui| {
                    let label = ui.label("Follow");
                    ComboBox::from_id_salt(("sync", fan.as_str()))
                        .selected_text(selected)
                        .show_ui(ui, |ui| {
                            for (id, name) in &others {
                                if ui.selectable_label(id == leader, name).clicked() && id != leader
                                {
                                    result = Some(FanMode::Sync { fan: id.clone() });
                                }
                            }
                        })
                        .response
                        .labelled_by(label.id);
                });
                ui.label(widgets::caption(
                    ui,
                    format!(
                        "Runs at the same duty as the fan it follows. {}",
                        format::describe_floor(floor)
                    ),
                ));
            }
        }
    });
    result
}

fn advanced_card(
    ui: &mut egui::Ui,
    fan: &FanId,
    limits: fanforge_core::board::FanLimits,
    settings: &mut FanSettings,
) -> bool {
    let mut changed = false;
    widgets::card(ui, |ui| {
        egui::CollapsingHeader::new(widgets::value("Advanced", theme::SECTION))
            .id_salt(("advanced", fan.as_str()))
            .show(ui, |ui| {
                egui::Grid::new(("advanced-grid", fan.as_str()))
                    .num_columns(2)
                    .spacing(egui::vec2(SPACE_M * 2.0, SPACE_M))
                    .show(ui, |ui| {
                        let label = ui.label("Minimum duty");
                        ui.vertical(|ui| {
                            let mut percent = settings.min_duty.percent();
                            if ui
                                .add(Slider::new(&mut percent, 0.0..=100.0).suffix("%").fixed_decimals(0).clamping(SliderClamping::Edits))
                                .labelled_by(label.id)
                                .changed()
                            {
                                settings.min_duty = Duty::new(percent);
                                changed = true;
                            }
                            let source = match fan.kind() {
                                FanKind::Header => "header floor",
                                FanKind::Gpu => "driver minimum",
                            };
                            ui.label(widgets::caption(
                                ui,
                                format!(
                                    "Raises the floor. The {source} of {} always applies.",
                                    limits.for_header(settings.case_fan).floor
                                ),
                            ));
                        });
                        ui.end_row();

                        let label = ui.label("Spin-up duty");
                        ui.vertical(|ui| {
                            let mut percent = settings.spin_up_duty.percent();
                            if ui
                                .add(Slider::new(&mut percent, 0.0..=100.0).suffix("%").fixed_decimals(0).clamping(SliderClamping::Edits))
                                .labelled_by(label.id)
                                .changed()
                            {
                                settings.spin_up_duty = Duty::new(percent);
                                changed = true;
                            }
                            ui.label(widgets::caption(
                                ui,
                                "Used briefly to restart a stopped fan.",
                            ));
                        });
                        ui.end_row();

                        if fan.kind() == FanKind::Gpu {
                            return;
                        }
                        ui.label("Stopping");
                        ui.vertical(|ui| {
                            let permitted = limits.for_header(settings.case_fan).stop_permitted;
                            let response = ui.add_enabled(
                                permitted,
                                egui::Checkbox::new(
                                    &mut settings.allow_stop,
                                    "Allow 0 RPM when the curve asks for 0%",
                                ),
                            );
                            changed |= response.changed();
                            if !permitted {
                                ui.label(widgets::caption(
                                    ui,
                                    "Only case fans may stop. Mark this header as a case fan first.",
                                ));
                            }
                        });
                        ui.end_row();

                        ui.label("Response check");
                        ui.vertical(|ui| {
                            changed |= ui
                                .checkbox(
                                    &mut settings.response_check,
                                    "Hand the fan back to the BIOS if it stops or doesn't follow its duty",
                                )
                                .changed();
                            ui.label(widgets::caption(
                                ui,
                                "Turn off for a pump or a fan that runs at a fixed speed.",
                            ));
                        });
                        ui.end_row();
                    });
            });
    });
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forgetting_a_gpu_fan_clears_its_settings_and_followers() {
        let gpu = FanId::for_gpu_fan("gpu/nvidia/gpu-old", 0);
        let second = FanId::for_gpu_fan("gpu/nvidia/gpu-old", 1);
        let mut config = Config::default();
        config.fans.insert(gpu.clone(), FanSettings::default());
        for profile in config.profiles.values_mut() {
            profile.fans.insert(
                gpu.clone(),
                FanMode::Manual {
                    duty: Duty::new(50.0),
                },
            );
            profile
                .fans
                .insert(second.clone(), FanMode::Sync { fan: gpu.clone() });
        }
        let state = crate::fixtures::with_gpu(crate::fixtures::state());
        assert_eq!(
            absent_gpu_fans(&config, &state),
            vec![gpu.clone(), second.clone()]
        );
        forget(&mut config, &gpu);
        assert!(!config.fans.contains_key(&gpu));
        for profile in config.profiles.values() {
            assert!(!profile.fans.contains_key(&gpu));
            assert_eq!(profile.fans[&second], FanMode::Auto);
        }
        config.validate().unwrap();
    }

    #[test]
    fn the_header_type_round_trips_through_the_case_fan_flag() {
        for kind in [HeaderType::CpuOrPump, HeaderType::Case] {
            let mut settings = FanSettings::default();
            kind.apply(&mut settings);
            assert_eq!(HeaderType::of(&settings), kind);
        }
    }

    #[test]
    fn choosing_cpu_fan_or_pump_clears_allow_stop() {
        let mut settings = FanSettings {
            case_fan: true,
            allow_stop: true,
            ..FanSettings::default()
        };
        HeaderType::CpuOrPump.apply(&mut settings);
        assert!(!settings.case_fan);
        assert!(!settings.allow_stop);
        HeaderType::Case.apply(&mut settings);
        assert!(!settings.allow_stop);
    }
}
