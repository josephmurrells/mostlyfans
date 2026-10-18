use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use fanforge_core::config::Config;
use fanforge_core::engine::{Engine, TickOutcome};
use fanforge_core::paths;
use fanforge_core::protocol::{HardwareStatus, ServiceState};
use fanforge_core::store::{self, CONFIG_BACKUPS};
use fanforge_core::telemetry::Readings;
use fanforge_hw::HwError;

use crate::backend::Backend;
use crate::conflicts::ConflictMonitor;
use crate::shared::{LoopMessage, Shared};

pub const IDENTIFY_DURATION: Duration = Duration::from_secs(3);
const HARDWARE_RETRY: Duration = Duration::from_secs(10);
const RESUME_ATTEMPTS: u32 = 5;
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
    engine: Engine,
    opener: Opener<B>,
    hardware: Option<B>,
    hardware_error: Option<String>,
    next_hardware_attempt: Instant,
    suspended: bool,
    read_failures: u32,
    lock_skips: u32,
    conflicts: ConflictMonitor,
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
            engine: Engine::new(config, Default::default()),
            opener,
            hardware: None,
            hardware_error: None,
            next_hardware_attempt: now,
            suspended: false,
            read_failures: 0,
            lock_skips: 0,
            conflicts,
            config_warning,
            last_outcome: TickOutcome::default(),
            last_readings: Readings::default(),
            last_tick: now,
        }
    }

    pub fn run(mut self) {
        self.open_hardware();
        self.beat();
        self.publish();
        loop {
            let deadline = Instant::now() + self.interval();
            loop {
                let wait = deadline.saturating_duration_since(Instant::now());
                match self.messages.recv_timeout(wait) {
                    Ok(message) => {
                        if let Flow::Stop = self.handle(message) {
                            self.shutdown();
                            return;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => {
                        self.shutdown();
                        return;
                    }
                }
            }
            self.tick();
        }
    }

    fn interval(&self) -> Duration {
        Duration::from_millis(self.engine.config().interval_ms)
    }

    fn beat(&self) {
        self.shared
            .heartbeat
            .beat(self.interval(), self.hardware.is_some());
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
            LoopMessage::Resume => self.resume(),
            LoopMessage::Stop => return Flow::Stop,
        }
        Flow::Continue
    }

    fn replace_config(&mut self, config: Config) -> Result<(), String> {
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
        if self.suspended {
            self.publish();
            return;
        }
        self.beat();
        if self.hardware.is_none() && now >= self.next_hardware_attempt {
            self.open_hardware();
        }

        let findings = self.conflicts.poll().clone();
        let read_only = self
            .hardware
            .as_ref()
            .is_some_and(|hardware| hardware.board_info().read_only);
        self.engine
            .set_monitor_only(read_only || !findings.controlling.is_empty());

        let Some(hardware) = self.hardware.as_mut() else {
            self.publish();
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
                self.publish();
                return;
            }
            Err(error) => {
                self.read_failures += 1;
                tracing::warn!(%error, failures = self.read_failures, "hardware read failed");
                if self.read_failures >= READ_FAILURES_BEFORE_REINIT {
                    self.reinitialize_after_failure();
                }
                self.publish();
                return;
            }
        };

        let outcome = self.engine.tick(&readings, elapsed);
        for command in &outcome.commands {
            if let Err(error) = hardware.apply(command) {
                tracing::warn!(fan = %command.fan, %error, "failed to apply fan command");
                self.engine
                    .record_apply_error(&command.fan, &error.to_string());
            }
        }
        self.last_outcome = outcome;
        self.last_readings = readings;
        self.publish();
    }

    fn open_hardware(&mut self) {
        self.next_hardware_attempt = Instant::now() + HARDWARE_RETRY;
        let mut hardware = match (self.opener)(&self.paths.snapshot) {
            Ok(hardware) => hardware,
            Err(error) => {
                let message = error.to_string();
                if self.hardware_error.as_deref() != Some(message.as_str()) {
                    tracing::error!(%error, "hardware unavailable");
                }
                self.hardware_error = Some(message);
                return;
            }
        };
        let read_only = hardware.board_info().read_only;
        let conflicting = !self.conflicts.poll().controlling.is_empty();
        let take_control = !read_only && !conflicting;
        if let Err(error) = hardware.initialize_channels(take_control) {
            tracing::error!(%error, "failed to return fans to BIOS control at startup");
        }
        let info = hardware.board_info();
        tracing::info!(
            board = %info.identity.product,
            profile = %info.profile_name,
            chip = format_args!("{:#06x}", info.chip_id),
            take_control,
            "hardware ready"
        );
        self.engine.set_limits(hardware.fan_limits());
        self.engine.forget_hardware_state();
        self.hardware = Some(hardware);
        self.hardware_error = None;
        self.read_failures = 0;
    }

    fn drop_hardware(&mut self, reason: String) {
        if let Some(mut hardware) = self.hardware.take()
            && let Err(error) = hardware.release_all()
        {
            tracing::error!(%error, "could not release fans through the failing handle");
            match (self.opener)(&self.paths.snapshot) {
                Ok(mut fresh) => {
                    if let Err(error) = fresh.release_all() {
                        tracing::error!(%error, "could not release fans through a fresh handle");
                    }
                }
                Err(error) => tracing::error!(%error, "could not reopen hardware to release fans"),
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
        self.publish();
    }

    fn resume(&mut self) {
        if !self.suspended {
            return;
        }
        tracing::info!("system resumed; reinitialising hardware");
        self.shared.heartbeat.pause();
        let mut ready = false;
        if let Some(hardware) = self.hardware.as_mut() {
            let mut backoff = RESUME_FIRST_BACKOFF;
            for attempt in 1..=RESUME_ATTEMPTS {
                match hardware.reinitialize() {
                    Ok(()) => {
                        ready = true;
                        break;
                    }
                    Err(error) => {
                        tracing::warn!(attempt, %error, "hardware not ready after resume");
                        if attempt < RESUME_ATTEMPTS {
                            thread::sleep(backoff);
                            backoff *= 2;
                        }
                    }
                }
            }
            if ready && let Err(error) = hardware.release_all() {
                tracing::warn!(%error, "failed to reset channels after resume");
            }
        }
        if !ready && self.hardware.is_some() {
            self.drop_hardware("hardware did not recover after resume".to_owned());
            self.next_hardware_attempt = Instant::now();
        }
        self.engine.forget_hardware_state();
        self.suspended = false;
        self.last_tick = Instant::now();
        self.beat();
    }

    fn shutdown(&mut self) {
        self.shared.heartbeat.pause();
        if let Some(mut hardware) = self.hardware.take() {
            match hardware.release_all() {
                Ok(()) => tracing::info!("all fans returned to BIOS control"),
                Err(error) => {
                    tracing::error!(%error, "failed to release every fan during shutdown");
                }
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
        let state = ServiceState {
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
        };
        if let Ok(mut slot) = self.shared.state.write() {
            *slot = Some(state);
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
mod tests {
    use std::panic::{self, AssertUnwindSafe};
    use std::sync::mpsc;

    use fanforge_core::Duty;
    use fanforge_core::board::BoardIdentity;
    use fanforge_core::config::FanMode;
    use fanforge_hw::board::embedded_profiles;
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
        _messages: mpsc::Sender<LoopMessage>,
    }

    fn opener(fake: SharedFake, tag: &str) -> Opener<Hardware<SharedFake>> {
        let name = format!(r"Local\fanforge-runtime-{tag}-{}", std::process::id());
        Box::new(move |snapshot: &Path| {
            let chip = It8655e::probe(fake.clone(), NamedMutex::open(&name)?)?;
            Hardware::from_parts(
                chip,
                Err("no CPU in tests".to_owned()),
                BoardIdentity::default(),
                embedded_profiles().remove(0),
                snapshot,
            )
        })
    }

    fn rig(tag: &str, modes: &[(&str, FanMode)], processes: Vec<String>) -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config: dir.path().join("config.toml"),
            snapshot: dir.path().join("snapshot.toml"),
        };
        let mut config = Config::default();
        config.safety.critical_sensor = "board/temp1".into();
        let profile = config.profiles.get_mut("Balanced").unwrap();
        for (fan, mode) in modes {
            profile.fans.insert((*fan).into(), mode.clone());
        }
        store::save_toml_atomic(&paths.config, &config, 0).unwrap();
        let fake = SharedFake::new(FakeIte::new());
        let (tx, rx) = mpsc::channel();
        let conflicts = ConflictMonitor::with_source(Box::new(move || Ok(processes.clone())));
        let runtime = Runtime::new(
            paths,
            Arc::new(Shared::new()),
            rx,
            opener(fake.clone(), tag),
            conflicts,
        );
        Rig {
            runtime,
            fake,
            _dir: dir,
            _messages: tx,
        }
    }

    fn manual(duty: f32) -> FanMode {
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
        rig.runtime.shutdown();
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
        rig.runtime.resume();
        rig.runtime.tick();
        assert_eq!(reg(&rig, CHA1_CONTROL) & AUTO, 0);
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
}
