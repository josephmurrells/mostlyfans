use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::board::FanLimits;
use crate::config::FanSettings;
use crate::telemetry::Readings;
use crate::{Duty, FanId, SensorId};

pub const STEP: f32 = 5.0;
pub const MIN_STEP_TIME: Duration = Duration::from_secs(3);
pub const MAX_STEP_TIME: Duration = Duration::from_secs(20);
pub const SETTLE_SAMPLES: usize = 3;
pub const SETTLE_TOLERANCE: f32 = 0.03;
pub const SETTLE_TOLERANCE_RPM: u32 = 25;
pub const STOPPED_BELOW_RPM: u32 = 100;
pub const MAX_TEMP_RISE: f32 = 5.0;
pub const MAX_TEMP: f32 = 75.0;
pub const MAX_TOTAL_TIME: Duration = Duration::from_secs(20 * 60);
pub const MARGIN: f32 = 10.0;
pub const RECOVER_TIME: Duration = Duration::from_secs(3);
pub const MAX_POINTS: usize = 24;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationPoint {
    pub duty: Duty,
    pub rpm: u32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FanCalibration {
    pub stop_duty: Option<Duty>,
    pub start_duty: Option<Duty>,
    pub lowest_tested: Duty,
    pub points: Vec<CalibrationPoint>,
}

impl FanCalibration {
    pub fn floor(&self) -> Option<Duty> {
        self.stop_duty
            .map(|duty| Duty::new(duty.percent() + MARGIN))
    }

    pub fn spin_up(&self) -> Option<Duty> {
        self.start_duty
            .map(|duty| Duty::new(duty.percent() + MARGIN))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Floor {
    pub percent: f32,
    pub may_stop: bool,
    pub calibrated: bool,
}

pub fn header_floor(limits: FanLimits, settings: &FanSettings) -> Floor {
    let limits = limits.for_header(settings.case_fan);
    Floor {
        percent: limits.floor.percent().max(settings.min_duty.percent()),
        may_stop: limits.stop_permitted && settings.allow_stop,
        calibrated: false,
    }
}

pub fn effective_floor(
    limits: FanLimits,
    settings: &FanSettings,
    calibration: Option<&FanCalibration>,
) -> Floor {
    let header = header_floor(limits, settings);
    match calibration.and_then(FanCalibration::floor) {
        Some(raised) if raised.percent() > header.percent => Floor {
            percent: raised.percent(),
            calibrated: true,
            ..header
        },
        _ => header,
    }
}

pub fn effective_spin_up(settings: &FanSettings, calibration: Option<&FanCalibration>) -> f32 {
    let configured = settings.spin_up_duty.percent();
    calibration
        .and_then(FanCalibration::spin_up)
        .map_or(configured, |raised| configured.max(raised.percent()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    StepDown,
    Rest,
    FindStart,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationProgress {
    pub fan: FanId,
    pub phase: Phase,
    pub duty: Duty,
    pub rpm: Option<u32>,
    pub points: Vec<CalibrationPoint>,
    pub elapsed: Duration,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "detail", rename_all = "snake_case")]
pub enum CalibrationOutcome {
    Completed(FanCalibration),
    Aborted(String),
    NotRestarting,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CalibrationReport {
    pub fan: FanId,
    pub outcome: CalibrationOutcome,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CalibrationView {
    pub active: Option<CalibrationProgress>,
    pub last: Option<CalibrationReport>,
    pub not_restarting: Vec<FanId>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Hold(Duty),
    Done(CalibrationOutcome),
}

#[derive(Clone, Debug)]
struct Watched {
    id: SensorId,
    label: String,
    start: f32,
}

#[derive(Debug)]
pub struct Calibration {
    fan: FanId,
    lowest: f32,
    may_stop: bool,
    phase: Phase,
    duty: f32,
    primed: bool,
    step_elapsed: Duration,
    step_samples: usize,
    total: Duration,
    samples: Vec<u32>,
    last_rpm: Option<u32>,
    points: Vec<CalibrationPoint>,
    stop_duty: Option<f32>,
    lowest_tested: f32,
    watched: Vec<Watched>,
}

impl Calibration {
    pub fn start(
        fan: FanId,
        readings: &Readings,
        limits: FanLimits,
        settings: &FanSettings,
        critical_sensor: &SensorId,
    ) -> Result<Self, String> {
        let reading = readings
            .fan(&fan)
            .ok_or_else(|| format!("fan `{fan}` does not exist"))?;
        if reading.rpm.is_none() {
            return Err("the fan has no RPM signal".to_owned());
        }
        if readings.temperature(critical_sensor).is_none() {
            return Err(format!(
                "the critical sensor `{critical_sensor}` is not reporting"
            ));
        }
        let watched = readings
            .sensors
            .iter()
            .filter_map(|sensor| {
                readings.temperature(&sensor.id).map(|start| Watched {
                    id: sensor.id.clone(),
                    label: sensor.label.clone(),
                    start,
                })
            })
            .collect::<Vec<_>>();
        if !watched.iter().any(|sensor| &sensor.id != critical_sensor) {
            return Err("no temperature sensor besides the critical one is reporting".to_owned());
        }
        if let Some(hot) = watched.iter().find(|sensor| sensor.start >= MAX_TEMP) {
            return Err(format!("{} is already at {:.0} Â°C", hot.label, hot.start));
        }
        let floor = header_floor(limits, settings);
        Ok(Self {
            fan,
            lowest: if floor.may_stop { 0.0 } else { floor.percent },
            may_stop: floor.may_stop,
            phase: Phase::StepDown,
            duty: 100.0,
            primed: false,
            step_elapsed: Duration::ZERO,
            step_samples: 0,
            total: Duration::ZERO,
            samples: Vec::new(),
            last_rpm: reading.rpm,
            points: Vec::new(),
            stop_duty: None,
            lowest_tested: 100.0,
            watched,
        })
    }

    pub fn fan(&self) -> &FanId {
        &self.fan
    }

    pub fn duty(&self) -> Duty {
        Duty::new(self.duty)
    }

    pub fn stop_duty(&self) -> Option<Duty> {
        self.stop_duty.map(Duty::new)
    }

    pub fn partial(&self) -> Option<FanCalibration> {
        self.stop_duty.map(|stop| FanCalibration {
            stop_duty: Some(Duty::new(stop)),
            start_duty: None,
            lowest_tested: Duty::new(self.lowest_tested),
            points: self.points.clone(),
        })
    }

    pub fn progress(&self) -> CalibrationProgress {
        CalibrationProgress {
            fan: self.fan.clone(),
            phase: self.phase,
            duty: self.duty(),
            rpm: self.last_rpm,
            points: self.points.clone(),
            elapsed: self.total,
        }
    }

    pub fn advance(&mut self, readings: &Readings, elapsed: Duration) -> Step {
        if let Err(reason) = self.check(readings) {
            return Step::Done(CalibrationOutcome::Aborted(reason));
        }
        let rpm = readings.fan(&self.fan).and_then(|fan| fan.rpm);
        self.last_rpm = rpm;
        if !self.primed {
            self.primed = true;
            return Step::Hold(self.duty());
        }
        self.total += elapsed;
        if self.total > MAX_TOTAL_TIME {
            return Step::Done(CalibrationOutcome::Aborted(
                "calibration took longer than 20 minutes".to_owned(),
            ));
        }
        self.step_elapsed += elapsed;
        if let Some(rpm) = rpm {
            self.samples.push(rpm);
            self.step_samples += 1;
            if self.samples.len() > SETTLE_SAMPLES {
                self.samples.remove(0);
            }
            if !self.may_stop && rpm < STOPPED_BELOW_RPM && self.duty < 100.0 {
                self.stop_duty = Some(self.duty);
                self.lowest_tested = self.lowest_tested.min(self.duty);
                return self.complete(None);
            }
        }
        if self.phase == Phase::Rest {
            return self.rest();
        }
        let Some(rpm) = self.settled() else {
            return Step::Hold(self.duty());
        };
        self.next_step();
        match self.phase {
            Phase::StepDown => self.step_down(rpm),
            Phase::Rest => Step::Hold(self.duty()),
            Phase::FindStart => self.find_start(rpm),
        }
    }

    fn check(&self, readings: &Readings) -> Result<(), String> {
        let fan = readings
            .fan(&self.fan)
            .ok_or_else(|| "the fan disappeared".to_owned())?;
        if fan.rpm.is_none() {
            return Err("the fan lost its RPM signal".to_owned());
        }
        for sensor in &self.watched {
            let Some(now) = readings.temperature(&sensor.id) else {
                return Err(format!("{} stopped reporting", sensor.label));
            };
            if now >= MAX_TEMP {
                return Err(format!("{} reached {now:.0} Â°C", sensor.label));
            }
            if now - sensor.start > MAX_TEMP_RISE {
                return Err(format!(
                    "{} rose by {:.1} Â°C",
                    sensor.label,
                    now - sensor.start
                ));
            }
        }
        Ok(())
    }

    fn next_step(&mut self) {
        self.samples.clear();
        self.step_samples = 0;
        self.step_elapsed = Duration::ZERO;
    }

    fn settled(&self) -> Option<u32> {
        let last = *self.samples.last()?;
        if self.step_samples < SETTLE_SAMPLES {
            return None;
        }
        if self.step_elapsed >= MAX_STEP_TIME {
            return Some(last);
        }
        if self.step_elapsed < MIN_STEP_TIME {
            return None;
        }
        let high = self.samples.iter().copied().max().unwrap_or(last);
        let low = self.samples.iter().copied().min().unwrap_or(last);
        let tolerance = SETTLE_TOLERANCE_RPM.max((high as f32 * SETTLE_TOLERANCE) as u32);
        (high - low <= tolerance).then_some(last)
    }

    fn step_down(&mut self, rpm: u32) -> Step {
        self.lowest_tested = self.lowest_tested.min(self.duty);
        if rpm < STOPPED_BELOW_RPM {
            if self.duty >= 100.0 {
                return Step::Done(CalibrationOutcome::Aborted(
                    "the fan does not spin at 100%".to_owned(),
                ));
            }
            self.stop_duty = Some(self.duty);
            if !self.may_stop {
                return self.complete(None);
            }
            self.phase = Phase::Rest;
            self.duty = 0.0;
            return Step::Hold(self.duty());
        }
        if self.points.len() < MAX_POINTS {
            self.points.push(CalibrationPoint {
                duty: self.duty(),
                rpm,
            });
        }
        if self.duty <= self.lowest {
            return self.complete(None);
        }
        self.duty = (self.duty - STEP).max(self.lowest);
        Step::Hold(self.duty())
    }

    fn rest(&mut self) -> Step {
        if self.step_samples < SETTLE_SAMPLES {
            return Step::Hold(self.duty());
        }
        let stopped = self.samples.iter().all(|rpm| *rpm < STOPPED_BELOW_RPM);
        if !stopped || self.step_elapsed < MIN_STEP_TIME {
            if self.step_elapsed >= MAX_STEP_TIME {
                return Step::Done(CalibrationOutcome::Aborted(
                    "the fan did not stop at 0%".to_owned(),
                ));
            }
            return Step::Hold(self.duty());
        }
        self.next_step();
        self.phase = Phase::FindStart;
        self.duty = (self.stop_duty.unwrap_or(0.0) + STEP).min(100.0);
        Step::Hold(self.duty())
    }

    fn find_start(&mut self, rpm: u32) -> Step {
        if rpm >= STOPPED_BELOW_RPM {
            return self.complete(Some(self.duty));
        }
        if self.duty >= 100.0 {
            return Step::Done(CalibrationOutcome::NotRestarting);
        }
        self.duty = (self.duty + STEP).min(100.0);
        Step::Hold(self.duty())
    }

    fn complete(&self, start_duty: Option<f32>) -> Step {
        Step::Done(CalibrationOutcome::Completed(FanCalibration {
            stop_duty: self.stop_duty.map(Duty::new),
            start_duty: start_duty.map(Duty::new),
            lowest_tested: Duty::new(self.lowest_tested),
            points: self.points.clone(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::{FanReading, HardwareMode, SensorReading};

    const SECOND: Duration = Duration::from_secs(1);

    struct SimFan {
        stops_below: f32,
        starts_at: f32,
        spinning: bool,
        rpm: Option<u32>,
    }

    impl SimFan {
        fn new(stops_below: f32, starts_at: f32) -> Self {
            Self {
                stops_below,
                starts_at,
                spinning: true,
                rpm: Some(1500),
            }
        }

        fn drive(&mut self, duty: f32) {
            if self.spinning && duty < self.stops_below {
                self.spinning = false;
            } else if !self.spinning && duty >= self.starts_at {
                self.spinning = true;
            }
            self.rpm = Some(if self.spinning {
                (duty * 15.0) as u32
            } else {
                0
            });
        }
    }

    fn readings(rpm: Option<u32>) -> Readings {
        Readings {
            sensors: vec![
                SensorReading {
                    id: "cpu/tctl".into(),
                    label: "Tctl".into(),
                    celsius: Some(50.0),
                },
                SensorReading {
                    id: "board/temp1".into(),
                    label: "Board".into(),
                    celsius: Some(40.0),
                },
            ],
            fans: vec![FanReading {
                id: "pwm1".into(),
                label: "Fan 1".into(),
                rpm,
                mode: HardwareMode::Auto,
                duty: None,
            }],
        }
    }

    fn case_fan(allow_stop: bool) -> FanSettings {
        FanSettings {
            case_fan: true,
            allow_stop,
            ..FanSettings::default()
        }
    }

    fn start(settings: &FanSettings, readings: &Readings) -> Result<Calibration, String> {
        Calibration::start(
            "pwm1".into(),
            readings,
            FanLimits::default(),
            settings,
            &"cpu/tctl".into(),
        )
    }

    struct Run {
        outcome: CalibrationOutcome,
        duties: Vec<f32>,
    }

    impl Run {
        fn ticks_at(&self, duty: f32) -> usize {
            self.duties.iter().filter(|held| **held == duty).count()
        }
    }

    fn run(settings: FanSettings, mut fan: SimFan, interval: Duration) -> Run {
        let mut readings = readings(fan.rpm);
        let mut calibration = start(&settings, &readings).unwrap();
        let mut duties = Vec::new();
        for _ in 0..100_000 {
            match calibration.advance(&readings, interval) {
                Step::Hold(duty) => {
                    duties.push(duty.percent());
                    fan.drive(duty.percent());
                    readings.fans[0].rpm = fan.rpm;
                }
                Step::Done(outcome) => return Run { outcome, duties },
            }
        }
        panic!("calibration never finished");
    }

    fn completed(run: &Run) -> &FanCalibration {
        match &run.outcome {
            CalibrationOutcome::Completed(result) => result,
            other => panic!("expected a completed run, got {other:?}"),
        }
    }

    fn aborted_with(outcome: CalibrationOutcome, text: &str) {
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

    #[test]
    fn a_stoppable_fan_finds_its_stop_and_start_duty() {
        let run = run(case_fan(true), SimFan::new(23.0, 33.0), SECOND);
        let result = completed(&run);
        assert_eq!(result.stop_duty, Some(Duty::new(20.0)));
        assert_eq!(result.start_duty, Some(Duty::new(35.0)));
        assert_eq!(result.lowest_tested, Duty::new(20.0));
        assert_eq!(result.points.len(), 16);
        assert_eq!(result.points[0].duty, Duty::FULL);
        assert_eq!(result.points[0].rpm, 1500);
        assert_eq!(result.points[15].duty, Duty::new(25.0));
        assert!(run.ticks_at(0.0) >= 3);
        assert_eq!(result.floor(), Some(Duty::new(30.0)));
        assert_eq!(result.spin_up(), Some(Duty::new(45.0)));
    }

    #[test]
    fn a_fan_that_never_stops_finishes_at_its_floor() {
        let run = run(FanSettings::default(), SimFan::new(10.0, 15.0), SECOND);
        let result = completed(&run);
        assert_eq!(result.stop_duty, None);
        assert_eq!(result.floor(), None);
        assert_eq!(result.lowest_tested, Duty::new(30.0));
        assert_eq!(result.points.len(), 15);
        assert!(run.duties.iter().all(|duty| *duty >= 30.0));
    }

    #[test]
    fn a_case_fan_without_0_rpm_never_goes_below_its_floor() {
        let run = run(case_fan(false), SimFan::new(10.0, 15.0), SECOND);
        assert_eq!(completed(&run).lowest_tested, Duty::new(20.0));
        assert!(run.duties.iter().all(|duty| *duty >= 20.0));
    }

    #[test]
    fn a_cpu_fan_that_stalls_ends_on_the_first_stopped_sample() {
        let run = run(FanSettings::default(), SimFan::new(38.0, 50.0), SECOND);
        let result = completed(&run);
        assert_eq!(result.stop_duty, Some(Duty::new(35.0)));
        assert_eq!(result.start_duty, None);
        assert_eq!(run.ticks_at(35.0), 1);
        assert_eq!(result.floor(), Some(Duty::new(45.0)));
    }

    #[test]
    fn a_dead_fan_aborts_instead_of_storing_a_full_floor() {
        let mut fan = SimFan::new(200.0, 200.0);
        fan.spinning = false;
        fan.rpm = Some(0);
        let run = run(FanSettings::default(), fan, SECOND);
        aborted_with(run.outcome, "does not spin at 100%");
    }

    #[test]
    fn a_fan_that_does_not_restart_is_reported() {
        let run = run(case_fan(true), SimFan::new(23.0, 200.0), SECOND);
        assert_eq!(run.outcome, CalibrationOutcome::NotRestarting);
        assert_eq!(run.duties.last(), Some(&100.0));
    }

    #[test]
    fn each_step_waits_at_least_three_seconds_and_three_samples() {
        let fast = run(
            FanSettings::default(),
            SimFan::new(10.0, 15.0),
            Duration::from_millis(250),
        );
        assert_eq!(fast.ticks_at(50.0), 12);
        let slow = run(
            FanSettings::default(),
            SimFan::new(10.0, 15.0),
            Duration::from_secs(5),
        );
        assert_eq!(slow.ticks_at(50.0), 3);
        assert_eq!(completed(&slow).points.len(), 15);
    }

    #[test]
    fn an_unsettled_step_times_out_after_twenty_seconds() {
        let mut readings = readings(Some(1500));
        let mut calibration = start(&FanSettings::default(), &readings).unwrap();
        assert_eq!(
            calibration.advance(&readings, SECOND),
            Step::Hold(Duty::FULL)
        );
        for tick in 1..MAX_STEP_TIME.as_secs() {
            readings.fans[0].rpm = Some(if tick % 2 == 0 { 1500 } else { 1100 });
            assert_eq!(
                calibration.advance(&readings, SECOND),
                Step::Hold(Duty::FULL)
            );
        }
        assert_eq!(
            calibration.advance(&readings, SECOND),
            Step::Hold(Duty::new(95.0))
        );
    }

    #[test]
    fn the_first_tick_only_primes() {
        let readings = readings(Some(1500));
        let mut calibration = start(&FanSettings::default(), &readings).unwrap();
        calibration.advance(&readings, Duration::from_secs(60));
        assert_eq!(calibration.progress().elapsed, Duration::ZERO);
    }

    #[test]
    fn a_stale_sample_never_counts_for_the_new_duty() {
        let mut readings = readings(Some(1500));
        let mut calibration = start(&FanSettings::default(), &readings).unwrap();
        for _ in 0..3 {
            calibration.advance(&readings, SECOND);
        }
        assert_eq!(
            calibration.advance(&readings, SECOND),
            Step::Hold(Duty::new(95.0))
        );
        readings.fans[0].rpm = Some(1425);
        for _ in 0..2 {
            assert_eq!(
                calibration.advance(&readings, SECOND),
                Step::Hold(Duty::new(95.0))
            );
        }
        assert_eq!(
            calibration.advance(&readings, SECOND),
            Step::Hold(Duty::new(90.0))
        );
        assert_eq!(calibration.progress().points[1].rpm, 1425);
    }

    fn abort_after(change: impl FnOnce(&mut Readings)) -> CalibrationOutcome {
        let mut readings = readings(Some(1500));
        let mut calibration = start(&FanSettings::default(), &readings).unwrap();
        calibration.advance(&readings, SECOND);
        change(&mut readings);
        match calibration.advance(&readings, SECOND) {
            Step::Done(outcome) => outcome,
            Step::Hold(_) => panic!("calibration did not abort"),
        }
    }

    #[test]
    fn every_watched_condition_aborts() {
        aborted_with(
            abort_after(|readings| readings.sensors[1].celsius = Some(45.5)),
            "rose by 5.5",
        );
        aborted_with(
            abort_after(|readings| readings.sensors[0].celsius = Some(75.0)),
            "reached 75",
        );
        aborted_with(
            abort_after(|readings| readings.sensors[1].celsius = None),
            "Board stopped reporting",
        );
        aborted_with(
            abort_after(|readings| readings.sensors[0].celsius = Some(200.0)),
            "Tctl stopped reporting",
        );
        aborted_with(
            abort_after(|readings| readings.fans[0].rpm = None),
            "RPM signal",
        );
        aborted_with(abort_after(|readings| readings.fans.clear()), "disappeared");
    }

    #[test]
    fn a_rise_of_exactly_five_degrees_is_allowed() {
        let mut readings = readings(Some(1500));
        let mut calibration = start(&FanSettings::default(), &readings).unwrap();
        calibration.advance(&readings, SECOND);
        readings.sensors[1].celsius = Some(45.0);
        assert!(matches!(
            calibration.advance(&readings, SECOND),
            Step::Hold(_)
        ));
    }

    #[test]
    fn a_run_that_takes_too_long_aborts() {
        let readings = readings(Some(1500));
        let mut calibration = start(&FanSettings::default(), &readings).unwrap();
        calibration.advance(&readings, SECOND);
        match calibration.advance(&readings, MAX_TOTAL_TIME + SECOND) {
            Step::Done(outcome) => aborted_with(outcome, "20 minutes"),
            Step::Hold(_) => panic!("did not abort"),
        }
    }

    fn drive_with(
        settings: &FanSettings,
        mut fan: SimFan,
        mut during_rest: impl FnMut(&mut SimFan),
    ) -> (CalibrationOutcome, Calibration) {
        let mut readings = readings(fan.rpm);
        let mut calibration = start(settings, &readings).unwrap();
        loop {
            match calibration.advance(&readings, SECOND) {
                Step::Hold(duty) => {
                    fan.drive(duty.percent());
                    if calibration.progress().phase == Phase::Rest {
                        during_rest(&mut fan);
                    }
                    readings.fans[0].rpm = fan.rpm;
                }
                Step::Done(outcome) => return (outcome, calibration),
            }
        }
    }

    #[test]
    fn a_fan_that_keeps_spinning_at_zero_aborts_the_rest() {
        let (outcome, calibration) = drive_with(&case_fan(true), SimFan::new(23.0, 33.0), |fan| {
            fan.rpm = Some(400);
        });
        aborted_with(outcome, "did not stop at 0%");
        assert_eq!(calibration.stop_duty(), Some(Duty::new(20.0)));
    }

    #[test]
    fn a_coasting_fan_rests_until_it_has_stopped() {
        let mut coast = 900u32;
        let mut rest_ticks = 0;
        let (outcome, _) = drive_with(&case_fan(true), SimFan::new(23.0, 33.0), |fan| {
            rest_ticks += 1;
            coast /= 3;
            fan.rpm = Some(coast);
        });
        assert!(matches!(outcome, CalibrationOutcome::Completed(_)));
        assert_eq!(rest_ticks, 5);
    }

    #[test]
    fn a_partial_run_keeps_the_stop_duty() {
        let mut fan = SimFan::new(23.0, 33.0);
        let mut readings = readings(fan.rpm);
        let mut calibration = start(&case_fan(true), &readings).unwrap();
        assert_eq!(calibration.partial(), None);
        while calibration.progress().phase != Phase::FindStart {
            let Step::Hold(duty) = calibration.advance(&readings, SECOND) else {
                panic!("finished early");
            };
            fan.drive(duty.percent());
            readings.fans[0].rpm = fan.rpm;
        }
        let partial = calibration.partial().unwrap();
        assert_eq!(partial.stop_duty, Some(Duty::new(20.0)));
        assert_eq!(partial.start_duty, None);
    }

    #[test]
    fn unsafe_starts_are_refused() {
        let settings = FanSettings::default();
        assert!(start(&settings, &readings(None)).is_err());
        let mut no_critical = readings(Some(1500));
        no_critical.sensors[0].celsius = None;
        assert!(start(&settings, &no_critical).is_err());
        let mut only_critical = readings(Some(1500));
        only_critical.sensors.truncate(1);
        assert!(start(&settings, &only_critical).is_err());
        let mut hot = readings(Some(1500));
        hot.sensors[1].celsius = Some(76.0);
        assert!(start(&settings, &hot).is_err());
        let mut missing = readings(Some(1500));
        missing.fans.clear();
        assert!(start(&settings, &missing).is_err());
    }

    #[test]
    fn calibration_only_ever_raises_the_floor_and_spin_up() {
        let limits = FanLimits::default();
        let settings = FanSettings {
            min_duty: Duty::new(40.0),
            ..FanSettings::default()
        };
        let measured = |stop: f32, start: f32| FanCalibration {
            stop_duty: Some(Duty::new(stop)),
            start_duty: Some(Duty::new(start)),
            ..FanCalibration::default()
        };
        let low = measured(10.0, 10.0);
        assert_eq!(effective_floor(limits, &settings, Some(&low)).percent, 40.0);
        assert!(!effective_floor(limits, &settings, Some(&low)).calibrated);
        assert_eq!(effective_spin_up(&settings, Some(&low)), 50.0);
        let high = measured(45.0, 60.0);
        let raised = effective_floor(limits, &settings, Some(&high));
        assert_eq!(raised.percent, 55.0);
        assert!(raised.calibrated);
        assert_eq!(effective_spin_up(&settings, Some(&high)), 70.0);
        let full = measured(95.0, 95.0);
        assert_eq!(
            effective_floor(limits, &settings, Some(&full)).percent,
            100.0
        );
        assert_eq!(effective_floor(limits, &settings, None).percent, 40.0);
    }
}
