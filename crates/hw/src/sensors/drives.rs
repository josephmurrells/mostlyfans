use std::io;
use std::time::{Duration, Instant};

use fanforge_core::SensorId;
use fanforge_core::telemetry::SensorReading;
use fanforge_win::storage::{BUS_TYPE_NVME, Drive, DriveDescriptor, MAX_PHYSICAL_DRIVES};

use super::SensorSource;

const CADENCE: Duration = Duration::from_secs(5);
const REDISCOVER_EVERY: Duration = Duration::from_secs(300);
const REDISCOVER_DETACHED_EVERY: Duration = Duration::from_secs(30);
const FAILURES_BEFORE_DETACH: u32 = 3;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExtraSensors {
    #[default]
    None,
    Descriptor,
    HealthLog,
}

#[derive(Debug)]
struct Tracked {
    serial: String,
    id: SensorId,
    label: String,
    drive: Option<Drive>,
    failures: u32,
    extras: ExtraSensors,
    nvme: bool,
}

pub fn extra_sensor_id(drive: &SensorId, sensor: u16) -> SensorId {
    SensorId::new(format!("{drive}/sensor{sensor}"))
}

pub fn extra_method(descriptor: &[(u16, i16)], health: Option<&[(u16, i16)]>) -> ExtraSensors {
    if descriptor.iter().any(|(index, _)| *index > 0) {
        ExtraSensors::Descriptor
    } else if health.is_some_and(|sensors| !sensors.is_empty()) {
        ExtraSensors::HealthLog
    } else {
        ExtraSensors::None
    }
}

fn probe_extras(drive: &Drive, bus_type: u32) -> ExtraSensors {
    let descriptor = drive.temperature_sensors().unwrap_or_default();
    let health = (bus_type == BUS_TYPE_NVME)
        .then(|| drive.nvme_health_sensors().ok())
        .flatten();
    extra_method(&descriptor, health.as_deref())
}

fn read_drive(entry: &Tracked, drive: &Drive) -> io::Result<(i16, Vec<SensorReading>)> {
    match entry.extras {
        ExtraSensors::Descriptor => {
            let sensors = drive.temperature_sensors()?;
            let composite = sensors
                .iter()
                .find(|(index, _)| *index == 0)
                .map(|(_, celsius)| *celsius)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "drive reports no temperature")
                })?;
            Ok((composite, extra_readings(entry, sensors)))
        }
        ExtraSensors::HealthLog => {
            let composite = drive.temperature()?;
            let sensors = drive
                .nvme_health_sensors()
                .map_err(|error| {
                    tracing::debug!(id = %entry.id, %error, "extra drive sensors unreadable");
                })
                .unwrap_or_default();
            Ok((composite, extra_readings(entry, sensors)))
        }
        ExtraSensors::None => Ok((drive.temperature()?, Vec::new())),
    }
}

fn extra_readings(entry: &Tracked, sensors: Vec<(u16, i16)>) -> Vec<SensorReading> {
    sensors
        .into_iter()
        .filter(|(index, _)| *index > 0)
        .map(|(index, celsius)| SensorReading {
            id: extra_sensor_id(&entry.id, index),
            label: format!("{} sensor {index}", entry.label),
            celsius: Some(f32::from(celsius)),
        })
        .collect()
}

#[derive(Debug, Default)]
pub struct DriveSource {
    tracked: Vec<Tracked>,
    discovered: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Placement {
    Ignore,
    Reattach(usize),
    Add,
}

pub fn sensor_id(serial: &str) -> Option<SensorId> {
    let cleaned: String = serial
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        .collect();
    (!cleaned.is_empty()).then(|| SensorId::new(format!("drive/{cleaned}")))
}

pub fn is_rotational(seek_penalty: Option<bool>, bus_type: u32) -> bool {
    match seek_penalty {
        Some(penalty) => penalty,
        None => bus_type != BUS_TYPE_NVME,
    }
}

pub fn placement<'a>(
    tracked: impl IntoIterator<Item = (&'a str, bool)>,
    serial: &str,
) -> Placement {
    match tracked
        .into_iter()
        .enumerate()
        .find(|(_, (known, _))| *known == serial)
    {
        Some((index, (_, false))) => Placement::Reattach(index),
        Some(_) => Placement::Ignore,
        None => Placement::Add,
    }
}

impl DriveSource {
    fn rediscover_every(&self) -> Duration {
        if self.tracked.iter().any(|entry| entry.drive.is_none()) {
            REDISCOVER_DETACHED_EVERY
        } else {
            REDISCOVER_EVERY
        }
    }

