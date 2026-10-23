use std::thread;
use std::time::{Duration, Instant};

use crate::HwError;
use crate::board::detect_identity;
use crate::chips::{ChipSpec, EcMap, NCT6687D_MSI_800};
use crate::mutex::{BusLock, DEFAULT_TIMEOUT};
use crate::port::PortIo;
use crate::superio::{BankDump, ChannelState, ChipConfig, ChipReadings, ControlKind, RegisterDump};

const PAGE_PORT_OFFSET: u16 = 4;
const INDEX_PORT_OFFSET: u16 = 5;
const DATA_PORT_OFFSET: u16 = 6;
const PAGE_SELECT: u8 = 0xFF;
const TEMP_BASE: u16 = 0x100;
const RPM_BASE: u16 = 0x140;
const MODE_REG: u16 = 0xA00;
const REQUEST_REG: u16 = 0xA01;
const COMMAND_BASE: u16 = 0xA28;
const ENGINE_STATUS_REG: u16 = 0xCF8;
const MONITOR_CONFIG_REG: u16 = 0x180;
const MONITOR_START: u8 = 0x80;
const CONFIG_REQUEST: u8 = 0x80;
const PHASE: u8 = 1 << 3;
const INVALID: u8 = 1 << 4;
const CHECK_DONE: u8 = 1 << 5;
const LOCKED: u8 = 1 << 6;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(250);
const POLL_INTERVAL: Duration = Duration::from_millis(1);
const FULL_SPEED: u8 = 0xFF;
const DUMP_PAGES: [u8; 3] = [0x01, 0x0A, 0x0C];
const MSI_VENDORS: [&str; 2] = ["Micro-Star", "MSI"];
const MSI_800_SERIES: [&str; 5] = ["B840", "B850", "B860", "X870", "Z890"];

pub fn is_msi_800_series(manufacturer: &str, product: &str) -> bool {
    let product = product.to_ascii_uppercase();
    MSI_VENDORS
        .iter()
        .any(|vendor| manufacturer.contains(vendor))
        && MSI_800_SERIES.iter().any(|series| product.contains(series))
}

pub(crate) fn board_variant(chip: &'static ChipSpec) -> &'static ChipSpec {
    if chip.ident != NCT6687D_MSI_800.ident {
        return chip;
    }
    let identity = detect_identity();
    if is_msi_800_series(&identity.manufacturer, &identity.product) {
        &NCT6687D_MSI_800
    } else {
        chip
    }
}

pub fn decode_temperature(value: u8, half: u8) -> Option<f32> {
    let celsius = f32::from(value as i8) + if half & 0x80 != 0 { 0.5 } else { 0.0 };
    (-55.0..=125.0).contains(&celsius).then_some(celsius)
}

struct Ec<'a, P: PortIo> {
    io: &'a mut P,
    base: u16,
}

