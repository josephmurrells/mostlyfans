#![cfg(windows)]

pub mod amd;
pub mod board;
pub mod chips;
mod error;
pub mod ite;
pub mod mutex;
pub mod nuvoton;
pub mod pawnio;
pub mod port;
pub mod sensors;
pub mod snapshot;
pub mod superio;
pub mod system;

pub use error::HwError;
