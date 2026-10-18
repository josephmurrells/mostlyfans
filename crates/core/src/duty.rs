use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(from = "f32", into = "f32")]
pub struct Duty(f32);

impl Duty {
    pub const OFF: Self = Self(0.0);
    pub const FULL: Self = Self(100.0);

    pub const fn new_const(percent: f32) -> Self {
        Self(percent.clamp(0.0, 100.0))
    }

    pub fn new(percent: f32) -> Self {
        if percent.is_nan() {
            Self::FULL
        } else {
            Self(percent.clamp(0.0, 100.0))
        }
    }

    pub fn percent(self) -> f32 {
        self.0
    }

    pub fn to_raw(self) -> u8 {
        (self.0 * 2.55).round() as u8
    }

    pub fn from_raw(raw: u8) -> Self {
        Self(f32::from(raw) / 2.55)
    }

    pub fn is_off(self) -> bool {
        self.to_raw() == 0
    }
}

impl From<f32> for Duty {
    fn from(percent: f32) -> Self {
        Self::new(percent)
    }
}

impl From<Duty> for f32 {
    fn from(duty: Duty) -> Self {
        duty.0
    }
}

impl fmt::Display for Duty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.0}%", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_and_rejects_nan() {
        assert_eq!(Duty::new(150.0), Duty::FULL);
        assert_eq!(Duty::new(-3.0), Duty::OFF);
        assert_eq!(Duty::new(f32::NAN), Duty::FULL);
    }

    #[test]
    fn raw_round_trip() {
        assert_eq!(Duty::FULL.to_raw(), 255);
        assert_eq!(Duty::OFF.to_raw(), 0);
        for raw in 0..=255u8 {
            assert_eq!(Duty::from_raw(raw).to_raw(), raw);
        }
    }

    #[test]
    fn deserialization_clamps() {
        let duty: Duty = serde_json::from_str("250.0").unwrap();
        assert_eq!(duty, Duty::FULL);
    }
}
