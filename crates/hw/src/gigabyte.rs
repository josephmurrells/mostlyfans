use std::time::{Duration, Instant};

use crate::HwError;
use crate::pawnio::{ISA_BRIDGE_EC_MODULE, PawnIoModule};
use crate::port::PortIo;

pub const SMARTFAN_ENABLE: u16 = 0x947;
pub const SMARTFAN_ON: u8 = 0x01;
pub const OPERATION_DEADLINE: Duration = Duration::from_millis(200);
pub const TICK_DEADLINE: Duration = Duration::from_millis(75);
pub const SETTLE: Duration = Duration::from_millis(500);

const G23_VERSION: [u16; 2] = [0x900, 0x901];
const G1_VERSION: [u16; 2] = [0xA00, 0xA01];
const G3_TACHS: [[u16; 2]; 3] = [[0x916, 0x917], [0x918, 0x919], [0x91A, 0x91B]];
const ECIO_STATUS: u16 = 0x3F4;
const ECIO_DATA: u16 = 0x3F0;
const ECIO_READ: u8 = 0xB0;
const ECIO_WRITE: u8 = 0xB1;
const INPUT_FULL: u8 = 0x02;
const OUTPUT_FULL: u8 = 0x01;
const ECIO_MAJOR_VERSION: u8 = 1;
const IT8792E_ID: u16 = 0x8733;
const IT87952E_ID: u16 = 0x8695;
const IT8790E_ID: u16 = 0x8790;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Generation {
    G1,
    G2,
    G3,
    Unknown,
}

pub fn generation(chip_id: u16, g23: [u8; 2], g1: [u8; 2]) -> Generation {
    let newer = matches!(chip_id, IT8792E_ID | IT87952E_ID);
    match g23 {
        [0x01, 0x00] if newer => Generation::G3,
        [0x00, 0x01 | 0x03 | 0x04] if chip_id == IT8792E_ID => Generation::G2,
        _ if matches!(chip_id, IT8790E_ID | IT8792E_ID)
            && g1[0] != 0
            && g1[1] != 0
            && g1 != [0xFF, 0xFF] =>
        {
            Generation::G1
        }
        _ => Generation::Unknown,
    }
}

pub fn restored_smartfan(saved: Option<u8>, current: u8) -> u8 {
    saved.unwrap_or(current) | SMARTFAN_ON
}

pub trait H2Ram {
    fn read(&mut self, offset: u16, deadline: Instant) -> Result<u8, HwError>;
    fn write(&mut self, offset: u16, value: u8, deadline: Instant) -> Result<(), HwError>;

    fn read_version(&mut self, offsets: [u16; 2], deadline: Instant) -> Result<[u8; 2], HwError> {
        Ok([
            self.read(offsets[0], deadline)?,
            self.read(offsets[1], deadline)?,
        ])
    }

    fn generation(&mut self, chip_id: u16) -> Result<Generation, HwError> {
        let deadline = Instant::now() + OPERATION_DEADLINE * 4;
        let g23 = self.read_version(G23_VERSION, deadline)?;
        let g1 = self.read_version(G1_VERSION, deadline)?;
        Ok(generation(chip_id, g23, g1))
    }

    fn g3_tachs(&mut self) -> Result<[u16; 3], HwError> {
        let deadline = Instant::now() + TICK_DEADLINE;
        let mut counts = [0u16; 3];
        for (count, [low, high]) in counts.iter_mut().zip(G3_TACHS) {
            *count = u16::from_le_bytes([self.read(low, deadline)?, self.read(high, deadline)?]);
        }
        Ok(counts)
    }
}

pub struct Ecio<'a, P: PortIo> {
    io: &'a mut P,
}

impl<'a, P: PortIo> Ecio<'a, P> {
    pub fn new(io: &'a mut P) -> Self {
        Self { io }
    }

    pub fn present(&mut self) -> Result<bool, HwError> {
        let deadline = Instant::now() + OPERATION_DEADLINE;
        Ok(self.read(G23_VERSION[0], deadline)? == ECIO_MAJOR_VERSION)
    }

