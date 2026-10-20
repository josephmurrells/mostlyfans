mod cache;
pub mod drives;
pub mod nvidia;
pub mod radeon;

use std::collections::HashMap;
use std::panic::{self, AssertUnwindSafe};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use fanforge_core::telemetry::SensorReading;

pub use cache::{STALE_CADENCES, SensorCache, SourceIndex};

pub const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(30);
pub const FAILURES_BEFORE_BACKOFF: u32 = 3;
pub const PANICS_BEFORE_GIVING_UP: u32 = 3;

pub trait SensorSource: Send + 'static {
    fn name(&self) -> &'static str;
    fn cadence(&self) -> Duration;
    fn read(&mut self) -> Result<Vec<SensorReading>, String>;
}

pub fn default_sources() -> Vec<Box<dyn SensorSource>> {
    vec![
        Box::new(nvidia::NvidiaSource::default()),
        Box::new(radeon::RadeonSource::default()),
        Box::new(drives::DriveSource::default()),
    ]
}

#[derive(Debug, Default)]
struct Hub {
    cache: Mutex<SensorCache>,
    reported: Condvar,
}

impl Hub {
    fn lock(&self) -> MutexGuard<'_, SensorCache> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Clone, Debug, Default)]
pub struct ExtraSensors {
    hub: Arc<Hub>,
    _stop: Arc<Vec<Sender<()>>>,
}

impl ExtraSensors {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn from_cache(cache: SensorCache) -> Self {
        Self {
            hub: Arc::new(Hub {
                cache: Mutex::new(cache),
                reported: Condvar::new(),
            }),
            _stop: Arc::default(),
        }
    }

    pub fn start(sources: Vec<Box<dyn SensorSource>>) -> Self {
        let hub = Arc::new(Hub::default());
        let mut stops = Vec::new();
        for source in sources {
            let name = source.name();
            let index = hub.lock().register(source.cadence());
            let (stop, stopped) = mpsc::channel();
            let poller_hub = Arc::clone(&hub);
            let spawned = thread::Builder::new()
                .name(format!("sensors-{name}"))
                .spawn(move || poll(source, index, &poller_hub, &stopped));
            match spawned {
                Ok(_) => stops.push(stop),
                Err(error) => {
                    tracing::error!(source = name, %error, "could not start sensor poller");
                    hub.lock()
                        .record(index, &Err(error.to_string()), Instant::now());
                }
            }
        }
        Self {
            hub,
            _stop: Arc::new(stops),
        }
    }

    pub fn with_cache<T>(&self, update: impl FnOnce(&mut SensorCache) -> T) -> T {
        let result = update(&mut self.hub.lock());
        self.hub.reported.notify_all();
        result
    }

    pub fn readings(&self, now: Instant) -> Vec<SensorReading> {
        self.hub.lock().readings(now)
    }

    pub fn wait_for_first_round(&self, timeout: Duration) -> bool {
        let guard = self.hub.lock();
        let (guard, _) = self
            .hub
            .reported
            .wait_timeout_while(guard, timeout, |cache| !cache.all_reported())
            .unwrap_or_else(PoisonError::into_inner);
        guard.all_reported()
    }
}

#[derive(Debug, Default)]
struct Health {
    ever_ok: bool,
    failures: u32,
    panics: u32,
}

impl Health {
    fn record(&mut self, ok: bool, panicked: bool) {
        if ok {
            self.ever_ok = true;
            self.failures = 0;
        } else {
            self.failures += 1;
        }
        self.panics = if panicked { self.panics + 1 } else { 0 };
    }

    fn next_wait(&self, cadence: Duration) -> Duration {
        if self.failures > 0 && (!self.ever_ok || self.failures >= FAILURES_BEFORE_BACKOFF) {
            RETRY_AFTER_FAILURE.max(cadence)
        } else {
            cadence
        }
    }

