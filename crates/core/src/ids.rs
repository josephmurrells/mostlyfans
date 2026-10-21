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

impl SensorId {
    pub fn core(index: usize) -> Self {
        Self(format!("{CORE_SENSOR_PREFIX}{index}"))
    }

    pub fn is_display_only(&self) -> bool {
        self.0.starts_with(CORE_SENSOR_PREFIX)
    }
}

impl FanId {
    pub fn for_channel(index: usize) -> Self {
        Self(format!("pwm{}", index + 1))
    }
}
