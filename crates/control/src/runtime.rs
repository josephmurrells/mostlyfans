use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use fanforge_core::FanId;
use fanforge_core::config::Config;
use fanforge_core::engine::{Engine, TickOutcome};
use fanforge_core::state::{ControlState, HardwareStatus};
use fanforge_core::store::{self, CONFIG_BACKUPS};
use fanforge_core::telemetry::Readings;
use fanforge_hw::HwError;
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
const LOCK_SKIPS_BEFORE_CALIBRATION_ABORT: u32 = 3;

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
    config_warning: Option<String>,
    calibration_warning: Option<String>,
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
        let Loaded {
            config,
            config_warning,
            calibration_warning,
        } = load_config(&paths);
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
            config_warning,
            calibration_warning,
            last_outcome: TickOutcome::default(),
            last_readings: Readings::default(),
            last_tick: now,
        }
    }

    pub fn with_signals(mut self, signals: SignalSink) -> Self {
        self.signals = Some(signals);
        self
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
            LoopMessage::Calibrate(fan, reply) => {
                reply.send(self.start_calibration(fan)).ok();
            }
            LoopMessage::CancelCalibration => {
                self.engine.cancel_calibration(None);
                self.publish();
            }
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

    fn start_calibration(&mut self, fan: FanId) -> Result<(), String> {
        if self.hardware.is_none() || self.suspended || self.shared.is_halted() {
            return Err("the fan controller is not available".to_owned());
        }
        if self.read_failures > 0 || self.lock_skips > 0 {
            return Err("the latest hardware read failed".to_owned());
        }
        self.engine.request_calibration(fan.clone())?;
        tracing::info!(%fan, "calibration requested");
        self.publish();
        Ok(())
    }

    fn cancel_calibration(&mut self, reason: &str) {
        if self.engine.is_calibrating() {
            tracing::warn!(reason, "calibration aborted");
            self.engine.cancel_calibration(Some(reason.to_owned()));
        }
    }

    fn store_calibration(&mut self) {
        let Some((fan, result)) = self.engine.take_calibration_result() else {
            return;
        };
        let mut config = self.engine.config().clone();
        config.calibration.insert(fan.clone(), result.clone());
        if let Err(error) = config.validate() {
            tracing::error!(%fan, %error, "calibration result rejected");
            self.calibration_warning = Some(format!(
                "the calibration of `{fan}` could not be stored ({error})"
            ));
            return;
        }
        self.engine.set_calibration(fan.clone(), result);
        let saved = if self.config_warning.is_some() {
            Err("the saved configuration is not in use".to_owned())
        } else {
            store::save_toml_atomic(&self.paths.config, &config, CONFIG_BACKUPS)
                .map_err(|error| error.to_string())
        };
        match saved {
            Ok(()) => {
                tracing::info!(%fan, "calibration stored");
                self.calibration_warning = None;
            }
            Err(error) => {
                tracing::error!(%fan, %error, "calibration result not saved");
                self.calibration_warning = Some(format!(
                    "the calibration of `{fan}` applies until FanForge restarts but was not saved ({error})"
                ));
            }
        }
    }

    fn replace_config(&mut self, mut config: Config) -> Result<(), String> {
        self.cancel_calibration("the configuration changed");
        self.store_calibration();
        config.calibration = self.engine.config().calibration.clone();
        config.validate().map_err(|error| error.to_string())?;
        let sensor = &config.safety.critical_sensor;
        if self.hardware.is_some()
            && !self.last_readings.sensors.is_empty()
            && !self
                .last_readings
                .sensors
                .iter()
                .any(|reading| &reading.id == sensor)
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
            self.cancel_calibration("fan control is not running");
            self.publish();
            return;
        }
        if self.hardware.is_none() && now >= self.next_hardware_attempt {
            self.open_hardware();
        }

        let findings = self.conflicts.poll().clone();
        self.engine
            .set_monitor_only(!findings.controlling.is_empty());

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
                if self.lock_skips >= LOCK_SKIPS_BEFORE_CALIBRATION_ABORT {
                    self.cancel_calibration("the hardware bus stayed busy");
                }
                if self.lock_skips == LOCK_SKIPS_BEFORE_WARNING {
                    tracing::warn!(mutex = %name, "bus has been busy for many cycles");
                }
                return;
            }
            Err(error) => {
                self.cancel_calibration("a hardware read failed");
                self.read_failures += 1;
                tracing::warn!(%error, failures = self.read_failures, "hardware read failed");
                if self.read_failures >= READ_FAILURES_BEFORE_REINIT {
                    self.reinitialize_after_failure();
                }
                return;
            }
        };

        let outcome = self.engine.tick(&readings, elapsed);
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
        self.store_calibration();
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
        self.cancel_calibration("the hardware was reopened");
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
        self.cancel_calibration("the hardware stopped responding");
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
        self.cancel_calibration("the hardware was reinitialised");
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
        self.cancel_calibration("the system is suspending");
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
        self.cancel_calibration("fan control is stopping");
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
        let mut warnings: Vec<String> = self
            .config_warning
            .iter()
            .chain(&self.calibration_warning)
            .cloned()
            .collect();
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
        let calibration = self.engine.calibration_view();
        warnings.extend(calibration.not_restarting.iter().map(|fan| {
            format!("fan `{fan}` did not restart during calibration, even at 100%; check that it is connected and working")
        }));
        let state = ControlState {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            hardware: hardware_status,
            board,
            readings: self.last_readings.clone(),
            fans: self.last_outcome.fans.clone(),
            limits: self.engine.limits().clone(),
            config: self.engine.config().clone(),
            critical: self.last_outcome.critical,
            monitor_only: self.engine.is_monitor_only(),
            conflicts: findings.controlling,
            warnings,
            calibration,
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

struct Loaded {
    config: Config,
    config_warning: Option<String>,
    calibration_warning: Option<String>,
}

fn load_config(paths: &Paths) -> Loaded {
    let defaults = |warning: String| Loaded {
        config: Config::default(),
        config_warning: Some(warning),
        calibration_warning: None,
    };
    match store::load_toml::<Config>(&paths.config) {
        Ok(Some(config)) => match config.validate() {
            Ok(()) => Loaded {
                config,
                config_warning: None,
                calibration_warning: None,
            },
            Err(error) => {
                let trimmed = Config {
                    calibration: Default::default(),
                    ..config
                };
                if trimmed.validate().is_ok() {
                    tracing::error!(%error, "saved calibration is invalid; dropping it");
                    return Loaded {
                        config: trimmed,
                        config_warning: None,
                        calibration_warning: Some(format!(
                            "saved fan calibration is invalid ({error}) and was ignored"
                        )),
                    };
                }
                tracing::error!(%error, "configuration is invalid; using defaults");
                defaults(format!(
                    "saved configuration is invalid ({error}); using defaults"
                ))
            }
        },
        Ok(None) => Loaded {
            config: Config::default(),
            config_warning: None,
            calibration_warning: None,
        },
        Err(error) => {
            tracing::error!(%error, "configuration could not be loaded; using defaults");
            defaults(format!(
                "saved configuration could not be loaded ({error}); using defaults"
            ))
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
    use fanforge_core::calibration::{CalibrationOutcome, FanCalibration, MAX_POINTS, Phase};
    use fanforge_core::config::{FanMode, MAX_FAN_ENTRIES};
    use fanforge_hw::ite::It8655e;
    use fanforge_hw::ite::fake::{FakeIte, SharedFake};
    use fanforge_hw::mutex::NamedMutex;
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
            let chip = It8655e::probe(fake.clone(), NamedMutex::open(&name)?)?;
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

    #[test]
    fn invalid_configs_fall_back_to_defaults_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config.toml"),
            snapshot: dir.path().join("snapshot.toml"),
        };
        let loaded = load_config(&paths);
        assert_eq!(loaded.config, Config::default());
        assert_eq!(loaded.config_warning, None);

        std::fs::write(&paths.config, "interval_ms = 1").unwrap();
        let loaded = load_config(&paths);
        assert_eq!(loaded.config, Config::default());
        assert!(loaded.config_warning.unwrap().contains("invalid"));

        std::fs::write(&paths.config, "not toml [").unwrap();
        assert!(
            load_config(&paths)
                .config_warning
                .unwrap()
                .contains("could not be loaded")
        );
    }

    const CHA1_TACH_LOW: u8 = 0x0E;
    const CHA1_TACH_HIGH: u8 = 0x19;
    const BOARD_TEMP2: usize = 0x2A;

    fn set_rpm(rig: &Rig, rpm: u32) {
        let count = 675_000u32.checked_div(rpm).map_or(u16::MAX, |count| {
            u16::try_from(count).unwrap_or(u16::MAX - 1)
        });
        rig.fake.with(|chip| {
            chip.banks[0][usize::from(CHA1_TACH_LOW)] = count as u8;
            chip.banks[0][usize::from(CHA1_TACH_HIGH)] = (count >> 8) as u8;
        });
    }

    fn step(rig: &mut Rig, stops_below: f32) {
        rig.runtime.last_tick = Instant::now() - Duration::from_secs(1);
        rig.runtime.tick();
        let (manual, duty) = rig.fake.with(|chip| {
            (
                chip.reg(CHA1_CONTROL) & AUTO == 0,
                Duty::from_raw(chip.reg(CHA1_DUTY)).percent(),
            )
        });
        let rpm = match manual {
            false => 1500,
            true if duty < stops_below => 0,
            true => (duty * 15.0) as u32,
        };
        set_rpm(rig, rpm);
    }

    fn started(tag: &str, modes: &[(&str, FanMode)]) -> Rig {
        let mut rig = rig(tag, modes, Vec::new());
        rig.fake.with(|chip| chip.banks[0][BOARD_TEMP2] = 38);
        rig.runtime.open_hardware();
        step(&mut rig, 0.0);
        rig.runtime.start_calibration("pwm2".into()).unwrap();
        step(&mut rig, 0.0);
        assert!(rig.runtime.engine.calibration_view().active.is_some());
        rig
    }

    fn last_outcome(rig: &Rig) -> CalibrationOutcome {
        rig.runtime.engine.calibration_view().last.unwrap().outcome
    }

    fn assert_aborted(rig: &Rig, text: &str) {
        match last_outcome(rig) {
            CalibrationOutcome::Aborted(reason) => assert!(reason.contains(text), "{reason}"),
            other => panic!("expected an abort, got {other:?}"),
        }
    }

    fn saved(rig: &Rig) -> Config {
        store::load_toml::<Config>(&rig.runtime.paths.config)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn a_calibration_run_is_stored_and_survives_a_stale_config() {
        let mut rig = started("cal-store", &[("pwm2", manual(60.0))]);
        for _ in 0..1000 {
            if !rig.runtime.engine.is_calibrating() {
                break;
            }
            step(&mut rig, 37.0);
        }
        assert!(matches!(
            last_outcome(&rig),
            CalibrationOutcome::Completed(_)
        ));
        let stored = saved(&rig).calibration[&FanId::from("pwm2")].clone();
        assert_eq!(stored.stop_duty, Some(Duty::new(35.0)));
        assert_eq!(
            rig.runtime.engine.config().calibration_for(&"pwm2".into()),
            Some(&stored)
        );
        assert_eq!(reg(&rig, CHA1_DUTY), Duty::FULL.to_raw());

        let stale = config_with(&[("pwm2", manual(70.0))]);
        assert!(stale.calibration.is_empty());
        rig.runtime.replace_config(stale).unwrap();
        assert_eq!(saved(&rig).calibration[&FanId::from("pwm2")], stored);
        assert_eq!(
            rig.runtime.engine.config().calibration_for(&"pwm2".into()),
            Some(&stored)
        );
        let state = rig.runtime.shared.state().unwrap();
        assert!(state.calibration.active.is_none());
        assert_eq!(state.config.calibration[&FanId::from("pwm2")], stored);
    }

    #[test]
    fn calibration_is_refused_when_control_cannot_run_it() {
        let mut rig = rig("cal-refuse", &[("pwm2", manual(60.0))], Vec::new());
        assert!(rig.runtime.start_calibration("pwm2".into()).is_err());
        rig.runtime.open_hardware();
        step(&mut rig, 0.0);
        rig.runtime.suspend();
        assert!(rig.runtime.start_calibration("pwm2".into()).is_err());
        assert!(matches!(rig.runtime.resume(), Flow::Continue));
        rig.runtime.start_calibration("pwm2".into()).unwrap();
        assert!(rig.runtime.start_calibration("pwm1".into()).is_err());
        rig.runtime.shared.halt();
        step(&mut rig, 0.0);
        assert_aborted(&rig, "not running");
        assert!(rig.runtime.start_calibration("pwm2".into()).is_err());

        let mut monitor = self::rig(
            "cal-monitor",
            &[("pwm2", manual(60.0))],
            vec!["FanControl.exe".to_owned()],
        );
        monitor.runtime.open_hardware();
        step(&mut monitor, 0.0);
        assert!(monitor.runtime.start_calibration("pwm2".into()).is_err());
    }

    #[test]
    fn suspend_and_stop_cancel_a_run() {
        let mut rig = started("cal-suspend", &[("pwm2", manual(60.0))]);
        rig.runtime.suspend();
        assert_aborted(&rig, "suspending");
        assert_eq!(reg(&rig, CHA1_CONTROL) & AUTO, AUTO);

        let mut rig = started("cal-stop", &[("pwm2", manual(60.0))]);
        assert!(rig.runtime.shutdown());
        assert_aborted(&rig, "stopping");
        assert_eq!(reg(&rig, CHA1_CONTROL) & AUTO, AUTO);
    }

    fn assert_recovers(rig: &mut Rig, then: u8) {
        for _ in 0..3 {
            step(rig, 0.0);
            assert_eq!(reg(rig, CHA1_DUTY), Duty::FULL.to_raw());
        }
        step(rig, 0.0);
        assert_eq!(reg(rig, CHA1_DUTY), then);
    }

    #[test]
    fn a_config_change_cancels_and_recovers_at_full_speed() {
        let mut rig = started("cal-config", &[("pwm2", manual(60.0))]);
        for _ in 0..5 {
            step(&mut rig, 0.0);
        }
        assert!(reg(&rig, CHA1_DUTY) < Duty::FULL.to_raw());
        let config = rig.runtime.engine.config().clone();
        rig.runtime.replace_config(config).unwrap();
        assert_aborted(&rig, "configuration changed");
        assert_recovers(&mut rig, Duty::new(60.0).to_raw());
    }

    #[test]
    fn a_read_failure_cancels_and_recovers_at_full_speed() {
        let mut rig = started("cal-read", &[("pwm2", manual(60.0))]);
        for _ in 0..5 {
            step(&mut rig, 0.0);
        }
        rig.fake
            .with(|chip| chip.unreadable_registers.push(CHA1_TACH_LOW));
        step(&mut rig, 0.0);
        assert_aborted(&rig, "read failed");
        rig.fake.with(|chip| chip.unreadable_registers.clear());
        assert_recovers(&mut rig, Duty::new(60.0).to_raw());
    }

    #[test]
    fn a_busy_bus_cancels_only_after_three_skipped_ticks() {
        let mut rig = started("cal-busy", &[("pwm2", manual(60.0))]);
        let name = format!(r"Local\fanforge-runtime-cal-busy-{}", std::process::id());
        let (release, held) = mpsc::channel::<()>();
        let (ready_tx, ready) = mpsc::channel();
        let holder = thread::spawn(move || {
            let mutex = NamedMutex::open(&name).unwrap();
            let _guard = mutex.acquire(fanforge_hw::mutex::DEFAULT_TIMEOUT).unwrap();
            ready_tx.send(()).unwrap();
            held.recv().ok();
        });
        ready.recv().unwrap();
        for _ in 0..LOCK_SKIPS_BEFORE_CALIBRATION_ABORT - 1 {
            step(&mut rig, 0.0);
            assert!(rig.runtime.engine.is_calibrating());
        }
        step(&mut rig, 0.0);
        assert!(!rig.runtime.engine.is_calibrating());
        assert_aborted(&rig, "busy");
        release.send(()).unwrap();
        holder.join().unwrap();
    }

    #[test]
    fn a_config_change_during_the_start_search_keeps_the_stop_duty() {
        let mut rig = in_find_start("cal-partial");
        let mut stale = rig.runtime.engine.config().clone();
        stale.calibration.clear();
        rig.runtime.replace_config(stale).unwrap();
        assert_aborted(&rig, "configuration changed");
        let stored = saved(&rig).calibration[&FanId::from("pwm2")].clone();
        assert_eq!(stored.stop_duty, Some(Duty::new(20.0)));
        assert_eq!(stored.start_duty, None);
    }

    #[test]
    fn a_result_that_fails_validation_is_rejected() {
        let mut rig = started("cal-reject-run", &[("pwm2", manual(60.0))]);
        let mut fresh = rig.runtime.engine.config().clone();
        for index in 3..=MAX_FAN_ENTRIES + 2 {
            fresh
                .calibration
                .insert(FanId::for_channel(index), FanCalibration::default());
        }
        rig.runtime.engine.set_config(fresh);
        for _ in 0..1000 {
            if !rig.runtime.engine.is_calibrating() {
                break;
            }
            step(&mut rig, 37.0);
        }
        assert!(matches!(
            last_outcome(&rig),
            CalibrationOutcome::Completed(_)
        ));
        assert!(
            rig.runtime
                .calibration_warning
                .as_ref()
                .unwrap()
                .contains("could not be stored")
        );
        assert!(
            rig.runtime
                .engine
                .config()
                .calibration_for(&"pwm2".into())
                .is_none()
        );
        let change = config_with(&[("pwm2", manual(70.0))]);
        rig.runtime.replace_config(change).unwrap();
        assert_eq!(saved(&rig).mode(&"pwm2".into()), manual(70.0));
        assert_eq!(saved(&rig).calibration.len(), MAX_FAN_ENTRIES);
    }

    fn in_find_start(tag: &str) -> Rig {
        let mut rig = started(tag, &[("pwm2", manual(60.0))]);
        let mut settings = rig.runtime.engine.config().clone();
        settings.fans.insert(
            "pwm2".into(),
            fanforge_core::config::FanSettings {
                case_fan: true,
                allow_stop: true,
                ..Default::default()
            },
        );
        rig.runtime.replace_config(settings).unwrap();
        rig.runtime.start_calibration("pwm2".into()).unwrap();
        for _ in 0..1000 {
            let in_search = rig
                .runtime
                .engine
                .calibration_view()
                .active
                .is_some_and(|progress| progress.phase == Phase::FindStart);
            if in_search {
                return rig;
            }
            step(&mut rig, 23.0);
        }
        panic!("never reached the start search");
    }

    #[test]
    fn a_partial_result_from_a_suspend_is_stored_after_resume() {
        let mut rig = in_find_start("cal-partial-suspend");
        rig.runtime.suspend();
        assert_aborted(&rig, "suspending");
        assert!(saved(&rig).calibration.is_empty());
        assert!(matches!(rig.runtime.resume(), Flow::Continue));
        step(&mut rig, 0.0);
        let stored = saved(&rig).calibration[&FanId::from("pwm2")].clone();
        assert_eq!(stored.stop_duty, Some(Duty::new(20.0)));
    }

    #[test]
    fn a_partial_result_from_a_stop_is_dropped() {
        let mut rig = in_find_start("cal-partial-stop");
        assert!(rig.runtime.shutdown());
        assert!(saved(&rig).calibration.is_empty());
    }

    #[test]
    fn an_invalid_calibration_table_is_dropped_without_losing_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config.toml"),
            snapshot: dir.path().join("snapshot.toml"),
        };
        let mut config = config_with(&[("pwm2", manual(60.0))]);
        config.calibration.insert(
            "pwm2".into(),
            FanCalibration {
                points: vec![
                    fanforge_core::calibration::CalibrationPoint {
                        duty: Duty::FULL,
                        rpm: 1,
                    };
                    MAX_POINTS + 1
                ],
                ..FanCalibration::default()
            },
        );
        store::save_toml_atomic(&paths.config, &config, 0).unwrap();
        let loaded = load_config(&paths);
        assert_eq!(loaded.config.mode(&"pwm2".into()), manual(60.0));
        assert!(loaded.config.calibration.is_empty());
        assert_eq!(loaded.config_warning, None);
        assert!(loaded.calibration_warning.unwrap().contains("ignored"));
    }
}
