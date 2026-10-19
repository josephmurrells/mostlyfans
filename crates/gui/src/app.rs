use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText, ViewportCommand};
use fanforge_core::config::Config;
use fanforge_core::state::{ControlState, HardwareStatus};

use crate::autostart::AutostartState;
use crate::banner;
use crate::connection::{Connection, ControlStatus, Request, RequestKind};
use crate::curves::CurveEditor;
use crate::format;
use crate::instance::ShowGate;
use crate::notices::Notices;
use crate::pawnio_setup::{OnInstalled, PawnIoPanel};
use crate::settings::{AutostartPanel, SettingsView};
use crate::tray::{Tray, TrayAction, reveal};
use crate::{fans, sensors};

const NOTICE_DURATION: Duration = Duration::from_secs(5);
const APPLY_SETTLE_TIMEOUT: Duration = Duration::from_secs(3);
const VISIBLE_REFRESH: Duration = Duration::from_millis(500);
const HIDDEN_REFRESH: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Fans,
    Curves,
    Sensors,
    Settings,
}

struct PendingApply {
    config: Config,
    accepted_at: Option<Instant>,
}

pub struct FanForgeApp {
    show_gate: ShowGate,
    connection: Connection,
    notices: Notices,
    autostart: AutostartPanel,
    pawnio: PawnIoPanel,
    tray: Option<Tray>,
    draft: Option<Config>,
    dirty: bool,
    pending: Option<PendingApply>,
    tab: Tab,
    curves: CurveEditor,
    settings: SettingsView,
    notice: Option<(String, Color32, Instant)>,
}

impl FanForgeApp {
    pub fn new(
        ctx: &egui::Context,
        connection: Connection,
        notices: Notices,
        show_gate: ShowGate,
        autostart: AutostartState,
        on_driver_installed: OnInstalled,
    ) -> Self {
        let repaint_ctx = ctx.clone();
        let refresher = thread::Builder::new()
            .name("tray-refresh".into())
            .spawn(move || {
                loop {
                    thread::sleep(HIDDEN_REFRESH);
                    repaint_ctx.request_repaint();
                }
            });
        if let Err(error) = refresher {
            tracing::warn!(%error, "tray refresh unavailable while the window is hidden");
        }
        let tray = match Tray::new(ctx) {
            Ok(tray) => Some(tray),
            Err(error) => {
                tracing::warn!(%error, "system tray unavailable; closing the window will exit");
                None
            }
        };
        Self {
            connection,
            notices,
            autostart: AutostartPanel::new(autostart),
            pawnio: PawnIoPanel::new(on_driver_installed),
            show_gate,
            tray,
            draft: None,
            dirty: false,
            pending: None,
            tab: Tab::Fans,
            curves: CurveEditor::default(),
            settings: SettingsView::default(),
            notice: None,
        }
    }

    fn notify(&mut self, text: impl Into<String>, color: Color32) {
        self.notice = Some((text.into(), color, Instant::now()));
    }

    fn apply(&mut self) {
        if let Some(draft) = &self.draft {
            self.connection
                .send(Request::ReplaceConfig(Box::new(draft.clone())));
            self.pending = Some(PendingApply {
                config: draft.clone(),
                accepted_at: None,
            });
        }
    }

    fn handle_results(&mut self) {
        for result in self.connection.take_results() {
            match (result.kind, result.outcome) {
                (RequestKind::Config, Ok(())) => {
                    if let Some(pending) = self.pending.as_mut() {
                        pending.accepted_at = Some(Instant::now());
                    }
                }
                (RequestKind::Config, Err(message)) => {
                    self.pending = None;
                    self.notify(format!("Rejected: {message}"), format::DANGER);
                }
                (RequestKind::Profile | RequestKind::Identify, Ok(())) => {}
                (RequestKind::Profile | RequestKind::Identify, Err(message)) => {
                    self.notify(format!("Failed: {message}"), format::DANGER);
                }
            }
        }
    }

    fn settle_pending(&mut self, state: Option<&ControlState>) {
        let Some(pending) = &self.pending else {
            return;
        };
        let Some(accepted_at) = pending.accepted_at else {
            return;
        };
        let visible = state.is_some_and(|state| state.config == pending.config);
        if visible || accepted_at.elapsed() >= APPLY_SETTLE_TIMEOUT {
            self.pending = None;
            self.dirty = false;
            self.draft = None;
            self.notify("Applied", format::OK);
        }
    }

    fn handle_tray(&mut self, ctx: &egui::Context, state: Option<&ControlState>) {
        let Some(tray) = self.tray.as_mut() else {
            return;
        };
        let actions = tray.actions();
        if let Some(state) = state {
            let profiles: Vec<String> = state.config.profiles.keys().cloned().collect();
            let cpu = state
                .readings
                .temperature(&state.config.safety.critical_sensor)
                .map_or_else(|| "n/a".to_owned(), |celsius| format!("{celsius:.0} °C"));
            tray.sync(
                &profiles,
                &state.config.active_profile,
                &format!("FanForge – CPU {cpu}"),
            );
        }
        for action in actions {
            match action {
                TrayAction::Show => reveal(ctx),
                TrayAction::SelectProfile(name) => {
                    if let Some(draft) = self.draft.as_mut() {
                        draft.active_profile = name.clone();
                    }
                    self.connection.send(Request::SetActiveProfile(name));
                }
            }
        }
    }