    fn wait(&mut self, ready: impl Fn(u8) -> bool, deadline: Instant) -> Result<(), HwError> {
        loop {
            if ready(self.io.read(ECIO_STATUS)?) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(HwError::FanConfigTimeout(
                    "waiting for the Gigabyte fan controller",
                ));
            }
        }
    }

    fn send(&mut self, port: u16, value: u8, deadline: Instant) -> Result<(), HwError> {
        self.wait(|status| status & INPUT_FULL == 0, deadline)?;
        self.io.write(port, value)
    }

    fn command(&mut self, command: u8, offset: u16, deadline: Instant) -> Result<(), HwError> {
        let [high, low] = offset.to_be_bytes();
        self.send(ECIO_STATUS, command, deadline)?;
        self.send(ECIO_DATA, high, deadline)?;
        self.send(ECIO_DATA, low, deadline)
    }
}

impl<P: PortIo> H2Ram for Ecio<'_, P> {
    fn read(&mut self, offset: u16, deadline: Instant) -> Result<u8, HwError> {
        self.command(ECIO_READ, offset, deadline)?;
        self.wait(|status| status & OUTPUT_FULL != 0, deadline)?;
        self.io.read(ECIO_DATA)
    }

    fn write(&mut self, offset: u16, value: u8, deadline: Instant) -> Result<(), HwError> {
        self.command(ECIO_WRITE, offset, deadline)?;
        self.send(ECIO_DATA, value, deadline)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BridgeState {
    Original,
    Disabled,
    Enabled2E,
    Enabled4E,
}

impl BridgeState {
    fn code(self) -> u64 {
        match self {
            Self::Original => u64::MAX,
            Self::Disabled => 0,
            Self::Enabled2E => 1,
            Self::Enabled4E => 2,
        }
    }

    fn from_code(code: u64) -> Self {
        match code {
            0 => Self::Disabled,
            1 => Self::Enabled2E,
            2 => Self::Enabled4E,
            _ => Self::Original,
        }
    }
}

pub trait Bridge: std::fmt::Debug + Send {
    fn second_window(&mut self) -> Result<Option<u64>, HwError>;
    fn current_state(&mut self) -> Result<BridgeState, HwError>;
    fn set_state(&mut self, state: BridgeState) -> Result<(), HwError>;
    fn map(&mut self) -> Result<(), HwError>;
    fn unmap(&mut self) -> Result<(), HwError>;
    fn read_byte(&mut self, offset: u16) -> Result<u8, HwError>;
    fn write_byte(&mut self, offset: u16, value: u8) -> Result<(), HwError>;
}

#[derive(Debug)]
pub struct IsaBridge {
    module: PawnIoModule,
}

const SECOND_SLOT: u64 = 1;
const BYTE: u64 = 1;
const ACCESS_READ: u64 = 0;
const ACCESS_WRITE: u64 = 1;

impl IsaBridge {
    pub fn open() -> Result<Self, HwError> {
        Ok(Self {
            module: PawnIoModule::load(ISA_BRIDGE_EC_MODULE)?,
        })
    }

    fn access(&mut self, offset: u16, kind: u64, value: u8) -> Result<u8, HwError> {
        let output = self.module.execute(
            "ioctl_access_superio_mmio",
            &[SECOND_SLOT, u64::from(offset), BYTE, kind, u64::from(value)],
            1,
        )?;
        Ok(output[0] as u8)
    }
}

impl Bridge for IsaBridge {
    fn second_window(&mut self) -> Result<Option<u64>, HwError> {
        let found = self.module.execute("ioctl_find_superio_mmio", &[], 6)?;
        Ok((found[3] != 0 && found[4] != 0).then_some(found[3]))
    }

    fn current_state(&mut self) -> Result<BridgeState, HwError> {
        let output = self
            .module
            .execute("ioctl_iomem_mmio_get_cur_state", &[], 1)?;
        Ok(BridgeState::from_code(output[0]))
    }

    fn set_state(&mut self, state: BridgeState) -> Result<(), HwError> {
        self.module
            .execute("ioctl_iomem_mmio_set_state", &[state.code()], 0)
            .map(drop)
    }

    fn map(&mut self) -> Result<(), HwError> {
        self.module
            .execute("ioctl_map_superio_mmio", &[], 0)
            .map(drop)
    }

    fn unmap(&mut self) -> Result<(), HwError> {
        self.module
            .execute("ioctl_unmap_superio_mmio", &[], 0)
            .map(drop)
    }

    fn read_byte(&mut self, offset: u16) -> Result<u8, HwError> {
        self.access(offset, ACCESS_READ, 0)
    }

    fn write_byte(&mut self, offset: u16, value: u8) -> Result<(), HwError> {
        self.access(offset, ACCESS_WRITE, value).map(drop)
    }
}

pub struct Window<'a>(pub &'a mut dyn Bridge);

