use std::marker::PhantomData;
use std::os::windows::io::OwnedHandle;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use fanforge_win::{SecurityAttributes, borrow_handle, own_handle, wide};
use windows::Win32::Foundation::{WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Threading::{
    CreateMutexW, MUTEX_MODIFY_STATE, OpenMutexW, ReleaseMutex, SYNCHRONIZATION_SYNCHRONIZE,
    WaitForSingleObject,
};
use windows::core::PCWSTR;

use crate::HwError;

pub const ISA_BUS: &str = r"Global\Access_ISABUS.HTP.Method";
pub const PCI_BUS: &str = r"Global\Access_PCI";
pub const DEFAULT_TIMEOUT: Duration = Duration::from_millis(100);

const MUTEX_SDDL: &str = "D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;0x100001;;;WD)";

#[derive(Debug)]
pub struct NamedMutex {
    name: String,
    handle: OwnedHandle,
}

#[derive(Debug)]
pub struct BusLock {
    mutex: NamedMutex,
    abandonments: AtomicU64,
}

impl BusLock {
    pub fn new(mutex: NamedMutex) -> Arc<Self> {
        Arc::new(Self {
            mutex,
            abandonments: AtomicU64::new(0),
        })
    }

    pub fn name(&self) -> &str {
        self.mutex.name()
    }

    pub fn acquire(&self, timeout: Duration) -> Result<MutexGuard<'_>, HwError> {
        let guard = self.mutex.acquire(timeout)?;
        if guard.was_abandoned() {
            self.abandonments.fetch_add(1, Ordering::SeqCst);
        }
        Ok(guard)
    }

    pub fn abandonments(&self) -> u64 {
        self.abandonments.load(Ordering::SeqCst)
    }
}

#[derive(Debug)]
pub struct MutexGuard<'a> {
    mutex: &'a NamedMutex,
    abandoned: bool,
    _thread_bound: PhantomData<*const ()>,
}

impl NamedMutex {
    pub fn open(name: &str) -> Result<Self, HwError> {
        let mutex_error = |source| HwError::Mutex {
            name: name.to_owned(),
            source,
        };
        let wide_name = wide(name);
        let security = SecurityAttributes::from_sddl(MUTEX_SDDL).map_err(mutex_error)?;
        let created =
            unsafe { CreateMutexW(Some(security.as_ptr()), false, PCWSTR(wide_name.as_ptr())) };
        let raw = match created {
            Ok(handle) => handle,
            Err(_) => unsafe {
                OpenMutexW(
                    SYNCHRONIZATION_SYNCHRONIZE | MUTEX_MODIFY_STATE,
                    false,
                    PCWSTR(wide_name.as_ptr()),
                )
            }
            .map_err(mutex_error)?,
        };
        let handle = unsafe { own_handle(raw) }
            .ok_or_else(|| mutex_error(windows::core::Error::from_thread()))?;
        Ok(Self {
            name: name.to_owned(),
            handle,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn acquire(&self, timeout: Duration) -> Result<MutexGuard<'_>, HwError> {
        let millis = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        let result = unsafe { WaitForSingleObject(borrow_handle(&self.handle), millis) };
        let abandoned = match result {
            WAIT_OBJECT_0 => false,
            WAIT_ABANDONED => true,
            WAIT_TIMEOUT => return Err(HwError::LockTimeout(self.name.clone())),
            _ => {
                return Err(HwError::Mutex {
                    name: self.name.clone(),
                    source: windows::core::Error::from_thread(),
                });
            }
        };
        Ok(MutexGuard {
            mutex: self,
            abandoned,
            _thread_bound: PhantomData,
        })
    }
}

impl MutexGuard<'_> {
    pub fn was_abandoned(&self) -> bool {
        self.abandoned
    }
}

impl Drop for MutexGuard<'_> {
    fn drop(&mut self) {
        if let Err(error) = unsafe { ReleaseMutex(borrow_handle(&self.mutex.handle)) } {
            tracing::error!(mutex = %self.mutex.name, %error, "failed to release mutex");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;

    use super::*;

    fn unique_name(tag: &str) -> String {
        format!(r"Local\fanforge-test-{tag}-{}", std::process::id())
    }

    #[test]
    fn excludes_other_threads_until_released() {
        let name = unique_name("exclusion");
        let mutex = NamedMutex::open(&name).unwrap();
        let guard = mutex.acquire(DEFAULT_TIMEOUT).unwrap();
        assert!(!guard.was_abandoned());

        let (tx, rx) = mpsc::channel();
        let other_name = name.clone();
        let worker = thread::spawn(move || {
            let other = NamedMutex::open(&other_name).unwrap();
            tx.send(matches!(
                other.acquire(Duration::from_millis(20)),
                Err(HwError::LockTimeout(_))
            ))
            .unwrap();
            rx_wait(&other)
        });
        assert!(rx.recv().unwrap());
        drop(guard);
        assert!(worker.join().unwrap());
    }

    fn rx_wait(mutex: &NamedMutex) -> bool {
        mutex.acquire(Duration::from_secs(5)).is_ok()
    }

    #[test]
    fn abandoned_mutex_is_reported() {
        let name = unique_name("abandoned");
        let owner_name = name.clone();
        thread::spawn(move || {
            let mutex = NamedMutex::open(&owner_name).unwrap();
            let guard = mutex.acquire(DEFAULT_TIMEOUT).unwrap();
            std::mem::forget(guard);
            std::mem::forget(mutex);
        })
        .join()
        .unwrap();
        let mutex = NamedMutex::open(&name).unwrap();
        let guard = mutex.acquire(DEFAULT_TIMEOUT).unwrap();
        assert!(guard.was_abandoned());
    }
}