impl<P: PortIo> Ec<'_, P> {
    fn select(&mut self, address: u16) -> Result<(), HwError> {
        let [page, index] = address.to_be_bytes();
        self.io.write(self.base + PAGE_PORT_OFFSET, PAGE_SELECT)?;
        self.io.write(self.base + PAGE_PORT_OFFSET, page)?;
        self.io.write(self.base + INDEX_PORT_OFFSET, index)
    }

    fn release(&mut self) -> Result<(), HwError> {
        self.io.write(self.base + PAGE_PORT_OFFSET, PAGE_SELECT)
    }

    fn read(&mut self, address: u16) -> Result<u8, HwError> {
        self.select(address)?;
        let value = self.io.read(self.base + DATA_PORT_OFFSET)?;
        self.release()?;
        Ok(value)
    }

    fn write(&mut self, address: u16, value: u8) -> Result<(), HwError> {
        self.select(address)?;
        self.io.write(self.base + DATA_PORT_OFFSET, value)?;
        self.release()
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

    fn wait(
        &mut self,
        step: &'static str,
        done: impl Fn(&mut Self) -> Result<bool, HwError>,
    ) -> Result<(), HwError> {
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        loop {
            if done(self)? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(HwError::FanConfigTimeout(step));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn start_config(&mut self) -> Result<(), HwError> {
        let unlocked = |status: u8| status & LOCKED == 0 && status & PHASE != 0;
        if unlocked(self.read(ENGINE_STATUS_REG)?) {
            return Ok(());
        }
        self.wait("waiting for the previous update", |ec| {
            Ok(ec.read(ENGINE_STATUS_REG)? & PHASE == 0
                && ec.read(REQUEST_REG)? & CONFIG_REQUEST == 0)
        })?;
        self.write(REQUEST_REG, CONFIG_REQUEST)?;
        self.wait("entering the configuration phase", |ec| {
            Ok(unlocked(ec.read(ENGINE_STATUS_REG)?))
        })
    }

    fn finish_config(&mut self, done: u8) -> Result<(), HwError> {
        self.write(REQUEST_REG, done)?;
        self.wait("waiting for the new configuration", |ec| {
            Ok(ec.read(ENGINE_STATUS_REG)? & CHECK_DONE != 0)
        })?;
        let status = self.read(ENGINE_STATUS_REG)?;
        if status & INVALID != 0 {
            return Err(HwError::FanConfigRejected);
        }
        if status & LOCKED == 0 {
            return Err(HwError::FanConfigTimeout("locking the fan registers"));
        }
        Ok(())
    }

    fn configure(
        &mut self,
        done: u8,
        change: impl FnOnce(&mut Self) -> Result<(), HwError>,
    ) -> Result<(), HwError> {
        self.start_config()?;
        let result = change(self);
        let finished = self.finish_config(done);
        result.and(finished)
    }
}

pub(crate) struct Session<'a, P: PortIo> {
    ec: Ec<'a, P>,
    map: &'static EcMap,
}

impl<'a, P: PortIo> Session<'a, P> {
    pub(crate) fn open(io: &'a mut P, base: u16, map: &'static EcMap) -> Self {
        Self {
            ec: Ec { io, base },
            map,
        }
    }

    fn channel_state(&mut self, channel: usize, mode: u8) -> Result<ChannelState, HwError> {
        Ok(ChannelState {
            control: u8::from(mode & (1 << channel) != 0),
            duty: self.ec.read(COMMAND_BASE + channel as u16)?,
            main_enabled: true,
            kind: ControlKind::Ec,
        })
    }

    fn set_mode_bit(&mut self, channel: usize, manual: bool) -> Result<(), HwError> {
        let mode = self.ec.read(MODE_REG)?;
        let bit = 1 << channel;
        let wanted = if manual { mode | bit } else { mode & !bit };
        if wanted != mode {
            self.ec.write_verified(MODE_REG, wanted)?;
        }
        Ok(())
    }
}

impl<P: PortIo> crate::superio::Session for Session<'_, P> {
    fn verify_vendor(&mut self) -> Result<(), HwError> {
        let config = self.ec.read(MONITOR_CONFIG_REG)?;
        if config & MONITOR_START == 0 {
            self.ec.write(MONITOR_CONFIG_REG, config | MONITOR_START)?;
        }
        Ok(())
    }

    fn read(&mut self) -> Result<ChipReadings, HwError> {
        let mut readings = ChipReadings::default();
        let mode = self.ec.read(MODE_REG)?;
        for channel in 0..self.map.fans {
            readings.channels.push(self.channel_state(channel, mode)?);
            let register = RPM_BASE + 2 * channel as u16;
            let high = self.ec.read(register)?;
            let low = self.ec.read(register + 1)?;
            readings
                .rpm
                .push(Some(u32::from(u16::from_be_bytes([high, low]))));
        }
        for index in 0..self.map.temps.len() {
            let register = TEMP_BASE + 2 * index as u16;
            let value = self.ec.read(register)?;
            let half = self.ec.read(register + 1)?;
            readings.temps.push(decode_temperature(value, half));
        }
        Ok(readings)
    }

    fn channel_states(&mut self) -> Result<Vec<ChannelState>, HwError> {
        let mode = self.ec.read(MODE_REG)?;
        (0..self.map.fans)
            .map(|channel| self.channel_state(channel, mode))
            .collect()
    }

    fn set_duty(&mut self, channel: usize, duty: u8) -> Result<(), HwError> {
        let done = self.map.done_command;
        let register = COMMAND_BASE + channel as u16;
        let bit = 1 << channel;
        self.ec.configure(done, |ec| {
            let mode = ec.read(MODE_REG)?;
            if mode & bit == 0 {
                ec.write_verified(MODE_REG, mode | bit)?;
            }
            ec.write_verified(register, duty)
        })?;
        if self.ec.read(MODE_REG)? & bit == 0 {
            return Err(HwError::FanConfigRejected);
        }
        Ok(())
    }

    fn restore(&mut self, channel: usize, saved: Option<ChannelState>) -> Result<(), HwError> {
        let done = self.map.done_command;
        let register = COMMAND_BASE + channel as u16;
        let manual = self.ec.read(MODE_REG)? & (1 << channel) != 0;
        match saved.filter(|state| state.kind == ControlKind::Ec) {
            Some(state) => {
                let duty = self
                    .ec
                    .configure(done, |ec| ec.write_verified(register, state.duty));
                let mode = self.set_mode_bit(channel, state.control != 0);
                duty.and(mode)
            }
            None if manual => self
                .ec
                .configure(done, |ec| ec.write_verified(register, FULL_SPEED)),
            None => Ok(()),
        }
    }
}

