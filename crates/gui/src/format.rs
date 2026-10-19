use fanforge_core::board::FanLimits;
use fanforge_core::config::{Config, FanMode, FanSettings};
use fanforge_core::engine::FanState;
use fanforge_core::state::{ControlState, HardwareStatus};
use fanforge_core::telemetry::FanReading;
use fanforge_core::{Duty, FanId};

use crate::theme::Tone;

pub fn fan_state(state: &FanState) -> (String, Tone) {
    match state {
        FanState::Released => ("BIOS control".to_owned(), Tone::Neutral),
        FanState::Controlled => ("Controlled".to_owned(), Tone::Ok),
        FanState::Clamped => ("At minimum floor".to_owned(), Tone::Warn),
        FanState::Holding(reason) => (format!("Holding: {reason}"), Tone::Warn),
        FanState::SensorFallback(reason) => (format!("100% fallback: {reason}"), Tone::Danger),
        FanState::Critical => ("Critical override".to_owned(), Tone::Danger),
        FanState::Identifying => ("Identifying".to_owned(), Tone::Accent),
        FanState::Faulted(reason) => (format!("Released after fault: {reason}"), Tone::Danger),
        FanState::MonitorOnly => ("Monitor only".to_owned(), Tone::Neutral),
    }
}

pub fn hardware(status: &HardwareStatus) -> (String, Tone) {
    match status {
        HardwareStatus::Ready => ("Hardware ready".to_owned(), Tone::Ok),
        HardwareStatus::DriverMissing => ("PawnIO driver not installed".to_owned(), Tone::Danger),
        HardwareStatus::Suspended => ("System suspended".to_owned(), Tone::Neutral),
        HardwareStatus::Unavailable(reason) => {
            (format!("Hardware unavailable: {reason}"), Tone::Danger)
        }
    }
}

pub fn duty(duty: Option<Duty>) -> String {
    duty.map_or_else(|| "auto".to_owned(), |duty| duty.to_string())
}

pub fn rpm(fan: &FanReading) -> String {
    match fan.rpm {
        Some(0) => "stopped".to_owned(),
        Some(rpm) => format!("{rpm} rpm"),
        None => "no signal".to_owned(),
    }
}

pub fn connected(fan: &FanReading) -> bool {
    fan.rpm.is_some_and(|rpm| rpm > 0)
}

pub fn celsius(value: Option<f32>) -> String {
    value.map_or_else(|| "n/a".to_owned(), |value| format!("{value:.1} °C"))
}

pub fn whole_celsius(value: Option<f32>) -> String {
    value.map_or_else(|| "–".to_owned(), |value| format!("{value:.0} °C"))
}

pub fn fan_name(config: &Config, state: &ControlState, fan: &FanId) -> String {
    config
        .fans
        .get(fan)
        .and_then(|settings| settings.name.clone())
        .filter(|name| !name.trim().is_empty())
        .or_else(|| state.readings.fan(fan).map(|reading| reading.label.clone()))
        .unwrap_or_else(|| fan.to_string())
}

pub fn is_named(config: &Config, fan: &FanId) -> bool {
    config
        .fans
        .get(fan)
        .and_then(|settings| settings.name.as_ref())
        .is_some_and(|name| !name.trim().is_empty())
}

pub fn mode_summary(config: &Config, state: &ControlState, mode: &FanMode) -> String {
    match mode {
        FanMode::Auto => "BIOS".to_owned(),
        FanMode::Manual { duty } => format!("Fixed {duty}"),
        FanMode::Curve { curve } => {
            let name = config
                .curves
                .iter()
                .find(|candidate| &candidate.id == curve)
                .map_or_else(|| curve.to_string(), |candidate| candidate.name.clone());
            format!("Curve · {name}")
        }
        FanMode::Sync { fan } => format!("Follows {}", fan_name(config, state, fan)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Floor {
    pub percent: f32,
    pub may_stop: bool,
}

pub fn effective_floor(limits: FanLimits, settings: &FanSettings) -> Floor {
    let limits = limits.for_header(settings.case_fan);
    Floor {
        percent: limits.floor.percent().max(settings.min_duty.percent()),
        may_stop: limits.stop_permitted && settings.allow_stop,
    }
}

impl Floor {
    pub fn describe(self) -> String {
        if self.may_stop {
            format!("Floor {:.0}%; may stop at 0%", self.percent)
        } else {
            format!("Floor {:.0}%", self.percent)
        }
    }
}

pub fn critical_sensor_label(state: &ControlState) -> String {
    let id = &state.config.safety.critical_sensor;
    state
        .readings
        .sensors
        .iter()
        .find(|sensor| &sensor.id == id)
        .map_or_else(|| id.to_string(), |sensor| sensor.label.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fanforge_core::board::{CASE_FAN_FLOOR, SAFETY_FLOOR};

    #[test]
    fn the_effective_floor_never_drops_below_the_header_floor() {
        let limits = FanLimits::default();
        let mut settings = FanSettings::default();
        assert_eq!(
            effective_floor(limits, &settings),
            Floor {
                percent: SAFETY_FLOOR.percent(),
                may_stop: false
            }
        );
        settings.allow_stop = true;
        assert!(!effective_floor(limits, &settings).may_stop);
        settings.case_fan = true;
        assert_eq!(
            effective_floor(limits, &settings),
            Floor {
                percent: CASE_FAN_FLOOR.percent(),
                may_stop: true
            }
        );
        settings.min_duty = Duty::new(45.0);
        assert_eq!(effective_floor(limits, &settings).percent, 45.0);
    }
}
