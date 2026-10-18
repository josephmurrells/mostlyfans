use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::board::FanLimits;
use crate::config::{Config, FanMode};
use crate::curve::{CurveEvaluator, HysteresisState, Tuning, find_curve};
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
    Critical,
    Identifying,
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
    fault: Option<String>,
}

#[derive(Clone, Debug)]
enum Target {
    Release,
    Immediate(f32),
    Smoothed { duty: f32, tuning: Tuning },
    Follow(FanId),
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
        let limits = self.limits.get(fan).copied().unwrap_or_default();
        let settings = self.config.settings(fan);
        let failure_limit = self.config.safety.sensor_failure_ticks;
        let runtime = self.fans.entry(fan.clone()).or_default();

        let override_state =
            override_state.or_else(|| runtime.fault.clone().map(FanState::Faulted));
        let (state, wanted) = match override_state {
            Some(state @ (FanState::MonitorOnly | FanState::Faulted(_))) => (state, None),
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

        let floor = limits.floor.percent().max(settings.min_duty.percent());
        let may_stop = limits.stop_permitted && settings.allow_stop;
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
            output = output.max(settings.spin_up_duty.percent());
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
}
