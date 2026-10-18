use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;

use fanforge_core::FanId;
use fanforge_core::board::{BoardInfo, FanLimits};
use fanforge_core::engine::Command;
use fanforge_core::telemetry::Readings;
use fanforge_hw::HwError;
use fanforge_hw::port::PortIo;
use fanforge_hw::system::Hardware;

pub trait Backend {
    fn read(&mut self) -> Result<Readings, HwError>;
    fn apply(&mut self, command: &Command, halted: &AtomicBool) -> Result<(), HwError>;
    fn release_all(&mut self) -> Result<(), HwError>;
    fn reinitialize(&mut self) -> Result<(), HwError>;
    fn initialize_channels(&mut self, take_control: bool) -> Result<(), HwError>;
    fn board_info(&self) -> BoardInfo;
    fn fan_limits(&self) -> BTreeMap<FanId, FanLimits>;
    fn warnings(&self) -> Vec<String>;
    fn owns_any(&self) -> bool;
}

impl<P: PortIo> Backend for Hardware<P> {
    fn read(&mut self) -> Result<Readings, HwError> {
        Hardware::read(self)
    }

    fn apply(&mut self, command: &Command, halted: &AtomicBool) -> Result<(), HwError> {
        Hardware::apply(self, command, halted)
    }

    fn release_all(&mut self) -> Result<(), HwError> {
        Hardware::release_all(self)
    }

    fn reinitialize(&mut self) -> Result<(), HwError> {
        Hardware::reinitialize(self)
    }

    fn initialize_channels(&mut self, take_control: bool) -> Result<(), HwError> {
        Hardware::initialize_channels(self, take_control)
    }

    fn board_info(&self) -> BoardInfo {
        Hardware::board_info(self)
    }

    fn fan_limits(&self) -> BTreeMap<FanId, FanLimits> {
        Hardware::fan_limits(self)
    }

    fn warnings(&self) -> Vec<String> {
        Hardware::warnings(self)
    }

    fn owns_any(&self) -> bool {
        Hardware::owns_any(self)
    }
}
