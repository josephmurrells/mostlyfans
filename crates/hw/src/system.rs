use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use fanforge_core::board::{
    BoardIdentity, BoardInfo, ChipInfo, FanLimits, chip_label, chip_temp_sensor_id,
    default_fan_name, temp_label, temp_sensor_id,
};
use fanforge_core::engine::{Action, Command};
use fanforge_core::telemetry::{FanReading, HardwareMode, Readings, SensorReading};
use fanforge_core::{Duty, FanId, SensorId};

use crate::HwError;
use crate::board::detect_identity;
use crate::cpu::{CPU_SENSOR, Cpu};
use crate::mutex::{ISA_BUS, NamedMutex};
use crate::port::{LpcIo, PortIo};
use crate::snapshot::BiosSnapshot;
use crate::superio::{ChipReadings, SuperIo, detect, detect_second};

pub const RELEASE_BUDGET: Duration = Duration::from_secs(2);
pub const NO_CONTROL_PIN: &str = "the board uses this header's control pin for something else";

pub const SECOND_CHIP: usize = 1;
const SECOND_FAILURES_BEFORE_BACKOFF: u32 = 3;
const SECOND_BACKOFF: Duration = Duration::from_secs(10);

pub type Chip = SuperIo<LpcIo>;

pub fn open_chip() -> Result<Chip, HwError> {
    detect(LpcIo::open, || NamedMutex::open(ISA_BUS), true)
}

pub fn attach_chip() -> Result<Chip, HwError> {
    detect(LpcIo::open, || NamedMutex::open(ISA_BUS), false)
}

pub fn channel_index(fan: &FanId, channels: usize) -> Option<usize> {
    fan.header_channel()
        .filter(|&(chip, index)| chip == 0 && index < channels)
        .map(|(_, index)| index)
}

fn wired_headers<P: PortIo>(chip: &SuperIo<P>) -> impl Iterator<Item = usize> + use<P> {
    let wiring = chip.wiring();
    (0..chip.fan_count()).filter(move |&channel| wiring.is_wired(channel))
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
    let foreign = match (snapshot.belongs_to(chip_id), chip.can_release()) {
        (true, _) => None,
        (false, true) => Some(error),
        (false, false) => {
            tracing::error!(%error, "refusing to report fans released");
            return Err(error);
        }
    };
    let empty = BiosSnapshot {
        lost: snapshot.lost && snapshot.owned.is_empty(),
        ..BiosSnapshot::for_chip(chip_id)
    };
    let snapshot = if snapshot.chip_id == chip_id {
        snapshot
    } else {
        &empty
    };
    let deadline = Instant::now() + budget;
    let mut first_error = None;
    let widen = chip.fan_control() && (snapshot.lost || chip.chip().forces_auto_without_snapshot());
    for channel in (0..chip.fan_count()).filter(|&channel| widen || snapshot.is_owned(channel)) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            tracing::error!(channel, "no time left to return channel to BIOS control");
            first_error.get_or_insert(HwError::ReleaseOutOfTime(channel));
            continue;
        }
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
    cpu: Result<Cpu, String>,
    identity: BoardIdentity,
    snapshot: BiosSnapshot,
    snapshot_path: PathBuf,
    foreign_owner: Option<u16>,
    second: Option<SecondChip<P>>,
}

#[derive(Debug)]
struct SecondChip<P: PortIo> {
    chip: SuperIo<P>,
    failures: u32,
    retry_at: Option<Instant>,
}

impl<P: PortIo> SecondChip<P> {
    fn info(&self) -> ChipInfo {
        ChipInfo {
            chip_id: self.chip.config().chip_id,
            chip_name: self.chip.chip().name.to_owned(),
            fan_control: false,
            pwm_misconfigured: self.chip.pwm_misconfigured(),
            fan_headers: wired_headers(&self.chip).count(),
        }
    }

    fn poll(&mut self) -> Option<ChipReadings> {
        if self.retry_at.is_some_and(|at| Instant::now() < at) {
            return None;
        }
        match self.chip.read() {
            Ok(readings) => {
                if self.failures > 0 {
                    tracing::info!(
                        chip = self.chip.chip().name,
                        "the second fan chip answers again"
                    );
                }
                self.failures = 0;
                self.retry_at = None;
                Some(readings)
            }
            Err(error) => {
                self.failures += 1;
                if self.failures == 1 {
                    tracing::warn!(%error, chip = self.chip.chip().name, "the second fan chip could not be read");
                }
                if self.failures >= SECOND_FAILURES_BEFORE_BACKOFF {
                    self.retry_at = Some(Instant::now() + SECOND_BACKOFF);
                }
                None
            }
        }
    }

