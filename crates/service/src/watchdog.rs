use std::panic;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use fanforge_hw::snapshot::BiosSnapshot;
use fanforge_hw::system::{force_all_auto, open_chip};

use crate::shared::Shared;

pub const EXIT_CODE: i32 = 3;
const POLL: Duration = Duration::from_millis(250);
const MIN_STALL: Duration = Duration::from_secs(3);
const FAIL_SAFE_BUDGET: Duration = Duration::from_secs(5);

static PANICKED: AtomicBool = AtomicBool::new(false);

pub fn install_panic_hook() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        PANICKED.store(true, Ordering::SeqCst);
        tracing::error!(%info, "panic in FanForge service");
        previous(info);
    }));
}

pub fn spawn(
    shared: Arc<Shared>,
    snapshot_path: PathBuf,
) -> std::io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("watchdog".into())
        .spawn(move || {
            loop {
                thread::sleep(POLL);
                let panicked = PANICKED.load(Ordering::SeqCst);
                let stalled = shared.heartbeat.stalled_for(MIN_STALL);
                if panicked || stalled.is_some() {
                    tracing::error!(
                        panicked,
                        ?stalled,
                        "control loop unhealthy; forcing BIOS fan control and exiting"
                    );
                    fail_safe(&snapshot_path);
                    process::exit(EXIT_CODE);
                }
            }
        })
}

fn fail_safe(snapshot_path: &Path) {
    let snapshot = BiosSnapshot::load(snapshot_path).unwrap_or_else(|error| {
        tracing::error!(%error, "watchdog could not read the BIOS snapshot");
        BiosSnapshot::default()
    });
    let deadline = Instant::now() + FAIL_SAFE_BUDGET;
    loop {
        let result = open_chip().and_then(|mut chip| force_all_auto(&mut chip, &snapshot));
        match result {
            Ok(()) => return,
            Err(error) if Instant::now() < deadline => {
                tracing::warn!(%error, "watchdog release attempt failed; retrying");
                thread::sleep(Duration::from_millis(200));
            }
            Err(error) => {
                tracing::error!(%error, "watchdog could not release every fan");
                return;
            }
        }
    }
}
