use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use fanforge_hw::HwError;
use fanforge_hw::ite::It8655e;
use fanforge_hw::port::PortIo;
use fanforge_hw::snapshot::BiosSnapshot;
use fanforge_hw::system::{force_all_auto, open_chip};

const RETRY_PAUSE: Duration = Duration::from_millis(100);

fn load_snapshot(path: &Path) -> BiosSnapshot {
    BiosSnapshot::load(path).unwrap_or_else(|error| {
        tracing::error!(%error, "BIOS snapshot unreadable; restoring BIOS defaults");
        BiosSnapshot::default()
    })
}

pub fn release_with<P: PortIo>(
    chip: &mut It8655e<P>,
    snapshot_path: &Path,
    budget: Duration,
) -> Result<(), HwError> {
    force_all_auto(chip, &load_snapshot(snapshot_path), budget)
}

pub fn release_preferring<P: PortIo>(
    chip: Option<&mut It8655e<P>>,
    snapshot_path: &Path,
    budget: Duration,
) -> Result<(), HwError> {
    let deadline = Instant::now() + budget;
    if let Some(chip) = chip {
        match release_with(chip, snapshot_path, budget) {
            Ok(()) => return Ok(()),
            Err(error) => {
                tracing::warn!(%error, "release through the existing handle failed; reopening")
            }
        }
    }
    release_from_disk(
        snapshot_path,
        deadline.saturating_duration_since(Instant::now()),
    )
}

pub fn release_from_disk(snapshot_path: &Path, budget: Duration) -> Result<(), HwError> {
    let snapshot = load_snapshot(snapshot_path);
    let deadline = Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let result =
            open_chip().and_then(|mut chip| force_all_auto(&mut chip, &snapshot, remaining));
        match result {
            Ok(()) => return Ok(()),
            Err(error) if Instant::now() + RETRY_PAUSE < deadline => {
                tracing::warn!(%error, "release attempt failed; retrying");
                thread::sleep(RETRY_PAUSE);
            }
            Err(error) => {
                tracing::error!(%error, "could not return every fan to BIOS control");
                return Err(error);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use fanforge_hw::ite::fake::{FakeIte, SharedFake};
    use fanforge_hw::mutex::NamedMutex;

    use super::*;

    #[test]
    fn releases_every_channel_from_the_saved_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.toml");
        let fake = SharedFake::new(FakeIte::new());
        fake.with(|chip| {
            for register in [0x15, 0x16, 0x17] {
                chip.banks[0][register] = 0x00;
            }
        });
        let lock =
            NamedMutex::open(&format!(r"Local\fanforge-failsafe-{}", std::process::id())).unwrap();
        let mut chip = It8655e::probe(fake.clone(), lock).unwrap();
        release_with(&mut chip, &path, Duration::from_secs(1)).unwrap();
        for register in [0x15, 0x16, 0x17] {
            assert_eq!(fake.with(|chip| chip.reg(register)) & 0x80, 0x80);
        }
    }
}
