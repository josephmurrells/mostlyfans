use std::collections::BTreeMap;

use crate::HwError;
use crate::chips::{ChipMap, NctMap, Tach};
use crate::mutex::{DEFAULT_TIMEOUT, NamedMutex};
use crate::port::PortIo;
use crate::superio::{
    BankDump, CHIP_ID_REG, ChannelState, ChipConfig, ChipReadings, ConfigSpace, ControlKind,
    RegisterDump, Slot, chip_matching, with_config_space,
};

pub(crate) const MODE_MASK: u8 = 0xF0;
pub(crate) const ENTER_KEY: [u8; 2] = [0x87, 0x87];
pub(crate) const EXIT_KEY: u8 = 0xAA;
pub(crate) const HWM_LDN: u8 = 0x0B;
pub(crate) const IO_LOCK_REG: u8 = 0x28;
pub(crate) const IO_LOCK_BIT: u8 = 0x10;
pub(crate) const BANK_REG: u8 = 0x4E;
pub(crate) const VENDOR_HIGH_REG: u16 = 0x804F;
pub(crate) const VENDOR_LOW_REG: u16 = 0x004F;
pub(crate) const VENDOR_ID: u16 = 0x5CA3;
pub(crate) const MODE_REGS: [u16; 7] = [0x102, 0x202, 0x302, 0x802, 0x902, 0xA02, 0xB02];
pub(crate) const COMMAND_REGS: [u16; 7] = [0x109, 0x209, 0x309, 0x809, 0x909, 0xA09, 0xB09];
pub(crate) const ADDRESS_PORT_OFFSET: u16 = 5;
pub(crate) const DATA_PORT_OFFSET: u16 = 6;

const CHIP_REVISION_REG: u8 = 0x21;
const LDN_DUMP_REGS: [u8; 37] = [
    0x28, 0x30, 0x60, 0x61, 0x70, 0xE0, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA,
    0xEB, 0xEC, 0xED, 0xEE, 0xEF, 0xF0, 0xF1, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA,
    0xFB, 0xFC, 0xFD, 0xFE, 0xFF,
];
const DUMP_ROWS: &[u16] = &[
    0x000, 0x010, 0x020, 0x030, 0x040, 0x050, 0x060, 0x070, 0x0F0, 0x100, 0x110, 0x120, 0x130,
    0x140, 0x150, 0x200, 0x210, 0x220, 0x230, 0x240, 0x250, 0x260, 0x300, 0x320, 0x330, 0x340,
    0x360, 0x400, 0x410, 0x420, 0x440, 0x450, 0x460, 0x480, 0x490, 0x4B0, 0x4C0, 0x4F0, 0x500,
    0x550, 0x560, 0x600, 0x610, 0x620, 0x630, 0x640, 0x650, 0x660, 0x670, 0x700, 0x710, 0x720,
    0x730, 0x800, 0x820, 0x830, 0x840, 0x900, 0x920, 0x930, 0x940, 0x960, 0xA00, 0xA10, 0xA20,
    0xA30, 0xA40, 0xA50, 0xA60, 0xA70, 0xB00, 0xB10, 0xB20, 0xB30, 0xB50, 0xB60, 0xB70, 0xC00,
    0xC10, 0xC20, 0xC30, 0xC50, 0xC60, 0xC70, 0xD00, 0xD10, 0xD20, 0xD30, 0xD50, 0xD60, 0xE00,
    0xE10, 0xE20, 0xE30, 0xF00, 0xF10, 0xF20, 0xF30, 0x8040, 0x80F0,
];
const SOURCE_MASK: u8 = 0x1F;
const FULL_SPEED: u8 = 0xFF;
const MIN_COUNT: u16 = 0x15;
const MAX_COUNT: u16 = 0x1FFF;
const MIN_TEMP: f32 = -55.0;
const MAX_TEMP: f32 = 125.0;

pub fn rpm_from_count(high: u8, low: u8) -> Option<u32> {
    let count = (u16::from(high) << 5) | u16::from(low & 0x1F);
    match count {
        MAX_COUNT.. => Some(0),
        MIN_COUNT.. => Some((1_350_000.0 / f32::from(count)).round() as u32),
        _ => None,
    }
}

pub fn rpm_from_register(high: u8, low: u8, min_rpm: u16) -> u32 {
    let rpm = u16::from_be_bytes([high, low]);
    if rpm > min_rpm { u32::from(rpm) } else { 0 }
}

pub fn temperature_from_halves(halves: i16) -> Option<f32> {
    let celsius = f32::from(halves) / 2.0;
    (MIN_TEMP..=MAX_TEMP).contains(&celsius).then_some(celsius)
}

