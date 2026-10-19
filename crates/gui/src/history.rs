use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fanforge_core::state::ControlState;
use fanforge_core::{FanId, SensorId};

pub const WINDOW: Duration = Duration::from_secs(300);
const MIN_SPAN: Duration = Duration::from_secs(60);

type Series = VecDeque<(Instant, f32)>;

#[derive(Default)]
pub struct History {
    last: Option<Arc<ControlState>>,
    temperatures: BTreeMap<SensorId, Series>,
    rpm: BTreeMap<FanId, Series>,
}

fn push(series: &mut Series, at: Instant, value: f32) {
    series.push_back((at, value));
}

fn evict(series: &mut Series, now: Instant) {
    while series
        .front()
        .is_some_and(|(at, _)| now.saturating_duration_since(*at) > WINDOW)
    {
        series.pop_front();
    }
}

fn normalised(series: Option<&Series>, now: Instant) -> Vec<(f32, f32)> {
    let Some(series) = series else {
        return Vec::new();
    };
    let oldest = series
        .front()
        .map_or(Duration::ZERO, |(at, _)| now.saturating_duration_since(*at));
    let window = oldest.clamp(MIN_SPAN, WINDOW).as_secs_f32();
    series
        .iter()
        .map(|(at, value)| {
            let age = now.saturating_duration_since(*at).as_secs_f32();
            (1.0 - age / window, *value)
        })
        .collect()
}

impl History {
    pub fn record(&mut self, state: &Arc<ControlState>, now: Instant) {
        if self
            .last
            .as_ref()
            .is_some_and(|last| Arc::ptr_eq(last, state))
        {
            return;
        }
        self.last = Some(Arc::clone(state));
        for sensor in &state.readings.sensors {
            if let Some(celsius) = sensor.celsius {
                push(
                    self.temperatures.entry(sensor.id.clone()).or_default(),
                    now,
                    celsius,
                );
            }
        }
        for fan in &state.readings.fans {
            if let Some(rpm) = fan.rpm {
                push(self.rpm.entry(fan.id.clone()).or_default(), now, rpm as f32);
            }
        }
        for series in self.temperatures.values_mut().chain(self.rpm.values_mut()) {
            evict(series, now);
        }
        self.temperatures.retain(|_, series| !series.is_empty());
        self.rpm.retain(|_, series| !series.is_empty());
    }

    pub fn temperature(&self, sensor: &SensorId, now: Instant) -> Vec<(f32, f32)> {
        normalised(self.temperatures.get(sensor), now)
    }

    pub fn rpm(&self, fan: &FanId, now: Instant) -> Vec<(f32, f32)> {
        normalised(self.rpm.get(fan), now)
    }

    #[cfg(test)]
    fn len(&self, sensor: &SensorId) -> usize {
        self.temperatures.get(sensor).map_or(0, VecDeque::len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;

    fn cpu() -> SensorId {
        SensorId::new("cpu/tctl")
    }

    #[test]
    fn the_same_snapshot_is_recorded_once() {
        let mut history = History::default();
        let state = Arc::new(fixtures::state());
        let start = Instant::now();
        history.record(&state, start);
        history.record(&state, start + Duration::from_secs(2));
        assert_eq!(history.len(&cpu()), 1);
        history.record(&Arc::new(fixtures::state()), start + Duration::from_secs(4));
        assert_eq!(history.len(&cpu()), 2);
    }

    #[test]
    fn samples_older_than_the_window_are_evicted() {
        let mut history = History::default();
        let start = Instant::now();
        history.record(&Arc::new(fixtures::state()), start);
        history.record(
            &Arc::new(fixtures::state()),
            start + WINDOW + Duration::from_secs(1),
        );
        assert_eq!(history.len(&cpu()), 1);
    }

    #[test]
    fn points_are_placed_by_age_not_by_count() {
        let mut history = History::default();
        let start = Instant::now();
        history.record(&Arc::new(fixtures::state()), start);
        let now = start + WINDOW / 2;
        history.record(&Arc::new(fixtures::state()), now);
        let points = history.temperature(&cpu(), now);
        assert_eq!(points.len(), 2);
        assert!(points[0].0.abs() < 1e-3);
        assert!((points[1].0 - 1.0).abs() < 1e-3);
    }

    #[test]
    fn a_short_history_spans_at_least_a_minute() {
        let mut history = History::default();
        let start = Instant::now();
        history.record(&Arc::new(fixtures::state()), start);
        let now = start + MIN_SPAN / 2;
        history.record(&Arc::new(fixtures::state()), now);
        let points = history.temperature(&cpu(), now);
        assert!((points[0].0 - 0.5).abs() < 1e-3);
    }

    #[test]
    fn missing_readings_leave_gaps_instead_of_zeros() {
        let mut history = History::default();
        let mut state = fixtures::state();
        state.readings.sensors[0].celsius = None;
        history.record(&Arc::new(state), Instant::now());
        assert_eq!(history.len(&cpu()), 0);
    }
}
