use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use fanforge_core::config::Config;
use fanforge_core::engine::{Engine, TickOutcome};
use fanforge_core::state::{ControlState, HardwareStatus};
use fanforge_core::store::{self, CONFIG_BACKUPS};
use fanforge_core::telemetry::Readings;
use fanforge_hw::HwError;
use fanforge_hw::sensors::ExtraSensors;
use fanforge_win::paths;

use crate::backend::Backend;
use crate::conflicts::ConflictMonitor;
use crate::guardian::{Signal, SignalSink};
use crate::shared::{LoopMessage, Shared};

pub const IDENTIFY_DURATION: Duration = Duration::from_secs(3);
pub const RESUME_BUDGET: Duration = Duration::from_secs(5);
const HARDWARE_RETRY: Duration = Duration::from_secs(10);
const RESUME_FIRST_BACKOFF: Duration = Duration::from_millis(500);
const READ_FAILURES_BEFORE_REINIT: u32 = 3;
const LOCK_SKIPS_BEFORE_WARNING: u32 = 10;

pub type Opener<B> = Box<dyn FnMut(&Path) -> Result<B, HwError> + Send>;

#[derive(Debug, Clone)]
pub struct Paths {
    pub config: PathBuf,
    pub snapshot: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            config: paths::config_path(),
            snapshot: paths::bios_snapshot_path(),
        }
    }
}

enum Flow {
    Continue,
    Stop,
}

pub struct Runtime<B: Backend> {
    paths: Paths,
    shared: Arc<Shared>,
    messages: Receiver<LoopMessage>,
    deferred: VecDeque<LoopMessage>,
    signals: Option<SignalSink>,
    engine: Engine,
    opener: Opener<B>,
    hardware: Option<B>,
    hardware_error: Option<String>,
    driver_missing: bool,
    next_hardware_attempt: Instant,
    suspended: bool,
    unreleased: bool,
    read_failures: u32,
    lock_skips: u32,
    conflicts: ConflictMonitor,
    extras: ExtraSensors,
    config_warning: Option<String>,
    last_outcome: TickOutcome,
    last_readings: Readings,
    last_tick: Instant,
}

impl<B: Backend> Runtime<B> {
    pub fn new(
        paths: Paths,
        shared: Arc<Shared>,
        messages: Receiver<LoopMessage>,
        opener: Opener<B>,
        conflicts: ConflictMonitor,
    ) -> Self {
        let (config, config_warning) = load_config(&paths);
        let now = Instant::now();
        Self {
            paths,
            shared,
            messages,
            deferred: VecDeque::new(),
            signals: None,
            engine: Engine::new(config, Default::default()),
            opener,
            hardware: None,
            hardware_error: None,
            driver_missing: false,
            next_hardware_attempt: now,
            suspended: false,
            unreleased: false,
            read_failures: 0,
            lock_skips: 0,
            conflicts,
            extras: ExtraSensors::none(),
            config_warning,
            last_outcome: TickOutcome::default(),
            last_readings: Readings::default(),
            last_tick: now,
        }
    }

    pub fn with_signals(mut self, signals: SignalSink) -> Self {
        self.signals = Some(signals);
        self
    }

    pub fn with_extra_sensors(mut self, extras: ExtraSensors) -> Self {
        self.extras = extras;
        self
    }

    fn with_extras(&self, mut readings: Readings) -> Readings {
        readings
            .sensors
            .extend(self.extras.readings(Instant::now()));
        readings
    }

    pub fn run(mut self) -> bool {
        self.open_hardware();
        self.beat();
        self.publish();
        loop {
            let deadline = Instant::now() + self.interval();
            loop {
                let message = match self.deferred.pop_front() {
                    Some(message) => message,
                    None => {
                        let wait = deadline.saturating_duration_since(Instant::now());
                        match self.messages.recv_timeout(wait) {
                            Ok(message) => message,
                            Err(RecvTimeoutError::Timeout) => break,
                            Err(RecvTimeoutError::Disconnected) => return self.shutdown(),
                        }
                    }
                };
                if let Flow::Stop = self.handle(message) {
                    return self.shutdown();
                }
            }
            self.tick();
        }
    }

    fn interval(&self) -> Duration {
        Duration::from_millis(self.engine.config().interval_ms)
    }

    fn signal(&self, signal: Signal) {
        if let Some(signals) = &self.signals {
            signals(signal);
        }
    }

    fn owns_any(&self) -> bool {
        self.hardware.as_ref().is_some_and(Backend::owns_any)
    }