    fn read_into(&mut self, sensors: &mut Vec<SensorReading>, fans: &mut Vec<FanReading>) {
        let readings = self.poll();
        let label = chip_label(self.chip.chip().name);
        for index in 0..self.chip.temp_names().len() {
            sensors.push(SensorReading {
                id: chip_temp_sensor_id(SECOND_CHIP, index),
                label: format!("{} ({label})", temp_label(index)),
                celsius: readings
                    .as_ref()
                    .and_then(|readings| readings.temps.get(index).copied().flatten()),
            });
        }
        let wiring = self.chip.wiring();
        for index in wired_headers(&self.chip) {
            let manual = readings
                .as_ref()
                .and_then(|readings| readings.channels.get(index))
                .is_some_and(|state| !state.is_auto());
            fans.push(FanReading {
                id: FanId::for_chip_channel(SECOND_CHIP, index),
                label: format!("{} ({label})", default_fan_name(index)),
                rpm: readings
                    .as_ref()
                    .and_then(|readings| readings.rpm.get(index).copied().flatten())
                    .filter(|_| wiring.has_tach(index)),
                mode: if manual {
                    HardwareMode::Manual
                } else {
                    HardwareMode::Auto
                },
                duty: None,
                unavailable: None,
                monitor_only: true,
            });
        }
    }
}

impl Hardware<LpcIo> {
    pub fn open(snapshot_path: &Path) -> Result<Self, HwError> {
        let chip = open_chip()?;
        let second = detect_second(
            &chip.config(),
            LpcIo::open,
            || NamedMutex::open(ISA_BUS),
            true,
        );
        let identity = detect_identity();
        let cpu = Cpu::open().map_err(|error| error.to_string());
        if let Err(error) = &cpu {
            tracing::warn!(%error, "CPU temperature sensors are unavailable");
        }
        Ok(Self::from_parts(chip, cpu, identity, snapshot_path)?.with_second(second))
    }
}

impl<P: PortIo> Hardware<P> {
    pub fn from_parts(
        chip: SuperIo<P>,
        cpu: Result<Cpu, String>,
        identity: BoardIdentity,
        snapshot_path: &Path,
    ) -> Result<Self, HwError> {
        let chip_id = chip.config().chip_id;
        let mut snapshot = BiosSnapshot::load_for(snapshot_path, chip_id).unwrap_or_else(|error| {
            tracing::error!(%error, "BIOS snapshot is unreadable; starting a new one");
            BiosSnapshot {
                lost: true,
                ..BiosSnapshot::for_chip(chip_id)
            }
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
            second: None,
        })
    }

    pub fn with_second(mut self, chip: Option<SuperIo<P>>) -> Self {
        self.second = chip.map(|chip| SecondChip {
            chip,
            failures: 0,
            retry_at: None,
        });
        self
    }

    pub fn second_chip(&self) -> Option<&SuperIo<P>> {
        self.second.as_ref().map(|second| &second.chip)
    }

    pub fn board_info(&self) -> BoardInfo {
        BoardInfo {
            identity: self.identity.clone(),
            chip_id: self.chip.config().chip_id,
            chip_name: self.chip.chip().name.to_owned(),
            fan_control: self.chip.fan_control(),
            pwm_misconfigured: self.chip.pwm_misconfigured(),
            fan_headers: self.headers().count(),
            likely_cpu_fan: self
                .chip
                .wiring()
                .is_wired(0)
                .then(|| FanId::for_channel(0)),
            chips: std::iter::once(ChipInfo {
                chip_id: self.chip.config().chip_id,
                chip_name: self.chip.chip().name.to_owned(),
                fan_control: self.chip.fan_control(),
                pwm_misconfigured: self.chip.pwm_misconfigured(),
                fan_headers: self.headers().count(),
            })
            .chain(self.second.as_ref().map(SecondChip::info))
            .collect(),
        }
    }

    fn headers(&self) -> impl Iterator<Item = usize> + use<P> {
        wired_headers(&self.chip)
    }

