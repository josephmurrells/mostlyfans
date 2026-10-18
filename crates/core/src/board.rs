use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{Duty, FanId, SensorId};

pub const UNVERIFIED_FLOOR: Duty = Duty::new_const(30.0);

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FanLimits {
    pub floor: Duty,
    pub stop_permitted: bool,
}

impl Default for FanLimits {
    fn default() -> Self {
        Self {
            floor: UNVERIFIED_FLOOR,
            stop_permitted: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FanChannel {
    pub index: usize,
    pub name: String,
    pub min_duty: Duty,
    #[serde(default)]
    pub stop_permitted: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TempChannel {
    pub index: usize,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BoardProfile {
    pub name: String,
    pub manufacturer: String,
    pub products: Vec<String>,
    pub chip_id: u16,
    #[serde(default)]
    pub verified: bool,
    #[serde(default)]
    pub read_only: bool,
    pub fans: Vec<FanChannel>,
    pub temps: Vec<TempChannel>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardIdentity {
    pub manufacturer: String,
    pub product: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BoardInfo {
    pub identity: BoardIdentity,
    pub profile_name: String,
    pub chip_id: u16,
    pub verified: bool,
    pub read_only: bool,
}

impl BoardProfile {
    pub fn matches(&self, identity: &BoardIdentity, chip_id: u16) -> bool {
        self.chip_id == chip_id
            && identity
                .manufacturer
                .trim()
                .eq_ignore_ascii_case(self.manufacturer.trim())
            && self
                .products
                .iter()
                .any(|product| identity.product.trim().eq_ignore_ascii_case(product.trim()))
    }

    pub fn generic(chip_id: u16, fans: usize, temps: usize) -> Self {
        Self {
            name: format!("Generic ITE {chip_id:04X}"),
            manufacturer: String::new(),
            products: Vec::new(),
            chip_id,
            verified: false,
            read_only: true,
            fans: (0..fans)
                .map(|index| FanChannel {
                    index,
                    name: format!("Fan {}", index + 1),
                    min_duty: UNVERIFIED_FLOOR,
                    stop_permitted: false,
                })
                .collect(),
            temps: (0..temps)
                .map(|index| TempChannel {
                    index,
                    label: format!("Board temp {}", index + 1),
                })
                .collect(),
        }
    }

    pub fn fan_limits(&self) -> BTreeMap<FanId, FanLimits> {
        self.fans
            .iter()
            .map(|channel| {
                let limits = if self.verified {
                    FanLimits {
                        floor: channel.min_duty,
                        stop_permitted: channel.stop_permitted,
                    }
                } else {
                    FanLimits {
                        floor: Duty::new(
                            channel.min_duty.percent().max(UNVERIFIED_FLOOR.percent()),
                        ),
                        stop_permitted: false,
                    }
                };
                (FanId::for_channel(channel.index), limits)
            })
            .collect()
    }

    pub fn fan_name(&self, index: usize) -> String {
        self.fans
            .iter()
            .find(|channel| channel.index == index)
            .map_or_else(
                || format!("Fan {}", index + 1),
                |channel| channel.name.clone(),
            )
    }

    pub fn temp_label(&self, index: usize) -> String {
        self.temps
            .iter()
            .find(|channel| channel.index == index)
            .map_or_else(
                || format!("Board temp {}", index + 1),
                |channel| channel.label.clone(),
            )
    }

    pub fn temp_sensor_id(index: usize) -> SensorId {
        SensorId::new(format!("board/temp{}", index + 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(verified: bool) -> BoardProfile {
        BoardProfile {
            name: "Test".into(),
            manufacturer: "ASUSTeK COMPUTER INC.".into(),
            products: vec!["PRIME B350-PLUS".into()],
            chip_id: 0x8655,
            verified,
            read_only: false,
            fans: vec![FanChannel {
                index: 1,
                name: "CHA".into(),
                min_duty: Duty::new(10.0),
                stop_permitted: true,
            }],
            temps: Vec::new(),
        }
    }

    #[test]
    fn matching_requires_board_and_chip() {
        let identity = BoardIdentity {
            manufacturer: "asustek computer inc. ".into(),
            product: "PRIME B350-PLUS".into(),
        };
        assert!(profile(true).matches(&identity, 0x8655));
        assert!(!profile(true).matches(&identity, 0x8665));
        let other = BoardIdentity {
            product: "TUF B350M-PLUS".into(),
            ..identity
        };
        assert!(!profile(true).matches(&other, 0x8655));
    }

    #[test]
    fn unverified_profiles_raise_floors_and_forbid_stopping() {
        let verified = profile(true).fan_limits();
        let unverified = profile(false).fan_limits();
        let id = FanId::for_channel(1);
        assert_eq!(
            verified[&id],
            FanLimits {
                floor: Duty::new(10.0),
                stop_permitted: true
            }
        );
        assert_eq!(
            unverified[&id],
            FanLimits {
                floor: UNVERIFIED_FLOOR,
                stop_permitted: false
            }
        );
    }
}