    fn beat(&self) {
        self.shared
            .heartbeat
            .beat(self.interval(), self.hardware.is_some());
        self.signal(Signal::Heartbeat {
            interval: self.interval(),
        });
    }

    fn handle(&mut self, message: LoopMessage) -> Flow {
        match message {
            LoopMessage::ReplaceConfig(config, reply) => {
                let result = self.replace_config(*config);
                if reply.send(result).is_err() {
                    tracing::warn!("config change requester went away");
                }
            }
            LoopMessage::SetActiveProfile(name, reply) => {
                let mut config = self.engine.config().clone();
                config.active_profile = name;
                reply.send(self.replace_config(config)).ok();
            }
            LoopMessage::Identify(fan) => self.engine.identify(fan, IDENTIFY_DURATION),
            LoopMessage::Suspend(done) => {
                self.suspend();
                done.send(()).ok();
            }
            LoopMessage::Resume => return self.resume(),
            LoopMessage::RetryHardware => {
                if self.hardware.is_none() {
                    self.open_hardware();
                    self.publish();
                }
            }
            LoopMessage::Stop => return Flow::Stop,
        }
        Flow::Continue
    }

    fn replace_config(&mut self, config: Config) -> Result<(), String> {
        config.validate().map_err(|error| error.to_string())?;
        let sensor = &config.safety.critical_sensor;
        let readings = self.with_extras(self.last_readings.clone());
        if self.hardware.is_some()
            && !self.last_readings.sensors.is_empty()
            && !readings.sensors.iter().any(|reading| &reading.id == sensor)
        {
            return Err(format!("critical sensor `{sensor}` does not exist"));
        }
        store::save_toml_atomic(&self.paths.config, &config, CONFIG_BACKUPS)
            .map_err(|error| error.to_string())?;
        tracing::info!(profile = %config.active_profile, "configuration updated");
        self.engine.set_config(config);
        self.config_warning = None;
        self.publish();
        Ok(())
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_tick);
        self.last_tick = now;
        if self.suspended || self.shared.is_halted() {
            self.publish();
            return;
        }
        if self.hardware.is_none() && now >= self.next_hardware_attempt {
            self.open_hardware();
        }

        let findings = self.conflicts.poll().clone();
        let unverified = self
            .hardware
            .as_ref()
            .is_some_and(|hardware| !hardware.fan_control());
        self.engine
            .set_monitor_only(!findings.controlling.is_empty() || unverified);