impl H2Ram for Window<'_> {
    fn read(&mut self, offset: u16, _deadline: Instant) -> Result<u8, HwError> {
        self.0.read_byte(offset)
    }

    fn write(&mut self, offset: u16, value: u8, _deadline: Instant) -> Result<(), HwError> {
        self.0.write_byte(offset, value)
    }
}

pub fn with_window<B: Bridge + ?Sized, T>(
    bridge: &mut B,
    action: impl FnOnce(&mut B) -> Result<T, HwError>,
) -> Result<T, HwError> {
    bridge.map()?;
    let opened = match bridge.current_state() {
        Ok(BridgeState::Enabled4E) => Ok(()),
        Ok(_) => bridge.set_state(BridgeState::Enabled4E),
        Err(error) => Err(error),
    };
    let result = opened.and_then(|()| action(bridge));
    let restored = bridge.set_state(BridgeState::Original);
    let unmapped = bridge.unmap();
    let value = result?;
    restored?;
    unmapped?;
    Ok(value)
}

pub fn smfi_present<B: Bridge + ?Sized>(bridge: &mut B) -> Result<bool, HwError> {
    if bridge.second_window()?.is_none() {
        return Ok(false);
    }
    with_window(bridge, |bridge| bridge.read_byte(SMARTFAN_ENABLE)).map(|value| value != 0xFF)
}

#[cfg(any(test, feature = "test-support"))]
pub mod fake {
    use std::collections::BTreeMap;

    use super::*;

    #[derive(Debug, Clone)]
    pub struct FakeEcio {
        pub ram: BTreeMap<u16, u8>,
        pub stuck: bool,
        pub writes: Vec<(u16, u8)>,
        pending: Vec<u8>,
        output: Option<u8>,
        command: Option<u8>,
        pub status_reads: usize,
    }

    impl FakeEcio {
        pub fn new() -> Self {
            Self {
                ram: BTreeMap::from([
                    (0x900, 0x01),
                    (0x901, 0x00),
                    (SMARTFAN_ENABLE, 0x01),
                    (0x916, 0x52),
                    (0x917, 0x02),
                ]),
                stuck: false,
                writes: Vec::new(),
                pending: Vec::new(),
                output: None,
                command: None,
                status_reads: 0,
            }
        }

        fn step(&mut self) {
            let Some(command) = self.command else {
                return;
            };
            if self.pending.len() < 2 {
                return;
            }
            let offset = u16::from_be_bytes([self.pending[0], self.pending[1]]);
            match command {
                ECIO_READ => {
                    self.output = Some(self.ram.get(&offset).copied().unwrap_or(0));
                    self.command = None;
                    self.pending.clear();
                }
                ECIO_WRITE if self.pending.len() == 3 => {
                    self.ram.insert(offset, self.pending[2]);
                    self.writes.push((offset, self.pending[2]));
                    self.command = None;
                    self.pending.clear();
                }
                _ => {}
            }
        }
    }

    impl Default for FakeEcio {
        fn default() -> Self {
            Self::new()
        }
    }

