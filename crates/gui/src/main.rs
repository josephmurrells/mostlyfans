#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod alerts;
mod app;
mod autostart;
mod changes;
mod connection;
mod curve_plot;
mod curves;
mod fans;
#[cfg(test)]
mod fixtures;
mod format;
mod glyph;
mod guardian;
mod guardian_link;
mod history;
mod icons;
mod instance;
mod logging;
mod notices;
mod overview;
mod pawnio_setup;
mod prefs;
mod session;
mod settings;
mod shell;
mod status;
mod theme;
mod tray;
mod widgets;

use std::env;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use eframe::egui;
use fanforge_control::conflicts::ConflictMonitor;
use fanforge_control::controller::Controller;
use fanforge_control::legacy::{self, Removal};
use fanforge_control::runtime::{Paths, Runtime};
use fanforge_control::shared::Shared;
use fanforge_control::{failsafe, watchdog};
use fanforge_hw::system::Hardware;
use fanforge_win::paths;
use fanforge_win::{DialogKind, message_box};

use crate::autostart::AutostartState;
use crate::guardian_link::GuardianLink;
use crate::instance::Claim;
use crate::notices::{Notices, Topic};
use crate::prefs::Prefs;

const TITLE: &str = "FanForge";
const WINDOW_ICON_SIZE: u32 = 64;

#[derive(Debug, Parser)]
#[command(version, about = "FanForge fan control")]
struct Cli {
    #[command(flatten)]
    guardian: guardian::GuardianArgs,
    #[arg(long, help = "Start hidden in the system tray")]
    minimized: bool,
}

fn fail(text: &str) -> ExitCode {
    tracing::error!(text, "startup refused");
    logging::flush();
    message_box(TITLE, text, DialogKind::Error);
    ExitCode::FAILURE
}

fn offer_legacy_removal() {
    if !legacy::service_installed() {
        return;
    }
    let remove = message_box(
        TITLE,
        "The old FanForge service is installed. It would fight this app over the fans.\n\nRemove it now?\n\nIf you choose No, FanForge only monitors and leaves the fans to the BIOS.",
        DialogKind::Question,
    );
    if !remove {
        tracing::warn!("old service kept; running monitor-only");
        return;
    }
    match legacy::remove_service() {
        Ok(Removal::Complete) => {}
        Ok(Removal::BinaryRemovedAtReboot) => {
            message_box(
                TITLE,
                "The old service is removed. Its program file is deleted at the next restart.",
                DialogKind::Info,
            );
        }
        Err(error) => {
            message_box(
                TITLE,
                &format!(
                    "The old service could not be removed: {error}\n\nFanForge only monitors until it is gone."
                ),
                DialogKind::Error,
            );
        }
    }
}

fn check_autostart(notices: &Notices) -> AutostartState {
    let state = autostart::sync();
    if let AutostartState::Mismatch(command) = &state {
        notices.set(
            Topic::Autostart,
            format!(
                "The Start with Windows task starts {}, not FanForge's protected copy. Repair it in Settings.",
                command.display()
            ),
        );
    }
    state
}

fn run_app(minimized: bool) -> ExitCode {
    if !fanforge_win::is_elevated().unwrap_or(false) {
        return fail("FanForge needs administrator rights to reach the fan controller.");
    }
    if let Err(error) =
        fanforge_win::secure_directory(&paths::data_dir(), fanforge_win::PRIVATE_DATA_SDDL)
    {
        return fail(&format!(
            "The FanForge data folder is not safe to use: {error}"
        ));
    }
    logging::init("fanforge");
    watchdog::install_panic_hook();
    let instance = match instance::claim() {
        Claim::Primary(instance) => instance,
        Claim::Revealed => return ExitCode::SUCCESS,
        Claim::Refused(reason) => return fail(&reason),
    };
    offer_legacy_removal();

    let executable = match env::current_exe() {
        Ok(executable) => executable,
        Err(error) => return fail(&format!("FanForge could not locate itself: {error}")),
    };
    let notices = Notices::default();
    let autostart = check_autostart(&notices);
    let guardian = GuardianLink::start(executable, notices.clone());
    let snapshot = paths::bios_snapshot_path();
    let shared = Arc::new(Shared::default());
    let sink = guardian.sink();
    let factory = Box::new(move |shared, messages| {
        Runtime::new(
            Paths::default(),
            shared,
            messages,
            Box::new(Hardware::open),
            ConflictMonitor::default(),
        )
        .with_signals(Arc::clone(&sink))
    });
    let release_snapshot = snapshot.clone();
    let direct_release =
        Arc::new(move |budget| failsafe::release_from_disk(&release_snapshot, budget).is_ok());
    let mut controller = match Controller::start(Arc::clone(&shared), factory, direct_release) {
        Ok(controller) => controller,
        Err(error) => return fail(&format!("Fan control could not start: {error}")),
    };
    let released = guardian.clone();
    controller.on_released(move || released.released());
    if let Err(error) = watchdog::spawn(Arc::clone(&shared), snapshot.clone(), logging::flush) {
        return fail(&format!(
            "The control loop watchdog could not start: {error}"
        ));
    }
    let installed_guardian = guardian.clone();
    let installed_remote = controller.remote();
    let on_driver_installed: pawnio_setup::OnInstalled = Arc::new(move || {
        installed_guardian.ensure_running();
        installed_remote.retry_hardware();
    });
    let _power = match session::watch(controller.remote(), guardian, snapshot) {
        Ok(power) => power,
        Err(error) => return fail(&format!("Session monitoring could not start: {error}")),
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_app_id("FanForge")
            .with_icon(Arc::new(egui::IconData {
                rgba: glyph::fan_rgba(WINDOW_ICON_SIZE, glyph::BLUE),
                width: WINDOW_ICON_SIZE,
                height: WINDOW_ICON_SIZE,
            }))
            .with_inner_size([1080.0, 720.0])
            .with_min_inner_size([720.0, 520.0])
            .with_visible(!minimized),
        persistence_path: Some(paths::data_dir().join("window.ron")),
        ..Default::default()
    };
    let show_gate = instance.show_gate();
    let result = eframe::run_native(
        TITLE,
        options,
        Box::new(move |creation| {
            let ctx = &creation.egui_ctx;
            theme::install(ctx);
            let prefs = Prefs::load(creation.storage);
            if let Err(error) = show_gate.forward_to(ctx) {
                tracing::warn!(%error, "a second launch cannot bring this window forward");
            }
            Ok(Box::new(app::FanForgeApp::new(
                ctx,
                Box::new(controller),
                notices,
                show_gate,
                autostart,
                on_driver_installed,
                prefs,
            )))
        }),
    );
    drop(instance);
    let code = match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "window failed");
            ExitCode::FAILURE
        }
    };
    logging::flush();
    code
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    if cli.guardian.guardian {
        return guardian::main(&cli.guardian);
    }
    run_app(cli.minimized)
}
