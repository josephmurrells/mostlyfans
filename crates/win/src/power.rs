use std::ffi::c_void;
use std::io;
use std::time::Duration;

use windows::Win32::Foundation::{ERROR_SUCCESS, HANDLE};
use windows::Win32::System::Power::{
    DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, HPOWERNOTIFY, PowerRegisterSuspendResumeNotification,
    PowerUnregisterSuspendResumeNotification,
};
use windows::Win32::System::WindowsProgramming::QueryUnbiasedInterruptTime;
use windows::Win32::UI::WindowsAndMessaging::{
    DEVICE_NOTIFY_CALLBACK, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, PBT_APMSUSPEND,
};

const TICKS_PER_MICROSECOND: u64 = 10;

pub fn unbiased_time() -> Duration {
    let mut ticks = 0u64;
    unsafe { QueryUnbiasedInterruptTime(&mut ticks) }.ok().ok();
    Duration::from_micros(ticks / TICKS_PER_MICROSECOND)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerEvent {
    Suspend,
    Resume,
}

type Callback = Box<dyn Fn(PowerEvent) + Send + Sync>;

pub struct PowerSubscription {
    handle: HPOWERNOTIFY,
}

impl std::fmt::Debug for PowerSubscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PowerSubscription").finish_non_exhaustive()
    }
}

unsafe extern "system" fn on_power_event(
    context: *const c_void,
    kind: u32,
    _setting: *const c_void,
) -> u32 {
    let callback = unsafe { &*context.cast::<Callback>() };
    let event = match kind {
        PBT_APMSUSPEND => Some(PowerEvent::Suspend),
        PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => Some(PowerEvent::Resume),
        _ => None,
    };
    if let Some(event) = event {
        callback(event);
    }
    ERROR_SUCCESS.0
}

pub fn subscribe_power(
    callback: impl Fn(PowerEvent) + Send + Sync + 'static,
) -> io::Result<PowerSubscription> {
    let callback: *mut Callback = Box::into_raw(Box::new(Box::new(callback)));
    let parameters = Box::into_raw(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
        Callback: Some(on_power_event),
        Context: callback.cast(),
    }));
    let mut handle: *mut c_void = std::ptr::null_mut();
    let status = unsafe {
        PowerRegisterSuspendResumeNotification(
            DEVICE_NOTIFY_CALLBACK,
            HANDLE(parameters.cast()),
            &mut handle,
        )
    };
    if status != ERROR_SUCCESS {
        drop(unsafe { Box::from_raw(parameters) });
        drop(unsafe { Box::from_raw(callback) });
        return Err(io::Error::from_raw_os_error(status.0 as i32));
    }
    Ok(PowerSubscription {
        handle: HPOWERNOTIFY(handle as isize),
    })
}

impl Drop for PowerSubscription {
    fn drop(&mut self) {
        let _ = unsafe { PowerUnregisterSuspendResumeNotification(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unbiased_time_advances() {
        let first = unbiased_time();
        std::thread::sleep(Duration::from_millis(20));
        assert!(unbiased_time() > first);
    }

    #[test]
    fn subscriptions_register_and_unregister() {
        let subscription = subscribe_power(|_| {}).unwrap();
        drop(subscription);
    }
}