    impl PortIo for FakeEcio {
        fn read(&mut self, port: u16) -> Result<u8, HwError> {
            match port {
                ECIO_STATUS => {
                    self.status_reads += 1;
                    if self.stuck {
                        return Ok(INPUT_FULL);
                    }
                    Ok(u8::from(self.output.is_some()))
                }
                ECIO_DATA => Ok(self.output.take().unwrap_or(0xFF)),
                _ => Ok(0xFF),
            }
        }

        fn write(&mut self, port: u16, value: u8) -> Result<(), HwError> {
            match port {
                ECIO_STATUS => {
                    self.command = Some(value);
                    self.pending.clear();
                    self.output = None;
                }
                ECIO_DATA => {
                    self.pending.push(value);
                    self.step();
                }
                _ => {}
            }
            Ok(())
        }

        fn find_bars(&mut self) -> Result<(), HwError> {
            Ok(())
        }
    }

    #[derive(Debug, Clone, Default)]
    pub struct FakeBridge {
        pub window: Option<u64>,
        pub ram: BTreeMap<u16, u8>,
        pub state: Option<BridgeState>,
        pub calls: Vec<String>,
        pub mapped: bool,
        pub fail_on: Option<&'static str>,
    }

    impl FakeBridge {
        fn call(&mut self, name: &'static str) -> Result<(), HwError> {
            self.calls.push(name.to_owned());
            if self.fail_on == Some(name) {
                return Err(HwError::FanConfigRejected);
            }
            Ok(())
        }
    }

    impl Bridge for FakeBridge {
        fn second_window(&mut self) -> Result<Option<u64>, HwError> {
            self.call("find")?;
            Ok(self.window)
        }

        fn current_state(&mut self) -> Result<BridgeState, HwError> {
            self.call("state")?;
            Ok(self.state.unwrap_or(BridgeState::Original))
        }

        fn set_state(&mut self, state: BridgeState) -> Result<(), HwError> {
            self.calls.push(format!("set {state:?}"));
            if self.fail_on == Some("set") && state != BridgeState::Original {
                return Err(HwError::FanConfigRejected);
            }
            self.state = (state != BridgeState::Original).then_some(state);
            Ok(())
        }

        fn map(&mut self) -> Result<(), HwError> {
            self.call("map")?;
            self.mapped = true;
            Ok(())
        }

        fn unmap(&mut self) -> Result<(), HwError> {
            self.call("unmap")?;
            self.mapped = false;
            Ok(())
        }

        fn read_byte(&mut self, offset: u16) -> Result<u8, HwError> {
            self.call("read")?;
            if self.state != Some(BridgeState::Enabled4E) || !self.mapped {
                return Ok(0xFF);
            }
            Ok(self.ram.get(&offset).copied().unwrap_or(0))
        }

