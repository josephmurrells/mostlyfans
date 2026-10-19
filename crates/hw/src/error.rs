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
    VendorMismatch(u8),
    #[error("environment controller did not acknowledge register {0:#04x}")]
    ReadNotAcknowledged(u8),
    #[error("register {register:#04x} verification failed: wrote {written:#04x}, read {read:#04x}")]
    VerifyFailed { register: u8, written: u8, read: u8 },
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