pub fn temperature_from_alternate(raw: u8) -> Option<f32> {
    let celsius = raw as i8;
    (celsius > 0 && f32::from(celsius) <= MAX_TEMP).then(|| f32::from(celsius))
}

fn exit_config<P: PortIo, T>(
    space: &mut ConfigSpace<'_, P>,
    _: &Result<T, HwError>,
) -> Result<(), HwError> {
    space.write_index(EXIT_KEY)
}

fn unlock_io_space<P: PortIo>(space: &mut ConfigSpace<'_, P>) -> Result<(), HwError> {
    let lock = space.read(IO_LOCK_REG)?;
    if lock & IO_LOCK_BIT == 0 {
        return Ok(());
    }
    tracing::info!("unlocking the Nuvoton hardware monitor I/O space");
    space.write(IO_LOCK_REG, lock & !IO_LOCK_BIT)?;
    if space.read(IO_LOCK_REG)? & IO_LOCK_BIT != 0 {
        return Err(HwError::IoSpaceLocked);
    }
    Ok(())
}

pub(crate) fn read_config<P: PortIo>(
    io: &mut P,
    slot: Slot,
    unlock: bool,
) -> Result<ChipConfig, HwError> {
    with_config_space(
        io,
        slot.config_port(),
        &ENTER_KEY,
        |space| {
            let id = space.read(CHIP_ID_REG)?;
            let revision = space.read(CHIP_REVISION_REG)?;
            let chip_id = u16::from_be_bytes([id, revision]);
            let chip = match chip_matching(|chip| chip.matches_nuvoton(id, revision)) {
                Some(chip) => chip,
                None if matches!(id, 0x00 | 0xFF) => return Err(HwError::ChipNotFound(chip_id)),
                None => return Err(HwError::UnsupportedChip(chip_id)),
            };
            let io_lock = matches!(&chip.map, ChipMap::Nuvoton(map) if map.io_lock);
            space.io.find_bars()?;
            space.select(HWM_LDN)?;
            let base_address = space.read_base_address()?;
            if io_lock && unlock {
                unlock_io_space(space)?;
            }
            Ok(ChipConfig {
                chip,
                slot,
                chip_id,
                version: revision,
                base_address,
            })
        },
        exit_config,
    )
}

struct Hwm<'a, P: PortIo> {
    io: &'a mut P,
    base: u16,
    bank: u8,
}

impl<'a, P: PortIo> Hwm<'a, P> {
    fn open(io: &'a mut P, base: u16) -> Result<Self, HwError> {
        io.write(base + ADDRESS_PORT_OFFSET, BANK_REG)?;
        let bank = io.read(base + DATA_PORT_OFFSET)?;
        Ok(Self { io, base, bank })
    }

    fn select(&mut self, bank: u8) -> Result<(), HwError> {
        if self.bank != bank {
            self.io.write(self.base + ADDRESS_PORT_OFFSET, BANK_REG)?;
            self.io.write(self.base + DATA_PORT_OFFSET, bank)?;
            self.bank = bank;
        }
        Ok(())
    }

    fn read(&mut self, address: u16) -> Result<u8, HwError> {
        let [bank, register] = address.to_be_bytes();
        self.select(bank)?;
        self.io.write(self.base + ADDRESS_PORT_OFFSET, register)?;
        self.io.read(self.base + DATA_PORT_OFFSET)
    }

    fn write(&mut self, address: u16, value: u8) -> Result<(), HwError> {
        let [bank, register] = address.to_be_bytes();
        self.select(bank)?;
        self.io.write(self.base + ADDRESS_PORT_OFFSET, register)?;
        self.io.write(self.base + DATA_PORT_OFFSET, value)
    }

    fn write_verified(&mut self, address: u16, value: u8) -> Result<(), HwError> {
        self.write(address, value)?;
        let read = self.read(address)?;
        if read == value {
            Ok(())
        } else {
            Err(HwError::VerifyFailed {
                register: address,
                written: value,
                read,
            })
        }
    }
}

pub(crate) struct Session<'a, P: PortIo> {
    hwm: Hwm<'a, P>,
    original: u8,
    map: &'static NctMap,
}

impl<'a, P: PortIo> Session<'a, P> {
    pub(crate) fn open(io: &'a mut P, base: u16, map: &'static NctMap) -> Result<Self, HwError> {
        let hwm = Hwm::open(io, base)?;
        let original = hwm.bank;
        let mut session = Self { hwm, original, map };
        if let Err(error) = session.check_vendor() {
            if let Err(restore) = session.close() {
                tracing::warn!(%restore, "could not restore the hardware monitor bank");
            }
            return Err(error);
        }
        Ok(session)
    }