    pub fn fan_limits(&self) -> BTreeMap<FanId, FanLimits> {
        let second = self.second.iter().flat_map(|second| {
            wired_headers(&second.chip).map(|channel| FanId::for_chip_channel(SECOND_CHIP, channel))
        });
        self.headers()
            .map(FanId::for_channel)
            .chain(second)
            .map(|fan| (fan, FanLimits::default()))
            .collect()
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
        self.chip.can_release() && !self.snapshot.owned.is_empty()
    }

    fn persist_snapshot(&self) -> Result<(), HwError> {
        self.snapshot
            .save(&self.snapshot_path)
            .map_err(HwError::from)
    }

    pub fn initialize_channels(&mut self, take_control: bool) -> Result<(), HwError> {
        if !self.chip.can_release() {
            return Ok(());
        }
        let states = self.chip.channel_states()?;
        let mut changed = self.snapshot.refresh(&states);
        let result = if take_control {
            let result = self.release_channels(false);
            changed = true;
            result
        } else {
            Ok(())
        };
        if result.is_ok() && self.snapshot.lost {
            self.snapshot.lost = false;
            changed = true;
        }
        if changed && let Err(error) = self.persist_snapshot() {
            tracing::error!(%error, "failed to persist BIOS snapshot");
        }
        result
    }

