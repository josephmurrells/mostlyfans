use thiserror::Error;

#[derive(Debug, Error)]
pub enum HwError {
    #[error("the PawnIO driver is not installed; install it from https://pawnio.eu")]
    PawnIoMissing,
    #[error("access to the PawnIO driver was denied; run FanForge as administrator")]
    AccessDenied,
    #[error("PawnIO call `{call}` failed: {source}")]
    Driver {
        call: String,
        #[source]
        source: windows::core::Error,
    },
    #[error("PawnIO call `{call}` returned {got} values, expected {expected}")]
    ShortResult {
        call: String,
        expected: usize,
        got: usize,
    },
    #[error("failed to open mutex `{name}`: {source}")]
    Mutex {
        name: String,
        #[source]
        source: windows::core::Error,
    },
    #[error("timed out waiting for mutex `{0}`")]
    LockTimeout(String),
    #[error("no Super I/O chip responded (chip id {0:#06x})")]
    ChipNotFound(u16),
    #[error("unsupported Super I/O chip {0:#06x}")]
    UnsupportedChip(u16),
    #[error(
        "invalid environment controller base address {address:#06x} (verify read {verify:#06x})"
    )]
    InvalidBaseAddress { address: u16, verify: u16 },
    #[error("unexpected environment controller vendor id {0:#04x}")]
    VendorMismatch(u16),
    #[error("chip id {0:#06x} is not an ITE chip")]
    NotIte(u16),
    #[error(
        "environment controller configuration register reads {0:#04x}; the monitor is not running"
    )]
    UnexpectedConfiguration(u8),
    #[error(
        "the firmware has locked the Nuvoton hardware monitor I/O space and it was not unlocked"
    )]
    IoSpaceLocked,
    #[error("the Super I/O chip changed from {expected:#06x} to {found:#06x}")]
    ChipChanged { expected: u16, found: u16 },
    #[error(
        "bios_snapshot.toml records fans driven on chip {owner:#06x}, but the attached chip is {found:#06x}; check that the fans are under BIOS control, then delete {}",
        fanforge_win::paths::data_dir().join("bios_snapshot.toml").display()
    )]
    ForeignSnapshot { owner: u16, found: u16 },
    #[error("FanForge doesn't support fan control on the {0}; it only monitors it")]
    NoFanControl(&'static str),
    #[error("fan channel {0} is in manual mode with no saved BIOS state to return to")]
    UnknownBiosMode(usize),
    #[error("environment controller did not acknowledge register {0:#04x}")]
    ReadNotAcknowledged(u8),
    #[error("register {register:#04x} verification failed: wrote {written:#04x}, read {read:#04x}")]
    VerifyFailed {
        register: u16,
        written: u8,
        read: u8,
    },
    #[error("the fan controller timed out {0}")]
    FanConfigTimeout(&'static str),
    #[error("the fan controller rejected the new fan settings")]
    FanConfigRejected,
    #[error("fan channel {0} does not exist")]
    InvalidChannel(usize),
    #[error("unsupported CPU: {0}")]
    UnsupportedCpu(String),
    #[error("unknown fan `{0}`")]
    UnknownFan(String),
    #[error("fan control is halted; manual speeds are refused")]
    Halted,
    #[error(transparent)]
    Store(#[from] fanforge_core::store::StoreError),
}
