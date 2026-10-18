pub mod board;
pub mod config;
pub mod curve;
pub mod duty;
pub mod engine;
pub mod ids;
pub mod paths;
pub mod protocol;
pub mod safety;
pub mod store;
pub mod telemetry;

pub use duty::Duty;
pub use ids::{CurveId, FanId, SensorId};