    pub(crate) fn close(mut self) -> Result<(), HwError> {
        self.hwm.select(self.original)
    }

    fn check_vendor(&mut self) -> Result<(), HwError> {
        let high = self.hwm.read(VENDOR_HIGH_REG)?;
        let low = self.hwm.read(VENDOR_LOW_REG)?;
        match u16::from_be_bytes([high, low]) {
            VENDOR_ID => Ok(()),
            other => Err(HwError::VendorMismatch(other)),
        }
    }

    fn channel_state(&mut self, channel: usize) -> Result<ChannelState, HwError> {
        Ok(ChannelState {
            control: self.hwm.read(MODE_REGS[channel])?,
            duty: self.hwm.read(COMMAND_REGS[channel])?,
            main_enabled: true,
            kind: ControlKind::Nuvoton,
        })
    }

    fn rpm(&mut self, channel: usize) -> Result<Option<u32>, HwError> {
        match &self.map.tach {
            Tach::Rpm { registers, min_rpm } => {
                let high = self.hwm.read(registers[channel])?;
                let low = self.hwm.read(registers[channel] + 1)?;
                Ok(Some(rpm_from_register(high, low, *min_rpm)))
            }
            Tach::Count { registers } => {
                let high = self.hwm.read(registers[channel])?;
                let low = self.hwm.read(registers[channel] + 1)?;
                Ok(rpm_from_count(high, low))
            }
        }
    }

    fn temperatures(&mut self) -> Result<Vec<Option<f32>>, HwError> {
        let sources = self.map.sources;
        let mut temps = vec![None; sources.len()];
        for slot in self.map.slots {
            let source = self.hwm.read(slot.source)? & SOURCE_MASK;
            let Some(index) = sources.iter().position(|known| known.id == source) else {
                continue;
            };
            if temps[index].is_some() {
                continue;
            }
            let mut halves = i16::from(self.hwm.read(slot.register)? as i8) * 2;
            if let Some((register, bit)) = slot.half {
                halves |= i16::from((self.hwm.read(register)? >> bit) & 1);
            }
            temps[index] = temperature_from_halves(halves);
        }
        for (temp, source) in temps.iter_mut().zip(sources) {
            if temp.is_none()
                && let Some(alternate) = source.alternate
            {
                *temp = temperature_from_alternate(self.hwm.read(alternate)?);
            }
        }
        Ok(temps)
    }
}

impl<P: PortIo> crate::superio::Session for Session<'_, P> {
    fn verify_vendor(&mut self) -> Result<(), HwError> {
        self.check_vendor()
    }

    fn read(&mut self) -> Result<ChipReadings, HwError> {
        let mut readings = ChipReadings::default();
        for channel in 0..self.map.fans {
            readings.channels.push(self.channel_state(channel)?);
            readings.rpm.push(self.rpm(channel)?);
        }
        readings.temps = self.temperatures()?;
        Ok(readings)
    }

    fn channel_states(&mut self) -> Result<Vec<ChannelState>, HwError> {
        (0..self.map.fans)
            .map(|channel| self.channel_state(channel))
            .collect()
    }

    fn set_duty(&mut self, channel: usize, duty: u8) -> Result<(), HwError> {
        let mode = self.hwm.read(MODE_REGS[channel])?;
        if mode & MODE_MASK != 0 {
            self.hwm
                .write_verified(MODE_REGS[channel], mode & !MODE_MASK)?;
        }
        self.hwm.write_verified(COMMAND_REGS[channel], duty)
    }

    fn restore(&mut self, channel: usize, saved: Option<ChannelState>) -> Result<(), HwError> {
        match saved.filter(|state| state.kind == ControlKind::Nuvoton) {
            Some(state) => {
                self.hwm.write(COMMAND_REGS[channel], state.duty)?;
                self.hwm.write_verified(MODE_REGS[channel], state.control)
            }
            None => {
                let mode = self.hwm.read(MODE_REGS[channel])?;
                if mode & MODE_MASK == 0 {
                    self.hwm.write_verified(COMMAND_REGS[channel], FULL_SPEED)?;
                }
                Ok(())
            }
        }
    }
}

