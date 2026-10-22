use std::fs::{self, File};
use std::io::{self, Read};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use clap::Args;
use fanforge_control::failsafe;
use fanforge_control::guardian::{
    self, LinkEvent, NO_HARDWARE_EXIT_CODE, Outcome, ParentLink, Releaser, Signal,
    TERMINATE_EXIT_CODE,
};
use fanforge_hw::gpu_fans::{GpuReleaser, nvidia};
use fanforge_hw::system::{Chip, open_chip};
use fanforge_win::paths;
use fanforge_win::{PowerEvent, PowerSubscription, Process};

use crate::logging;

const USAGE_EXIT_CODE: u8 = 2;
const RELEASE_FAILED_EXIT_CODE: u8 = 3;

#[derive(Debug, Args)]
pub struct GuardianArgs {
    #[arg(long, hide = true)]
    pub guardian: bool,
    #[arg(long, hide = true, requires = "guardian")]
    pub parent_handle: Option<usize>,
    #[arg(long, hide = true, requires = "guardian")]
    pub pipe_handle: Option<usize>,
    #[arg(long, hide = true, requires = "guardian")]
    pub dry_run: Option<PathBuf>,
}

enum GuardianReleaser {
    Fans {
        chip: Option<Chip>,
        snapshot: PathBuf,
        gpus: GpuReleaser,
    },
    DryRun(PathBuf),
}

impl Releaser for GuardianReleaser {
    fn release(&mut self, budget: Duration) -> Result<(), String> {
        match self {
            Self::Fans {
                chip,
                snapshot,
                gpus,
            } => failsafe::release_with_gpus(Some(gpus), budget, |budget| {
                failsafe::release_preferring(chip.as_mut(), snapshot, budget)
            }),
            Self::DryRun(marker) => {
                fs::write(marker, b"released").map_err(|error| error.to_string())
            }
        }
    }
}

struct ProcessLink {
    parent: Arc<Process>,
    events: Receiver<LinkEvent>,
    _power: Option<PowerSubscription>,
}

impl ProcessLink {
    fn start(parent: Process, mut pipe: File) -> io::Result<Self> {
        let (events_tx, events) = mpsc::channel();
        let parent = Arc::new(parent);

        let pipe_tx = events_tx.clone();
        thread::Builder::new()
            .name("guardian-pipe".into())
            .spawn(move || {
                let mut buffer = [0u8; 64];
                loop {
                    match pipe.read(&mut buffer) {
                        Ok(0) | Err(_) => {
                            pipe_tx.send(LinkEvent::Closed).ok();
                            return;
                        }
                        Ok(read) => {
                            for signal in buffer[..read]
                                .iter()
                                .filter_map(|&byte| Signal::decode(byte))
                            {
                                pipe_tx.send(LinkEvent::Signal(signal)).ok();
                            }
                        }
                    }
                }
            })?;

        let exit_tx = events_tx.clone();
        let watched = Arc::clone(&parent);
        thread::Builder::new()
            .name("guardian-parent".into())
            .spawn(move || {
                if watched.wait(None).unwrap_or(true) {
                    exit_tx.send(LinkEvent::ParentExited).ok();
                }
            })?;

        let power = fanforge_win::subscribe_power(move |event| {
            if event == PowerEvent::Resume {
                events_tx.send(LinkEvent::SystemResumed).ok();
            }
        })
        .map_err(|error| tracing::warn!(%error, "guardian could not watch for resume"))
        .ok();

        Ok(Self {
            parent,
            events,
            _power: power,
        })
    }
}

impl ParentLink for ProcessLink {
    fn next_event(&mut self, timeout: Duration) -> LinkEvent {
        match self.events.recv_timeout(timeout) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => LinkEvent::Idle,
            Err(RecvTimeoutError::Disconnected) => LinkEvent::Closed,
        }
    }

    fn parent_alive(&mut self) -> bool {
        !self.parent.wait(Some(Duration::ZERO)).unwrap_or(true)
    }

    fn terminate_parent(&mut self, wait: Duration) {
        tracing::warn!("ending the unresponsive FanForge process");
        if let Err(error) = self.parent.terminate(TERMINATE_EXIT_CODE) {
            tracing::error!(%error, "could not end the FanForge process");
        }
        if !self.parent.wait(Some(wait)).unwrap_or(false) {
            tracing::error!("FanForge process did not exit in time");
        }
    }
}

fn inherited_link(args: &GuardianArgs) -> Option<(Process, File)> {
    let (Some(parent), Some(pipe)) = (args.parent_handle, args.pipe_handle) else {
        tracing::error!("guardian started without its parent and pipe handles");
        return None;
    };
    if parent == pipe {
        tracing::error!("guardian was given the same handle twice");
        return None;
    }
    let parent = unsafe { fanforge_win::inherited_process(parent) };
    let pipe = unsafe { fanforge_win::inherited_pipe(pipe) };
    match (parent, pipe) {
        (Some(parent), Some(pipe)) => Some((parent, pipe)),
        _ => {
            tracing::error!("guardian handles are not a process and a pipe");
            None
        }
    }
}

pub fn main(args: &GuardianArgs) -> ExitCode {
    if args.dry_run.is_none() {
        logging::init("guardian");
    }
    let code = run(args);
    logging::flush();
    code
}

fn run(args: &GuardianArgs) -> ExitCode {
    let Some((parent, pipe)) = inherited_link(args) else {
        return ExitCode::from(USAGE_EXIT_CODE);
    };
    let mut releaser = match &args.dry_run {
        Some(marker) => GuardianReleaser::DryRun(marker.clone()),
        None => {
            let chip = open_chip()
                .map_err(
                    |error| tracing::warn!(%error, "guardian could not open the fan controller"),
                )
                .ok();
            let gpus = GpuReleaser::open(nvidia::opener(), paths::gpu_ownership_path());
            if chip.is_none() && !gpus.has_session() {
                tracing::error!("guardian found neither a fan controller nor a GPU driver");
                return ExitCode::from(NO_HARDWARE_EXIT_CODE as u8);
            }
            GuardianReleaser::Fans {
                chip,
                snapshot: paths::bios_snapshot_path(),
                gpus,
            }
        }
    };
    let mut link = match ProcessLink::start(parent, pipe) {
        Ok(link) => link,
        Err(error) => {
            tracing::error!(%error, "guardian could not start watching FanForge");
            return ExitCode::from(USAGE_EXIT_CODE);
        }
    };
    tracing::info!("guardian watching FanForge");
    match guardian::run(&mut link, &mut releaser, &fanforge_win::unbiased_time) {
        Outcome::CleanExit => {
            tracing::info!("FanForge released the fans and exited; guardian stopping");
            ExitCode::SUCCESS
        }
        Outcome::Released(reason) => {
            tracing::warn!(?reason, "guardian returned the fans to BIOS control");
            ExitCode::SUCCESS
        }
        Outcome::ReleaseFailed(reason, error) => {
            tracing::error!(?reason, %error, "guardian could not return the fans to BIOS control");
            ExitCode::from(RELEASE_FAILED_EXIT_CODE)
        }
    }
}
