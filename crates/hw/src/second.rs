use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use mostlyfans_core::board::{
    ChipInfo, chip_label, chip_temp_sensor_id, default_fan_name, temp_label,
};
use mostlyfans_core::engine::Action;
use mostlyfans_core::telemetry::{FanReading, HardwareMode, SensorReading};
use mostlyfans_core::{Duty, FanId};

use crate::HwError;
use crate::gigabyte::{
    self, Bridge, Ecio, Generation, H2Ram, OPERATION_DEADLINE, SETTLE, SMARTFAN_ENABLE, Window,
};
use crate::ite::rpm_from_count;
use crate::port::PortIo;
use crate::snapshot::{BiosSnapshot, SmartfanRoute};
use crate::superio::{ChipReadings, SuperIo};

pub const SECOND_CHIP: usize = 1;
pub const LOST_CONTACT: &str =
    "MostlyFans lost contact with the second fan chip and handed its fans back";
pub const RELEASE_BUDGET: Duration = Duration::from_secs(2);
const FAILURES_BEFORE_BACKOFF: u32 = 3;
const BACKOFF: Duration = Duration::from_secs(10);
const SNAPSHOT_FILE: &str = "bios_snapshot_sio2.toml";
pub const SHORT_BUDGET: Duration = Duration::from_millis(400);
const LOST_CONTACT_BUDGET: Duration = Duration::from_millis(500);
const FAILING_RELEASE_BUDGET: Duration = Duration::from_millis(200);
const RETRY_PAUSE: Duration = Duration::from_millis(20);
pub const PRIMARY_SHARE: f32 = 0.6;
const IT8792E_ID: u16 = 0x8733;
const NOT_GIGABYTE: &str = "MostlyFans only controls the second fan chip on Gigabyte boards";
const NO_ROUTE: &str = "MostlyFans can't reach Gigabyte's fan controller on this board";
const G3_WITHOUT_ECIO: &str =
    "the fan speeds on this board can't be read reliably, so MostlyFans only monitors these fans";

pub fn snapshot_path(primary: &Path) -> PathBuf {
    primary.with_file_name(SNAPSHOT_FILE)
}

pub(crate) fn wired_headers<P: PortIo>(chip: &SuperIo<P>) -> impl Iterator<Item = usize> + use<P> {
    let wiring = chip.wiring();
    (0..chip.fan_count()).filter(move |&channel| wiring.is_wired(channel))
}

pub type BridgeOpener = fn() -> Result<Box<dyn Bridge>, HwError>;

pub fn open_isa_bridge() -> Result<Box<dyn Bridge>, HwError> {
    Ok(Box::new(gigabyte::IsaBridge::open()?))
}

#[derive(Debug)]
pub enum Route {
    Smfi {
        bridge: Box<dyn Bridge>,
        open: BridgeOpener,
    },
    Ecio,
}

impl Route {
    fn kind(&self) -> SmartfanRoute {
        match self {
            Self::Smfi { .. } => SmartfanRoute::Smfi,
            Self::Ecio => SmartfanRoute::Ecio,
        }
    }
}

#[derive(Debug)]
pub struct Control {
    route: Route,
    generation: Generation,
    handed_over: bool,
}

