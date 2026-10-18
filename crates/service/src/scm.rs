use std::ffi::OsString;
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use fanforge_core::paths::SERVICE_NAME;
use windows_service::service::{
    PowerEventParam, ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState,
    ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{
    self, ServiceControlHandlerResult, ServiceStatusHandle,
};
use windows_service::{define_windows_service, service_dispatcher};

use crate::shared::LoopMessage;
use crate::{StartError, start};

const SUSPEND_ACK_TIMEOUT: Duration = Duration::from_secs(2);
const STOP_WAIT_HINT: Duration = Duration::from_secs(10);
const FAILURE_EXIT_CODE: u32 = 1;

define_windows_service!(ffi_service_main, service_main);

pub fn dispatch() -> windows_service::Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
}

fn service_main(_arguments: Vec<OsString>) {
    if let Err(error) = run() {
        tracing::error!(%error, "service terminated with an error");
    }
}

fn run() -> Result<(), StartError> {
    let (loop_tx, loop_rx) = mpsc::channel();
    let handler_tx = loop_tx.clone();
    let status_slot: Arc<OnceLock<ServiceStatusHandle>> = Arc::new(OnceLock::new());
    let handler_slot = Arc::clone(&status_slot);
    let handler = move |control| -> ServiceControlHandlerResult {
        match control {
            ServiceControl::Stop | ServiceControl::Preshutdown => {
                if let Some(status) = handler_slot.get() {
                    report(status, ServiceState::StopPending, None, STOP_WAIT_HINT).ok();
                }
                handler_tx.send(LoopMessage::Stop).ok();
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::PowerEvent(PowerEventParam::Suspend) => {
                let (done_tx, done_rx) = mpsc::sync_channel(1);
                if handler_tx.send(LoopMessage::Suspend(done_tx)).is_ok() {
                    done_rx.recv_timeout(SUSPEND_ACK_TIMEOUT).ok();
                }
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::PowerEvent(
                PowerEventParam::ResumeAutomatic | PowerEventParam::ResumeSuspend,
            ) => {
                handler_tx.send(LoopMessage::Resume).ok();
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::PowerEvent(_) | ServiceControl::Interrogate => {
                ServiceControlHandlerResult::NoError
            }
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    };
    let status = service_control_handler::register(SERVICE_NAME, handler)?;
    status_slot.set(status).ok();
    report(&status, ServiceState::Running, None, Duration::ZERO)?;
    let result = start(loop_tx, loop_rx);
    let failure = result.as_ref().err().map(|_| FAILURE_EXIT_CODE);
    report(&status, ServiceState::Stopped, failure, Duration::ZERO)?;
    result
}

fn report(
    handle: &ServiceStatusHandle,
    state: ServiceState,
    failure: Option<u32>,
    wait_hint: Duration,
) -> windows_service::Result<()> {
    let controls_accepted = if state == ServiceState::Running {
        ServiceControlAccept::STOP
            | ServiceControlAccept::PRESHUTDOWN
            | ServiceControlAccept::POWER_EVENT
    } else {
        ServiceControlAccept::empty()
    };
    let exit_code = failure.map_or(ServiceExitCode::Win32(0), ServiceExitCode::ServiceSpecific);
    handle.set_service_status(ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted,
        exit_code,
        checkpoint: 0,
        wait_hint,
        process_id: None,
    })
}
