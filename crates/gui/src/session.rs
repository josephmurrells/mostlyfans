use std::io;
use std::path::PathBuf;
use std::process;

use fanforge_control::controller::{DIRECT_RELEASE_BUDGET, Remote};
use fanforge_control::failsafe;
use fanforge_hw::gpu_fans::GpuReleaser;
use fanforge_hw::system::{Chip, open_chip};
use fanforge_win::{PowerEvent, PowerSubscription, SessionEvent};

use crate::guardian_link::GuardianLink;
use crate::logging;

fn release_for_session_end(
    chip: Option<&mut Chip>,
    snapshot: &std::path::Path,
    gpus: &GpuReleaser,
) -> bool {
    let released = failsafe::release_with_gpus(Some(gpus), DIRECT_RELEASE_BUDGET, |budget| {
        failsafe::release_preferring(chip, snapshot, budget)
    });
    match released {
        Ok(()) => true,
        Err(error) => {
            tracing::error!(%error, "could not release fans before the session ended");
            false
        }
    }
}

pub fn watch(
    remote: Remote,
    guardian: GuardianLink,
    snapshot: PathBuf,
    gpus: GpuReleaser,
) -> io::Result<Option<PowerSubscription>> {
    let mut chip = open_chip()
        .map_err(|error| tracing::warn!(%error, "no spare hardware handle for session end"))
        .ok();
    let session_remote = remote.clone();
    fanforge_win::spawn_session_window(move |event| match event {
        SessionEvent::QueryEnd => tracing::info!("session end requested"),
        SessionEvent::EndCancelled => tracing::info!("session end cancelled; keeping control"),
        SessionEvent::Ending { close_app } => {
            tracing::info!(close_app, "session ending; returning fans to BIOS control");
            session_remote.halt();
            if release_for_session_end(chip.as_mut(), &snapshot, &gpus) {
                guardian.released();
            }
            logging::flush();
            process::exit(0);
        }
    })?;
    let power = fanforge_win::subscribe_power(move |event| match event {
        PowerEvent::Suspend => {
            if !remote.suspend() {
                tracing::warn!("control loop did not confirm the suspend in time");
            }
        }
        PowerEvent::Resume => remote.resume(),
    })
    .map_err(|error| tracing::warn!(%error, "sleep and resume notifications unavailable"))
    .ok();
    Ok(power)
}
