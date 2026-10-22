use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use fanforge_core::board::FanLimits;
use fanforge_core::config::Config;
use fanforge_core::engine::{Action, Engine, TickOutcome};
use fanforge_core::state::{ControlState, GpuState, HardwareStatus};
use fanforge_core::store::{self, CONFIG_BACKUPS};
use fanforge_core::telemetry::{Readings, SensorReading};
use fanforge_core::{FanId, FanKind};
use fanforge_hw::HwError;
use fanforge_hw::gpu_fans::{GpuFans, GpuReadings, GpuReleaser};
use fanforge_hw::sensors::ExtraSensors;
use fanforge_win::paths;

use crate::backend::Backend;
use crate::conflicts::ConflictMonitor;
use crate::guardian::{Signal, SignalSink};
use crate::shared::{LoopMessage, Shared};

pub const IDENTIFY_DURATION: Duration = Duration::from_secs(3);
pub const RESUME_BUDGET: Duration = Duration::from_secs(5);
pub const GPU_RELEASE_BUDGET: Duration = Duration::from_secs(2);
pub const GPU_SUSPEND_BUDGET: Duration = Duration::from_secs(1);
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

enum BoardRead {
    Fresh(Readings),
    Stale(Vec<SensorReading>),
    Missing,
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
    gpus: Option<GpuFans>,
    gpu_releaser: Option<GpuReleaser>,
    gpu_readings: GpuReadings,
    board_limits: BTreeMap<FanId, FanLimits>,
    last_board: Readings,
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
            extras: ExtraSensors::none(),
            gpus: None,
            gpu_releaser: None,
            gpu_readings: GpuReadings::default(),
            board_limits: BTreeMap::new(),
            last_board: Readings::default(),
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

    pub fn with_extra_sensors(mut self, extras: ExtraSensors) -> Self {
        self.extras = extras;
        self
    }

    pub fn with_gpu_fans(mut self, gpus: GpuFans) -> Self {
        self.gpus = Some(gpus);
        self
    }

    pub fn with_gpu_releaser(mut self, releaser: GpuReleaser) -> Self {
        self.gpu_releaser = Some(releaser);
        self
    }

