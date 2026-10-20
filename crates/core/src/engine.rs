use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::board::FanLimits;
use crate::calibration::{
    self, Calibration, CalibrationOutcome, CalibrationReport, CalibrationView, FanCalibration,
    RECOVER_TIME, Step,
};
use crate::config::{Config, FanMode};
use crate::curve::{CurveError, CurveEvaluator, HysteresisState, Tuning, find_curve};
use crate::safety::CriticalGuard;
use crate::telemetry::{FanReading, HardwareMode, Readings};
use crate::{Duty, FanId};

pub const MAX_WRITE_MISMATCHES: u32 = 3;
pub const SPIN_UP_DURATION: Duration = Duration::from_secs(2);
const MAX_SYNC_DEPTH: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", content = "duty", rename_all = "snake_case")]
pub enum Action {
    SetDuty(Duty),
    Release,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Command {
    pub fan: FanId,
    pub action: Action,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "reason", rename_all = "snake_case")]
pub enum FanState {
    Released,
    Controlled,
    Clamped,
    Holding(String),
    SensorFallback(String),
    SensorReleased(String),
    Critical,
    Identifying,
    Calibrating,
    Faulted(String),
    MonitorOnly,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FanStatus {
    pub id: FanId,
    pub state: FanState,
    pub target: Option<Duty>,
    pub output: Option<Duty>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TickOutcome {
    pub commands: Vec<Command>,
    pub fans: Vec<FanStatus>,
    pub critical: bool,
    pub critical_reached: bool,
    pub critical_sensor_missing: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum LastCommand {
    Duty(u8),
    Release,
}

#[derive(Debug, Default)]
struct FanRuntime {
    smoothed: Option<f32>,
    applied: Option<f32>,
    failures: u32,
    mismatches: u32,
    write_errors: u32,
    last_command: Option<LastCommand>,
    spin_up_left: Duration,
    recover_left: Duration,
    fault: Option<String>,
}

#[derive(Clone, Debug)]
enum Target {
    Release,
    Immediate(f32),
    Smoothed { duty: f32, tuning: Tuning },
    Follow(FanId),
    SensorLost(String),
    Failed(String),
}

#[derive(Debug)]
pub struct Engine {
    config: Config,
    limits: BTreeMap<FanId, FanLimits>,
    fans: HashMap<FanId, FanRuntime>,
    hysteresis: HysteresisState,
    critical: CriticalGuard,
    monitor_only: bool,
    identify: Option<(FanId, Duration)>,
    calibration: Option<Calibration>,
    calibration_request: Option<FanId>,
    calibration_clamped: bool,
    calibration_report: Option<CalibrationReport>,
    calibration_result: Option<(FanId, FanCalibration)>,
}

impl Engine {
    pub fn new(config: Config, limits: BTreeMap<FanId, FanLimits>) -> Self {
        Self {
            config,
            limits,
            fans: HashMap::new(),
            hysteresis: HysteresisState::default(),
            critical: CriticalGuard::default(),
            monitor_only: false,
            identify: None,
            calibration: None,
            calibration_request: None,
            calibration_clamped: false,
            calibration_report: None,
            calibration_result: None,
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn limits(&self) -> &BTreeMap<FanId, FanLimits> {
        &self.limits
    }

    pub fn set_config(&mut self, config: Config) {
        self.hysteresis.retain(&config.curves);
        self.config = config;
        for runtime in self.fans.values_mut() {
            runtime.fault = None;
            runtime.failures = 0;
            runtime.mismatches = 0;
            runtime.write_errors = 0;
        }
    }

    pub fn set_limits(&mut self, limits: BTreeMap<FanId, FanLimits>) {
        self.limits = limits;
    }

    pub fn set_monitor_only(&mut self, monitor_only: bool) {
        self.monitor_only = monitor_only;
    }

    pub fn is_monitor_only(&self) -> bool {
        self.monitor_only
    }

    pub fn identify(&mut self, fan: FanId, duration: Duration) {
        self.identify = Some((fan, duration));
    }

    pub fn request_calibration(&mut self, fan: FanId) -> Result<(), String> {
        if self.monitor_only {
            return Err("FanForge is in monitor-only mode".to_owned());
        }
        if let Some(cause) = self.critical.active_cause(&self.config.safety) {
            return Err(format!("{} is active", cause.describe()));
        }
        if let Some(running) = self
            .calibration
            .as_ref()
            .map(Calibration::fan)
            .or(self.calibration_request.as_ref())
        {
            return Err(format!("fan `{running}` is already being calibrated"));
        }
        self.calibration_request = Some(fan);
        Ok(())
    }

    pub fn cancel_calibration(&mut self, reason: Option<String>) {
        let outcome = reason.map_or(CalibrationOutcome::Cancelled, CalibrationOutcome::Aborted);
        if let Some(calibration) = self.calibration.take() {
            self.finish_calibration(&calibration, outcome);
        } else if let Some(fan) = self.calibration_request.take() {
            self.calibration_report = Some(CalibrationReport { fan, outcome });
        }
    }

    pub fn is_calibrating(&self) -> bool {
        self.calibration.is_some() || self.calibration_request.is_some()
    }

    pub fn calibration_view(&self) -> CalibrationView {
        CalibrationView {
            active: self.calibration.as_ref().map(Calibration::progress),
            last: self.calibration_report.clone(),
            not_restarting: self
                .config
                .calibration
                .iter()
                .filter(|(_, calibration)| calibration.not_restarting)
                .map(|(fan, _)| fan.clone())
                .collect(),
        }
    }

    pub fn take_calibration_result(&mut self) -> Option<(FanId, FanCalibration)> {
        self.calibration_result.take()
    }

    pub fn set_calibration(&mut self, fan: FanId, calibration: FanCalibration) {
        self.config.calibration.insert(fan, calibration);
    }

    fn finish_calibration(&mut self, calibration: &Calibration, outcome: CalibrationOutcome) {
        let fan = calibration.fan().clone();
        self.calibration_clamped = false;
        self.fans.entry(fan.clone()).or_default().recover_left = RECOVER_TIME;
        let result = match &outcome {
            CalibrationOutcome::Completed(result) => Some(result.clone()),
            _ => calibration
                .partial()
                .map(|partial| match self.config.calibration_for(&fan) {
                    Some(old) => FanCalibration {
                        stop_duty: match (old.stop_duty, partial.stop_duty) {
                            (Some(old), Some(new)) if new > old => Some(new),
                            (old, new) => old.or(new),
                        },
                        ..old.clone()
                    },
                    None => partial,
                }),
        };
        if let Some(mut result) = result {
            result.not_restarting |= outcome == CalibrationOutcome::NotRestarting;
            self.calibration_result = Some((fan.clone(), result));
        }
        self.calibration_report = Some(CalibrationReport { fan, outcome });
    }

    fn begin_calibration(&mut self, readings: &Readings) {
        let Some(fan) = self.calibration_request.take() else {
            return;
        };
        let runtime = self.fans.get(&fan);
        let fault = runtime.and_then(|runtime| runtime.fault.clone());
        let failing = runtime.is_some_and(|runtime| runtime.failures > 0);
        let started = if self.monitor_only {
            Err("FanForge is in monitor-only mode".to_owned())
        } else if let Some(cause) = self.critical.active_cause(&self.config.safety) {
            Err(format!("{} is active", cause.describe()))
        } else if let Some(reason) = fault {
            Err(format!("the fan is faulted: {reason}"))
        } else if failing {
            Err("the sensor controlling this fan is failing".to_owned())
        } else {
            Calibration::start(
                fan.clone(),
                readings,
                self.limits.get(&fan).copied().unwrap_or_default(),
                &self.config.settings(&fan),
                &self.config.safety.critical_sensor,
            )
        };
        match started {
            Ok(calibration) => {
                self.calibration_clamped = false;
                self.calibration = Some(calibration);
            }
            Err(reason) => {
                self.calibration_report = Some(CalibrationReport {
                    fan,
                    outcome: CalibrationOutcome::Aborted(reason),
                });
            }
        }
    }

    fn advance_calibration(
        &mut self,
        readings: &Readings,
        elapsed: Duration,
        identifying: Option<&FanId>,
    ) {
        self.begin_calibration(readings);
        let Some(mut calibration) = self.calibration.take() else {
            return;
        };
        let fan = calibration.fan().clone();
        let abort = if self.monitor_only {
            Some("monitor-only mode turned on".to_owned())
        } else if let Some(cause) = self.critical.active_cause(&self.config.safety) {
            Some(format!("{} fired", cause.describe()))
        } else if identifying == Some(&fan) {
            Some("Identify was requested for this fan".to_owned())
        } else if std::mem::take(&mut self.calibration_clamped) {
            Some("the engine had to clamp the probe duty".to_owned())
        } else {
            self.fans
                .get(&fan)
                .and_then(|runtime| runtime.fault.clone())
                .map(|reason| format!("the fan faulted: {reason}"))
        };
        let step = match abort {
            Some(reason) => Step::Done(CalibrationOutcome::Aborted(reason)),
            None => calibration.advance(readings, elapsed),
        };
        match step {
            Step::Hold(_) => self.calibration = Some(calibration),
            Step::Done(outcome) => self.finish_calibration(&calibration, outcome),
        }
    }

    pub fn forget_hardware_state(&mut self) {
        for runtime in self.fans.values_mut() {
            runtime.last_command = None;
            runtime.mismatches = 0;
            runtime.write_errors = 0;
            runtime.applied = None;
        }
    }

    pub fn record_apply_error(&mut self, fan: &FanId, error: &str) {
        let runtime = self.fans.entry(fan.clone()).or_default();
        runtime.write_errors += 1;
        if runtime.write_errors >= MAX_WRITE_MISMATCHES {
            runtime.fault = Some(format!("hardware write failed: {error}"));
        }
    }

    pub fn tick(&mut self, readings: &Readings, elapsed: Duration) -> TickOutcome {
        let critical_temp = readings.temperature(&self.config.safety.critical_sensor);
        let critical = self
            .critical
            .update(critical_temp, &self.config.safety, elapsed);
        let identifying = self.advance_identify(elapsed);
        self.advance_calibration(readings, elapsed, identifying.as_ref());

        let mut pending: Vec<(FanId, Target)> = {
            let mut evaluator =
                CurveEvaluator::new(&self.config.curves, readings, &mut self.hysteresis);
            readings
                .fans
                .iter()
                .map(|fan| {
                    (
                        fan.id.clone(),
                        resolve(&self.config, &fan.id, &mut evaluator),
                    )
                })
                .collect()
        };

        let mut planned: HashMap<FanId, (FanStatus, Action)> = HashMap::new();
        for _ in 0..=MAX_SYNC_DEPTH {
            let mut waiting = Vec::new();
            for (fan, target) in pending {
                let target = match target {
                    Target::Follow(leader) => match planned.get(&leader) {
                        Some((_, Action::SetDuty(duty))) => Target::Immediate(duty.percent()),
                        Some((_, Action::Release)) => Target::Release,
                        None if readings.fan(&leader).is_none() => {
                            Target::Failed(format!("followed fan `{leader}` does not exist"))
                        }
                        None => {
                            waiting.push((fan, Target::Follow(leader)));
                            continue;
                        }
                    },
                    other => other,
                };
                let override_state = self.override_state(&fan, critical, identifying.as_ref());
                let plan = self.plan_fan(&fan, target, override_state, elapsed);
                planned.insert(fan, plan);
            }
            pending = waiting;
            if pending.is_empty() {
                break;
            }
        }
        for (fan, _) in pending {
            let override_state = self.override_state(&fan, critical, identifying.as_ref());
            let target = Target::Failed("fan follow chain is a cycle".to_owned());
            let plan = self.plan_fan(&fan, target, override_state, elapsed);
            planned.insert(fan, plan);
        }

        let mut outcome = TickOutcome {
            critical,
            critical_reached: self.critical.is_hot(),
            critical_sensor_missing: self.critical.is_sensor_missing(),
            ..TickOutcome::default()
        };
        for reading in &readings.fans {
            let Some((mut status, action)) = planned.remove(&reading.id) else {
                continue;
            };
            if let Some(command) = self.reconcile(&mut status, reading, action) {
                outcome.commands.push(command);
            }
            outcome.fans.push(status);
        }
        outcome
    }

    fn override_state(
        &self,
        fan: &FanId,
        critical: bool,
        identifying: Option<&FanId>,
    ) -> Option<FanState> {
        if self.monitor_only {
            Some(FanState::MonitorOnly)
        } else if critical {
            Some(FanState::Critical)
        } else if identifying == Some(fan) {
            Some(FanState::Identifying)
        } else {
            None
        }
    }

    fn advance_identify(&mut self, elapsed: Duration) -> Option<FanId> {
        let (fan, remaining) = self.identify.take()?;
        let remaining = remaining.saturating_sub(elapsed);
        if remaining.is_zero() {
            return None;
        }
        self.identify = Some((fan.clone(), remaining));
        Some(fan)
    }

    fn plan_fan(
        &mut self,
        fan: &FanId,
        target: Target,
        override_state: Option<FanState>,
        elapsed: Duration,
    ) -> (FanStatus, Action) {
        let settings = self.config.settings(fan);
        let limits = self.limits.get(fan).copied().unwrap_or_default();
        let calibrated = self.config.calibration_for(fan);
        let header = calibration::header_floor(limits, &settings);
        let floor = calibration::effective_floor(limits, &settings, calibrated);
        let spin_up = calibration::effective_spin_up(&settings, calibrated);
        let probe = self
            .calibration
            .as_ref()
            .filter(|calibration| calibration.fan() == fan)
            .map(|calibration| calibration.duty().percent());
        let failure_limit = self.config.safety.sensor_failure_ticks;
        let sensor_fallback = self.config.safety.sensor_fallback_enabled;
        let runtime = self.fans.entry(fan.clone()).or_default();

        let recovering = !runtime.recover_left.is_zero();
        let override_state = override_state
            .or_else(|| runtime.fault.clone().map(FanState::Faulted))
            .or_else(|| (probe.is_some() || recovering).then_some(FanState::Calibrating));
        let (state, wanted) = match override_state {
            Some(state @ (FanState::MonitorOnly | FanState::Faulted(_))) => (state, None),
            Some(FanState::Calibrating) => {
                runtime.smoothed = Some(100.0);
                if probe.is_none() {
                    runtime.recover_left = runtime.recover_left.saturating_sub(elapsed);
                }
                (FanState::Calibrating, Some(probe.unwrap_or(100.0)))
            }
            Some(state) => {
                runtime.smoothed = Some(100.0);
                (state, Some(100.0))
            }
            None => match target {
                Target::Release => {
                    runtime.smoothed = None;
                    runtime.failures = 0;
                    (FanState::Released, None)
                }
                Target::Immediate(duty) => {
                    runtime.smoothed = Some(duty);
                    runtime.failures = 0;
                    (FanState::Controlled, Some(duty))
                }
                Target::Smoothed { duty, tuning } => {
                    runtime.failures = 0;
                    let next = runtime
                        .smoothed
                        .map_or(duty, |previous| smooth(previous, duty, &tuning, elapsed));
                    let next = if duty <= 0.0 && next < 0.5 { 0.0 } else { next };
                    runtime.smoothed = Some(next);
                    (FanState::Controlled, Some(next))
                }
                Target::SensorLost(reason) => {
                    runtime.failures += 1;
                    if runtime.failures < failure_limit {
                        (FanState::Holding(reason), runtime.smoothed)
                    } else if sensor_fallback {
                        runtime.smoothed = Some(100.0);
                        (FanState::SensorFallback(reason), Some(100.0))
                    } else {
                        runtime.smoothed = None;
                        (FanState::SensorReleased(reason), None)
                    }
                }
                Target::Failed(reason) => {
                    runtime.failures += 1;
                    if runtime.failures >= failure_limit {
                        runtime.smoothed = Some(100.0);
                        (FanState::SensorFallback(reason), Some(100.0))
                    } else {
                        (FanState::Holding(reason), runtime.smoothed)
                    }
                }
                Target::Follow(_) => (FanState::Released, None),
            },
        };

        let Some(wanted) = wanted else {
            runtime.applied = None;
            runtime.spin_up_left = Duration::ZERO;
            return (
                FanStatus {
                    id: fan.clone(),
                    state,
                    target: None,
                    output: None,
                },
                Action::Release,
            );
        };

        if let Some(probe) = probe.filter(|_| state == FanState::Calibrating) {
            let output = if header.may_stop {
                probe
            } else {
                probe.max(header.percent)
            };
            runtime.spin_up_left = Duration::ZERO;
            runtime.applied = Some(output);
            self.calibration_clamped |= output != probe;
            let output = Duty::new(output);
            let status = FanStatus {
                id: fan.clone(),
                state,
                target: Some(Duty::new(wanted)),
                output: Some(output),
            };
            return (status, Action::SetDuty(output));
        }

        let may_stop = floor.may_stop;
        let floor = floor.percent;
        let (mut output, mut state) = if wanted >= floor {
            (wanted, state)
        } else if may_stop && wanted <= 0.0 {
            (0.0, state)
        } else {
            let clamped = if state == FanState::Controlled {
                FanState::Clamped
            } else {
                state
            };
            (floor, clamped)
        };

        let was_stopped = runtime.applied.is_some_and(|applied| applied <= 0.0);
        if output > 0.0 && was_stopped {
            runtime.spin_up_left = SPIN_UP_DURATION;
        }
        if output > 0.0 && !runtime.spin_up_left.is_zero() {
            output = output.max(spin_up);
            runtime.spin_up_left = runtime.spin_up_left.saturating_sub(elapsed);
        } else if output <= 0.0 {
            runtime.spin_up_left = Duration::ZERO;
        }
        if state == FanState::Clamped && output > floor {
            state = FanState::Controlled;
        }
        runtime.applied = Some(output);

        let output = Duty::new(output);
        let status = FanStatus {
            id: fan.clone(),
            state,
            target: Some(Duty::new(wanted)),
            output: Some(output),
        };
        (status, Action::SetDuty(output))
    }

    fn reconcile(
        &mut self,
        status: &mut FanStatus,
        reading: &FanReading,
        action: Action,
    ) -> Option<Command> {
        let fan = &status.id;
        let runtime = self.fans.entry(fan.clone()).or_default();
        let hardware_raw = match reading.mode {
            HardwareMode::Manual => reading.duty.map(Duty::to_raw),
            HardwareMode::Auto => None,
        };
        match action {
            Action::Release => {
                let driven = matches!(runtime.last_command, Some(LastCommand::Duty(_)));
                runtime.last_command = Some(LastCommand::Release);
                runtime.mismatches = 0;
                driven.then(|| Command {
                    fan: fan.clone(),
                    action: Action::Release,
                })
            }
            Action::SetDuty(duty) => {
                let raw = duty.to_raw();
                if let Some(LastCommand::Duty(previous)) = runtime.last_command {
                    if hardware_raw == Some(previous) {
                        runtime.mismatches = 0;
                        runtime.write_errors = 0;
                    } else {
                        runtime.mismatches += 1;
                    }
                }
                if runtime.mismatches >= MAX_WRITE_MISMATCHES {
                    let reason = "hardware did not keep the requested duty".to_owned();
                    runtime.fault = Some(reason.clone());
                    runtime.last_command = Some(LastCommand::Release);
                    runtime.applied = None;
                    status.state = FanState::Faulted(reason);
                    status.target = None;
                    status.output = None;
                    return Some(Command {
                        fan: fan.clone(),
                        action: Action::Release,
                    });
                }
                runtime.last_command = Some(LastCommand::Duty(raw));
                (hardware_raw != Some(raw)).then(|| Command {
                    fan: fan.clone(),
                    action,
                })
            }
        }
    }
}

fn resolve(config: &Config, fan: &FanId, evaluator: &mut CurveEvaluator<'_>) -> Target {
    match config.mode(fan) {
        FanMode::Auto => Target::Release,
        FanMode::Manual { duty } => Target::Immediate(duty.percent()),
        FanMode::Curve { curve } => match evaluator.evaluate(&curve) {
            Ok(duty) => Target::Smoothed {
                duty,
                tuning: find_curve(&config.curves, &curve)
                    .map(|c| c.tuning.clone())
                    .unwrap_or_default(),
            },
            Err(error @ CurveError::SensorUnavailable(_)) => Target::SensorLost(error.to_string()),
            Err(error) => Target::Failed(error.to_string()),
        },
        FanMode::Sync { fan: leader } if &leader != fan => Target::Follow(leader),
        FanMode::Sync { .. } => Target::Failed("a fan cannot follow itself".to_owned()),
    }
}

fn smooth(previous: f32, target: f32, tuning: &Tuning, elapsed: Duration) -> f32 {
    let dt = elapsed.as_secs_f32();
    let alpha = if tuning.response_secs <= 0.0 {
        1.0
    } else {
        1.0 - (-dt / tuning.response_secs).exp()
    };
    let eased = previous + (target - previous) * alpha;
    let max_up = if tuning.step_up_per_sec > 0.0 {
        tuning.step_up_per_sec * dt
    } else {
        f32::INFINITY
    };
    let max_down = if tuning.step_down_per_sec > 0.0 {
        tuning.step_down_per_sec * dt
    } else {
        f32::INFINITY
    };
    eased.clamp(previous - max_down, previous + max_up)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calibration::Phase;
    use crate::curve::{CurveDef, CurveKind, CurvePoint};
    use crate::telemetry::SensorReading;

    const TICK: Duration = Duration::from_secs(1);

    struct Rig {
        engine: Engine,
        readings: Readings,
    }

    impl Rig {
        fn new(config: Config, limits: FanLimits) -> Self {
            let fans = ["pwm1", "pwm2"]
                .into_iter()
                .map(|id| FanReading {
                    id: id.into(),
                    label: id.into(),
                    rpm: Some(1000),
                    mode: HardwareMode::Auto,
                    duty: None,
                })
                .collect();
            let limits = [("pwm1".into(), limits), ("pwm2".into(), limits)]
                .into_iter()
                .collect();
            Self {
                engine: Engine::new(config, limits),
                readings: Readings {
                    sensors: vec![
                        SensorReading {
                            id: "cpu/tctl".into(),
                            label: "Tctl".into(),
                            celsius: Some(50.0),
                        },
                        SensorReading {
                            id: "board/temp1".into(),
                            label: "Board".into(),
                            celsius: Some(50.0),
                        },
                    ],
                    fans,
                },
            }
        }

        fn set_temp(&mut self, celsius: Option<f32>) {
            for sensor in &mut self.readings.sensors {
                sensor.celsius = celsius;
            }
        }

        fn set_curve_temp(&mut self, celsius: Option<f32>) {
            self.readings.sensors[1].celsius = celsius;
        }

        fn tick(&mut self) -> TickOutcome {
            let outcome = self.engine.tick(&self.readings, TICK);
            for command in &outcome.commands {
                let fan = self
                    .readings
                    .fans
                    .iter_mut()
                    .find(|fan| fan.id == command.fan)
                    .unwrap();
                match command.action {
                    Action::SetDuty(duty) => {
                        fan.mode = HardwareMode::Manual;
                        fan.duty = Some(duty);
                    }
                    Action::Release => {
                        fan.mode = HardwareMode::Auto;
                        fan.duty = None;
                    }
                }
            }
            outcome
        }

        fn status(outcome: &TickOutcome, fan: &str) -> FanStatus {
            outcome
                .fans
                .iter()
                .find(|status| status.id.as_str() == fan)
                .cloned()
                .unwrap()
        }
    }

    fn open_limits() -> FanLimits {
        FanLimits {
            floor: Duty::OFF,
            stop_permitted: true,
        }
    }

    fn config_with(modes: &[(&str, FanMode)]) -> Config {
        let mut config = Config::default();
        config.curves.push(CurveDef {
            id: "cpu".into(),
            name: "CPU".into(),
            kind: CurveKind::Graph {
                sensor: "board/temp1".into(),
                points: vec![CurvePoint::new(40.0, 20.0), CurvePoint::new(80.0, 100.0)],
            },
            tuning: Tuning {
                hysteresis_c: 0.0,
                response_secs: 0.0,
                step_up_per_sec: 0.0,
                step_down_per_sec: 0.0,
            },
        });
        let profile = config.profiles.get_mut("Balanced").unwrap();
        for (fan, mode) in modes {
            profile.fans.insert((*fan).into(), mode.clone());
        }
        config.validate().unwrap();
        config
    }

    #[test]
    fn auto_fans_are_left_alone() {
        let mut rig = Rig::new(Config::default(), open_limits());
        let outcome = rig.tick();
        assert!(outcome.commands.is_empty());
        assert_eq!(Rig::status(&outcome, "pwm1").state, FanState::Released);
    }

    #[test]
    fn curve_output_is_written_only_when_it_changes() {
        let mut rig = Rig::new(
            config_with(&[(
                "pwm1",
                FanMode::Curve {
                    curve: "cpu".into(),
                },
            )]),
            open_limits(),
        );
        let first = rig.tick();
        assert_eq!(first.commands.len(), 1);
        assert_eq!(Rig::status(&first, "pwm1").output, Some(Duty::new(40.0)));
        assert!(rig.tick().commands.is_empty());
        rig.set_temp(Some(60.0));
        let changed = rig.tick();
        assert_eq!(changed.commands[0].action, Action::SetDuty(Duty::new(60.0)));
    }

    #[test]
    fn floors_are_enforced_even_for_manual_zero() {
        let limits = FanLimits {
            floor: Duty::new(30.0),
            stop_permitted: false,
        };
        let mut rig = Rig::new(
            config_with(&[("pwm1", FanMode::Manual { duty: Duty::OFF })]),
            limits,
        );
        let outcome = rig.tick();
        let status = Rig::status(&outcome, "pwm1");
        assert_eq!(status.output, Some(Duty::new(30.0)));
        assert_eq!(status.state, FanState::Clamped);
    }

    #[test]
    fn stopping_requires_hardware_permission_and_user_opt_in() {
        let mut config = config_with(&[("pwm1", FanMode::Manual { duty: Duty::OFF })]);
        config.fans.insert(
            "pwm1".into(),
            crate::config::FanSettings {
                allow_stop: true,
                ..Default::default()
            },
        );
        let mut rig = Rig::new(
            config.clone(),
            FanLimits {
                floor: Duty::new(20.0),
                stop_permitted: true,
            },
        );
        assert_eq!(Rig::status(&rig.tick(), "pwm1").output, Some(Duty::OFF));

        let mut rig = Rig::new(
            config,
            FanLimits {
                floor: Duty::new(20.0),
                stop_permitted: false,
            },
        );
        assert_eq!(
            Rig::status(&rig.tick(), "pwm1").output,
            Some(Duty::new(20.0))
        );
    }

    fn header_output(duty: f32, case_fan: bool, allow_stop: bool) -> Option<Duty> {
        let mut config = config_with(&[(
            "pwm1",
            FanMode::Manual {
                duty: Duty::new(duty),
            },
        )]);
        config.fans.insert(
            "pwm1".into(),
            crate::config::FanSettings {
                case_fan,
                allow_stop,
                ..Default::default()
            },
        );
        let mut rig = Rig::new(config, FanLimits::default());
        Rig::status(&rig.tick(), "pwm1").output
    }

    #[test]
    fn case_fans_use_a_lower_floor_and_stop_only_when_allowed() {
        assert_eq!(header_output(25.0, false, false), Some(Duty::new(30.0)));
        assert_eq!(header_output(25.0, true, false), Some(Duty::new(25.0)));
        assert_eq!(header_output(10.0, true, false), Some(Duty::new(20.0)));
        assert_eq!(header_output(0.0, true, false), Some(Duty::new(20.0)));
        assert_eq!(header_output(0.0, true, true), Some(Duty::OFF));
        assert_eq!(header_output(0.0, false, true), Some(Duty::new(30.0)));
    }

    #[test]
    fn spin_up_kick_applies_when_restarting_a_stopped_fan() {
        let mut config = config_with(&[("pwm1", FanMode::Manual { duty: Duty::OFF })]);
        config.fans.insert(
            "pwm1".into(),
            crate::config::FanSettings {
                allow_stop: true,
                spin_up_duty: Duty::new(60.0),
                ..Default::default()
            },
        );
        let mut rig = Rig::new(config.clone(), open_limits());
        rig.tick();
        config.profiles.get_mut("Balanced").unwrap().fans.insert(
            "pwm1".into(),
            FanMode::Manual {
                duty: Duty::new(25.0),
            },
        );
        rig.engine.set_config(config);
        assert_eq!(
            Rig::status(&rig.tick(), "pwm1").output,
            Some(Duty::new(60.0))
        );
        assert_eq!(
            Rig::status(&rig.tick(), "pwm1").output,
            Some(Duty::new(60.0))
        );
        assert_eq!(
            Rig::status(&rig.tick(), "pwm1").output,
            Some(Duty::new(25.0))
        );
    }

    #[test]
    fn sensor_failure_holds_then_falls_back_to_full_speed() {
        let mut rig = Rig::new(
            config_with(&[(
                "pwm1",
                FanMode::Curve {
                    curve: "cpu".into(),
                },
            )]),
            open_limits(),
        );
        rig.tick();
        rig.set_curve_temp(None);
        assert!(matches!(
            Rig::status(&rig.tick(), "pwm1").state,
            FanState::Holding(_)
        ));
        assert!(matches!(
            Rig::status(&rig.tick(), "pwm1").state,
            FanState::Holding(_)
        ));
        let fallback = Rig::status(&rig.tick(), "pwm1");
        assert!(matches!(fallback.state, FanState::SensorFallback(_)));
        assert_eq!(fallback.output, Some(Duty::FULL));
        rig.set_temp(Some(50.0));
        assert_eq!(Rig::status(&rig.tick(), "pwm1").state, FanState::Controlled);
    }

    fn safety_off(mut config: Config, critical: bool, fallback: bool) -> Config {
        config.safety.critical_enabled = critical;
        config.safety.sensor_fallback_enabled = fallback;
        config
    }

    fn curve_fan() -> Config {
        config_with(&[(
            "pwm1",
            FanMode::Curve {
                curve: "cpu".into(),
            },
        )])
    }

    #[test]
    fn with_the_fallback_off_a_lost_sensor_holds_then_releases_to_the_bios() {
        let mut rig = Rig::new(safety_off(curve_fan(), true, false), open_limits());
        rig.tick();
        rig.set_curve_temp(None);
        for _ in 0..2 {
            assert!(matches!(
                Rig::status(&rig.tick(), "pwm1").state,
                FanState::Holding(_)
            ));
        }
        let outcome = rig.tick();
        let released = Rig::status(&outcome, "pwm1");
        assert!(matches!(released.state, FanState::SensorReleased(_)));
        assert_eq!(released.output, None);
        assert!(outcome.commands.contains(&Command {
            fan: "pwm1".into(),
            action: Action::Release,
        }));
        rig.set_curve_temp(Some(50.0));
        assert_eq!(Rig::status(&rig.tick(), "pwm1").state, FanState::Controlled);
    }

    #[test]
    fn a_config_apply_never_brings_back_a_released_fans_old_duty() {
        let config = safety_off(curve_fan(), true, false);
        let mut rig = Rig::new(config.clone(), open_limits());
        rig.tick();
        rig.set_curve_temp(None);
        for _ in 0..3 {
            rig.tick();
        }
        rig.engine.set_config(config);
        for _ in 0..5 {
            let outcome = rig.tick();
            assert_eq!(Rig::status(&outcome, "pwm1").output, None);
            assert!(
                outcome
                    .commands
                    .iter()
                    .all(|command| command.action == Action::Release)
            );
        }
    }

    #[test]
    fn with_the_fallback_off_config_errors_still_go_to_full_speed() {
        let mut rig = Rig::new(
            safety_off(
                config_with(&[("pwm2", FanMode::Sync { fan: "pwm9".into() })]),
                true,
                false,
            ),
            open_limits(),
        );
        rig.tick();
        rig.tick();
        let status = Rig::status(&rig.tick(), "pwm2");
        assert!(matches!(status.state, FanState::SensorFallback(_)));
        assert_eq!(status.output, Some(Duty::FULL));
    }

    #[test]
    fn a_follower_of_a_released_leader_is_released_too() {
        let mut config = safety_off(curve_fan(), true, false);
        config
            .profiles
            .get_mut(&config.active_profile)
            .unwrap()
            .fans
            .insert("pwm2".into(), FanMode::Sync { fan: "pwm1".into() });
        let mut rig = Rig::new(config, open_limits());
        rig.tick();
        rig.set_curve_temp(None);
        for _ in 0..2 {
            rig.tick();
        }
        let outcome = rig.tick();
        assert!(matches!(
            Rig::status(&outcome, "pwm1").state,
            FanState::SensorReleased(_)
        ));
        let follower = Rig::status(&outcome, "pwm2");
        assert_eq!(follower.state, FanState::Released);
        assert_eq!(follower.output, None);
    }

    #[test]
    fn with_the_override_off_heat_is_reported_but_fans_are_not_forced() {
        let mut rig = Rig::new(
            safety_off(
                config_with(&[(
                    "pwm1",
                    FanMode::Manual {
                        duty: Duty::new(30.0),
                    },
                )]),
                false,
                true,
            ),
            open_limits(),
        );
        rig.set_temp(Some(95.0));
        let outcome = rig.tick();
        assert!(!outcome.critical);
        assert!(outcome.critical_reached);
        assert_eq!(Rig::status(&outcome, "pwm1").output, Some(Duty::new(30.0)));
    }

    #[test]
    fn with_the_fallback_off_a_missing_safety_sensor_forces_nothing() {
        let mut rig = Rig::new(
            safety_off(
                config_with(&[(
                    "pwm1",
                    FanMode::Manual {
                        duty: Duty::new(40.0),
                    },
                )]),
                true,
                false,
            ),
            open_limits(),
        );
        rig.readings.sensors[0].celsius = None;
        for _ in 0..5 {
            let outcome = rig.tick();
            assert!(!outcome.critical);
            assert!(outcome.critical_sensor_missing);
            assert_eq!(Rig::status(&outcome, "pwm1").output, Some(Duty::new(40.0)));
        }
    }

    #[test]
    fn implausible_values_count_as_sensor_failure() {
        let mut rig = Rig::new(
            config_with(&[(
                "pwm1",
                FanMode::Curve {
                    curve: "cpu".into(),
                },
            )]),
            open_limits(),
        );
        rig.set_curve_temp(Some(200.0));
        let first = rig.tick();
        let status = Rig::status(&first, "pwm1");
        assert!(matches!(status.state, FanState::Holding(_)));
        assert_eq!(status.output, None);
        assert!(first.commands.is_empty());
        rig.tick();
        let status = Rig::status(&rig.tick(), "pwm1");
        assert!(matches!(status.state, FanState::SensorFallback(_)));
        assert_eq!(status.output, Some(Duty::FULL));
    }

    #[test]
    fn missing_critical_sensor_eventually_forces_full_speed() {
        let mut rig = Rig::new(
            config_with(&[(
                "pwm1",
                FanMode::Manual {
                    duty: Duty::new(40.0),
                },
            )]),
            open_limits(),
        );
        rig.readings.sensors[0].celsius = None;
        assert!(!rig.tick().critical);
        assert!(!rig.tick().critical);
        let outcome = rig.tick();
        assert!(outcome.critical);
        assert!(outcome.critical_sensor_missing);
        assert_eq!(Rig::status(&outcome, "pwm2").output, Some(Duty::FULL));
    }

    #[test]
    fn monitor_only_never_touches_fans_it_did_not_drive() {
        let mut rig = Rig::new(Config::default(), open_limits());
        rig.engine.set_monitor_only(true);
        rig.readings.fans[0].mode = HardwareMode::Manual;
        rig.readings.fans[0].duty = Some(Duty::new(35.0));
        for _ in 0..3 {
            assert!(rig.tick().commands.is_empty());
        }
    }

    #[test]
    fn write_errors_fault_after_three_failed_ticks() {
        let mut rig = Rig::new(
            config_with(&[(
                "pwm1",
                FanMode::Manual {
                    duty: Duty::new(50.0),
                },
            )]),
            open_limits(),
        );
        for attempt in 1..=MAX_WRITE_MISMATCHES {
            let outcome = rig.engine.tick(&rig.readings, TICK);
            assert!(
                !matches!(Rig::status(&outcome, "pwm1").state, FanState::Faulted(_)),
                "faulted early on attempt {attempt}"
            );
            rig.engine.record_apply_error(&"pwm1".into(), "boom");
        }
        let outcome = rig.engine.tick(&rig.readings, TICK);
        assert!(matches!(
            Rig::status(&outcome, "pwm1").state,
            FanState::Faulted(_)
        ));
    }

    #[test]
    fn followers_track_the_leader_output_and_missing_leaders_fail_safe() {
        let limits = FanLimits {
            floor: Duty::new(30.0),
            stop_permitted: false,
        };
        let mut rig = Rig::new(
            config_with(&[
                (
                    "pwm1",
                    FanMode::Manual {
                        duty: Duty::new(10.0),
                    },
                ),
                ("pwm2", FanMode::Sync { fan: "pwm1".into() }),
            ]),
            limits,
        );
        let outcome = rig.tick();
        assert_eq!(Rig::status(&outcome, "pwm1").output, Some(Duty::new(30.0)));
        assert_eq!(Rig::status(&outcome, "pwm2").output, Some(Duty::new(30.0)));

        let mut rig = Rig::new(
            config_with(&[("pwm2", FanMode::Sync { fan: "pwm9".into() })]),
            open_limits(),
        );
        rig.tick();
        rig.tick();
        let status = Rig::status(&rig.tick(), "pwm2");
        assert!(matches!(status.state, FanState::SensorFallback(_)));
        assert_eq!(status.output, Some(Duty::FULL));
    }

    #[test]
    fn critical_temperature_forces_every_fan_to_full_speed() {
        let mut rig = Rig::new(
            config_with(&[(
                "pwm1",
                FanMode::Manual {
                    duty: Duty::new(30.0),
                },
            )]),
            open_limits(),
        );
        rig.tick();
        rig.set_temp(Some(90.0));
        let outcome = rig.tick();
        assert!(outcome.critical);
        for fan in ["pwm1", "pwm2"] {
            let status = Rig::status(&outcome, fan);
            assert_eq!(status.state, FanState::Critical);
            assert_eq!(status.output, Some(Duty::FULL));
        }
    }

    #[test]
    fn critical_release_ramps_curve_fans_down_smoothly() {
        let mut config = config_with(&[(
            "pwm1",
            FanMode::Curve {
                curve: "cpu".into(),
            },
        )]);
        config.curves[0].tuning = Tuning {
            step_down_per_sec: 10.0,
            response_secs: 0.0,
            ..Tuning::default()
        };
        config.safety.release_hold_secs = 1.0;
        let mut rig = Rig::new(config, open_limits());
        rig.set_temp(Some(90.0));
        rig.tick();
        rig.set_temp(Some(50.0));
        assert_eq!(
            Rig::status(&rig.tick(), "pwm1").output,
            Some(Duty::new(90.0))
        );
        assert_eq!(
            Rig::status(&rig.tick(), "pwm1").output,
            Some(Duty::new(80.0))
        );
    }

    #[test]
    fn critical_and_identify_skip_even_the_slowest_smoothing() {
        let slowest = Tuning {
            hysteresis_c: 20.0,
            response_secs: 60.0,
            step_up_per_sec: 0.1,
            step_down_per_sec: 0.1,
        };
        let mut config = config_with(&[
            (
                "pwm1",
                FanMode::Curve {
                    curve: "cpu".into(),
                },
            ),
            (
                "pwm2",
                FanMode::Curve {
                    curve: "cpu".into(),
                },
            ),
        ]);
        config.curves[0].tuning = slowest;
        let mut rig = Rig::new(config, open_limits());
        rig.set_temp(Some(45.0));
        rig.tick();
        rig.engine.identify("pwm2".into(), Duration::from_secs(2));
        let outcome = rig.tick();
        assert_eq!(Rig::status(&outcome, "pwm2").state, FanState::Identifying);
        assert_eq!(Rig::status(&outcome, "pwm2").output, Some(Duty::FULL));
        assert!(Rig::status(&outcome, "pwm1").output < Some(Duty::FULL));

        rig.set_temp(Some(90.0));
        let outcome = rig.tick();
        assert_eq!(Rig::status(&outcome, "pwm1").state, FanState::Critical);
        assert_eq!(Rig::status(&outcome, "pwm1").output, Some(Duty::FULL));
    }

    #[test]
    fn monitor_only_releases_controlled_fans() {
        let mut rig = Rig::new(
            config_with(&[(
                "pwm1",
                FanMode::Manual {
                    duty: Duty::new(50.0),
                },
            )]),
            open_limits(),
        );
        rig.tick();
        rig.engine.set_monitor_only(true);
        let outcome = rig.tick();
        assert_eq!(
            outcome.commands,
            vec![Command {
                fan: "pwm1".into(),
                action: Action::Release
            }]
        );
        assert_eq!(Rig::status(&outcome, "pwm1").state, FanState::MonitorOnly);
    }

    #[test]
    fn firmware_reverting_to_auto_is_reasserted_then_faulted() {
        let mut rig = Rig::new(
            config_with(&[(
                "pwm1",
                FanMode::Manual {
                    duty: Duty::new(50.0),
                },
            )]),
            open_limits(),
        );
        rig.tick();
        for _ in 0..MAX_WRITE_MISMATCHES - 1 {
            rig.readings.fans[0].mode = HardwareMode::Auto;
            let outcome = rig.tick();
            assert_eq!(outcome.commands[0].action, Action::SetDuty(Duty::new(50.0)));
        }
        rig.readings.fans[0].mode = HardwareMode::Auto;
        let outcome = rig.tick();
        assert_eq!(outcome.commands[0].action, Action::Release);
        assert!(matches!(
            Rig::status(&outcome, "pwm1").state,
            FanState::Faulted(_)
        ));
        rig.engine.set_config(rig.engine.config().clone());
        assert_eq!(Rig::status(&rig.tick(), "pwm1").state, FanState::Controlled);
    }

    #[test]
    fn sync_follows_the_leader_and_identify_overrides_temporarily() {
        let mut rig = Rig::new(
            config_with(&[
                (
                    "pwm1",
                    FanMode::Curve {
                        curve: "cpu".into(),
                    },
                ),
                ("pwm2", FanMode::Sync { fan: "pwm1".into() }),
            ]),
            open_limits(),
        );
        assert_eq!(
            Rig::status(&rig.tick(), "pwm2").output,
            Some(Duty::new(40.0))
        );
        rig.engine.identify("pwm2".into(), Duration::from_secs(2));
        assert_eq!(
            Rig::status(&rig.tick(), "pwm2").state,
            FanState::Identifying
        );
        assert_eq!(
            Rig::status(&rig.tick(), "pwm2").output,
            Some(Duty::new(40.0))
        );
    }

    #[test]
    fn smoothing_respects_rate_limits() {
        let tuning = Tuning {
            hysteresis_c: 0.0,
            response_secs: 0.0,
            step_up_per_sec: 5.0,
            step_down_per_sec: 2.0,
        };
        assert_eq!(smooth(50.0, 80.0, &tuning, TICK), 55.0);
        assert_eq!(smooth(50.0, 20.0, &tuning, TICK), 48.0);
        let eased = Tuning {
            response_secs: 1.0,
            step_up_per_sec: 0.0,
            step_down_per_sec: 0.0,
            hysteresis_c: 0.0,
        };
        let value = smooth(0.0, 100.0, &eased, TICK);
        assert!((value - 63.212).abs() < 0.01);
    }

    fn calibration_config(modes: &[(&str, FanMode)], case_fan: bool, allow_stop: bool) -> Config {
        let mut config = config_with(modes);
        config.fans.insert(
            "pwm1".into(),
            crate::config::FanSettings {
                case_fan,
                allow_stop,
                ..Default::default()
            },
        );
        config
    }

    fn calibration_rig(modes: &[(&str, FanMode)], case_fan: bool, allow_stop: bool) -> Rig {
        Rig::new(
            calibration_config(modes, case_fan, allow_stop),
            FanLimits::default(),
        )
    }

    impl Rig {
        fn spin(&mut self, stops_below: f32, restarts: bool) -> TickOutcome {
            let outcome = self.tick();
            let fan = &mut self.readings.fans[0];
            let duty = fan.duty.map_or(100.0, Duty::percent);
            let stopped = fan.rpm == Some(0);
            fan.rpm = Some(if duty < stops_below || (stopped && !restarts) {
                0
            } else {
                (duty * 15.0) as u32
            });
            outcome
        }

        fn spin_until(
            &mut self,
            stops_below: f32,
            mut done: impl FnMut(&Engine, &TickOutcome) -> bool,
        ) -> TickOutcome {
            for _ in 0..2000 {
                let outcome = self.spin(stops_below, true);
                if done(&self.engine, &outcome) {
                    return outcome;
                }
            }
            panic!("condition never reached");
        }

        fn report(&self) -> CalibrationOutcome {
            self.engine.calibration_view().last.unwrap().outcome
        }
    }

    fn finished(engine: &Engine, _: &TickOutcome) -> bool {
        !engine.is_calibrating()
    }

    fn output_of(outcome: &TickOutcome, fan: &str) -> f32 {
        Rig::status(outcome, fan)
            .output
            .map_or(f32::NAN, Duty::percent)
    }

    fn manual(duty: f32) -> FanMode {
        FanMode::Manual {
            duty: Duty::new(duty),
        }
    }

    fn aborted(outcome: CalibrationOutcome, text: &str) {
        match outcome {
            CalibrationOutcome::Aborted(reason) => {
                assert!(
                    reason.contains(text),
                    "`{reason}` does not mention `{text}`"
                );
            }
            other => panic!("expected an abort, got {other:?}"),
        }
    }

    fn completed(outcome: CalibrationOutcome) -> FanCalibration {
        match outcome {
            CalibrationOutcome::Completed(result) => result,
            other => panic!("expected a completed run, got {other:?}"),
        }
    }

    #[test]
    fn calibration_starts_on_the_next_tick_and_probes_from_full_speed() {
        let mut rig = calibration_rig(&[("pwm1", manual(40.0))], false, false);
        rig.tick();
        rig.engine.request_calibration("pwm1".into()).unwrap();
        assert!(rig.engine.calibration_view().active.is_none());
        assert!(rig.engine.request_calibration("pwm2".into()).is_err());
        let status = Rig::status(&rig.tick(), "pwm1");
        assert_eq!(status.state, FanState::Calibrating);
        assert_eq!(status.output, Some(Duty::FULL));
        assert!(rig.engine.calibration_view().active.is_some());
        assert!(rig.engine.request_calibration("pwm2".into()).is_err());
    }

    #[test]
    fn a_failed_start_check_reports_and_commands_nothing() {
        let mut rig = calibration_rig(&[("pwm1", manual(40.0))], false, false);
        rig.tick();
        rig.readings.fans[0].rpm = None;
        rig.engine.request_calibration("pwm1".into()).unwrap();
        let status = Rig::status(&rig.tick(), "pwm1");
        assert_eq!(status.state, FanState::Controlled);
        assert_eq!(status.output, Some(Duty::new(40.0)));
        aborted(rig.report(), "no RPM signal");
        assert!(!rig.engine.is_calibrating());
        assert!(rig.engine.take_calibration_result().is_none());
    }

    #[test]
    fn calibration_is_refused_in_monitor_only_and_under_critical() {
        let mut rig = calibration_rig(&[], false, false);
        rig.engine.set_monitor_only(true);
        assert!(rig.engine.request_calibration("pwm1".into()).is_err());
        rig.engine.set_monitor_only(false);
        rig.set_temp(Some(90.0));
        rig.tick();
        assert!(rig.engine.request_calibration("pwm1".into()).is_err());
    }

    #[test]
    fn a_missing_safety_sensor_names_the_fallback_when_refusing_calibration() {
        let mut rig = calibration_rig(&[], false, false);
        let mut config = rig.engine.config().clone();
        config.safety.critical_enabled = false;
        rig.engine.set_config(config);
        rig.readings.sensors[0].celsius = None;
        for _ in 0..3 {
            rig.tick();
        }
        let error = rig.engine.request_calibration("pwm1".into()).unwrap_err();
        assert_eq!(error, "the missing-sensor fallback is active");
    }

    #[test]
    fn calibration_still_aborts_at_75_degrees_with_the_override_off() {
        let mut rig = calibration_rig(&[("pwm1", manual(40.0))], false, false);
        let mut config = rig.engine.config().clone();
        config.safety.critical_enabled = false;
        rig.engine.set_config(config);
        rig.engine.request_calibration("pwm1".into()).unwrap();
        rig.spin(0.0, true);
        rig.spin(0.0, true);
        rig.set_temp(Some(95.0));
        rig.spin_until(0.0, |engine, _| !engine.is_calibrating());
        aborted(rig.report(), "reached");
    }

    #[test]
    fn a_full_run_completes_and_hands_over_its_result() {
        let mut rig = calibration_rig(&[("pwm1", manual(40.0))], true, true);
        rig.engine.request_calibration("pwm1".into()).unwrap();
        let mut lowest = 100.0f32;
        rig.spin_until(23.0, |engine, outcome| {
            lowest = lowest.min(output_of(outcome, "pwm1"));
            finished(engine, outcome)
        });
        assert_eq!(lowest, 0.0);
        let result = completed(rig.report());
        assert_eq!(result.stop_duty, Some(Duty::new(20.0)));
        assert_eq!(result.start_duty, Some(Duty::new(25.0)));
        let (fan, handed) = rig.engine.take_calibration_result().unwrap();
        assert_eq!(fan.as_str(), "pwm1");
        assert_eq!(handed, result);
        assert!(rig.engine.take_calibration_result().is_none());
    }

    #[test]
    fn critical_wins_in_the_same_tick_even_with_the_fan_stopped() {
        let mut rig = calibration_rig(&[("pwm1", manual(40.0))], true, true);
        rig.engine.request_calibration("pwm1".into()).unwrap();
        rig.spin_until(23.0, |_, outcome| output_of(outcome, "pwm1") == 0.0);
        rig.set_temp(Some(90.0));
        let outcome = rig.tick();
        let status = Rig::status(&outcome, "pwm1");
        assert_eq!(status.state, FanState::Critical);
        assert_eq!(status.output, Some(Duty::FULL));
        assert!(outcome.commands.contains(&Command {
            fan: "pwm1".into(),
            action: Action::SetDuty(Duty::FULL),
        }));
        aborted(rig.report(), "critical");
        assert!(!rig.engine.is_calibrating());
    }

    #[test]
    fn identify_monitor_only_and_faults_abort_the_run() {
        let start = || {
            let mut rig = calibration_rig(&[("pwm1", manual(40.0))], false, false);
            rig.engine.request_calibration("pwm1".into()).unwrap();
            rig.tick();
            rig
        };
        let mut rig = start();
        rig.engine.identify("pwm1".into(), Duration::from_secs(3));
        assert_eq!(
            Rig::status(&rig.tick(), "pwm1").state,
            FanState::Identifying
        );
        aborted(rig.report(), "Identify");

        let mut rig = start();
        rig.engine.set_monitor_only(true);
        assert_eq!(
            Rig::status(&rig.tick(), "pwm1").state,
            FanState::MonitorOnly
        );
        aborted(rig.report(), "monitor-only");

        let mut rig = start();
        for _ in 0..MAX_WRITE_MISMATCHES {
            rig.engine.record_apply_error(&"pwm1".into(), "boom");
        }
        assert!(matches!(
            Rig::status(&rig.tick(), "pwm1").state,
            FanState::Faulted(_)
        ));
        aborted(rig.report(), "faulted");
    }

    #[test]
    fn recovery_runs_at_full_speed_then_restores_the_previous_control() {
        let mut rig = calibration_rig(&[("pwm1", manual(40.0))], false, false);
        rig.engine.request_calibration("pwm1".into()).unwrap();
        for _ in 0..4 {
            rig.tick();
        }
        assert_eq!(output_of(&rig.tick(), "pwm1"), 95.0);
        rig.engine.cancel_calibration(None);
        assert_eq!(rig.report(), CalibrationOutcome::Cancelled);
        rig.engine.set_config(rig.engine.config().clone());
        for _ in 0..3 {
            let status = Rig::status(&rig.tick(), "pwm1");
            assert_eq!(status.state, FanState::Calibrating);
            assert_eq!(status.output, Some(Duty::FULL));
        }
        let status = Rig::status(&rig.tick(), "pwm1");
        assert_eq!(status.state, FanState::Controlled);
        assert_eq!(status.output, Some(Duty::new(40.0)));
    }

    #[test]
    fn a_curve_fan_ramps_down_from_full_speed_after_recovery() {
        let mut config = calibration_config(
            &[(
                "pwm1",
                FanMode::Curve {
                    curve: "cpu".into(),
                },
            )],
            false,
            false,
        );
        config.curves[0].tuning.step_down_per_sec = 10.0;
        let mut rig = Rig::new(config, FanLimits::default());
        assert_eq!(output_of(&rig.tick(), "pwm1"), 40.0);
        rig.engine.request_calibration("pwm1".into()).unwrap();
        for _ in 0..4 {
            rig.tick();
        }
        rig.engine.cancel_calibration(None);
        for _ in 0..3 {
            assert_eq!(output_of(&rig.tick(), "pwm1"), 100.0);
        }
        let status = Rig::status(&rig.tick(), "pwm1");
        assert_eq!(status.state, FanState::Controlled);
        assert_eq!(status.output, Some(Duty::new(90.0)));
        assert_eq!(output_of(&rig.tick(), "pwm1"), 80.0);
    }

    #[test]
    fn a_bios_fan_is_released_after_recovery() {
        let mut rig = calibration_rig(&[], false, false);
        rig.engine.request_calibration("pwm1".into()).unwrap();
        rig.tick();
        rig.engine
            .cancel_calibration(Some("the configuration changed".to_owned()));
        for _ in 0..3 {
            assert_eq!(output_of(&rig.tick(), "pwm1"), 100.0);
        }
        let outcome = rig.tick();
        assert_eq!(Rig::status(&outcome, "pwm1").state, FanState::Released);
        assert_eq!(outcome.commands[0].action, Action::Release);
    }

    #[test]
    fn a_stalled_cpu_fan_is_back_at_full_speed_in_the_same_tick() {
        let mut rig = calibration_rig(&[("pwm1", manual(30.0))], false, false);
        rig.engine.request_calibration("pwm1".into()).unwrap();
        let outcome = rig.spin_until(38.0, finished);
        assert_eq!(output_of(&outcome, "pwm1"), 100.0);
        assert_eq!(completed(rig.report()).stop_duty, Some(Duty::new(35.0)));
    }

    #[test]
    fn the_probe_ignores_an_older_calibrated_raise() {
        let mut config = calibration_config(&[("pwm1", manual(30.0))], false, false);
        config.calibration.insert(
            "pwm1".into(),
            FanCalibration {
                stop_duty: Some(Duty::new(50.0)),
                ..FanCalibration::default()
            },
        );
        let mut rig = Rig::new(config, FanLimits::default());
        assert_eq!(output_of(&rig.tick(), "pwm1"), 60.0);
        rig.engine.request_calibration("pwm1".into()).unwrap();
        let mut lowest = 100.0f32;
        rig.spin_until(0.0, |engine, outcome| {
            lowest = lowest.min(output_of(outcome, "pwm1"));
            finished(engine, outcome)
        });
        assert_eq!(lowest, 30.0);
        let result = completed(rig.report());
        let last = result.points.last().unwrap();
        assert_eq!(last.duty, Duty::new(30.0));
        assert_eq!(last.rpm, 450);
    }

    #[test]
    fn a_clamped_probe_aborts_the_run() {
        let mut rig = calibration_rig(&[("pwm1", manual(40.0))], false, false);
        rig.engine.request_calibration("pwm1".into()).unwrap();
        rig.spin_until(0.0, |_, outcome| output_of(outcome, "pwm1") == 85.0);
        let raised = FanLimits {
            floor: Duty::new(80.0),
            stop_permitted: false,
        };
        rig.engine
            .set_limits([("pwm1".into(), raised), ("pwm2".into(), raised)].into());
        let mut lowest = 100.0f32;
        rig.spin_until(0.0, |engine, outcome| {
            lowest = lowest.min(output_of(outcome, "pwm1"));
            finished(engine, outcome)
        });
        assert!(lowest >= 80.0);
        aborted(rig.report(), "clamp");
    }

    #[test]
    fn a_follower_keeps_its_own_floor_and_joins_the_recovery() {
        let mut rig = calibration_rig(
            &[
                ("pwm1", manual(40.0)),
                ("pwm2", FanMode::Sync { fan: "pwm1".into() }),
            ],
            true,
            true,
        );
        rig.engine.request_calibration("pwm1".into()).unwrap();
        let outcome = rig.spin_until(23.0, |_, outcome| output_of(outcome, "pwm1") == 0.0);
        assert_eq!(output_of(&outcome, "pwm2"), 30.0);
        rig.engine.cancel_calibration(None);
        let outcome = rig.tick();
        assert_eq!(output_of(&outcome, "pwm1"), 100.0);
        assert_eq!(output_of(&outcome, "pwm2"), 100.0);
    }

    fn partial_after(old: Option<f32>) -> FanCalibration {
        let mut config = calibration_config(&[("pwm1", manual(40.0))], true, true);
        if let Some(stop) = old {
            config.calibration.insert(
                "pwm1".into(),
                FanCalibration {
                    stop_duty: Some(Duty::new(stop)),
                    start_duty: Some(Duty::new(50.0)),
                    ..FanCalibration::default()
                },
            );
        }
        let mut rig = Rig::new(config, FanLimits::default());
        rig.engine.request_calibration("pwm1".into()).unwrap();
        rig.spin_until(23.0, |engine, _| {
            engine
                .calibration_view()
                .active
                .is_some_and(|progress| progress.phase == Phase::FindStart)
        });
        rig.engine.cancel_calibration(Some("test".to_owned()));
        rig.engine.take_calibration_result().unwrap().1
    }

    #[test]
    fn a_partial_result_only_raises_the_stored_stop_duty() {
        let fresh = partial_after(None);
        assert_eq!(fresh.stop_duty, Some(Duty::new(20.0)));
        assert_eq!(fresh.start_duty, None);
        let higher = partial_after(Some(40.0));
        assert_eq!(higher.stop_duty, Some(Duty::new(40.0)));
        assert_eq!(higher.start_duty, Some(Duty::new(50.0)));
        let lower = partial_after(Some(10.0));
        assert_eq!(lower.stop_duty, Some(Duty::new(20.0)));
        assert_eq!(lower.start_duty, Some(Duty::new(50.0)));
    }

    #[test]
    fn a_fan_that_does_not_restart_stays_flagged_until_a_good_run() {
        let mut rig = calibration_rig(&[("pwm1", manual(40.0))], true, true);
        rig.engine.request_calibration("pwm1".into()).unwrap();
        rig.tick();
        while rig.engine.is_calibrating() {
            rig.spin(23.0, false);
        }
        assert_eq!(rig.report(), CalibrationOutcome::NotRestarting);
        let (fan, flagged) = rig.engine.take_calibration_result().unwrap();
        assert!(flagged.not_restarting);
        assert_eq!(flagged.stop_duty, Some(Duty::new(20.0)));
        rig.engine.set_calibration(fan.clone(), flagged);
        assert_eq!(rig.engine.calibration_view().not_restarting, vec![fan]);
        rig.readings.fans[0].rpm = Some(1500);
        rig.engine.request_calibration("pwm1".into()).unwrap();
        rig.tick();
        rig.spin_until(23.0, finished);
        completed(rig.report());
        let (fan, good) = rig.engine.take_calibration_result().unwrap();
        assert!(!good.not_restarting);
        rig.engine.set_calibration(fan, good);
        assert!(rig.engine.calibration_view().not_restarting.is_empty());
    }

    #[test]
    fn a_fan_on_a_failing_sensor_is_not_calibrated() {
        let mut rig = calibration_rig(
            &[(
                "pwm1",
                FanMode::Curve {
                    curve: "cpu".into(),
                },
            )],
            false,
            false,
        );
        rig.tick();
        rig.set_curve_temp(None);
        rig.tick();
        rig.engine.request_calibration("pwm1".into()).unwrap();
        let status = Rig::status(&rig.tick(), "pwm1");
        assert!(matches!(status.state, FanState::Holding(_)));
        aborted(rig.report(), "sensor controlling this fan is failing");
    }

    #[test]
    fn an_abort_never_leaves_a_stale_clamp_for_the_next_run() {
        let mut rig = calibration_rig(&[("pwm1", manual(40.0))], false, false);
        rig.engine.request_calibration("pwm1".into()).unwrap();
        rig.spin_until(0.0, |_, outcome| output_of(outcome, "pwm1") == 85.0);
        let raised = FanLimits {
            floor: Duty::new(80.0),
            stop_permitted: false,
        };
        rig.engine
            .set_limits([("pwm1".into(), raised), ("pwm2".into(), raised)].into());
        let outcome = rig.spin_until(0.0, |engine, _| {
            engine
                .calibration_view()
                .active
                .is_some_and(|progress| progress.duty == Duty::new(75.0))
        });
        assert_eq!(output_of(&outcome, "pwm1"), 80.0);
        rig.engine.set_monitor_only(true);
        rig.tick();
        aborted(rig.report(), "monitor-only");
        rig.engine.set_monitor_only(false);
        rig.engine.set_limits(
            [
                ("pwm1".into(), FanLimits::default()),
                ("pwm2".into(), FanLimits::default()),
            ]
            .into(),
        );
        for _ in 0..4 {
            rig.tick();
        }
        rig.engine.request_calibration("pwm1".into()).unwrap();
        rig.tick();
        rig.tick();
        assert!(rig.engine.is_calibrating());
    }

    #[test]
    fn a_calibrated_floor_raises_the_clamp_and_the_spin_up() {
        let mut config = calibration_config(&[("pwm1", manual(0.0))], true, true);
        config.calibration.insert(
            "pwm1".into(),
            FanCalibration {
                stop_duty: Some(Duty::new(45.0)),
                start_duty: Some(Duty::new(60.0)),
                ..FanCalibration::default()
            },
        );
        let mut rig = Rig::new(config.clone(), FanLimits::default());
        assert_eq!(output_of(&rig.tick(), "pwm1"), 0.0);
        config
            .profiles
            .get_mut("Balanced")
            .unwrap()
            .fans
            .insert("pwm1".into(), manual(25.0));
        rig.engine.set_config(config);
        assert_eq!(output_of(&rig.tick(), "pwm1"), 70.0);
        rig.tick();
        let status = Rig::status(&rig.tick(), "pwm1");
        assert_eq!(status.output, Some(Duty::new(55.0)));
        assert_eq!(status.state, FanState::Clamped);
    }
}
