use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }
    };
}

id_type!(FanId);
id_type!(SensorId);
id_type!(CurveId);

pub const CORE_SENSOR_PREFIX: &str = "cpu/core";
pub const GPU_FAN_PREFIX: &str = "gpu/";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FanKind {
    Header,
    Gpu,
}

impl SensorId {
    pub fn core(index: usize) -> Self {
        Self(format!("{CORE_SENSOR_PREFIX}{index}"))
    }

    pub fn is_display_only(&self) -> bool {
        self.0
            .strip_prefix(CORE_SENSOR_PREFIX)
            .is_some_and(|index| {
                !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit())
            })
    }
}

impl FanId {
    pub fn for_channel(index: usize) -> Self {
        Self(format!("pwm{}", index + 1))
    }

    pub fn for_gpu_fan(gpu: &str, index: u32) -> Self {
        Self(format!("{gpu}/fan{}", index + 1))
    }

    pub fn kind(&self) -> FanKind {
        if self.0.starts_with(GPU_FAN_PREFIX) {
            FanKind::Gpu
        } else {
            FanKind::Header
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_numbered_core_ids_are_display_only() {
        assert!(SensorId::core(0).is_display_only());
        assert!(SensorId::core(15).is_display_only());
        assert!(!SensorId::new("cpu/core").is_display_only());
        assert!(!SensorId::new("cpu/core_voltage").is_display_only());
        assert!(!SensorId::new("cpu/package").is_display_only());
    }

    #[test]
    fn gpu_fans_are_recognised_by_their_id() {
        let fan = FanId::for_gpu_fan("gpu/nvidia/gpu-1234", 1);
        assert_eq!(fan.as_str(), "gpu/nvidia/gpu-1234/fan2");
        assert_eq!(fan.kind(), FanKind::Gpu);
        assert_eq!(FanId::for_channel(0).kind(), FanKind::Header);
        assert_eq!(FanId::from("gpufan").kind(), FanKind::Header);
    }
}