pub(crate) fn dump<P: PortIo>(
    io: &mut P,
    lock: &BusLock,
    config: &ChipConfig,
) -> Result<RegisterDump, HwError> {
    let (global_config, ec_ldn_config) = crate::nuvoton::ldn_config(io, lock, config)?;
    let mut banks = Vec::with_capacity(DUMP_PAGES.len());
    for page in DUMP_PAGES {
        let _guard = lock.acquire(DEFAULT_TIMEOUT)?;
        let mut ec = Ec {
            io: &mut *io,
            base: config.base_address,
        };
        let registers = (0..=u8::MAX)
            .map(|index| ec.read(u16::from_be_bytes([page, index])).map(Some))
            .collect::<Result<Vec<_>, _>>()?;
        banks.push(BankDump {
            bank: page,
            registers,
        });
    }
    Ok(RegisterDump {
        chip: config.chip.name.to_owned(),
        chip_id: config.chip_id,
        chip_version: config.version,
        base_address: config.base_address,
        global_config,
        ldn: crate::nuvoton::HWM_LDN,
        ec_ldn_config,
        bank_register: None,
        banks,
    })
}

#[cfg(test)]
mod tests {
    use crate::mutex::NamedMutex;
    use std::collections::BTreeMap;

    use super::*;
    use crate::chips::CHIPS;
    use crate::nuvoton::fake::FakeNct;
    use crate::superio::{Slot, SuperIo};

    struct FakeEc {
        nct: FakeNct,
        page: u8,
        index: u8,
        memory: BTreeMap<u16, u8>,
        status: u8,
        refuse_phase: bool,
        writes: Vec<(u16, u8)>,
    }

    impl FakeEc {
        fn new(id: u8, revision: u8) -> Self {
            let mut memory = BTreeMap::new();
            for channel in 0..8u16 {
                memory.insert(RPM_BASE + 2 * channel, 0x04);
                memory.insert(RPM_BASE + 2 * channel + 1, 0xB0);
                memory.insert(COMMAND_BASE + channel, 0x60);
            }
            memory.insert(TEMP_BASE, 45);
            memory.insert(TEMP_BASE + 1, 0x80);
            memory.insert(TEMP_BASE + 2, 0xF6);
            memory.insert(ENGINE_STATUS_REG, LOCKED);
            Self {
                nct: FakeNct::new(id, revision),
                page: PAGE_SELECT,
                index: 0,
                memory,
                status: LOCKED,
                refuse_phase: false,
                writes: Vec::new(),
            }
        }

