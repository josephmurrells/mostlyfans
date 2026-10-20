use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::FanId;
use crate::board::{BoardInfo, FanLimits};
use crate::calibration::CalibrationView;
use crate::config::Config;
use crate::engine::FanStatus;
use crate::telemetry::Readings;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", content = "detail", rename_all = "snake_case")]
pub enum HardwareStatus {
    Ready,
    Unavailable(String),
    DriverMissing,
    Suspended,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ControlState {
    pub version: String,
    pub hardware: HardwareStatus,
    pub board: Option<BoardInfo>,
    pub readings: Readings,
    pub fans: Vec<FanStatus>,
    pub limits: BTreeMap<FanId, FanLimits>,
    pub config: Config,
    pub critical: bool,
    pub monitor_only: bool,
    pub conflicts: Vec<String>,
    pub warnings: Vec<String>,
    #[serde(default)]
    pub calibration: CalibrationView,
}
