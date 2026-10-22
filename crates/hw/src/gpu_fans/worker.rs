use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::mem;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use fanforge_core::board::FanLimits;
use fanforge_core::engine::Action;
use fanforge_core::state::GpuFanInfo;
use fanforge_core::telemetry::{FanReading, HardwareMode};
use fanforge_core::{Duty, FanId};

use super::ownership::{self, Loaded, Ownership};
use super::{DriverOpener, FOREIGN, FanDriver, GpuDesc, locate, writes};

pub const NOT_RESPONDING: &str = "GPU driver not responding";
const POLL: Duration = Duration::from_secs(1);
const REOPEN_AFTER: Duration = Duration::from_secs(10);
const MIN_STALE: Duration = Duration::from_secs(3);
const STALE_INTERVALS: u32 = 3;

#[derive(Clone, Debug)]
struct FanSnapshot {
    id: FanId,
    gpu: String,
    index: u32,
    minimum: Option<u32>,
    rpm: Option<u32>,
    manual: bool,
    target: Option<u32>,
    foreign: bool,
    error: Option<String>,
}

#[derive(Debug, Default)]
struct State {
    requests: BTreeMap<FanId, Action>,
    in_flight: BTreeMap<FanId, Action>,
    release_requested: u64,
    release_done: u64,
    release_error: Option<String>,
    fans: Vec<FanSnapshot>,
    sampled_at: Option<Instant>,
    error: Option<String>,
    written: BTreeMap<FanId, u32>,
    errors: Vec<(FanId, String)>,
    owned: BTreeSet<FanId>,
    stop: bool,
}

