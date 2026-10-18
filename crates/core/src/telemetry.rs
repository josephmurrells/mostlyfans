use serde::{Deserialize, Serialize};

use crate::{Duty, FanId, SensorId};

pub const MIN_PLAUSIBLE_CELSIUS: f32 = 0.0;
pub const MAX_PLAUSIBLE_CELSIUS: f32 = 125.0;

pub fn is_plausible(celsius: f32) -> bool {
    celsius.is_finite() && (MIN_PLAUSIBLE_CELSIUS..=MAX_PLAUSIBLE_CELSIUS).contains(&celsius)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HardwareMode {
    Auto,
    Manual,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SensorReading {
    pub id: SensorId,
    pub label: String,
    pub celsius: Option<f32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FanReading {
    pub id: FanId,
    pub label: String,
    pub rpm: Option<u32>,
    pub mode: HardwareMode,
    pub duty: Option<Duty>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Readings {
    pub sensors: Vec<SensorReading>,
    pub fans: Vec<FanReading>,
}

impl Readings {
    pub fn temperature(&self, id: &SensorId) -> Option<f32> {
        self.sensors
            .iter()
            .find(|sensor| &sensor.id == id)
            .and_then(|sensor| sensor.celsius)
            .filter(|celsius| is_plausible(*celsius))
    }

    pub fn fan(&self, id: &FanId) -> Option<&FanReading> {
        self.fans.iter().find(|fan| &fan.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implausible_temperatures_are_hidden() {
        let readings = Readings {
            sensors: vec![
                SensorReading {
                    id: "a".into(),
                    label: "A".into(),
                    celsius: Some(-5.0),
                },
                SensorReading {
                    id: "b".into(),
                    label: "B".into(),
                    celsius: Some(130.0),
                },
                SensorReading {
                    id: "c".into(),
                    label: "C".into(),
                    celsius: Some(45.0),
                },
            ],
            fans: Vec::new(),
        };
        assert_eq!(readings.temperature(&"a".into()), None);
        assert_eq!(readings.temperature(&"b".into()), None);
        assert_eq!(readings.temperature(&"c".into()), Some(45.0));
        assert_eq!(readings.temperature(&"missing".into()), None);
    }
}
