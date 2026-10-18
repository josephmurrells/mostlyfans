use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use fanforge_core::paths::{self, SERVICE_NAME};
use windows_service::service::{
    ServiceAccess, ServiceAction, ServiceActionType, ServiceErrorControl, ServiceFailureActions,
    ServiceFailureResetPeriod, ServiceInfo, ServiceStartType, ServiceState, ServiceType,
};
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

const DISPLAY_NAME: &str = "FanForge Fan Control";
const DESCRIPTION: &str =
    "Controls motherboard fan speeds and curves. Fans return to BIOS control when stopped.";
const STOP_TIMEOUT: Duration = Duration::from_secs(20);
const EXECUTABLE_NAME: &str = "fanforge-service.exe";

fn install_dir() -> PathBuf {
    env::var_os("ProgramW6432")
        .or_else(|| env::var_os("ProgramFiles"))
        .map_or_else(|| PathBuf::from(r"C:\Program Files"), PathBuf::from)
        .join(SERVICE_NAME)
}

fn deploy_binary() -> io::Result<PathBuf> {
    let source = env::current_exe()?;
    let directory = install_dir();
    fs::create_dir_all(&directory)?;
    let target = directory.join(EXECUTABLE_NAME);
    if fs::canonicalize(&source)? != fs::canonicalize(&directory)?.join(EXECUTABLE_NAME) {
        fs::copy(&source, &target)?;
    }
    Ok(target)
}

pub fn install() -> windows_service::Result<()> {
    let executable_path = deploy_binary().map_err(windows_service::Error::Winapi)?;
    fanforge_win::secure_directory(&paths::data_dir(), fanforge_win::PRIVATE_DATA_SDDL)
        .map_err(windows_service::Error::Winapi)?;
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )?;
    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from(DISPLAY_NAME),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path,
        launch_arguments: Vec::new(),
        dependencies: Vec::new(),
        account_name: None,
        account_password: None,
    };
    let service =
        manager.create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)?;
    service.set_description(DESCRIPTION)?;
    let restart = ServiceAction {
        action_type: ServiceActionType::Restart,
        delay: Duration::from_secs(5),
    };
    service.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(24 * 60 * 60)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![restart.clone(), restart.clone(), restart]),
    })?;
    service.set_failure_actions_on_non_crash_failures(true)?;
    service.start::<&str>(&[])?;
    Ok(())
}

pub fn uninstall() -> windows_service::Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(
        SERVICE_NAME,
        ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE,
    )?;
    if service.query_status()?.current_state != ServiceState::Stopped {
        service.stop()?;
        let started = Instant::now();
        while service.query_status()?.current_state != ServiceState::Stopped {
            if started.elapsed() > STOP_TIMEOUT {
                tracing::warn!("service did not stop in time; deleting anyway");
                break;
            }
            thread::sleep(Duration::from_millis(250));
        }
    }
    service.delete()
}
