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
mod pins;
mod prefs;
mod session;
mod settings;
mod shell;
mod status;
mod temperatures;
mod theme;
mod tray;
mod widgets;
mod window_geometry;

use std::cell::RefCell;
use std::env;
use std::path::Path;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use eframe::egui;
use eframe::egui_wgpu::WgpuSetup;
use mostlyfans_control::conflicts::ConflictMonitor;
use mostlyfans_control::controller::Controller;
use mostlyfans_control::legacy::{self, Removal};
use mostlyfans_control::runtime::{Paths, Runtime};
use mostlyfans_control::shared::Shared;
use mostlyfans_control::{failsafe, watchdog};
use mostlyfans_hw::gpu_fans::{GpuFans, GpuReleaser, nvidia};
use mostlyfans_hw::sensors::{self, ExtraSensors};
use mostlyfans_hw::system::Hardware;
use mostlyfans_win::paths;
use mostlyfans_win::{DialogKind, message_box};

use crate::autostart::AutostartState;
use crate::connection::ControlLink;
use crate::guardian_link::GuardianLink;
use crate::instance::{Claim, ShowGate};
use crate::notices::{Notices, Topic};
use crate::prefs::Prefs;
use crate::window_geometry::LastGoodWindow;

const TITLE: &str = "MostlyFans";
const WINDOW_ICON_SIZE: u32 = 64;
const FIRST_SENSOR_ROUND: Duration = Duration::from_secs(3);

#[derive(Debug, Parser)]
#[command(version, about = "MostlyFans fan control")]
struct Cli {
    #[command(flatten)]
    guardian: guardian::GuardianArgs,
    #[arg(long, help = "Start hidden in the system tray")]
    minimized: bool,
}

struct Startup {
    link: Box<dyn ControlLink>,
    notices: Notices,
    show_gate: ShowGate,
    autostart: AutostartState,
    on_driver_installed: pawnio_setup::OnInstalled,
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
        "The old FanForge service is installed. It would fight this app over the fans.\n\nRemove it now?\n\nIf you choose No, MostlyFans only monitors and leaves the fans to the BIOS.",
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
                    "The old service could not be removed: {error}\n\nMostlyFans only monitors until it is gone."
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
                "The Start with Windows task starts {}, not MostlyFans's protected copy. Repair it in Settings.",
                command.display()
            ),
        );
    }
    state
}

