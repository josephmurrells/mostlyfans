use fanforge_core::board::FanLimits;
use fanforge_core::calibration::{self, FanCalibration};
use fanforge_core::config::{Config, FanMode, FanSettings};
use fanforge_core::engine::FanState;
use fanforge_core::state::{ControlState, HardwareStatus};
use fanforge_core::telemetry::FanReading;
use fanforge_core::{Duty, FanId, FanKind};

use crate::theme::Tone;

pub fn fan_state(state: &FanState) -> (String, Tone) {
    match state {
        FanState::Released => ("BIOS control".to_owned(), Tone::Neutral),
        FanState::Controlled => ("Controlled".to_owned(), Tone::Ok),
        FanState::Clamped => ("At minimum floor".to_owned(), Tone::Warn),
        FanState::Holding(reason) => (format!("Holding: {reason}"), Tone::Warn),
        FanState::SensorFallback(reason) => (format!("100% fallback: {reason}"), Tone::Danger),
        FanState::SensorReleased(reason) => {
            (format!("BIOS control (sensor lost): {reason}"), Tone::Warn)
        }
        FanState::Critical => ("Critical override".to_owned(), Tone::Danger),
        FanState::Identifying => ("Identifying".to_owned(), Tone::Accent),
        FanState::Calibrating => ("Calibrating".to_owned(), Tone::Accent),
        FanState::Faulted(_) => ("Released after fault".to_owned(), Tone::Danger),
        FanState::LeaderReleased(_) => ("BIOS control".to_owned(), Tone::Warn),
        FanState::MonitorOnly => ("Monitor only".to_owned(), Tone::Neutral),
        FanState::Unavailable(reason) => (format!("Not available: {reason}"), Tone::Warn),
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
    fan.id.kind() == FanKind::Gpu || fan.rpm.is_some_and(|rpm| rpm > 0)
}

pub fn nothing_driven(state: &ControlState) -> bool {
    state.monitor_only && (state.gpus.fans.is_empty() || state.gpus.monitor_only)
}

pub fn auto_name(fan: &FanId) -> &'static str {
    match fan.kind() {
        FanKind::Header => "BIOS",
        FanKind::Gpu => "Driver",
    }
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

pub fn mode_summary(config: &Config, state: &ControlState, fan: &FanId, mode: &FanMode) -> String {
    match mode {
        FanMode::Auto => auto_name(fan).to_owned(),
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

pub use calibration::Floor;

pub fn effective_floor(
    limits: FanLimits,
    settings: &FanSettings,
    calibration: Option<&FanCalibration>,
) -> Floor {
    calibration::effective_floor(limits, settings, calibration)
}

pub fn describe_floor(floor: Floor) -> String {
    let source = if floor.calibrated {
        " (calibrated)"
    } else {
        ""
    };
    if floor.may_stop {
        format!("Floor {:.0}%{source}; may stop at 0%", floor.percent)
    } else {
        format!("Floor {:.0}%{source}", floor.percent)
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
            effective_floor(limits, &settings, None),
            Floor {
                percent: SAFETY_FLOOR.percent(),
                may_stop: false,
                calibrated: false
            }
        );
        settings.allow_stop = true;
        assert!(!effective_floor(limits, &settings, None).may_stop);
        settings.case_fan = true;
        assert_eq!(
            effective_floor(limits, &settings, None),
            Floor {
                percent: CASE_FAN_FLOOR.percent(),
                may_stop: true,
                calibrated: false
            }
        );
        settings.min_duty = Duty::new(45.0);
        assert_eq!(effective_floor(limits, &settings, None).percent, 45.0);
    }

    #[test]
    fn gpu_fans_count_as_connected_while_stopped_and_auto_means_the_driver() {
        let reading = |id: &str| FanReading {
            id: id.into(),
            label: id.into(),
            rpm: Some(0),
            mode: fanforge_core::telemetry::HardwareMode::Auto,
            duty: None,
            unavailable: None,
        };
        assert!(connected(&reading("gpu/nvidia/gpu-1/fan1")));
        assert!(!connected(&reading("pwm1")));
        assert_eq!(auto_name(&"gpu/nvidia/gpu-1/fan1".into()), "Driver");
        assert_eq!(auto_name(&"pwm1".into()), "BIOS");
    }

    #[test]
    fn a_calibration_only_raises_the_floor() {
        let limits = FanLimits::default();
        let settings = FanSettings::default();
        let stalls_at = |percent: f32| FanCalibration {
            stop_duty: Some(Duty::new(percent)),
            ..FanCalibration::default()
        };
        let raised = effective_floor(limits, &settings, Some(&stalls_at(35.0)));
        assert_eq!(raised.percent, 45.0);
        assert!(raised.calibrated);
        assert_eq!(describe_floor(raised), "Floor 45% (calibrated)");
        let low = effective_floor(limits, &settings, Some(&stalls_at(5.0)));
        assert_eq!(low.percent, SAFETY_FLOOR.percent());
        assert!(!low.calibrated);
    }
}
