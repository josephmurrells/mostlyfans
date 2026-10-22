#[cfg(any(test, feature = "test-support"))]
pub mod fake;
pub mod nvidia;
pub mod ownership;
mod release;
mod worker;

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use fanforge_core::FanId;

pub use release::GpuReleaser;
pub use worker::{GpuFans, GpuReadings, NOT_RESPONDING};

pub const FOREIGN: &str = "controlled by another program";

const BRANDS: [&str; 4] = ["NVIDIA", "GeForce", "AMD", "Radeon"];

static WRITES: Mutex<()> = Mutex::new(());

fn writes() -> MutexGuard<'static, ()> {
    WRITES.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuDesc {
    pub id: String,
    pub key: String,
    pub label: String,
    pub fans: u32,
    pub range: Option<(u32, u32)>,
}

impl GpuDesc {
    pub fn fan_id(&self, fan: u32) -> FanId {
        FanId::for_gpu_fan(&self.id, fan)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FanSample {
    pub percent: Option<u32>,
    pub target: Option<u32>,
    pub rpm: Option<u32>,
    pub manual: Option<bool>,
}

pub trait FanDriver: Send {
    fn gpus(&self) -> Vec<GpuDesc>;
    fn sample(&self, gpu: &GpuDesc, fan: u32) -> Result<FanSample, String>;
    fn set_speed(&self, gpu: &GpuDesc, fan: u32, percent: u32) -> Result<(), String>;
    fn restore(&self, gpu: &GpuDesc, fan: u32) -> Result<(), String>;
}

pub type DriverOpener = Arc<dyn Fn() -> Result<Box<dyn FanDriver>, String> + Send + Sync>;

pub fn short_name(name: &str) -> String {
    let mut rest = name.trim();
    while let Some(stripped) = BRANDS.iter().find_map(|brand| {
        rest.strip_prefix(brand)
            .and_then(|tail| tail.strip_prefix(' '))
            .map(str::trim_start)
            .filter(|tail| !tail.is_empty())
    }) {
        rest = stripped;
    }
    rest.to_owned()
}

pub fn locate<'a>(gpus: &'a [GpuDesc], fan: &FanId) -> Option<(&'a GpuDesc, u32)> {
    gpus.iter().find_map(|gpu| {
        (0..gpu.fans)
            .find(|&index| &gpu.fan_id(index) == fan)
            .map(|index| (gpu, index))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brand_prefixes_are_dropped_from_gpu_names() {
        assert_eq!(
            short_name("NVIDIA GeForce RTX 4070 SUPER"),
            "RTX 4070 SUPER"
        );
        assert_eq!(short_name("NVIDIA RTX A4000"), "RTX A4000");
        assert_eq!(short_name("AMD Radeon RX 7900 XTX"), "RX 7900 XTX");
        assert_eq!(short_name("Quadro RTX 4000"), "Quadro RTX 4000");
        assert_eq!(short_name("NVIDIA GeForce"), "GeForce");
        assert_eq!(short_name("NVIDIAX 1"), "NVIDIAX 1");
    }

    #[test]
    fn fans_are_located_by_their_id() {
        let gpus = vec![GpuDesc {
            id: "gpu/nvidia/gpu-1".into(),
            key: "GPU-1".into(),
            label: "RTX".into(),
            fans: 2,
            range: Some((30, 100)),
        }];
        let (gpu, index) = locate(&gpus, &"gpu/nvidia/gpu-1/fan2".into()).unwrap();
        assert_eq!((gpu.key.as_str(), index), ("GPU-1", 1));
        assert!(locate(&gpus, &"gpu/nvidia/gpu-1/fan3".into()).is_none());
        assert!(locate(&gpus, &"pwm1".into()).is_none());
    }
}
