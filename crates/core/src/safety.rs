use std::time::Duration;

use crate::config::SafetyConfig;

#[derive(Debug, Default)]
pub struct CriticalGuard {
    active: bool,
    cool_for: Duration,
    missing_ticks: u32,
}

impl CriticalGuard {
    pub fn is_active(&self) -> bool {
        self.active
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
            if self.missing_ticks >= config.sensor_failure_ticks {
                self.active = true;
            }
            return self.active;
        };
        self.missing_ticks = 0;
        if temp >= config.critical_c {
            self.active = true;
            self.cool_for = Duration::ZERO;
        } else if self.active && temp < config.release_c {
            self.cool_for += elapsed;
            if self.cool_for.as_secs_f32() >= config.release_hold_secs {
                self.active = false;
                self.cool_for = Duration::ZERO;
            }
        } else {
            self.cool_for = Duration::ZERO;
        }
        self.active
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
}