    pub fn reinitialize(&mut self) -> Result<(), HwError> {
        let primary = self.chip.reinitialize();
        if let Some(second) = &mut self.second
            && let Err(error) = second.chip.reinitialize()
        {
            tracing::warn!(%error, "the second fan chip could not be set up again");
        }
        primary
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
        let wiring = self.chip.wiring();
        let mut fans: Vec<FanReading> = chip
            .channels
            .iter()
            .enumerate()
            .filter(|&(index, _)| wiring.is_wired(index))
            .map(|(index, state)| {
                let mode = if state.is_auto() {
                    HardwareMode::Auto
                } else {
                    HardwareMode::Manual
                };
                FanReading {
                    id: FanId::for_channel(index),
                    label: default_fan_name(index),
                    rpm: chip.rpm[index].filter(|_| wiring.has_tach(index)),
                    mode,
                    duty: (mode == HardwareMode::Manual).then(|| Duty::from_raw(state.duty)),
                    unavailable: (!wiring.has_pwm(index)).then(|| NO_CONTROL_PIN.to_owned()),
                    monitor_only: false,
                }
            })
            .collect();
        if let Some(second) = &mut self.second {
            second.read_into(&mut sensors, &mut fans);
        }
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
            return vec![sensor(CPU_SENSOR, "CPU", None)];
        };
        temps
            .into_iter()
            .map(|reading| sensor(&reading.id, &reading.label, Some(reading.celsius)))
            .collect()
    }

    pub fn apply(&mut self, command: &Command, halted: &AtomicBool) -> Result<(), HwError> {
        if let Some(second) = &self.second
            && command.fan.header_channel().is_some_and(|(chip, channel)| {
                chip == SECOND_CHIP && channel < second.chip.fan_count()
            })
        {
            return match command.action {
                Action::SetDuty(_) => Err(HwError::NoFanControl(second.chip.chip().name)),
                Action::Release => Ok(()),
            };
        }
        let channel = channel_index(&command.fan, self.chip.fan_count())
            .ok_or_else(|| HwError::UnknownFan(command.fan.to_string()))?;
        match command.action {
            Action::SetDuty(_) if !self.fan_control() => {
                Err(HwError::NoFanControl(self.chip.chip().name))
            }
            Action::SetDuty(_) if !self.chip.wiring().has_pwm(channel) => {
                Err(HwError::NoControlPin(channel))
            }
            Action::Release if !self.chip.can_release() || !self.resettable(channel, false) => {
                Ok(())
            }
            Action::SetDuty(duty) => {
                if halted.load(Ordering::SeqCst) {
                    return Err(HwError::Halted);
                }
                if !self.snapshot.is_owned(channel) {
                    let current = self.chip.channel_states()?[channel];
                    let chip = self.chip.chip();
                    if !chip.forces_auto_without_snapshot()
                        && !current.is_auto()
                        && chip.manual_control() != Some(current.control)
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

    fn release_channels(&mut self, owned_only: bool) -> Result<(), HwError> {
        let mut first_error = None;
        for channel in 0..self.chip.fan_count() {
            if !self.resettable(channel, owned_only) {
                continue;
            }
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

    fn resettable(&self, channel: usize, owned_only: bool) -> bool {
        self.snapshot.is_owned(channel)
            || (!owned_only
                && self.chip.fan_control()
                && (self.snapshot.lost || self.chip.chip().forces_auto_without_snapshot()))
    }

    pub fn release_all(&mut self) -> Result<(), HwError> {
        self.release(false)
    }

    pub fn release_owned(&mut self) -> Result<(), HwError> {
        self.release(true)
    }

    fn release(&mut self, owned_only: bool) -> Result<(), HwError> {
        if let Some(owner) = self.foreign_owner.filter(|_| !self.chip.can_release()) {
            return Err(HwError::ForeignSnapshot {
                owner,
                found: self.chip.config().chip_id,
            });
        }
        if !self.chip.can_release() {
            return Ok(());
        }
        let result = self.release_channels(owned_only);
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
        hardware_for(chip, path)
    }

    fn monitor_only(
        tag: &str,
        fake: &SharedFake,
        path: &Path,
        spec: &'static crate::chips::ChipSpec,
    ) -> Hardware<SharedFake> {
        let mut chip = SuperIo::probe(fake.clone(), Slot::Primary, lock(tag)).unwrap();
        chip.config.chip = spec;
        hardware_for(chip, path)
    }

    fn hardware_for(chip: SuperIo<SharedFake>, path: &Path) -> Hardware<SharedFake> {
        Hardware::from_parts(
            chip,
            Err("no CPU in tests".to_owned()),
            BoardIdentity::default(),
            path,
        )
        .unwrap()
    }

    fn with_second_chip(tag: &str, path: &Path) -> (Hardware<SharedFake>, SharedFake, SharedFake) {
        let primary = SharedFake::new(FakeIte::chip(0x8689));
        let second = SharedFake::new(FakeIte::second(0x8733));
        let found = detect_second(
            &SuperIo::probe(primary.clone(), Slot::Primary, lock(&format!("{tag}-a")))
                .unwrap()
                .config(),
            |_| Ok(second.clone()),
            || Ok(lock(&format!("{tag}-b"))),
            true,
        );
        assert!(found.is_some());
        let hardware = hardware(tag, &primary, path).with_second(found);
        (hardware, primary, second)
    }

    fn data_writes(fake: &SharedFake) -> usize {
        fake.with(|chip| {
            chip.writes
                .iter()
                .filter(|&&(port, ..)| port == chip.base + 6)
                .count()
        })
    }

    #[test]
    fn a_second_chip_adds_monitor_only_fans_and_temperatures_without_renaming_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let (mut hardware, _, second) = with_second_chip("two-chips", &dir.path().join("s.toml"));
        let readings = hardware.read().unwrap();
        let ids: Vec<&str> = readings.fans.iter().map(|fan| fan.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "pwm1",
                "pwm2",
                "pwm3",
                "pwm4",
                "pwm5",
                "pwm6",
                "sio2/pwm1",
                "sio2/pwm2",
                "sio2/pwm3"
            ]
        );
        let extra = &readings.fans[6];
        assert_eq!(extra.label, "Fan 1 (IT8792E)");
        assert!(extra.monitor_only);
        assert_eq!(extra.rpm, Some(1136));
        assert_eq!(extra.duty, None);
        assert!(readings.fans[..6].iter().all(|fan| !fan.monitor_only));
        let board2: Vec<(&str, &str)> = readings
            .sensors
            .iter()
            .filter(|sensor| sensor.id.as_str().starts_with("board2/"))
            .map(|sensor| (sensor.id.as_str(), sensor.label.as_str()))
            .collect();
        assert_eq!(board2[0], ("board2/temp1", "Board temp 1 (IT8792E)"));
        assert_eq!(board2.len(), 3);
        assert!(
            readings
                .sensors
                .iter()
                .any(|sensor| sensor.id.as_str() == "board/temp1")
        );

        let info = hardware.board_info();
        assert_eq!(info.chip_name, "ITE IT8689E");
        assert_eq!(info.fan_headers, 6);
        assert_eq!(info.chips.len(), 2);
        assert_eq!(info.chips[1].chip_name, "ITE IT8792E");
        assert!(!info.chips[1].fan_control);
        assert!(hardware.fan_control());
        assert!(
            hardware
                .fan_limits()
                .contains_key(&FanId::from("sio2/pwm2"))
        );

        let before = data_writes(&second);
        assert!(matches!(
            hardware.apply(&set("sio2/pwm1", 60.0), &RUNNING),
            Err(HwError::NoFanControl("ITE IT8792E"))
        ));
        hardware.apply(&release("sio2/pwm1"), &RUNNING).unwrap();
        assert!(matches!(
            hardware.apply(&set("sio2/pwm4", 60.0), &RUNNING),
            Err(HwError::UnknownFan(_))
        ));
        hardware.initialize_channels(true).unwrap();
        hardware.apply(&set("pwm1", 60.0), &RUNNING).unwrap();
        hardware.release_all().unwrap();
        assert_eq!(data_writes(&second), before);
        assert!(
            !hardware
                .snapshot
                .owned
                .iter()
                .any(|fan| fan.as_str().contains('/'))
        );
    }

    #[test]
    fn a_failing_second_chip_never_fails_the_first_and_backs_off() {
        let dir = tempfile::tempdir().unwrap();
        let (mut hardware, _, second) = with_second_chip("two-fail", &dir.path().join("s.toml"));
        second.with(|chip| chip.offline = true);
        for _ in 0..3 {
            let readings = hardware.read().unwrap();
            let extra = readings
                .fans
                .iter()
                .find(|fan| fan.id.as_str() == "sio2/pwm1")
                .unwrap();
            assert!(extra.monitor_only);
            assert_eq!(extra.rpm, None);
            assert!(
                readings
                    .sensors
                    .iter()
                    .filter(|sensor| sensor.id.as_str().starts_with("board2/"))
                    .all(|sensor| sensor.celsius.is_none())
            );
            assert_eq!(readings.fans[0].rpm, Some(1136));
        }
        second.with(|chip| chip.offline = false);
        let writes = second.with(|chip| chip.writes.len());
        let readings = hardware.read().unwrap();
        assert_eq!(second.with(|chip| chip.writes.len()), writes);
        assert_eq!(readings.fans.len(), 9);
        hardware.apply(&set("pwm2", 50.0), &RUNNING).unwrap();
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
    fn owned_fans_are_still_released_after_the_pwm_check_starts_failing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.toml");
        let fake = SharedFake::new(FakeIte::chip(0x8628));
        let mut hardware = hardware("polarity-release", &fake, &path);
        hardware.apply(&set("pwm2", 60.0), &RUNNING).unwrap();
        fake.with(|chip| chip.banks[0][0x14] = 0x00);
        hardware.reinitialize().unwrap();
        assert!(!hardware.fan_control());
        assert!(hardware.owns_any());
        assert!(matches!(
            hardware.apply(&set("pwm2", 70.0), &RUNNING),
            Err(HwError::NoFanControl(_))
        ));
        hardware.release_all().unwrap();
        assert_eq!(fake.with(|chip| chip.reg(0x16)) & 0x80, 0x80);
        assert!(!hardware.owns_any());
    }

    #[test]
    fn an_unreadable_snapshot_sends_a_nuvoton_header_left_in_manual_to_full_speed() {
        use crate::nuvoton::fake::{FakeNct, SharedNct};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.toml");
        std::fs::write(&path, "not toml [").unwrap();
        let mut nct = FakeNct::new(0xC5, 0x62);
        nct.set(0x102, 0x00);
        nct.set(0x109, 0x30);
        let fake = SharedNct::new(nct);
        let chip = SuperIo::probe(fake.clone(), Slot::Primary, lock("nct-lost")).unwrap();
        let mut hardware = Hardware::from_parts(
            chip,
            Err("no CPU in tests".to_owned()),
            BoardIdentity::default(),
            &path,
        )
        .unwrap();
        hardware.initialize_channels(true).unwrap();
        assert_eq!(fake.with(|chip| chip.get(0x109)), 0xFF);
        fake.with(|chip| chip.set(0x109, 0x30));
        hardware.release_all().unwrap();
        assert_eq!(fake.with(|chip| chip.get(0x109)), 0x30);

        let mut chip =
            SuperIo::probe(fake.clone(), Slot::Primary, lock("nct-lost-failsafe")).unwrap();
        force_all_auto(&mut chip, &BiosSnapshot::lost(), RELEASE_BUDGET).unwrap();
        assert_eq!(fake.with(|chip| chip.get(0x109)), 0xFF);
    }

    #[test]
    fn a_lost_snapshot_never_touches_another_tools_headers() {
        use crate::nuvoton::fake::{FakeNct, SharedNct};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.toml");
        std::fs::write(&path, "not toml [").unwrap();
        let mut nct = FakeNct::new(0xC5, 0x62);
        nct.set(0x102, 0x00);
        nct.set(0x109, 0x30);
        let fake = SharedNct::new(nct);
        let chip = SuperIo::probe(fake.clone(), Slot::Primary, lock("nct-lost-tool")).unwrap();
        let mut hardware = Hardware::from_parts(
            chip,
            Err("no CPU in tests".to_owned()),
            BoardIdentity::default(),
            &path,
        )
        .unwrap();
        hardware.initialize_channels(false).unwrap();
        hardware.release_owned().unwrap();
        hardware.release_all().unwrap();
        assert_eq!(fake.with(|chip| chip.get(0x109)), 0x30);
    }

    #[test]
    fn a_chip_failing_the_pwm_check_from_the_start_is_never_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.toml");
        let mut ite = FakeIte::chip(0x8628);
        ite.banks[0][0x14] = 0x00;
        ite.banks[0][0x16] = 0x00;
        let fake = SharedFake::new(ite);
        let mut hardware = hardware("polarity-start", &fake, &path);
        assert!(!hardware.fan_control());
        hardware.initialize_channels(true).unwrap();
        hardware.apply(&release("pwm2"), &RUNNING).unwrap();
        hardware.release_all().unwrap();
        let snapshot = hardware.snapshot.clone();
        force_all_auto(&mut hardware.chip, &snapshot, RELEASE_BUDGET).unwrap();
        fake.with(|chip| {
            assert_eq!(chip.reg(0x16), 0x00);
            assert!(
                chip.writes
                    .iter()
                    .all(|&(port, _, index)| port != chip.base + 6 || index == 0x06)
            );
        });
    }

    #[test]
    fn releasing_owned_fans_leaves_the_others_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.toml");
        let fake = SharedFake::new(FakeIte::new());
        let mut hardware = hardware("owned-only", &fake, &path);
        hardware.initialize_channels(false).unwrap();
        hardware.apply(&set("pwm1", 60.0), &RUNNING).unwrap();
        fake.with(|chip| {
            chip.banks[0][0x16] = 0x01;
            chip.banks[0][0x6B] = 0x99;
        });
        hardware.release_owned().unwrap();
        fake.with(|chip| {
            assert_eq!((chip.reg(0x15), chip.reg(0x63)), (0x80, 0x40));
            assert_eq!((chip.reg(0x16), chip.reg(0x6B)), (0x01, 0x99));
        });
        assert!(!hardware.owns_any());
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
    fn an_it8689e_header_left_at_its_manual_value_can_still_be_driven() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.toml");
        let mut ite = FakeIte::chip(0x8689);
        ite.banks[0][0x16] = 0x7F;
        let fake = SharedFake::new(ite);
        let mut hardware = hardware("8689-leftover", &fake, &path);
        hardware.initialize_channels(true).unwrap();
        assert_eq!(fake.with(|chip| chip.reg(0x6B)), 0x40);
        hardware.apply(&set("pwm2", 60.0), &RUNNING).unwrap();
        assert!(hardware.owns_any());
        assert_eq!(fake.with(|chip| chip.reg(0x6B)), Duty::new(60.0).to_raw());
        hardware.apply(&release("pwm2"), &RUNNING).unwrap();
        assert_eq!(
            fake.with(|chip| (chip.reg(0x16), chip.reg(0x6B))),
            (0x7F, 0xFF)
        );
    }

    #[test]
    fn headers_the_board_wired_to_something_else_are_hidden_or_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.toml");
        let mut ite = FakeIte::chip(0x8689);
        ite.pins[0x29] = 0x02;
        ite.pins[0x27] |= 0x80;
        ite.pins[0x26] = 0x20;
        let fake = SharedFake::new(ite);
        let mut hardware = hardware("8689-pins", &fake, &path);
        let info = hardware.board_info();
        assert_eq!(info.fan_headers, 5);
        assert_eq!(info.likely_cpu_fan, Some(FanId::from("pwm1")));
        assert_eq!(
            hardware
                .fan_limits()
                .keys()
                .map(FanId::as_str)
                .collect::<Vec<_>>(),
            ["pwm1", "pwm2", "pwm3", "pwm4", "pwm5"]
        );

        hardware.initialize_channels(true).unwrap();
        let readings = hardware.read().unwrap();
        let ids: Vec<&str> = readings.fans.iter().map(|fan| fan.id.as_str()).collect();
        assert_eq!(ids, ["pwm1", "pwm2", "pwm3", "pwm4", "pwm5"]);
        assert_eq!(
            readings.fans[1].unavailable.as_deref(),
            Some(NO_CONTROL_PIN)
        );
        assert_eq!(readings.fans[1].rpm, Some(1136));
        assert_eq!(readings.fans[2].rpm, None);
        assert!(readings.fans[2].unavailable.is_none());
        assert_eq!(readings.fans[4].rpm, None);

        let before = fake.with(|chip| chip.writes.len());
        assert!(matches!(
            hardware.apply(&set("pwm2", 60.0), &RUNNING),
            Err(HwError::NoControlPin(1))
        ));
        assert_eq!(fake.with(|chip| chip.writes.len()), before);
        assert!(!hardware.owns_any());
        hardware.apply(&set("pwm3", 60.0), &RUNNING).unwrap();
    }

    #[test]
    fn the_it8689e_drops_pwm6_when_the_ec_has_it_turned_off() {
        let dir = tempfile::tempdir().unwrap();
        let mut ite = FakeIte::chip(0x8689);
        ite.banks[0][0x0B] = 0x00;
        let fake = SharedFake::new(ite);
        let it8689e = hardware("8689-pwm6", &fake, &dir.path().join("snapshot.toml"));
        assert_eq!(it8689e.board_info().fan_headers, 6);
        assert!(it8689e.chip.wiring().has_tach(5));
        assert!(!it8689e.chip.wiring().has_pwm(5));

        let mut ite = FakeIte::chip(0x8628);
        ite.banks[0][0x0B] = 0x00;
        let fake = SharedFake::new(ite);
        let it8628e = hardware("8628-pwm6", &fake, &dir.path().join("other.toml"));
        assert!(it8628e.chip.wiring().has_pwm(5));
    }

    #[test]
    fn unverified_chips_are_monitored_without_touching_fans_or_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snapshot.toml");
        let fake = SharedFake::new(FakeIte::chip(0x8686));
        let mut hardware =
            monitor_only("unverified", &fake, &path, &crate::chips::MONITOR_ONLY_8686);
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
            Err(HwError::NoFanControl(_))
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
        let mut hardware = monitor_only("leftover", &fake, &path, &crate::chips::MONITOR_ONLY_8628);
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
        chip.config.chip = &crate::chips::MONITOR_ONLY_8628;
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
        fan_control: true,
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
            pinmux: crate::pinmux::NctMux::Nct6779,
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
        let bios = fake.with(|chip| chip.get(0x109));
        hardware.apply(&release("pwm1"), &RUNNING).unwrap();
        assert_eq!(
            fake.with(|chip| (chip.get(0x102), chip.get(0x109))),
            (0x00, bios)
        );
        hardware.apply(&set("pwm2", 60.0), &RUNNING).unwrap();
        assert!(hardware.owns_any());
        assert_eq!(fake.with(|chip| chip.get(0x202)), 0x00);
        hardware.apply(&release("pwm2"), &RUNNING).unwrap();
        assert_eq!(
            fake.with(|chip| (chip.get(0x202), chip.get(0x209))),
            (0x50, 0x80)
        );
        let bios_manual = fake.with(|chip| chip.get(0x109));
        hardware.initialize_channels(true).unwrap();
        hardware.release_all().unwrap();
        let snapshot = hardware.snapshot.clone();
        force_all_auto(&mut hardware.chip, &snapshot, RELEASE_BUDGET).unwrap();
        assert_eq!(
            fake.with(|chip| (chip.get(0x102), chip.get(0x109))),
            (0x00, bios_manual)
        );
    }
}
