use std::io;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use fanforge_win::paths::APP_NAME;
use thiserror::Error;
use windows_service::service::{ServiceAccess, ServiceState};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

pub const SERVICE_NAME: &str = "FanForge";
pub const SERVICE_LABEL: &str = "the old FanForge service";
const SERVICE_EXECUTABLE: &str = "fanforge-service.exe";
const STOP_TIMEOUT: Duration = Duration::from_secs(20);
const STOP_POLL: Duration = Duration::from_millis(250);
const ERROR_SERVICE_MARKED_FOR_DELETE: i32 = 1072;

#[derive(Debug, Error)]
pub enum LegacyError {
    #[error("the service manager refused the request: {0}")]
    Service(#[from] windows_service::Error),
    #[error("the service did not stop within {} seconds", STOP_TIMEOUT.as_secs())]
    StopTimeout,
    #[error("could not remove {}: {source}", path.display())]
    Binary {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removal {
    Complete,
    BinaryRemovedAtReboot,
}

pub fn service_installed() -> bool {
    ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .and_then(|manager| manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS))
        .is_ok()
}

fn is_marked_for_delete(error: &windows_service::Error) -> bool {
    matches!(error, windows_service::Error::Winapi(source)
        if source.raw_os_error() == Some(ERROR_SERVICE_MARKED_FOR_DELETE))
}

fn stop_and_delete() -> Result<(), LegacyError> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    )?;
    if service.query_status()?.current_state != ServiceState::Stopped {
        if let Err(error) = service.stop() {
            tracing::warn!(%error, "stop request for the old service failed");
        }
        let started = Instant::now();
        while service.query_status()?.current_state != ServiceState::Stopped {
            if started.elapsed() > STOP_TIMEOUT {
                return Err(LegacyError::StopTimeout);
            }
            thread::sleep(STOP_POLL);
        }
    }
    match service.delete() {
        Ok(()) => Ok(()),
        Err(error) if is_marked_for_delete(&error) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn service_binary() -> io::Result<PathBuf> {
    Ok(fanforge_win::program_files_dir()?
        .join(APP_NAME)
        .join(SERVICE_EXECUTABLE))
}

fn remove_binary() -> Result<Removal, LegacyError> {
    let path = service_binary().map_err(|source| LegacyError::Binary {
        path: PathBuf::from(SERVICE_EXECUTABLE),
        source,
    })?;
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(Removal::Complete),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Removal::Complete),
        Err(error) => {
            tracing::warn!(%error, path = %path.display(), "service binary is locked; deleting it at reboot");
            fanforge_win::delete_at_reboot(&path)
                .map_err(|source| LegacyError::Binary { path, source })?;
            Ok(Removal::BinaryRemovedAtReboot)
        }
    }
}

pub fn remove_service() -> Result<Removal, LegacyError> {
    stop_and_delete()?;
    tracing::info!("old FanForge service removed");
    remove_binary()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_service_binary_lives_under_program_files() {
        let _ = service_installed();
        assert!(
            service_binary()
                .unwrap()
                .ends_with(r"FanForge\fanforge-service.exe")
        );
    }
}
