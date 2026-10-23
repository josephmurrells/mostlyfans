use std::time::Duration;

use mostlyfans_core::SensorId;
use mostlyfans_core::telemetry::SensorReading;
use mostlyfans_win::SystemLibrary;
use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::TemperatureSensor;

use super::{SensorSource, unique_labels};
use crate::gpu_fans::short_name;

pub const LIBRARY: &str = "nvml.dll";
const CADENCE: Duration = Duration::from_secs(1);

#[derive(Debug)]
struct Gpu {
    uuid: String,
    id: SensorId,
    label: String,
}

struct Session {
    nvml: Nvml,
    gpus: Vec<Gpu>,
    _library: SystemLibrary,
}

#[derive(Default)]
pub struct NvidiaSource {
    session: Option<Session>,
}

pub fn sensor_id(uuid: &str) -> SensorId {
    SensorId::new(format!("gpu/nvidia/{}", uuid.trim().to_ascii_lowercase()))
}

pub fn init() -> Result<(Nvml, SystemLibrary), String> {
    let library = SystemLibrary::load(LIBRARY)
        .map_err(|error| format!("NVIDIA driver not found ({LIBRARY}: {error})"))?;
    let nvml = Nvml::builder()
        .lib_path(library.path().as_os_str())
        .init()
        .map_err(|error| format!("NVML could not start: {error}"))?;
    Ok((nvml, library))
}

fn open() -> Result<Session, String> {
    let (nvml, library) = init()?;
    let count = nvml.device_count().map_err(|error| error.to_string())?;
    let mut found = Vec::new();
    let mut last_error = None;
    for index in 0..count {
        let identity = nvml.device_by_index(index).and_then(|device| {
            let name = device.name().unwrap_or_else(|_| "NVIDIA GPU".to_owned());
            device.uuid().map(|uuid| (uuid, name))
        });
        match identity {
            Ok(identity) => found.push(identity),
            Err(error) => {
                tracing::warn!(index, %error, "skipping an NVIDIA GPU that NVML cannot identify");
                last_error = Some(error.to_string());
            }
        }
    }
    if let Some(error) = last_error.filter(|_| found.is_empty()) {
        return Err(error);
    }
    let (uuids, names): (Vec<String>, Vec<String>) = found.into_iter().unzip();
    let gpus = uuids
        .into_iter()
        .zip(unique_labels(
            names.iter().map(|name| short_name(name)).collect(),
        ))
        .map(|(uuid, label)| Gpu {
            id: sensor_id(&uuid),
            uuid,
            label,
        })
        .collect();
    Ok(Session {
        nvml,
        gpus,
        _library: library,
    })
}

impl Session {
    fn read(&self) -> Result<Vec<SensorReading>, String> {
        let mut readings = Vec::new();
        let mut last_error = None;
        for gpu in &self.gpus {
            let celsius = self
                .nvml
                .device_by_uuid(gpu.uuid.as_str())
                .and_then(|device| device.temperature(TemperatureSensor::Gpu));
            match celsius {
                Ok(celsius) => readings.push(SensorReading {
                    id: gpu.id.clone(),
                    label: gpu.label.clone(),
                    celsius: Some(celsius as f32),
                }),
                Err(error) => last_error = Some(error.to_string()),
            }
        }
        match last_error {
            Some(error) if readings.is_empty() => Err(error),
            _ => Ok(readings),
        }
    }
}

impl SensorSource for NvidiaSource {
    fn name(&self) -> &'static str {
        "nvidia"
    }

    fn cadence(&self) -> Duration {
        CADENCE
    }

    fn read(&mut self) -> Result<Vec<SensorReading>, String> {
        let session = match self.session.take() {
            Some(session) => session,
            None => open()?,
        };
        let result = session.read();
        if result.is_ok() {
            self.session = Some(session);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_derived_from_the_board_uuid() {
        assert_eq!(
            sensor_id(" GPU-1A2B3C4D-0000-1111-2222-333344445555 ").as_str(),
            "gpu/nvidia/gpu-1a2b3c4d-0000-1111-2222-333344445555"
        );
    }

    #[test]
    fn reading_without_a_driver_fails_cleanly_or_reports_gpus() {
        let mut source = NvidiaSource::default();
        match source.read() {
            Ok(readings) => assert!(
                readings
                    .iter()
                    .all(|reading| reading.id.as_str().starts_with("gpu/nvidia/"))
            ),
            Err(error) => assert!(!error.is_empty()),
        }
    }
}
