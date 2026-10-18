use std::io;

use fanforge_core::protocol::{FrameError, Request, Response, read_frame, write_frame};
use fanforge_win::{own_handle, wide};
use thiserror::Error;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PIPE_BUSY};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_NONE, FILE_WRITE_DATA,
    OPEN_EXISTING, SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT, SYNCHRONIZE,
};
use windows::core::PCWSTR;
use windows_service::service::ServiceAccess;
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

use crate::PipeStream;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerCheck {
    Service(String),
    AnyProcess,
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("the FanForge service is not running")]
    NotRunning,
    #[error("the FanForge service is busy")]
    Busy,
    #[error("pipe server process {actual} is not the FanForge service (expected {expected:?})")]
    Untrusted { expected: Option<u32>, actual: u32 },
    #[error("failed to query the FanForge service: {0}")]
    Service(#[from] windows_service::Error),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Frame(#[from] FrameError),
}

#[derive(Debug)]
pub struct Client {
    stream: PipeStream,
}

impl Client {
    pub fn connect(pipe_name: &str, check: &ServerCheck) -> Result<Self, ClientError> {
        let name = wide(pipe_name);
        let access = FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
        let raw = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                access.0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                None,
            )
        }
        .map_err(|error| match error.code() {
            code if code == ERROR_FILE_NOT_FOUND.to_hresult() => ClientError::NotRunning,
            code if code == ERROR_PIPE_BUSY.to_hresult() => ClientError::Busy,
            _ => ClientError::Io(error.into()),
        })?;
        let handle = unsafe { own_handle(raw) }.ok_or_else(io::Error::last_os_error)?;
        let stream = PipeStream::new(handle);
        if let ServerCheck::Service(service) = check {
            let expected = service_process_id(service)?;
            let actual = stream.server_process_id()?;
            if expected != Some(actual) {
                return Err(ClientError::Untrusted { expected, actual });
            }
        }
        Ok(Self { stream })
    }

    pub fn request(&mut self, request: &Request) -> Result<Response, ClientError> {
        write_frame(&mut self.stream, request)?;
        Ok(read_frame(&mut self.stream)?)
    }
}

fn service_process_id(name: &str) -> Result<Option<u32>, ClientError> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(name, ServiceAccess::QUERY_STATUS)?;
    Ok(service.query_status()?.process_id)
}

#[cfg(test)]
mod tests {
    use std::thread;

    use fanforge_core::protocol::{read_frame, write_frame};

    use super::*;
    use crate::PipeListener;

    fn pipe_name(tag: &str) -> String {
        format!(r"\\.\pipe\fanforge-test-{tag}-{}", std::process::id())
    }

    #[test]
    fn request_response_round_trip() {
        let name = pipe_name("roundtrip");
        let mut listener = PipeListener::bind(&name).unwrap();
        let server = thread::spawn(move || {
            let mut stream = listener.accept().unwrap();
            for _ in 0..2 {
                let request: Request = read_frame(&mut stream).unwrap();
                let response = match request {
                    Request::GetState => Response::Ok,
                    _ => Response::Error {
                        message: "unexpected".into(),
                    },
                };
                write_frame(&mut stream, &response).unwrap();
            }
        });
        let mut client = Client::connect(&name, &ServerCheck::AnyProcess).unwrap();
        assert_eq!(client.request(&Request::GetState).unwrap(), Response::Ok);
        assert!(matches!(
            client
                .request(&Request::Identify { fan: "pwm1".into() })
                .unwrap(),
            Response::Error { .. }
        ));
        server.join().unwrap();
    }

    #[test]
    fn second_listener_on_the_same_name_is_refused() {
        let name = pipe_name("squat");
        let _first = PipeListener::bind(&name).unwrap();
        assert!(PipeListener::bind(&name).is_err());
    }

    #[test]
    fn missing_server_reports_not_running() {
        let result = Client::connect(&pipe_name("missing"), &ServerCheck::AnyProcess);
        assert!(matches!(result, Err(ClientError::NotRunning)));
    }

    #[test]
    fn untrusted_server_is_rejected() {
        let name = pipe_name("untrusted");
        let mut listener = PipeListener::bind(&name).unwrap();
        let (done, wait) = std::sync::mpsc::channel::<()>();
        let server = thread::spawn(move || {
            let _stream = listener.accept().unwrap();
            wait.recv().ok();
        });
        let result = Client::connect(&name, &ServerCheck::Service("Spooler".into()));
        done.send(()).unwrap();
        server.join().unwrap();
        assert!(
            matches!(
                result,
                Err(ClientError::Untrusted { .. } | ClientError::Service(_))
            ),
            "{result:?}"
        );
    }
}