    fn discover(&mut self) {
        for entry in &mut self.tracked {
            if entry.extras == ExtraSensors::None
                && let Some(drive) = &entry.drive
            {
                let bus_type = if entry.nvme { BUS_TYPE_NVME } else { 0 };
                entry.extras = probe_extras(drive, bus_type);
            }
        }
        for index in 0..MAX_PHYSICAL_DRIVES {
            let Ok(drive) = Drive::open(index) else {
                continue;
            };
            let Ok(DriveDescriptor {
                model,
                serial,
                bus_type,
            }) = drive.describe()
            else {
                continue;
            };
            let Some(id) = sensor_id(&serial) else {
                continue;
            };
            let target = placement(
                self.tracked
                    .iter()
                    .map(|entry| (entry.serial.as_str(), entry.drive.is_some())),
                &serial,
            );
            if target == Placement::Ignore
                || is_rotational(drive.incurs_seek_penalty().ok(), bus_type)
                || drive.temperature().is_err()
            {
                continue;
            }
            match target {
                Placement::Reattach(slot) => {
                    tracing::info!(%id, "drive reattached");
                    self.tracked[slot].extras = probe_extras(&drive, bus_type);
                    self.tracked[slot].drive = Some(drive);
                    self.tracked[slot].failures = 0;
                }
                Placement::Add => {
                    tracing::info!(%id, model, "drive temperature sensor found");
                    let label = if model.is_empty() {
                        format!("Drive {serial}")
                    } else {
                        model
                    };
                    let extras = probe_extras(&drive, bus_type);
                    if extras != ExtraSensors::None {
                        tracing::info!(%id, ?extras, "drive has extra temperature sensors");
                    }
                    self.tracked.push(Tracked {
                        serial,
                        id,
                        label,
                        drive: Some(drive),
                        failures: 0,
                        extras,
                        nvme: bus_type == BUS_TYPE_NVME,
                    });
                }
                Placement::Ignore => {}
            }
        }
    }
}

impl SensorSource for DriveSource {
    fn name(&self) -> &'static str {
        "drives"
    }

    fn cadence(&self) -> Duration {
        CADENCE
    }

    fn read(&mut self) -> Result<Vec<SensorReading>, String> {
        let now = Instant::now();
        if self
            .discovered
            .is_none_or(|last| now.duration_since(last) >= self.rediscover_every())
        {
            self.discover();
            self.discovered = Some(now);
        }
        let mut readings = Vec::new();
        for entry in &mut self.tracked {
            let Some(drive) = &entry.drive else {
                continue;
            };
            match read_drive(entry, drive) {
                Ok((celsius, extras)) => {
                    if entry.failures > 0 {
                        tracing::info!(id = %entry.id, "drive temperature recovered");
                    }
                    entry.failures = 0;
                    readings.push(SensorReading {
                        id: entry.id.clone(),
                        label: entry.label.clone(),
                        celsius: Some(f32::from(celsius)),
                    });
                    readings.extend(extras);
                }
                Err(error) => {
                    entry.failures += 1;
                    if entry.failures == 1 {
                        tracing::warn!(id = %entry.id, %error, "drive temperature read failed");
                    }
                    if entry.failures >= FAILURES_BEFORE_DETACH {
                        tracing::warn!(id = %entry.id, "drive keeps failing; reopening it later");
                        entry.drive = None;
                    }
                }
            }
        }
        Ok(readings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serials_become_stable_ids() {
        assert_eq!(
            sensor_id(" 6479_A73A_5332_338D. ").unwrap().as_str(),
            "drive/6479_A73A_5332_338D"
        );
        assert_eq!(
            sensor_id("S1DBNSADB47267Y").unwrap().as_str(),
            "drive/S1DBNSADB47267Y"
        );
        assert_eq!(sensor_id("  ..  "), None);
        assert_eq!(sensor_id(""), None);
    }

    #[test]
    fn unknown_drive_types_count_as_rotational_except_nvme() {
        assert!(is_rotational(Some(true), BUS_TYPE_NVME));
        assert!(!is_rotational(Some(false), 11));
        assert!(is_rotational(None, 11));
        assert!(!is_rotational(None, BUS_TYPE_NVME));
    }

    #[test]
    fn a_reused_index_never_attaches_to_another_drives_id() {
        let detached = [("SERIAL-A", false)];
        assert_eq!(placement(detached, "SERIAL-B"), Placement::Add);
        assert_eq!(placement(detached, "SERIAL-A"), Placement::Reattach(0));
    }

    #[test]
    fn extra_sensors_prefer_the_descriptor_then_the_health_log() {
        assert_eq!(extra_method(&[(0, 39)], None), ExtraSensors::None);
        assert_eq!(extra_method(&[(0, 39)], Some(&[])), ExtraSensors::None);
        assert_eq!(
            extra_method(&[(0, 39), (1, 45)], Some(&[(1, 45)])),
            ExtraSensors::Descriptor
        );
        assert_eq!(
            extra_method(&[(0, 39)], Some(&[(1, 45), (2, 41)])),
            ExtraSensors::HealthLog
        );
    }

    #[test]
    fn extra_sensors_hang_off_the_drive_id() {
        let drive = sensor_id("S1DB").unwrap();
        assert_eq!(extra_sensor_id(&drive, 2).as_str(), "drive/S1DB/sensor2");
    }

    #[test]
    fn attached_drives_are_not_attached_twice() {
        let list = [("SERIAL-A", false), ("SERIAL-B", true)];
        assert_eq!(placement(list, "SERIAL-B"), Placement::Ignore);
        assert_eq!(placement(list, "SERIAL-A"), Placement::Reattach(0));
    }
}
