use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::HwError;
use crate::chips::{CHIPS, ChipMap, ChipSpec};
use crate::mutex::{DEFAULT_TIMEOUT, NamedMutex};
use crate::port::PortIo;
use crate::{ite, nuvoton};

pub(crate) const LDN_SELECT_REG: u8 = 0x07;
pub(crate) const CHIP_ID_REG: u8 = 0x20;
pub(crate) const BASE_ADDRESS_REG: u8 = 0x60;
pub(crate) const GLOBAL_DUMP_REGS: std::ops::RangeInclusive<u8> = 0x20..=0x2F;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    Primary,
    Secondary,
}

impl Slot {
    pub fn index(self) -> u64 {
        match self {
            Self::Primary => 0,
            Self::Secondary => 1,
        }
    }

    pub fn config_port(self) -> u16 {
        match self {
            Self::Primary => 0x2E,
            Self::Secondary => 0x4E,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlKind {
    #[default]
    Ite,
    Nuvoton,
}

impl ControlKind {
    pub fn is_ite(&self) -> bool {
        *self == Self::Ite
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelState {
    pub control: u8,
    pub duty: u8,
    pub main_enabled: bool,
    #[serde(default, skip_serializing_if = "ControlKind::is_ite")]
    pub kind: ControlKind,
}

impl ChannelState {
    pub fn is_auto(&self) -> bool {
        match self.kind {
            ControlKind::Ite => self.control & ite::AUTO_MODE_BIT != 0,
            ControlKind::Nuvoton => self.control & nuvoton::MODE_MASK != 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChipConfig {
    pub chip: &'static ChipSpec,
    pub slot: Slot,
    pub chip_id: u16,
    pub version: u8,
    pub base_address: u16,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChipReadings {
    pub rpm: Vec<Option<u32>>,
    pub temps: Vec<Option<f32>>,
    pub channels: Vec<ChannelState>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BankDump {
    pub bank: u8,
    pub registers: Vec<Option<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterDump {
    #[serde(default)]
    pub chip: String,
    pub chip_id: u16,
    pub chip_version: u8,
    pub base_address: u16,
    pub global_config: BTreeMap<String, u8>,
    #[serde(default = "default_dump_ldn")]
    pub ldn: u8,
    pub ec_ldn_config: BTreeMap<String, u8>,
    pub bank_register: u8,
    pub banks: Vec<BankDump>,
}

fn default_dump_ldn() -> u8 {
    ite::EC_LDN
}

pub(crate) fn hex(register: u8) -> String {
    format!("{register:#04x}")
}

pub(crate) struct ConfigSpace<'a, P: PortIo> {
    pub(crate) io: &'a mut P,
    port: u16,
}

impl<P: PortIo> ConfigSpace<'_, P> {
    pub(crate) fn read(&mut self, register: u8) -> Result<u8, HwError> {
        self.io.write(self.port, register)?;
        self.io.read(self.port + 1)
    }

    pub(crate) fn write(&mut self, register: u8, value: u8) -> Result<(), HwError> {
        self.io.write(self.port, register)?;
        self.io.write(self.port + 1, value)
    }

    pub(crate) fn write_index(&mut self, value: u8) -> Result<(), HwError> {
        self.io.write(self.port, value)
    }

    pub(crate) fn read_word(&mut self, register: u8) -> Result<u16, HwError> {
        let high = self.read(register)?;
        let low = self.read(register + 1)?;
        Ok(u16::from_be_bytes([high, low]))
    }

    pub(crate) fn select(&mut self, ldn: u8) -> Result<(), HwError> {
        self.write(LDN_SELECT_REG, ldn)
    }

    pub(crate) fn read_base_address(&mut self) -> Result<u16, HwError> {
        let address = self.read_word(BASE_ADDRESS_REG)?;
        thread::sleep(Duration::from_millis(1));
        let verify = self.read_word(BASE_ADDRESS_REG)?;
        if address != verify || address < 0x100 || address & 0xF007 != 0 {
            return Err(HwError::InvalidBaseAddress { address, verify });
        }
        Ok(address)
    }

    pub(crate) fn dump(&mut self, ldn: u8, registers: &[u8]) -> Result<ConfigDump, HwError> {
        let mut global = BTreeMap::new();
        for register in GLOBAL_DUMP_REGS {
            global.insert(hex(register), self.read(register)?);
        }
        self.select(ldn)?;
        let mut device = BTreeMap::new();
        for &register in registers {
            device.insert(hex(register), self.read(register)?);
        }
        Ok((global, device))
    }
}

pub(crate) type ConfigDump = (BTreeMap<String, u8>, BTreeMap<String, u8>);

pub(crate) fn with_config_space<P: PortIo, T>(
    io: &mut P,
    port: u16,
    enter: &[u8],
    action: impl FnOnce(&mut ConfigSpace<'_, P>) -> Result<T, HwError>,
    exit: impl FnOnce(&mut ConfigSpace<'_, P>, &Result<T, HwError>) -> Result<(), HwError>,
) -> Result<T, HwError> {
    for &byte in enter {
        io.write(port, byte)?;
    }
    let mut space = ConfigSpace { io, port };
    let result = action(&mut space);
    let exited = exit(&mut space, &result);
    let value = result?;
    exited?;
    Ok(value)
}

pub(crate) fn chip_matching(matches: impl Fn(&ChipSpec) -> bool) -> Option<&'static ChipSpec> {
    CHIPS.iter().find(|chip| matches(chip))
}

fn read_config<P: PortIo>(io: &mut P, slot: Slot) -> Result<ChipConfig, HwError> {
    if slot == Slot::Primary {
        match ite::read_config(io, true) {
            Err(HwError::ChipNotFound(_) | HwError::NotIte(_)) => {}
            result => return result,
        }
    }
    nuvoton::read_config(io, slot)
}

fn reread_config<P: PortIo>(io: &mut P, current: &ChipConfig) -> Result<ChipConfig, HwError> {
    let changed = |found| HwError::ChipChanged {
        expected: current.chip_id,
        found,
    };
    let config = match current.chip.map {
        ChipMap::Ite(_) => ite::read_config(io, false),
        ChipMap::Nuvoton(_) => nuvoton::read_config(io, current.slot),
    }
    .map_err(|error| match error {
        HwError::UnsupportedChip(found) => changed(found),
        other => other,
    })?;
    if config.chip_id == current.chip_id && std::ptr::eq(config.chip, current.chip) {
        Ok(config)
    } else {
        Err(changed(config.chip_id))
    }
}

pub fn detect<P: PortIo>(
    mut open: impl FnMut(Slot) -> Result<P, HwError>,
    mut lock: impl FnMut() -> Result<NamedMutex, HwError>,
    verify: bool,
) -> Result<SuperIo<P>, HwError> {
    let mut connect = |slot| {
        let io = open(slot)?;
        if verify {
            SuperIo::probe(io, slot, lock()?)
        } else {
            SuperIo::attach(io, slot, lock()?)
        }
    };
    match connect(Slot::Primary) {
        Err(HwError::ChipNotFound(_)) => connect(Slot::Secondary),
        result => result,
    }
}

pub(crate) trait Session {
    fn verify_vendor(&mut self) -> Result<(), HwError>;
    fn read(&mut self) -> Result<ChipReadings, HwError>;
    fn channel_states(&mut self) -> Result<Vec<ChannelState>, HwError>;
    fn set_duty(&mut self, channel: usize, duty: u8) -> Result<(), HwError>;
    fn restore(&mut self, channel: usize, saved: Option<ChannelState>) -> Result<(), HwError>;
}

#[derive(Debug)]
pub struct SuperIo<P: PortIo> {
    pub(crate) io: P,
    lock: NamedMutex,
    pub(crate) config: ChipConfig,
}

impl<P: PortIo> SuperIo<P> {
    pub fn attach(mut io: P, slot: Slot, lock: NamedMutex) -> Result<Self, HwError> {
        let config = {
            let _guard = lock.acquire(DEFAULT_TIMEOUT)?;
            read_config(&mut io, slot)?
        };
        Ok(Self { io, lock, config })
    }

    pub fn probe(io: P, slot: Slot, lock: NamedMutex) -> Result<Self, HwError> {
        let mut chip = Self::attach(io, slot, lock)?;
        chip.transaction(|session| session.verify_vendor())?;
        Ok(chip)
    }

    pub fn reinitialize(&mut self) -> Result<(), HwError> {
        let config = {
            let _guard = self.lock.acquire(DEFAULT_TIMEOUT)?;
            reread_config(&mut self.io, &self.config)?
        };
        self.config = config;
        self.transaction(|session| session.verify_vendor())
    }

    pub fn config(&self) -> ChipConfig {
        self.config
    }

    pub fn chip(&self) -> &'static ChipSpec {
        self.config.chip
    }

    pub fn fan_count(&self) -> usize {
        self.config.chip.fan_count()
    }

    pub fn temp_names(&self) -> Vec<Option<&'static str>> {
        self.config.chip.temp_names()
    }

    pub fn fan_control(&self) -> bool {
        self.config.chip.verified
    }

    fn transaction<T>(
        &mut self,
        action: impl FnOnce(&mut dyn Session) -> Result<T, HwError>,
    ) -> Result<T, HwError> {
        let guard = self.lock.acquire(DEFAULT_TIMEOUT)?;
        let abandoned = guard.was_abandoned();
        if abandoned {
            tracing::warn!(mutex = %self.lock.name(), "bus mutex was abandoned; reinitialising the chip");
            self.config = reread_config(&mut self.io, &self.config)?;
        }
        let base = self.config.base_address;
        match &self.config.chip.map {
            ChipMap::Ite(map) => {
                let mut session = ite::Session::open(&mut self.io, base, map)?;
                if abandoned {
                    session.verify_vendor()?;
                }
                action(&mut session)
            }
            ChipMap::Nuvoton(map) => {
                let mut session = nuvoton::Session::open(&mut self.io, base, map)?;
                let result = action(&mut session);
                let closed = session.close();
                let value = result?;
                closed?;
                Ok(value)
            }
        }
    }

    pub fn restore_patiently(
        &mut self,
        channel: usize,
        saved: Option<ChannelState>,
        budget: Duration,
    ) -> Result<(), HwError> {
        let deadline = Instant::now() + budget;
        loop {
            match self.restore(channel, saved) {
                Err(HwError::LockTimeout(_)) if Instant::now() < deadline => continue,
                Err(HwError::VerifyFailed { .. } | HwError::ReadNotAcknowledged(_))
                    if Instant::now() < deadline =>
                {
                    thread::sleep(Duration::from_millis(20));
                }
                result => return result,
            }
        }
    }

    pub fn read(&mut self) -> Result<ChipReadings, HwError> {
        self.transaction(|session| session.read())
    }

    pub fn channel_states(&mut self) -> Result<Vec<ChannelState>, HwError> {
        self.transaction(|session| session.channel_states())
    }

    pub fn set_duty(&mut self, channel: usize, duty: u8) -> Result<(), HwError> {
        self.set_duty_unless_halted(channel, duty, &AtomicBool::new(false))
    }

    pub fn set_duty_unless_halted(
        &mut self,
        channel: usize,
        duty: u8,
        halted: &AtomicBool,
    ) -> Result<(), HwError> {
        self.check_channel(channel)?;
        if !self.fan_control() {
            return Err(HwError::Unverified(self.config.chip.name));
        }
        self.transaction(|session| {
            if halted.load(Ordering::SeqCst) {
                return Err(HwError::Halted);
            }
            session.set_duty(channel, duty)
        })
    }

    pub fn restore(&mut self, channel: usize, saved: Option<ChannelState>) -> Result<(), HwError> {
        self.check_channel(channel)?;
        if !self.fan_control() {
            return Ok(());
        }
        self.transaction(|session| session.restore(channel, saved))
    }

    pub fn dump(&mut self) -> Result<RegisterDump, HwError> {
        match &self.config.chip.map {
            ChipMap::Ite(map) => ite::dump(&mut self.io, &self.lock, &self.config, map),
            ChipMap::Nuvoton(_) => nuvoton::dump(&mut self.io, &self.lock, &self.config),
        }
    }

    fn check_channel(&self, channel: usize) -> Result<(), HwError> {
        if channel < self.fan_count() {
            Ok(())
        } else {
            Err(HwError::InvalidChannel(channel))
        }
    }
}

impl RegisterDump {
    pub fn diff(&self, other: &Self) -> Vec<String> {
        let mut lines = Vec::new();
        let mut field = |name: &str, a: String, b: String| {
            if a != b {
                lines.push(format!("{name}: {a} -> {b}"));
            }
        };
        field("chip", self.chip.clone(), other.chip.clone());
        field(
            "chip_id",
            format!("{:#06x}", self.chip_id),
            format!("{:#06x}", other.chip_id),
        );
        field(
            "chip_version",
            self.chip_version.to_string(),
            other.chip_version.to_string(),
        );
        field(
            "base_address",
            format!("{:#06x}", self.base_address),
            format!("{:#06x}", other.base_address),
        );
        field(
            "bank_register",
            hex(self.bank_register),
            hex(other.bank_register),
        );
        diff_maps(
            &mut lines,
            "global",
            &self.global_config,
            &other.global_config,
        );
        diff_maps(
            &mut lines,
            &format!("ldn{:x}", self.ldn),
            &self.ec_ldn_config,
            &other.ec_ldn_config,
        );
        for (left, right) in self.banks.iter().zip(&other.banks) {
            for (register, (a, b)) in left.registers.iter().zip(&right.registers).enumerate() {
                if a != b {
                    lines.push(format!(
                        "bank{} {}: {} -> {}",
                        left.bank,
                        hex(register as u8),
                        format_optional(*a),
                        format_optional(*b)
                    ));
                }
            }
        }
        lines
    }
}

fn diff_maps(
    lines: &mut Vec<String>,
    section: &str,
    a: &BTreeMap<String, u8>,
    b: &BTreeMap<String, u8>,
) {
    for (register, left) in a {
        let right = b.get(register).copied();
        if right != Some(*left) {
            lines.push(format!(
                "{section} {register}: {} -> {}",
                hex(*left),
                format_optional(right)
            ));
        }
    }
    for (register, right) in b.iter().filter(|(register, _)| !a.contains_key(*register)) {
        lines.push(format!("{section} {register}: missing -> {}", hex(*right)));
    }
}

fn format_optional(value: Option<u8>) -> String {
    value.map_or_else(|| "unacknowledged".to_owned(), hex)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chips::Ident;

    fn ite(chip_id: u16) -> Option<&'static str> {
        chip_matching(|chip| chip.matches_ite(chip_id)).map(|chip| chip.name)
    }

    fn nuvoton(id: u8, revision: u8) -> Option<&'static str> {
        chip_matching(|chip| chip.matches_nuvoton(id, revision)).map(|chip| chip.name)
    }

    #[test]
    fn chip_ids_map_to_the_expected_rows() {
        let ite_rows = [
            (0x8655, "ITE IT8655E"),
            (0x8665, "ITE IT8665E"),
            (0x8625, "ITE IT8625E"),
            (0x8620, "ITE IT8620E"),
            (0x8628, "ITE IT8628E"),
            (0x8686, "ITE IT8686E"),
            (0x8688, "ITE IT8688E"),
            (0x8689, "ITE IT8689E"),
            (0x8721, "ITE IT8721F"),
            (0x8728, "ITE IT8728F"),
            (0x8772, "ITE IT8772E"),
        ];
        for (chip_id, name) in ite_rows {
            assert_eq!(ite(chip_id), Some(name), "{chip_id:#06x}");
        }
        for chip_id in [
            0x8613, 0x8705, 0x8712, 0x8771, 0x8733, 0x8695, 0x0000, 0xFFFF,
        ] {
            assert_eq!(ite(chip_id), None, "{chip_id:#06x}");
        }

        let nuvoton_rows = [
            (0xB4, 0x70, "Nuvoton NCT6771F"),
            (0xB4, 0x7F, "Nuvoton NCT6771F"),
            (0xC3, 0x33, "Nuvoton NCT6776F"),
            (0xC5, 0x62, "Nuvoton NCT6779D"),
            (0xC8, 0x03, "Nuvoton NCT6791D"),
            (0xC9, 0x11, "Nuvoton NCT6792D"),
            (0xC9, 0x13, "Nuvoton NCT6792D-A"),
            (0xD1, 0x21, "Nuvoton NCT6793D"),
            (0xD3, 0x52, "Nuvoton NCT6795D"),
            (0xD4, 0x23, "Nuvoton NCT6796D"),
            (0xD4, 0x2A, "Nuvoton NCT6796D-R or NCT5585D"),
            (0xD4, 0x51, "Nuvoton NCT6797D"),
            (0xD4, 0x2B, "Nuvoton NCT6798D"),
            (0xD8, 0x02, "Nuvoton NCT6799D or NCT6796D-S"),
        ];
        for (id, revision, name) in nuvoton_rows {
            assert_eq!(
                nuvoton(id, revision),
                Some(name),
                "{id:#04x}/{revision:#04x}"
            );
        }
        for (id, revision) in [
            (0xB4, 0x60),
            (0xC8, 0x13),
            (0xD4, 0x40),
            (0xD5, 0x92),
            (0xD8, 0x06),
            (0xC4, 0x52),
            (0xFF, 0xFF),
        ] {
            assert_eq!(nuvoton(id, revision), None, "{id:#04x}/{revision:#04x}");
        }
    }

    #[test]
    fn fan_control_is_on_only_for_listed_chips_and_never_for_ambiguous_rows() {
        let verified: Vec<&str> = CHIPS
            .iter()
            .filter(|chip| chip.verified)
            .map(|chip| chip.name)
            .collect();
        assert_eq!(verified, ["ITE IT8655E", "ITE IT8689E"]);
        for chip in CHIPS.iter().filter(|chip| chip.name.contains(" or ")) {
            assert!(!chip.verified, "{}", chip.name);
        }
        assert_eq!(
            CHIPS
                .iter()
                .filter(|chip| chip.name.contains(" or "))
                .count(),
            2
        );
    }

    #[test]
    fn table_rows_are_unique_and_consistent() {
        for (index, chip) in CHIPS.iter().enumerate() {
            assert!(
                CHIPS[index + 1..]
                    .iter()
                    .all(|other| other.ident != chip.ident),
                "{}",
                chip.name
            );
            match (&chip.ident, &chip.map) {
                (Ident::Ite(chip_id), ChipMap::Ite(map)) => {
                    assert!(ite::is_ite_id(*chip_id));
                    assert!(map.fans <= 6 && map.temps <= 7);
                    assert!(map.dump_banks == 1 || map.dump_banks == 4);
                }
                (Ident::Nuvoton { .. }, ChipMap::Nuvoton(map)) => {
                    assert!(map.fans <= nuvoton::MODE_REGS.len());
                    let tachs = match &map.tach {
                        crate::chips::Tach::Rpm { registers, .. }
                        | crate::chips::Tach::Count { registers } => registers.len(),
                    };
                    assert!(tachs >= map.fans);
                    assert!(!map.slots.is_empty() && !map.sources.is_empty());
                }
                _ => panic!("{} mixes vendors", chip.name),
            }
        }
    }

    #[test]
    fn nuvoton_states_report_auto_from_the_mode_nibble() {
        let state = |control| ChannelState {
            control,
            duty: 0,
            main_enabled: true,
            kind: ControlKind::Nuvoton,
        };
        assert!(state(0x50).is_auto());
        assert!(state(0x10).is_auto());
        assert!(!state(0x0F).is_auto());
        assert!(
            !ChannelState {
                control: 0x50,
                ..ChannelState::default()
            }
            .is_auto()
        );
    }
}
