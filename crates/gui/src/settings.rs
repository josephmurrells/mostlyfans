use std::process::Command;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use eframe::egui::{self, ComboBox, DragValue, Id, RichText, Slider, SliderClamping, vec2};
use mostlyfans_core::config::{
    Config, MAX_INTERVAL_MS, MAX_NAME_LEN, MAX_PROFILES, MIN_INTERVAL_MS, Profile,
};
use mostlyfans_core::state::ControlState;
use mostlyfans_win::paths;

use crate::autostart::{self, AutostartState};
use crate::connection::{ControlLink, Request};
use crate::format;
use crate::icons;
use crate::notices::{Notices, Topic};
use crate::theme::{self, SPACE_M, SPACE_S};
use crate::widgets::{self, Segment};

type AutostartOutcome = (AutostartState, Option<String>);

const SAFETY_LABEL_WIDTH: f32 = 180.0;

#[derive(Debug)]
pub struct AutostartPanel {
    state: AutostartState,
    error: Option<String>,
    pending: Option<Receiver<AutostartOutcome>>,
}

impl AutostartPanel {
    pub fn new(state: AutostartState) -> Self {
        Self {
            state,
            error: None,
            pending: None,
        }
    }

    pub fn repair(&mut self, ctx: &egui::Context) {
        self.apply(true, ctx);
    }

