use std::thread;
use std::time::Duration;

use eframe::egui::{self, ViewportCommand};
use fanforge_core::state::ControlState;

use crate::autostart::AutostartState;
use crate::connection::ControlLink;
use crate::format;
use crate::glyph;
use crate::instance::ShowGate;
use crate::notices::Notices;
use crate::pawnio_setup::{OnInstalled, PawnIoPanel};
use crate::prefs::Prefs;
use crate::settings::AutostartPanel;
use crate::shell::Shell;
use crate::status::{self, Overall};
use crate::tray::{Tray, TrayAction, reveal};
use crate::window_geometry::LastGoodWindow;

const VISIBLE_REFRESH: Duration = Duration::from_millis(500);
const HIDDEN_REFRESH: Duration = Duration::from_secs(2);

pub struct FanForgeApp {
    show_gate: ShowGate,
    tray: Option<Tray>,
    shell: Shell,
    window: LastGoodWindow,
}

fn tint(overall: &Overall) -> [u8; 3] {
    match overall {
        Overall::Controlling(_) => glyph::BLUE,
        Overall::FanFault(_) => glyph::AMBER,
        Overall::Stopped
        | Overall::Critical
        | Overall::CriticalUnforced
        | Overall::DriverMissing
        | Overall::Unavailable => glyph::RED,
        Overall::Starting | Overall::Suspended | Overall::MonitorOnly | Overall::BiosOnly => {
            glyph::GREY
        }
    }
}

impl FanForgeApp {
    pub fn new(
        ctx: &egui::Context,
        link: Box<dyn ControlLink>,
        notices: Notices,
        show_gate: ShowGate,
        autostart: AutostartState,
        on_driver_installed: OnInstalled,
        prefs: Prefs,
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
            show_gate,
            tray,
            shell: Shell::new(
                link,
                notices,
                AutostartPanel::new(autostart),
                PawnIoPanel::new(on_driver_installed),
                prefs,
            ),
            window: LastGoodWindow::default(),
        }
    }

    pub fn with_window(mut self, window: LastGoodWindow) -> Self {
        self.window = window;
        self
    }

    fn handle_tray(&mut self, ctx: &egui::Context) {
        let Some(tray) = self.tray.as_mut() else {
            return;
        };
        let actions = tray.actions();
        let (control, state) = self.shell.view();
        if let Some(state) = state.as_deref() {
            let overall = status::overall(&control, Some(state));
            let profiles: Vec<String> = state.config.profiles.keys().cloned().collect();
            let active = self.shell.shown_profile(state);
            tray.sync(
                &profiles,
                &active,
                &overall.label(),
                &tooltip(state, &overall, &active),
                tint(&overall),
                state.user_monitor_only,
            );
        }
        for action in actions {
            match action {
                TrayAction::Show => reveal(ctx),
                TrayAction::SelectProfile(name) => {
                    if self.shell.request_profile(name) {
                        reveal(ctx);
                    }
                }
                TrayAction::MonitorOnly(on) => self.shell.set_monitor_only(on),
            }
        }
    }
}

fn tooltip(state: &ControlState, overall: &Overall, profile: &str) -> String {
    let sensor = format::critical_sensor_label(state);
    let reading = state
        .readings
        .temperature(&state.config.safety.critical_sensor);
    format!(
        "FanForge – {} · {profile} · {sensor} {}",
        overall.label(),
        format::whole_celsius(reading)
    )
}

impl Drop for FanForgeApp {
    fn drop(&mut self) {
        self.show_gate.close();
    }
}

impl eframe::App for FanForgeApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(VISIBLE_REFRESH);
        self.shell.logic();
        self.handle_tray(ctx);

        let exiting = self.tray.as_ref().is_none_or(Tray::exit_requested);
        if ctx.input(|input| input.viewport().close_requested()) && !exiting {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        self.shell.ui(ui);
        if self.shell.take_prefs_changed()
            && let Some(storage) = frame.storage_mut()
        {
            self.shell.prefs.save(storage);
            storage.flush();
        }
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.window.guard(storage);
        self.shell.prefs.save(storage);
    }
}
