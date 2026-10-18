use std::io;
use std::os::windows::io::OwnedHandle;
use std::time::Duration;

use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, GetLastError, HANDLE,
    WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, OpenEventW, OpenMutexW, ReleaseMutex,
    SYNCHRONIZATION_READ_CONTROL, SYNCHRONIZATION_SYNCHRONIZE, SetEvent, WaitForSingleObject,
};
use windows::core::PCWSTR;

use crate::integrity::{object_integrity, process_integrity};
use crate::{SecurityAttributes, borrow_handle, own_handle, wide};

pub const ADMIN_ONLY_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)S:(ML;;NW;;;HI)";

fn millis(timeout: Option<Duration>) -> u32 {
    timeout.map_or(INFINITE, |timeout| {
        u32::try_from(timeout.as_millis()).unwrap_or(INFINITE - 1)
    })
}

pub(crate) fn wait_handle(handle: &OwnedHandle, timeout: Option<Duration>) -> io::Result<bool> {
    match unsafe { WaitForSingleObject(borrow_handle(handle), millis(timeout)) } {
        WAIT_OBJECT_0 | WAIT_ABANDONED => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(io::Error::last_os_error()),
    }
}

fn squatted(name: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("`{name}` exists with permissions FanForge did not set"),
    )
}

fn verify_trusted(name: &str, handle: OwnedHandle) -> io::Result<OwnedHandle> {
    let ours = process_integrity()?;
    match object_integrity(&handle) {
        Ok(theirs) if theirs >= ours => Ok(handle),
        _ => Err(squatted(name)),
    }
}

fn created_handle(name: &str, result: windows::core::Result<HANDLE>) -> io::Result<OwnedHandle> {
    let existed = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    let handle = match result {
        Ok(raw) => unsafe { own_handle(raw) }.ok_or_else(io::Error::last_os_error)?,
        Err(error) if error.code() == ERROR_ACCESS_DENIED.to_hresult() => {
            return Err(squatted(name));
        }
        Err(error) => return Err(error.into()),
    };
    if existed {
        verify_trusted(name, handle)
    } else {
        Ok(handle)
    }
}

#[derive(Debug)]
pub struct InstanceMutex {
    name: String,
    handle: OwnedHandle,
}

impl InstanceMutex {
    pub fn create(name: &str, sddl: &str) -> io::Result<Self> {
        let security = SecurityAttributes::from_sddl(sddl)?;
        let wide_name = wide(name);
        let result =
            unsafe { CreateMutexW(Some(security.as_ptr()), false, PCWSTR(wide_name.as_ptr())) };
        Ok(Self {
            name: name.to_owned(),
            handle: created_handle(name, result)?,
        })
    }

    pub fn exists(name: &str) -> bool {
        let wide_name = wide(name);
        let opened = unsafe {
            OpenMutexW(
                SYNCHRONIZATION_SYNCHRONIZE,
                false,
                PCWSTR(wide_name.as_ptr()),
            )
        };
        match opened {
            Ok(raw) => {
                drop(unsafe { own_handle(raw) });
                true
            }
            Err(error) => error.code() != ERROR_FILE_NOT_FOUND.to_hresult(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn acquire(&self, timeout: Duration) -> io::Result<bool> {
        wait_handle(&self.handle, Some(timeout))
    }
}

impl Drop for InstanceMutex {
    fn drop(&mut self) {
        unsafe { ReleaseMutex(borrow_handle(&self.handle)) }.ok();
    }
}

#[derive(Debug)]
pub struct Event {
    handle: OwnedHandle,
}

impl Event {
    pub fn create(name: &str, sddl: &str) -> io::Result<Self> {
        let security = SecurityAttributes::from_sddl(sddl)?;
        let wide_name = wide(name);
        let result = unsafe {
            CreateEventW(
                Some(security.as_ptr()),
                false,
                false,
                PCWSTR(wide_name.as_ptr()),
            )
        };
        Ok(Self {
            handle: created_handle(name, result)?,
        })
    }

    pub fn open(name: &str) -> io::Result<Option<Self>> {
        let wide_name = wide(name);
        let opened = unsafe {
            OpenEventW(
                SYNCHRONIZATION_SYNCHRONIZE | EVENT_MODIFY_STATE | SYNCHRONIZATION_READ_CONTROL,
                false,
                PCWSTR(wide_name.as_ptr()),
            )
        };
        match opened {
            Ok(raw) => match unsafe { own_handle(raw) } {
                Some(handle) => Ok(Some(Self {
                    handle: verify_trusted(name, handle)?,
                })),
                None => Ok(None),
            },
            Err(error) if error.code() == ERROR_FILE_NOT_FOUND.to_hresult() => Ok(None),
            Err(error) if error.code() == ERROR_ACCESS_DENIED.to_hresult() => Err(squatted(name)),
            Err(error) => Err(error.into()),
        }
    }

    pub fn set(&self) -> io::Result<()> {
        unsafe { SetEvent(borrow_handle(&self.handle)) }.map_err(io::Error::from)
    }

    pub fn wait(&self, timeout: Option<Duration>) -> io::Result<bool> {
        wait_handle(&self.handle, timeout)
    }
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::*;

    const TEST_SDDL: &str = "D:P(A;;GA;;;WD)";

    fn unique(tag: &str) -> String {
        format!(r"Local\fanforge-win-{tag}-{}", std::process::id())
    }

    #[test]
    fn instance_mutex_excludes_other_threads() {
        let name = unique("instance");
        assert!(!InstanceMutex::exists(&name));
        let mutex = InstanceMutex::create(&name, TEST_SDDL).unwrap();
        assert!(InstanceMutex::exists(&name));
        assert!(mutex.acquire(Duration::ZERO).unwrap());
        let other = name.clone();
        let blocked = thread::spawn(move || {
            let mutex = InstanceMutex::create(&other, TEST_SDDL).unwrap();
            !mutex.acquire(Duration::from_millis(20)).unwrap()
        });
        assert!(blocked.join().unwrap());
    }

    #[test]
    fn objects_planted_at_a_lower_integrity_are_refused() {
        const LOW_LABEL_SDDL: &str = "D:P(A;;GA;;;WD)S:(ML;;NW;;;LW)";
        let mutex_name = unique("planted-mutex");
        let _planted = InstanceMutex::create(&mutex_name, LOW_LABEL_SDDL).unwrap();
        let error = InstanceMutex::create(&mutex_name, TEST_SDDL).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);

        let event_name = unique("planted-event");
        let _planted = Event::create(&event_name, LOW_LABEL_SDDL).unwrap();
        assert!(Event::create(&event_name, TEST_SDDL).is_err());
        assert!(Event::open(&event_name).is_err());
    }

    #[test]
    fn events_signal_across_handles() {
        let name = unique("event");
        assert!(Event::open(&name).unwrap().is_none());
        let event = Event::create(&name, TEST_SDDL).unwrap();
        let opened = Event::open(&name).unwrap().unwrap();
        assert!(!event.wait(Some(Duration::ZERO)).unwrap());
        opened.set().unwrap();
        assert!(event.wait(Some(Duration::from_secs(1))).unwrap());
        assert!(!event.wait(Some(Duration::ZERO)).unwrap());
    }
}
