use std::sync::Arc;

use mostlyfans_win::SystemLibrary;
use nvml_wrapper::enums::device::FanControlPolicy;
use nvml_wrapper::{Device, Nvml};

use super::{DriverOpener, FanDriver, FanSample, GpuDesc, short_name};
use crate::sensors::nvidia::{init, sensor_id};
use crate::sensors::unique_labels;

const NVML_SUCCESS: u32 = 0;

pub struct NvidiaFans {
    nvml: Nvml,
    gpus: Vec<GpuDesc>,
    _library: SystemLibrary,
}

pub fn driver_present() -> bool {
    SystemLibrary::load(crate::sensors::nvidia::LIBRARY).is_ok()
}

pub fn opener() -> DriverOpener {
    Arc::new(|| NvidiaFans::open().map(|fans| Box::new(fans) as Box<dyn FanDriver>))
}

impl NvidiaFans {
    pub fn open() -> Result<Self, String> {
        let (nvml, library) = init()?;
        let count = nvml.device_count().map_err(|error| error.to_string())?;
        let mut found = Vec::new();
        for index in 0..count {
            let identity = nvml
                .device_by_index(index)
                .and_then(|device| Ok((device.uuid()?, device)));
            let (uuid, device) = match identity {
                Ok(identity) => identity,
                Err(error) => {
                    tracing::warn!(index, %error, "skipping an NVIDIA GPU that NVML cannot identify");
                    continue;
                }
            };
            let name = device.name().unwrap_or_else(|_| "NVIDIA GPU".to_owned());
            let range = device.min_max_fan_speed().ok();
            let controllable = range.is_some() && device.fan_control_policy(0).is_ok();
            let fans = if controllable {
                device.num_fans().unwrap_or(0)
            } else {
                0
            };
            found.push((uuid, name, fans, range));
        }
        let labels = unique_labels(found.iter().map(|(_, name, ..)| short_name(name)).collect());
        let gpus = found
            .into_iter()
            .zip(labels)
            .map(|((uuid, _, fans, range), label)| GpuDesc {
                id: sensor_id(&uuid).to_string(),
                key: uuid,
                label,
                fans,
                range,
            })
            .collect();
        Ok(Self {
            nvml,
            gpus,
            _library: library,
        })
    }

    fn device(&self, gpu: &GpuDesc) -> Result<Device<'_>, String> {
        self.nvml
            .device_by_uuid(gpu.key.as_str())
            .map_err(|error| error.to_string())
    }
}

impl FanDriver for NvidiaFans {
    fn gpus(&self) -> Vec<GpuDesc> {
        self.gpus.clone()
    }

    fn sample(&self, gpu: &GpuDesc, fan: u32) -> Result<FanSample, String> {
        let device = self.device(gpu)?;
        let policy = device
            .fan_control_policy(fan)
            .map_err(|error| error.to_string())?;
        Ok(FanSample {
            percent: device.fan_speed(fan).ok(),
            target: target_speed(&device, fan),
            rpm: device.fan_speed_rpm(fan).ok(),
            manual: Some(matches!(policy, FanControlPolicy::Manual)),
        })
    }

    fn set_speed(&self, gpu: &GpuDesc, fan: u32, percent: u32) -> Result<(), String> {
        self.device(gpu)?
            .set_fan_speed(fan, percent)
            .map_err(|error| error.to_string())
    }

    fn restore(&self, gpu: &GpuDesc, fan: u32) -> Result<(), String> {
        self.device(gpu)?
            .set_default_fan_speed(fan)
            .map_err(|error| error.to_string())
    }
}

fn target_speed(device: &Device<'_>, fan: u32) -> Option<u32> {
    let function = device
        .nvml()
        .lib()
        .nvmlDeviceGetTargetFanSpeed
        .as_ref()
        .ok()?;
    let mut target = 0;
    let status = unsafe { function(device.handle(), fan, &raw mut target) };
    (status == NVML_SUCCESS).then_some(target)
}
