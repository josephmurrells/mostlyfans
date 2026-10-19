use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{Duty, FanId, SensorId};

pub const SAFETY_FLOOR: Duty = Duty::new_const(30.0);
pub const CASE_FAN_FLOOR: Duty = Duty::new_const(20.0);

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FanLimits {
    pub floor: Duty,
    pub stop_permitted: bool,
}

impl Default for FanLimits {
    fn default() -> Self {
        Self {
            floor: SAFETY_FLOOR,
            stop_permitted: false,
        }
    }
}

impl FanLimits {
    pub fn for_header(self, case_fan: bool) -> Self {
        if case_fan {
            Self {
                floor: CASE_FAN_FLOOR,
                stop_permitted: true,
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
    pub fan_headers: usize,
    pub likely_cpu_fan: Option<FanId>,
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
                stop_permitted: true
            }
        );
    }
}
