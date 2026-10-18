use std::collections::BTreeMap;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::HwError;
use crate::mutex::{DEFAULT_TIMEOUT, NamedMutex};
use crate::port::PortIo;

pub const CHIP_ID: u16 = 0x8655;
pub const FAN_CHANNELS: usize = 3;
pub const TEMP_CHANNELS: usize = 6;
pub const BANK_COUNT: u8 = 4;

const CONFIG_INDEX_PORT: u16 = 0x2E;
const CONFIG_DATA_PORT: u16 = 0x2F;
const ENTER_SEQUENCE: [u8; 4] = [0x87, 0x01, 0x55, 0x55];
const CONFIG_CONTROL_REG: u8 = 0x02;
const CONFIG_EXIT_VALUE: u8 = 0x02;
const LDN_SELECT_REG: u8 = 0x07;
const CHIP_ID_REG: u8 = 0x20;
const CHIP_VERSION_REG: u8 = 0x22;
const BASE_ADDRESS_REG: u8 = 0x60;
const EC_LDN: u8 = 0x04;
const GLOBAL_DUMP_REGS: std::ops::RangeInclusive<u8> = 0x20..=0x2F;
const EC_LDN_DUMP_REGS: [u8; 4] = [0x30, 0x60, 0x61, 0x70];
const EC_LDN_VENDOR_DUMP_REGS: std::ops::RangeInclusive<u8> = 0xF0..=0xFF;

const ADDRESS_PORT_OFFSET: u16 = 5;
const DATA_PORT_OFFSET: u16 = 6;
const BANK_REG: u8 = 0x06;
const BANK_MASK: u8 = 0x60;
const BANK_SHIFT: u8 = 5;
const VENDOR_ID_REG: u8 = 0x58;
const VENDOR_IDS: [u8; 2] = [0x90, 0x7F];
const MAIN_CONTROL_REG: u8 = 0x13;
const PWM_CONTROL_REGS: [u8; FAN_CHANNELS] = [0x15, 0x16, 0x17];
const PWM_DUTY_REGS: [u8; FAN_CHANNELS] = [0x63, 0x6B, 0x73];
const TACH_LOW_REGS: [u8; FAN_CHANNELS] = [0x0D, 0x0E, 0x0F];
const TACH_HIGH_REGS: [u8; FAN_CHANNELS] = [0x18, 0x19, 0x1A];
const TEMP_BASE_REG: u8 = 0x29;
const AUTO_MODE_BIT: u8 = 0x80;
const MIN_VALID_TACH: u16 = 0x3F;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelState {
    pub control: u8,
    pub duty: u8,
    pub main_enabled: bool,
}

