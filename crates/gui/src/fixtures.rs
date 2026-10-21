use std::cell::RefCell;
use std::io;
use std::rc::Rc;
use std::sync::Arc;

use fanforge_control::controller::CommandResult;
use fanforge_core::board::{BoardIdentity, BoardInfo, fan_limits};
use fanforge_core::calibration::{
    CalibrationOutcome, CalibrationProgress, CalibrationReport, CalibrationView, Phase,
};
use fanforge_core::config::{Config, FanMode, FanSettings};
use fanforge_core::curve::{CurveDef, CurveKind, CurvePoint, Tuning};
use fanforge_core::engine::{FanState, FanStatus};
use fanforge_core::state::{ControlState, HardwareStatus};
use fanforge_core::telemetry::{FanReading, HardwareMode, Readings, SensorReading};
use fanforge_core::{CurveId, Duty, FanId, SensorId};

use crate::connection::{ControlLink, ControlStatus, Request, RequestKind};

pub fn fan(index: usize) -> FanId {
    FanId::for_channel(index)
}

pub fn config() -> Config {
    let mut config = Config::default();
    config.curves.push(CurveDef {
        id: CurveId::new("curve-1"),
        name: "Silent CPU".to_owned(),
        kind: CurveKind::Graph {
            sensor: SensorId::new("cpu/package"),
            points: vec![
                CurvePoint::new(40.0, 30.0),
                CurvePoint::new(60.0, 45.0),
                CurvePoint::new(80.0, 100.0),
            ],
        },
        tuning: Tuning::default(),
    });
    if let Some(profile) = config.profiles.get_mut(&config.active_profile) {
        profile.fans.insert(
            fan(0),
            FanMode::Curve {
                curve: CurveId::new("curve-1"),
            },
        );
        profile.fans.insert(
            fan(1),
            FanMode::Manual {
                duty: Duty::new(40.0),
            },
        );
    }
    config.fans.insert(
        fan(0),
        FanSettings {
            name: Some("CPU cooler".to_owned()),
            ..FanSettings::default()
        },
    );
    config
}

pub fn state() -> ControlState {
    let sensor = |id: &str, label: &str, celsius: f32| SensorReading {
        id: SensorId::new(id),
        label: label.to_owned(),
        celsius: Some(celsius),
    };
    let reading = |index: usize, rpm: Option<u32>, duty: Option<f32>| FanReading {
        id: fan(index),
        label: format!("Fan {}", index + 1),
        rpm,
        mode: if duty.is_some() {
            HardwareMode::Manual
        } else {
            HardwareMode::Auto
        },
        duty: duty.map(Duty::new),
    };
    let status = |index: usize, state: FanState, output: Option<f32>| FanStatus {
        id: fan(index),
        state,
        target: output.map(Duty::new),
        output: output.map(Duty::new),
    };
    ControlState {
        version: "0.1.0".to_owned(),
        hardware: HardwareStatus::Ready,
        board: Some(BoardInfo {
            identity: BoardIdentity {
                manufacturer: "ASUSTeK COMPUTER INC.".to_owned(),
                product: "PRIME B350-PLUS".to_owned(),
            },
            chip_id: 0x8655,
            chip_name: "ITE IT8655E".to_owned(),
            fan_control: true,
            fan_headers: 3,
            likely_cpu_fan: Some(fan(0)),
        }),
        readings: Readings {
            sensors: vec![
                sensor("cpu/package", "CPU Tctl", 52.4),
                sensor("board/temp1", "Board temp 1", 36.0),
                sensor("board/temp2", "Board temp 2", 41.5),
            ],
            fans: vec![
                reading(0, Some(1180), Some(38.0)),
                reading(1, Some(820), Some(40.0)),
                reading(2, Some(0), None),
            ],
        },
        fans: vec![
            status(0, FanState::Controlled, Some(38.0)),
            status(1, FanState::Controlled, Some(40.0)),
            status(2, FanState::Released, None),
        ],
        limits: fan_limits(3),
        config: config(),
        critical: false,
        critical_reached: false,
        monitor_only: false,
        conflicts: Vec::new(),
        warnings: Vec::new(),
        calibration: CalibrationView::default(),
    }
}

#[derive(Debug)]
pub enum Sent {
    Config(Box<Config>),
    Profile(String),
    Identify(FanId),
    Calibrate(FanId),
    CancelCalibration,
}