        fn get(&self, address: u16) -> u8 {
            match address {
                ENGINE_STATUS_REG => self.status,
                _ => self.memory.get(&address).copied().unwrap_or(0),
            }
        }

        fn address(&self) -> u16 {
            u16::from_be_bytes([self.page, self.index])
        }

        fn store(&mut self, address: u16, value: u8) {
            self.writes.push((address, value));
            match (address, value) {
                (REQUEST_REG, CONFIG_REQUEST) if !self.refuse_phase => self.status = PHASE,
                (REQUEST_REG, _) if self.status & PHASE != 0 => self.status = LOCKED | CHECK_DONE,
                (register, _)
                    if (COMMAND_BASE..COMMAND_BASE + 8).contains(&register)
                        && self.status & PHASE == 0 => {}
                _ => {
                    self.memory.insert(address, value);
                }
            }
        }
    }

    impl PortIo for FakeEc {
        fn read(&mut self, port: u16) -> Result<u8, HwError> {
            match port.checked_sub(self.nct.base) {
                Some(PAGE_PORT_OFFSET) => Ok(self.page),
                Some(DATA_PORT_OFFSET) => Ok(self.get(self.address())),
                _ => self.nct.read(port),
            }
        }

        fn write(&mut self, port: u16, value: u8) -> Result<(), HwError> {
            match port.checked_sub(self.nct.base) {
                Some(PAGE_PORT_OFFSET) => self.page = value,
                Some(INDEX_PORT_OFFSET) => self.index = value,
                Some(DATA_PORT_OFFSET) => self.store(self.address(), value),
                _ => return self.nct.write(port, value),
            }
            Ok(())
        }

        fn find_bars(&mut self) -> Result<(), HwError> {
            self.nct.find_bars()
        }
    }

    fn lock(tag: &str) -> NamedMutex {
        NamedMutex::open(&format!(r"Local\fanforge-ec-{tag}-{}", std::process::id())).unwrap()
    }

    fn nct6687d(tag: &str) -> SuperIo<FakeEc> {
        SuperIo::probe(FakeEc::new(0xD5, 0x92), Slot::Primary, lock(tag)).unwrap()
    }

    #[test]
    fn msi_800_series_boards_are_told_apart_by_name() {
        let msi = "Micro-Star International Co., Ltd.";
        assert!(is_msi_800_series(msi, "MAG X870 TOMAHAWK WIFI (MS-7E51)"));
        assert!(is_msi_800_series(msi, "PRO b850-p wifi"));
        assert!(!is_msi_800_series(msi, "MAG B650 TOMAHAWK WIFI (MS-7D75)"));
        assert!(!is_msi_800_series(
            "ASUSTeK COMPUTER INC.",
            "ROG STRIX X870-E"
        ));
    }

    #[test]
    fn the_nct668x_family_is_detected_with_its_layout() {
        let chip = nct6687d("detect");
        assert_eq!(chip.chip().name, "Nuvoton NCT6687D");
        assert!(chip.fan_control());
        assert_eq!(chip.fan_count(), 8);
        assert_eq!(chip.temp_names()[0], Some("CPU"));
        let nct6683d = CHIPS
            .iter()
            .find(|chip| chip.name == "Nuvoton NCT6683D")
            .unwrap();
        assert!(!nct6683d.fan_control);
        assert!(NCT6687D_MSI_800.fan_control);
        assert_eq!(NCT6687D_MSI_800.fan_count(), 2);
    }

