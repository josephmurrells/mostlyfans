use std::time::{Duration, Instant};

use fanforge_core::SensorId;
use fanforge_core::telemetry::SensorReading;
use fanforge_win::storage::{BUS_TYPE_NVME, Drive, DriveDescriptor, MAX_PHYSICAL_DRIVES};

use super::SensorSource;

const CADENCE: Duration = Duration::from_secs(5);
const REDISCOVER_EVERY: Duration = Duration::from_secs(300);

#[derive(Debug)]
struct Tracked {
    serial: String,
    id: SensorId,
    label: String,
    drive: Option<Drive>,
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

fn placement(tracked: &[Tracked], serial: &str) -> Placement {
    match tracked.iter().position(|entry| entry.serial == serial) {
        Some(index) if tracked[index].drive.is_none() => Placement::Reattach(index),
        Some(_) => Placement::Ignore,
        None => Placement::Add,
    }
}

impl DriveSource {
    fn discover(&mut self) {
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
            let target = placement(&self.tracked, &serial);
            if target == Placement::Ignore
                || is_rotational(drive.incurs_seek_penalty().ok(), bus_type)
                || drive.temperature().is_err()
            {
                continue;
            }
            match target {
                Placement::Reattach(slot) => self.tracked[slot].drive = Some(drive),
                Placement::Add => {
                    tracing::info!(%id, model, "drive temperature sensor found");
                    let label = if model.is_empty() {
                        format!("Drive {serial}")
                    } else {
                        model
                    };
                    self.tracked.push(Tracked {
                        serial,
                        id,
                        label,
                        drive: Some(drive),
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
            .is_none_or(|last| now.duration_since(last) >= REDISCOVER_EVERY)
        {
            self.discover();
            self.discovered = Some(now);
        }
        let mut readings = Vec::new();
        for entry in &mut self.tracked {
            let Some(drive) = &entry.drive else {
                continue;
            };
            match drive.temperature() {
                Ok(celsius) => readings.push(SensorReading {
                    id: entry.id.clone(),
                    label: entry.label.clone(),
                    celsius: Some(f32::from(celsius)),
                }),
                Err(error) => {
                    tracing::warn!(id = %entry.id, %error, "drive temperature read failed");
                    entry.drive = None;
                }
            }
        }
        Ok(readings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracked(serial: &str, attached: bool) -> Tracked {
        Tracked {
            serial: serial.to_owned(),
            id: sensor_id(serial).unwrap(),
            label: serial.to_owned(),
            drive: attached.then(|| Drive::open(0)).and_then(Result::ok),
        }
    }

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
        let list = vec![tracked("SERIAL-A", false)];
        assert_eq!(placement(&list, "SERIAL-B"), Placement::Add);
        assert_eq!(placement(&list, "SERIAL-A"), Placement::Reattach(0));
    }

    #[test]
    fn attached_drives_are_not_attached_twice() {
        let mut list = vec![tracked("SERIAL-A", false)];
        list[0].drive = Drive::open(0).ok();
        if list[0].drive.is_some() {
            assert_eq!(placement(&list, "SERIAL-A"), Placement::Ignore);
        }
    }
}
