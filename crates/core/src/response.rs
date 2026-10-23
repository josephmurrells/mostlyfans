use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use crate::calibration::{
    SETTLE_SAMPLES, SETTLE_TOLERANCE, SETTLE_TOLERANCE_RPM, STOPPED_BELOW_RPM,
};
use crate::engine::SPIN_UP_DURATION;

pub const STALL_TIME: Duration = Duration::from_secs(10);
pub const SETTLE_TIME: Duration = Duration::from_secs(8);
pub const SAMPLE_LIFETIME: Duration = Duration::from_secs(30 * 60);
pub const MIN_TICKS: u32 = 3;
pub const SPREAD: f32 = 30.0;
pub const LOW_DUTY_MAX: f32 = 50.0;
pub const FLAT_RATIO: f32 = 1.10;
pub const NO_RESPONSE: &str = "its speed doesn't follow the duty MostlyFans sets (a 3-pin fan on a PWM header, a pump, or a mismatched header)";
const HOLD_TOLERANCE: f32 = 2.0;
const BAND: f32 = 10.0;

pub fn stalled(duty: f32) -> String {
    format!("the fan stopped at {duty:.0}%; raise its minimum or spin-up duty, or calibrate it")
}

pub fn flat(low: (f32, u32), high: (f32, u32)) -> bool {
    let ((low_duty, low_rpm), (high_duty, high_rpm)) = (low, high);
    low_duty <= LOW_DUTY_MAX
        && high_duty >= low_duty + SPREAD
        && low_rpm >= STOPPED_BELOW_RPM
        && high_rpm >= STOPPED_BELOW_RPM
        && (high_rpm as f32) < low_rpm as f32 * FLAT_RATIO
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Observation {
    Reset,
    Pause,
    Drive { output: f32, rpm: u32 },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    Fine,
    Kick,
    Fault(String),
}

#[derive(Clone, Copy, Debug)]
struct Sample {
    duty: f32,
    rpm: u32,
    age: Duration,
}

#[derive(Clone, Copy, Debug)]
struct Timer {
    time: Duration,
    ticks: u32,
}

impl Timer {
    fn new() -> Self {
        Self {
            time: Duration::ZERO,
            ticks: 0,
        }
    }

    fn advance(&mut self, elapsed: Duration) {
        self.time += elapsed;
        self.ticks += 1;
    }

    fn reached(&self, time: Duration) -> bool {
        self.time >= time && self.ticks >= MIN_TICKS
    }
}

#[derive(Clone, Copy, Debug)]
struct Kick {
    duty: f32,
    timer: Timer,
}

#[derive(Debug, Default)]
pub struct Tracker {
    spinning: u32,
    stall: Option<Timer>,
    kick: Option<Kick>,
    held: Option<(f32, Timer)>,
    recent: VecDeque<u32>,
    samples: BTreeMap<u8, Sample>,
}

impl Tracker {
    pub fn restart(&mut self) {
        *self = Self {
            spinning: self.spinning,
            samples: std::mem::take(&mut self.samples),
            ..Self::default()
        };
    }

    pub fn observe(&mut self, observation: Observation, elapsed: Duration) -> Verdict {
        if observation == Observation::Reset {
            *self = Self::default();
            return Verdict::Fine;
        }
        if let Some(kick) = &mut self.kick {
            if observation == Observation::Pause {
                return Verdict::Kick;
            }
            kick.timer.advance(elapsed);
            if kick.timer.time >= SPIN_UP_DURATION {
                let duty = kick.duty;
                *self = Self::default();
                return Verdict::Fault(stalled(duty));
            }
            return Verdict::Kick;
        }
        self.age(elapsed);
        let Observation::Drive { output, rpm } = observation else {
            self.held = None;
            self.recent.clear();
            return Verdict::Fine;
        };
        if rpm >= STOPPED_BELOW_RPM {
            self.spinning = (self.spinning + 1).min(SETTLE_SAMPLES as u32);
        } else if !self.seen_spinning() {
            self.spinning = 0;
        }
        if !self.seen_spinning() {
            return Verdict::Fine;
        }
        if output > 0.0 && rpm < STOPPED_BELOW_RPM {
            let stall = self.stall.get_or_insert_with(Timer::new);
            stall.advance(elapsed);
            if stall.reached(STALL_TIME) {
                self.kick = Some(Kick {
                    duty: output,
                    timer: Timer::new(),
                });
                return Verdict::Kick;
            }
        } else {
            self.stall = None;
        }
        self.sample(output, rpm, elapsed)
    }

    fn seen_spinning(&self) -> bool {
        self.spinning >= SETTLE_SAMPLES as u32
    }

    fn age(&mut self, elapsed: Duration) {
        for sample in self.samples.values_mut() {
            sample.age += elapsed;
        }
        self.samples
            .retain(|_, sample| sample.age < SAMPLE_LIFETIME);
    }

    fn sample(&mut self, output: f32, rpm: u32, elapsed: Duration) -> Verdict {
        match &mut self.held {
            Some((duty, timer)) if (*duty - output).abs() <= HOLD_TOLERANCE => {
                timer.advance(elapsed);
            }
            _ => {
                self.held = Some((output, Timer::new()));
                self.recent.clear();
            }
        }
        self.recent.push_back(rpm);
        if self.recent.len() > SETTLE_SAMPLES {
            self.recent.pop_front();
        }
        let Some((duty, timer)) = self.held else {
            return Verdict::Fine;
        };
        if output <= 0.0 || !timer.reached(SETTLE_TIME) || !self.steady() {
            return Verdict::Fine;
        }
        let mean = self.recent.iter().sum::<u32>() / self.recent.len() as u32;
        if mean < STOPPED_BELOW_RPM {
            return Verdict::Fine;
        }
        self.samples.insert(
            (duty / BAND) as u8,
            Sample {
                duty,
                rpm: mean,
                age: Duration::ZERO,
            },
        );
        let samples: Vec<Sample> = self.samples.values().copied().collect();
        let unresponsive = samples.iter().any(|low| {
            samples
                .iter()
                .any(|high| flat((low.duty, low.rpm), (high.duty, high.rpm)))
        });
        if unresponsive {
            *self = Self::default();
            Verdict::Fault(NO_RESPONSE.to_owned())
        } else {
            Verdict::Fine
        }
    }

    fn steady(&self) -> bool {
        if self.recent.len() < SETTLE_SAMPLES {
            return false;
        }
        let high = self.recent.iter().copied().max().unwrap_or(0);
        let low = self.recent.iter().copied().min().unwrap_or(0);
        high - low <= SETTLE_TOLERANCE_RPM.max((high as f32 * SETTLE_TOLERANCE) as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK: Duration = Duration::from_secs(1);

    fn drive(tracker: &mut Tracker, output: f32, rpm: u32, ticks: u32) -> Verdict {
        let mut verdict = Verdict::Fine;
        for _ in 0..ticks {
            verdict = tracker.observe(Observation::Drive { output, rpm }, TICK);
            if verdict != Verdict::Fine {
                return verdict;
            }
        }
        verdict
    }

    #[test]
    fn a_stalled_fan_is_kicked_then_faulted() {
        let mut tracker = Tracker::default();
        assert_eq!(drive(&mut tracker, 30.0, 900, 5), Verdict::Fine);
        assert_eq!(drive(&mut tracker, 30.0, 0, 9), Verdict::Fine);
        assert_eq!(drive(&mut tracker, 30.0, 0, 1), Verdict::Kick);
        assert_eq!(
            tracker.observe(
                Observation::Drive {
                    output: 100.0,
                    rpm: 0
                },
                TICK
            ),
            Verdict::Kick
        );
        assert_eq!(tracker.observe(Observation::Pause, TICK), Verdict::Kick);
        assert_eq!(
            tracker.observe(
                Observation::Drive {
                    output: 100.0,
                    rpm: 0
                },
                TICK
            ),
            Verdict::Fault(stalled(30.0))
        );
    }

    #[test]
    fn a_header_that_never_spun_never_trips() {
        let mut tracker = Tracker::default();
        assert_eq!(drive(&mut tracker, 30.0, 0, 120), Verdict::Fine);
        for _ in 0..40 {
            assert_eq!(drive(&mut tracker, 30.0, 0, 3), Verdict::Fine);
            assert_eq!(drive(&mut tracker, 30.0, 400, 1), Verdict::Fine);
        }
    }

    #[test]
    fn a_missing_reading_pauses_the_stall_timer() {
        let mut tracker = Tracker::default();
        drive(&mut tracker, 30.0, 900, 5);
        assert_eq!(drive(&mut tracker, 30.0, 0, 6), Verdict::Fine);
        for _ in 0..30 {
            assert_eq!(tracker.observe(Observation::Pause, TICK), Verdict::Fine);
        }
        assert_eq!(drive(&mut tracker, 30.0, 0, 3), Verdict::Fine);
        assert_eq!(drive(&mut tracker, 30.0, 0, 1), Verdict::Kick);
    }

    #[test]
    fn a_fan_that_starts_again_resets_the_stall_timer() {
        let mut tracker = Tracker::default();
        drive(&mut tracker, 30.0, 900, 5);
        for _ in 0..10 {
            assert_eq!(drive(&mut tracker, 30.0, 0, 8), Verdict::Fine);
            assert_eq!(drive(&mut tracker, 30.0, 700, 1), Verdict::Fine);
        }
    }

    #[test]
    fn a_flat_response_faults() {
        let mut tracker = Tracker::default();
        assert_eq!(drive(&mut tracker, 30.0, 1500, 12), Verdict::Fine);
        assert_eq!(
            drive(&mut tracker, 80.0, 1520, 10),
            Verdict::Fault(NO_RESPONSE.to_owned())
        );
    }

    #[test]
    fn an_inverted_response_faults() {
        let mut tracker = Tracker::default();
        drive(&mut tracker, 40.0, 1400, 12);
        assert_eq!(
            drive(&mut tracker, 90.0, 600, 10),
            Verdict::Fault(NO_RESPONSE.to_owned())
        );
    }

    #[test]
    fn a_fan_that_saturates_near_the_top_is_fine() {
        let mut tracker = Tracker::default();
        for (duty, rpm) in [(30.0, 600), (50.0, 1000), (70.0, 1480), (100.0, 1500)] {
            assert_eq!(drive(&mut tracker, duty, rpm, 12), Verdict::Fine);
        }
    }

    #[test]
    fn a_responsive_fan_is_fine() {
        let mut tracker = Tracker::default();
        for _ in 0..5 {
            for (duty, rpm) in [(30.0, 600), (80.0, 1400), (45.0, 850)] {
                assert_eq!(drive(&mut tracker, duty, rpm, 12), Verdict::Fine);
            }
        }
    }

    #[test]
    fn samples_need_a_steady_rpm() {
        let mut tracker = Tracker::default();
        drive(&mut tracker, 30.0, 1500, 12);
        for _ in 0..10 {
            for rpm in [1200, 1500, 1800] {
                assert_eq!(drive(&mut tracker, 80.0, rpm, 1), Verdict::Fine);
            }
        }
    }

    #[test]
    fn old_samples_expire() {
        let mut tracker = Tracker::default();
        drive(&mut tracker, 30.0, 1500, 12);
        tracker.observe(Observation::Pause, SAMPLE_LIFETIME);
        assert_eq!(drive(&mut tracker, 80.0, 1500, 10), Verdict::Fine);
    }

    #[test]
    fn slow_ticks_still_need_three_readings() {
        let mut tracker = Tracker::default();
        let slow = Duration::from_secs(5);
        for _ in 0..3 {
            tracker.observe(
                Observation::Drive {
                    output: 30.0,
                    rpm: 900,
                },
                slow,
            );
        }
        assert_eq!(
            tracker.observe(
                Observation::Drive {
                    output: 30.0,
                    rpm: 0
                },
                slow
            ),
            Verdict::Fine
        );
        assert_eq!(
            tracker.observe(
                Observation::Drive {
                    output: 30.0,
                    rpm: 0
                },
                slow
            ),
            Verdict::Fine
        );
        assert_eq!(
            tracker.observe(
                Observation::Drive {
                    output: 30.0,
                    rpm: 0
                },
                slow
            ),
            Verdict::Kick
        );
    }

    #[test]
    fn a_restart_keeps_the_samples_and_forgets_the_stall() {
        let mut tracker = Tracker::default();
        drive(&mut tracker, 30.0, 1500, 12);
        drive(&mut tracker, 30.0, 0, 8);
        tracker.restart();
        assert_eq!(drive(&mut tracker, 30.0, 0, 9), Verdict::Fine);
        tracker.restart();
        assert_eq!(
            drive(&mut tracker, 80.0, 1500, 10),
            Verdict::Fault(NO_RESPONSE.to_owned())
        );
    }

    #[test]
    fn a_reset_forgets_everything() {
        let mut tracker = Tracker::default();
        drive(&mut tracker, 30.0, 1500, 12);
        tracker.observe(Observation::Reset, TICK);
        assert_eq!(drive(&mut tracker, 80.0, 1500, 10), Verdict::Fine);
    }
}
