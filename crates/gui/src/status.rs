use fanforge_core::engine::FanState;
use fanforge_core::state::{ControlState, HardwareStatus};

use crate::connection::ControlStatus;
use crate::theme::Tone;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overall {
    Starting,
    Stopped,
    Critical,
    DriverMissing,
    Unavailable,
    Suspended,
    MonitorOnly,
    FanFault(usize),
    Controlling(usize),
    BiosOnly,
}

pub fn overall(status: &ControlStatus, state: Option<&ControlState>) -> Overall {
    if matches!(status, ControlStatus::Stopped(_)) {
        return Overall::Stopped;
    }
    let Some(state) = state else {
        return Overall::Starting;
    };
    if state.critical {
        return Overall::Critical;
    }
    match state.hardware {
        HardwareStatus::DriverMissing => return Overall::DriverMissing,
        HardwareStatus::Unavailable(_) => return Overall::Unavailable,
        HardwareStatus::Suspended => return Overall::Suspended,
        HardwareStatus::Ready => {}
    }
    if state.monitor_only {
        return Overall::MonitorOnly;
    }
    let faults = state
        .fans
        .iter()
        .filter(|fan| {
            matches!(
                fan.state,
                FanState::Faulted(_) | FanState::SensorFallback(_)
            )
        })
        .count();
    if faults > 0 {
        return Overall::FanFault(faults);
    }
    let controlled = state
        .fans
        .iter()
        .filter(|fan| {
            matches!(
                fan.state,
                FanState::Controlled
                    | FanState::Clamped
                    | FanState::Holding(_)
                    | FanState::Identifying
            )
        })
        .count();
    if controlled == 0 {
        Overall::BiosOnly
    } else {
        Overall::Controlling(controlled)
    }
}

fn fans(count: usize) -> &'static str {
    if count == 1 { "fan" } else { "fans" }
}

impl Overall {
    pub fn label(&self) -> String {
        match self {
            Self::Starting => "Starting…".to_owned(),
            Self::Stopped => "Control stopped".to_owned(),
            Self::Critical => "Critical temperature".to_owned(),
            Self::DriverMissing => "Driver missing".to_owned(),
            Self::Unavailable => "Hardware unavailable".to_owned(),
            Self::Suspended => "Suspended".to_owned(),
            Self::MonitorOnly => "Monitor only".to_owned(),
            Self::FanFault(count) => format!("{count} {} need attention", fans(*count)),
            Self::Controlling(count) => format!("Controlling {count} {}", fans(*count)),
            Self::BiosOnly => "BIOS controls every fan".to_owned(),
        }
    }

    pub fn tone(&self) -> Tone {
        match self {
            Self::Stopped | Self::Critical | Self::DriverMissing | Self::Unavailable => {
                Tone::Danger
            }
            Self::MonitorOnly | Self::FanFault(_) => Tone::Warn,
            Self::Controlling(_) => Tone::Ok,
            Self::Starting | Self::Suspended | Self::BiosOnly => Tone::Neutral,
        }
    }

    pub fn is_live(&self) -> bool {
        !matches!(self, Self::Stopped | Self::Starting)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    #[test]
    fn a_stopped_controller_outranks_a_stale_snapshot() {
        let mut state = fixtures::state();
        state.critical = true;
        assert_eq!(
            overall(&ControlStatus::Stopped("panic".into()), Some(&state)),
            Overall::Stopped
        );
        assert_eq!(
            overall(&ControlStatus::Running, Some(&state)),
            Overall::Critical
        );
    }

    #[test]
    fn the_status_follows_the_documented_priority() {
        let running = ControlStatus::Running;
        assert_eq!(overall(&running, None), Overall::Starting);

        let mut state = fixtures::state();
        assert_eq!(overall(&running, Some(&state)), Overall::Controlling(2));

        state.fans[0].state = FanState::SensorFallback("sensor".into());
        assert_eq!(overall(&running, Some(&state)), Overall::FanFault(1));

        state.monitor_only = true;
        assert_eq!(overall(&running, Some(&state)), Overall::MonitorOnly);

        state.hardware = HardwareStatus::Suspended;
        assert_eq!(overall(&running, Some(&state)), Overall::Suspended);

        state.hardware = HardwareStatus::Unavailable("busy".into());
        assert_eq!(overall(&running, Some(&state)), Overall::Unavailable);

        state.hardware = HardwareStatus::DriverMissing;
        assert_eq!(overall(&running, Some(&state)), Overall::DriverMissing);

        state.critical = true;
        assert_eq!(overall(&running, Some(&state)), Overall::Critical);
    }

    #[test]
    fn released_fans_read_as_bios_control() {
        let mut state = fixtures::state();
        for fan in &mut state.fans {
            fan.state = FanState::Released;
        }
        let status = overall(&ControlStatus::Running, Some(&state));
        assert_eq!(status, Overall::BiosOnly);
        assert_eq!(status.tone(), Tone::Neutral);
    }
}