impl Control {
    pub fn new(route: Route, generation: Generation) -> Self {
        Self {
            route,
            generation,
            handed_over: false,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Board {
    pub gigabyte: bool,
    pub amd: bool,
}

pub fn discover<P: PortIo>(
    chip: &mut SuperIo<P>,
    board: Board,
    open_bridge: BridgeOpener,
) -> Result<Control, String> {
    let name = chip.chip().name;
    if !chip.fan_control() {
        return Err(format!("MostlyFans doesn't control fans on the {name}"));
    }
    if !board.gigabyte {
        return Err(NOT_GIGABYTE.to_owned());
    }
    let smfi = chip.smfi_active().unwrap_or_else(|error| {
        tracing::warn!(%error, "could not read the second chip's SMFI device");
        false
    });
    let route = if smfi {
        open_bridge()
            .and_then(|mut bridge| {
                let present = chip.with_bus(|_| gigabyte::smfi_present(bridge.as_mut()))?;
                Ok(present.then_some(Route::Smfi {
                    bridge,
                    open: open_bridge,
                }))
            })
            .unwrap_or_else(|error| {
                tracing::warn!(%error, "the ISA bridge window to Gigabyte's fan controller failed");
                None
            })
    } else {
        None
    };
    let route = match route {
        Some(route) => route,
        None if board.amd && chip.config().chip_id == IT8792E_ID => {
            let present = chip
                .with_bus(|io| Ecio::new(io).present())
                .unwrap_or_else(|error| {
                    tracing::warn!(%error, "the ECIO mailbox to Gigabyte's fan controller failed");
                    false
                });
            if !present {
                return Err(NO_ROUTE.to_owned());
            }
            Route::Ecio
        }
        None => return Err(NO_ROUTE.to_owned()),
    };
    let mut control = Control::new(route, Generation::Unknown);
    let chip_id = chip.config().chip_id;
    control.generation = h2ram(chip, &mut control, |ram| ram.generation(chip_id))
        .map_err(|error| format!("{NO_ROUTE}: {error}"))?;
    if control.generation == Generation::G3 && !matches!(control.route, Route::Ecio) {
        return Err(G3_WITHOUT_ECIO.to_owned());
    }
    tracing::info!(
        route = ?control.route.kind(),
        generation = ?control.generation,
        "MostlyFans can take over the second fan chip from Gigabyte's fan controller"
    );
    Ok(control)
}

fn h2ram<P: PortIo, T>(
    chip: &mut SuperIo<P>,
    control: &mut Control,
    action: impl FnOnce(&mut dyn H2Ram) -> Result<T, HwError>,
) -> Result<T, HwError> {
    match &mut control.route {
        Route::Ecio => chip.with_bus(|io| action(&mut Ecio::new(io))),
        Route::Smfi { bridge, .. } => chip.with_bus(|_| {
            gigabyte::with_window(bridge.as_mut(), |bridge| action(&mut Window(bridge)))
        }),
    }
}

#[derive(Debug)]
pub struct SecondChip<P: PortIo> {
    pub chip: SuperIo<P>,
    failures: u32,
    pub(crate) retry_at: Option<Instant>,
    pub(crate) backoff: Duration,
    control: Option<Control>,
    blocked: Option<String>,
    open_bridge: BridgeOpener,
    snapshot: BiosSnapshot,
    snapshot_path: PathBuf,
}

impl<P: PortIo> SecondChip<P> {
    pub fn new(
        chip: SuperIo<P>,
        control: Result<Control, String>,
        snapshot_path: PathBuf,
        open_bridge: BridgeOpener,
    ) -> Self {
        let chip_id = chip.config().chip_id;
        let snapshot = BiosSnapshot::load_for(&snapshot_path, chip_id).unwrap_or_else(|error| {
            tracing::error!(%error, "the second chip's BIOS snapshot is unreadable");
            BiosSnapshot {
                lost: true,
                ..BiosSnapshot::for_chip(chip_id)
            }
        });
        let (control, blocked) = match control {
            Ok(control) => (Some(control), None),
            Err(reason) => {
                tracing::info!(reason, "the second fan chip is monitor only");
                (None, Some(reason))
            }
        };
        Self {
            chip,
            failures: 0,
            retry_at: None,
            backoff: BACKOFF,
            control,
            blocked,
            open_bridge,
            snapshot,
            snapshot_path,
        }
    }

    pub fn controllable(&self) -> bool {
        self.control.is_some() && self.blocked.is_none()
    }

    pub fn snapshot(&self) -> &BiosSnapshot {
        &self.snapshot
    }

    pub fn owns_any(&self) -> bool {
        !self.snapshot.owned.is_empty() || self.snapshot.smartfan.is_some()
    }

    pub fn info(&self) -> ChipInfo {
        ChipInfo {
            chip_id: self.chip.config().chip_id,
            chip_name: self.chip.chip().name.to_owned(),
            fan_control: self.controllable(),
            pwm_misconfigured: self.chip.pwm_misconfigured(),
            fan_headers: wired_headers(&self.chip).count(),
            note: self.blocked.clone(),
        }
    }

    fn persist(&self) -> Result<(), HwError> {
        self.snapshot
            .save(&self.snapshot_path)
            .map_err(HwError::from)
    }

    fn persist_logged(&self) {
        if let Err(error) = self.persist() {
            tracing::warn!(%error, "failed to persist the second chip's BIOS snapshot");
        }
    }

    fn h2ram<T>(
        &mut self,
        action: impl FnOnce(&mut dyn H2Ram) -> Result<T, HwError>,
    ) -> Result<T, HwError> {
        let name = self.chip.chip().name;
        let control = self.control.as_mut().ok_or(HwError::NoFanControl(name))?;
        h2ram(&mut self.chip, control, action)
    }

    fn ensure_route(&mut self) {
        if self.control.is_some() || !self.owns_any() {
            return;
        }
        match route_for_release(&mut self.chip, &self.snapshot, self.open_bridge) {
            Ok(control) => self.control = Some(control),
            Err(reason) => tracing::error!(
                reason,
                "Gigabyte's fan controller can't be reached to switch it back on"
            ),
        }
    }

    fn backing_off(&self) -> bool {
        self.retry_at.is_some_and(|at| Instant::now() < at)
    }

    fn failing(&self) -> bool {
        self.failures > 0
    }

    fn failed(&mut self, error: &HwError) {
        self.failures += 1;
        if self.failures == 1 {
            tracing::warn!(%error, chip = self.chip.chip().name, "the second fan chip could not be read");
        }
        if self.failures >= FAILURES_BEFORE_BACKOFF {
            self.retry_at = Some(Instant::now() + self.backoff);
        }
    }

    pub fn reinitialize(&mut self) {
        if let Some(control) = &mut self.control {
            control.handed_over = false;
        }
        if self.backing_off() {
            return;
        }
        if let Err(error) = self.chip.reinitialize() {
            self.failed(&error);
            return;
        }
        let Some(control) = &mut self.control else {
            return;
        };
        if let Route::Smfi { bridge, open } = &mut control.route {
            let open = *open;
            *bridge = Box::new(Closed);
            match open().and_then(|mut fresh| {
                let present = self
                    .chip
                    .with_bus(|_| gigabyte::smfi_present(fresh.as_mut()))?;
                Ok(present.then_some(fresh))
            }) {
                Ok(Some(fresh)) => *bridge = fresh,
                Ok(None) | Err(_) => {
                    tracing::warn!("the ISA bridge window is gone after resume");
                    self.blocked = Some(NO_ROUTE.to_owned());
                    return;
                }
            }
        }
        let chip_id = self.chip.config().chip_id;
        match self.h2ram(|ram| ram.generation(chip_id)) {
            Ok(generation) => {
                if let Some(control) = &mut self.control
                    && control.generation != generation
                {
                    tracing::info!(
                        ?generation,
                        "Gigabyte's fan controller reports a new generation after resume"
                    );
                    control.generation = generation;
                }
            }
            Err(error) => {
                tracing::warn!(%error, "could not read Gigabyte's fan controller after resume")
            }
        }
    }

    fn read_chip(&mut self) -> Result<ChipReadings, HwError> {
        let mut readings = self.chip.read()?;
        let g3 = self
            .control
            .as_ref()
            .is_some_and(|control| control.generation == Generation::G3);
        if g3 {
            let counts = self.h2ram(|ram| ram.g3_tachs())?;
            for (rpm, count) in readings.rpm.iter_mut().zip(counts) {
                *rpm = rpm_from_count(count);
            }
        }
        Ok(readings)
    }

    fn poll(&mut self) -> Option<ChipReadings> {
        if self.backing_off() {
            return None;
        }
        match self.read_chip() {
            Ok(readings) => {
                if self.failing() {
                    tracing::info!(
                        chip = self.chip.chip().name,
                        "the second fan chip answers again"
                    );
                }
                self.failures = 0;
                self.retry_at = None;
                Some(readings)
            }
            Err(error) => {
                self.failed(&error);
                if self.failures == 1 && self.owns_any() {
                    tracing::error!(
                        "lost contact with the second fan chip while controlling it; handing its fans back"
                    );
                    if let Err(error) = self.release(false, LOST_CONTACT_BUDGET) {
                        tracing::error!(%error, "could not hand the second chip's fans back");
                    }
                }
                None
            }
        }
    }

    pub fn read_into(&mut self, sensors: &mut Vec<SensorReading>, fans: &mut Vec<FanReading>) {
        let readings = self.poll();
        let label = chip_label(self.chip.chip().name);
        for index in 0..self.chip.temp_names().len() {
            sensors.push(SensorReading {
                id: chip_temp_sensor_id(SECOND_CHIP, index),
                label: format!("{} ({label})", temp_label(index)),
                celsius: readings
                    .as_ref()
                    .and_then(|readings| readings.temps.get(index).copied().flatten()),
            });
        }
        let wiring = self.chip.wiring();
        let controllable = self.controllable();
        let lost = readings.is_none() && controllable;
        for index in wired_headers(&self.chip) {
            let state = readings
                .as_ref()
                .and_then(|readings| readings.channels.get(index));
            let manual = state.is_some_and(|state| !state.is_auto());
            fans.push(FanReading {
                id: FanId::for_chip_channel(SECOND_CHIP, index),
                label: format!("{} ({label})", default_fan_name(index)),
                rpm: readings
                    .as_ref()
                    .and_then(|readings| readings.rpm.get(index).copied().flatten())
                    .filter(|_| wiring.has_tach(index)),
                mode: if manual {
                    HardwareMode::Manual
                } else {
                    HardwareMode::Auto
                },
                duty: state
                    .filter(|_| manual && controllable)
                    .map(|state| Duty::from_raw(state.duty)),
                unavailable: lost.then(|| LOST_CONTACT.to_owned()),
                monitor_only: !controllable,
            });
        }
    }

    fn handover(&mut self, halted: &AtomicBool) -> Result<(), HwError> {
        if self
            .control
            .as_ref()
            .is_some_and(|control| control.handed_over)
        {
            return Ok(());
        }
        let current =
            self.h2ram(|ram| ram.read(SMARTFAN_ENABLE, Instant::now() + OPERATION_DEADLINE))?;
        if self.snapshot.smartfan.is_none() {
            if current & gigabyte::SMARTFAN_ON == 0 {
                tracing::warn!(
                    current,
                    "Gigabyte's fan controller was already off before MostlyFans took over; it will be switched on when MostlyFans lets go"
                );
            }
            self.snapshot.smartfan = Some(current);
            self.snapshot.smartfan_route =
                self.control.as_ref().map(|control| control.route.kind());
            if let Err(error) = self.persist() {
                self.snapshot.smartfan = None;
                self.snapshot.smartfan_route = None;
                return Err(error);
            }
        }
        if current != 0 {
            let write = |ram: &mut dyn H2Ram| {
                if halted.load(Ordering::SeqCst) {
                    return Err(HwError::Halted);
                }
                ram.write(SMARTFAN_ENABLE, 0, Instant::now() + OPERATION_DEADLINE)
            };
            let written = match self.h2ram(write) {
                Err(HwError::FanConfigTimeout(_)) => self.h2ram(write),
                result => result,
            };
            match written {
                Ok(()) => {}
                Err(HwError::Halted) => return Err(HwError::Halted),
                Err(error) => {
                    tracing::error!(%error, "switching Gigabyte's fan controller off failed; switching it back on");
                    self.blocked = Some(format!(
                        "Gigabyte's fan controller couldn't be switched off: {error}"
                    ));
                    if let Err(restore) = self.restore_smartfan(Instant::now() + SHORT_BUDGET) {
                        tracing::error!(%restore, "and switching it back on failed too");
                    }
                    self.persist_logged();
                    return Err(error);
                }
            }
            thread::sleep(SETTLE);
            if halted.load(Ordering::SeqCst) {
                if let Err(error) = self.restore_smartfan(Instant::now() + SHORT_BUDGET) {
                    tracing::error!(%error, "could not switch Gigabyte's fan controller back on after a halt");
                }
                self.persist_logged();
                return Err(HwError::Halted);
            }
        }
        if let Some(control) = &mut self.control {
            control.handed_over = true;
        }
        Ok(())
    }

    fn restore_smartfan(&mut self, deadline: Instant) -> Result<(), HwError> {
        let saved = self.snapshot.smartfan;
        let result = loop {
            let attempt = self.h2ram(|ram| {
                let operation = deadline.min(Instant::now() + OPERATION_DEADLINE * 2);
                let current = match saved {
                    Some(_) => 0,
                    None => ram.read(SMARTFAN_ENABLE, operation)?,
                };
                ram.write(
                    SMARTFAN_ENABLE,
                    gigabyte::restored_smartfan(saved, current),
                    operation,
                )
            });
            match attempt {
                Err(HwError::LockTimeout(_) | HwError::FanConfigTimeout(_))
                    if Instant::now() + RETRY_PAUSE < deadline =>
                {
                    thread::sleep(RETRY_PAUSE);
                }
                result => break result,
            }
        };
        match &result {
            Ok(()) => {
                if let Some(control) = &mut self.control {
                    control.handed_over = false;
                }
                if self.snapshot.owned.is_empty() {
                    self.snapshot.smartfan = None;
                    self.snapshot.smartfan_route = None;
                }
            }
            Err(error) => {
                tracing::error!(%error, "could not switch Gigabyte's fan controller back on");
            }
        }
        result
    }

    fn needs_smartfan_restore(&self) -> bool {
        self.snapshot.smartfan.is_some() || (self.snapshot.lost && self.control.is_some())
    }

    fn release_budget(&self) -> Duration {
        if self.failing() {
            FAILING_RELEASE_BUDGET
        } else {
            RELEASE_BUDGET
        }
    }

    pub fn apply(
        &mut self,
        channel: usize,
        action: Action,
        halted: &AtomicBool,
    ) -> Result<(), HwError> {
        match action {
            Action::SetDuty(duty) => {
                if !self.controllable() {
                    return Err(HwError::NoFanControl(self.chip.chip().name));
                }
                if !self.chip.wiring().has_pwm(channel) {
                    return Err(HwError::NoControlPin(channel));
                }
                if halted.load(Ordering::SeqCst) {
                    return Err(HwError::Halted);
                }
                if !self.snapshot.is_owned(channel) {
                    let current = self.chip.channel_states()?[channel];
                    self.handover(halted)?;
                    self.snapshot.claim_exact(channel, current);
                    if let Err(error) = self.persist() {
                        self.snapshot.release(channel);
                        return Err(error);
                    }
                } else {
                    self.handover(halted)?;
                }
                self.chip
                    .set_duty_unless_halted(channel, duty.to_raw(), halted)
            }
            Action::Release => {
                if !self.snapshot.is_owned(channel) {
                    return Ok(());
                }
                let deadline = Instant::now() + self.release_budget();
                self.restore_channel(channel, deadline)?;
                self.snapshot.release(channel);
                let restored = if self.snapshot.owned.is_empty() && self.needs_smartfan_restore() {
                    self.restore_smartfan(deadline.max(Instant::now() + SHORT_BUDGET))
                } else {
                    Ok(())
                };
                self.persist_logged();
                restored
            }
        }
    }

    fn restore_channel(&mut self, channel: usize, deadline: Instant) -> Result<(), HwError> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(HwError::ReleaseOutOfTime(channel));
        }
        match self.snapshot.channel(channel) {
            Some(saved) => self.chip.restore_exact_patiently(channel, saved, remaining),
            None => self.chip.restore_patiently(channel, None, remaining),
        }
    }

    pub fn initialize(&mut self, take_control: bool) -> Result<(), HwError> {
        self.ensure_route();
        if self.controllable() {
            let states = self.chip.channel_states()?;
            if self.snapshot.refresh(&states) && !take_control {
                self.persist_logged();
            }
        }
        let result = if take_control && (self.owns_any() || self.needs_smartfan_restore()) {
            self.release(true, RELEASE_BUDGET)
        } else {
            Ok(())
        };
        if result.is_ok() && self.snapshot.lost {
            self.snapshot.lost = false;
            self.persist_logged();
        }
        result
    }

    pub fn release(&mut self, owned_only: bool, budget: Duration) -> Result<(), HwError> {
        let deadline = Instant::now() + budget;
        self.ensure_route();
        let restore = self.needs_smartfan_restore();
        let mut smartfan = None;
        if restore && budget < SHORT_BUDGET {
            smartfan = Some(self.restore_smartfan(deadline));
        }
        let widen = !owned_only && self.snapshot.lost && self.control.is_some();
        let mut first_error = None;
        for channel in 0..self.chip.fan_count() {
            if !(widen || self.snapshot.is_owned(channel)) {
                continue;
            }
            match self.restore_channel(channel, deadline) {
                Ok(()) => {
                    self.snapshot.release(channel);
                }
                Err(error) => {
                    tracing::error!(channel, %error, "failed to return a second-chip channel to BIOS control");
                    first_error.get_or_insert(error);
                }
            }
        }
        let smartfan = match smartfan {
            Some(result) => {
                if result.is_ok() && self.snapshot.owned.is_empty() {
                    self.snapshot.smartfan = None;
                    self.snapshot.smartfan_route = None;
                }
                result
            }
            None if restore => self.restore_smartfan(deadline.max(Instant::now() + SHORT_BUDGET)),
            None => Ok(()),
        };
        self.persist_logged();
        match first_error {
            Some(error) => Err(error),
            None => smartfan,
        }
    }
}

#[derive(Debug)]
struct Closed;

impl Bridge for Closed {
    fn second_window(&mut self) -> Result<Option<u64>, HwError> {
        Ok(None)
    }

