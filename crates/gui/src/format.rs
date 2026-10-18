use eframe::egui::Color32;
use fanforge_core::config::{Config, FanMode};
use fanforge_core::engine::FanState;
use fanforge_core::protocol::{HardwareStatus, ServiceState};
use fanforge_core::telemetry::FanReading;
use fanforge_core::{Duty, FanId};

pub const OK: Color32 = Color32::from_rgb(90, 190, 120);
pub const WARN: Color32 = Color32::from_rgb(230, 180, 60);
pub const DANGER: Color32 = Color32::from_rgb(230, 80, 70);
pub const MUTED: Color32 = Color32::from_rgb(140, 140, 150);

pub fn fan_state(state: &FanState) -> (String, Color32) {
    match state {
        FanState::Released => ("BIOS auto".to_owned(), MUTED),
        FanState::Controlled => ("Controlled".to_owned(), OK),
        FanState::Clamped => ("At minimum floor".to_owned(), WARN),
        FanState::Holding(reason) => (format!("Holding: {reason}"), WARN),
        FanState::SensorFallback(reason) => (format!("100% fallback: {reason}"), DANGER),
        FanState::Critical => ("Critical override".to_owned(), DANGER),
        FanState::Identifying => ("Identifying".to_owned(), WARN),
        FanState::Faulted(reason) => (format!("Released after fault: {reason}"), DANGER),
        FanState::MonitorOnly => ("Monitor only".to_owned(), MUTED),
    }
}

pub fn hardware(status: &HardwareStatus) -> (String, Color32) {
    match status {
        HardwareStatus::Ready => ("Hardware ready".to_owned(), OK),
        HardwareStatus::Suspended => ("System suspended".to_owned(), MUTED),
        HardwareStatus::Unavailable(reason) => (format!("Hardware unavailable: {reason}"), DANGER),
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

pub fn celsius(value: Option<f32>) -> String {
    value.map_or_else(|| "n/a".to_owned(), |value| format!("{value:.1} °C"))
}

pub fn mode_name(mode: &FanMode) -> &'static str {
    match mode {
        FanMode::Auto => "BIOS auto",
        FanMode::Manual { .. } => "Fixed",
        FanMode::Curve { .. } => "Curve",
        FanMode::Sync { .. } => "Follow fan",
    }
}

pub fn fan_name(config: &Config, state: &ServiceState, fan: &FanId) -> String {
    config
        .fans
        .get(fan)
        .and_then(|settings| settings.name.clone())
        .filter(|name| !name.trim().is_empty())
        .or_else(|| state.readings.fan(fan).map(|reading| reading.label.clone()))
        .unwrap_or_else(|| fan.to_string())
}
