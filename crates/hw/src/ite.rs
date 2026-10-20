use crate::HwError;
use crate::chips::IteMap;
use crate::mutex::{DEFAULT_TIMEOUT, NamedMutex};
use crate::port::PortIo;
use crate::superio::{
    BankDump, CHIP_ID_REG, ChannelState, ChipConfig, ChipReadings, ConfigSpace, ControlKind,
    RegisterDump, Slot, chip_matching, with_config_space,
};

pub const IT8655E_ID: u16 = 0x8655;
pub(crate) const AUTO_MODE_BIT: u8 = 0x80;
pub(crate) const EC_LDN: u8 = 0x04;

const CONFIG_PORT: u16 = 0x2E;
const ENTER_SEQUENCE: [u8; 4] = [0x87, 0x01, 0x55, 0x55];
const CONFIG_CONTROL_REG: u8 = 0x02;
const CONFIG_EXIT_VALUE: u8 = 0x02;
const CHIP_VERSION_REG: u8 = 0x22;
const EC_LDN_DUMP_REGS: [u8; 20] = [
    0x30, 0x60, 0x61, 0x70, 0xF0, 0xF1, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA, 0xFB,
    0xFC, 0xFD, 0xFE, 0xFF,
];

const ADDRESS_PORT_OFFSET: u16 = 5;
const DATA_PORT_OFFSET: u16 = 6;
const CONFIGURATION_REG: u8 = 0x00;
const CONFIGURATION_READY: u8 = 0x10;
const BANK_REG: u8 = 0x06;
const BANK_MASK: u8 = 0x60;
const BANK_SHIFT: u8 = 5;
const VENDOR_ID_REG: u8 = 0x58;
const VENDOR_IDS: [u8; 2] = [0x90, 0x7F];
const MAIN_CONTROL_REG: u8 = 0x13;
const MAIN_CONTROL_CHANNELS: usize = 3;
const PWM_DUTY_REGS: [u8; 6] = [0x63, 0x6B, 0x73, 0x7B, 0xA3, 0xAB];
const TEMP_BASE_REG: u8 = 0x29;
const MIN_VALID_TACH: u16 = 0x3F;

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

fn exit_config<P: PortIo, T>(
    space: &mut ConfigSpace<'_, P>,
    result: &Result<T, HwError>,
) -> Result<(), HwError> {
    if matches!(result, Err(HwError::NotIte(_))) {
        return Ok(());
    }
    space.write(CONFIG_CONTROL_REG, CONFIG_EXIT_VALUE)
}

pub(crate) fn is_ite_id(chip_id: u16) -> bool {
    matches!(chip_id >> 8, 0x86 | 0x87)
}

pub(crate) fn read_config<P: PortIo>(io: &mut P) -> Result<ChipConfig, HwError> {
    with_config_space(
        io,
        CONFIG_PORT,
        &ENTER_SEQUENCE,
        |space| {
            let chip_id = space.read_word(CHIP_ID_REG)?;
            let chip = match chip_matching(|chip| chip.matches_ite(chip_id)) {
                Some(chip) => chip,
                None if matches!(chip_id, 0x0000 | 0xFFFF) => {
                    return Err(HwError::ChipNotFound(chip_id));
                }
                None if is_ite_id(chip_id) => return Err(HwError::UnsupportedChip(chip_id)),
                None => return Err(HwError::NotIte(chip_id)),
            };
            let version = space.read(CHIP_VERSION_REG)? & 0x0F;
            space.io.find_bars()?;
            space.select(EC_LDN)?;
            let base_address = space.read_base_address()?;
            Ok(ChipConfig {
                chip,
                slot: Slot::Primary,
                chip_id,
                version,
                base_address,
            })
        },
        exit_config,
    )
}

struct Ec<'a, P: PortIo> {
    io: &'a mut P,
    base: u16,
    index_echo: bool,
}

