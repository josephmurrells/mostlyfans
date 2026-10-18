mod backend;
mod conflicts;
mod install;
mod ipc_server;
mod logging;
mod runtime;
mod scm;
mod shared;
mod watchdog;

use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};

use clap::{Parser, Subcommand};
use fanforge_core::paths;
use fanforge_hw::system::Hardware;
use thiserror::Error;

use crate::conflicts::ConflictMonitor;
use crate::runtime::{Paths, Runtime};
use crate::shared::{LoopMessage, Shared};

#[derive(Debug, Parser)]
#[command(version, about = "FanForge fan control service")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Install,
    Uninstall,
    Run,
}

#[derive(Debug, Error)]
pub enum StartError {
    #[error("failed to start the IPC server (is another FanForge instance running?): {0}")]
    Ipc(#[source] io::Error),
    #[error("failed to start the watchdog: {0}")]
    Watchdog(#[source] io::Error),
    #[error("FanForge must run as the service or from an elevated terminal")]
    NotElevated,
    #[error("the data directory is not safe to use: {0}")]
    DataDirectory(#[source] io::Error),
    #[error("the control loop panicked; fans were returned to BIOS control")]
    Panicked,
    #[error(transparent)]
    Service(#[from] windows_service::Error),
}

pub fn start(
    loop_tx: Sender<LoopMessage>,
    loop_rx: Receiver<LoopMessage>,
) -> Result<(), StartError> {
    watchdog::install_panic_hook();
    if !fanforge_win::is_elevated().unwrap_or(false) {
        return Err(StartError::NotElevated);
    }
    fanforge_win::secure_directory(&paths::data_dir(), fanforge_win::PRIVATE_DATA_SDDL)
        .map_err(StartError::DataDirectory)?;
    let paths = Paths::default();
    let shared = Arc::new(Shared::new());
    ipc_server::spawn(Arc::clone(&shared), loop_tx).map_err(StartError::Ipc)?;
    watchdog::spawn(Arc::clone(&shared), paths.snapshot.clone()).map_err(StartError::Watchdog)?;
    let runtime = Runtime::new(
        paths,
        shared,
        loop_rx,
        Box::new(Hardware::open),
        ConflictMonitor::default(),
    );
    panic::catch_unwind(AssertUnwindSafe(move || runtime.run())).map_err(|_| StartError::Panicked)
}

fn run_console() -> Result<(), StartError> {
    let (loop_tx, loop_rx) = std::sync::mpsc::channel();
    let stop_tx = loop_tx.clone();
    if let Err(error) = ctrlc::set_handler(move || {
        stop_tx.send(LoopMessage::Stop).ok();
    }) {
        tracing::warn!(%error, "Ctrl+C handler unavailable");
    }
    start(loop_tx, loop_rx)
}

fn report<E: std::fmt::Display>(action: &str, result: Result<(), E>) -> ExitCode {
    match result {
        Ok(()) => {
            println!("{action} succeeded");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{action} failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        None => {
            let _guard = logging::init_file();
            match scm::dispatch() {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    tracing::error!(%error, "not started by the service manager; use `run` for console mode");
                    eprintln!(
                        "fanforge-service must be started by the service manager ({error}); try `fanforge-service run`"
                    );
                    ExitCode::FAILURE
                }
            }
        }
        Some(Command::Run) => {
            logging::init_console();
            report("run", run_console())
        }
        Some(Command::Install) => {
            logging::init_console();
            report("install", install::install())
        }
        Some(Command::Uninstall) => {
            logging::init_console();
            report("uninstall", install::uninstall())
        }
    }
}
