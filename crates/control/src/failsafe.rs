use std::path::Path;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use mostlyfans_hw::HwError;
use mostlyfans_hw::gpu_fans::{GpuReleaser, NOT_RESPONDING};
use mostlyfans_hw::port::PortIo;
use mostlyfans_hw::second;
use mostlyfans_hw::snapshot::BiosSnapshot;
use mostlyfans_hw::superio::SuperIo;
use mostlyfans_hw::system::{force_all_auto, open_chip};

const RETRY_PAUSE: Duration = Duration::from_millis(100);

fn load_snapshot(path: &Path) -> BiosSnapshot {
    BiosSnapshot::load(path).unwrap_or_else(|error| {
        tracing::error!(%error, "BIOS snapshot unreadable; restoring BIOS defaults");
        BiosSnapshot::lost()
    })
}

fn nothing_to_release(error: &HwError, snapshot: &BiosSnapshot) -> bool {
    matches!(
        error,
        HwError::PawnIoMissing | HwError::ChipNotFound(_) | HwError::UnsupportedChip(_)
    ) && snapshot.owned.is_empty()
        && !snapshot.lost
}

pub fn release_with_gpus(
    gpus: Option<&GpuReleaser>,
    budget: Duration,
    chip: impl FnOnce(Duration) -> Result<(), HwError>,
) -> Result<(), String> {
    let pending = gpus.cloned().and_then(|gpus| {
        let (sender, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("failsafe-gpu".into())
            .spawn(move || sender.send(gpus.release(budget)).ok())
            .map_err(|error| tracing::error!(%error, "could not start the GPU fan release"))
            .ok()
            .map(|_| receiver)
    });
    let started = Instant::now();
    let chip = chip(budget).map_err(|error| error.to_string());
    let gpus = match pending {
        Some(receiver) => receiver
            .recv_timeout(budget.saturating_sub(started.elapsed()))
            .unwrap_or_else(|_| Err(NOT_RESPONDING.to_owned())),
        None if gpus.is_some() => Err("the GPU fan release could not start".to_owned()),
        None => Ok(()),
    };
    if let Err(error) = &gpus {
        tracing::error!(%error, "could not return every GPU fan to the driver");
    }
    chip.and(gpus)
}

fn with_second_chip(
    snapshot_path: &Path,
    budget: Duration,
    primary: impl FnOnce(Duration) -> Result<(), HwError>,
) -> Result<(), HwError> {
    let started = Instant::now();
    let (share, pending) = second::split_budget(snapshot_path, budget);
    let primary = primary(share);
    if !pending {
        return primary;
    }
    let remaining = budget.saturating_sub(started.elapsed());
    let second = second::release_from_disk(None, snapshot_path, remaining).inspect_err(|error| {
        tracing::error!(%error, "fans on the second chip may still be at MostlyFans's last speed");
    });
    primary.and(second)
}

pub fn release_with<P: PortIo>(
    chip: &mut SuperIo<P>,
    snapshot_path: &Path,
    budget: Duration,
) -> Result<(), HwError> {
    with_second_chip(snapshot_path, budget, |budget| {
        force_all_auto(chip, &load_snapshot(snapshot_path), budget)
    })
}

pub fn release_preferring<P: PortIo>(
    chip: Option<&mut SuperIo<P>>,
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
    with_second_chip(snapshot_path, budget, |budget| {
        release_primary_from_disk(snapshot_path, budget)
    })
}

fn release_primary_from_disk(snapshot_path: &Path, budget: Duration) -> Result<(), HwError> {
    let snapshot = load_snapshot(snapshot_path);
    let deadline = Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let result =
            open_chip().and_then(|mut chip| force_all_auto(&mut chip, &snapshot, remaining));
        match result {
            Ok(()) => return Ok(()),
            Err(error) if nothing_to_release(&error, &snapshot) => {
                tracing::info!(%error, "no fan controller to release");
                return Ok(());
            }
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
    use mostlyfans_hw::gpu_fans::fake::{FakeFan, FakeGpus, SharedGpus};
    use mostlyfans_hw::gpu_fans::ownership::{self, Ownership};
    use mostlyfans_hw::ite::fake::{FakeIte, SharedFake};
    use mostlyfans_hw::mutex::NamedMutex;
    use mostlyfans_hw::superio::Slot;

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
        let lock = NamedMutex::open(&format!(
            r"Local\mostlyfans-failsafe-{}",
            std::process::id()
        ))
        .unwrap();
        let mut chip = SuperIo::probe(fake.clone(), Slot::Primary, lock).unwrap();
        release_with(&mut chip, &path, Duration::from_secs(1)).unwrap();
        for register in [0x15, 0x16, 0x17] {
            assert_eq!(fake.with(|chip| chip.reg(register)) & 0x80, 0x80);
        }
    }

    #[test]
    fn gpu_fans_are_released_alongside_the_chip_even_when_the_chip_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpu_owned.toml");
        let fan = mostlyfans_core::FanId::for_gpu_fan(mostlyfans_hw::gpu_fans::fake::GPU_ID, 0);
        ownership::save(&path, &Ownership { fans: [fan].into() }).unwrap();
        let mut gpus = FakeGpus::default();
        gpus.fans.insert(
            0,
            FakeFan {
                manual: true,
                target: 70,
            },
        );
        let gpus = SharedGpus::new(gpus);
        let releaser = GpuReleaser::open(gpus.opener(), path);
        let result = release_with_gpus(Some(&releaser), Duration::from_secs(2), |_| {
            Err(HwError::LockTimeout("test".into()))
        });
        assert!(result.unwrap_err().contains("test"));
        assert!(!gpus.fan(0).manual);

        gpus.hold();
        let started = Instant::now();
        let result = release_with_gpus(Some(&releaser), Duration::from_millis(100), |_| Ok(()));
        assert_eq!(result, Err(NOT_RESPONDING.to_owned()));
        assert!(started.elapsed() < Duration::from_secs(1));
        gpus.unhold();
        assert_eq!(
            release_with_gpus(None, Duration::from_secs(1), |_| Ok(())),
            Ok(())
        );
    }

    #[test]
    fn a_missing_chip_with_nothing_owned_needs_no_release() {
        let snapshot = BiosSnapshot::default();
        assert!(nothing_to_release(&HwError::PawnIoMissing, &snapshot));
        assert!(nothing_to_release(
            &HwError::ChipNotFound(0xFFFF),
            &snapshot
        ));
        assert!(!nothing_to_release(
            &HwError::LockTimeout("bus".into()),
            &snapshot
        ));
        assert!(!nothing_to_release(
            &HwError::PawnIoMissing,
            &BiosSnapshot::lost()
        ));
        let mut owned = BiosSnapshot::default();
        owned.claim(0, Default::default());
        assert!(!nothing_to_release(&HwError::PawnIoMissing, &owned));
    }
}
