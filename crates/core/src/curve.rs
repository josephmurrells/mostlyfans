use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::telemetry::Readings;
use crate::{CurveId, SensorId};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CurvePoint {
    pub temp: f32,
    pub duty: f32,
}

impl CurvePoint {
    pub const fn new(temp: f32, duty: f32) -> Self {
        Self { temp, duty }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MixOp {
    Max,
    Min,
    Average,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CurveKind {
    Graph {
        sensor: SensorId,
        points: Vec<CurvePoint>,
    },
    Linear {
        sensor: SensorId,
        start: CurvePoint,
        end: CurvePoint,
    },
    Flat {
        duty: f32,
    },
    Mix {
        op: MixOp,
        curves: Vec<CurveId>,
    },
}

impl CurveKind {
    pub fn sensor(&self) -> Option<&SensorId> {
        match self {
            Self::Graph { sensor, .. } | Self::Linear { sensor, .. } => Some(sensor),
            Self::Flat { .. } | Self::Mix { .. } => None,
        }
    }

    pub fn references(&self) -> &[CurveId] {
        match self {
            Self::Mix { curves, .. } => curves,
            _ => &[],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tuning {
    pub hysteresis_c: f32,
    pub response_secs: f32,
    pub step_up_per_sec: f32,
    pub step_down_per_sec: f32,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            hysteresis_c: 2.0,
            response_secs: 2.0,
            step_up_per_sec: 25.0,
            step_down_per_sec: 8.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CurveDef {
    pub id: CurveId,
    pub name: String,
    #[serde(flatten)]
    pub kind: CurveKind,
    #[serde(default)]
    pub tuning: Tuning,
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum CurveError {
    #[error("sensor `{0}` is unavailable")]
    SensorUnavailable(SensorId),
    #[error("curve `{0}` does not exist")]
    UnknownCurve(CurveId),
    #[error("curve `{0}` references itself")]
    Cycle(CurveId),
    #[error("curve `{0}` has no points or inputs")]
    Empty(CurveId),
}

pub fn interpolate(points: &[CurvePoint], temp: f32) -> Option<f32> {
    let first = points.first()?;
    let last = points.last()?;
    if temp <= first.temp {
        return Some(first.duty);
    }
    if temp >= last.temp {
        return Some(last.duty);
    }
    points.windows(2).find_map(|pair| {
        let (low, high) = (pair[0], pair[1]);
        (temp >= low.temp && temp <= high.temp).then(|| {
            let span = high.temp - low.temp;
            if span <= f32::EPSILON {
                high.duty
            } else {
                low.duty + (high.duty - low.duty) * (temp - low.temp) / span
            }
        })
    })
}

pub fn sorted_points(points: &[CurvePoint]) -> Vec<CurvePoint> {
    let mut sorted = points.to_vec();
    sorted.sort_by(|a, b| a.temp.total_cmp(&b.temp));
    sorted
}

#[derive(Debug, Default)]
pub struct HysteresisState {
    effective: HashMap<CurveId, f32>,
}

impl HysteresisState {
    pub fn apply(&mut self, curve: &CurveId, temp: f32, hysteresis: f32) -> f32 {
        let effective = self.effective.entry(curve.clone()).or_insert(temp);
        if temp > *effective || temp <= *effective - hysteresis.max(0.0) {
            *effective = temp;
        }
        *effective
    }

    pub fn retain(&mut self, curves: &[CurveDef]) {
        self.effective
            .retain(|id, _| curves.iter().any(|curve| &curve.id == id));
    }
}

pub fn find_curve<'a>(curves: &'a [CurveDef], id: &CurveId) -> Option<&'a CurveDef> {
    curves.iter().find(|curve| &curve.id == id)
}

pub struct CurveEvaluator<'a> {
    curves: &'a [CurveDef],
    readings: &'a Readings,
    hysteresis: &'a mut HysteresisState,
    cache: HashMap<CurveId, Result<f32, CurveError>>,
    visiting: HashSet<CurveId>,
}

impl<'a> CurveEvaluator<'a> {
    pub fn new(
        curves: &'a [CurveDef],
        readings: &'a Readings,
        hysteresis: &'a mut HysteresisState,
    ) -> Self {
        Self {
            curves,
            readings,
            hysteresis,
            cache: HashMap::new(),
            visiting: HashSet::new(),
        }
    }

    pub fn evaluate(&mut self, id: &CurveId) -> Result<f32, CurveError> {
        if let Some(result) = self.cache.get(id) {
            return result.clone();
        }
        if !self.visiting.insert(id.clone()) {
            return Err(CurveError::Cycle(id.clone()));
        }
        let result = self.compute(id).map(|duty| duty.clamp(0.0, 100.0));
        self.visiting.remove(id);
        self.cache.insert(id.clone(), result.clone());
        result
    }

    fn compute(&mut self, id: &CurveId) -> Result<f32, CurveError> {
        let curve =
            find_curve(self.curves, id).ok_or_else(|| CurveError::UnknownCurve(id.clone()))?;
        match &curve.kind {
            CurveKind::Graph { sensor, points } => {
                let temp = self.input(curve, sensor)?;
                interpolate(&sorted_points(points), temp)
                    .ok_or_else(|| CurveError::Empty(id.clone()))
            }
            CurveKind::Linear { sensor, start, end } => {
                let temp = self.input(curve, sensor)?;
                interpolate(&sorted_points(&[*start, *end]), temp)
                    .ok_or_else(|| CurveError::Empty(id.clone()))
            }
            CurveKind::Flat { duty } => Ok(*duty),
            CurveKind::Mix { op, curves } => {
                let values = curves
                    .iter()
                    .map(|child| self.evaluate(child))
                    .collect::<Result<Vec<_>, _>>()?;
                mix(*op, &values).ok_or_else(|| CurveError::Empty(id.clone()))
            }
        }
    }

    fn input(&mut self, curve: &CurveDef, sensor: &SensorId) -> Result<f32, CurveError> {
        let temp = self
            .readings
            .temperature(sensor)
            .ok_or_else(|| CurveError::SensorUnavailable(sensor.clone()))?;
        Ok(self
            .hysteresis
            .apply(&curve.id, temp, curve.tuning.hysteresis_c))
    }
}

fn mix(op: MixOp, values: &[f32]) -> Option<f32> {
    if values.is_empty() {
        return None;
    }
    Some(match op {
        MixOp::Max => values.iter().copied().fold(f32::MIN, f32::max),
        MixOp::Min => values.iter().copied().fold(f32::MAX, f32::min),
        MixOp::Average => values.iter().sum::<f32>() / values.len() as f32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::SensorReading;

    fn readings(temp: f32) -> Readings {
        Readings {
            sensors: vec![SensorReading {
                id: "cpu".into(),
                label: "CPU".into(),
                celsius: Some(temp),
            }],
            fans: Vec::new(),
        }
    }

    fn graph(id: &str, points: Vec<CurvePoint>, hysteresis_c: f32) -> CurveDef {
        CurveDef {
            id: id.into(),
            name: id.into(),
            kind: CurveKind::Graph {
                sensor: "cpu".into(),
                points,
            },
            tuning: Tuning {
                hysteresis_c,
                ..Tuning::default()
            },
        }
    }

    #[test]
    fn interpolates_between_and_clamps_outside_points() {
        let points = [CurvePoint::new(30.0, 20.0), CurvePoint::new(70.0, 100.0)];
        assert_eq!(interpolate(&points, 10.0), Some(20.0));
        assert_eq!(interpolate(&points, 50.0), Some(60.0));
        assert_eq!(interpolate(&points, 90.0), Some(100.0));
        assert_eq!(interpolate(&[], 50.0), None);
    }

    #[test]
    fn unsorted_points_are_sorted_before_use() {
        let curves = [graph(
            "g",
            vec![CurvePoint::new(70.0, 100.0), CurvePoint::new(30.0, 20.0)],
            0.0,
        )];
        let data = readings(50.0);
        let mut state = HysteresisState::default();
        let mut evaluator = CurveEvaluator::new(&curves, &data, &mut state);
        assert_eq!(evaluator.evaluate(&"g".into()), Ok(60.0));
    }

    #[test]
    fn hysteresis_delays_only_falling_temperatures() {
        let mut state = HysteresisState::default();
        let id = CurveId::from("g");
        assert_eq!(state.apply(&id, 50.0, 3.0), 50.0);
        assert_eq!(state.apply(&id, 52.0, 3.0), 52.0);
        assert_eq!(state.apply(&id, 50.0, 3.0), 52.0);
        assert_eq!(state.apply(&id, 49.0, 3.0), 49.0);
    }

    #[test]
    fn mix_combines_children() {
        let curves = [
            graph("a", vec![CurvePoint::new(0.0, 40.0)], 0.0),
            CurveDef {
                id: "flat".into(),
                name: "flat".into(),
                kind: CurveKind::Flat { duty: 60.0 },
                tuning: Tuning::default(),
            },
            CurveDef {
                id: "max".into(),
                name: "max".into(),
                kind: CurveKind::Mix {
                    op: MixOp::Max,
                    curves: vec!["a".into(), "flat".into()],
                },
                tuning: Tuning::default(),
            },
            CurveDef {
                id: "avg".into(),
                name: "avg".into(),
                kind: CurveKind::Mix {
                    op: MixOp::Average,
                    curves: vec!["a".into(), "flat".into()],
                },
                tuning: Tuning::default(),
            },
        ];
        let data = readings(50.0);
        let mut state = HysteresisState::default();
        let mut evaluator = CurveEvaluator::new(&curves, &data, &mut state);
        assert_eq!(evaluator.evaluate(&"max".into()), Ok(60.0));
        assert_eq!(evaluator.evaluate(&"avg".into()), Ok(50.0));
    }

    #[test]
    fn cycles_and_missing_inputs_are_errors() {
        let curves = [
            CurveDef {
                id: "a".into(),
                name: "a".into(),
                kind: CurveKind::Mix {
                    op: MixOp::Max,
                    curves: vec!["b".into()],
                },
                tuning: Tuning::default(),
            },
            CurveDef {
                id: "b".into(),
                name: "b".into(),
                kind: CurveKind::Mix {
                    op: MixOp::Max,
                    curves: vec!["a".into()],
                },
                tuning: Tuning::default(),
            },
            graph("g", vec![CurvePoint::new(0.0, 40.0)], 0.0),
        ];
        let data = Readings::default();
        let mut state = HysteresisState::default();
        let mut evaluator = CurveEvaluator::new(&curves, &data, &mut state);
        assert!(matches!(
            evaluator.evaluate(&"a".into()),
            Err(CurveError::Cycle(_))
        ));
        assert_eq!(
            evaluator.evaluate(&"g".into()),
            Err(CurveError::SensorUnavailable("cpu".into()))
        );
        assert_eq!(
            evaluator.evaluate(&"nope".into()),
            Err(CurveError::UnknownCurve("nope".into()))
        );
    }
}