impl<'a, P: PortIo> Ec<'a, P> {
    fn new(io: &'a mut P, base: u16, map: &IteMap) -> Self {
        Self {
            io,
            base,
            index_echo: map.index_echo,
        }
    }

    fn read_raw(&mut self, register: u8) -> Result<(u8, bool), HwError> {
        self.io.write(self.base + ADDRESS_PORT_OFFSET, register)?;
        let value = self.io.read(self.base + DATA_PORT_OFFSET)?;
        let echoed = self.io.read(self.base + ADDRESS_PORT_OFFSET)? == register;
        Ok((value, echoed || !self.index_echo))
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
                register: u16::from(register),
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
}

pub(crate) struct Session<'a, P: PortIo> {
    ec: Ec<'a, P>,
    map: &'static IteMap,
}

impl<'a, P: PortIo> Session<'a, P> {
    pub(crate) fn open(io: &'a mut P, base: u16, map: &'static IteMap) -> Result<Self, HwError> {
        let mut ec = Ec::new(io, base, map);
        if map.bank_select {
            ec.select_bank(0)?;
        }
        Ok(Self { ec, map })
    }

    fn channel_state(&mut self, channel: usize, main: u8) -> Result<ChannelState, HwError> {
        Ok(ChannelState {
            control: self.ec.read(self.map.pwm_control[channel])?,
            duty: self.ec.read(PWM_DUTY_REGS[channel])?,
            main_enabled: channel >= MAIN_CONTROL_CHANNELS || main & (1 << channel) != 0,
            kind: ControlKind::Ite,
        })
    }

    fn rpm(&mut self, channel: usize) -> Result<Option<u32>, HwError> {
        if let Some((register, bit)) = self.map.tach_enable[channel]
            && self.ec.read(register)? & (1 << bit) == 0
        {
            return Ok(None);
        }
        let low = self.ec.read(self.map.tach_low[channel])?;
        let high = self.ec.read(self.map.tach_high[channel])?;
        Ok(rpm_from_count(u16::from_le_bytes([low, high])))
    }
}

impl<P: PortIo> crate::superio::Session for Session<'_, P> {
    fn verify_vendor(&mut self) -> Result<(), HwError> {
        let vendor = self.ec.read(VENDOR_ID_REG)?;
        if !VENDOR_IDS.contains(&vendor) {
            return Err(HwError::VendorMismatch(u16::from(vendor)));
        }
        if self.map.config_check {
            let configuration = self.ec.read(CONFIGURATION_REG)?;
            if configuration & CONFIGURATION_READY == 0 {
                return Err(HwError::UnexpectedConfiguration(configuration));
            }
        }
        Ok(())
    }

    fn read(&mut self) -> Result<ChipReadings, HwError> {
        let mut readings = ChipReadings::default();
        let main = self.ec.read(MAIN_CONTROL_REG)?;
        for channel in 0..self.map.fans {
            readings.channels.push(self.channel_state(channel, main)?);
            readings.rpm.push(self.rpm(channel)?);
        }
        for index in 0..self.map.temps {
            let raw = self.ec.read(TEMP_BASE_REG + index as u8)?;
            readings.temps.push(decode_temperature(raw).map(f32::from));
        }
        Ok(readings)
    }

    fn channel_states(&mut self) -> Result<Vec<ChannelState>, HwError> {
        let main = self.ec.read(MAIN_CONTROL_REG)?;
        (0..self.map.fans)
            .map(|channel| self.channel_state(channel, main))
            .collect()
    }

    fn set_duty(&mut self, channel: usize, duty: u8) -> Result<(), HwError> {
        if channel < MAIN_CONTROL_CHANNELS {
            let main = self.ec.read(MAIN_CONTROL_REG)?;
            let bit = 1 << channel;
            if main & bit == 0 {
                self.ec.write_verified(MAIN_CONTROL_REG, main | bit)?;
            }
        }
        let register = self.map.pwm_control[channel];
        let control = self.ec.read(register)?;
        match self.map.manual_control {
            Some(manual) if control != manual => self.ec.write_verified(register, manual)?,
            None if control & AUTO_MODE_BIT != 0 => {
                self.ec.write_verified(register, control & !AUTO_MODE_BIT)?;
            }
            _ => {}
        }
        self.ec.write_verified(PWM_DUTY_REGS[channel], duty)
    }

    fn restore(&mut self, channel: usize, saved: Option<ChannelState>) -> Result<(), HwError> {
        let saved = saved.filter(|state| state.kind == ControlKind::Ite);
        let register = self.map.pwm_control[channel];
        let current = self.ec.read(register)?;
        let control = saved.map_or(current, |state| state.control) | AUTO_MODE_BIT;
        if let Some(state) = saved {
            self.ec.write(PWM_DUTY_REGS[channel], state.duty)?;
        }
        self.ec.write_verified(register, control)?;
        if let Some(state) = saved
            && channel < MAIN_CONTROL_CHANNELS
        {
            let main = self.ec.read(MAIN_CONTROL_REG)?;
            let bit = 1 << channel;
            let wanted = if state.main_enabled {
                main | bit
            } else {
                main & !bit
            };
            if wanted != main {
                self.ec.write_verified(MAIN_CONTROL_REG, wanted)?;
            }
        }
        Ok(())
    }
}

