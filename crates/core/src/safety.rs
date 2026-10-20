use std::time::Duration;

use crate::config::SafetyConfig;

#[derive(Debug, Default)]
pub struct CriticalGuard {
    hot: bool,
    cool_for: Duration,
    lost: bool,
    lost_cool_for: Duration,
    missing_ticks: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    Override,
    SensorFallback,
}

impl Cause {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Override => "the critical temperature override",
            Self::SensorFallback => "the missing-sensor fallback",
        }
    }
}

impl CriticalGuard {
    pub fn is_active(&self, config: &SafetyConfig) -> bool {
        self.active_cause(config).is_some()
    }

    pub fn active_cause(&self, config: &SafetyConfig) -> Option<Cause> {
        if config.critical_enabled && self.hot {
            Some(Cause::Override)
        } else if config.sensor_fallback_enabled && self.lost {
            Some(Cause::SensorFallback)
        } else {
            None
        }
    }

    pub fn is_hot(&self) -> bool {
        self.hot
    }

    pub fn is_sensor_missing(&self) -> bool {
        self.missing_ticks > 0
    }

    pub fn update(
        &mut self,
        temperature: Option<f32>,
        config: &SafetyConfig,
        elapsed: Duration,
    ) -> bool {
        let Some(temp) = temperature else {
            self.missing_ticks = self.missing_ticks.saturating_add(1);
            self.cool_for = Duration::ZERO;
            self.lost_cool_for = Duration::ZERO;
            if self.missing_ticks >= config.sensor_failure_ticks {
                self.lost = true;
            }
            return self.is_active(config);
        };
        self.missing_ticks = 0;
        if temp >= config.critical_c {
            self.hot = true;
            self.cool_for = Duration::ZERO;
        } else {
            cool_down(&mut self.hot, &mut self.cool_for, temp, config, elapsed);
        }
        cool_down(
            &mut self.lost,
            &mut self.lost_cool_for,
            temp,
            config,
            elapsed,
        );
        self.is_active(config)
    }
}

fn cool_down(
    latched: &mut bool,
    cool_for: &mut Duration,
    temp: f32,
    config: &SafetyConfig,
    elapsed: Duration,
) {
    if *latched && temp < config.release_c {
        *cool_for += elapsed;
        if cool_for.as_secs_f32() >= config.release_hold_secs {
            *latched = false;
            *cool_for = Duration::ZERO;
        }
    } else {
        *cool_for = Duration::ZERO;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    #[test]
    fn engages_at_threshold_and_releases_after_hold() {
        let config = SafetyConfig::default();
        let mut guard = CriticalGuard::default();
        assert!(!guard.update(Some(84.0), &config, SECOND));
        assert!(guard.update(Some(85.0), &config, SECOND));
        assert!(guard.update(Some(82.0), &config, SECOND));
        for _ in 0..9 {
            assert!(guard.update(Some(70.0), &config, SECOND));
        }
        assert!(guard.update(Some(81.0), &config, SECOND));
        for _ in 0..9 {
            assert!(guard.update(Some(70.0), &config, SECOND));
        }
        assert!(!guard.update(Some(70.0), &config, SECOND));
    }

    #[test]
    fn a_missing_sensor_engages_after_the_failure_limit() {
        let config = SafetyConfig::default();
        let mut guard = CriticalGuard::default();
        assert!(!guard.update(None, &config, SECOND));
        assert!(!guard.update(None, &config, SECOND));
        assert!(guard.update(None, &config, SECOND));
        assert!(guard.is_sensor_missing());
        assert!(guard.update(Some(50.0), &config, SECOND));
        assert!(!guard.is_sensor_missing());
        for _ in 0..8 {
            assert!(guard.update(Some(50.0), &config, SECOND));
        }
        assert!(!guard.update(Some(50.0), &config, SECOND));
    }

    #[test]
    fn a_brief_dropout_does_not_reset_an_active_override() {
        let config = SafetyConfig::default();
        let mut guard = CriticalGuard::default();
        guard.update(Some(90.0), &config, SECOND);
        assert!(guard.update(None, &config, Duration::from_secs(30)));
    }

    #[test]
    fn with_the_override_off_heat_is_reported_but_not_forced() {
        let config = SafetyConfig {
            critical_enabled: false,
            ..SafetyConfig::default()
        };
        let mut guard = CriticalGuard::default();
        assert!(!guard.update(Some(100.0), &config, SECOND));
        assert!(guard.is_hot());
        for _ in 0..10 {
            guard.update(Some(70.0), &config, SECOND);
        }
        assert!(!guard.is_hot());
    }

    #[test]
    fn with_the_fallback_off_a_missing_sensor_is_reported_but_not_forced() {
        let config = SafetyConfig {
            sensor_fallback_enabled: false,
            ..SafetyConfig::default()
        };
        let mut guard = CriticalGuard::default();
        for _ in 0..5 {
            assert!(!guard.update(None, &config, SECOND));
        }
        assert!(guard.is_sensor_missing());
    }

    #[test]
    fn a_missing_sensor_latch_clears_when_it_returns_with_the_override_off() {
        let config = SafetyConfig {
            critical_enabled: false,
            ..SafetyConfig::default()
        };
        let mut guard = CriticalGuard::default();
        for _ in 0..3 {
            guard.update(None, &config, SECOND);
        }
        assert!(guard.is_active(&config));
        for _ in 0..9 {
            assert!(guard.update(Some(50.0), &config, SECOND));
        }
        assert!(!guard.update(Some(50.0), &config, SECOND));
    }

    #[test]
    fn with_the_fallback_off_a_brief_dropout_keeps_a_hot_override() {
        let config = SafetyConfig {
            sensor_fallback_enabled: false,
            ..SafetyConfig::default()
        };
        let mut guard = CriticalGuard::default();
        assert!(guard.update(Some(90.0), &config, SECOND));
        assert!(guard.update(None, &config, Duration::from_secs(30)));
        assert_eq!(guard.active_cause(&config), Some(Cause::Override));
    }

    #[test]
    fn with_both_causes_active_one_toggle_off_leaves_the_other_forcing() {
        let mut config = SafetyConfig::default();
        let mut guard = CriticalGuard::default();
        guard.update(Some(90.0), &config, SECOND);
        for _ in 0..3 {
            guard.update(None, &config, SECOND);
        }
        assert_eq!(guard.active_cause(&config), Some(Cause::Override));
        config.critical_enabled = false;
        assert_eq!(guard.active_cause(&config), Some(Cause::SensorFallback));
        config.critical_enabled = true;
        config.sensor_fallback_enabled = false;
        assert_eq!(guard.active_cause(&config), Some(Cause::Override));
    }

    #[test]
    fn turning_a_toggle_off_stops_forcing_without_touching_the_other_cause() {
        let mut config = SafetyConfig::default();
        let mut guard = CriticalGuard::default();
        assert!(guard.update(Some(90.0), &config, SECOND));
        config.critical_enabled = false;
        assert!(!guard.is_active(&config));
        assert!(guard.is_hot());

        let mut config = SafetyConfig::default();
        let mut guard = CriticalGuard::default();
        for _ in 0..3 {
            guard.update(None, &config, SECOND);
        }
        config.critical_enabled = false;
        assert!(guard.is_active(&config));
        config.sensor_fallback_enabled = false;
        assert!(!guard.is_active(&config));
    }
}