    fn gave_up(&self) -> bool {
        self.panics >= PANICS_BEFORE_GIVING_UP
    }
}

fn poll(mut source: Box<dyn SensorSource>, index: SourceIndex, hub: &Hub, stop: &Receiver<()>) {
    let name = source.name();
    let cadence = source.cadence();
    let mut health = Health::default();
    loop {
        let (result, panicked) = match panic::catch_unwind(AssertUnwindSafe(|| source.read())) {
            Ok(result) => (result, false),
            Err(_) => (Err(format!("{name} sensor source panicked")), true),
        };
        let was_failing = health.failures > 0;
        health.record(result.is_ok(), panicked);
        match &result {
            Ok(_) if was_failing => tracing::info!(source = name, "sensor source recovered"),
            Err(error) if !was_failing && health.ever_ok => {
                tracing::warn!(source = name, %error, "sensor source failed");
            }
            Err(error) if !was_failing => {
                tracing::info!(source = name, %error, "sensor source unavailable");
            }
            _ => {}
        }
        hub.lock().record(index, &result, Instant::now());
        hub.reported.notify_all();
        if health.gave_up() {
            tracing::error!(source = name, "sensor source keeps panicking; stopping it");
            return;
        }
        match stop.recv_timeout(health.next_wait(cadence)) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

pub fn unique_labels(names: Vec<String>) -> Vec<String> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    names
        .into_iter()
        .map(|name| {
            let count = seen.entry(name.clone()).or_default();
            *count += 1;
            if *count == 1 {
                name
            } else {
                format!("{name} #{count}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fanforge_core::SensorId;
    use std::sync::atomic::{AtomicU32, Ordering};

    const FAST: Duration = Duration::from_millis(5);

    struct Scripted {
        calls: Arc<AtomicU32>,
        behaviour: fn(u32) -> Result<Vec<SensorReading>, String>,
    }

    impl SensorSource for Scripted {
        fn name(&self) -> &'static str {
            "scripted"
        }

        fn cadence(&self) -> Duration {
            FAST
        }

        fn read(&mut self) -> Result<Vec<SensorReading>, String> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            (self.behaviour)(call)
        }
    }

    fn scripted(
        behaviour: fn(u32) -> Result<Vec<SensorReading>, String>,
    ) -> (Box<Scripted>, Arc<AtomicU32>) {
        let calls = Arc::new(AtomicU32::new(0));
        (
            Box::new(Scripted {
                calls: Arc::clone(&calls),
                behaviour,
            }),
            calls,
        )
    }

    fn gpu(celsius: f32) -> Vec<SensorReading> {
        vec![SensorReading {
            id: SensorId::new("gpu/test"),
            label: "Test GPU".into(),
            celsius: Some(celsius),
        }]
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "condition not reached in time");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn pollers_publish_readings_and_signal_the_first_round() {
        let (source, calls) = scripted(|call| Ok(gpu(40.0 + call as f32)));
        let extras = ExtraSensors::start(vec![source]);
        assert!(extras.wait_for_first_round(Duration::from_secs(5)));
        assert_eq!(extras.readings(Instant::now()).len(), 1);
        wait_until(|| {
            extras.readings(Instant::now())[0]
                .celsius
                .is_some_and(|celsius| celsius >= 42.0)
        });
        assert!(calls.load(Ordering::SeqCst) >= 3);
    }

    #[test]
    fn a_panicking_source_is_caught_and_its_sensors_fail() {
        let (source, calls) = scripted(|call| {
            if call == 0 {
                Ok(gpu(50.0))
            } else {
                panic!("decoder bug")
            }
        });
        let extras = ExtraSensors::start(vec![source]);
        wait_until(|| calls.load(Ordering::SeqCst) >= 2);
        wait_until(|| extras.readings(Instant::now())[0].celsius.is_none());
        let readings = extras.readings(Instant::now());
        assert_eq!(readings[0].id.as_str(), "gpu/test");
    }

    #[test]
    fn dropping_the_last_handle_stops_the_pollers() {
        let (source, calls) = scripted(|_| Ok(gpu(50.0)));
        let extras = ExtraSensors::start(vec![source]);
        wait_until(|| calls.load(Ordering::SeqCst) >= 1);
        let copy = extras.clone();
        drop(extras);
        let still = calls.load(Ordering::SeqCst);
        wait_until(|| calls.load(Ordering::SeqCst) > still);
        drop(copy);
        thread::sleep(FAST * 4);
        let stopped = calls.load(Ordering::SeqCst);
        thread::sleep(FAST * 10);
        assert!(calls.load(Ordering::SeqCst) <= stopped + 1);
    }

    #[test]
    fn first_round_wait_times_out_without_reports() {
        let mut cache = SensorCache::default();
        cache.register(FAST);
        let extras = ExtraSensors::from_cache(cache);
        assert!(!extras.wait_for_first_round(Duration::from_millis(10)));
        extras.with_cache(|cache| cache.record(0, &Ok(gpu(30.0)), Instant::now()));
        assert!(extras.wait_for_first_round(Duration::from_millis(10)));
    }

    #[test]
    fn a_poisoned_cache_still_yields_readings() {
        let extras = ExtraSensors::from_cache(SensorCache::default());
        extras.with_cache(|cache| {
            let index = cache.register(Duration::from_secs(60));
            cache.record(index, &Ok(gpu(45.0)), Instant::now());
        });
        let poisoner = extras.clone();
        let result = thread::spawn(move || {
            poisoner.with_cache(|_| panic!("poison the cache"));
        })
        .join();
        assert!(result.is_err());
        assert_eq!(extras.readings(Instant::now())[0].celsius, Some(45.0));
    }

    #[test]
    fn backoff_only_applies_to_never_working_or_repeatedly_failing_sources() {
        let cadence = Duration::from_secs(1);
        let mut health = Health::default();
        assert_eq!(health.next_wait(cadence), cadence);
        health.record(false, false);
        assert_eq!(health.next_wait(cadence), RETRY_AFTER_FAILURE);
        health.record(true, false);
        assert_eq!(health.next_wait(cadence), cadence);
        health.record(false, false);
        health.record(false, false);
        assert_eq!(health.next_wait(cadence), cadence);
        health.record(false, false);
        assert_eq!(health.next_wait(cadence), RETRY_AFTER_FAILURE);
    }

    #[test]
    fn consecutive_panics_give_up() {
        let mut health = Health::default();
        health.record(false, true);
        health.record(false, true);
        health.record(true, false);
        health.record(false, true);
        health.record(false, true);
        assert!(!health.gave_up());
        health.record(false, true);
        assert!(health.gave_up());
    }

    #[test]
    fn duplicate_labels_are_numbered() {
        let labels = unique_labels(vec!["RTX".into(), "RX".into(), "RTX".into(), "RTX".into()]);
        assert_eq!(labels, vec!["RTX", "RX", "RTX #2", "RTX #3"]);
    }
}