    fn current_state(&mut self) -> Result<gigabyte::BridgeState, HwError> {
        Err(HwError::NoFanControl("the closed ISA bridge window"))
    }

    fn set_state(&mut self, _: gigabyte::BridgeState) -> Result<(), HwError> {
        Err(HwError::NoFanControl("the closed ISA bridge window"))
    }

    fn map(&mut self) -> Result<(), HwError> {
        Err(HwError::NoFanControl("the closed ISA bridge window"))
    }

    fn unmap(&mut self) -> Result<(), HwError> {
        Ok(())
    }

    fn read_byte(&mut self, _: u16) -> Result<u8, HwError> {
        Err(HwError::NoFanControl("the closed ISA bridge window"))
    }

    fn write_byte(&mut self, _: u16, _: u8) -> Result<(), HwError> {
        Err(HwError::NoFanControl("the closed ISA bridge window"))
    }
}

pub fn pending(primary_snapshot: &Path) -> bool {
    let path = snapshot_path(primary_snapshot);
    path.exists()
        && BiosSnapshot::load(&path).map_or(true, |snapshot| {
            !snapshot.owned.is_empty() || snapshot.smartfan.is_some()
        })
}

pub fn split_budget(primary_snapshot: &Path, budget: Duration) -> (Duration, bool) {
    if pending(primary_snapshot) {
        (budget.mul_f32(PRIMARY_SHARE), true)
    } else {
        (budget, false)
    }
}

pub fn release_from_disk(
    primary: Option<&SuperIo<crate::port::LpcIo>>,
    primary_snapshot: &Path,
    budget: Duration,
) -> Result<(), HwError> {
    let deadline = Instant::now() + budget;
    if !pending(primary_snapshot) {
        return Ok(());
    }
    let path = snapshot_path(primary_snapshot);
    let opened;
    let primary = match primary {
        Some(primary) => primary,
        None => {
            opened = crate::system::open_chip()?;
            &opened
        }
    };
    let Some(chip) = crate::superio::detect_second(primary, crate::port::LpcIo::open, true).chip()
    else {
        tracing::error!("the second fan chip is gone, so its fans can't be handed back");
        return Err(HwError::ChipNotFound(0xFFFF));
    };
    let mut second = SecondChip::new(
        chip,
        Err("released from disk".to_owned()),
        path,
        open_isa_bridge,
    );
    if Instant::now() < deadline {
        second.ensure_route();
    }
    second.release(false, deadline.saturating_duration_since(Instant::now()))
}

pub fn route_for_release<P: PortIo>(
    chip: &mut SuperIo<P>,
    snapshot: &BiosSnapshot,
    open_bridge: BridgeOpener,
) -> Result<Control, String> {
    let route = match snapshot.smartfan_route {
        Some(SmartfanRoute::Ecio) => Route::Ecio,
        Some(SmartfanRoute::Smfi) => {
            let mut bridge = open_bridge().map_err(|error| error.to_string())?;
            let present = chip
                .with_bus(|_| gigabyte::smfi_present(bridge.as_mut()))
                .map_err(|error| error.to_string())?;
            if !present {
                return Err(NO_ROUTE.to_owned());
            }
            Route::Smfi {
                bridge,
                open: open_bridge,
            }
        }
        None => return Err("no SmartFan handover was recorded".to_owned()),
    };
    Ok(Control {
        route,
        generation: Generation::Unknown,
        handed_over: true,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::gigabyte::fake::{FakeBridge, FakeEcio};
    use crate::ite::fake::{FakeIte, SharedFake};
    use crate::mutex::{BusLock, NamedMutex};

    static RUNNING: AtomicBool = AtomicBool::new(false);

    #[derive(Debug, Clone)]
    struct Board2 {
        ite: SharedFake,
        ecio: Arc<Mutex<FakeEcio>>,
    }

    impl PortIo for Board2 {
        fn read(&mut self, port: u16) -> Result<u8, HwError> {
            if matches!(port, 0x3F0 | 0x3F4) {
                return self.ecio.lock().unwrap().read(port);
            }
            self.ite.read(port)
        }

        fn write(&mut self, port: u16, value: u8) -> Result<(), HwError> {
            if matches!(port, 0x3F0 | 0x3F4) {
                return self.ecio.lock().unwrap().write(port, value);
            }
            self.ite.write(port, value)
        }

        fn find_bars(&mut self) -> Result<(), HwError> {
            self.ite.find_bars()
        }
    }

    fn controllable_spec() -> &'static crate::chips::ChipSpec {
        crate::superio::chip_matching(|chip| chip.matches_ite(0x8733)).unwrap()
    }

    fn rig(tag: &str, path: &Path, board: Board) -> (SecondChip<Board2>, Board2) {
        let fake = Board2 {
            ite: SharedFake::new(FakeIte::second(0x8733)),
            ecio: Arc::new(Mutex::new(FakeEcio::new())),
        };
        let lock = BusLock::new(
            NamedMutex::open(&format!(
                r"Local\mostlyfans-second-{tag}-{}",
                std::process::id()
            ))
            .unwrap(),
        );
        let mut chip = SuperIo::probe_second(fake.clone(), lock).unwrap();
        assert!(std::ptr::eq(chip.chip(), controllable_spec()));
        let control = discover(&mut chip, board, || Err(HwError::AccessDenied));
        (
            SecondChip::new(chip, control, path.to_owned(), || {
                Err(HwError::AccessDenied)
            }),
            fake,
        )
    }

    fn gigabyte_amd() -> Board {
        Board {
            gigabyte: true,
            amd: true,
        }
    }

    fn smartfan(fake: &Board2) -> u8 {
        fake.ecio.lock().unwrap().ram[&SMARTFAN_ENABLE]
    }

    #[test]
    fn the_first_chip_keeps_the_whole_budget_unless_the_second_owns_something() {
        let dir = tempfile::tempdir().unwrap();
        let primary = dir.path().join("bios_snapshot.toml");
        let budget = Duration::from_secs(2);
        assert_eq!(split_budget(&primary, budget), (budget, false));
        let mut snapshot = BiosSnapshot::for_chip(0x8733);
        snapshot.save(&snapshot_path(&primary)).unwrap();
        assert_eq!(split_budget(&primary, budget), (budget, false));
        snapshot.smartfan = Some(0x01);
        snapshot.save(&snapshot_path(&primary)).unwrap();
        assert_eq!(
            split_budget(&primary, budget),
            (budget.mul_f32(PRIMARY_SHARE), true)
        );
        let empty = tempfile::tempdir().unwrap();
        assert!(release_from_disk(None, &empty.path().join("bios_snapshot.toml"), budget).is_ok());
    }

    static HALTED: AtomicBool = AtomicBool::new(true);

    #[test]
    fn a_halt_during_the_handover_never_switches_smartfan_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SNAPSHOT_FILE);
        let (mut second, fake) = rig("halted", &path, gigabyte_amd());
        assert!(matches!(second.handover(&HALTED), Err(HwError::Halted)));
        assert_eq!(smartfan(&fake), 0x01);
        assert!(fake.ecio.lock().unwrap().writes.is_empty());
        assert_eq!(BiosSnapshot::load(&path).unwrap().smartfan, Some(0x01));
        second.release(false, RELEASE_BUDGET).unwrap();
        assert!(!pending(&dir.path().join("bios_snapshot.toml")));
    }

