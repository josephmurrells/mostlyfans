use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::curve::{CurveDef, CurveKind, find_curve};
use crate::telemetry::{MAX_PLAUSIBLE_CELSIUS, MIN_PLAUSIBLE_CELSIUS};
use crate::{CurveId, Duty, FanId, SensorId};

pub const MIN_INTERVAL_MS: u64 = 250;
pub const MAX_INTERVAL_MS: u64 = 5000;
pub const CONFIG_VERSION: u32 = 1;
pub const MAX_PROFILES: usize = 16;
pub const MAX_CURVES: usize = 64;
pub const MAX_CURVE_POINTS: usize = 32;
pub const MAX_MIX_INPUTS: usize = 16;
pub const MAX_FAN_ENTRIES: usize = 16;
pub const MAX_NAME_LEN: usize = 64;
pub const MAX_SERIALIZED_LEN: usize = 32 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum FanMode {
    #[default]
    Auto,
    Manual {
        duty: Duty,
    },
    Curve {
        curve: CurveId,
    },
    Sync {
        fan: FanId,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FanSettings {
    pub name: Option<String>,
    pub case_fan: bool,
    pub min_duty: Duty,
    pub allow_stop: bool,
    pub spin_up_duty: Duty,
}

impl Default for FanSettings {
    fn default() -> Self {
        Self {
            name: None,
            case_fan: false,
            min_duty: Duty::OFF,
            allow_stop: false,
            spin_up_duty: Duty::new(50.0),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub fans: BTreeMap<FanId, FanMode>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SafetyConfig {
    pub critical_sensor: SensorId,
    pub critical_c: f32,
    pub release_c: f32,
    pub release_hold_secs: f32,
    pub sensor_failure_ticks: u32,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            critical_sensor: SensorId::new("cpu/tctl"),
            critical_c: 85.0,
            release_c: 80.0,
            release_hold_secs: 10.0,
            sensor_failure_ticks: 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    pub interval_ms: u64,
    pub active_profile: String,
    pub profiles: BTreeMap<String, Profile>,
    pub fans: BTreeMap<FanId, FanSettings>,
    pub curves: Vec<CurveDef>,
    pub safety: SafetyConfig,
}

impl Default for Config {
    fn default() -> Self {
        let profiles = ["Silent", "Balanced", "Performance"]
            .into_iter()
            .map(|name| (name.to_owned(), Profile::default()))
            .collect();
        Self {
            version: CONFIG_VERSION,
            interval_ms: 1000,
            active_profile: "Balanced".to_owned(),
            profiles,
            fans: BTreeMap::new(),
            curves: Vec::new(),
            safety: SafetyConfig::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Error)]
pub enum ConfigError {
    #[error("unsupported config version {0}")]
    UnsupportedVersion(u32),
    #[error("interval {0} ms is outside {MIN_INTERVAL_MS}..={MAX_INTERVAL_MS} ms")]
    Interval(u64),
    #[error("active profile `{0}` does not exist")]
    MissingActiveProfile(String),
    #[error("profile name must not be empty")]
    EmptyProfileName,
    #[error("curve id `{0}` is used more than once")]
    DuplicateCurve(CurveId),
    #[error("curve `{0}` has an empty name or id")]
    UnnamedCurve(CurveId),
    #[error("curve `{0}` has no points or inputs")]
    EmptyCurve(CurveId),
    #[error("curve `{curve}` has an invalid point ({temp} °C, {duty}%)")]
    InvalidPoint {
        curve: CurveId,
        temp: f32,
        duty: f32,
    },
    #[error("curve `{curve}` references unknown curve `{target}`")]
    UnknownCurveReference { curve: CurveId, target: CurveId },
    #[error("curve `{0}` is part of a reference cycle")]
    CurveCycle(CurveId),
    #[error("curve `{0}` has invalid tuning values")]
    InvalidTuning(CurveId),
    #[error("fan `{fan}` in profile `{profile}` uses unknown curve `{curve}`")]
    UnknownFanCurve {
        profile: String,
        fan: FanId,
        curve: CurveId,
    },
    #[error("fan `{fan}` in profile `{profile}` has a sync cycle")]
    SyncCycle { profile: String, fan: FanId },
    #[error("critical temperature settings are invalid")]
    InvalidSafety,
    #[error("configuration is too large: {0}")]
    TooLarge(String),
}

impl Config {
    pub fn active_modes(&self) -> Option<&BTreeMap<FanId, FanMode>> {
        self.profiles
            .get(&self.active_profile)
            .map(|profile| &profile.fans)
    }

    pub fn mode(&self, fan: &FanId) -> FanMode {
        self.active_modes()
            .and_then(|modes| modes.get(fan))
            .cloned()
            .unwrap_or_default()
    }

    pub fn settings(&self, fan: &FanId) -> FanSettings {
        self.fans.get(fan).cloned().unwrap_or_default()
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.version != CONFIG_VERSION {
            return Err(ConfigError::UnsupportedVersion(self.version));
        }
        if !(MIN_INTERVAL_MS..=MAX_INTERVAL_MS).contains(&self.interval_ms) {
            return Err(ConfigError::Interval(self.interval_ms));
        }
        if !self.profiles.contains_key(&self.active_profile) {
            return Err(ConfigError::MissingActiveProfile(
                self.active_profile.clone(),
            ));
        }
        if self.profiles.keys().any(|name| name.trim().is_empty()) {
            return Err(ConfigError::EmptyProfileName);
        }
        self.validate_size()?;
        self.validate_safety()?;
        self.validate_curves()?;
        self.validate_profiles()
    }

    fn validate_size(&self) -> Result<(), ConfigError> {
        let too_large = |what: String| Err(ConfigError::TooLarge(what));
        if self.profiles.len() > MAX_PROFILES {
            return too_large(format!("more than {MAX_PROFILES} profiles"));
        }
        if self.curves.len() > MAX_CURVES {
            return too_large(format!("more than {MAX_CURVES} curves"));
        }
        if self.fans.len() > MAX_FAN_ENTRIES
            || self
                .profiles
                .values()
                .any(|profile| profile.fans.len() > MAX_FAN_ENTRIES)
        {
            return too_large(format!("more than {MAX_FAN_ENTRIES} fan entries"));
        }
        let long_name = self.profiles.keys().any(|name| name.len() > MAX_NAME_LEN)
            || self.curves.iter().any(|curve| {
                curve.name.len() > MAX_NAME_LEN || curve.id.as_str().len() > MAX_NAME_LEN
            })
            || self.fans.iter().any(|(id, settings)| {
                id.as_str().len() > MAX_NAME_LEN
                    || settings
                        .name
                        .as_ref()
                        .is_some_and(|name| name.len() > MAX_NAME_LEN)
            });
        if long_name {
            return too_large(format!("a name is longer than {MAX_NAME_LEN} bytes"));
        }
        for curve in &self.curves {
            let inputs = match &curve.kind {
                CurveKind::Graph { points, .. } => points.len() > MAX_CURVE_POINTS,
                CurveKind::Mix { curves, .. } => curves.len() > MAX_MIX_INPUTS,
                CurveKind::Linear { .. } | CurveKind::Flat { .. } => false,
            };
            if inputs {
                return too_large(format!(
                    "curve `{}` has too many points or inputs",
                    curve.id
                ));
            }
        }
        let serialized = serde_json::to_vec(self).map_or(usize::MAX, |bytes| bytes.len());
        if serialized > MAX_SERIALIZED_LEN {
            return too_large(format!("{serialized} bytes exceeds {MAX_SERIALIZED_LEN}"));
        }
        Ok(())
    }

    fn validate_safety(&self) -> Result<(), ConfigError> {
        let safety = &self.safety;
        let valid = !safety.critical_sensor.as_str().trim().is_empty()
            && (40.0..=110.0).contains(&safety.critical_c)
            && safety.release_c < safety.critical_c
            && safety.release_c >= MIN_PLAUSIBLE_CELSIUS
            && (0.0..=600.0).contains(&safety.release_hold_secs)
            && (1..=60).contains(&safety.sensor_failure_ticks);
        if valid {
            Ok(())
        } else {
            Err(ConfigError::InvalidSafety)
        }
    }

    fn validate_curves(&self) -> Result<(), ConfigError> {
        let mut seen = HashSet::new();
        for curve in &self.curves {
            if !seen.insert(&curve.id) {
                return Err(ConfigError::DuplicateCurve(curve.id.clone()));
            }
            if curve.id.as_str().trim().is_empty() || curve.name.trim().is_empty() {
                return Err(ConfigError::UnnamedCurve(curve.id.clone()));
            }
            let tuning = &curve.tuning;
            let tuning_valid = [
                tuning.hysteresis_c,
                tuning.response_secs,
                tuning.step_up_per_sec,
                tuning.step_down_per_sec,
            ]
            .iter()
            .all(|value| value.is_finite() && *value >= 0.0);
            if !tuning_valid {
                return Err(ConfigError::InvalidTuning(curve.id.clone()));
            }
            match &curve.kind {
                CurveKind::Graph { points, .. } => {
                    if points.is_empty() {
                        return Err(ConfigError::EmptyCurve(curve.id.clone()));
                    }
                    points
                        .iter()
                        .try_for_each(|point| check_point(&curve.id, point.temp, point.duty))?;
                }
                CurveKind::Linear { start, end, .. } => {
                    check_point(&curve.id, start.temp, start.duty)?;
                    check_point(&curve.id, end.temp, end.duty)?;
                }
                CurveKind::Flat { duty } => check_point(&curve.id, MIN_PLAUSIBLE_CELSIUS, *duty)?,
                CurveKind::Mix { curves, .. } => {
                    if curves.is_empty() {
                        return Err(ConfigError::EmptyCurve(curve.id.clone()));
                    }
                    if let Some(target) = curves
                        .iter()
                        .find(|target| find_curve(&self.curves, target).is_none())
                    {
                        return Err(ConfigError::UnknownCurveReference {
                            curve: curve.id.clone(),
                            target: target.clone(),
                        });
                    }
                }
            }
        }
        for curve in &self.curves {
            if self.curve_has_cycle(&curve.id, &mut Vec::new()) {
                return Err(ConfigError::CurveCycle(curve.id.clone()));
            }
        }
        Ok(())
    }

    fn curve_has_cycle<'a>(&'a self, id: &'a CurveId, path: &mut Vec<&'a CurveId>) -> bool {
        if path.contains(&id) {
            return true;
        }
        let Some(curve) = find_curve(&self.curves, id) else {
            return false;
        };
        path.push(id);
        let cycle = curve
            .kind
            .references()
            .iter()
            .any(|child| self.curve_has_cycle(child, path));
        path.pop();
        cycle
    }

    fn validate_profiles(&self) -> Result<(), ConfigError> {
        for (profile_name, profile) in &self.profiles {
            for (fan, mode) in &profile.fans {
                match mode {
                    FanMode::Curve { curve } if find_curve(&self.curves, curve).is_none() => {
                        return Err(ConfigError::UnknownFanCurve {
                            profile: profile_name.clone(),
                            fan: fan.clone(),
                            curve: curve.clone(),
                        });
                    }
                    FanMode::Sync { .. } if sync_has_cycle(profile, fan) => {
                        return Err(ConfigError::SyncCycle {
                            profile: profile_name.clone(),
                            fan: fan.clone(),
                        });
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }
}

fn check_point(curve: &CurveId, temp: f32, duty: f32) -> Result<(), ConfigError> {
    let valid = temp.is_finite()
        && duty.is_finite()
        && (MIN_PLAUSIBLE_CELSIUS..=MAX_PLAUSIBLE_CELSIUS).contains(&temp)
        && (0.0..=100.0).contains(&duty);
    if valid {
        Ok(())
    } else {
        Err(ConfigError::InvalidPoint {
            curve: curve.clone(),
            temp,
            duty,
        })
    }
}

fn sync_has_cycle(profile: &Profile, start: &FanId) -> bool {
    let mut visited = HashSet::from([start]);
    let mut current = start;
    while let Some(FanMode::Sync { fan }) = profile.fans.get(current) {
        if !visited.insert(fan) {
            return true;
        }
        current = fan;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curve::{CurvePoint, MixOp, Tuning};

    fn curve(id: &str, kind: CurveKind) -> CurveDef {
        CurveDef {
            id: id.into(),
            name: id.into(),
            kind,
            tuning: Tuning::default(),
        }
    }

    #[test]
    fn default_config_is_valid_and_round_trips_through_toml() {
        let mut config = Config::default();
        config.curves.push(curve(
            "cpu",
            CurveKind::Graph {
                sensor: "cpu/tctl".into(),
                points: vec![CurvePoint::new(40.0, 30.0), CurvePoint::new(80.0, 100.0)],
            },
        ));
        config.profiles.get_mut("Balanced").unwrap().fans.insert(
            "pwm1".into(),
            FanMode::Curve {
                curve: "cpu".into(),
            },
        );
        config.validate().unwrap();
        let text = toml::to_string_pretty(&config).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn rejects_curve_cycles_and_unknown_references() {
        let mut config = Config {
            curves: vec![
                curve(
                    "a",
                    CurveKind::Mix {
                        op: MixOp::Max,
                        curves: vec!["b".into()],
                    },
                ),
                curve(
                    "b",
                    CurveKind::Mix {
                        op: MixOp::Max,
                        curves: vec!["a".into()],
                    },
                ),
            ],
            ..Config::default()
        };
        assert!(matches!(config.validate(), Err(ConfigError::CurveCycle(_))));

        config.curves = vec![curve(
            "a",
            CurveKind::Mix {
                op: MixOp::Max,
                curves: vec!["x".into()],
            },
        )];
        assert!(matches!(
            config.validate(),
            Err(ConfigError::UnknownCurveReference { .. })
        ));
    }

    #[test]
    fn rejects_sync_cycles() {
        let mut config = Config::default();
        let fans = &mut config.profiles.get_mut("Balanced").unwrap().fans;
        fans.insert("pwm1".into(), FanMode::Sync { fan: "pwm2".into() });
        fans.insert("pwm2".into(), FanMode::Sync { fan: "pwm1".into() });
        assert!(matches!(
            config.validate(),
            Err(ConfigError::SyncCycle { .. })
        ));
    }

    #[test]
    fn rejects_invalid_points_and_safety() {
        let config = Config {
            curves: vec![curve("a", CurveKind::Flat { duty: 140.0 })],
            ..Config::default()
        };
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidPoint { .. })
        ));

        let mut config = Config::default();
        config.safety.release_c = 90.0;
        assert_eq!(config.validate(), Err(ConfigError::InvalidSafety));
    }

    #[test]
    fn oversized_configs_are_rejected() {
        let mut config = Config::default();
        for index in 0..=MAX_CURVES {
            config
                .curves
                .push(curve(&format!("c{index}"), CurveKind::Flat { duty: 50.0 }));
        }
        assert!(matches!(config.validate(), Err(ConfigError::TooLarge(_))));

        let mut config = Config::default();
        config
            .profiles
            .insert("x".repeat(MAX_NAME_LEN + 1), Profile::default());
        assert!(matches!(config.validate(), Err(ConfigError::TooLarge(_))));

        let mut config = Config::default();
        config.safety.critical_sensor = SensorId::new(" ");
        assert_eq!(config.validate(), Err(ConfigError::InvalidSafety));
    }

    #[test]
    fn missing_fields_use_defaults() {
        let config: Config =
            toml::from_str("active_profile = \"Balanced\"\n[profiles.Balanced]\n").unwrap();
        assert_eq!(config.interval_ms, 1000);
        config.validate().unwrap();
    }
}