#[derive(Debug, Default)]
struct Inner {
    state: Mutex<State>,
    wake: Condvar,
    done: Condvar,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct GpuReadings {
    pub fans: Vec<FanReading>,
    pub limits: BTreeMap<FanId, FanLimits>,
    pub info: Vec<GpuFanInfo>,
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct GpuFans {
    inner: Arc<Inner>,
}

impl GpuFans {
    pub fn start(opener: DriverOpener, path: PathBuf, halt: Arc<AtomicBool>) -> io::Result<Self> {
        let inner = Arc::new(Inner::default());
        let (owned, lost) = match ownership::load(&path) {
            Loaded::Owned(ownership) => (ownership, false),
            Loaded::Lost => (Ownership::default(), true),
        };
        inner.lock().owned = owned.fans.clone();
        let worker = Worker {
            inner: Arc::clone(&inner),
            opener,
            path,
            halt,
            driver: None,
            gpus: Vec::new(),
            next_open: Instant::now(),
            started: false,
            owned,
            lost,
            written: BTreeMap::new(),
        };
        thread::Builder::new()
            .name("gpu-fans".into())
            .spawn(move || worker.run())?;
        Ok(Self { inner })
    }

    pub fn request(&self, fan: &FanId, action: Action) {
        self.inner.lock().requests.insert(fan.clone(), action);
        self.inner.wake.notify_all();
    }

    pub fn take_errors(&self) -> Vec<(FanId, String)> {
        mem::take(&mut self.inner.lock().errors)
    }

    pub fn owns_any(&self) -> bool {
        !self.inner.lock().owned.is_empty()
    }

    pub fn readings(&self, now: Instant, interval: Duration) -> GpuReadings {
        let state = self.inner.lock();
        let stale_after = (interval * STALE_INTERVALS).max(MIN_STALE);
        let stale = state
            .sampled_at
            .is_none_or(|at| now.saturating_duration_since(at) > stale_after);
        let mut readings = GpuReadings {
            error: state.error.clone(),
            ..GpuReadings::default()
        };
        for fan in &state.fans {
            let unavailable = if stale {
                Some(NOT_RESPONDING.to_owned())
            } else if fan.foreign {
                Some(FOREIGN.to_owned())
            } else {
                fan.error.clone()
            };
            let manual = !stale && fan.manual;
            let duty = fan.target.filter(|_| manual).map(|target| {
                let pending = match state
                    .requests
                    .get(&fan.id)
                    .or_else(|| state.in_flight.get(&fan.id))
                {
                    Some(Action::SetDuty(duty)) => Some(*duty),
                    _ => None,
                };
                match pending {
                    Some(duty) if state.written.get(&fan.id) == Some(&target) => duty,
                    _ => Duty::new(target as f32),
                }
            });
            readings.fans.push(FanReading {
                id: fan.id.clone(),
                label: format!("{} fan {}", fan.gpu, fan.index + 1),
                rpm: fan.rpm.filter(|_| !stale),
                mode: if manual {
                    HardwareMode::Manual
                } else {
                    HardwareMode::Auto
                },
                duty,
                unavailable,
            });
            if let Some(minimum) = fan.minimum {
                readings.limits.insert(
                    fan.id.clone(),
                    FanLimits::for_gpu(Duty::new(minimum as f32)),
                );
            }
            readings.info.push(GpuFanInfo {
                id: fan.id.clone(),
                gpu: fan.gpu.clone(),
                index: fan.index,
                minimum: fan.minimum.map(|minimum| Duty::new(minimum as f32)),
            });
        }
        readings
    }

    pub fn release(&self, budget: Duration) -> Result<(), String> {
        let deadline = Instant::now() + budget;
        let mut state = self.inner.lock();
        state.requests.clear();
        state.in_flight.clear();
        state.release_requested += 1;
        let round = state.release_requested;
        self.inner.wake.notify_all();
        while state.release_done < round {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(NOT_RESPONDING.to_owned());
            }
            state = self
                .inner
                .done
                .wait_timeout(state, remaining)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        state.release_error.clone().map_or(Ok(()), Err)
    }
}

impl Drop for GpuFans {
    fn drop(&mut self) {
        self.inner.lock().stop = true;
        self.inner.wake.notify_all();
    }
}

struct Worker {
    inner: Arc<Inner>,
    opener: DriverOpener,
    path: PathBuf,
    halt: Arc<AtomicBool>,
    driver: Option<Box<dyn FanDriver>>,
    gpus: Vec<GpuDesc>,
    next_open: Instant,
    started: bool,
    owned: Ownership,
    lost: bool,
    written: BTreeMap<FanId, u32>,
}

impl Worker {
    fn run(mut self) {
        loop {
            let (requests, release) = {
                let mut state = self.inner.lock();
                let idle =
                    state.requests.is_empty() && state.release_done == state.release_requested;
                if idle && !state.stop {
                    state = self
                        .inner
                        .wake
                        .wait_timeout(state, POLL)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
                if state.stop {
                    return;
                }
                let release = (state.release_done < state.release_requested)
                    .then_some(state.release_requested);
                let requests = mem::take(&mut state.requests);
                let requests = if release.is_some() {
                    BTreeMap::new()
                } else {
                    requests
                };
                state.in_flight = requests.clone();
                (requests, release)
            };
            let round = panic::catch_unwind(AssertUnwindSafe(|| self.round(requests, release)));
            self.inner.lock().in_flight.clear();
            if round.is_err() {
                tracing::error!("the GPU fan driver panicked; reopening it later");
                self.close("the GPU driver panicked".to_owned());
                if let Some(round) = release {
                    self.finish_release(round, Err("the GPU driver panicked".to_owned()));
                }
            }
        }
    }

    fn round(&mut self, requests: BTreeMap<FanId, Action>, release: Option<u64>) {
        self.ensure_open();
        if let Some(round) = release {
            let result = self.release_owned();
            self.finish_release(round, result);
        }
        for (fan, action) in requests {
            self.apply(&fan, action);
        }
        self.sample();
    }

    fn close(&mut self, error: String) {
        self.driver = None;
        self.next_open = Instant::now() + REOPEN_AFTER;
        self.inner.lock().error = Some(error);
    }

    fn ensure_open(&mut self) {
        if self.driver.is_some() || Instant::now() < self.next_open {
            return;
        }
        match (self.opener)() {
            Ok(driver) => {
                self.gpus = driver.gpus();
                self.driver = Some(driver);
                self.inner.lock().error = None;
                if !self.started {
                    self.started = true;
                    self.release_leftovers();
                }
            }
            Err(error) => {
                if self.inner.lock().error.as_ref() != Some(&error) {
                    tracing::info!(%error, "GPU fan control unavailable");
                }
                self.close(error);
            }
        }
    }

    fn release_leftovers(&mut self) {
        let _writes = writes();
        let Some(driver) = &self.driver else {
            return;
        };
        let mut changed = self.lost;
        for gpu in &self.gpus {
            for index in 0..gpu.fans {
                let fan = gpu.fan_id(index);
                let ours = if self.lost {
                    driver
                        .sample(gpu, index)
                        .map_or(true, |sample| sample.manual == Some(true))
                } else {
                    self.owned.fans.contains(&fan)
                };
                if !ours {
                    continue;
                }
                match driver.restore(gpu, index) {
                    Ok(()) => {
                        tracing::info!(%fan, "returned a GPU fan left in manual mode to the driver");
                        changed |= self.owned.fans.remove(&fan);
                    }
                    Err(error) => {
                        tracing::error!(%fan, %error, "could not return a leftover GPU fan to the driver");
                    }
                }
            }
        }
        self.lost = false;
        if changed {
            self.persist();
        }
    }

    fn persist(&mut self) -> bool {
        self.inner.lock().owned = self.owned.fans.clone();
        match ownership::save(&self.path, &self.owned) {
            Ok(()) => true,
            Err(error) => {
                tracing::error!(%error, "could not save which GPU fans FanForge controls");
                false
            }
        }
    }

    fn report(&self, fan: &FanId, error: String) {
        self.inner.lock().errors.push((fan.clone(), error));
    }

    fn apply(&mut self, fan: &FanId, action: Action) {
        if self.driver.is_none() {
            return;
        }
        let Some((gpu, index)) = locate(&self.gpus, fan).map(|(gpu, index)| (gpu.clone(), index))
        else {
            self.report(fan, "the GPU fan is no longer present".to_owned());
            return;
        };
        match action {
            Action::SetDuty(duty) => self.set(fan, &gpu, index, duty),
            Action::Release => {
                if !self.owned.fans.contains(fan) {
                    return;
                }
                let _writes = writes();
                match self.call(|driver| driver.restore(&gpu, index)) {
                    Ok(()) => {
                        self.written.remove(fan);
                        self.owned.fans.remove(fan);
                        self.persist();
                    }
                    Err(error) => self.report(fan, error),
                }
            }
        }
    }

    fn call<T>(&self, call: impl FnOnce(&dyn FanDriver) -> Result<T, String>) -> Result<T, String> {
        match &self.driver {
            Some(driver) => call(driver.as_ref()),
            None => Err("the GPU driver is unavailable".to_owned()),
        }
    }

    fn set(&mut self, fan: &FanId, gpu: &GpuDesc, index: u32, duty: Duty) {
        let _writes = writes();
        if self.halt.load(Ordering::SeqCst) {
            return;
        }
        let (minimum, maximum) = gpu.range.unwrap_or((100, 100));
        let percent = (duty.percent().round() as u32).clamp(minimum, maximum);
        if !self.owned.fans.contains(fan) {
            match self.call(|driver| driver.sample(gpu, index)) {
                Ok(sample) if sample.manual == Some(false) => {}
                Ok(_) => return,
                Err(error) => {
                    self.report(fan, error);
                    return;
                }
            }
            self.owned.fans.insert(fan.clone());
            if !self.persist() {
                self.owned.fans.remove(fan);
                self.inner.lock().owned = self.owned.fans.clone();
                self.report(
                    fan,
                    "could not record that FanForge controls this fan".to_owned(),
                );
                return;
            }
        }
        if self.halt.load(Ordering::SeqCst) {
            return;
        }
        match self.call(|driver| driver.set_speed(gpu, index, percent)) {
            Ok(()) => {
                self.written.insert(fan.clone(), percent);
            }
            Err(error) => self.report(fan, error),
        }
    }

    fn release_owned(&mut self) -> Result<(), String> {
        if self.owned.fans.is_empty() {
            return Ok(());
        }
        let _writes = writes();
        let Some(driver) = &self.driver else {
            return Err("the GPU driver is unavailable".to_owned());
        };
        let mut first_error = None;
        for fan in self.owned.fans.clone() {
            let Some((gpu, index)) = locate(&self.gpus, &fan) else {
                tracing::warn!(%fan, "an owned GPU fan is no longer present; forgetting it");
                self.owned.fans.remove(&fan);
                self.written.remove(&fan);
                continue;
            };
            match driver.restore(gpu, index) {
                Ok(()) => {
                    self.owned.fans.remove(&fan);
                    self.written.remove(&fan);
                }
                Err(error) => {
                    tracing::error!(%fan, %error, "could not return a GPU fan to the driver");
                    first_error.get_or_insert(error);
                }
            }
        }
        self.persist();
        first_error.map_or(Ok(()), Err)
    }

    fn finish_release(&self, round: u64, result: Result<(), String>) {
        let mut state = self.inner.lock();
        state.release_done = round;
        state.release_error = result.err();
        self.inner.done.notify_all();
    }

    fn sample(&mut self) {
        let Some(driver) = &self.driver else {
            return;
        };
        let mut fans = Vec::new();
        let mut last_error = None;
        for gpu in &self.gpus {
            for index in 0..gpu.fans {
                let id = gpu.fan_id(index);
                let sample = driver.sample(gpu, index);
                let error = sample.as_ref().err().cloned();
                let sample = sample.unwrap_or_default();
                let manual = sample.manual == Some(true);
                if let Some(error) = &error {
                    last_error = Some(error.clone());
                }
                let target = sample
                    .target
                    .or_else(|| self.written.get(&id).copied().filter(|_| manual));
                fans.push(FanSnapshot {
                    foreign: manual && !self.owned.fans.contains(&id),
                    id,
                    gpu: gpu.label.clone(),
                    index,
                    minimum: gpu.range.map(|(minimum, _)| minimum),
                    rpm: sample.rpm,
                    manual,
                    target,
                    error,
                });
            }
        }
        if let Some(error) = last_error.filter(|_| fans.iter().all(|fan| fan.error.is_some())) {
            tracing::warn!(%error, "the GPU driver stopped answering; reopening it later");
            self.close(error);
            return;
        }
        let mut state = self.inner.lock();
        state.fans = fans;
        state.sampled_at = Some(Instant::now());
        state.written = self.written.clone();
        state.owned = self.owned.fans.clone();
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::gpu_fans::GpuReleaser;
    use crate::gpu_fans::fake::{FakeFan, FakeGpus, GPU_ID, SharedGpus, Write};

    const INTERVAL: Duration = Duration::from_secs(1);

    fn fan(index: u32) -> FanId {
        FanId::for_gpu_fan(GPU_ID, index)
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "condition not reached in time");
            thread::sleep(Duration::from_millis(2));
        }
    }

    struct Rig {
        fake: SharedGpus,
        gpus: GpuFans,
        halt: Arc<AtomicBool>,
        path: PathBuf,
        _dir: tempfile::TempDir,
    }

    fn rig_with(fake: FakeGpus, prepare: impl FnOnce(&Path)) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gpu_owned.toml");
        prepare(&path);
        let fake = SharedGpus::new(fake);
        let halt = Arc::new(AtomicBool::new(false));
        let gpus = GpuFans::start(fake.opener(), path.clone(), Arc::clone(&halt)).unwrap();
        let rig = Rig {
            fake,
            gpus,
            halt,
            path,
            _dir: dir,
        };
        wait_until(|| rig.reading(0).is_some());
        rig
    }

    fn rig() -> Rig {
        rig_with(FakeGpus::default(), |_| {})
    }

    impl Rig {
        fn reading(&self, index: u32) -> Option<FanReading> {
            self.gpus
                .readings(Instant::now(), INTERVAL)
                .fans
                .into_iter()
                .find(|reading| reading.id == fan(index))
        }

        fn writes(&self) -> Vec<Write> {
            self.fake.with(|gpus| gpus.writes.clone())
        }

        fn owned(&self) -> Loaded {
            ownership::load(&self.path)
        }

        fn set(&self, index: u32, percent: f32) {
            self.gpus
                .request(&fan(index), Action::SetDuty(Duty::new(percent)));
        }
    }

    #[test]
    fn ownership_is_saved_before_the_first_write_and_cleared_on_release() {
        let rig = rig();
        let readings = rig.gpus.readings(Instant::now(), INTERVAL);
        assert_eq!(readings.fans.len(), 2);
        assert_eq!(readings.fans[0].label, "Test RTX fan 1");
        assert_eq!(
            readings.limits[&fan(0)],
            FanLimits::for_gpu(Duty::new(30.0))
        );
        rig.set(0, 60.0);
        wait_until(|| rig.fake.fan(0).manual);
        assert!(rig.owned().owns(&fan(0)));
        assert!(!rig.owned().owns(&fan(1)));
        assert!(rig.gpus.owns_any());
        wait_until(|| {
            rig.reading(0)
                .is_some_and(|reading| reading.mode == HardwareMode::Manual)
        });
        assert_eq!(rig.reading(0).unwrap().duty, Some(Duty::new(60.0)));

        rig.gpus.release(Duration::from_secs(2)).unwrap();
        assert!(!rig.fake.fan(0).manual);
        assert_eq!(rig.writes(), vec![Write::Speed(0, 60), Write::Restore(0)]);
        assert!(!rig.owned().owns(&fan(0)));
        assert!(!rig.gpus.owns_any());
    }

    #[test]
    fn a_fan_another_program_controls_is_never_claimed() {
        let mut fake = FakeGpus::default();
        fake.fans.insert(
            1,
            FakeFan {
                manual: true,
                target: 70,
            },
        );
        let rig = rig_with(fake, |_| {});
        assert_eq!(
            rig.reading(1).unwrap().unavailable.as_deref(),
            Some(FOREIGN)
        );
        rig.set(1, 40.0);
        rig.set(0, 40.0);
        wait_until(|| rig.fake.fan(0).manual);
        rig.gpus.release(Duration::from_secs(2)).unwrap();
        assert_eq!(rig.writes(), vec![Write::Speed(0, 40), Write::Restore(0)]);
        assert_eq!(
            rig.fake.fan(1),
            FakeFan {
                manual: true,
                target: 70
            }
        );
    }

    #[test]
    fn control_is_refused_when_ownership_cannot_be_saved() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocked");
        std::fs::write(&blocker, b"a file, not a directory").unwrap();
        let fake = SharedGpus::new(FakeGpus::default());
        let gpus = GpuFans::start(
            fake.opener(),
            blocker.join("gpu_owned.toml"),
            Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        gpus.request(&fan(0), Action::SetDuty(Duty::new(60.0)));
        let mut errors = Vec::new();
        wait_until(|| {
            errors.extend(gpus.take_errors());
            !errors.is_empty()
        });
        assert_eq!(errors[0].0, fan(0));
        assert!(fake.with(|gpus| gpus.writes.is_empty()));
        assert!(!gpus.owns_any());
    }

    #[test]
    fn a_halt_stops_every_write_but_releases() {
        let rig = rig();
        rig.set(0, 60.0);
        wait_until(|| rig.fake.fan(0).manual);
        rig.halt.store(true, Ordering::SeqCst);
        rig.set(0, 90.0);
        rig.set(1, 90.0);
        thread::sleep(Duration::from_millis(50));
        rig.gpus.release(Duration::from_secs(2)).unwrap();
        assert_eq!(rig.writes(), vec![Write::Speed(0, 60), Write::Restore(0)]);
    }

    #[test]
    fn only_the_latest_command_for_a_fan_is_written() {
        let rig = rig();
        rig.fake.hold();
        rig.set(0, 40.0);
        thread::sleep(Duration::from_millis(50));
        for percent in [50.0, 55.0, 60.0] {
            rig.set(0, percent);
        }
        rig.fake.unhold();
        wait_until(|| rig.fake.fan(0).target == 60);
        let writes = rig.writes();
        assert_eq!(writes.last(), Some(&Write::Speed(0, 60)));
        assert!(!writes.contains(&Write::Speed(0, 50)));
        assert!(!writes.contains(&Write::Speed(0, 55)));
    }

    #[test]
    fn a_pending_command_is_reported_while_the_driver_holds_the_last_write() {
        let rig = rig();
        rig.set(0, 50.0);
        wait_until(|| rig.reading(0).unwrap().duty == Some(Duty::new(50.0)));
        rig.fake.hold();
        rig.set(0, 55.0);
        assert_eq!(rig.reading(0).unwrap().duty, Some(Duty::new(55.0)));
        rig.fake.unhold();
    }

    #[test]
    fn a_hung_driver_makes_fans_not_responding_and_they_recover() {
        let rig = rig();
        rig.set(0, 60.0);
        wait_until(|| rig.fake.fan(0).manual);
        rig.fake.hold();
        let later = Instant::now() + MIN_STALE + Duration::from_millis(10);
        let readings = rig.gpus.readings(later, INTERVAL);
        assert!(readings.fans.iter().all(|reading| {
            reading.unavailable.as_deref() == Some(NOT_RESPONDING) && reading.rpm.is_none()
        }));
        let started = Instant::now();
        assert_eq!(
            rig.gpus.release(Duration::from_millis(100)),
            Err(NOT_RESPONDING.to_owned())
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        rig.fake.unhold();
        wait_until(|| {
            rig.gpus
                .readings(Instant::now(), INTERVAL)
                .fans
                .iter()
                .all(|reading| reading.unavailable.is_none())
        });
    }

    #[test]
    fn a_panicking_driver_never_kills_the_worker() {
        let rig = rig();
        rig.fake.with(|gpus| gpus.panic_on_sample = true);
        rig.set(0, 60.0);
        wait_until(|| rig.gpus.readings(Instant::now(), INTERVAL).error.is_some());
        rig.fake.with(|gpus| gpus.panic_on_sample = false);
        assert_eq!(rig.gpus.release(Duration::from_secs(2)), Ok(()));
    }

    #[test]
    fn fans_left_by_a_crash_are_returned_to_the_driver_at_startup() {
        let mut fake = FakeGpus::default();
        for index in [0, 1] {
            fake.fans.insert(
                index,
                FakeFan {
                    manual: true,
                    target: 60,
                },
            );
        }
        let rig = rig_with(fake, |path| {
            ownership::save(
                path,
                &Ownership {
                    fans: [fan(0)].into(),
                },
            )
            .unwrap();
        });
        assert_eq!(rig.writes(), vec![Write::Restore(0)]);
        assert!(rig.fake.fan(1).manual);
        assert_eq!(rig.owned(), Loaded::Owned(Ownership::default()));
        assert!(!rig.gpus.owns_any());
    }

    #[test]
    fn an_unreadable_ownership_file_returns_every_manual_fan_to_the_driver() {
        let mut fake = FakeGpus::default();
        fake.fans.insert(
            1,
            FakeFan {
                manual: true,
                target: 60,
            },
        );
        let rig = rig_with(fake, |path| std::fs::write(path, "not toml [").unwrap());
        assert_eq!(rig.writes(), vec![Write::Restore(1)]);
        assert_eq!(rig.owned(), Loaded::Owned(Ownership::default()));
    }

    #[test]
    fn the_releaser_resets_owned_fans_without_rewriting_the_file() {
        let rig = rig();
        rig.set(0, 60.0);
        wait_until(|| rig.fake.fan(0).manual);
        let before = std::fs::read(&rig.path).unwrap();
        let releaser = GpuReleaser::open(rig.fake.opener(), rig.path.clone());
        assert!(releaser.has_session());
        releaser.release(Duration::from_secs(2)).unwrap();
        assert!(!rig.fake.fan(0).manual);
        assert_eq!(std::fs::read(&rig.path).unwrap(), before);

        rig.fake.hold();
        let started = Instant::now();
        assert!(releaser.release(Duration::from_millis(100)).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        rig.fake.unhold();
    }

    #[test]
    fn a_write_in_progress_when_the_halt_arrives_never_lands() {
        let rig = rig();
        let releaser = GpuReleaser::open(rig.fake.opener(), rig.path.clone());
        rig.fake.hold();
        rig.set(0, 60.0);
        thread::sleep(Duration::from_millis(50));
        rig.halt.store(true, Ordering::SeqCst);
        let pending = {
            let releaser = releaser.clone();
            thread::spawn(move || releaser.release(Duration::from_secs(5)))
        };
        thread::sleep(Duration::from_millis(50));
        rig.fake.unhold();
        pending.join().unwrap().unwrap();
        thread::sleep(Duration::from_millis(50));
        assert!(!rig.fake.fan(0).manual);
        assert!(
            !rig.writes()
                .iter()
                .any(|write| matches!(write, Write::Speed(..)))
        );
    }

    #[test]
    fn a_stale_spare_session_is_replaced_and_retried() {
        let rig = rig();
        rig.set(0, 60.0);
        wait_until(|| rig.fake.fan(0).manual);
        let releaser = GpuReleaser::open(rig.fake.opener(), rig.path.clone());
        rig.fake.with(|gpus| gpus.expired_before = gpus.opens + 1);
        releaser.release(Duration::from_secs(2)).unwrap();
        assert!(!rig.fake.fan(0).manual);
    }

    #[test]
    fn without_a_target_readback_the_last_write_is_reported() {
        let rig = rig_with(
            FakeGpus {
                no_target: true,
                ..FakeGpus::default()
            },
            |_| {},
        );
        rig.set(0, 65.0);
        wait_until(|| rig.reading(0).unwrap().mode == HardwareMode::Manual);
        assert_eq!(rig.reading(0).unwrap().duty, Some(Duty::new(65.0)));
    }

    #[test]
    fn an_owned_fan_whose_gpu_is_gone_is_forgotten_on_release() {
        let gone = FanId::for_gpu_fan("gpu/nvidia/gpu-gone", 0);
        let rig = rig_with(FakeGpus::default(), |path| {
            ownership::save(
                path,
                &Ownership {
                    fans: [gone.clone()].into(),
                },
            )
            .unwrap();
        });
        assert!(rig.gpus.owns_any());
        rig.gpus.release(Duration::from_secs(2)).unwrap();
        assert!(!rig.gpus.owns_any());
        assert!(!rig.owned().owns(&gone));
    }

    #[test]
    fn the_releaser_skips_the_driver_when_nothing_is_owned() {
        let dir = tempfile::tempdir().unwrap();
        let fake = SharedGpus::new(FakeGpus {
            fail_open: true,
            ..FakeGpus::default()
        });
        let releaser = GpuReleaser::open(fake.opener(), dir.path().join("gpu_owned.toml"));
        assert!(!releaser.has_session());
        assert_eq!(releaser.release(Duration::from_secs(1)), Ok(()));
        assert_eq!(fake.with(|gpus| gpus.opens), 1);
    }
}