    #[test]
    fn a_failed_smartfan_restore_is_reported_within_its_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SNAPSHOT_FILE);
        let (mut second, fake) = rig("restore-error", &path, gigabyte_amd());
        second
            .apply(0, Action::SetDuty(Duty::new(60.0)), &RUNNING)
            .unwrap();
        fake.ecio.lock().unwrap().stuck = true;
        let started = Instant::now();
        assert!(second.release(false, Duration::from_millis(600)).is_err());
        assert!(started.elapsed() < Duration::from_millis(900));
        assert_eq!(BiosSnapshot::load(&path).unwrap().smartfan, Some(0x01));
    }

    #[test]
    fn a_header_smartfan_drove_in_software_mode_gets_its_exact_bytes_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SNAPSHOT_FILE);
        let (mut second, fake) = rig("exact", &path, gigabyte_amd());
        fake.ite.with(|chip| {
            chip.banks[0][0x16] = 0x05;
            chip.banks[0][0x6B] = 0x33;
        });
        second
            .apply(1, Action::SetDuty(Duty::new(80.0)), &RUNNING)
            .unwrap();
        assert_eq!(
            fake.ite.with(|chip| chip.reg(0x6B)),
            Duty::new(80.0).to_raw()
        );
        second.apply(1, Action::Release, &RUNNING).unwrap();
        assert_eq!(
            fake.ite.with(|chip| (chip.reg(0x16), chip.reg(0x6B))),
            (0x05, 0x33)
        );
        assert_eq!(smartfan(&fake), 0x01);
    }

    #[test]
    fn a_crash_left_handover_is_undone_even_when_discovery_fails_this_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SNAPSHOT_FILE);
        {
            let (mut second, _) = rig("orphan-a", &path, gigabyte_amd());
            second
                .apply(1, Action::SetDuty(Duty::new(70.0)), &RUNNING)
                .unwrap();
        }
        let (mut second, fake) = rig("orphan-b", &path, Board::default());
        assert!(!second.controllable());
        second.initialize(true).unwrap();
        assert_eq!(smartfan(&fake), 0x01);
        assert_eq!(fake.ite.with(|chip| chip.reg(0x16)) & 0x80, 0x80);
        assert!(!pending(&dir.path().join("bios_snapshot.toml")));
    }

    #[test]
    fn a_failing_chip_gets_short_releases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SNAPSHOT_FILE);
        let (mut second, fake) = rig("failing", &path, gigabyte_amd());
        second
            .apply(0, Action::SetDuty(Duty::new(60.0)), &RUNNING)
            .unwrap();
        second.failures = 1;
        fake.ite.with(|chip| chip.stuck_registers.push(0x15));
        let started = Instant::now();
        assert!(second.apply(0, Action::Release, &RUNNING).is_err());
        assert!(started.elapsed() < Duration::from_millis(600));
    }

    #[test]
    fn only_a_gigabyte_board_with_a_route_gets_control() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SNAPSHOT_FILE);
        let (second, _) = rig("not-gigabyte", &path, Board::default());
        assert_eq!(second.info().note.as_deref(), Some(NOT_GIGABYTE));
        let (second, _) = rig(
            "intel",
            &path,
            Board {
                gigabyte: true,
                amd: false,
            },
        );
        assert_eq!(second.info().note.as_deref(), Some(NO_ROUTE));
        let (second, _) = rig("ecio", &path, gigabyte_amd());
        assert!(second.controllable());
        assert!(second.info().fan_control);
    }

    #[test]
    fn the_handover_saves_the_byte_before_switching_smartfan_off_and_restores_it_last() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SNAPSHOT_FILE);
        let (mut second, fake) = rig("handover", &path, gigabyte_amd());
        second
            .apply(0, Action::SetDuty(Duty::new(60.0)), &RUNNING)
            .unwrap();
        assert_eq!(smartfan(&fake), 0);
        let saved = BiosSnapshot::load(&path).unwrap();
        assert_eq!(saved.smartfan, Some(0x01));
        assert_eq!(saved.smartfan_route, Some(SmartfanRoute::Ecio));
        assert!(saved.is_owned(0));

        second
            .apply(1, Action::SetDuty(Duty::new(50.0)), &RUNNING)
            .unwrap();
        assert_eq!(fake.ecio.lock().unwrap().writes.len(), 1);
        second.apply(0, Action::Release, &RUNNING).unwrap();
        assert_eq!(smartfan(&fake), 0);
        second.apply(1, Action::Release, &RUNNING).unwrap();
        assert_eq!(smartfan(&fake), 0x01);
        let saved = BiosSnapshot::load(&path).unwrap();
        assert_eq!(saved.smartfan, None);
        assert!(saved.owned.is_empty());
    }

    #[test]
    fn a_restart_after_a_crash_keeps_the_first_saved_byte_and_restores_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SNAPSHOT_FILE);
        {
            let (mut second, fake) = rig("crash-a", &path, gigabyte_amd());
            second
                .apply(2, Action::SetDuty(Duty::new(70.0)), &RUNNING)
                .unwrap();
            assert_eq!(smartfan(&fake), 0);
        }
        let (mut second, fake) = rig("crash-b", &path, gigabyte_amd());
        fake.ecio.lock().unwrap().ram.insert(SMARTFAN_ENABLE, 0);
        second.initialize(true).unwrap();
        assert_eq!(smartfan(&fake), 0x01);
        assert_eq!(fake.ite.with(|chip| chip.reg(0x17)) & 0x80, 0x80);
        assert!(!pending(&dir.path().join("bios_snapshot.toml")));
    }

    #[test]
    fn smartfan_goes_back_on_even_when_a_header_restore_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SNAPSHOT_FILE);
        let (mut second, fake) = rig("restore-fail", &path, gigabyte_amd());
        second
            .apply(0, Action::SetDuty(Duty::new(60.0)), &RUNNING)
            .unwrap();
        fake.ite.with(|chip| chip.stuck_registers.push(0x15));
        assert!(second.release(false, Duration::from_millis(300)).is_err());
        assert_eq!(smartfan(&fake), 0x01);
        let saved = BiosSnapshot::load(&path).unwrap();
        assert!(saved.is_owned(0));
        assert_eq!(saved.smartfan, Some(0x01));
    }

    #[test]
    fn a_lost_snapshot_switches_smartfan_on_from_its_current_byte() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SNAPSHOT_FILE);
        std::fs::write(&path, "not toml [").unwrap();
        let (mut second, fake) = rig("lost", &path, gigabyte_amd());
        fake.ecio.lock().unwrap().ram.insert(SMARTFAN_ENABLE, 0x40);
        second.initialize(true).unwrap();
        assert_eq!(smartfan(&fake), 0x41);
    }

    #[test]
    fn a_stuck_mailbox_during_handover_leaves_the_chip_monitor_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SNAPSHOT_FILE);
        let (mut second, fake) = rig("stuck", &path, gigabyte_amd());
        fake.ecio.lock().unwrap().stuck = true;
        let started = Instant::now();
        assert!(
            second
                .apply(0, Action::SetDuty(Duty::new(60.0)), &RUNNING)
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(!second.snapshot.is_owned(0));
    }

    #[test]
    fn g3_speeds_come_from_h2ram() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SNAPSHOT_FILE);
        let (mut second, fake) = rig("g3", &path, gigabyte_amd());
        fake.ecio.lock().unwrap().ram.insert(0x916, 0xA3);
        fake.ecio.lock().unwrap().ram.insert(0x917, 0x02);
        let mut sensors = Vec::new();
        let mut fans = Vec::new();
        second.read_into(&mut sensors, &mut fans);
        assert_eq!(fans[0].rpm, rpm_from_count(0x02A3));
        assert_ne!(fans[0].rpm, Some(1136));
        assert!(!fans[0].monitor_only);
    }

    #[test]
    fn losing_the_chip_while_controlling_hands_the_fans_back_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SNAPSHOT_FILE);
        let (mut second, fake) = rig("lost-contact", &path, gigabyte_amd());
        second
            .apply(1, Action::SetDuty(Duty::new(60.0)), &RUNNING)
            .unwrap();
        fake.ecio.lock().unwrap().stuck = true;
        let mut sensors = Vec::new();
        let mut fans = Vec::new();
        second.read_into(&mut sensors, &mut fans);
        let fan = &fans[1];
        assert_eq!(fan.unavailable.as_deref(), Some(LOST_CONTACT));
        assert!(!fan.monitor_only);
        fake.ecio.lock().unwrap().stuck = false;
        second.release(false, RELEASE_BUDGET).unwrap();
        assert_eq!(smartfan(&fake), 0x01);
        assert_eq!(fake.ite.with(|chip| chip.reg(0x16)) & 0x80, 0x80);
    }

    #[test]
    fn the_smfi_route_goes_through_the_bridge_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SNAPSHOT_FILE);
        let fake = Board2 {
            ite: SharedFake::new(FakeIte::second(0x8733)),
            ecio: Arc::new(Mutex::new(FakeEcio::new())),
        };
        fake.ite.with(|chip| chip.smfi_active = true);
        let lock = BusLock::new(
            NamedMutex::open(&format!(
                r"Local\mostlyfans-second-smfi-{}",
                std::process::id()
            ))
            .unwrap(),
        );
        let mut chip = SuperIo::probe_second(fake.clone(), lock).unwrap();
        fn bridge() -> Result<Box<dyn Bridge>, HwError> {
            Ok(Box::new(FakeBridge {
                window: Some(0xFE00_0000),
                ram: [(SMARTFAN_ENABLE, 0x01), (0x900, 0x00), (0x901, 0x03)].into(),
                ..FakeBridge::default()
            }))
        }
        let control = discover(&mut chip, gigabyte_amd(), bridge).unwrap();
        assert_eq!(control.generation, Generation::G2);
        assert!(matches!(control.route, Route::Smfi { .. }));
        let mut second = SecondChip::new(chip, Ok(control), path.clone(), bridge);
        second
            .apply(0, Action::SetDuty(Duty::new(60.0)), &RUNNING)
            .unwrap();
        assert_eq!(
            BiosSnapshot::load(&path).unwrap().smartfan_route,
            Some(SmartfanRoute::Smfi)
        );
        assert!(fake.ecio.lock().unwrap().writes.is_empty());
    }

    #[test]
    fn a_g3_board_reachable_only_through_smfi_stays_monitor_only() {
        let fake = Board2 {
            ite: SharedFake::new(FakeIte::second(0x8733)),
            ecio: Arc::new(Mutex::new(FakeEcio::new())),
        };
        fake.ite.with(|chip| chip.smfi_active = true);
        let lock = BusLock::new(
            NamedMutex::open(&format!(
                r"Local\mostlyfans-second-g3smfi-{}",
                std::process::id()
            ))
            .unwrap(),
        );
        let mut chip = SuperIo::probe_second(fake, lock).unwrap();
        fn bridge() -> Result<Box<dyn Bridge>, HwError> {
            Ok(Box::new(FakeBridge {
                window: Some(0xFE00_0000),
                ram: [(SMARTFAN_ENABLE, 0x01), (0x900, 0x01), (0x901, 0x00)].into(),
                ..FakeBridge::default()
            }))
        }
        assert_eq!(
            discover(&mut chip, gigabyte_amd(), bridge).err().as_deref(),
            Some(G3_WITHOUT_ECIO)
        );
    }
}