    fn apply(&mut self, enable: bool, ctx: &egui::Context) {
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let ctx = ctx.clone();
        let spawned = thread::Builder::new()
            .name("autostart".into())
            .spawn(move || {
                let result = if enable {
                    autostart::enable()
                } else {
                    autostart::disable()
                };
                let error = result.err().map(|error| error.to_string());
                done_tx.send((autostart::state(), error)).ok();
                ctx.request_repaint();
            });
        match spawned {
            Ok(_) => self.pending = Some(done_rx),
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    pub fn poll(&mut self, notices: &Notices) {
        let Some(pending) = &self.pending else {
            return;
        };
        let outcome = match pending.try_recv() {
            Ok(outcome) => outcome,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => (
                self.state.clone(),
                Some("the autostart change stopped unexpectedly".to_owned()),
            ),
        };
        (self.state, self.error) = outcome;
        self.pending = None;
        if !matches!(self.state, AutostartState::Mismatch(_)) {
            notices.clear(Topic::Autostart);
        }
    }

    fn show(&mut self, ui: &mut egui::Ui) {
        if self.pending.is_some() {
            widgets::spinner_label(ui, "Updating the autostart task…");
            return;
        }
        match self.state.clone() {
            AutostartState::Unavailable(reason) => {
                ui.add_enabled(false, egui::Checkbox::new(&mut false, "Start with Windows"));
                ui.label(widgets::caption(ui, reason));
            }
            AutostartState::Off | AutostartState::On => {
                let mut enabled = self.state == AutostartState::On;
                if ui
                    .checkbox(&mut enabled, "Start with Windows, minimised to the tray")
                    .changed()
                {
                    self.apply(enabled, ui.ctx());
                }
                ui.label(widgets::caption(
                    ui,
                    "Keeps a protected copy in Program Files, so signing in needs no UAC prompt. Turning it off removes the copy.",
                ));
            }
            AutostartState::Mismatch(command) => {
                ui.label(
                    RichText::new(format!(
                        "{}  The autostart task starts {}",
                        icons::WARNING,
                        command.display()
                    ))
                    .color(theme::of(ui).warn),
                );
                if ui.button("Repair autostart").clicked() {
                    self.apply(true, ui.ctx());
                }
            }
        }
        if let Some(error) = &self.error {
            ui.label(RichText::new(error).color(theme::of(ui).danger));
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Interval {
    Fast,
    Normal,
    Relaxed,
    Custom,
}

impl Interval {
    const PRESETS: [(Self, u64); 3] = [
        (Self::Fast, 250),
        (Self::Normal, 1000),
        (Self::Relaxed, 2000),
    ];

    fn of(ms: u64) -> Self {
        Self::PRESETS
            .iter()
            .find(|(_, preset)| *preset == ms)
            .map_or(Self::Custom, |(interval, _)| *interval)
    }

    fn ms(self) -> Option<u64> {
        Self::PRESETS
            .iter()
            .find(|(interval, _)| *interval == self)
            .map(|(_, ms)| *ms)
    }
}

pub fn safety_sentence(state: &ControlState, config: &Config) -> String {
    let safety = &config.safety;
    let sensor = state
        .readings
        .sensors
        .iter()
        .find(|sensor| sensor.id == safety.critical_sensor)
        .map_or_else(
            || safety.critical_sensor.to_string(),
            |sensor| sensor.label.clone(),
        );
    if safety.critical_enabled {
        format!(
            "All fans go to 100% when {sensor} reaches {:.0} °C, and return to normal after {:.0} s below {:.0} °C.",
            safety.critical_c, safety.release_hold_secs, safety.release_c
        )
    } else {
        format!(
            "The critical temperature override is off: fans are not forced to 100% when {sensor} gets hot."
        )
    }
}

pub fn failure_time(config: &Config) -> f32 {
    config.safety.sensor_failure_ticks as f32 * config.interval_ms as f32 / 1000.0
}

fn unique_name(config: &Config, base: &str) -> String {
    std::iter::once(base.to_owned())
        .chain((2..).map(|n| format!("{base} {n}")))
        .find(|name| !config.profiles.contains_key(name))
        .unwrap_or_else(|| base.to_owned())
}

pub fn rename_profile(config: &mut Config, from: &str, to: &str) -> Result<(), &'static str> {
    let to = to.trim();
    if to.is_empty() {
        return Err("A profile needs a name");
    }
    if to.len() > MAX_NAME_LEN {
        return Err("That name is too long");
    }
    if to != from && config.profiles.contains_key(to) {
        return Err("Another profile already has that name");
    }
    let Some(profile) = config.profiles.remove(from) else {
        return Err("That profile no longer exists");
    };
    config.profiles.insert(to.to_owned(), profile);
    if config.active_profile == from {
        config.active_profile = to.to_owned();
    }
    Ok(())
}

#[derive(Debug, Default)]
pub struct SettingsView {
    new_profile: String,
    renaming: Option<(String, String)>,
    rename_error: Option<&'static str>,
    custom_interval: bool,
}

impl SettingsView {
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        state: &ControlState,
        draft: &mut Config,
        autostart: &mut AutostartPanel,
        link: &dyn ControlLink,
    ) -> bool {
        let mut changed = false;
        egui::ScrollArea::vertical().show(ui, |ui| {
            widgets::page_title(ui, "Settings", None);
            widgets::card(ui, |ui| fan_control(ui, state, link));
            ui.add_space(SPACE_M);
            widgets::card(ui, |ui| {
                widgets::card_title(ui, "General", None);
                autostart.show(ui);
                ui.add_space(SPACE_M);
                changed |= self.interval(ui, draft);
            });
            ui.add_space(SPACE_M);
            widgets::card(ui, |ui| {
                changed |= safety(ui, state, draft);
            });
            ui.add_space(SPACE_M);
            widgets::card(ui, |ui| {
                changed |= self.profiles(ui, draft);
            });
            ui.add_space(SPACE_M);
            widgets::card(ui, |ui| about(ui, state));
        });
        changed
    }

    fn interval(&mut self, ui: &mut egui::Ui, draft: &mut Config) -> bool {
        let mut changed = false;
        ui.label(RichText::new("Update interval").strong());
        ui.label(widgets::caption(
            ui,
            "How often MostlyFans reads the sensors and sets the fans.",
        ));
        let current = if self.custom_interval {
            Interval::Custom
        } else {
            Interval::of(draft.interval_ms)
        };
        let mut chosen = current;
        widgets::segmented(
            ui,
            &mut chosen,
            &[
                Segment::new(Interval::Fast, "Fast · 250 ms"),
                Segment::new(Interval::Normal, "Normal · 1 s"),
                Segment::new(Interval::Relaxed, "Relaxed · 2 s"),
                Segment::new(Interval::Custom, "Custom"),
            ],
        );
        if chosen != current {
            self.custom_interval = chosen == Interval::Custom;
            if let Some(ms) = chosen.ms() {
                draft.interval_ms = ms;
                changed = true;
            }
        }
        if chosen == Interval::Custom {
            changed |= ui
                .add(
                    DragValue::new(&mut draft.interval_ms)
                        .range(MIN_INTERVAL_MS..=MAX_INTERVAL_MS)
                        .speed(10.0)
                        .suffix(" ms"),
                )
                .on_hover_text("Update interval in milliseconds")
                .changed();
        }
        changed
    }

    fn profiles(&mut self, ui: &mut egui::Ui, draft: &mut Config) -> bool {
        let mut changed = false;
        widgets::card_title(
            ui,
            "Profiles",
            Some(
                "Each profile keeps its own fan modes. Switch profiles from the top bar; changes here are applied with Apply.",
            ),
        );
        let names: Vec<String> = draft.profiles.keys().cloned().collect();
        let mut remove = None;
        let mut duplicate = None;
        for name in &names {
            ui.horizontal(|ui| {
                ui.set_min_height(30.0);
                if let Some((from, buffer)) =
                    self.renaming.as_mut().filter(|(from, _)| from == name)
                {
                    let edit = ui.add(
                        egui::TextEdit::singleline(buffer)
                            .char_limit(MAX_NAME_LEN)
                            .desired_width(200.0),
                    );
                    let submit =
                        edit.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
                    if submit || ui.button("Save").clicked() {
                        let (from, buffer) = (from.clone(), buffer.clone());
                        match rename_profile(draft, &from, &buffer) {
                            Ok(()) => {
                                self.renaming = None;
                                self.rename_error = None;
                                changed |= from != buffer.trim();
                            }
                            Err(error) => self.rename_error = Some(error),
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        self.renaming = None;
                        self.rename_error = None;
                    }
                    return;
                }
                ui.label(name);
                if name == &draft.active_profile {
                    widgets::chip(ui, "Active");
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let removable = name != &draft.active_profile && names.len() > 1;
                    let reason = if names.len() <= 1 {
                        "The last profile can't be deleted"
                    } else {
                        "Switch to another profile before deleting this one"
                    };
                    let id = Id::unique(("delete-profile", name.as_str()));
                    let response_enabled = removable;
                    if widgets::confirm(ui, id, icons::DELETE, "Delete?", response_enabled) {
                        remove = Some(name.clone());
                    }
                    if !removable {
                        ui.label(widgets::caption(ui, reason));
                    }
                    if ui
                        .add_enabled(
                            names.len() < MAX_PROFILES,
                            egui::Button::new(icons::DUPLICATE),
                        )
                        .on_hover_text("Duplicate")
                        .clicked()
                    {
                        duplicate = Some(name.clone());
                    }
                    if ui.button(icons::RENAME).on_hover_text("Rename").clicked() {
                        self.renaming = Some((name.clone(), name.clone()));
                        self.rename_error = None;
                    }
                });
            });
        }
        if let Some(error) = self.rename_error {
            ui.label(RichText::new(error).color(theme::of(ui).danger));
        }
        if let Some(name) = remove {
            draft.profiles.remove(&name);
            changed = true;
        }
        if let Some(name) = duplicate
            && let Some(profile) = draft.profiles.get(&name).cloned()
        {
            let copy = unique_name(draft, &format!("{name} copy"));
            draft.profiles.insert(copy, profile);
            changed = true;
        }
        ui.add_space(SPACE_S);
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.new_profile)
                    .hint_text("New profile name")
                    .char_limit(MAX_NAME_LEN)
                    .desired_width(200.0),
            );
            let name = self.new_profile.trim().to_owned();
            let valid = !name.is_empty()
                && !draft.profiles.contains_key(&name)
                && draft.profiles.len() < MAX_PROFILES;
            if ui
                .add_enabled(
                    valid,
                    egui::Button::new(widgets::with_icon(
                        icons::ADD,
                        "Add a copy of the active profile",
                    )),
                )
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

fn fan_control(ui: &mut egui::Ui, state: &ControlState, link: &dyn ControlLink) {
    widgets::card_title(
        ui,
        "Fan control",
        Some(
            "Monitor-only mode hands every fan back to the BIOS and the graphics driver. MostlyFans keeps showing temperatures and fan speeds, and controls the fans again after a restart.",
        ),
    );
    let mut on = state.user_monitor_only;
    if ui
        .checkbox(
            &mut on,
            "Monitor only: leave all fans to the BIOS and graphics driver",
        )
        .changed()
    {
        link.send(Request::SetMonitorOnly(on));
    }
}

fn safety(ui: &mut egui::Ui, state: &ControlState, draft: &mut Config) -> bool {
    let mut changed = false;
    widgets::card_title(ui, "Safety", None);
    ui.label(RichText::new(safety_sentence(state, draft)).color(theme::of(ui).text));
    ui.add_space(SPACE_S);
    let failure = failure_time(draft);
    let safety = &mut draft.safety;
    safety_grid("safety-sensor").show(ui, |ui| {
        let label = ui.label("Safety sensor");
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
            .width(200.0)
            .show_ui(ui, |ui| {
                for sensor in state
                    .readings
                    .sensors
                    .iter()
                    .filter(|sensor| !sensor.id.is_display_only())
                {
                    let label = format!("{}  ({})", sensor.label, format::celsius(sensor.celsius));
                    if ui
                        .selectable_label(sensor.id == safety.critical_sensor, label)
                        .clicked()
                    {
                        safety.critical_sensor = sensor.id.clone();
                        changed = true;
                    }
                }
            })
            .response
            .labelled_by(label.id);
        ui.end_row();
    });
    ui.add_space(SPACE_M);
    changed |= ui
        .checkbox(
            &mut safety.critical_enabled,
            "Critical temperature override: all fans to 100% when it gets too hot",
        )
        .changed();
    ui.add_space(SPACE_M);

    let critical = safety.critical_enabled;
    let releasing = critical || safety.sensor_fallback_enabled;
    safety_grid("safety-critical").show(ui, |ui| {
        let label = ui.label("Full speed at");
        changed |= ui
            .add_enabled(
                critical,
                Slider::new(&mut safety.critical_c, 40.0..=110.0)
                    .suffix(" °C")
                    .fixed_decimals(1)
                    .clamping(SliderClamping::Edits),
            )
            .labelled_by(label.id)
            .changed();
        ui.end_row();

        let label = ui.label("Back to normal below");
        let max_release = safety.critical_c - 1.0;
        changed |= ui
            .add_enabled(
                releasing,
                Slider::new(&mut safety.release_c, 0.0..=max_release)
                    .suffix(" °C")
                    .fixed_decimals(1)
                    .clamping(SliderClamping::Edits),
            )
            .labelled_by(label.id)
            .changed();
        ui.end_row();

        let label = ui.label("for at least");
        changed |= ui
            .add_enabled(
                releasing,
                DragValue::new(&mut safety.release_hold_secs)
                    .range(0.0..=600.0)
                    .speed(0.5)
                    .suffix(" s"),
            )
            .labelled_by(label.id)
            .changed();
        ui.end_row();
    });
    ui.add_space(SPACE_M);
    changed |= ui
        .checkbox(
            &mut safety.sensor_fallback_enabled,
            "Missing-sensor fallback: fans go to 100% when their sensor stops reporting",
        )
        .changed();
    ui.add_space(SPACE_M);

    safety_grid("safety-missing").show(ui, |ui| {
        let label = ui.label("Missing sensor readings");
        ui.horizontal(|ui| {
            changed |= ui
                .add(DragValue::new(&mut safety.sensor_failure_ticks).range(1..=60))
                .labelled_by(label.id)
                .changed();
            ui.label(widgets::caption(
                ui,
                format!("readings ≈ {failure:.1} s at the current interval"),
            ));
        });
        ui.end_row();
    });
    ui.add_space(SPACE_S);
    let missed = safety.sensor_failure_ticks;
    let consequence = if safety.sensor_fallback_enabled {
        format!(
            "When a sensor misses {missed} readings in a row, its fans go to 100% (every fan, for the safety sensor) until it reads below the normal temperature again for the hold time."
        )
    } else {
        format!(
            "When a sensor misses {missed} readings in a row, its fans go back to BIOS control; a lost safety sensor only raises a warning."
        )
    };
    ui.label(widgets::caption(ui, consequence));
    if safety.release_c >= safety.critical_c {
        safety.release_c = safety.critical_c - 1.0;
        changed = true;
    }
    changed
}

fn safety_grid(id: &str) -> egui::Grid {
    egui::Grid::new(id)
        .num_columns(2)
        .min_col_width(SAFETY_LABEL_WIDTH)
        .spacing(vec2(SPACE_M * 2.0, SPACE_M))
}

fn about(ui: &mut egui::Ui, state: &ControlState) {
    widgets::card_title(ui, "About", None);
    egui::Grid::new("about")
        .num_columns(2)
        .spacing(vec2(SPACE_M * 2.0, SPACE_S))
        .show(ui, |ui| {
            if let Some(board) = &state.board {
                ui.label(widgets::muted("Motherboard"));
                ui.label(format!(
                    "{} {}",
                    board.identity.manufacturer, board.identity.product
                ));
                ui.end_row();
                for (index, line) in format::chip_lines(board).into_iter().enumerate() {
                    ui.label(widgets::muted(if index == 0 {
                        "Fan controller"
                    } else {
                        "Second fan controller"
                    }));
                    ui.label(line);
                    ui.end_row();
                }
            }
            ui.label(widgets::muted("Hardware"));
            let (text, tone) = format::hardware(&state.hardware);
            widgets::status(ui, tone, &text);
            ui.end_row();
            ui.label(widgets::muted("Version"));
            ui.label(&state.version);
            ui.end_row();
        });
    ui.add_space(SPACE_S);
    if ui
        .button(widgets::with_icon(icons::FOLDER, "Open log folder"))
        .clicked()
        && let Err(error) = Command::new("explorer.exe").arg(paths::log_dir()).spawn()
    {
        tracing::warn!(%error, "could not open the log folder");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    #[test]
    fn renaming_the_active_profile_follows_it() {
        let mut config = fixtures::config();
        let active = config.active_profile.clone();
        rename_profile(&mut config, &active, "Daily").unwrap();
        assert_eq!(config.active_profile, "Daily");
        assert!(config.profiles.contains_key("Daily"));
        assert!(!config.profiles.contains_key(&active));
        assert!(config.validate().is_ok());
    }

    #[test]
    fn renames_reject_blank_and_duplicate_names() {
        let mut config = fixtures::config();
        assert!(rename_profile(&mut config, "Silent", "  ").is_err());
        assert!(rename_profile(&mut config, "Silent", "Performance").is_err());
        assert!(rename_profile(&mut config, "Silent", "Silent").is_ok());
    }

    #[test]
    fn the_failure_time_follows_the_interval() {
        let mut config = fixtures::config();
        config.safety.sensor_failure_ticks = 5;
        config.interval_ms = 2000;
        assert_eq!(failure_time(&config), 10.0);
    }

    #[test]
    fn intervals_map_to_presets_or_custom() {
        assert_eq!(Interval::of(250), Interval::Fast);
        assert_eq!(Interval::of(1000), Interval::Normal);
        assert_eq!(Interval::of(750), Interval::Custom);
        assert_eq!(Interval::Relaxed.ms(), Some(2000));
    }

    #[test]
    fn the_safety_sentence_names_the_sensor() {
        let state = fixtures::state();
        assert_eq!(
            safety_sentence(&state, &state.config),
            "All fans go to 100% when CPU Tctl reaches 85 °C, and return to normal after 10 s below 80 °C."
        );
    }
}