impl ChannelState {
    pub fn is_auto(&self) -> bool {
        self.control & AUTO_MODE_BIT != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChipConfig {
    pub chip_id: u16,
    pub version: u8,
    pub base_address: u16,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChipReadings {
    pub rpm: [Option<u32>; FAN_CHANNELS],
    pub temps: [Option<i8>; TEMP_CHANNELS],
    pub channels: [ChannelState; FAN_CHANNELS],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BankDump {
    pub bank: u8,
    pub registers: Vec<Option<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisterDump {
    pub chip_id: u16,
    pub chip_version: u8,
    pub base_address: u16,
    pub global_config: BTreeMap<String, u8>,
    pub ec_ldn_config: BTreeMap<String, u8>,
    pub bank_register: u8,
    pub banks: Vec<BankDump>,
}

pub fn rpm_from_count(count: u16) -> Option<u32> {
    match count {
        0..=MIN_VALID_TACH => None,
        u16::MAX => Some(0),
        _ => Some((1_350_000.0 / (f32::from(count) * 2.0)).round() as u32),
    }
}

pub fn decode_temperature(raw: u8) -> Option<i8> {
    let value = raw as i8;
    (value > 0 && value < i8::MAX).then_some(value)
}

fn hex(register: u8) -> String {
    format!("{register:#04x}")
}

struct ConfigSpace<'a, P: PortIo> {
    io: &'a mut P,
}

impl<P: PortIo> ConfigSpace<'_, P> {
    fn read(&mut self, register: u8) -> Result<u8, HwError> {
        self.io.write(CONFIG_INDEX_PORT, register)?;
        self.io.read(CONFIG_DATA_PORT)
    }

    fn write(&mut self, register: u8, value: u8) -> Result<(), HwError> {
        self.io.write(CONFIG_INDEX_PORT, register)?;
        self.io.write(CONFIG_DATA_PORT, value)
    }

    fn read_word(&mut self, register: u8) -> Result<u16, HwError> {
        let high = self.read(register)?;
        let low = self.read(register + 1)?;
        Ok(u16::from_be_bytes([high, low]))
    }

    fn select(&mut self, ldn: u8) -> Result<(), HwError> {
        self.write(LDN_SELECT_REG, ldn)
    }
}

fn with_config_space<P: PortIo, T>(
    io: &mut P,
    action: impl FnOnce(&mut ConfigSpace<'_, P>) -> Result<T, HwError>,
) -> Result<T, HwError> {
    for byte in ENTER_SEQUENCE {
        io.write(CONFIG_INDEX_PORT, byte)?;
    }
    let mut space = ConfigSpace { io };
    let result = action(&mut space);
    let exited = space.write(CONFIG_CONTROL_REG, CONFIG_EXIT_VALUE);
    let value = result?;
    exited?;
    Ok(value)
}

fn read_chip_config<P: PortIo>(io: &mut P) -> Result<ChipConfig, HwError> {
    with_config_space(io, |space| {
        let chip_id = space.read_word(CHIP_ID_REG)?;
        match chip_id {
            CHIP_ID => {}
            0x0000 | 0xFFFF => return Err(HwError::ChipNotFound(chip_id)),
            other => return Err(HwError::UnsupportedChip(other)),
        }
        let version = space.read(CHIP_VERSION_REG)? & 0x0F;
        space.io.find_bars()?;
        space.select(EC_LDN)?;
        let address = space.read_word(BASE_ADDRESS_REG)?;
        thread::sleep(Duration::from_millis(1));
        let verify = space.read_word(BASE_ADDRESS_REG)?;
        if address != verify || address < 0x100 || address & 0xF007 != 0 {
            return Err(HwError::InvalidBaseAddress { address, verify });
        }
        Ok(ChipConfig {
            chip_id,
            version,
            base_address: address,
        })
    })
}

struct Ec<'a, P: PortIo> {
    io: &'a mut P,
    base: u16,
}

impl<P: PortIo> Ec<'_, P> {
    fn read_raw(&mut self, register: u8) -> Result<(u8, bool), HwError> {
        self.io.write(self.base + ADDRESS_PORT_OFFSET, register)?;
        let value = self.io.read(self.base + DATA_PORT_OFFSET)?;
        let acknowledged = self.io.read(self.base + ADDRESS_PORT_OFFSET)? == register;
        Ok((value, acknowledged))
    }

    fn read(&mut self, register: u8) -> Result<u8, HwError> {
        match self.read_raw(register)? {
            (value, true) => Ok(value),
            (_, false) => Err(HwError::ReadNotAcknowledged(register)),
        }
    }

    fn write(&mut self, register: u8, value: u8) -> Result<(), HwError> {
        self.io.write(self.base + ADDRESS_PORT_OFFSET, register)?;
        self.io.write(self.base + DATA_PORT_OFFSET, value)?;
        self.io.read(self.base + ADDRESS_PORT_OFFSET).map(drop)
    }

    fn write_verified(&mut self, register: u8, value: u8) -> Result<(), HwError> {
        self.write(register, value)?;
        let read = self.read(register)?;
        if read == value {
            Ok(())
        } else {
            Err(HwError::VerifyFailed {
                register,
                written: value,
                read,
            })
        }
    }

    fn select_bank(&mut self, bank: u8) -> Result<(), HwError> {
        let current = self.read(BANK_REG)?;
        let wanted = (current & !BANK_MASK) | ((bank << BANK_SHIFT) & BANK_MASK);
        if current != wanted {
            self.write_verified(BANK_REG, wanted)?;
        }
        Ok(())
    }

    fn verify_vendor(&mut self) -> Result<(), HwError> {
        let vendor = self.read(VENDOR_ID_REG)?;
        if VENDOR_IDS.contains(&vendor) {
            Ok(())
        } else {
            Err(HwError::VendorMismatch(vendor))
        }
    }

    fn channel_state(&mut self, channel: usize, main: u8) -> Result<ChannelState, HwError> {
        Ok(ChannelState {
            control: self.read(PWM_CONTROL_REGS[channel])?,
            duty: self.read(PWM_DUTY_REGS[channel])?,
            main_enabled: main & (1 << channel) != 0,
        })
    }
}

#[derive(Debug)]
pub struct It8655e<P: PortIo> {
    io: P,
    lock: NamedMutex,
    config: ChipConfig,
}

impl<P: PortIo> It8655e<P> {
    pub fn attach(mut io: P, lock: NamedMutex) -> Result<Self, HwError> {
        let config = {
            let _guard = lock.acquire(DEFAULT_TIMEOUT)?;
            read_chip_config(&mut io)?
        };
        Ok(Self { io, lock, config })
    }

    pub fn probe(io: P, lock: NamedMutex) -> Result<Self, HwError> {
        let mut chip = Self::attach(io, lock)?;
        chip.transaction(|ec| ec.verify_vendor())?;
        Ok(chip)
    }

    pub fn reinitialize(&mut self) -> Result<(), HwError> {
        let config = {
            let _guard = self.lock.acquire(DEFAULT_TIMEOUT)?;
            read_chip_config(&mut self.io)?
        };
        self.config = config;
        self.transaction(|ec| ec.verify_vendor())
    }

    pub fn config(&self) -> ChipConfig {
        self.config
    }

    fn transaction<T>(
        &mut self,
        action: impl FnOnce(&mut Ec<'_, P>) -> Result<T, HwError>,
    ) -> Result<T, HwError> {
        let guard = self.lock.acquire(DEFAULT_TIMEOUT)?;
        let abandoned = guard.was_abandoned();
        if abandoned {
            tracing::warn!(mutex = %self.lock.name(), "bus mutex was abandoned; reinitialising the chip");
            self.config = read_chip_config(&mut self.io)?;
        }
        let mut ec = Ec {
            io: &mut self.io,
            base: self.config.base_address,
        };
        ec.select_bank(0)?;
        if abandoned {
            ec.verify_vendor()?;
        }
        action(&mut ec)
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
        self.transaction(|ec| {
            let mut readings = ChipReadings::default();
            let main = ec.read(MAIN_CONTROL_REG)?;
            for channel in 0..FAN_CHANNELS {
                readings.channels[channel] = ec.channel_state(channel, main)?;
                let low = ec.read(TACH_LOW_REGS[channel])?;
                let high = ec.read(TACH_HIGH_REGS[channel])?;
                readings.rpm[channel] = rpm_from_count(u16::from_le_bytes([low, high]));
            }
            for (index, temp) in readings.temps.iter_mut().enumerate() {
                *temp = decode_temperature(ec.read(TEMP_BASE_REG + index as u8)?);
            }
            Ok(readings)
        })
    }

    pub fn channel_states(&mut self) -> Result<[ChannelState; FAN_CHANNELS], HwError> {
        self.transaction(|ec| {
            let main = ec.read(MAIN_CONTROL_REG)?;
            let mut states = [ChannelState::default(); FAN_CHANNELS];
            for (channel, state) in states.iter_mut().enumerate() {
                *state = ec.channel_state(channel, main)?;
            }
            Ok(states)
        })
    }

    pub fn set_duty(&mut self, channel: usize, duty: u8) -> Result<(), HwError> {
        check_channel(channel)?;
        self.transaction(|ec| {
            let main = ec.read(MAIN_CONTROL_REG)?;
            let bit = 1 << channel;
            if main & bit == 0 {
                ec.write_verified(MAIN_CONTROL_REG, main | bit)?;
            }
            let control = ec.read(PWM_CONTROL_REGS[channel])?;
            if control & AUTO_MODE_BIT != 0 {
                ec.write_verified(PWM_CONTROL_REGS[channel], control & !AUTO_MODE_BIT)?;
            }
            ec.write_verified(PWM_DUTY_REGS[channel], duty)
        })
    }

    pub fn restore(&mut self, channel: usize, saved: Option<ChannelState>) -> Result<(), HwError> {
        check_channel(channel)?;
        self.transaction(|ec| {
            let current = ec.read(PWM_CONTROL_REGS[channel])?;
            let control = saved.map_or(current, |state| state.control) | AUTO_MODE_BIT;
            if let Some(state) = saved {
                ec.write(PWM_DUTY_REGS[channel], state.duty)?;
            }
            ec.write_verified(PWM_CONTROL_REGS[channel], control)?;
            if let Some(state) = saved {
                let main = ec.read(MAIN_CONTROL_REG)?;
                let bit = 1 << channel;
                let wanted = if state.main_enabled {
                    main | bit
                } else {
                    main & !bit
                };
                if wanted != main {
                    ec.write_verified(MAIN_CONTROL_REG, wanted)?;
                }
            }
            Ok(())
        })
    }

    pub fn dump(&mut self) -> Result<RegisterDump, HwError> {
        let (global_config, ec_ldn_config) = {
            let _guard = self.lock.acquire(DEFAULT_TIMEOUT)?;
            with_config_space(&mut self.io, |space| {
                let mut global = BTreeMap::new();
                for register in GLOBAL_DUMP_REGS {
                    global.insert(hex(register), space.read(register)?);
                }
                space.select(EC_LDN)?;
                let mut ldn = BTreeMap::new();
                for register in EC_LDN_DUMP_REGS.into_iter().chain(EC_LDN_VENDOR_DUMP_REGS) {
                    ldn.insert(hex(register), space.read(register)?);
                }
                Ok((global, ldn))
            })?
        };

        let bank_register = {
            let _guard = self.lock.acquire(DEFAULT_TIMEOUT)?;
            Ec {
                io: &mut self.io,
                base: self.config.base_address,
            }
            .read(BANK_REG)?
        };

        let mut banks = Vec::with_capacity(usize::from(BANK_COUNT));
        for bank in 0..BANK_COUNT {
            let _guard = self.lock.acquire(DEFAULT_TIMEOUT)?;
            let mut ec = Ec {
                io: &mut self.io,
                base: self.config.base_address,
            };
            let original = ec.read(BANK_REG)?;
            ec.write(BANK_REG, (original & !BANK_MASK) | (bank << BANK_SHIFT))?;
            let registers = (0..=u8::MAX)
                .map(|register| ec.read_raw(register).map(|(value, ok)| ok.then_some(value)))
                .collect::<Result<Vec<_>, _>>();
            ec.write(BANK_REG, original)?;
            banks.push(BankDump {
                bank,
                registers: registers?,
            });
        }

        Ok(RegisterDump {
            chip_id: self.config.chip_id,
            chip_version: self.config.version,
            base_address: self.config.base_address,
            global_config,
            ec_ldn_config,
            bank_register,
            banks,
        })
    }
}

fn check_channel(channel: usize) -> Result<(), HwError> {
    if channel < FAN_CHANNELS {
        Ok(())
    } else {
        Err(HwError::InvalidChannel(channel))
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
            "ldn4",
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

#[cfg(any(test, feature = "test-support"))]
pub mod fake {
    use std::sync::{Arc, Mutex, PoisonError};

    use super::*;

    #[derive(Debug, Clone)]
    pub struct FakeIte {
        pub chip_id: u16,
        pub base: u16,
        pub config_mode: bool,
        pub enter_progress: usize,
        pub config_index: u8,
        pub ldn: u8,
        pub bars_found: bool,
        pub ec_index: u8,
        pub bank_register: u8,
        pub banks: [[u8; 256]; BANK_COUNT as usize],
        pub stuck_registers: Vec<u8>,
        pub offline: bool,
        pub unreadable_registers: Vec<u8>,
        pub exits: usize,
    }

    impl FakeIte {
        pub fn new() -> Self {
            let mut banks = [[0u8; 256]; BANK_COUNT as usize];
            banks[0][usize::from(VENDOR_ID_REG)] = 0x90;
            banks[0][usize::from(MAIN_CONTROL_REG)] = 0b111;
            for channel in 0..FAN_CHANNELS {
                banks[0][usize::from(PWM_CONTROL_REGS[channel])] = AUTO_MODE_BIT | channel as u8;
                banks[0][usize::from(PWM_DUTY_REGS[channel])] = 0x40;
                banks[0][usize::from(TACH_LOW_REGS[channel])] = 0x52;
                banks[0][usize::from(TACH_HIGH_REGS[channel])] = 0x02;
            }
            banks[0][usize::from(TEMP_BASE_REG)] = 41;
            banks[0][usize::from(TEMP_BASE_REG) + 1] = 0x80;
            for bank in &mut banks[1..] {
                bank.fill(0xEE);
            }
            Self {
                chip_id: CHIP_ID,
                base: 0x290,
                config_mode: false,
                enter_progress: 0,
                config_index: 0,
                ldn: 0,
                bars_found: false,
                ec_index: 0,
                bank_register: 0,
                banks,
                stuck_registers: Vec::new(),
                offline: false,
                unreadable_registers: Vec::new(),
                exits: 0,
            }
        }

        pub fn bank(&self) -> usize {
            usize::from((self.bank_register & BANK_MASK) >> BANK_SHIFT)
        }

        pub fn reg(&self, register: u8) -> u8 {
            self.banks[0][usize::from(register)]
        }

        fn is_ec_port(&self, port: u16) -> bool {
            port == self.base + ADDRESS_PORT_OFFSET || port == self.base + DATA_PORT_OFFSET
        }
    }

    impl PortIo for FakeIte {
        fn read(&mut self, port: u16) -> Result<u8, HwError> {
            if self.offline || (self.is_ec_port(port) && !self.bars_found) {
                return Err(HwError::AccessDenied);
            }
            if port == self.base + DATA_PORT_OFFSET
                && self.unreadable_registers.contains(&self.ec_index)
            {
                return Err(HwError::ReadNotAcknowledged(self.ec_index));
            }
            Ok(match port {
                CONFIG_DATA_PORT if self.config_mode => match self.config_index {
                    0x20 => (self.chip_id >> 8) as u8,
                    0x21 => self.chip_id as u8,
                    0x22 => 0x01,
                    0x60 if self.ldn == EC_LDN => (self.base >> 8) as u8,
                    0x61 if self.ldn == EC_LDN => self.base as u8,
                    _ => 0,
                },
                CONFIG_DATA_PORT => 0xFF,
                p if p == self.base + ADDRESS_PORT_OFFSET => self.ec_index,
                p if p == self.base + DATA_PORT_OFFSET => {
                    if self.ec_index == BANK_REG {
                        self.bank_register
                    } else {
                        self.banks[self.bank()][usize::from(self.ec_index)]
                    }
                }
                _ => 0xFF,
            })
        }

        fn write(&mut self, port: u16, value: u8) -> Result<(), HwError> {
            if self.offline || (self.is_ec_port(port) && !self.bars_found) {
                return Err(HwError::AccessDenied);
            }
            match port {
                CONFIG_INDEX_PORT if self.config_mode => self.config_index = value,
                CONFIG_INDEX_PORT => {
                    if value == ENTER_SEQUENCE[self.enter_progress] {
                        self.enter_progress += 1;
                    } else {
                        self.enter_progress = usize::from(value == ENTER_SEQUENCE[0]);
                    }
                    if self.enter_progress == ENTER_SEQUENCE.len() {
                        self.config_mode = true;
                        self.enter_progress = 0;
                    }
                }
                CONFIG_DATA_PORT if self.config_mode => match self.config_index {
                    LDN_SELECT_REG => self.ldn = value,
                    CONFIG_CONTROL_REG if value & CONFIG_EXIT_VALUE != 0 => {
                        self.config_mode = false;
                        self.exits += 1;
                    }
                    _ => {}
                },
                p if p == self.base + ADDRESS_PORT_OFFSET => self.ec_index = value,
                p if p == self.base + DATA_PORT_OFFSET => {
                    if self.stuck_registers.contains(&self.ec_index) {
                        return Ok(());
                    }
                    if self.ec_index == BANK_REG {
                        self.bank_register = value;
                    } else {
                        let bank = self.bank();
                        self.banks[bank][usize::from(self.ec_index)] = value;
                    }
                }
                _ => {}
            }
            Ok(())
        }

        fn find_bars(&mut self) -> Result<(), HwError> {
            if !self.config_mode {
                return Err(HwError::ChipNotFound(0xFFFF));
            }
            self.bars_found = true;
            Ok(())
        }
    }

    impl Default for FakeIte {
        fn default() -> Self {
            Self::new()
        }
    }

    #[derive(Debug, Clone, Default)]
    pub struct SharedFake(Arc<Mutex<FakeIte>>);

    impl SharedFake {
        pub fn new(fake: FakeIte) -> Self {
            Self(Arc::new(Mutex::new(fake)))
        }

        pub fn with<R>(&self, action: impl FnOnce(&mut FakeIte) -> R) -> R {
            action(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner))
        }
    }

    impl PortIo for SharedFake {
        fn read(&mut self, port: u16) -> Result<u8, HwError> {
            self.with(|fake| fake.read(port))
        }

        fn write(&mut self, port: u16, value: u8) -> Result<(), HwError> {
            self.with(|fake| fake.write(port, value))
        }

        fn find_bars(&mut self) -> Result<(), HwError> {
            self.with(PortIo::find_bars)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeIte;
    use super::*;

    fn lock(tag: &str) -> NamedMutex {
        NamedMutex::open(&format!(r"Local\fanforge-ite-{tag}-{}", std::process::id())).unwrap()
    }

    fn probe(fake: FakeIte, tag: &str) -> It8655e<FakeIte> {
        It8655e::probe(fake, lock(tag)).unwrap()
    }

    #[test]
    fn probe_reads_config_and_exits_config_mode() {
        let chip = probe(FakeIte::new(), "probe");
        assert_eq!(
            chip.config(),
            ChipConfig {
                chip_id: CHIP_ID,
                version: 1,
                base_address: 0x290
            }
        );
        assert!(!chip.io.config_mode);
        assert_eq!(chip.io.exits, 1);
    }

    #[test]
    fn probe_rejects_wrong_chips_and_still_exits_config_mode() {
        let mut fake = FakeIte::new();
        fake.chip_id = 0x8665;
        let mut io = fake;
        assert!(matches!(
            read_chip_config(&mut io),
            Err(HwError::UnsupportedChip(0x8665))
        ));
        assert!(!io.config_mode);

        let mut io = FakeIte {
            chip_id: 0xFFFF,
            ..FakeIte::new()
        };
        assert!(matches!(
            read_chip_config(&mut io),
            Err(HwError::ChipNotFound(0xFFFF))
        ));
    }

    #[test]
    fn probe_rejects_invalid_base_addresses() {
        for base in [0x0000, 0x0095, 0x1290] {
            let mut io = FakeIte {
                base,
                ..FakeIte::new()
            };
            assert!(
                matches!(
                    read_chip_config(&mut io),
                    Err(HwError::InvalidBaseAddress { .. })
                ),
                "base {base:#x}"
            );
        }
    }

    #[test]
    fn firmware_selected_bank_is_reset_before_any_access() {
        let mut fake = FakeIte::new();
        fake.bank_register = 0x20 | 0x01;
        let mut chip = probe(fake, "bank");
        assert_eq!(chip.io.bank(), 0);
        assert_eq!(chip.io.bank_register & !BANK_MASK, 0x01);

        let readings = chip.read().unwrap();
        assert_eq!(readings.rpm, [Some(1136); 3]);
        assert_eq!(readings.temps[0], Some(41));
        assert_eq!(readings.temps[1], None);
        assert!(readings.channels.iter().all(ChannelState::is_auto));

        chip.io.bank_register = 0x40;
        assert_eq!(chip.read().unwrap().rpm, [Some(1136); 3]);
    }

    #[test]
    fn set_duty_switches_to_software_mode_and_restore_returns_to_auto() {
        let mut chip = probe(FakeIte::new(), "duty");
        let saved = chip.channel_states().unwrap()[1];
        chip.io.banks[0][usize::from(MAIN_CONTROL_REG)] = 0b101;

        chip.set_duty(1, 0x99).unwrap();
        assert_eq!(chip.io.reg(PWM_DUTY_REGS[1]), 0x99);
        assert_eq!(chip.io.reg(PWM_CONTROL_REGS[1]), 0x01);
        assert_eq!(chip.io.reg(MAIN_CONTROL_REG), 0b111);
        assert!(!chip.read().unwrap().channels[1].is_auto());

        chip.restore(1, Some(saved)).unwrap();
        assert_eq!(chip.io.reg(PWM_CONTROL_REGS[1]), AUTO_MODE_BIT | 0x01);
        assert_eq!(chip.io.reg(PWM_DUTY_REGS[1]), 0x40);
        assert_eq!(chip.io.reg(MAIN_CONTROL_REG), 0b111);
    }

    #[test]
    fn restore_without_snapshot_still_forces_auto() {
        let mut chip = probe(FakeIte::new(), "force");
        chip.set_duty(2, 0x10).unwrap();
        chip.restore(2, None).unwrap();
        assert_eq!(chip.io.reg(PWM_CONTROL_REGS[2]), AUTO_MODE_BIT | 0x02);
        assert_eq!(chip.io.reg(PWM_DUTY_REGS[2]), 0x10);
    }

    #[test]
    fn writes_that_do_not_stick_are_reported() {
        let mut fake = FakeIte::new();
        fake.stuck_registers.push(PWM_DUTY_REGS[0]);
        let mut chip = probe(fake, "stuck");
        assert!(matches!(
            chip.set_duty(0, 0x80),
            Err(HwError::VerifyFailed { .. })
        ));
        assert!(matches!(
            chip.set_duty(7, 0x80),
            Err(HwError::InvalidChannel(7))
        ));
    }

    #[test]
    fn dump_observes_without_changing_the_bank() {
        let mut fake = FakeIte::new();
        fake.bank_register = 0x40;
        let mut chip = It8655e::attach(fake, lock("dump")).unwrap();
        let dump = chip.dump().unwrap();
        assert_eq!(dump.bank_register, 0x40);
        assert_eq!(dump.banks.len(), usize::from(BANK_COUNT));
        assert_eq!(
            dump.banks[0].registers[usize::from(VENDOR_ID_REG)],
            Some(0x90)
        );
        assert_eq!(dump.banks[2].registers[0x10], Some(0xEE));
        assert_eq!(chip.io.bank_register, 0x40);
        assert!(dump.global_config.contains_key("0x20"));
        assert_eq!(dump.ec_ldn_config["0x60"], 0x02);

        let mut changed = dump.clone();
        changed.bank_register = 0x00;
        changed.banks[0].registers[0x15] = Some(0x01);
        let diff = dump.diff(&changed);
        assert_eq!(
            diff,
            vec!["bank_register: 0x40 -> 0x00", "bank0 0x15: 0x80 -> 0x01"]
        );
    }

    #[test]
    fn abandoned_mutex_triggers_full_reinitialisation() {
        let name = format!(r"Local\fanforge-ite-abandon-{}", std::process::id());
        let mut chip = It8655e::probe(FakeIte::new(), NamedMutex::open(&name).unwrap()).unwrap();
        let exits = chip.io.exits;
        let owner = name.clone();
        thread::spawn(move || {
            let mutex = NamedMutex::open(&owner).unwrap();
            std::mem::forget(mutex.acquire(DEFAULT_TIMEOUT).unwrap());
            std::mem::forget(mutex);
        })
        .join()
        .unwrap();
        chip.read().unwrap();
        assert_eq!(chip.io.exits, exits + 1);
        chip.read().unwrap();
        assert_eq!(chip.io.exits, exits + 1);
    }

    #[test]
    fn patient_restore_waits_out_a_busy_bus() {
        let name = format!(r"Local\fanforge-ite-busy-{}", std::process::id());
        let mut chip = It8655e::probe(FakeIte::new(), NamedMutex::open(&name).unwrap()).unwrap();
        chip.set_duty(0, 0x20).unwrap();
        let (held, release) = std::sync::mpsc::channel::<()>();
        let (ready_tx, ready) = std::sync::mpsc::channel();
        let holder = thread::spawn(move || {
            let mutex = NamedMutex::open(&name).unwrap();
            let _guard = mutex.acquire(DEFAULT_TIMEOUT).unwrap();
            ready_tx.send(()).unwrap();
            release.recv_timeout(Duration::from_millis(300)).ok();
        });
        ready.recv().unwrap();
        assert!(matches!(
            chip.restore(0, None),
            Err(HwError::LockTimeout(_))
        ));
        chip.restore_patiently(0, None, Duration::from_secs(5))
            .unwrap();
        drop(held);
        holder.join().unwrap();
        assert!(chip.channel_states().unwrap()[0].is_auto());
    }

    #[test]
    fn tach_and_temperature_decoding() {
        assert_eq!(rpm_from_count(0), None);
        assert_eq!(rpm_from_count(0x3F), None);
        assert_eq!(rpm_from_count(u16::MAX), Some(0));
        assert_eq!(rpm_from_count(675), Some(1000));
        assert_eq!(decode_temperature(0), None);
        assert_eq!(decode_temperature(127), None);
        assert_eq!(decode_temperature(0x80), None);
        assert_eq!(decode_temperature(55), Some(55));
    }
}
