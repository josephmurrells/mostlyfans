use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{Duty, FanId, SensorId};

pub const SAFETY_FLOOR: Duty = Duty::new_const(30.0);
pub const CASE_FAN_FLOOR: Duty = Duty::new_const(20.0);

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FanLimits {
    pub floor: Duty,
    pub stop_permitted: bool,
    #[serde(default)]
    pub fixed: bool,
}

impl Default for FanLimits {
    fn default() -> Self {
        Self {
            floor: SAFETY_FLOOR,
            stop_permitted: false,
            fixed: false,
        }
    }
}

impl FanLimits {
    pub fn for_gpu(driver_minimum: Duty) -> Self {
        Self {
            floor: Duty::new(driver_minimum.percent().max(SAFETY_FLOOR.percent())),
            stop_permitted: false,
            fixed: true,
        }
    }

    pub fn for_header(self, case_fan: bool) -> Self {
        if case_fan && !self.fixed {
            Self {
                floor: CASE_FAN_FLOOR,
                stop_permitted: true,
                fixed: false,
            }
        } else {
            self
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardIdentity {
    pub manufacturer: String,
    pub product: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BoardInfo {
    pub identity: BoardIdentity,
    pub chip_id: u16,
    #[serde(default)]
    pub chip_name: String,
    #[serde(default)]
    pub fan_control: bool,
    #[serde(default)]
    pub pwm_misconfigured: bool,
    pub fan_headers: usize,
    pub likely_cpu_fan: Option<FanId>,
    #[serde(default)]
    pub chips: Vec<ChipInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChipInfo {
    pub chip_id: u16,
    pub chip_name: String,
    pub fan_control: bool,
    #[serde(default)]
    pub pwm_misconfigured: bool,
    pub fan_headers: usize,
    #[serde(default)]
    pub note: Option<String>,
}

pub fn chip_label(chip_name: &str) -> &str {
    chip_name.strip_prefix("ITE ").unwrap_or(chip_name)
}

pub fn fan_limits(headers: usize) -> BTreeMap<FanId, FanLimits> {
    (0..headers)
        .map(|index| (FanId::for_channel(index), FanLimits::default()))
        .collect()
}

pub fn default_fan_name(index: usize) -> String {
    format!("Fan {}", index + 1)
}

pub fn temp_label(index: usize) -> String {
    format!("Board temp {}", index + 1)
}

pub fn temp_sensor_id(index: usize) -> SensorId {
    SensorId::new(format!("board/temp{}", index + 1))
}

pub fn chip_temp_sensor_id(chip: usize, index: usize) -> SensorId {
    match chip {
        0 => temp_sensor_id(index),
        _ => SensorId::new(format!("board{}/temp{}", chip + 1, index + 1)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_header_gets_the_safety_floor() {
        let limits = fan_limits(3);
        assert_eq!(limits.len(), 3);
        assert!(
            limits
                .values()
                .all(|header| *header == FanLimits::default())
        );
        assert_eq!(FanLimits::default().floor, SAFETY_FLOOR);
        assert!(limits.contains_key(&FanId::for_channel(2)));
    }

    #[test]
    fn only_case_fans_may_drop_below_the_floor_or_stop() {
        let limits = FanLimits::default();
        assert_eq!(limits.for_header(false), limits);
        assert_eq!(
            limits.for_header(true),
            FanLimits {
                floor: CASE_FAN_FLOOR,
                stop_permitted: true,
                fixed: false,
            }
        );
    }

    #[test]
    fn gpu_fans_keep_the_driver_minimum_and_never_stop() {
        let gpu = FanLimits::for_gpu(Duty::new(30.0));
        assert_eq!(gpu.floor, SAFETY_FLOOR);
        assert_eq!(FanLimits::for_gpu(Duty::new(10.0)).floor, SAFETY_FLOOR);
        assert_eq!(FanLimits::for_gpu(Duty::new(45.0)).floor, Duty::new(45.0));
        assert_eq!(gpu.for_header(true), gpu);
        assert!(!gpu.for_header(true).stop_permitted);
    }
}
