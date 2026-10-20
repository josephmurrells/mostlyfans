use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use fanforge_core::SensorId;
use fanforge_core::telemetry::SensorReading;

pub const STALE_CADENCES: u32 = 3;

pub type SourceIndex = usize;

#[derive(Debug)]
struct Source {
    stale_after: Duration,
    reported: bool,
}

#[derive(Debug)]
struct Entry {
    source: SourceIndex,
    label: String,
    celsius: Option<f32>,
    updated: Instant,
}

#[derive(Debug, Default)]
pub struct SensorCache {
    sources: Vec<Source>,
    sensors: BTreeMap<SensorId, Entry>,
}

impl SensorCache {
    pub fn register(&mut self, cadence: Duration) -> SourceIndex {
        self.sources.push(Source {
            stale_after: cadence * STALE_CADENCES,
            reported: false,
        });
        self.sources.len() - 1
    }

    pub fn record(
        &mut self,
        source: SourceIndex,
        result: &Result<Vec<SensorReading>, String>,
        now: Instant,
    ) {
        let Some(state) = self.sources.get_mut(source) else {
            return;
        };
        state.reported = true;
        for entry in self.sensors.values_mut() {
            if entry.source == source {
                entry.celsius = None;
            }
        }
        let Ok(readings) = result else {
            return;
        };
        for reading in readings {
            let entry = self.sensors.entry(reading.id.clone()).or_insert(Entry {
                source,
                label: reading.label.clone(),
                celsius: None,
                updated: now,
            });
            if entry.source != source {
                continue;
            }
            entry.label.clone_from(&reading.label);
            entry.celsius = reading.celsius;
            entry.updated = now;
        }
    }

    pub fn readings(&self, now: Instant) -> Vec<SensorReading> {
        self.sensors
            .iter()
            .map(|(id, entry)| {
                let fresh = self.sources.get(entry.source).is_some_and(|source| {
                    now.saturating_duration_since(entry.updated) <= source.stale_after
                });
                SensorReading {
                    id: id.clone(),
                    label: entry.label.clone(),
                    celsius: entry.celsius.filter(|_| fresh),
                }
            })
            .collect()
    }

    pub fn all_reported(&self) -> bool {
        self.sources.iter().all(|source| source.reported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CADENCE: Duration = Duration::from_secs(1);

    fn reading(id: &str, celsius: f32) -> SensorReading {
        SensorReading {
            id: SensorId::new(id),
            label: id.to_uppercase(),
            celsius: Some(celsius),
        }
    }

    fn values(cache: &SensorCache, now: Instant) -> Vec<(String, Option<f32>)> {
        cache
            .readings(now)
            .into_iter()
            .map(|reading| (reading.id.to_string(), reading.celsius))
            .collect()
    }

    #[test]
    fn fresh_values_are_reported_until_they_go_stale() {
        let start = Instant::now();
        let mut cache = SensorCache::default();
        let gpu = cache.register(CADENCE);
        cache.record(gpu, &Ok(vec![reading("gpu/a", 55.0)]), start);
        assert_eq!(values(&cache, start), vec![("gpu/a".into(), Some(55.0))]);
        assert_eq!(
            values(&cache, start + CADENCE * STALE_CADENCES),
            vec![("gpu/a".into(), Some(55.0))]
        );
        assert_eq!(
            values(
                &cache,
                start + CADENCE * STALE_CADENCES + Duration::from_millis(1)
            ),
            vec![("gpu/a".into(), None)]
        );
    }

    #[test]
    fn a_source_error_keeps_its_ids_and_fails_them_at_once() {
        let start = Instant::now();
        let mut cache = SensorCache::default();
        let gpu = cache.register(CADENCE);
        let drives = cache.register(CADENCE);
        cache.record(gpu, &Ok(vec![reading("gpu/a", 55.0)]), start);
        cache.record(drives, &Ok(vec![reading("drive/x", 35.0)]), start);
        cache.record(gpu, &Err("lost".into()), start);
        assert_eq!(
            values(&cache, start),
            vec![("drive/x".into(), Some(35.0)), ("gpu/a".into(), None)]
        );
        cache.record(gpu, &Ok(vec![reading("gpu/a", 57.0)]), start + CADENCE);
        assert_eq!(
            values(&cache, start + CADENCE)[1],
            ("gpu/a".into(), Some(57.0))
        );
    }

    #[test]
    fn a_device_missing_from_a_round_fails() {
        let start = Instant::now();
        let mut cache = SensorCache::default();
        let gpu = cache.register(CADENCE);
        cache.record(
            gpu,
            &Ok(vec![reading("gpu/a", 55.0), reading("gpu/b", 60.0)]),
            start,
        );
        cache.record(gpu, &Ok(vec![reading("gpu/b", 61.0)]), start);
        assert_eq!(
            values(&cache, start),
            vec![("gpu/a".into(), None), ("gpu/b".into(), Some(61.0))]
        );
    }

    #[test]
    fn one_source_cannot_overwrite_another_sources_sensor() {
        let start = Instant::now();
        let mut cache = SensorCache::default();
        let first = cache.register(CADENCE);
        let second = cache.register(CADENCE);
        cache.record(first, &Ok(vec![reading("gpu/a", 55.0)]), start);
        cache.record(second, &Ok(vec![reading("gpu/a", 99.0)]), start);
        assert_eq!(values(&cache, start), vec![("gpu/a".into(), Some(55.0))]);
    }

    #[test]
    fn every_source_must_report_before_the_first_round_is_done() {
        let start = Instant::now();
        let mut cache = SensorCache::default();
        assert!(cache.all_reported());
        let gpu = cache.register(CADENCE);
        let drives = cache.register(CADENCE);
        assert!(!cache.all_reported());
        cache.record(gpu, &Err("no driver".into()), start);
        assert!(!cache.all_reported());
        cache.record(drives, &Ok(Vec::new()), start);
        assert!(cache.all_reported());
    }
}