pub(crate) fn dump<P: PortIo>(
    io: &mut P,
    lock: &NamedMutex,
    config: &ChipConfig,
    map: &IteMap,
) -> Result<RegisterDump, HwError> {
    let (global_config, ec_ldn_config) = {
        let _guard = lock.acquire(DEFAULT_TIMEOUT)?;
        with_config_space(
            io,
            CONFIG_PORT,
            &ENTER_SEQUENCE,
            |space| space.dump(EC_LDN, &EC_LDN_DUMP_REGS),
            exit_config,
        )?
    };

    let bank_register = {
        let _guard = lock.acquire(DEFAULT_TIMEOUT)?;
        Ec::new(&mut *io, config.base_address, map).read(BANK_REG)?
    };

    let mut banks = Vec::with_capacity(usize::from(map.dump_banks));
    for bank in 0..map.dump_banks {
        let _guard = lock.acquire(DEFAULT_TIMEOUT)?;
        let mut ec = Ec::new(&mut *io, config.base_address, map);
        let switch = map.dump_banks > 1;
        let original = if switch {
            let original = ec.read(BANK_REG)?;
            ec.write(BANK_REG, (original & !BANK_MASK) | (bank << BANK_SHIFT))?;
            Some(original)
        } else {
            None
        };
        let registers = (0..=u8::MAX)
            .map(|register| ec.read_raw(register).map(|(value, ok)| ok.then_some(value)))
            .collect::<Result<Vec<_>, _>>();
        if let Some(original) = original {
            ec.write(BANK_REG, original)?;
        }
        banks.push(BankDump {
            bank,
            registers: registers?,
        });
    }

    Ok(RegisterDump {
        chip: config.chip.name.to_owned(),
        chip_id: config.chip_id,
        chip_version: config.version,
        base_address: config.base_address,
        global_config,
        ldn: EC_LDN,
        ec_ldn_config,
        bank_register,
        banks,
    })
}

#[cfg(any(test, feature = "test-support"))]
pub mod fake {
    use std::sync::{Arc, Mutex, PoisonError};

    use super::*;

