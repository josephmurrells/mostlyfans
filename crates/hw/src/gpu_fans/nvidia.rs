use fanforge_win::SystemLibrary;
use nvml_wrapper::enums::device::FanControlPolicy;
use nvml_wrapper::{Device, Nvml};

use crate::sensors::nvidia::init;
use crate::sensors::unique_labels;

const NVML_SUCCESS: u32 = 0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NvidiaGpu {
    pub uuid: String,
    pub label: String,
    pub fans: u32,
    pub range: Option<(u32, u32)>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FanSample {
    pub percent: Option<u32>,
    pub target: Option<u32>,
    pub rpm: Option<u32>,
    pub manual: Option<bool>,
}

pub struct NvidiaFans {
    nvml: Nvml,
    gpus: Vec<NvidiaGpu>,
    _library: SystemLibrary,
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
            let fans = device.num_fans().unwrap_or(0);
            let range = device.min_max_fan_speed().ok();
            found.push((uuid, name, fans, range));
        }
        let labels = unique_labels(found.iter().map(|(_, name, ..)| name.clone()).collect());
        let gpus = found
            .into_iter()
            .zip(labels)
            .map(|((uuid, _, fans, range), label)| NvidiaGpu {
                uuid,
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

    pub fn gpus(&self) -> &[NvidiaGpu] {
        &self.gpus
    }

    fn device(&self, gpu: &NvidiaGpu) -> Result<Device<'_>, String> {
        self.nvml
            .device_by_uuid(gpu.uuid.as_str())
            .map_err(|error| error.to_string())
    }

    pub fn sample(&self, gpu: &NvidiaGpu, fan: u32) -> Result<FanSample, String> {
        let device = self.device(gpu)?;
        Ok(FanSample {
            percent: device.fan_speed(fan).ok(),
            target: target_speed(&device, fan),
            rpm: device.fan_speed_rpm(fan).ok(),
            manual: device
                .fan_control_policy(fan)
                .ok()
                .map(|policy| matches!(policy, FanControlPolicy::Manual)),
        })
    }

    pub fn set_speed(&self, gpu: &NvidiaGpu, fan: u32, percent: u32) -> Result<(), String> {
        self.device(gpu)?
            .set_fan_speed(fan, percent)
            .map_err(|error| error.to_string())
    }

    pub fn restore(&self, gpu: &NvidiaGpu, fan: u32) -> Result<(), String> {
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
