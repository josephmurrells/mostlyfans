use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use eframe::egui::{self, ComboBox, DragValue, RichText};
use fanforge_core::config::{Config, MAX_INTERVAL_MS, MIN_INTERVAL_MS, Profile};
use fanforge_core::state::ControlState;

use crate::autostart::{self, AutostartState};
use crate::format;
use crate::notices::{Notices, Topic};

type AutostartOutcome = (AutostartState, Option<String>);

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

    fn poll(&mut self, notices: &Notices) {
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

    fn show(&mut self, ui: &mut egui::Ui, notices: &Notices) {
        self.poll(notices);
        if self.pending.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Updating the autostart task…");
            });
            return;
        }
        match self.state.clone() {
            AutostartState::Unavailable(reason) => {
                ui.add_enabled(false, egui::Checkbox::new(&mut false, "Start with Windows"));
                ui.label(RichText::new(reason).color(format::MUTED));
            }
            AutostartState::Off | AutostartState::On => {
                let mut enabled = self.state == AutostartState::On;
                if ui
                    .checkbox(&mut enabled, "Start with Windows, minimised to the tray")
                    .changed()
                {
                    self.apply(enabled, ui.ctx());
                }
                ui.label(
                    RichText::new(
                        "Keeps a protected copy in Program Files for this, so sign-in needs no UAC prompt. Turning it off removes the copy.",
                    )
                    .color(format::MUTED),
                );
            }
            AutostartState::Mismatch(command) => {
                ui.colored_label(
                    format::WARN,
                    format!("The autostart task starts {}", command.display()),
                );
                if ui.button("Repair autostart").clicked() {
                    self.apply(true, ui.ctx());
                }
            }
        }
        if let Some(error) = &self.error {
            ui.colored_label(format::DANGER, error);
        }
    }
}

#[derive(Debug, Default)]
pub struct SettingsView {
    new_profile: String,
}

impl SettingsView {
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        state: &ControlState,
        draft: &mut Config,
        autostart: &mut AutostartPanel,
        notices: &Notices,
    ) -> bool {
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
            ui.heading("Startup");
            autostart.show(ui, notices);
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

fn safety(ui: &mut egui::Ui, state: &ControlState, draft: &mut Config) -> bool {
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

fn board(ui: &mut egui::Ui, state: &ControlState) {
    let (text, color) = format::hardware(&state.hardware);
    ui.colored_label(color, text);
    if let Some(board) = &state.board {
        ui.label(format!(
            "{} {}",
            board.identity.manufacturer, board.identity.product
        ));
        ui.label(format!(
            "Super I/O {:#06x}  ·  {} fan headers",
            board.chip_id, board.fan_headers
        ));
    }
    ui.label(RichText::new(format!("Version {}", state.version)).color(format::MUTED));
}