pub(crate) fn dump<P: PortIo>(
    io: &mut P,
    lock: &NamedMutex,
    config: &ChipConfig,
) -> Result<RegisterDump, HwError> {
    let (global_config, ec_ldn_config) = {
        let _guard = lock.acquire(DEFAULT_TIMEOUT)?;
        with_config_space(
            io,
            config.slot.config_port(),
            &ENTER_KEY,
            |space| space.dump(HWM_LDN, &LDN_DUMP_REGS),
            exit_config,
        )?
    };

    let mut banks: BTreeMap<u8, Vec<Option<u8>>> = BTreeMap::new();
    let mut bank_register = None;
    for &row in DUMP_ROWS {
        let _guard = lock.acquire(DEFAULT_TIMEOUT)?;
        let mut hwm = Hwm::open(&mut *io, config.base_address)?;
        let original = hwm.bank;
        bank_register.get_or_insert(original);
        let values = (0..16u16)
            .map(|offset| hwm.read(row | offset))
            .collect::<Result<Vec<_>, _>>();
        hwm.select(original)?;
        let [bank, start] = row.to_be_bytes();
        let registers = banks.entry(bank).or_insert_with(|| vec![None; 256]);
        for (offset, value) in values?.into_iter().enumerate() {
            registers[usize::from(start) + offset] = Some(value);
        }
    }

    Ok(RegisterDump {
        chip: config.chip.name.to_owned(),
        chip_id: config.chip_id,
        chip_version: config.version,
        base_address: config.base_address,
        global_config,
        ldn: HWM_LDN,
        ec_ldn_config,
        bank_register: bank_register.unwrap_or_default(),
        banks: banks
            .into_iter()
            .map(|(bank, registers)| BankDump { bank, registers })
            .collect(),
    })
}