        self.beat();
        self.control(elapsed);
        self.signal(Signal::Owning(self.owns_any()));
        self.publish();
    }

    fn control(&mut self, elapsed: Duration) {
        let Some(hardware) = self.hardware.as_mut() else {
            return;
        };
        let readings = match hardware.read() {
            Ok(readings) => {
                self.read_failures = 0;
                self.lock_skips = 0;
                readings
            }
            Err(HwError::LockTimeout(name)) => {
                self.lock_skips += 1;
                if self.lock_skips == LOCK_SKIPS_BEFORE_WARNING {
                    tracing::warn!(mutex = %name, "bus has been busy for many cycles");
                }
                return;
            }
            Err(error) => {
                self.read_failures += 1;
                tracing::warn!(%error, failures = self.read_failures, "hardware read failed");
                if self.read_failures >= READ_FAILURES_BEFORE_REINIT {
                    self.reinitialize_after_failure();
                }
                return;
            }
        };

        let mut inputs = readings.clone();
        inputs.sensors.extend(self.extras.readings(Instant::now()));
        let outcome = self.engine.tick(&inputs, elapsed);
        for command in &outcome.commands {
            match hardware.apply(command, self.shared.halt_flag()) {
                Ok(()) | Err(HwError::Halted) => {}
                Err(error) => {
                    tracing::warn!(fan = %command.fan, %error, "failed to apply fan command");
                    self.engine
                        .record_apply_error(&command.fan, &error.to_string());
                }
            }
        }
        self.last_outcome = outcome;
        self.last_readings = readings;
    }

    fn open_hardware(&mut self) {
        self.next_hardware_attempt = Instant::now() + HARDWARE_RETRY;
        let mut hardware = match (self.opener)(&self.paths.snapshot) {
            Ok(hardware) => hardware,
            Err(error) => {
                self.driver_missing = matches!(error, HwError::PawnIoMissing);
                let message = error.to_string();
                if self.hardware_error.as_deref() != Some(message.as_str()) {
                    tracing::error!(%error, "hardware unavailable");
                }
                self.hardware_error = Some(message);
                return;
            }
        };
        let take_control = self.conflicts.poll().controlling.is_empty();
        match hardware.initialize_channels(take_control) {
            Ok(()) if take_control => self.unreleased = false,
            Ok(()) => {}
            Err(error) => {
                tracing::error!(%error, "failed to return fans to BIOS control at startup");
            }
        }
        let info = hardware.board_info();
        tracing::info!(
            board = %info.identity.product,
            fan_headers = info.fan_headers,
            chip = format_args!("{:#06x}", info.chip_id),
            take_control,
            "hardware ready"
        );
        self.engine.set_limits(hardware.fan_limits());
        self.engine.forget_hardware_state();
        self.hardware = Some(hardware);
        self.hardware_error = None;
        self.driver_missing = false;
        self.read_failures = 0;
    }

    fn drop_hardware(&mut self, reason: String) {
        if let Some(mut hardware) = self.hardware.take()
            && let Err(error) = hardware.release_all()
        {
            tracing::error!(%error, "could not release fans through the failing handle");
            let fresh =
                (self.opener)(&self.paths.snapshot).and_then(|mut fresh| fresh.release_all());
            if let Err(error) = fresh {
                tracing::error!(%error, "could not release fans through a fresh handle");
                self.unreleased = true;
            }
        }
        self.engine.forget_hardware_state();
        self.hardware_error = Some(reason);
        self.next_hardware_attempt = Instant::now() + HARDWARE_RETRY;
    }

    fn reinitialize_after_failure(&mut self) {
        let Some(hardware) = self.hardware.as_mut() else {
            return;
        };
        match hardware.reinitialize() {
            Ok(()) => {
                tracing::info!("hardware reinitialised after read failures");
                self.read_failures = 0;
                self.engine.forget_hardware_state();
            }
            Err(error) => {
                tracing::error!(%error, "hardware reinitialisation failed; releasing fans and reopening later");
                self.drop_hardware(error.to_string());
            }
        }
    }

    fn suspend(&mut self) {
        tracing::info!("system suspending; returning fans to BIOS control");
        self.suspended = true;
        self.shared.heartbeat.pause();
        if let Some(hardware) = self.hardware.as_mut()
            && let Err(error) = hardware.release_all()
        {
            tracing::error!(%error, "failed to release fans before suspend");
        }
        self.engine.forget_hardware_state();
        self.signal(Signal::Suspended);
        self.signal(Signal::Owning(self.owns_any()));
        self.publish();
    }

    fn wait_for_stop(&mut self, wait: Duration) -> Flow {
        let deadline = Instant::now() + wait;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.messages.recv_timeout(remaining) {
                Ok(LoopMessage::Stop) | Err(RecvTimeoutError::Disconnected) => return Flow::Stop,
                Ok(other) => self.deferred.push_back(other),
                Err(RecvTimeoutError::Timeout) => return Flow::Continue,
            }
        }
    }

    fn reinitialize_after_resume(&mut self) -> Result<bool, ()> {
        let deadline = Instant::now() + RESUME_BUDGET;
        let mut backoff = RESUME_FIRST_BACKOFF;
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let Some(hardware) = self.hardware.as_mut() else {
                return Ok(false);
            };
            match hardware.reinitialize() {
                Ok(()) => return Ok(true),
                Err(error) => tracing::warn!(attempt, %error, "hardware not ready after resume"),
            }
            let wait = backoff.min(deadline.saturating_duration_since(Instant::now()));
            if wait.is_zero() {
                return Ok(false);
            }
            if let Flow::Stop = self.wait_for_stop(wait) {
                return Err(());
            }
            backoff *= 2;
        }
    }

    fn resume(&mut self) -> Flow {
        if !self.suspended {
            return Flow::Continue;
        }
        tracing::info!("system resumed; reinitialising hardware");
        self.shared.heartbeat.pause();
        let Ok(ready) = self.reinitialize_after_resume() else {
            return Flow::Stop;
        };
        if ready
            && let Some(hardware) = self.hardware.as_mut()
            && let Err(error) = hardware.release_all()
        {
            tracing::warn!(%error, "failed to reset channels after resume");
        }
        if !ready && self.hardware.is_some() {
            self.drop_hardware("hardware did not recover after resume".to_owned());
            self.next_hardware_attempt = Instant::now();
        }
        self.engine.forget_hardware_state();
        self.suspended = false;
        self.last_tick = Instant::now();
        self.signal(Signal::Resumed);
        self.beat();
        Flow::Continue
    }

    fn shutdown(&mut self) -> bool {
        self.shared.heartbeat.pause();
        let Some(mut hardware) = self.hardware.take() else {
            return !self.unreleased;
        };
        match hardware.release_all() {
            Ok(()) => {
                tracing::info!("all fans returned to BIOS control");
                self.signal(Signal::Owning(false));
                true
            }
            Err(error) => {
                tracing::error!(%error, "failed to release every fan during shutdown");
                false
            }
        }
    }

    fn publish(&mut self) {
        let findings = self.conflicts.poll().clone();
        let mut warnings: Vec<String> = self.config_warning.iter().cloned().collect();
        let board = self.hardware.as_ref().map(Backend::board_info);
        let hardware_status = match (&self.hardware, self.suspended) {
            (_, true) => HardwareStatus::Suspended,
            (Some(hardware), false) => {
                warnings.extend(hardware.warnings());
                HardwareStatus::Ready
            }
            (None, false) if self.driver_missing => HardwareStatus::DriverMissing,
            (None, false) => HardwareStatus::Unavailable(
                self.hardware_error
                    .clone()
                    .unwrap_or_else(|| "starting".to_owned()),
            ),
        };
        if self.last_outcome.critical_sensor_missing {
            warnings.push(format!(
                "critical sensor `{}` is not reporting; fans go to 100% if it stays silent",
                self.engine.config().safety.critical_sensor
            ));
        }
        if self.lock_skips >= LOCK_SKIPS_BEFORE_WARNING {
            warnings.push("another program has held the hardware bus for a long time".to_owned());
        }
        warnings.extend(
            findings.monitoring.iter().map(|tool| {
                format!("{tool} is running; if it controls fans it will fight FanForge")
            }),
        );
        let state = ControlState {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            hardware: hardware_status,
            board,
            readings: self.with_extras(self.last_readings.clone()),
            fans: self.last_outcome.fans.clone(),
            limits: self.engine.limits().clone(),
            config: self.engine.config().clone(),
            critical: self.last_outcome.critical,
            monitor_only: self.engine.is_monitor_only(),
            conflicts: findings.controlling,
            warnings,
        };
        if let Ok(mut slot) = self.shared.state.write() {
            *slot = Some(Arc::new(state));
        }
    }
}