    #[test]
    fn readings_follow_the_ec_layout() {
        let mut chip = nct6687d("read");
        let readings = chip.read().unwrap();
        assert_eq!(readings.rpm[0], Some(1200));
        assert_eq!(readings.temps[0], Some(45.5));
        assert_eq!(readings.temps[1], Some(-10.0));
        assert!(readings.channels.iter().all(ChannelState::is_auto));
        assert_eq!(chip.io.page, PAGE_SELECT);
    }

    #[test]
    fn manual_control_goes_through_the_configuration_handshake_and_restores() {
        let mut chip = nct6687d("control");
        let saved = chip.channel_states().unwrap()[2];
        chip.set_duty(2, 0x99).unwrap();
        assert_eq!(chip.io.get(MODE_REG), 0b100);
        assert_eq!(chip.io.get(COMMAND_BASE + 2), 0x99);
        assert_eq!(chip.io.status, LOCKED | CHECK_DONE);
        assert!(!chip.channel_states().unwrap()[2].is_auto());
        let request = chip
            .io
            .writes
            .iter()
            .position(|&write| write == (REQUEST_REG, CONFIG_REQUEST));
        let duty = chip
            .io
            .writes
            .iter()
            .position(|&write| write == (COMMAND_BASE + 2, 0x99));
        let done = chip
            .io
            .writes
            .iter()
            .position(|&write| write == (REQUEST_REG, 0x40));
        assert!(request < duty && duty < done);

        chip.restore(2, Some(saved)).unwrap();
        assert_eq!(chip.io.get(MODE_REG), 0);
        assert_eq!(chip.io.get(COMMAND_BASE + 2), 0x60);
    }

    #[test]
    fn without_a_snapshot_only_manual_headers_go_to_full_speed() {
        let mut chip = nct6687d("unsaved");
        chip.set_duty(1, 0x30).unwrap();
        let writes = chip.io.writes.len();
        chip.restore(0, None).unwrap();
        assert_eq!(chip.io.writes.len(), writes);
        chip.restore(1, None).unwrap();
        assert_eq!(chip.io.get(COMMAND_BASE + 1), FULL_SPEED);
        assert_eq!(chip.io.get(MODE_REG), 0b10);
    }

    #[test]
    fn a_failed_duty_restore_still_hands_the_header_back() {
        let mut chip = nct6687d("restore-stuck");
        let saved = chip.channel_states().unwrap()[3];
        chip.set_duty(3, 0x40).unwrap();
        chip.io.refuse_phase = true;
        assert!(matches!(
            chip.restore(3, Some(saved)),
            Err(HwError::FanConfigTimeout(_))
        ));
        assert_eq!(chip.io.get(MODE_REG), 0);
    }

    #[test]
    fn probing_starts_monitoring() {
        let chip = nct6687d("start");
        assert_eq!(chip.io.get(MONITOR_CONFIG_REG), MONITOR_START);
    }

    #[test]
    fn a_controller_that_never_opens_the_configuration_phase_times_out() {
        let mut chip = nct6687d("stuck");
        chip.io.refuse_phase = true;
        assert!(matches!(
            chip.set_duty(0, 0x80),
            Err(HwError::FanConfigTimeout(_))
        ));
        assert_eq!(chip.io.get(COMMAND_BASE), 0x60);
    }

    #[test]
    fn the_nct6683d_is_monitored_only() {
        let mut chip =
            SuperIo::probe(FakeEc::new(0xC7, 0x32), Slot::Primary, lock("6683")).unwrap();
        assert_eq!(chip.chip().name, "Nuvoton NCT6683D");
        assert!(!chip.fan_control());
        assert!(matches!(
            chip.set_duty(0, 0x80),
            Err(HwError::NoFanControl(_))
        ));
        chip.read().unwrap();
    }

    #[test]
    fn temperatures_decode_halves_and_reject_junk() {
        assert_eq!(decode_temperature(40, 0x80), Some(40.5));
        assert_eq!(decode_temperature(40, 0x7F), Some(40.0));
        assert_eq!(decode_temperature(0x80, 0), None);
    }
}
