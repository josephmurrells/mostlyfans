use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use fanforge_core::board::{
    BoardIdentity, BoardInfo, FanLimits, default_fan_name, fan_limits, temp_label, temp_sensor_id,
};
use fanforge_core::engine::{Action, Command};
use fanforge_core::telemetry::{FanReading, HardwareMode, Readings, SensorReading};
use fanforge_core::{Duty, FanId, SensorId};

use crate::HwError;
use crate::amd::AmdCpu;
use crate::board::detect_identity;
use crate::mutex::{ISA_BUS, NamedMutex};
use crate::port::{LpcIo, PortIo};
use crate::snapshot::BiosSnapshot;
use crate::superio::{SuperIo, detect};

pub const RELEASE_BUDGET: Duration = Duration::from_secs(2);

pub type Chip = SuperIo<LpcIo>;

pub fn open_chip() -> Result<Chip, HwError> {
    detect(LpcIo::open, || NamedMutex::open(ISA_BUS), true)
}

pub fn attach_chip() -> Result<Chip, HwError> {
    detect(LpcIo::open, || NamedMutex::open(ISA_BUS), false)
}

pub fn channel_index(fan: &FanId, channels: usize) -> Option<usize> {
    let index = fan
        .as_str()
        .strip_prefix("pwm")?
        .parse::<usize>()
        .ok()?
        .checked_sub(1)?;
    (index < channels).then_some(index)
}

