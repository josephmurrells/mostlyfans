use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use fanforge_core::board::{BoardIdentity, BoardInfo, BoardProfile, FanLimits};
use fanforge_core::engine::{Action, Command};
use fanforge_core::telemetry::{FanReading, HardwareMode, Readings, SensorReading};
use fanforge_core::{Duty, FanId, SensorId};

use crate::HwError;
use crate::amd::AmdCpu;
use crate::board::{detect_identity, select_profile};
use crate::ite::{FAN_CHANNELS, It8655e};
use crate::mutex::{ISA_BUS, NamedMutex};
use crate::port::{LpcIo, PortIo};
use crate::snapshot::BiosSnapshot;

pub const RELEASE_BUDGET: Duration = Duration::from_secs(2);

pub type Chip = It8655e<LpcIo>;

pub fn open_chip() -> Result<Chip, HwError> {
    It8655e::probe(LpcIo::open_primary()?, NamedMutex::open(ISA_BUS)?)
}

pub fn channel_index(fan: &FanId) -> Option<usize> {
    let index = fan
        .as_str()
        .strip_prefix("pwm")?
        .parse::<usize>()
        .ok()?
        .checked_sub(1)?;
    (index < FAN_CHANNELS).then_some(index)
}

pub fn force_all_auto<P: PortIo>(
    chip: &mut It8655e<P>,
    snapshot: &BiosSnapshot,
) -> Result<(), HwError> {
    let mut first_error = None;
    for channel in 0..FAN_CHANNELS {
        if let Err(error) =
            chip.restore_patiently(channel, snapshot.channel(channel), RELEASE_BUDGET)
        {
            tracing::error!(channel, %error, "failed to return channel to BIOS control");
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

#[derive(Debug)]
pub struct Hardware<P: PortIo = LpcIo> {
    chip: It8655e<P>,
    cpu: Result<AmdCpu, String>,
    identity: BoardIdentity,
    profile: BoardProfile,
    snapshot: BiosSnapshot,
    snapshot_path: PathBuf,
}

impl Hardware<LpcIo> {
    pub fn open(snapshot_path: &Path) -> Result<Self, HwError> {
        let chip = open_chip()?;
        let identity = detect_identity();
        let profile = select_profile(&identity, chip.config().chip_id);
        let cpu = AmdCpu::open().map_err(|error| error.to_string());
        if let Err(error) = &cpu {
            tracing::warn!(%error, "CPU temperature sensors are unavailable");
        }
        Self::from_parts(chip, cpu, identity, profile, snapshot_path)
    }
}

impl<P: PortIo> Hardware<P> {
    pub fn from_parts(
        chip: It8655e<P>,
        cpu: Result<AmdCpu, String>,
        identity: BoardIdentity,
        profile: BoardProfile,
        snapshot_path: &Path,
    ) -> Result<Self, HwError> {
        let snapshot = BiosSnapshot::load(snapshot_path).unwrap_or_else(|error| {
            tracing::error!(%error, "BIOS snapshot is unreadable; starting a new one");
            BiosSnapshot::default()
        });
        Ok(Self {
            chip,
            cpu,
            identity,
            profile,
            snapshot,
            snapshot_path: snapshot_path.to_owned(),
        })
    }

    pub fn board_info(&self) -> BoardInfo {
        BoardInfo {
            identity: self.identity.clone(),
            profile_name: self.profile.name.clone(),
            chip_id: self.chip.config().chip_id,
            verified: self.profile.verified,
            read_only: self.profile.read_only,
        }
    }

    pub fn fan_limits(&self) -> BTreeMap<FanId, FanLimits> {
        self.profile.fan_limits()
    }

    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if let Err(error) = &self.cpu {
            warnings.push(format!("CPU temperatures unavailable: {error}"));
        }
        if self.profile.read_only {
            warnings.push(format!(
                "board `{}` has no profile; fans are monitored but not controlled",
                self.identity.product
            ));
        } else if !self.profile.verified {
            warnings.push(
                "board profile is unverified: header names are provisional and every fan keeps a 30% floor"
                    .to_owned(),
            );
        }
        warnings
    }

    pub fn snapshot(&self) -> &BiosSnapshot {
        &self.snapshot
    }

    fn persist_snapshot(&self) -> Result<(), HwError> {
        self.snapshot
            .save(&self.snapshot_path)
            .map_err(HwError::from)
    }

    pub fn initialize_channels(&mut self, take_control: bool) -> Result<(), HwError> {
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
        for (index, temp) in chip.temps.iter().enumerate() {
            sensors.push(SensorReading {
                id: BoardProfile::temp_sensor_id(index),
                label: self.profile.temp_label(index),
                celsius: temp.map(f32::from),
            });
        }
        let fans = self
            .profile
            .fans
            .iter()
            .filter(|channel| channel.index < FAN_CHANNELS)
            .map(|channel| {
                let state = chip.channels[channel.index];
                let mode = if state.is_auto() {
                    HardwareMode::Auto
                } else {
                    HardwareMode::Manual
                };
                FanReading {
                    id: FanId::for_channel(channel.index),
                    label: channel.name.clone(),
                    rpm: chip.rpm[channel.index],
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

    pub fn apply(&mut self, command: &Command) -> Result<(), HwError> {
        let channel = channel_index(&command.fan)
            .ok_or_else(|| HwError::UnknownFan(command.fan.to_string()))?;
        match command.action {
            Action::SetDuty(duty) => {
                if !self.snapshot.is_owned(channel) {
                    let current = self.chip.channel_states()?[channel];
                    self.snapshot.claim(channel, current);
                    if let Err(error) = self.persist_snapshot() {
                        self.snapshot.release(channel);
                        return Err(error);
                    }
                }
                self.chip.set_duty(channel, duty.to_raw())
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
        for channel in 0..FAN_CHANNELS {
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
    use crate::board::embedded_profiles;
    use crate::ite::fake::{FakeIte, SharedFake};

    fn lock(tag: &str) -> NamedMutex {
        NamedMutex::open(&format!(
            r"Local\fanforge-system-{tag}-{}",
            std::process::id()
        ))
        .unwrap()
    }

    fn hardware(tag: &str, fake: &SharedFake, path: &Path) -> Hardware<SharedFake> {
        let chip = It8655e::probe(fake.clone(), lock(tag)).unwrap();
        Hardware::from_parts(
            chip,
            Err("no CPU in tests".to_owned()),
            BoardIdentity::default(),
            embedded_profiles().remove(0),
            path,
        )
        .unwrap()
    }

    #[test]
    fn channel_ids_map_to_indices() {
        assert_eq!(channel_index(&"pwm1".into()), Some(0));
        assert_eq!(channel_index(&"pwm3".into()), Some(2));
        assert_eq!(channel_index(&"pwm4".into()), None);
        assert_eq!(channel_index(&"pwm0".into()), None);
        assert_eq!(channel_index(&"fan1".into()), None);
    }

    #[test]
    fn ownership_is_persisted_before_the_first_manual_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.toml");
        let fake = SharedFake::new(FakeIte::new());
        let mut hardware = hardware("own", &fake, &path);
        hardware.initialize_channels(true).unwrap();
        hardware
            .apply(&Command {
                fan: "pwm2".into(),
                action: Action::SetDuty(Duty::new(60.0)),
            })
            .unwrap();
        let saved = BiosSnapshot::load(&path).unwrap();
        assert!(saved.is_owned(1));
        assert_eq!(saved.channel(1).unwrap().duty, 0x40);

        hardware
            .apply(&Command {
                fan: "pwm2".into(),
                action: Action::Release,
            })
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
                .apply(&Command {
                    fan: "pwm1".into(),
                    action: Action::SetDuty(Duty::new(35.0)),
                })
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
        let result = hardware.apply(&Command {
            fan: "pwm1".into(),
            action: Action::SetDuty(Duty::new(50.0)),
        });
        assert!(result.is_err());
        assert_eq!(fake.with(|chip| chip.reg(0x15)), 0x80);
    }
}
