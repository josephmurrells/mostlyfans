#![cfg(windows)]

pub mod amd;
pub mod board;
pub mod chips;
pub mod cpu;
pub mod ec;
mod error;
pub mod gigabyte;
pub mod gpu_fans;
pub mod intel;
pub mod ite;
pub mod mutex;
pub mod nuvoton;
pub mod pawnio;
pub mod pinmux;
pub mod port;
pub mod sensors;
pub mod smu;
pub mod snapshot;
pub mod superio;
pub mod system;

pub use error::HwError;