        fn write_byte(&mut self, offset: u16, value: u8) -> Result<(), HwError> {
            self.call("write")?;
            if self.state == Some(BridgeState::Enabled4E) && self.mapped {
                self.ram.insert(offset, value);
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeBridge, FakeEcio};
    use super::*;

    #[test]
    fn generations_follow_the_controller_version_bytes() {
        assert_eq!(generation(0x8733, [1, 0], [0, 0]), Generation::G3);
        assert_eq!(generation(0x8695, [1, 0], [0, 0]), Generation::G3);
        assert_eq!(generation(0x8790, [1, 0], [0, 0]), Generation::Unknown);
        assert_eq!(generation(0x8733, [0, 3], [0, 0]), Generation::G2);
        assert_eq!(generation(0x8695, [0, 3], [0, 0]), Generation::Unknown);
        assert_eq!(generation(0x8790, [0, 0], [2, 1]), Generation::G1);
        assert_eq!(generation(0x8695, [0, 0], [2, 1]), Generation::Unknown);
        assert_eq!(
            generation(0x8790, [0xFF, 0xFF], [0xFF, 0xFF]),
            Generation::Unknown
        );
    }

    #[test]
    fn the_restored_smartfan_byte_always_has_bit_0_on() {
        assert_eq!(restored_smartfan(Some(0x01), 0x00), 0x01);
        assert_eq!(restored_smartfan(Some(0x00), 0x00), 0x01);
        assert_eq!(restored_smartfan(Some(0x84), 0x00), 0x85);
        assert_eq!(restored_smartfan(None, 0x40), 0x41);
    }

    #[test]
    fn the_ecio_mailbox_reads_and_writes_h2ram() {
        let mut fake = FakeEcio::new();
        let mut ecio = Ecio::new(&mut fake);
        assert!(ecio.present().unwrap());
        assert_eq!(ecio.generation(0x8733).unwrap(), Generation::G3);
        let deadline = Instant::now() + OPERATION_DEADLINE;
        assert_eq!(ecio.read(SMARTFAN_ENABLE, deadline).unwrap(), 0x01);
        ecio.write(SMARTFAN_ENABLE, 0x00, deadline).unwrap();
        assert_eq!(ecio.read(SMARTFAN_ENABLE, deadline).unwrap(), 0x00);
        assert_eq!(ecio.g3_tachs().unwrap(), [0x0252, 0, 0]);
        assert_eq!(fake.writes, [(SMARTFAN_ENABLE, 0x00)]);
    }

    #[test]
    fn a_stuck_mailbox_gives_up_at_its_deadline() {
        let mut fake = FakeEcio::new();
        fake.stuck = true;
        let started = Instant::now();
        let result =
            Ecio::new(&mut fake).read(SMARTFAN_ENABLE, started + Duration::from_millis(30));
        assert!(matches!(result, Err(HwError::FanConfigTimeout(_))));
        assert!(started.elapsed() < Duration::from_millis(150));

        let started = Instant::now();
        assert!(Ecio::new(&mut fake).g3_tachs().is_err());
        assert!(started.elapsed() < TICK_DEADLINE + Duration::from_millis(75));
    }

    #[test]
    fn the_bridge_window_always_goes_back_to_the_original_state() {
        let mut bridge = FakeBridge {
            window: Some(0xFE00_0000),
            ram: [(SMARTFAN_ENABLE, 0x01)].into(),
            ..FakeBridge::default()
        };
        assert!(smfi_present(&mut bridge).unwrap());
        assert_eq!(
            bridge.calls,
            [
                "find",
                "map",
                "state",
                "set Enabled4E",
                "read",
                "set Original",
                "unmap"
            ]
        );
        assert_eq!(bridge.state, None);
        assert!(!bridge.mapped);

        for fail in ["read", "state", "set"] {
            let mut failing = FakeBridge {
                window: Some(0xFE00_0000),
                fail_on: Some(fail),
                ..FakeBridge::default()
            };
            assert!(with_window(&mut failing, |bridge| bridge.read_byte(SMARTFAN_ENABLE)).is_err());
            assert!(
                failing
                    .calls
                    .ends_with(&["set Original".to_owned(), "unmap".to_owned()]),
                "{fail}"
            );
            assert_eq!(failing.state, None);
        }

        let mut absent = FakeBridge::default();
        assert!(!smfi_present(&mut absent).unwrap());
        assert_eq!(absent.calls, ["find"]);
    }

    #[test]
    fn an_already_open_window_is_not_switched_again_but_is_still_restored() {
        let mut bridge = FakeBridge {
            window: Some(0xFE00_0000),
            state: Some(BridgeState::Enabled4E),
            ram: [(SMARTFAN_ENABLE, 0x01)].into(),
            ..FakeBridge::default()
        };
        with_window(&mut bridge, |bridge| bridge.write_byte(SMARTFAN_ENABLE, 0)).unwrap();
        assert!(!bridge.calls.contains(&"set Enabled4E".to_owned()));
        assert!(bridge.calls.contains(&"set Original".to_owned()));
        assert_eq!(bridge.ram[&SMARTFAN_ENABLE], 0);
    }
}