pub fn force_all_auto<P: PortIo>(
    chip: &mut SuperIo<P>,
    snapshot: &BiosSnapshot,
    budget: Duration,
) -> Result<(), HwError> {
    let chip_id = chip.config().chip_id;
    let error = HwError::ForeignSnapshot {
        owner: snapshot.chip_id,
        found: chip_id,
    };
    let foreign = match (snapshot.belongs_to(chip_id), chip.fan_control()) {
        (true, _) => None,
        (false, true) => Some(error),
        (false, false) => {
            tracing::error!(%error, "refusing to report fans released");
            return Err(error);
        }
    };
    let empty = BiosSnapshot::for_chip(chip_id);
    let snapshot = if snapshot.chip_id == chip_id {
        snapshot
    } else {
        &empty
    };
    let deadline = Instant::now() + budget;
    let mut first_error = None;
    for channel in 0..chip.fan_count() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if let Err(error) = chip.restore_patiently(channel, snapshot.channel(channel), remaining) {
            tracing::error!(channel, %error, "failed to return channel to BIOS control");
            first_error.get_or_insert(error);
        }
    }
    match first_error.or(foreign) {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

#[derive(Debug)]
pub struct Hardware<P: PortIo = LpcIo> {
    chip: SuperIo<P>,
    cpu: Result<AmdCpu, String>,
    identity: BoardIdentity,
    snapshot: BiosSnapshot,
    snapshot_path: PathBuf,
    foreign_owner: Option<u16>,
}

impl Hardware<LpcIo> {
    pub fn open(snapshot_path: &Path) -> Result<Self, HwError> {
        let chip = open_chip()?;
        let identity = detect_identity();
        let cpu = AmdCpu::open().map_err(|error| error.to_string());
        if let Err(error) = &cpu {
            tracing::warn!(%error, "CPU temperature sensors are unavailable");
        }
        Self::from_parts(chip, cpu, identity, snapshot_path)
    }
}

impl<P: PortIo> Hardware<P> {
    pub fn from_parts(
        chip: SuperIo<P>,
        cpu: Result<AmdCpu, String>,
        identity: BoardIdentity,
        snapshot_path: &Path,
    ) -> Result<Self, HwError> {
        let chip_id = chip.config().chip_id;
        let mut snapshot = BiosSnapshot::load_for(snapshot_path, chip_id).unwrap_or_else(|error| {
            tracing::error!(%error, "BIOS snapshot is unreadable; starting a new one");
            BiosSnapshot::for_chip(chip_id)
        });
        let foreign_owner = (!snapshot.belongs_to(chip_id)).then_some(snapshot.chip_id);
        if snapshot.chip_id != chip_id {
            if foreign_owner.is_some() {
                tracing::warn!(
                    saved = format_args!("{:#06x}", snapshot.chip_id),
                    "BIOS snapshot owns fans on another chip; refusing to report them released"
                );
            }
            snapshot = BiosSnapshot::for_chip(chip_id);
        }
        Ok(Self {
            chip,
            cpu,
            identity,
            snapshot,
            snapshot_path: snapshot_path.to_owned(),
            foreign_owner,
        })
    }

    pub fn board_info(&self) -> BoardInfo {
        BoardInfo {
            identity: self.identity.clone(),
            chip_id: self.chip.config().chip_id,
            chip_name: self.chip.chip().name.to_owned(),
            fan_control: self.chip.fan_control(),
            fan_headers: self.chip.fan_count(),
            likely_cpu_fan: Some(FanId::for_channel(0)),
        }
    }

    pub fn fan_limits(&self) -> BTreeMap<FanId, FanLimits> {
        fan_limits(self.chip.fan_count())
    }

    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if let Err(error) = &self.cpu {
            warnings.push(format!("CPU temperatures unavailable: {error}"));
        }
        warnings
    }

    pub fn snapshot(&self) -> &BiosSnapshot {
        &self.snapshot
    }

    pub fn fan_control(&self) -> bool {
        self.chip.fan_control()
    }

    pub fn owns_any(&self) -> bool {
        self.fan_control() && !self.snapshot.owned.is_empty()
    }

    fn persist_snapshot(&self) -> Result<(), HwError> {
        self.snapshot
            .save(&self.snapshot_path)
            .map_err(HwError::from)
    }

    pub fn initialize_channels(&mut self, take_control: bool) -> Result<(), HwError> {
        if !self.fan_control() {
            return Ok(());
        }
        let states = self.chip.channel_states()?;
        let mut changed = self.snapshot.refresh(&states);
        let result = if take_control {
            let result = self.release_all_channels();
            changed = true;
            result
        } else {
            Ok(())
        };
        if changed && let Err(error) = self.persist_snapshot() {
            tracing::error!(%error, "failed to persist BIOS snapshot");
        }
        result
    }

    pub fn reinitialize(&mut self) -> Result<(), HwError> {
        self.chip.reinitialize()
    }

    pub fn read(&mut self) -> Result<Readings, HwError> {
        let chip = self.chip.read()?;
        let mut sensors = self.cpu_sensors();
        let names = self.chip.temp_names();
        for (index, temp) in chip.temps.iter().enumerate() {
            let label = match names.get(index).copied().flatten() {
                Some(name) => format!("{} ({name})", temp_label(index)),
                None => temp_label(index),
            };
            sensors.push(SensorReading {
                id: temp_sensor_id(index),
                label,
                celsius: *temp,
            });
        }
        let fans = chip
            .channels
            .iter()
            .enumerate()
            .map(|(index, state)| {
                let mode = if state.is_auto() {
                    HardwareMode::Auto
                } else {
                    HardwareMode::Manual
                };
                FanReading {
                    id: FanId::for_channel(index),
                    label: default_fan_name(index),
                    rpm: chip.rpm[index],
                    mode,
                    duty: (mode == HardwareMode::Manual).then(|| Duty::from_raw(state.duty)),
                }
            })
            .collect();
        Ok(Readings { sensors, fans })
    }

    fn cpu_sensors(&self) -> Vec<SensorReading> {
        let temps = match &self.cpu {
            Ok(cpu) => cpu.read().map_err(|error| {
                tracing::warn!(%error, "failed to read CPU temperatures");
            }),
            Err(_) => Err(()),
        };
        let Ok(temps) = temps else {
            return vec![sensor("cpu/tctl", "CPU Tctl", None)];
        };
        let mut sensors = vec![sensor("cpu/tctl", "CPU Tctl", Some(temps.tctl))];
        if let Some(tdie) = temps.tdie {
            sensors.push(sensor("cpu/tdie", "CPU Tdie", Some(tdie)));
        }
        sensors.extend(temps.ccds.into_iter().map(|(index, celsius)| {
            sensor(
                &format!("cpu/ccd{}", index + 1),
                &format!("CPU CCD{}", index + 1),
                Some(celsius),
            )
        }));
        sensors
    }

    pub fn apply(&mut self, command: &Command, halted: &AtomicBool) -> Result<(), HwError> {
        let channel = channel_index(&command.fan, self.chip.fan_count())
            .ok_or_else(|| HwError::UnknownFan(command.fan.to_string()))?;
        if !self.fan_control() {
            return match command.action {
                Action::SetDuty(_) => Err(HwError::Unverified(self.chip.chip().name)),
                Action::Release => Ok(()),
            };
        }
        match command.action {
            Action::SetDuty(duty) => {
                if halted.load(Ordering::SeqCst) {
                    return Err(HwError::Halted);
                }
                if !self.snapshot.is_owned(channel) {
                    let current = self.chip.channel_states()?[channel];
                    if !self.chip.chip().forces_auto_without_snapshot()
                        && !current.is_auto()
                        && self.snapshot.channel(channel).is_none()
                    {
                        return Err(HwError::UnknownBiosMode(channel));
                    }
                    self.snapshot.claim(channel, current);
                    if let Err(error) = self.persist_snapshot() {
                        self.snapshot.release(channel);
                        return Err(error);
                    }
                }
                self.chip
                    .set_duty_unless_halted(channel, duty.to_raw(), halted)
            }
            Action::Release => {
                self.chip.restore_patiently(
                    channel,
                    self.snapshot.channel(channel),
                    RELEASE_BUDGET,
                )?;
                if self.snapshot.release(channel)
                    && let Err(error) = self.persist_snapshot()
                {
                    tracing::warn!(%error, "failed to persist released channel");
                }
                Ok(())
            }
        }
    }

    fn release_all_channels(&mut self) -> Result<(), HwError> {
        let mut first_error = None;
        for channel in 0..self.chip.fan_count() {
            match self.chip.restore_patiently(
                channel,
                self.snapshot.channel(channel),
                RELEASE_BUDGET,
            ) {
                Ok(()) => {
                    self.snapshot.release(channel);
                }
                Err(error) => {
                    tracing::error!(channel, %error, "failed to return channel to BIOS control");
                    first_error.get_or_insert(error);
                }
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    pub fn release_all(&mut self) -> Result<(), HwError> {
        if let Some(owner) = self.foreign_owner.filter(|_| !self.fan_control()) {
            return Err(HwError::ForeignSnapshot {
                owner,
                found: self.chip.config().chip_id,
            });
        }
        if !self.fan_control() {
            return Ok(());
        }
        let result = self.release_all_channels();
        if let Err(error) = self.persist_snapshot() {
            tracing::warn!(%error, "failed to persist BIOS snapshot after release");
        }
        result
    }
}

fn sensor(id: &str, label: &str, celsius: Option<f32>) -> SensorReading {
    SensorReading {
        id: SensorId::new(id),
        label: label.to_owned(),
        celsius,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static RUNNING: AtomicBool = AtomicBool::new(false);
    use crate::ite::fake::{FakeIte, SharedFake};
    use crate::superio::Slot;

    fn lock(tag: &str) -> NamedMutex {
        NamedMutex::open(&format!(
            r"Local\fanforge-system-{tag}-{}",
            std::process::id()
        ))
        .unwrap()
    }

    fn hardware(tag: &str, fake: &SharedFake, path: &Path) -> Hardware<SharedFake> {
        let chip = SuperIo::probe(fake.clone(), Slot::Primary, lock(tag)).unwrap();
        Hardware::from_parts(
            chip,
            Err("no CPU in tests".to_owned()),
            BoardIdentity::default(),
            path,
        )
        .unwrap()
    }

    #[test]
    fn header_one_is_the_likely_cpu_fan() {
        let dir = tempfile::tempdir().unwrap();
        let fake = SharedFake::new(FakeIte::new());
        let hardware = hardware("cpu-guess", &fake, &dir.path().join("snapshot.toml"));
        let info = hardware.board_info();
        assert_eq!(info.fan_headers, 3);
        assert_eq!(info.chip_name, "ITE IT8655E");
        assert!(info.fan_control);
        assert_eq!(info.likely_cpu_fan, Some(FanId::from("pwm1")));
    }

    #[test]
    fn channel_ids_map_to_indices() {
        assert_eq!(channel_index(&"pwm1".into(), 3), Some(0));
        assert_eq!(channel_index(&"pwm3".into(), 3), Some(2));
        assert_eq!(channel_index(&"pwm4".into(), 3), None);
        assert_eq!(channel_index(&"pwm4".into(), 5), Some(3));
        assert_eq!(channel_index(&"pwm0".into(), 3), None);
        assert_eq!(channel_index(&"fan1".into(), 3), None);
    }

    #[test]
    fn ownership_is_persisted_before_the_first_manual_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.toml");
        let fake = SharedFake::new(FakeIte::new());
        let mut hardware = hardware("own", &fake, &path);
        hardware.initialize_channels(true).unwrap();
        hardware
            .apply(
                &Command {
                    fan: "pwm2".into(),
                    action: Action::SetDuty(Duty::new(60.0)),
                },
                &RUNNING,
            )
            .unwrap();
        let saved = BiosSnapshot::load(&path).unwrap();
        assert!(saved.is_owned(1));
        assert_eq!(saved.channel(1).unwrap().duty, 0x40);

        hardware
            .apply(
                &Command {
                    fan: "pwm2".into(),
                    action: Action::Release,
                },
                &RUNNING,
            )
            .unwrap();
        assert!(!BiosSnapshot::load(&path).unwrap().is_owned(1));
        assert_eq!(fake.with(|chip| chip.reg(0x6B)), 0x40);
    }

    #[test]
    fn a_crash_restart_restores_the_pre_control_bios_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.toml");
        let fake = SharedFake::new(FakeIte::new());
        {
            let mut hardware = hardware("crash-a", &fake, &path);
            hardware.initialize_channels(true).unwrap();
            hardware
                .apply(
                    &Command {
                        fan: "pwm1".into(),
                        action: Action::SetDuty(Duty::new(35.0)),
                    },
                    &RUNNING,
                )
                .unwrap();
        }
        let mut restarted = hardware("crash-b", &fake, &path);
        restarted.initialize_channels(true).unwrap();
        let state = fake.with(|chip| (chip.reg(0x15), chip.reg(0x63)));
        assert_eq!(state, (0x80, 0x40));
        assert!(!BiosSnapshot::load(&path).unwrap().is_owned(0));
    }

    #[test]
    fn manual_control_is_refused_when_the_snapshot_cannot_be_saved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing-dir").join("blocked");
        std::fs::write(dir.path().join("missing-dir"), b"file, not a directory").unwrap();
        let fake = SharedFake::new(FakeIte::new());
        let mut hardware = hardware("refuse", &fake, &path);
        let result = hardware.apply(
            &Command {
                fan: "pwm1".into(),
                action: Action::SetDuty(Duty::new(50.0)),
            },
            &RUNNING,
        );
        assert!(result.is_err());
        assert_eq!(fake.with(|chip| chip.reg(0x15)), 0x80);
    }

    #[test]
    fn a_halted_flag_refuses_manual_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.toml");
        let fake = SharedFake::new(FakeIte::new());
        let mut hardware = hardware("halted", &fake, &path);
        let halted = AtomicBool::new(true);
        let command = Command {
            fan: "pwm1".into(),
            action: Action::SetDuty(Duty::new(50.0)),
        };
        assert!(matches!(
            hardware.apply(&command, &halted),
            Err(HwError::Halted)
        ));
        assert!(matches!(
            hardware.chip.set_duty_unless_halted(0, 0x10, &halted),
            Err(HwError::Halted)
        ));
        assert_eq!(fake.with(|chip| chip.reg(0x15)), 0x80);
        assert!(!hardware.owns_any());
    }

    fn set(fan: &str, duty: f32) -> Command {
        Command {
            fan: fan.into(),
            action: Action::SetDuty(Duty::new(duty)),
        }
    }

    fn release(fan: &str) -> Command {
        Command {
            fan: fan.into(),
            action: Action::Release,
        }
    }

    #[test]
    fn the_it8655e_still_claims_a_manual_header_without_a_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.toml");
        let mut ite = FakeIte::new();
        ite.banks[0][0x16] = 0x01;
        let fake = SharedFake::new(ite);
        let mut hardware = hardware("8655-manual", &fake, &path);
        hardware.apply(&set("pwm2", 60.0), &RUNNING).unwrap();
        assert!(hardware.owns_any());
        assert_eq!(hardware.snapshot.channel(1), None);
        hardware.apply(&release("pwm2"), &RUNNING).unwrap();
        assert_eq!(fake.with(|chip| chip.reg(0x16)), 0x81);
    }

    #[test]
    fn the_it8689e_restores_all_six_headers_after_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.toml");
        let mut ite = FakeIte::chip(0x8689);
        ite.banks[0][0xAF] = 0x05;
        let fake = SharedFake::new(ite);
        {
            let mut hardware = hardware("8689-a", &fake, &path);
            assert!(hardware.fan_control());
            hardware.initialize_channels(true).unwrap();
            for fan in ["pwm1", "pwm2", "pwm3", "pwm4", "pwm5"] {
                hardware.apply(&set(fan, 60.0), &RUNNING).unwrap();
            }
            assert!(matches!(
                hardware.apply(&set("pwm6", 60.0), &RUNNING),
                Err(HwError::UnknownBiosMode(5))
            ));
        }
        fake.with(|chip| assert_eq!(chip.reg(0xA7), 0x7F));
        let mut restarted = hardware("8689-b", &fake, &path);
        restarted.initialize_channels(true).unwrap();
        fake.with(|chip| {
            for (channel, register) in [0x15, 0x16, 0x17, 0x7F, 0xA7].into_iter().enumerate() {
                assert_eq!(chip.reg(register), 0x80 | channel as u8);
            }
            assert_eq!((chip.reg(0xAF), chip.reg(0xAB)), (0x05, 0x40));
        });
        assert!(!restarted.owns_any());
    }

    #[test]
    fn unverified_chips_are_monitored_without_touching_fans_or_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.toml");
        let fake = SharedFake::new(FakeIte::chip(0x8686));
        let mut hardware = hardware("unverified", &fake, &path);
        let info = hardware.board_info();
        assert_eq!(info.chip_name, "ITE IT8686E");
        assert!(!info.fan_control);
        assert_eq!(info.fan_headers, 5);
        assert_eq!(hardware.fan_limits().len(), 5);

        hardware.initialize_channels(true).unwrap();
        let readings = hardware.read().unwrap();
        assert_eq!(readings.fans.len(), 5);
        assert!(matches!(
            hardware.apply(&set("pwm1", 60.0), &RUNNING),
            Err(HwError::Unverified(_))
        ));
        hardware.apply(&release("pwm1"), &RUNNING).unwrap();
        hardware.release_all().unwrap();
        let snapshot = hardware.snapshot.clone();
        force_all_auto(&mut hardware.chip, &snapshot, RELEASE_BUDGET).unwrap();
        assert!(!path.exists());
        assert!(!hardware.owns_any());
        fake.with(|chip| {
            assert!(chip.writes.iter().all(|&(port, ..)| port != chip.base + 6));
        });
    }

    #[test]
    fn a_leftover_snapshot_is_left_alone_on_an_unverified_chip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.toml");
        let mut old = BiosSnapshot::default();
        old.claim(0, crate::superio::ChannelState::default());
        old.save(&path).unwrap();
        let before = std::fs::read(&path).unwrap();
        let fake = SharedFake::new(FakeIte::chip(0x8628));
        let mut hardware = hardware("leftover", &fake, &path);
        assert!(!hardware.owns_any());
        hardware.initialize_channels(true).unwrap();
        let error = hardware.release_all().unwrap_err();
        assert!(matches!(
            error,
            HwError::ForeignSnapshot {
                owner: 0x8655,
                found: 0x8628
            }
        ));
        assert!(error.to_string().contains("delete"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let mut chip = SuperIo::probe(fake.clone(), Slot::Primary, lock("leftover-chip")).unwrap();
        let old = BiosSnapshot::load(&path).unwrap();
        assert!(matches!(
            force_all_auto(&mut chip, &old, RELEASE_BUDGET),
            Err(HwError::ForeignSnapshot { .. })
        ));
        assert!(fake.with(|chip| chip.writes.iter().all(|&(port, ..)| port != chip.base + 6)));
    }

    #[test]
    fn a_snapshot_from_another_chip_is_never_restored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.toml");
        let mut foreign = BiosSnapshot::for_chip(0xD42B);
        foreign.claim(
            0,
            crate::superio::ChannelState {
                control: 0x80 | 0x33,
                duty: 0x11,
                main_enabled: true,
                ..Default::default()
            },
        );
        foreign.save(&path).unwrap();
        let fake = SharedFake::new(FakeIte::new());
        fake.with(|chip| chip.banks[0][0x15] = 0x00);
        let hardware = hardware("foreign", &fake, &path);
        assert!(!hardware.owns_any());
        assert_eq!(hardware.snapshot().chip_id, 0x8655);

        let mut chip = SuperIo::probe(fake.clone(), Slot::Primary, lock("foreign-chip")).unwrap();
        assert!(matches!(
            force_all_auto(&mut chip, &foreign, RELEASE_BUDGET),
            Err(HwError::ForeignSnapshot {
                owner: 0xD42B,
                found: 0x8655
            })
        ));
        assert_eq!(fake.with(|chip| chip.reg(0x15)), 0x80);
        assert_eq!(fake.with(|chip| chip.reg(0x63)), 0x40);

        fake.with(|chip| chip.banks[0][0x15] = 0x00);
        foreign.owned.clear();
        force_all_auto(&mut chip, &foreign, RELEASE_BUDGET).unwrap();
        assert_eq!(fake.with(|chip| chip.reg(0x15)), 0x80);
    }

    static VERIFIED_NCT: crate::chips::ChipSpec = crate::chips::ChipSpec {
        name: "test NCT6779D",
        verified: true,
        ident: crate::chips::Ident::Nuvoton {
            id: 0xC5,
            revision: 0x60,
            mask: 0xF0,
        },
        map: crate::chips::ChipMap::Nuvoton(crate::chips::NctMap {
            fans: 5,
            tach: crate::chips::Tach::Count {
                registers: &[0x4B0, 0x4B2, 0x4B4, 0x4B6, 0x4B8],
            },
            slots: &[],
            sources: &[],
            io_lock: false,
        }),
    };

    #[test]
    fn a_manual_nuvoton_channel_without_a_saved_bios_mode_is_never_claimed() {
        use crate::nuvoton::fake::{FakeNct, SharedNct};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.toml");
        let mut nct = FakeNct::new(0xC5, 0x62);
        nct.set(0x102, 0x00);
        let fake = SharedNct::new(nct);
        let mut chip = SuperIo::probe(fake.clone(), Slot::Primary, lock("nct-claim")).unwrap();
        chip.config.chip = &VERIFIED_NCT;
        let mut hardware = Hardware::from_parts(
            chip,
            Err("no CPU in tests".to_owned()),
            BoardIdentity::default(),
            &path,
        )
        .unwrap();
        assert!(matches!(
            hardware.apply(&set("pwm1", 60.0), &RUNNING),
            Err(HwError::UnknownBiosMode(0))
        ));
        assert!(!hardware.owns_any());
        hardware.apply(&set("pwm2", 60.0), &RUNNING).unwrap();
        assert!(hardware.owns_any());
        assert_eq!(fake.with(|chip| chip.get(0x202)), 0x00);
        hardware.apply(&release("pwm2"), &RUNNING).unwrap();
        assert_eq!(
            fake.with(|chip| (chip.get(0x202), chip.get(0x209))),
            (0x50, 0x80)
        );
    }
}
