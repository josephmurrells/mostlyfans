use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows::Win32::Foundation::HANDLE;

/// # Safety
/// `handle` must be a valid handle that is not owned or closed by anything else.
pub unsafe fn own_handle(handle: HANDLE) -> Option<OwnedHandle> {
    if handle.is_invalid() {
        return None;
    }
    Some(unsafe { OwnedHandle::from_raw_handle(handle.0) })
}

pub fn borrow_handle(handle: &OwnedHandle) -> HANDLE {
    HANDLE(handle.as_raw_handle())
}