#[cfg(any(test, feature = "test-support"))]
pub mod fake {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex, PoisonError};

    use super::*;

    pub const RPM_1200: (u8, u8) = (0x23, 0x05);
    pub const OUTSIDE_CONFIG_MODE: u8 = 0xFF;

    #[derive(Debug, Clone)]
    pub struct FakeNct {
        pub id: u8,
        pub revision: u8,
        pub port: u16,
        pub base: u16,
        pub config_mode: bool,
        pub enter_progress: usize,
        pub config_index: u8,
        pub ldn: u8,
        pub bars_found: bool,
        pub lock_register: u8,
        pub hwm_index: u8,
        pub bank_register: u8,
        pub banks: BTreeMap<u8, [u8; 256]>,
        pub locked: bool,
        pub lock_stuck: bool,
        pub stuck: Vec<u16>,
        pub exits: usize,
        pub writes: Vec<(u16, u8, u8)>,
    }

    impl FakeNct {
        pub fn new(id: u8, revision: u8) -> Self {
            let mut fake = Self {
                id,
                revision,
                port: 0x2E,
                base: 0x290,
                config_mode: false,
                enter_progress: 0,
                config_index: 0,
                ldn: 0,
                bars_found: false,
                lock_register: 0,
                hwm_index: 0,
                bank_register: 0,
                banks: BTreeMap::new(),
                locked: false,
                lock_stuck: false,
                stuck: Vec::new(),
                exits: 0,
                writes: Vec::new(),
            };
            fake.set(VENDOR_HIGH_REG, 0x5C);
            fake.set(VENDOR_LOW_REG, 0xA3);
            for channel in 0..7 {
                fake.set(MODE_REGS[channel], 0x50);
                fake.set(COMMAND_REGS[channel], 0x80);
            }
            for register in [0x4B0, 0x4B2, 0x4B4, 0x4B6, 0x4B8, 0x4BA, 0x4CC] {
                fake.set(register, RPM_1200.0);
                fake.set(register + 1, RPM_1200.1);
            }
            fake.set(0x100, 2);
            fake.set(0x073, 45);
            fake.set(0x074, 0x80);
            fake.set(0x200, 1);
            fake.set(0x075, 30);
            fake.set(0x300, 2);
            fake.set(0x077, 99);
            fake.set(0x492, 0x7F);
            fake.set(0x493, 38);
            fake
        }

        pub fn get(&self, address: u16) -> u8 {
            let [bank, register] = address.to_be_bytes();
            self.banks
                .get(&bank)
                .map_or(0, |values| values[usize::from(register)])
        }

        pub fn set(&mut self, address: u16, value: u8) {
            let [bank, register] = address.to_be_bytes();
            self.banks.entry(bank).or_insert([0; 256])[usize::from(register)] = value;
        }

        pub fn data_writes(&self) -> Vec<(u8, u8)> {
            self.writes
                .iter()
                .filter(|(port, ..)| *port == self.base + DATA_PORT_OFFSET)
                .map(|&(_, value, index)| (index, value))
                .collect()
        }

        fn is_hwm_port(&self, port: u16) -> bool {
            port == self.base + ADDRESS_PORT_OFFSET || port == self.base + DATA_PORT_OFFSET
        }
    }

    impl PortIo for FakeNct {
        fn read(&mut self, port: u16) -> Result<u8, HwError> {
            if self.is_hwm_port(port) && !self.bars_found {
                return Err(HwError::AccessDenied);
            }
            Ok(match port {
                p if p == self.port + 1 && self.config_mode => match self.config_index {
                    0x07 => self.ldn,
                    0x20 => self.id,
                    0x21 => self.revision,
                    IO_LOCK_REG if self.ldn == HWM_LDN => self.lock_register,
                    0x60 if self.ldn == HWM_LDN => (self.base >> 8) as u8,
                    0x61 if self.ldn == HWM_LDN => self.base as u8,
                    _ => 0,
                },
                p if p == self.base + ADDRESS_PORT_OFFSET => self.hwm_index,
                p if p == self.base + DATA_PORT_OFFSET => {
                    if self.locked {
                        0xFF
                    } else if self.hwm_index == BANK_REG {
                        self.bank_register
                    } else {
                        self.get(u16::from_be_bytes([self.bank_register, self.hwm_index]))
                    }
                }
                _ => 0xFF,
            })
        }

        fn write(&mut self, port: u16, value: u8) -> Result<(), HwError> {
            if self.is_hwm_port(port) && !self.bars_found {
                return Err(HwError::AccessDenied);
            }
            let index = if port == self.port + 1 && !self.config_mode {
                OUTSIDE_CONFIG_MODE
            } else if port == self.port + 1 {
                self.config_index
            } else {
                self.hwm_index
            };
            self.writes.push((port, value, index));
            match port {
                p if p == self.port && self.config_mode => {
                    self.config_index = value;
                    if value == EXIT_KEY {
                        self.config_mode = false;
                        self.exits += 1;
                    }
                }
                p if p == self.port => {
                    if value == ENTER_KEY[0] {
                        self.enter_progress += 1;
                    } else {
                        self.enter_progress = 0;
                    }
                    if self.enter_progress == ENTER_KEY.len() {
                        self.config_mode = true;
                        self.enter_progress = 0;
                    }
                }
                p if p == self.port + 1 && self.config_mode && self.config_index == 0x07 => {
                    self.ldn = value;
                }
                p if p == self.port + 1
                    && self.config_mode
                    && self.config_index == IO_LOCK_REG
                    && self.ldn == HWM_LDN
                    && !self.lock_stuck =>
                {
                    self.lock_register = value;
                    self.locked = value & IO_LOCK_BIT != 0;
                }
                p if p == self.base + ADDRESS_PORT_OFFSET => self.hwm_index = value,
                p if p == self.base + DATA_PORT_OFFSET && !self.locked => {
                    if self.hwm_index == BANK_REG {
                        self.bank_register = value;
                    } else {
                        let address = u16::from_be_bytes([self.bank_register, self.hwm_index]);
                        if !self.stuck.contains(&address) {
                            self.set(address, value);
                        }
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

    #[derive(Debug, Clone)]
    pub struct SharedNct(Arc<Mutex<FakeNct>>);

    impl SharedNct {
        pub fn new(fake: FakeNct) -> Self {
            Self(Arc::new(Mutex::new(fake)))
        }

        pub fn with<R>(&self, action: impl FnOnce(&mut FakeNct) -> R) -> R {
            action(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner))
        }
    }

    impl PortIo for SharedNct {
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
    use std::cell::Cell;

    use super::fake::FakeNct;
    use super::*;
    use crate::chips::{ChipSpec, Ident};
    use crate::superio::{SuperIo, detect};

    fn lock(tag: &str) -> NamedMutex {
        NamedMutex::open(&format!(r"Local\fanforge-nct-{tag}-{}", std::process::id())).unwrap()
    }

    fn probe(fake: FakeNct, tag: &str) -> SuperIo<FakeNct> {
        SuperIo::probe(fake, Slot::Primary, lock(tag)).unwrap()
    }

    fn nct6798d() -> FakeNct {
        FakeNct::new(0xD4, 0x2B)
    }

    #[test]
    fn the_nct6798d_is_found_after_the_ite_probe_and_left_out_of_config_mode() {
        let chip = probe(nct6798d(), "detect");
        let config = chip.config();
        assert_eq!(config.chip.name, "Nuvoton NCT6798D");
        assert_eq!(config.chip_id, 0xD42B);
        assert_eq!(config.version, 0x2B);
        assert_eq!(config.slot, Slot::Primary);
        assert_eq!(config.base_address, 0x290);
        assert!(!chip.fan_control());
        assert_eq!(chip.fan_count(), 7);
        assert!(!chip.io.config_mode);
        assert_eq!(chip.io.exits, 1);
        assert_eq!(chip.io.ldn, HWM_LDN);
    }

    #[test]
    fn readings_decode_tach_counts_and_routed_temperatures() {
        let mut fake = nct6798d();
        fake.bank_register = 0x03;
        let mut chip = probe(fake, "read");
        let readings = chip.read().unwrap();
        assert_eq!(readings.rpm, [Some(1200); 7]);
        assert!(readings.channels.iter().all(ChannelState::is_auto));
        assert_eq!(readings.channels[0].kind, ControlKind::Nuvoton);
        assert_eq!(readings.channels[0].duty, 0x80);
        assert_eq!(
            readings.temps,
            vec![
                Some(30.0),
                Some(45.5),
                None,
                Some(38.0),
                None,
                None,
                None,
                None,
                None
            ]
        );
        assert_eq!(
            chip.temp_names(),
            [
                "SYSTIN", "CPUTIN", "AUXTIN0", "AUXTIN1", "AUXTIN2", "AUXTIN3", "AUXTIN4",
                "TSENSOR", "PECI 0"
            ]
            .map(Some)
        );
        assert_eq!(chip.io.bank_register, 0x03);
        assert!(
            chip.io
                .data_writes()
                .iter()
                .all(|&(index, _)| index == BANK_REG)
        );
    }

    #[test]
    fn the_nct6776f_reads_rpm_registers_and_half_bit_zero() {
        let mut fake = FakeNct::new(0xC3, 0x33);
        fake.set(0x656, 0x04);
        fake.set(0x657, 0xB0);
        fake.set(0x658, 0x00);
        fake.set(0x659, 100);
        fake.set(0x65A, 0x00);
        fake.set(0x65B, 0x00);
        fake.set(0x624, 3);
        fake.set(0x62B, 20);
        fake.set(0x62E, 0b001);
        let mut chip = probe(fake, "6776");
        assert_eq!(chip.chip().name, "Nuvoton NCT6776F");
        let readings = chip.read().unwrap();
        assert_eq!(readings.rpm, [Some(1200), Some(0), Some(0)]);
        assert_eq!(
            readings.temps,
            vec![Some(30.0), Some(45.5), Some(20.5), None]
        );
    }

    #[test]
    fn decoders_follow_the_documented_ranges() {
        assert_eq!(rpm_from_count(0x23, 0x05), Some(1200));
        assert_eq!(rpm_from_count(0xFF, 0x1F), Some(0));
        assert_eq!(rpm_from_count(0x00, 0x14), None);
        assert_eq!(rpm_from_count(0x00, 0x15), Some(64286));
        assert_eq!(rpm_from_count(0x23, 0xE5), Some(1200));
        assert_eq!(rpm_from_register(0x04, 0xB0, 164), 1200);
        assert_eq!(rpm_from_register(0x00, 164, 164), 0);
        assert_eq!(rpm_from_register(0x00, 21, 20), 21);
        assert_eq!(temperature_from_halves(91), Some(45.5));
        assert_eq!(temperature_from_halves(-110), Some(-55.0));
        assert_eq!(temperature_from_halves(-112), None);
        assert_eq!(temperature_from_halves(251), None);
        assert_eq!(temperature_from_halves(-1), Some(-0.5));
        assert_eq!(temperature_from_alternate(0), None);
        assert_eq!(temperature_from_alternate(125), Some(125.0));
        assert_eq!(temperature_from_alternate(126), None);
        assert_eq!(temperature_from_alternate(0xFF), None);
    }

    #[test]
    fn a_locked_monitor_is_unlocked_on_679x_chips_only() {
        let mut fake = nct6798d();
        fake.lock_register = IO_LOCK_BIT | 0x01;
        fake.locked = true;
        let mut chip = probe(fake, "locked");
        assert_eq!(chip.io.lock_register, 0x01);
        assert!(chip.read().is_ok());
        assert_eq!(chip.io.exits, 1);

        let mut fake = FakeNct::new(0xC5, 0x62);
        fake.lock_register = IO_LOCK_BIT;
        let chip = probe(fake, "6779-cr28");
        assert_eq!(chip.chip().name, "Nuvoton NCT6779D");
        assert_eq!(chip.io.lock_register, IO_LOCK_BIT);
    }

    #[test]
    fn a_lock_that_does_not_clear_is_reported_and_config_mode_is_left() {
        let mut fake = nct6798d();
        fake.lock_register = IO_LOCK_BIT;
        fake.lock_stuck = true;
        assert!(matches!(
            read_config(&mut fake, Slot::Primary, true),
            Err(HwError::IoSpaceLocked)
        ));
        assert!(!fake.config_mode);
        assert_eq!(fake.exits, 1);
    }

    #[test]
    fn an_unlocked_chip_never_gets_a_lock_write() {
        let chip = probe(nct6798d(), "no-lock-write");
        assert!(
            chip.io
                .writes
                .iter()
                .all(|&(port, _, index)| port != chip.io.port + 1 || index != IO_LOCK_REG)
        );
    }

    #[test]
    fn attaching_read_only_dumps_a_locked_chip_without_clearing_the_lock() {
        let mut fake = nct6798d();
        fake.lock_register = IO_LOCK_BIT;
        fake.locked = true;
        let mut chip = SuperIo::attach(fake, Slot::Primary, lock("attach-locked")).unwrap();
        let dump = chip.dump().unwrap();
        assert_eq!(dump.ec_ldn_config["0x28"], IO_LOCK_BIT);
        chip.reinitialize().unwrap_err();
        assert_eq!(chip.io.lock_register, IO_LOCK_BIT);
        assert!(!chip.io.config_mode);
        assert!(
            chip.io
                .writes
                .iter()
                .all(|&(port, _, index)| port != chip.io.port + 1 || index != IO_LOCK_REG)
        );
    }

    #[test]
    fn a_lock_set_after_probing_fails_reads_and_is_cleared_on_reinitialisation() {
        let mut chip = probe(nct6798d(), "relock");
        chip.io.locked = true;
        chip.io.lock_register = IO_LOCK_BIT;
        assert!(matches!(chip.read(), Err(HwError::VendorMismatch(0xFFFF))));
        chip.reinitialize().unwrap();
        assert_eq!(chip.io.lock_register, 0);
        assert!(chip.read().is_ok());
    }

    #[test]
    fn a_wrong_vendor_is_reported() {
        let mut fake = FakeNct::new(0xC5, 0x62);
        fake.set(VENDOR_LOW_REG, 0x00);
        assert!(matches!(
            SuperIo::probe(fake, Slot::Primary, lock("vendor")),
            Err(HwError::VendorMismatch(0x5C00))
        ));
    }

    #[test]
    fn unknown_and_absent_chips_are_told_apart() {
        let mut io = FakeNct::new(0xD4, 0x99);
        assert!(matches!(
            read_config(&mut io, Slot::Primary, true),
            Err(HwError::UnsupportedChip(0xD499))
        ));
        assert!(!io.config_mode);
        let mut io = FakeNct::new(0xFF, 0xFF);
        assert!(matches!(
            read_config(&mut io, Slot::Primary, true),
            Err(HwError::ChipNotFound(0xFFFF))
        ));
    }

    #[test]
    fn a_chip_already_in_config_mode_is_found_without_the_ite_exit() {
        let mut fake = nct6798d();
        fake.config_mode = true;
        let chip = probe(fake, "entered");
        assert_eq!(chip.chip().name, "Nuvoton NCT6798D");
        assert!(
            chip.io
                .writes
                .iter()
                .all(|&(port, _, index)| port != 0x2F || index == 0x07)
        );
    }

    #[test]
    fn the_secondary_slot_is_tried_only_when_nothing_answers_on_the_primary() {
        let mut fake = nct6798d();
        fake.port = 0x4E;
        let opened = Cell::new(Vec::new());
        let chip = detect(
            |slot| {
                let mut slots = opened.take();
                slots.push(slot);
                opened.set(slots);
                Ok(fake.clone())
            },
            || Ok(lock("secondary")),
            true,
        )
        .unwrap();
        assert_eq!(chip.config().slot, Slot::Secondary);
        assert_eq!(opened.take(), vec![Slot::Primary, Slot::Secondary]);

        let opened = Cell::new(0);
        let result = detect(
            |_| {
                opened.set(opened.get() + 1);
                Ok(FakeNct::new(0xD4, 0x99))
            },
            || Ok(lock("primary-unknown")),
            true,
        );
        assert!(matches!(result, Err(HwError::UnsupportedChip(0xD499))));
        assert_eq!(opened.get(), 1);
    }

    #[test]
    fn dump_reads_the_documented_rows_and_restores_the_bank() {
        let mut fake = nct6798d();
        fake.bank_register = 0x05;
        let mut chip = SuperIo::attach(fake, Slot::Primary, lock("dump")).unwrap();
        let dump = chip.dump().unwrap();
        assert_eq!(dump.chip, "Nuvoton NCT6798D");
        assert_eq!(dump.ldn, HWM_LDN);
        assert_eq!(dump.bank_register, 0x05);
        assert_eq!(chip.io.bank_register, 0x05);
        assert_eq!(dump.ec_ldn_config["0x61"], 0x90);
        let bank = |number: u8| dump.banks.iter().find(|bank| bank.bank == number).unwrap();
        assert_eq!(bank(0x01).registers[0x09], Some(0x80));
        assert_eq!(bank(0x80).registers[0x4F], Some(0x5C));
        assert_eq!(bank(0x01).registers[0x60], None);
        assert_eq!(dump.banks.last().unwrap().bank, 0x80);
        let mut changed = dump.clone();
        changed.banks[0].registers[0x4F] = Some(0x00);
        assert_eq!(dump.diff(&changed), vec!["bank0 0x4f: 0xa3 -> 0x00"]);
    }

    #[test]
    fn unverified_nuvoton_chips_never_write_fan_registers() {
        let mut chip = probe(nct6798d(), "gate");
        chip.io.set(MODE_REGS[0], 0x00);
        assert!(matches!(
            chip.set_duty(0, 0x20),
            Err(HwError::Unverified(_))
        ));
        chip.restore(0, None).unwrap();
        assert_eq!(chip.io.get(COMMAND_REGS[0]), 0x80);
        assert!(
            chip.io
                .data_writes()
                .iter()
                .all(|&(index, _)| index == BANK_REG)
        );
    }

    static VERIFIED_6798: ChipSpec = ChipSpec {
        name: "test NCT6798D",
        verified: true,
        ident: Ident::Nuvoton {
            id: 0xD4,
            revision: 0x2B,
            mask: 0xFF,
        },
        map: ChipMap::Nuvoton(NctMap {
            fans: 7,
            tach: Tach::Count {
                registers: &[0x4B0, 0x4B2, 0x4B4, 0x4B6, 0x4B8, 0x4BA, 0x4CC],
            },
            slots: &[],
            sources: &[],
            io_lock: true,
        }),
    };

    #[test]
    fn the_gated_control_path_switches_modes_and_restores_them() {
        let mut chip = probe(nct6798d(), "control");
        chip.config.chip = &VERIFIED_6798;
        chip.io.set(MODE_REGS[1], 0x53);
        let saved = chip.channel_states().unwrap()[1];
        chip.set_duty(1, 0x40).unwrap();
        assert_eq!(chip.io.get(MODE_REGS[1]), 0x03);
        assert_eq!(chip.io.get(COMMAND_REGS[1]), 0x40);
        assert!(!chip.channel_states().unwrap()[1].is_auto());

        chip.restore(1, Some(saved)).unwrap();
        assert_eq!(chip.io.get(MODE_REGS[1]), 0x53);
        assert_eq!(chip.io.get(COMMAND_REGS[1]), 0x80);

        chip.set_duty(2, 0x30).unwrap();
        chip.restore(2, None).unwrap();
        assert_eq!(chip.io.get(MODE_REGS[2]), 0x00);
        assert_eq!(chip.io.get(COMMAND_REGS[2]), 0xFF);

        chip.restore(3, None).unwrap();
        assert_eq!(chip.io.get(MODE_REGS[3]), 0x50);
        assert_eq!(chip.io.get(COMMAND_REGS[3]), 0x80);

        let ite_state = ChannelState {
            control: 0x80,
            duty: 0x10,
            main_enabled: true,
            kind: ControlKind::Ite,
        };
        chip.set_duty(4, 0x30).unwrap();
        chip.restore(4, Some(ite_state)).unwrap();
        assert_eq!(chip.io.get(COMMAND_REGS[4]), 0xFF);
        assert_eq!(chip.io.bank_register, 0x00);
    }

    #[test]
    fn writes_that_do_not_stick_are_reported() {
        let mut fake = nct6798d();
        fake.stuck.push(COMMAND_REGS[0]);
        let mut chip = probe(fake, "stuck");
        chip.config.chip = &VERIFIED_6798;
        assert!(matches!(
            chip.set_duty(0, 0x20),
            Err(HwError::VerifyFailed {
                register: 0x109,
                ..
            })
        ));
    }

    #[test]
    fn a_vendor_failure_on_open_still_restores_the_bank() {
        let mut fake = nct6798d();
        fake.bank_register = 0x02;
        let mut chip = probe(fake, "open-fail");
        chip.io.set(VENDOR_LOW_REG, 0x00);
        let map = match &chip.chip().map {
            ChipMap::Nuvoton(map) => map,
            ChipMap::Ite(_) => unreachable!(),
        };
        let result = Session::open(&mut chip.io, 0x290, map);
        assert!(matches!(
            result.map(|_| ()),
            Err(HwError::VendorMismatch(0x5C00))
        ));
        assert_eq!(chip.io.bank_register, 0x02);
    }
}