pub struct Inner {
    pub status: ControlStatus,
    pub state: Arc<ControlState>,
    pub sent: Vec<Sent>,
    results: Vec<CommandResult>,
    held: Vec<Request>,
    pub hold: bool,
    pub reject_config: Option<String>,
}

#[derive(Clone)]
pub struct FakeLink(pub Rc<RefCell<Inner>>);

impl FakeLink {
    pub fn new(state: ControlState) -> Self {
        Self(Rc::new(RefCell::new(Inner {
            status: ControlStatus::Running,
            state: Arc::new(state),
            sent: Vec::new(),
            results: Vec::new(),
            held: Vec::new(),
            hold: false,
            reject_config: None,
        })))
    }

    pub fn update(&self, change: impl FnOnce(&mut ControlState)) {
        let mut inner = self.0.borrow_mut();
        let mut state = (*inner.state).clone();
        change(&mut state);
        inner.state = Arc::new(state);
    }

    pub fn release(&self) {
        let held = {
            let mut inner = self.0.borrow_mut();
            inner.hold = false;
            std::mem::take(&mut inner.held)
        };
        for request in held {
            self.send(request);
        }
    }

    pub fn live_config(&self) -> Config {
        self.0.borrow().state.config.clone()
    }

    pub fn sent(&self) -> Vec<String> {
        self.0
            .borrow()
            .sent
            .iter()
            .map(|sent| match sent {
                Sent::Config(config) => format!("config:{}", config.active_profile),
                Sent::Profile(name) => format!("profile:{name}"),
                Sent::Identify(fan) => format!("identify:{fan}"),
                Sent::Calibrate(fan) => format!("calibrate:{fan}"),
                Sent::CancelCalibration => "cancel-calibration".to_owned(),
            })
            .collect()
    }
}

impl ControlLink for FakeLink {
    fn view(&self) -> (ControlStatus, Option<Arc<ControlState>>) {
        let inner = self.0.borrow();
        (inner.status.clone(), Some(Arc::clone(&inner.state)))
    }

    fn send(&self, request: Request) {
        if self.0.borrow().hold {
            self.0.borrow_mut().held.push(request);
            return;
        }
        let rejection = self.0.borrow().reject_config.clone();
        let (sent, kind) = match request {
            Request::ReplaceConfig(config) if rejection.is_some() => {
                let mut inner = self.0.borrow_mut();
                inner.sent.push(Sent::Config(config));
                inner.results.push(CommandResult {
                    kind: RequestKind::Config,
                    outcome: Err(rejection.unwrap_or_default()),
                });
                return;
            }
            Request::ReplaceConfig(config) => {
                let applied = (*config).clone();
                self.update(|state| state.config = applied);
                (Sent::Config(config), RequestKind::Config)
            }
            Request::SetActiveProfile(name) => {
                let active = name.clone();
                self.update(|state| state.config.active_profile = active);
                (Sent::Profile(name), RequestKind::Profile)
            }
            Request::Identify(fan) => {
                let target = fan.clone();
                self.update(|state| {
                    for status in &mut state.fans {
                        if status.id == target {
                            status.state = FanState::Identifying;
                        }
                    }
                });
                (Sent::Identify(fan), RequestKind::Identify)
            }
            Request::Calibrate(fan) => {
                let target = fan.clone();
                self.update(|state| {
                    state.calibration.active = Some(CalibrationProgress {
                        fan: target.clone(),
                        phase: Phase::StepDown,
                        duty: Duty::FULL,
                        rpm: Some(1500),
                        points: Vec::new(),
                        elapsed: std::time::Duration::ZERO,
                    });
                    for status in &mut state.fans {
                        if status.id == target {
                            status.state = FanState::Calibrating;
                        }
                    }
                });
                (Sent::Calibrate(fan), RequestKind::Calibrate)
            }
            Request::CancelCalibration => {
                self.update(|state| {
                    if let Some(progress) = state.calibration.active.take() {
                        state.calibration.last = Some(CalibrationReport {
                            fan: progress.fan,
                            outcome: CalibrationOutcome::Cancelled,
                        });
                    }
                });
                (Sent::CancelCalibration, RequestKind::Calibrate)
            }
        };
        let mut inner = self.0.borrow_mut();
        inner.sent.push(sent);
        inner.results.push(CommandResult {
            kind,
            outcome: Ok(()),
        });
    }

    fn take_results(&self) -> Vec<CommandResult> {
        std::mem::take(&mut self.0.borrow_mut().results)
    }

    fn restart(&mut self) -> io::Result<()> {
        self.0.borrow_mut().status = ControlStatus::Running;
        Ok(())
    }
}
