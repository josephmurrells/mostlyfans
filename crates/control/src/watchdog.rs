use std::panic;
use std::path::PathBuf;
use std::process;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use fanforge_hw::gpu_fans::GpuReleaser;

use crate::failsafe;
use crate::shared::Shared;

pub const EXIT_CODE: i32 = 3;
pub const MIN_STALL: Duration = Duration::from_secs(3);
pub const RELEASE_BUDGET: Duration = Duration::from_secs(1);
const POLL: Duration = Duration::from_millis(250);

pub fn stall_limit(interval: Duration) -> Duration {
    (interval * 3).max(MIN_STALL)
}

pub fn install_panic_hook() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let thread = thread::current();
        let name = thread.name().unwrap_or("unnamed");
        tracing::error!(thread = name, %info, "panic");
        previous(info);
    }));
}

pub fn spawn(
    shared: Arc<Shared>,
    snapshot_path: PathBuf,
    gpus: Option<GpuReleaser>,
    before_exit: fn(),
) -> std::io::Result<()> {
    thread::Builder::new()
        .name("watchdog".into())
        .spawn(move || {
            loop {
                thread::sleep(POLL);
                if let Some(stalled) = shared.heartbeat.stalled_for(MIN_STALL) {
                    tracing::error!(
                        ?stalled,
                        "control loop stalled; forcing BIOS fan control and exiting"
                    );
                    shared.halt();
                    failsafe::release_with_gpus(gpus.as_ref(), RELEASE_BUDGET, |budget| {
                        failsafe::release_from_disk(&snapshot_path, budget)
                    })
                    .ok();
                    before_exit();
                    process::exit(EXIT_CODE);
                }
            }
        })?;
    Ok(())
}