    fn header(&mut self, ui: &mut egui::Ui, status: &ControlStatus, state: Option<&ControlState>) {
        ui.horizontal(|ui| {
            ui.heading("FanForge");
            ui.separator();
            match status {
                ControlStatus::Running => ui.colored_label(format::OK, "Fan control active"),
                ControlStatus::Stopped(_) => {
                    ui.colored_label(format::DANGER, "Fan control stopped")
                }
            };
            if let Some(state) = state {
                let (text, color) = format::hardware(&state.hardware);
                ui.separator();
                ui.colored_label(color, text);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                for (tab, label) in [
                    (Tab::Settings, "Settings"),
                    (Tab::Sensors, "Sensors"),
                    (Tab::Curves, "Curves"),
                    (Tab::Fans, "Fans"),
                ] {
                    ui.selectable_value(&mut self.tab, tab, label);
                }
                if let Some(draft) = self.draft.as_mut() {
                    ui.separator();
                    let mut active = draft.active_profile.clone();
                    egui::ComboBox::from_id_salt("active-profile")
                        .selected_text(&active)
                        .show_ui(ui, |ui| {
                            for name in draft.profiles.keys() {
                                ui.selectable_value(&mut active, name.clone(), name);
                            }
                        });
                    if active != draft.active_profile {
                        draft.active_profile = active;
                        self.dirty = true;
                    }
                    ui.label("Profile");
                }
            });
        });
        if let ControlStatus::Stopped(reason) = status {
            let restart = banner::with_action(ui, format::DANGER, reason, "Restart control");
            if restart && let Err(error) = self.connection.restart() {
                self.notify(format!("Restart failed: {error}"), format::DANGER);
            }
        }
        for notice in self.notices.list() {
            banner::show(ui, format::WARN, &notice);
        }
        let driver_missing =
            state.is_some_and(|state| state.hardware == HardwareStatus::DriverMissing);
        self.pawnio.show(ui, driver_missing);
        let Some(state) = state else {
            return;
        };
        if state.critical {
            banner::show(
                ui,
                format::DANGER,
                "Critical temperature: every fan is at 100%",
            );
        }
        if state.monitor_only && !state.conflicts.is_empty() {
            banner::show(
                ui,
                format::WARN,
                &format!(
                    "Monitor-only mode: {} is running. Fans are left to the BIOS.",
                    state.conflicts.join(", ")
                ),
            );
        }
        for warning in &state.warnings {
            banner::show(ui, format::WARN, warning);
        }
    }

    fn footer(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if self.dirty {
                if ui.button(RichText::new("Apply changes").strong()).clicked() {
                    self.apply();
                }
                if ui.button("Revert").clicked() {
                    self.dirty = false;
                    self.draft = None;
                }
                ui.colored_label(format::WARN, "Unsaved changes");
            } else {
                ui.colored_label(format::MUTED, "All changes applied");
            }
            if let Some((text, color, at)) = &self.notice {
                if at.elapsed() < NOTICE_DURATION {
                    ui.separator();
                    ui.colored_label(*color, text);
                } else {
                    self.notice = None;
                }
            }
        });
    }
}

impl Drop for FanForgeApp {
    fn drop(&mut self) {
        self.show_gate.close();
    }
}

impl eframe::App for FanForgeApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let (_, state) = self.connection.view();
        ctx.request_repaint_after(VISIBLE_REFRESH);
        self.handle_tray(ctx, state.as_deref());
        self.handle_results();
        self.settle_pending(state.as_deref());

        let exiting = self.tray.as_ref().is_none_or(Tray::exit_requested);
        if ctx.input(|input| input.viewport().close_requested()) && !exiting {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let (status, state): (ControlStatus, Option<Arc<ControlState>>) = self.connection.view();
        if !self.dirty {
            self.draft = state.as_ref().map(|state| state.config.clone());
        }

        egui::Panel::top("header").show(ui, |ui| self.header(ui, &status, state.as_deref()));
        egui::Panel::bottom("footer").show(ui, |ui| self.footer(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            let (Some(state), Some(draft)) = (state.as_deref(), self.draft.as_mut()) else {
                ui.centered_and_justified(|ui| {
                    ui.label("Starting fan control…");
                });
                return;
            };
            let changed = match self.tab {
                Tab::Fans => fans::show(ui, state, draft, &self.connection),
                Tab::Curves => self.curves.show(ui, state, draft),
                Tab::Sensors => {
                    sensors::show(ui, state);
                    false
                }
                Tab::Settings => {
                    self.settings
                        .show(ui, state, draft, &mut self.autostart, &self.notices)
                }
            };
            self.dirty |= changed;
        });
    }
}