fn run_app(minimized: bool) -> ExitCode {
    if !mostlyfans_win::is_elevated().unwrap_or(false) {
        return fail("MostlyFans needs administrator rights to reach the fan controller.");
    }
    if let Err(error) =
        mostlyfans_win::secure_directory(&paths::data_dir(), mostlyfans_win::PRIVATE_DATA_SDDL)
    {
        return fail(&format!(
            "The MostlyFans data folder is not safe to use: {error}"
        ));
    }
    logging::init("mostlyfans");
    watchdog::install_panic_hook();
    let instance = match instance::claim() {
        Claim::Primary(instance) => instance,
        Claim::Revealed => return ExitCode::SUCCESS,
        Claim::Refused(reason) => return fail(&reason),
    };
    offer_legacy_removal();

    let executable = match env::current_exe() {
        Ok(executable) => executable,
        Err(error) => return fail(&format!("MostlyFans could not locate itself: {error}")),
    };
    let notices = Notices::default();
    let autostart = check_autostart(&notices);
    let started_by_task = minimized;
    let guardian = GuardianLink::start(executable, started_by_task, notices.clone());
    let snapshot = paths::bios_snapshot_path();
    let shared = Arc::new(Shared::default());
    let sink = guardian.sink();
    let extras = ExtraSensors::start(sensors::elevated_sources());
    if !extras.wait_for_first_round(FIRST_SENSOR_ROUND) {
        tracing::warn!("GPU, drive and CPU core sensors are slow to report; starting without them");
    }
    let gpu_releaser = GpuReleaser::open(nvidia::opener(), paths::gpu_ownership_path());
    let runtime_releaser = gpu_releaser.clone();
    let factory = Box::new(move |shared: Arc<Shared>, messages| {
        let gpus = GpuFans::start(
            nvidia::opener(),
            paths::gpu_ownership_path(),
            shared.halt_handle(),
        );
        let runtime = Runtime::new(
            Paths::default(),
            shared,
            messages,
            Box::new(Hardware::open),
            ConflictMonitor::default(),
        )
        .with_signals(Arc::clone(&sink))
        .with_extra_sensors(extras.clone())
        .with_gpu_releaser(runtime_releaser.clone());
        match gpus {
            Ok(gpus) => runtime.with_gpu_fans(gpus),
            Err(error) => {
                tracing::error!(%error, "GPU fan control could not start");
                runtime
            }
        }
    });
    let release_snapshot = snapshot.clone();
    let release_gpus = gpu_releaser.clone();
    let direct_release = Arc::new(move |budget| {
        failsafe::release_with_gpus(Some(&release_gpus), budget, |budget| {
            failsafe::release_from_disk(&release_snapshot, budget)
        })
        .is_ok()
    });
    let mut controller = match Controller::start(Arc::clone(&shared), factory, direct_release) {
        Ok(controller) => controller,
        Err(error) => return fail(&format!("Fan control could not start: {error}")),
    };
    let released = guardian.clone();
    controller.on_released(move || released.released());
    if let Err(error) = watchdog::spawn(
        Arc::clone(&shared),
        snapshot.clone(),
        Some(gpu_releaser.clone()),
        logging::flush,
    ) {
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
    let _power = match session::watch(controller.remote(), guardian, snapshot, gpu_releaser) {
        Ok(power) => power,
        Err(error) => return fail(&format!("Session monitoring could not start: {error}")),
    };

    let window_state = paths::data_dir().join("window.ron");
    window_geometry::repair(&window_state);
    let startup = Rc::new(RefCell::new(Some(Startup {
        link: Box::new(controller),
        notices,
        show_gate: instance.show_gate(),
        autostart,
        on_driver_installed,
    })));
    let mut result = open_window(eframe::Renderer::Glow, &window_state, minimized, &startup);
    if let Err(error) = &result
        && startup.borrow().is_some()
    {
        tracing::warn!(%error, "OpenGL is unavailable; drawing with Direct3D 12 instead");
        result = open_window(eframe::Renderer::Wgpu, &window_state, minimized, &startup);
    }
    drop(startup);
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

fn native_options(
    renderer: eframe::Renderer,
    window_state: &Path,
    minimized: bool,
) -> eframe::NativeOptions {
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_app_id("MostlyFans")
            .with_icon(Arc::new(egui::IconData {
                rgba: glyph::fan_rgba(WINDOW_ICON_SIZE, glyph::BLUE),
                width: WINDOW_ICON_SIZE,
                height: WINDOW_ICON_SIZE,
            }))
            .with_inner_size([1080.0, 720.0])
            .with_min_inner_size(window_geometry::MIN_SIZE)
            .with_visible(!minimized),
        persistence_path: Some(window_state.to_path_buf()),
        renderer,
        ..Default::default()
    };
    if let WgpuSetup::CreateNew(setup) = &mut options.wgpu_options.wgpu_setup {
        setup.instance_descriptor.backends = wgpu::Backends::DX12;
    }
    options
}

fn open_window(
    renderer: eframe::Renderer,
    window_state: &Path,
    minimized: bool,
    startup: &Rc<RefCell<Option<Startup>>>,
) -> eframe::Result {
    let startup = Rc::clone(startup);
    eframe::run_native(
        TITLE,
        native_options(renderer, window_state, minimized),
        Box::new(move |creation| {
            let Startup {
                link,
                notices,
                show_gate,
                autostart,
                on_driver_installed,
            } = startup
                .borrow_mut()
                .take()
                .ok_or("the window was already set up")?;
            tracing::info!(%renderer, "window ready");
            let ctx = &creation.egui_ctx;
            theme::install(ctx);
            let prefs = Prefs::load(creation.storage);
            let window = LastGoodWindow::load(creation.storage);
            if let Err(error) = show_gate.forward_to(ctx) {
                tracing::warn!(%error, "a second launch cannot bring this window forward");
            }
            Ok(Box::new(
                app::MostlyFansApp::new(
                    ctx,
                    link,
                    notices,
                    show_gate,
                    autostart,
                    on_driver_installed,
                    prefs,
                )
                .with_window(window)
                .start_hidden(minimized),
            ))
        }),
    )
}

fn main() -> ExitCode {
    if let Err(error) = mostlyfans_win::restrict_dll_search() {
        return fail(&format!(
            "MostlyFans could not limit where Windows loads DLLs from: {error}"
        ));
    }
    let cli = Cli::parse();
    if cli.guardian.guardian {
        return guardian::main(&cli.guardian);
    }
    run_app(cli.minimized)
}