    pub const FAKE_BANKS: usize = 4;

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
        pub banks: [[u8; 256]; FAKE_BANKS],
        pub stuck_registers: Vec<u8>,
        pub offline: bool,
        pub unreadable_registers: Vec<u8>,
        pub echo_index: bool,
        pub exits: usize,
        pub writes: Vec<(u16, u8, u8)>,
    }

    impl FakeIte {
        pub fn new() -> Self {
            let mut banks = [[0u8; 256]; FAKE_BANKS];
            banks[0][usize::from(VENDOR_ID_REG)] = 0x90;
            banks[0][usize::from(MAIN_CONTROL_REG)] = 0b111;
            for channel in 0..MAIN_CONTROL_CHANNELS {
                Self::fill_channel(&mut banks[0], &crate::chips::CHIPS[0], channel);
            }
            banks[0][usize::from(TEMP_BASE_REG)] = 41;
            banks[0][usize::from(TEMP_BASE_REG) + 1] = 0x80;
            for bank in &mut banks[1..] {
                bank.fill(0xEE);
            }
            Self {
                chip_id: IT8655E_ID,
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
                echo_index: true,
                exits: 0,
                writes: Vec::new(),
            }
        }

        pub fn chip(chip_id: u16) -> Self {
            let mut fake = Self::new();
            fake.chip_id = chip_id;
            fake.banks[0][usize::from(CONFIGURATION_REG)] = CONFIGURATION_READY;
            if let Some(spec) = chip_matching(|chip| chip.matches_ite(chip_id)) {
                for channel in 0..spec.fan_count() {
                    Self::fill_channel(&mut fake.banks[0], spec, channel);
                }
            }
            fake
        }

        fn fill_channel(bank: &mut [u8; 256], spec: &crate::chips::ChipSpec, channel: usize) {
            let crate::chips::ChipMap::Ite(map) = &spec.map else {
                return;
            };
            bank[usize::from(map.pwm_control[channel])] = AUTO_MODE_BIT | channel as u8;
            bank[usize::from(PWM_DUTY_REGS[channel])] = 0x40;
            bank[usize::from(map.tach_low[channel])] = 0x52;
            bank[usize::from(map.tach_high[channel])] = 0x02;
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
                0x2F if self.config_mode => match self.config_index {
                    0x20 => (self.chip_id >> 8) as u8,
                    0x21 => self.chip_id as u8,
                    0x22 => 0x01,
                    0x60 if self.ldn == EC_LDN => (self.base >> 8) as u8,
                    0x61 if self.ldn == EC_LDN => self.base as u8,
                    _ => 0,
                },
                0x2F => 0xFF,
                p if p == self.base + ADDRESS_PORT_OFFSET => {
                    if self.echo_index {
                        self.ec_index
                    } else {
                        0xFF
                    }
                }
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
            let index = if port == 0x2F {
                self.config_index
            } else {
                self.ec_index
            };
            self.writes.push((port, value, index));
            match port {
                CONFIG_PORT if self.config_mode => self.config_index = value,
                CONFIG_PORT => {
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
                0x2F if self.config_mode => match self.config_index {
                    0x07 => self.ldn = value,
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
    use std::thread;
    use std::time::Duration;

    use super::fake::{FAKE_BANKS, FakeIte};
    use super::*;
    use crate::chips::CHIPS;
    use crate::superio::SuperIo;

    const CONTROL: [u8; 3] = [0x15, 0x16, 0x17];

    fn lock(tag: &str) -> NamedMutex {
        NamedMutex::open(&format!(r"Local\fanforge-ite-{tag}-{}", std::process::id())).unwrap()
    }

    fn probe(fake: FakeIte, tag: &str) -> SuperIo<FakeIte> {
        SuperIo::probe(fake, Slot::Primary, lock(tag)).unwrap()
    }

    fn data_writes(fake: &FakeIte) -> Vec<(u8, u8)> {
        fake.writes
            .iter()
            .filter(|(port, ..)| *port == fake.base + DATA_PORT_OFFSET)
            .map(|&(_, value, index)| (index, value))
            .collect()
    }

    #[test]
    fn probe_reads_config_and_exits_config_mode() {
        let chip = probe(FakeIte::new(), "probe");
        assert_eq!(
            chip.config(),
            ChipConfig {
                chip: &CHIPS[0],
                slot: Slot::Primary,
                chip_id: IT8655E_ID,
                version: 1,
                base_address: 0x290
            }
        );
        assert!(chip.fan_control());
        assert!(!chip.io.config_mode);
        assert_eq!(chip.io.exits, 1);
    }

    #[test]
    fn probe_rejects_wrong_chips_and_still_exits_config_mode() {
        let mut io = FakeIte {
            chip_id: 0x8613,
            ..FakeIte::new()
        };
        assert!(matches!(
            read_config(&mut io),
            Err(HwError::UnsupportedChip(0x8613))
        ));
        assert!(!io.config_mode);

        let mut io = FakeIte {
            chip_id: 0xFFFF,
            ..FakeIte::new()
        };
        assert!(matches!(
            read_config(&mut io),
            Err(HwError::ChipNotFound(0xFFFF))
        ));
    }

    #[test]
    fn a_foreign_chip_in_config_mode_is_not_sent_the_ite_exit() {
        let mut io = FakeIte {
            chip_id: 0xD42B,
            ..FakeIte::new()
        };
        assert!(matches!(read_config(&mut io), Err(HwError::NotIte(0xD42B))));
        assert!(io.config_mode);
        assert_eq!(io.exits, 0);
        assert!(io.writes.iter().all(|&(port, ..)| port == CONFIG_PORT));
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
                    read_config(&mut io),
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
        assert_eq!(readings.temps[0], Some(41.0));
        assert_eq!(readings.temps[1], None);
        assert_eq!(readings.temps.len(), 6);
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
        assert_eq!(chip.io.reg(CONTROL[1]), 0x01);
        assert_eq!(chip.io.reg(MAIN_CONTROL_REG), 0b111);
        assert!(!chip.read().unwrap().channels[1].is_auto());

        chip.restore(1, Some(saved)).unwrap();
        assert_eq!(chip.io.reg(CONTROL[1]), AUTO_MODE_BIT | 0x01);
        assert_eq!(chip.io.reg(PWM_DUTY_REGS[1]), 0x40);
        assert_eq!(chip.io.reg(MAIN_CONTROL_REG), 0b111);
    }

    #[test]
    fn restore_without_snapshot_still_forces_auto() {
        let mut chip = probe(FakeIte::new(), "force");
        chip.set_duty(2, 0x10).unwrap();
        chip.restore(2, None).unwrap();
        assert_eq!(chip.io.reg(CONTROL[2]), AUTO_MODE_BIT | 0x02);
        assert_eq!(chip.io.reg(PWM_DUTY_REGS[2]), 0x10);
    }

    #[test]
    fn a_saved_state_of_another_kind_is_ignored() {
        let mut chip = probe(FakeIte::new(), "kind");
        chip.set_duty(0, 0x10).unwrap();
        let foreign = ChannelState {
            control: 0x00,
            duty: 0x77,
            main_enabled: false,
            kind: ControlKind::Nuvoton,
        };
        chip.restore(0, Some(foreign)).unwrap();
        assert_eq!(chip.io.reg(CONTROL[0]), AUTO_MODE_BIT);
        assert_eq!(chip.io.reg(PWM_DUTY_REGS[0]), 0x10);
        assert_eq!(chip.io.reg(MAIN_CONTROL_REG), 0b111);
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
        let mut chip = SuperIo::attach(fake, Slot::Primary, lock("dump")).unwrap();
        let dump = chip.dump().unwrap();
        assert_eq!(dump.chip, "ITE IT8655E");
        assert_eq!(dump.bank_register, 0x40);
        assert_eq!(dump.banks.len(), FAKE_BANKS);
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
        changed.ec_ldn_config.insert("0x70".to_owned(), 0x09);
        let diff = dump.diff(&changed);
        assert_eq!(
            diff,
            vec![
                "bank_register: 0x40 -> 0x00",
                "ldn4 0x70: 0x00 -> 0x09",
                "bank0 0x15: 0x80 -> 0x01"
            ]
        );
    }

    #[test]
    fn dumps_written_before_the_chip_field_still_load() {
        let mut chip = SuperIo::attach(FakeIte::new(), Slot::Primary, lock("old-dump")).unwrap();
        let mut json = serde_json::to_value(chip.dump().unwrap()).unwrap();
        let object = json.as_object_mut().unwrap();
        object.remove("chip");
        object.remove("ldn");
        let old: RegisterDump = serde_json::from_value(json).unwrap();
        assert_eq!(old.ldn, EC_LDN);
        assert!(old.chip.is_empty());
    }

    #[test]
    fn abandoned_mutex_triggers_full_reinitialisation() {
        let name = format!(r"Local\fanforge-ite-abandon-{}", std::process::id());
        let mut chip = SuperIo::probe(
            FakeIte::new(),
            Slot::Primary,
            NamedMutex::open(&name).unwrap(),
        )
        .unwrap();
        let exits = chip.io.exits;
        abandon(&name);
        chip.read().unwrap();
        assert_eq!(chip.io.exits, exits + 1);
        chip.read().unwrap();
        assert_eq!(chip.io.exits, exits + 1);
    }

    fn abandon(name: &str) {
        let owner = name.to_owned();
        thread::spawn(move || {
            let mutex = NamedMutex::open(&owner).unwrap();
            std::mem::forget(mutex.acquire(DEFAULT_TIMEOUT).unwrap());
            std::mem::forget(mutex);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn a_different_chip_after_reinitialisation_is_refused() {
        let mut chip = probe(FakeIte::new(), "changed");
        chip.io.chip_id = 0x8686;
        chip.io.banks[0][usize::from(CONFIGURATION_REG)] = CONFIGURATION_READY;
        assert!(matches!(
            chip.reinitialize(),
            Err(HwError::ChipChanged {
                expected: IT8655E_ID,
                found: 0x8686
            })
        ));
        assert!(chip.fan_control());
        chip.set_duty(0, 0x20).unwrap();
        chip.io.chip_id = IT8655E_ID;
        chip.restore(0, None).unwrap();
        assert_eq!(chip.io.reg(CONTROL[0]) & AUTO_MODE_BIT, AUTO_MODE_BIT);
    }

    #[test]
    fn a_different_chip_after_an_abandoned_mutex_is_refused() {
        let name = format!(r"Local\fanforge-ite-abandon-changed-{}", std::process::id());
        let mut chip = SuperIo::probe(
            FakeIte::new(),
            Slot::Primary,
            NamedMutex::open(&name).unwrap(),
        )
        .unwrap();
        chip.io.chip_id = 0x8628;
        abandon(&name);
        assert!(matches!(chip.read(), Err(HwError::ChipChanged { .. })));
        assert!(std::ptr::eq(chip.chip(), &CHIPS[0]));
    }

    #[test]
    fn patient_restore_waits_out_a_busy_bus() {
        let name = format!(r"Local\fanforge-ite-busy-{}", std::process::id());
        let mut chip = SuperIo::probe(
            FakeIte::new(),
            Slot::Primary,
            NamedMutex::open(&name).unwrap(),
        )
        .unwrap();
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

    struct Recorder {
        fake: FakeIte,
        trace: Vec<String>,
        drop_write_to: Option<u8>,
    }

    impl PortIo for Recorder {
        fn read(&mut self, port: u16) -> Result<u8, HwError> {
            let value = self.fake.read(port)?;
            self.trace.push(format!("r {port:04x} {value:02x}"));
            Ok(value)
        }

        fn write(&mut self, port: u16, value: u8) -> Result<(), HwError> {
            self.trace.push(format!("w {port:04x} {value:02x}"));
            if port == 0x296 && self.drop_write_to == Some(self.fake.ec_index) {
                self.drop_write_to = None;
                return Ok(());
            }
            self.fake.write(port, value)
        }

        fn find_bars(&mut self) -> Result<(), HwError> {
            self.trace.push("bars".to_owned());
            self.fake.find_bars()
        }
    }

    #[test]
    fn it8655e_port_traffic_matches_the_original_driver() {
        let mut fake = FakeIte::new();
        fake.bank_register = 0x21;
        let name = format!(r"Local\fanforge-ite-trace-{}", std::process::id());
        let recorder = Recorder {
            fake,
            trace: Vec::new(),
            drop_write_to: None,
        };
        let mut chip =
            SuperIo::probe(recorder, Slot::Primary, NamedMutex::open(&name).unwrap()).unwrap();
        let mark = |chip: &mut SuperIo<Recorder>, label: &str| {
            chip.io.trace.push(format!("# {label}"));
        };
        mark(&mut chip, "read");
        chip.read().unwrap();
        mark(&mut chip, "channel_states");
        let saved = chip.channel_states().unwrap()[1];
        mark(&mut chip, "set_duty");
        chip.set_duty(1, 0x99).unwrap();
        mark(&mut chip, "restore saved");
        chip.restore(1, Some(saved)).unwrap();
        mark(&mut chip, "restore none");
        chip.restore(2, None).unwrap();
        mark(&mut chip, "reinitialize");
        chip.reinitialize().unwrap();
        mark(&mut chip, "abandoned");
        abandon(&name);
        chip.read().unwrap();
        mark(&mut chip, "patient");
        chip.set_duty(0, 0x20).unwrap();
        chip.io.drop_write_to = Some(0x15);
        chip.restore_patiently(0, None, Duration::from_secs(5))
            .unwrap();
        mark(&mut chip, "dump");
        chip.io.fake.bank_register = 0x41;
        chip.dump().unwrap();

        let golden = include_str!("testdata/it8655e_trace.txt");
        let expected: Vec<&str> = golden.lines().collect();
        let actual: Vec<&str> = chip.io.trace.iter().map(String::as_str).collect();
        for (line, (want, got)) in expected.iter().zip(&actual).enumerate() {
            assert_eq!(want, got, "trace differs at line {}", line + 1);
        }
        assert_eq!(expected.len(), actual.len());
    }

    #[test]
    fn it8686e_reads_five_channels_and_honours_tach_enable_bits() {
        let mut fake = FakeIte::chip(0x8686);
        fake.banks[0][0x0C] = 1 << 4;
        let mut chip = probe(fake, "8686");
        assert_eq!(chip.chip().name, "ITE IT8686E");
        assert!(!chip.fan_control());
        assert_eq!(chip.fan_count(), 5);
        let readings = chip.read().unwrap();
        assert_eq!(readings.channels.len(), 5);
        assert_eq!(readings.temps.len(), 7);
        assert_eq!(
            readings.rpm,
            [Some(1136), Some(1136), Some(1136), Some(1136), None]
        );
        assert_eq!(readings.channels[4].control, AUTO_MODE_BIT | 4);
        assert!(readings.channels[3].main_enabled);
        assert!(data_writes(&chip.io).is_empty());
    }

    #[test]
    fn it8665e_uses_the_alternate_registers_and_selects_bank_zero() {
        let mut fake = FakeIte::chip(0x8665);
        fake.banks[0][usize::from(CONFIGURATION_REG)] = 0;
        fake.banks[0][0x0C] = (1 << 4) | (1 << 5) | (1 << 2);
        fake.banks[0][0x0B] = 0;
        fake.banks[0][0x92] = AUTO_MODE_BIT | 0x05;
        fake.bank_register = 0x20;
        let mut chip = probe(fake, "8665");
        let readings = chip.read().unwrap();
        assert_eq!(chip.io.bank(), 0);
        assert_eq!(readings.channels[5].control, AUTO_MODE_BIT | 0x05);
        assert_eq!(readings.rpm[5], None);
        assert_eq!(readings.rpm[4], Some(1136));
        chip.io.banks[0][0x0B] = 1 << 3;
        assert_eq!(chip.read().unwrap().rpm[5], Some(1136));
        assert_eq!(data_writes(&chip.io), vec![(BANK_REG, 0x00)]);
    }

    #[test]
    fn chips_that_need_it_must_report_a_running_monitor() {
        let mut fake = FakeIte::chip(0x8628);
        fake.banks[0][usize::from(CONFIGURATION_REG)] = 0;
        assert!(matches!(
            SuperIo::probe(fake, Slot::Primary, lock("config")),
            Err(HwError::UnexpectedConfiguration(0))
        ));
    }

    #[test]
    fn only_the_it8688e_tolerates_an_index_port_that_does_not_echo() {
        let mut fake = FakeIte::chip(0x8688);
        fake.echo_index = false;
        let mut chip = probe(fake, "8688");
        assert_eq!(chip.read().unwrap().rpm.len(), 5);

        let mut fake = FakeIte::chip(0x8686);
        fake.echo_index = false;
        assert!(matches!(
            SuperIo::probe(fake, Slot::Primary, lock("8686-echo")),
            Err(HwError::ReadNotAcknowledged(_))
        ));
    }

    #[test]
    fn it8689e_dumps_four_banks_and_puts_the_bank_back() {
        let mut fake = FakeIte::chip(0x8689);
        fake.bank_register = 0x40 | 0x03;
        let mut chip = SuperIo::attach(fake, Slot::Primary, lock("8689")).unwrap();
        let dump = chip.dump().unwrap();
        assert_eq!(dump.banks.len(), 4);
        assert_eq!(dump.banks[1].registers[0x10], Some(0xEE));
        assert_eq!(chip.io.bank_register, 0x43);
        let readings = chip.read().unwrap();
        assert_eq!(readings.channels.len(), 6);
        assert_eq!(chip.io.bank_register, 0x43);
    }

    #[test]
    fn single_bank_chips_never_write_the_bank_register() {
        let mut chip = SuperIo::attach(FakeIte::chip(0x8620), Slot::Primary, lock("8620")).unwrap();
        let dump = chip.dump().unwrap();
        assert_eq!(dump.banks.len(), 1);
        chip.read().unwrap();
        assert!(data_writes(&chip.io).is_empty());
    }

    #[test]
    fn unverified_chips_refuse_manual_control_without_touching_the_chip() {
        let mut chip = probe(FakeIte::chip(0x8686), "unverified");
        chip.io.banks[0][0x15] = 0x00;
        let before = chip.io.writes.len();
        assert!(matches!(
            chip.set_duty(0, 0x80),
            Err(HwError::Unverified("ITE IT8686E"))
        ));
        chip.restore(0, None).unwrap();
        chip.restore_patiently(1, None, Duration::from_millis(50))
            .unwrap();
        assert_eq!(chip.io.writes.len(), before);
        assert_eq!(chip.io.reg(0x15), 0x00);
        assert!(matches!(
            chip.restore(5, None),
            Err(HwError::InvalidChannel(5))
        ));
    }

    #[test]
    fn the_it8689e_control_path_writes_its_manual_value_when_verified() {
        static VERIFIED_8689: crate::chips::ChipSpec = crate::chips::ChipSpec {
            name: "test IT8689E",
            verified: true,
            ident: crate::chips::Ident::Ite(0x8689),
            map: crate::chips::ChipMap::Ite(IteMap {
                fans: 6,
                temps: 6,
                pwm_control: [0x15, 0x16, 0x17, 0x7F, 0xA7, 0xAF],
                tach_low: [0x0D, 0x0E, 0x0F, 0x80, 0x82, 0x4C],
                tach_high: [0x18, 0x19, 0x1A, 0x81, 0x83, 0x4D],
                tach_enable: [None; 6],
                bank_select: false,
                dump_banks: 4,
                index_echo: true,
                config_check: true,
                manual_control: Some(0x7F),
            }),
        };
        let mut chip = probe(FakeIte::chip(0x8689), "8689-control");
        chip.config.chip = &VERIFIED_8689;
        chip.set_duty(4, 0x66).unwrap();
        assert_eq!(chip.io.reg(0xA7), 0x7F);
        assert_eq!(chip.io.reg(0xA3), 0x66);
        chip.restore(4, None).unwrap();
        assert_eq!(chip.io.reg(0xA7), 0xFF);
    }
}