impl<B: Backend> Drop for Runtime<B> {
    fn drop(&mut self) {
        if self.hardware.is_some() {
            tracing::warn!("runtime dropped while controlling fans; releasing them");
            self.shutdown();
        }
    }
}

fn load_config(paths: &Paths) -> (Config, Option<String>) {
    match store::load_toml::<Config>(&paths.config) {
        Ok(Some(config)) => match config.validate() {
            Ok(()) => (config, None),
            Err(error) => {
                tracing::error!(%error, "configuration is invalid; using defaults");
                (
                    Config::default(),
                    Some(format!(
                        "saved configuration is invalid ({error}); using defaults"
                    )),
                )
            }
        },
        Ok(None) => (Config::default(), None),
        Err(error) => {
            tracing::error!(%error, "configuration could not be loaded; using defaults");
            (
                Config::default(),
                Some(format!(
                    "saved configuration could not be loaded ({error}); using defaults"
                )),
            )
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::panic::{self, AssertUnwindSafe};
    use std::sync::{Mutex, mpsc};
    use std::thread;

    use fanforge_core::Duty;
    use fanforge_core::board::BoardIdentity;
    use fanforge_core::config::FanMode;
    use fanforge_core::curve::{CurveDef, CurveKind, CurvePoint, Tuning};
    use fanforge_core::engine::FanState;
    use fanforge_core::telemetry::SensorReading;
    use fanforge_hw::ite::fake::{FakeIte, SharedFake};
    use fanforge_hw::mutex::NamedMutex;
    use fanforge_hw::nuvoton::fake::{FakeNct, OUTSIDE_CONFIG_MODE, SharedNct};
    use fanforge_hw::sensors::SensorCache;
    use fanforge_hw::superio::{Slot, SuperIo};
    use fanforge_hw::system::Hardware;

    use super::*;

    const CPU_CONTROL: u8 = 0x15;
    const CHA1_CONTROL: u8 = 0x16;
    const CHA1_DUTY: u8 = 0x6B;
    const AUTO: u8 = 0x80;

    struct Rig {
        runtime: Runtime<Hardware<SharedFake>>,
        fake: SharedFake,
        _dir: tempfile::TempDir,
        messages: mpsc::Sender<LoopMessage>,
    }

    pub(crate) fn opener(fake: SharedFake, tag: &str) -> Opener<Hardware<SharedFake>> {
        let name = format!(r"Local\fanforge-runtime-{tag}-{}", std::process::id());
        Box::new(move |snapshot: &Path| {
            let chip = SuperIo::probe(fake.clone(), Slot::Primary, NamedMutex::open(&name)?)?;
            Hardware::from_parts(
                chip,
                Err("no CPU in tests".to_owned()),
                BoardIdentity::default(),
                snapshot,
            )
        })
    }

    pub(crate) fn config_with(modes: &[(&str, FanMode)]) -> Config {
        let mut config = Config::default();
        config.safety.critical_sensor = "board/temp1".into();
        let profile = config.profiles.get_mut("Balanced").unwrap();
        for (fan, mode) in modes {
            profile.fans.insert((*fan).into(), mode.clone());
        }
        config
    }

    fn rig(tag: &str, modes: &[(&str, FanMode)], processes: Vec<String>) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config.toml"),
            snapshot: dir.path().join("snapshot.toml"),
        };
        store::save_toml_atomic(&paths.config, &config_with(modes), 0).unwrap();
        let fake = SharedFake::new(FakeIte::new());
        let (tx, rx) = mpsc::channel();
        let conflicts = ConflictMonitor::with_processes(Box::new(move || Ok(processes.clone())));
        let runtime = Runtime::new(
            paths,
            Arc::new(Shared::default()),
            rx,
            opener(fake.clone(), tag),
            conflicts,
        );
        Rig {
            runtime,
            fake,
            _dir: dir,
            messages: tx,
        }
    }

    pub(crate) fn manual(duty: f32) -> FanMode {
        FanMode::Manual {
            duty: Duty::new(duty),
        }
    }

    fn reg(rig: &Rig, register: u8) -> u8 {
        rig.fake.with(|chip| chip.reg(register))
    }

    #[test]
    fn controls_fans_and_releases_them_on_stop() {
        let mut rig = rig("stop", &[("pwm2", manual(60.0))], Vec::new());
        rig.runtime.open_hardware();
        rig.runtime.tick();
        assert_eq!(reg(&rig, CHA1_CONTROL) & AUTO, 0);
        assert_eq!(reg(&rig, CHA1_DUTY), Duty::new(60.0).to_raw());
        assert!(rig.runtime.shutdown());
        assert_eq!(reg(&rig, CHA1_CONTROL) & AUTO, AUTO);
        assert_eq!(reg(&rig, CHA1_DUTY), 0x40);
    }

    #[test]
    fn a_panic_unwinding_out_of_the_runtime_releases_fans() {
        let mut rig = rig("panic", &[("pwm2", manual(60.0))], Vec::new());
        rig.runtime.open_hardware();
        rig.runtime.tick();
        let fake = rig.fake.clone();
        let runtime = rig.runtime;
        let result = panic::catch_unwind(AssertUnwindSafe(move || {
            let _owned = runtime;
            panic!("simulated control loop panic");
        }));
        assert!(result.is_err());
        assert_eq!(fake.with(|chip| chip.reg(CHA1_CONTROL)) & AUTO, AUTO);
    }

    #[test]
    fn a_halted_runtime_never_takes_a_fan() {
        let mut rig = rig("halted", &[("pwm2", manual(60.0))], Vec::new());
        rig.runtime.open_hardware();
        rig.runtime.shared.halt();
        rig.runtime.tick();
        assert_eq!(reg(&rig, CHA1_CONTROL) & AUTO, AUTO);
        assert!(!rig.runtime.owns_any());
    }

    #[test]
    fn the_guardian_hears_heartbeats_and_ownership() {
        let mut rig = rig("signals", &[("pwm2", manual(60.0))], Vec::new());
        let heard = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&heard);
        rig.runtime.signals = Some(Arc::new(move |signal| {
            sink.lock().unwrap().push(signal);
        }));
        rig.runtime.open_hardware();
        rig.runtime.tick();
        rig.runtime.suspend();
        assert!(rig.runtime.shutdown());
        let heard = heard.lock().unwrap().clone();
        assert_eq!(
            heard,
            vec![
                Signal::Heartbeat {
                    interval: Duration::from_secs(1)
                },
                Signal::Owning(true),
                Signal::Suspended,
                Signal::Owning(false),
                Signal::Owning(false),
            ]
        );
    }

    #[test]
    fn suspend_releases_and_does_not_arm_the_watchdog() {
        let mut rig = rig("suspend", &[("pwm2", manual(60.0))], Vec::new());
        rig.runtime.open_hardware();
        rig.runtime.tick();
        rig.runtime.suspend();
        assert_eq!(reg(&rig, CHA1_CONTROL) & AUTO, AUTO);
        rig.runtime.tick();
        thread::sleep(Duration::from_millis(30));
        assert_eq!(
            rig.runtime.shared.heartbeat.stalled_for(Duration::ZERO),
            None
        );
        assert!(matches!(rig.runtime.resume(), Flow::Continue));
        rig.runtime.tick();
        assert_eq!(reg(&rig, CHA1_CONTROL) & AUTO, 0);
    }

    #[test]
    fn a_stop_interrupts_the_resume_backoff() {
        let mut rig = rig("resume-stop", &[("pwm2", manual(60.0))], Vec::new());
        rig.runtime.open_hardware();
        rig.runtime.tick();
        rig.runtime.suspend();
        rig.fake.with(|chip| chip.chip_id = 0xFFFF);
        rig.messages.send(LoopMessage::Stop).unwrap();
        let started = Instant::now();
        assert!(matches!(rig.runtime.resume(), Flow::Stop));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_failed_resume_gives_up_within_the_budget() {
        let mut rig = rig("resume-budget", &[("pwm2", manual(60.0))], Vec::new());
        rig.runtime.open_hardware();
        rig.runtime.tick();
        rig.runtime.suspend();
        rig.fake.with(|chip| chip.chip_id = 0xFFFF);
        let started = Instant::now();
        assert!(matches!(rig.runtime.resume(), Flow::Continue));
        assert!(started.elapsed() < RESUME_BUDGET + Duration::from_secs(1));
        assert!(rig.runtime.hardware.is_none());
    }

    #[test]
    fn a_failed_reinitialisation_releases_fans_before_dropping_hardware() {
        let mut rig = rig("reinit", &[("pwm2", manual(60.0))], Vec::new());
        rig.runtime.open_hardware();
        rig.runtime.tick();
        rig.fake.with(|chip| {
            chip.chip_id = 0xFFFF;
            chip.unreadable_registers.push(0x0D);
        });
        for _ in 0..READ_FAILURES_BEFORE_REINIT {
            rig.runtime.tick();
        }
        assert!(rig.runtime.hardware.is_none());
        assert_eq!(reg(&rig, CHA1_CONTROL) & AUTO, AUTO);
    }

    #[test]
    fn an_unconfirmed_release_is_never_reported_as_released() {
        let mut rig = rig("unreleased", &[("pwm2", manual(60.0))], Vec::new());
        rig.runtime.open_hardware();
        rig.runtime.tick();
        rig.fake.with(|chip| {
            chip.chip_id = 0xFFFF;
            chip.unreadable_registers.extend([0x0D, CHA1_CONTROL]);
        });
        for _ in 0..READ_FAILURES_BEFORE_REINIT {
            rig.runtime.tick();
        }
        assert!(rig.runtime.hardware.is_none());
        assert!(!rig.runtime.shutdown());
    }

    #[test]
    fn a_missing_driver_is_reported_and_retried_on_request() {
        let mut rig = rig("driver", &[("pwm2", manual(60.0))], Vec::new());
        let installed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&installed);
        let mut open = opener(rig.fake.clone(), "driver-open");
        rig.runtime.opener = Box::new(move |snapshot: &Path| {
            if flag.load(std::sync::atomic::Ordering::SeqCst) {
                open(snapshot)
            } else {
                Err(HwError::PawnIoMissing)
            }
        });
        rig.runtime.open_hardware();
        rig.runtime.publish();
        let status = rig.runtime.shared.state().unwrap().hardware.clone();
        assert_eq!(status, HardwareStatus::DriverMissing);
        installed.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(
            rig.runtime.handle(LoopMessage::RetryHardware),
            Flow::Continue
        ));
        let status = rig.runtime.shared.state().unwrap().hardware.clone();
        assert_eq!(status, HardwareStatus::Ready);
    }

    #[test]
    fn conflicting_tools_keep_startup_hands_off() {
        let mut rig = rig(
            "conflict",
            &[("pwm2", manual(60.0))],
            vec!["FanControl.exe".to_owned()],
        );
        rig.fake.with(|chip| {
            chip.banks[0][usize::from(CPU_CONTROL)] = 0x00;
            chip.banks[0][0x63] = 0x55;
        });
        rig.runtime.open_hardware();
        rig.runtime.tick();
        assert_eq!(reg(&rig, CPU_CONTROL), 0x00);
        assert_eq!(reg(&rig, 0x63), 0x55);
        assert_eq!(reg(&rig, CHA1_CONTROL) & AUTO, AUTO);
        assert!(rig.runtime.engine.is_monitor_only());
    }

    #[test]
    fn unknown_critical_sensors_are_rejected() {
        let mut rig = rig("critical", &[], Vec::new());
        rig.runtime.open_hardware();
        rig.runtime.tick();
        let mut config = rig.runtime.engine.config().clone();
        config.safety.critical_sensor = "gpu/missing".into();
        assert!(rig.runtime.replace_config(config).is_err());
        let mut config = rig.runtime.engine.config().clone();
        config.safety.critical_sensor = "board/temp1".into();
        assert!(rig.runtime.replace_config(config).is_ok());
    }

    fn gpu_reading(celsius: Option<f32>) -> Vec<SensorReading> {
        vec![SensorReading {
            id: "gpu/test".into(),
            label: "Test GPU".into(),
            celsius,
        }]
    }

    fn extras_with_gpu(celsius: Option<f32>) -> ExtraSensors {
        let mut cache = SensorCache::default();
        let source = cache.register(Duration::from_secs(60));
        cache.record(source, &Ok(gpu_reading(celsius)), Instant::now());
        ExtraSensors::from_cache(cache)
    }

    fn gpu_curve_config() -> Config {
        let mut config = config_with(&[(
            "pwm2",
            FanMode::Curve {
                curve: "gpu".into(),
            },
        )]);
        config.curves.push(CurveDef {
            id: "gpu".into(),
            name: "GPU".into(),
            kind: CurveKind::Graph {
                sensor: "gpu/test".into(),
                points: vec![CurvePoint::new(30.0, 40.0), CurvePoint::new(80.0, 100.0)],
            },
            tuning: Tuning::default(),
        });
        config
    }

    #[test]
    fn extra_sensors_are_published_even_without_hardware() {
        let mut rig = rig("extras-none", &[], Vec::new());
        rig.runtime = rig.runtime.with_extra_sensors(extras_with_gpu(Some(48.0)));
        rig.runtime.publish();
        let state = rig.runtime.shared.state().unwrap();
        assert_eq!(state.readings.temperature(&"gpu/test".into()), Some(48.0));
    }

    #[test]
    fn a_curve_on_a_failing_extra_sensor_goes_to_full_speed() {
        let mut rig = rig("extras-fail", &[], Vec::new());
        let extras = extras_with_gpu(Some(40.0));
        rig.runtime = rig.runtime.with_extra_sensors(extras.clone());
        rig.runtime.open_hardware();
        rig.runtime.replace_config(gpu_curve_config()).unwrap();
        rig.runtime.tick();
        assert_eq!(reg(&rig, CHA1_CONTROL) & AUTO, 0);
        assert!(reg(&rig, CHA1_DUTY) < Duty::new(100.0).to_raw());

        extras.with_cache(|cache| cache.record(0, &Err("lost".into()), Instant::now()));
        let ticks = rig.runtime.engine.config().safety.sensor_failure_ticks;
        for _ in 0..=ticks {
            rig.runtime.tick();
        }
        let fan = rig
            .runtime
            .last_outcome
            .fans
            .iter()
            .find(|fan| fan.id.as_str() == "pwm2")
            .unwrap();
        assert!(matches!(fan.state, FanState::SensorFallback(_)));
        assert_eq!(reg(&rig, CHA1_DUTY), Duty::new(100.0).to_raw());
    }

    #[test]
    fn an_extra_critical_sensor_counts_as_present() {
        let mut rig = rig("extras-critical", &[], Vec::new());
        rig.runtime = rig.runtime.with_extra_sensors(extras_with_gpu(Some(45.0)));
        rig.runtime.open_hardware();
        rig.runtime.tick();
        let mut config = rig.runtime.engine.config().clone();
        config.safety.critical_sensor = "gpu/test".into();
        rig.runtime.replace_config(config).unwrap();
        for _ in 0..5 {
            rig.runtime.tick();
        }
        assert!(!rig.runtime.last_outcome.critical_sensor_missing);
        assert!(!rig.runtime.last_outcome.critical);
    }

    #[test]
    fn invalid_configs_fall_back_to_defaults_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config.toml"),
            snapshot: dir.path().join("snapshot.toml"),
        };
        assert_eq!(load_config(&paths), (Config::default(), None));

        std::fs::write(&paths.config, "interval_ms = 1").unwrap();
        let (config, warning) = load_config(&paths);
        assert_eq!(config, Config::default());
        assert!(warning.unwrap().contains("invalid"));

        std::fs::write(&paths.config, "not toml [").unwrap();
        assert!(
            load_config(&paths)
                .1
                .unwrap()
                .contains("could not be loaded")
        );
    }

    fn hands_off_lifecycle<P: fanforge_hw::port::PortIo + Clone + 'static>(tag: &str, io: P) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config.toml"),
            snapshot: dir.path().join("snapshot.toml"),
        };
        store::save_toml_atomic(&paths.config, &config_with(&[("pwm2", manual(60.0))]), 0).unwrap();
        let name = format!(
            r"Local\fanforge-runtime-hands-off-{tag}-{}",
            std::process::id()
        );
        let chip_io = io.clone();
        let chip_name = name.clone();
        let opener: Opener<Hardware<P>> = Box::new(move |snapshot: &Path| {
            let chip = SuperIo::probe(
                chip_io.clone(),
                Slot::Primary,
                NamedMutex::open(&chip_name)?,
            )?;
            Hardware::from_parts(
                chip,
                Err("no CPU in tests".to_owned()),
                BoardIdentity::default(),
                snapshot,
            )
        });
        let (_messages, rx) = mpsc::channel();
        let conflicts = ConflictMonitor::with_processes(Box::new(|| Ok(Vec::new())));
        let mut runtime = Runtime::new(
            paths.clone(),
            Arc::new(Shared::default()),
            rx,
            opener,
            conflicts,
        );
        runtime.open_hardware();
        runtime.tick();
        runtime.tick();
        assert!(runtime.engine.is_monitor_only());
        let state = runtime.shared.state().unwrap();
        assert!(state.monitor_only);
        assert!(!state.board.as_ref().unwrap().fan_control);
        assert!(!runtime.owns_any());
        runtime.suspend();
        assert!(matches!(runtime.resume(), Flow::Continue));
        runtime.tick();
        runtime.reinitialize_after_failure();
        runtime.drop_hardware("test".to_owned());
        runtime.open_hardware();
        runtime.tick();
        assert!(runtime.shutdown());
        drop(runtime);

        let mut chip = SuperIo::probe(io, Slot::Primary, NamedMutex::open(&name).unwrap()).unwrap();
        assert!(matches!(
            chip.set_duty(1, 0x20),
            Err(HwError::Unverified(_))
        ));
        crate::failsafe::release_with(&mut chip, &paths.snapshot, Duration::from_secs(1)).unwrap();
        crate::failsafe::release_preferring(
            Some(&mut chip),
            &paths.snapshot,
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(!paths.snapshot.exists());
    }

    fn ite_write_allowed(
        fake: &FakeIte,
        bank_select: bool,
        (port, value, index): (u16, u8, u8),
    ) -> bool {
        match port {
            0x2E => true,
            0x2F => matches!(index, 0x07 | 0x02),
            p if p == fake.base + 5 => true,
            p if p == fake.base + 6 => bank_select && index == 0x06 && value & 0x60 == 0,
            _ => false,
        }
    }

    #[test]
    fn an_unverified_ite_chip_is_never_written_through_the_whole_lifecycle() {
        let fake = SharedFake::new(FakeIte::chip(0x8686));
        hands_off_lifecycle("8686", fake.clone());
        fake.with(|chip| {
            assert!(!chip.writes.is_empty());
            for write in chip.writes.clone() {
                assert!(ite_write_allowed(chip, false, write), "{write:x?}");
            }
        });
    }

    #[test]
    fn the_it8665e_only_ever_selects_bank_zero() {
        let mut it8665e = FakeIte::chip(0x8665);
        it8665e.banks[0][0x00] = 0;
        it8665e.bank_register = 0x20;
        let fake = SharedFake::new(it8665e);
        hands_off_lifecycle("8665", fake.clone());
        fake.with(|chip| {
            assert!(
                chip.writes
                    .iter()
                    .any(|&(port, _, index)| port == chip.base + 6 && index == 0x06)
            );
            for write in chip.writes.clone() {
                assert!(ite_write_allowed(chip, true, write), "{write:x?}");
            }
        });
    }

    #[test]
    fn an_unverified_nuvoton_chip_only_sees_bank_selects() {
        let mut nct = FakeNct::new(0xD4, 0x2B);
        nct.bank_register = 0x04;
        let fake = SharedNct::new(nct);
        hands_off_lifecycle("6798", fake.clone());
        fake.with(|chip| {
            assert_eq!(chip.bank_register, 0x04);
            for (port, value, index) in chip.writes.clone() {
                let allowed = match port {
                    0x2E => true,
                    0x2F => matches!(index, 0x07 | OUTSIDE_CONFIG_MODE),
                    p if p == chip.base + 5 => true,
                    p if p == chip.base + 6 => index == 0x4E,
                    _ => false,
                };
                assert!(allowed, "{port:#x} {value:#x} {index:#x}");
            }
        });
    }
}