    fn release_gpus(&self, budget: Duration) -> Result<(), String> {
        let Some(gpus) = &self.gpus else {
            return Ok(());
        };
        let started = Instant::now();
        let error = match gpus.release(budget / 2) {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        tracing::warn!(%error, "the GPU fan worker could not release; using the spare session");
        match &self.gpu_releaser {
            Some(releaser) => releaser.release(budget.saturating_sub(started.elapsed())),
            None => Err(error),
        }
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
            || self.gpus.as_ref().is_some_and(GpuFans::owns_any)
    }

    fn beat(&self) {
        let controlling = self.hardware.is_some() || !self.gpu_readings.fans.is_empty();
        self.shared.heartbeat.beat(self.interval(), controlling);
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
            LoopMessage::SetMonitorOnly(on, reply) => {
                tracing::info!(on, "monitor-only switch changed");
                self.shared.set_user_monitor_only(on);
                self.tick();
                reply.send(Ok(())).ok();
            }
            LoopMessage::RetryFan(fan, reply) => {
                let result = if self.engine.is_monitor_only_for(fan.kind()) {
                    Err("FanForge is in monitor-only mode".to_owned())
                } else {
                    tracing::info!(%fan, "retrying a fan after a fault");
                    self.engine.clear_fault(&fan);
                    self.tick();
                    Ok(())
                };
                reply.send(result).ok();
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
            self.cancel_calibration("fan control is not running");
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
        let fan_tools = !findings.controlling.is_empty();
        let paused = self.shared.user_monitor_only();
        self.engine
            .set_monitor_only(FanKind::Header, paused || fan_tools || unverified);
        self.engine.set_monitor_only(
            FanKind::Gpu,
            paused || fan_tools || !findings.gpu_controlling.is_empty(),
        );

        self.beat();
        self.control(elapsed);
        self.signal(Signal::Owning(self.owns_any()));
        self.publish();
    }

    fn read_board(&mut self) -> BoardRead {
        let Some(hardware) = self.hardware.as_mut() else {
            return BoardRead::Missing;
        };
        match hardware.read() {
            Ok(readings) => {
                self.read_failures = 0;
                self.lock_skips = 0;
                self.last_board = readings.clone();
                return BoardRead::Fresh(readings);
            }
            Err(HwError::LockTimeout(name)) => {
                self.lock_skips += 1;
                if self.lock_skips >= LOCK_SKIPS_BEFORE_CALIBRATION_ABORT {
                    self.cancel_calibration("the hardware bus stayed busy");
                }
                if self.lock_skips == LOCK_SKIPS_BEFORE_WARNING {
                    tracing::warn!(mutex = %name, "bus has been busy for many cycles");
                }
            }
            Err(error) => {
                self.cancel_calibration("a hardware read failed");
                self.read_failures += 1;
                tracing::warn!(%error, failures = self.read_failures, "hardware read failed");
                if self.read_failures >= READ_FAILURES_BEFORE_REINIT {
                    self.reinitialize_after_failure();
                }
            }
        }
        let recent = self.lock_skips < LOCK_SKIPS_BEFORE_WARNING
            && self.read_failures < READ_FAILURES_BEFORE_REINIT;
        if self.hardware.is_some() && recent {
            BoardRead::Stale(self.last_board.sensors.clone())
        } else {
            BoardRead::Missing
        }
    }

    fn apply_limits(&mut self) {
        let mut limits = self.board_limits.clone();
        limits.extend(self.gpu_readings.limits.clone());
        if &limits != self.engine.limits() {
            self.engine.set_limits(limits);
        }
    }

    fn control(&mut self, elapsed: Duration) {
        let board = self.read_board();
        let gpu = self
            .gpus
            .as_ref()
            .map(|gpus| gpus.readings(Instant::now(), self.interval()))
            .unwrap_or_default();
        let fresh = matches!(board, BoardRead::Fresh(_));
        let no_gpu_fans = gpu.fans.is_empty();
        self.gpu_readings = gpu;
        self.apply_limits();
        if !fresh && (no_gpu_fans || self.engine.is_calibrating()) {
            return;
        }
        let mut readings = match board {
            BoardRead::Fresh(readings) => readings,
            BoardRead::Stale(sensors) => Readings {
                sensors,
                fans: Vec::new(),
            },
            BoardRead::Missing => Readings::default(),
        };
        readings.fans.extend(self.gpu_readings.fans.iter().cloned());

        let mut inputs = readings.clone();
        inputs.sensors.extend(self.extras.readings(Instant::now()));
        let mut outcome = self.engine.tick(&inputs, elapsed);
        for command in &outcome.commands {
            if command.fan.kind() == FanKind::Gpu {
                if let Some(gpus) = &self.gpus {
                    gpus.request(&command.fan, command.action);
                }
                continue;
            }
            let Some(hardware) = self.hardware.as_mut() else {
                continue;
            };
            match hardware.apply(command, self.shared.halt_flag()) {
                Ok(()) | Err(HwError::Halted) => {}
                Err(error) => {
                    tracing::warn!(fan = %command.fan, %error, "failed to apply fan command");
                    if command.action == Action::Release {
                        self.engine.record_release_error(&command.fan);
                    } else {
                        self.engine
                            .record_apply_error(&command.fan, &error.to_string());
                    }
                }
            }
        }
        if let Some(gpus) = &self.gpus {
            for (fan, error) in gpus.take_errors() {
                tracing::warn!(%fan, %error, "failed to apply GPU fan command");
                self.engine.record_apply_error(&fan, &error);
            }
        }
        if !fresh {
            let board_fans = self.last_board.fans.iter().cloned();
            readings.fans = board_fans.chain(readings.fans).collect();
            outcome.fans.extend(
                self.last_outcome
                    .fans
                    .iter()
                    .filter(|status| status.id.kind() == FanKind::Header)
                    .cloned(),
            );
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
        self.board_limits = hardware.fan_limits();
        self.apply_limits();
        self.engine.forget_hardware_state();
        self.hardware = Some(hardware);
        self.hardware_error = None;
        self.driver_missing = false;
        self.read_failures = 0;
    }

    fn others_in_control(&mut self) -> bool {
        !self.conflicts.poll().controlling.is_empty()
    }

    fn drop_hardware(&mut self, reason: String) {
        self.cancel_calibration("the hardware stopped responding");
        let others = self.others_in_control();
        if let Some(mut hardware) = self.hardware.take()
            && let Err(error) = release(&mut hardware, others)
        {
            tracing::error!(%error, "could not release fans through the failing handle");
            let fresh = (self.opener)(&self.paths.snapshot)
                .and_then(|mut fresh| release(&mut fresh, others));
            if let Err(error) = fresh {
                tracing::error!(%error, "could not release fans through a fresh handle");
                self.unreleased = true;
            }
        }
        self.last_board = Readings::default();
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
        if let Err(error) = self.release_gpus(GPU_SUSPEND_BUDGET) {
            tracing::error!(%error, "failed to return GPU fans to the driver before suspend");
        }
        let others = self.others_in_control();
        if let Some(hardware) = self.hardware.as_mut()
            && let Err(error) = release(hardware, others)
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
        let others = self.others_in_control();
        if ready
            && let Some(hardware) = self.hardware.as_mut()
            && let Err(error) = release(hardware, others)
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
        let others = self.others_in_control();
        let board = match self.hardware.take() {
            None => !self.unreleased,
            Some(mut hardware) => match release(&mut hardware, others) {
                Ok(()) => true,
                Err(error) => {
                    tracing::error!(%error, "failed to release every fan during shutdown");
                    false
                }
            },
        };
        let gpus = self.release_gpus(GPU_RELEASE_BUDGET);
        if let Err(error) = &gpus {
            tracing::error!(%error, "failed to return every GPU fan to the driver during shutdown");
        }
        let released = board && gpus.is_ok();
        if released {
            tracing::info!("all fans returned to BIOS control");
            self.signal(Signal::Owning(false));
        }
        released
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
            let safety = &self.engine.config().safety;
            let consequence = if safety.sensor_fallback_enabled {
                "fans go to 100% if it stays silent"
            } else {
                "the missing-sensor fallback is off"
            };
            warnings.push(format!(
                "critical sensor `{}` is not reporting; {consequence}",
                safety.critical_sensor
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
            readings: self.with_extras(self.last_readings.clone()),
            fans: self.last_outcome.fans.clone(),
            limits: self.engine.limits().clone(),
            config: self.engine.config().clone(),
            critical: self.last_outcome.critical,
            critical_reached: self.last_outcome.critical_reached,
            monitor_only: self.engine.is_monitor_only(),
            user_monitor_only: self.shared.user_monitor_only(),
            gpus: GpuState {
                fans: self.gpu_readings.info.clone(),
                monitor_only: self.engine.is_monitor_only_for(FanKind::Gpu),
                conflicts: findings.gpu_controlling.clone(),
                error: self.gpu_readings.error.clone(),
            },
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
        if self.hardware.is_some() || self.owns_any() {
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

fn release<B: Backend>(hardware: &mut B, others_in_control: bool) -> Result<(), HwError> {
    if others_in_control {
        hardware.release_owned()
    } else {
        hardware.release_all()
    }
}

fn load_config(paths: &Paths) -> Loaded {
    let defaults = |warning: String| Loaded {
        config: Config::default(),
        config_warning: Some(warning),
        calibration_warning: None,
    };
    match store::load_toml::<Config>(&paths.config) {
        Ok(Some(mut config)) => {
            if config.migrate() {
                tracing::info!("moved old or per-core temperature references to `cpu/package`");
            }
            match config.validate() {
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
            }
        }
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
    fn the_missing_safety_sensor_warning_follows_the_fallback_toggle() {
        let extras = extras_with_gpu(Some(45.0));
        let mut rig = rig("extras-warning", &[], Vec::new());
        rig.runtime = rig.runtime.with_extra_sensors(extras.clone());
        rig.runtime.open_hardware();
        rig.runtime.tick();
        let mut config = rig.runtime.engine.config().clone();
        config.safety.critical_sensor = "gpu/test".into();
        rig.runtime.replace_config(config.clone()).unwrap();
        extras.with_cache(|cache| cache.record(0, &Err("lost".into()), Instant::now()));
        let warning = |rig: &mut Rig| {
            rig.runtime.tick();
            rig.runtime.publish();
            rig.runtime
                .shared
                .state()
                .unwrap()
                .warnings
                .iter()
                .find(|warning| warning.contains("gpu/test"))
                .cloned()
                .unwrap()
        };
        assert!(warning(&mut rig).ends_with("fans go to 100% if it stays silent"));
        config.safety.sensor_fallback_enabled = false;
        rig.runtime.replace_config(config).unwrap();
        assert!(warning(&mut rig).ends_with("the missing-sensor fallback is off"));
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

    use fanforge_hw::gpu_fans::fake::{FakeGpus, GPU_ID, SharedGpus};

    fn gpu_fan() -> FanId {
        FanId::for_gpu_fan(GPU_ID, 0)
    }

    fn wait_until(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "condition not reached in time");
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn with_gpus(rig: &mut Rig) -> SharedGpus {
        let fake = SharedGpus::new(FakeGpus::default());
        let gpus = GpuFans::start(
            fake.opener(),
            rig._dir.path().join("gpu_owned.toml"),
            rig.runtime.shared.halt_handle(),
        )
        .unwrap();
        wait_until(|| {
            !gpus
                .readings(Instant::now(), Duration::from_secs(1))
                .fans
                .is_empty()
        });
        rig.runtime.gpus = Some(gpus);
        fake
    }

    fn gpu_rig(tag: &str, processes: Vec<String>) -> (Rig, SharedGpus) {
        let gpu = gpu_fan();
        let modes = [("pwm2", manual(60.0)), (gpu.as_str(), manual(55.0))];
        let mut rig = rig(tag, &modes, processes);
        let fake = with_gpus(&mut rig);
        (rig, fake)
    }

    fn switch(rig: &mut Rig, on: bool) -> Result<(), String> {
        let (reply, answer) = mpsc::sync_channel(1);
        rig.runtime.handle(LoopMessage::SetMonitorOnly(on, reply));
        answer.recv().unwrap()
    }

    #[test]
    fn the_monitor_only_switch_releases_every_fan_at_once_and_resumes() {
        let (mut rig, fake) = gpu_rig("switch", Vec::new());
        rig.runtime.open_hardware();
        rig.runtime.tick();
        wait_until(|| fake.fan(0).manual);
        assert_eq!(reg(&rig, CHA1_DUTY), Duty::new(60.0).to_raw());

        switch(&mut rig, true).unwrap();
        assert_eq!(reg(&rig, CHA1_CONTROL) & AUTO, AUTO);
        wait_until(|| !fake.fan(0).manual);
        let state = rig.runtime.shared.state().unwrap();
        assert!(state.monitor_only && state.user_monitor_only && state.gpus.monitor_only);
        rig.runtime.tick();
        assert_eq!(reg(&rig, CHA1_CONTROL) & AUTO, AUTO);

        switch(&mut rig, false).unwrap();
        assert_eq!(reg(&rig, CHA1_DUTY), Duty::new(60.0).to_raw());
        thread::sleep(Duration::from_millis(100));
        rig.runtime.tick();
        wait_until(|| fake.fan(0).manual);
        let state = rig.runtime.shared.state().unwrap();
        assert!(!state.monitor_only && !state.user_monitor_only);
    }

    #[test]
    fn a_conflict_keeps_monitor_only_after_the_switch_is_turned_off() {
        let mut rig = rig(
            "switch-conflict",
            &[("pwm2", manual(60.0))],
            vec!["FanControl.exe".to_owned()],
        );
        rig.runtime.open_hardware();
        switch(&mut rig, true).unwrap();
        switch(&mut rig, false).unwrap();
        assert!(rig.runtime.engine.is_monitor_only());
        assert_eq!(reg(&rig, CHA1_CONTROL) & AUTO, AUTO);
    }

    #[test]
    fn the_switch_outlives_the_runtime() {
        let mut rig = rig("switch-restart", &[("pwm2", manual(60.0))], Vec::new());
        rig.runtime.open_hardware();
        switch(&mut rig, true).unwrap();
        assert!(rig.runtime.shutdown());
        let (_messages, receiver) = mpsc::channel();
        let mut again = Runtime::new(
            Paths {
                config: rig._dir.path().join("config.toml"),
                snapshot: rig._dir.path().join("snapshot.toml"),
            },
            Arc::clone(&rig.runtime.shared),
            receiver,
            opener(rig.fake.clone(), "switch-restart-again"),
            ConflictMonitor::with_processes(Box::new(|| Ok(Vec::new()))),
        );
        again.open_hardware();
        again.tick();
        assert_eq!(reg(&rig, CHA1_CONTROL) & AUTO, AUTO);
        assert!(again.engine.is_monitor_only());
    }

    #[test]
    fn retrying_a_fan_is_refused_while_monitor_only() {
        let mut rig = rig("retry", &[("pwm2", manual(60.0))], Vec::new());
        rig.runtime.open_hardware();
        rig.runtime.tick();
        let (reply, answer) = mpsc::sync_channel(1);
        rig.runtime
            .handle(LoopMessage::RetryFan("pwm2".into(), reply));
        assert_eq!(answer.recv().unwrap(), Ok(()));
        switch(&mut rig, true).unwrap();
        let (reply, answer) = mpsc::sync_channel(1);
        rig.runtime
            .handle(LoopMessage::RetryFan("pwm2".into(), reply));
        assert!(answer.recv().unwrap().is_err());
    }

    #[test]
    fn gpu_fans_are_driven_and_released_alongside_the_board() {
        let (mut rig, fake) = gpu_rig("gpu-both", Vec::new());
        rig.runtime.open_hardware();
        rig.runtime.tick();
        wait_until(|| fake.fan(0).manual);
        assert_eq!(fake.fan(0).target, 55);
        assert_eq!(reg(&rig, CHA1_DUTY), Duty::new(60.0).to_raw());
        assert!(!fake.fan(1).manual);
        assert!(rig.runtime.owns_any());
        rig.runtime.publish();
        let state = rig.runtime.shared.state().unwrap();
        assert_eq!(state.gpus.fans.len(), 2);
        assert!(state.readings.fan(&gpu_fan()).is_some());

        assert!(rig.runtime.shutdown());
        assert!(!fake.fan(0).manual);
        assert_eq!(reg(&rig, CHA1_CONTROL) & AUTO, AUTO);
    }

    #[test]
    fn gpu_fans_work_without_a_board_chip_and_the_board_is_still_retried() {
        let (mut rig, fake) = gpu_rig("gpu-only", Vec::new());
        let installed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&installed);
        let mut open = opener(rig.fake.clone(), "gpu-only-open");
        rig.runtime.opener = Box::new(move |snapshot: &Path| {
            if flag.load(std::sync::atomic::Ordering::SeqCst) {
                open(snapshot)
            } else {
                Err(HwError::PawnIoMissing)
            }
        });
        rig.runtime.open_hardware();
        rig.runtime.tick();
        wait_until(|| fake.fan(0).manual);
        let status = rig.runtime.shared.state().unwrap().hardware.clone();
        assert_eq!(status, HardwareStatus::DriverMissing);
        assert_eq!(reg(&rig, CHA1_CONTROL) & AUTO, AUTO);

        installed.store(true, std::sync::atomic::Ordering::SeqCst);
        rig.runtime.handle(LoopMessage::RetryHardware);
        rig.runtime.tick();
        assert_eq!(reg(&rig, CHA1_DUTY), Duty::new(60.0).to_raw());
        assert!(rig.runtime.shutdown());
        assert!(!fake.fan(0).manual);
    }

    #[test]
    fn a_busy_board_still_takes_gpu_fans_to_full_speed_when_critical() {
        let (mut rig, fake) = gpu_rig("gpu-busy", Vec::new());
        rig.runtime.open_hardware();
        rig.runtime.tick();
        wait_until(|| fake.fan(0).target == 55);
        rig.fake.with(|chip| chip.banks[0][0x29] = 95);
        rig.runtime.tick();
        wait_until(|| fake.fan(0).target == 100);
        rig.fake.with(|chip| chip.banks[0][0x29] = 40);
        rig.runtime.tick();

        let name = format!(r"Local\fanforge-runtime-gpu-busy-{}", std::process::id());
        let (release, held) = mpsc::channel::<()>();
        let (ready_tx, ready) = mpsc::channel();
        let holder = thread::spawn(move || {
            let mutex = NamedMutex::open(&name).unwrap();
            let _guard = mutex.acquire(fanforge_hw::mutex::DEFAULT_TIMEOUT).unwrap();
            ready_tx.send(()).unwrap();
            held.recv().ok();
        });
        ready.recv().unwrap();
        let mut config = rig.runtime.engine.config().clone();
        config.safety.critical_c = 60.0;
        config.safety.release_c = 50.0;
        config.safety.critical_sensor = "board/temp1".into();
        rig.runtime.engine.set_config(config);
        rig.runtime.last_board.sensors[1].celsius = Some(70.0);
        rig.runtime.tick();
        wait_until(|| fake.fan(0).target == 100);
        assert!(rig.runtime.lock_skips > 0);
        let state = rig.runtime.shared.state().unwrap();
        assert!(state.readings.fan(&"pwm2".into()).is_some());
        assert!(state.fans.iter().any(|fan| fan.id.as_str() == "pwm2"));
        release.send(()).unwrap();
        holder.join().unwrap();
    }

    #[test]
    fn dropping_a_failed_board_keeps_gpu_fans_under_the_safety_fallback() {
        let (mut rig, fake) = gpu_rig("gpu-reinit", Vec::new());
        rig.runtime.open_hardware();
        rig.runtime.tick();
        wait_until(|| fake.fan(0).manual);
        rig.fake.with(|chip| {
            chip.chip_id = 0xFFFF;
            chip.unreadable_registers.push(0x0D);
        });
        for _ in 0..READ_FAILURES_BEFORE_REINIT + 2 {
            rig.runtime.tick();
        }
        assert!(rig.runtime.hardware.is_none());
        assert_eq!(reg(&rig, CHA1_CONTROL) & AUTO, AUTO);
        wait_until(|| fake.fan(0).target == 100);
        assert!(fake.fan(0).manual);
    }

    #[test]
    fn a_gpu_tool_only_makes_gpu_fans_monitor_only() {
        let (mut rig, fake) = gpu_rig("gpu-tool", vec!["MSIAfterburner.exe".to_owned()]);
        rig.runtime.open_hardware();
        rig.runtime.tick();
        rig.runtime.tick();
        assert_eq!(reg(&rig, CHA1_DUTY), Duty::new(60.0).to_raw());
        thread::sleep(Duration::from_millis(50));
        assert!(fake.with(|gpus| gpus.writes.is_empty()));
        let state = rig.runtime.shared.state().unwrap();
        assert!(!state.monitor_only);
        assert!(state.gpus.monitor_only);
        assert_eq!(state.gpus.conflicts, vec!["MSI Afterburner"]);
    }

    #[test]
    fn a_fan_tool_makes_gpu_fans_monitor_only_too() {
        let (mut rig, fake) = gpu_rig("gpu-fan-tool", vec!["FanControl.exe".to_owned()]);
        rig.runtime.open_hardware();
        rig.runtime.tick();
        thread::sleep(Duration::from_millis(50));
        assert!(fake.with(|gpus| gpus.writes.is_empty()));
        assert!(rig.runtime.engine.is_monitor_only_for(FanKind::Gpu));
    }

    #[test]
    fn a_hung_gpu_driver_never_holds_up_the_board() {
        let (mut rig, fake) = gpu_rig("gpu-hung", Vec::new());
        rig.runtime.open_hardware();
        rig.runtime.tick();
        wait_until(|| fake.fan(0).manual);
        fake.hold();
        let mut config = rig.runtime.engine.config().clone();
        config
            .profiles
            .get_mut("Balanced")
            .unwrap()
            .fans
            .insert("pwm2".into(), manual(70.0));
        rig.runtime.replace_config(config).unwrap();
        let started = Instant::now();
        rig.runtime.tick();
        assert_eq!(reg(&rig, CHA1_DUTY), Duty::new(70.0).to_raw());
        assert!(started.elapsed() < Duration::from_secs(1));
        let started = Instant::now();
        assert!(!rig.runtime.shutdown());
        assert!(started.elapsed() < GPU_RELEASE_BUDGET + Duration::from_secs(1));
        assert_eq!(reg(&rig, CHA1_CONTROL) & AUTO, AUTO);
        fake.unhold();
    }

    #[test]
    fn suspend_returns_gpu_fans_to_the_driver_and_resume_retakes_them() {
        let (mut rig, fake) = gpu_rig("gpu-suspend", Vec::new());
        rig.runtime.open_hardware();
        rig.runtime.tick();
        wait_until(|| fake.fan(0).manual);
        rig.runtime.suspend();
        assert!(!fake.fan(0).manual);
        assert!(matches!(rig.runtime.resume(), Flow::Continue));
        rig.runtime.tick();
        wait_until(|| fake.fan(0).manual);
    }

    #[test]
    fn suspend_falls_back_to_the_spare_session_when_the_worker_is_stuck() {
        let (mut rig, fake) = gpu_rig("gpu-suspend-hung", Vec::new());
        rig.runtime.gpu_releaser = Some(GpuReleaser::open(
            fake.opener(),
            rig._dir.path().join("gpu_owned.toml"),
        ));
        rig.runtime.open_hardware();
        rig.runtime.tick();
        wait_until(|| fake.fan(0).manual);
        rig.runtime
            .gpus
            .as_ref()
            .unwrap()
            .request(&gpu_fan(), Action::SetDuty(Duty::new(80.0)));
        let worker = rig.runtime.gpus.take().unwrap();
        drop(worker);
        rig.runtime.gpus = Some(
            GpuFans::start(
                Arc::new(|| Err("the driver is restarting".to_owned())),
                rig._dir.path().join("gpu_owned.toml"),
                rig.runtime.shared.halt_handle(),
            )
            .unwrap(),
        );
        let started = Instant::now();
        rig.runtime.suspend();
        assert!(started.elapsed() < GPU_SUSPEND_BUDGET + Duration::from_millis(500));
        assert!(!fake.fan(0).manual);
        assert_eq!(reg(&rig, CHA1_CONTROL) & AUTO, AUTO);
    }

    #[test]
    fn the_guardian_hears_ownership_of_gpu_fans_alone() {
        let gpu = gpu_fan();
        let mut rig = rig("gpu-owning", &[(gpu.as_str(), manual(55.0))], Vec::new());
        let fake = with_gpus(&mut rig);
        let heard = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&heard);
        rig.runtime.signals = Some(Arc::new(move |signal| {
            sink.lock().unwrap().push(signal);
        }));
        rig.runtime.opener = Box::new(|_: &Path| Err(HwError::PawnIoMissing));
        rig.runtime.open_hardware();
        rig.runtime.tick();
        wait_until(|| fake.fan(0).manual);
        rig.runtime.tick();
        assert!(heard.lock().unwrap().contains(&Signal::Owning(true)));
        assert!(rig.runtime.shutdown());
        assert!(!fake.fan(0).manual);
        assert_eq!(heard.lock().unwrap().last(), Some(&Signal::Owning(false)));
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
        let conflicts =
            ConflictMonitor::with_processes(Box::new(|| Ok(vec!["FanControl.exe".to_owned()])));
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
        assert!(state.board.as_ref().unwrap().fan_control);
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
    fn an_ite_chip_beside_another_fan_tool_is_never_written_through_the_whole_lifecycle() {
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
    fn a_nuvoton_chip_beside_another_fan_tool_only_sees_bank_selects() {
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
