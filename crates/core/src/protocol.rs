use std::collections::BTreeMap;
use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::FanId;
use crate::board::{BoardInfo, FanLimits};
use crate::config::Config;
use crate::engine::FanStatus;
use crate::telemetry::Readings;

pub const MAX_FRAME_LEN: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Request {
    GetState,
    ReplaceConfig(Box<Config>),
    Identify { fan: FanId },
    SetActiveProfile { name: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Response {
    State(Box<ServiceState>),
    Ok,
    Error { message: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", content = "detail", rename_all = "snake_case")]
pub enum HardwareStatus {
    Ready,
    Unavailable(String),
    Suspended,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServiceState {
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
}

#[derive(Debug, Error)]
pub enum FrameError {
    #[error("connection closed")]
    Closed,
    #[error("frame of {0} bytes exceeds the {MAX_FRAME_LEN} byte limit")]
    TooLarge(usize),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("invalid message: {0}")]
    Json(#[from] serde_json::Error),
}

pub fn write_frame<W: Write, T: Serialize>(writer: &mut W, message: &T) -> Result<(), FrameError> {
    let body = serde_json::to_vec(message)?;
    if body.len() > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(body.len()));
    }
    let len = u32::try_from(body.len()).map_err(|_| FrameError::TooLarge(body.len()))?;
    let mut frame = Vec::with_capacity(body.len() + 4);
    frame.extend_from_slice(&len.to_le_bytes());
    frame.extend_from_slice(&body);
    writer.write_all(&frame)?;
    writer.flush()?;
    Ok(())
}

pub fn read_frame<R: Read, T: DeserializeOwned>(reader: &mut R) -> Result<T, FrameError> {
    let mut header = [0u8; 4];
    match reader.read_exact(&mut header) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
            return Err(FrameError::Closed);
        }
        Err(error) => return Err(error.into()),
    }
    let len = u32::from_le_bytes(header) as usize;
    if len > MAX_FRAME_LEN {
        return Err(FrameError::TooLarge(len));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    Ok(serde_json::from_slice(&body)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip() {
        let mut buffer = Vec::new();
        let request = Request::ReplaceConfig(Box::default());
        write_frame(&mut buffer, &request).unwrap();
        write_frame(&mut buffer, &Request::Identify { fan: "pwm1".into() }).unwrap();
        let mut cursor = io::Cursor::new(buffer);
        assert_eq!(read_frame::<_, Request>(&mut cursor).unwrap(), request);
        assert_eq!(
            read_frame::<_, Request>(&mut cursor).unwrap(),
            Request::Identify { fan: "pwm1".into() }
        );
        assert!(matches!(
            read_frame::<_, Request>(&mut cursor),
            Err(FrameError::Closed)
        ));
    }

    #[test]
    fn oversized_frames_are_rejected_before_allocation() {
        let mut data = (u32::MAX).to_le_bytes().to_vec();
        data.extend_from_slice(b"{}");
        let result = read_frame::<_, Request>(&mut io::Cursor::new(data));
        assert!(matches!(result, Err(FrameError::TooLarge(_))));
    }

    #[test]
    fn malformed_json_is_an_error() {
        let mut data = 2u32.to_le_bytes().to_vec();
        data.extend_from_slice(b"{]");
        assert!(matches!(
            read_frame::<_, Request>(&mut io::Cursor::new(data)),
            Err(FrameError::Json(_))
        ));
    }
}
