use windows::Win32::Foundation::HANDLE;
use windows::Win32::Security::{GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::{borrow_handle, own_handle};

pub fn is_elevated() -> windows::core::Result<bool> {
    let mut raw = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw)? };
    let token = unsafe { own_handle(raw) }.ok_or_else(windows::core::Error::from_thread)?;
    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0u32;
    unsafe {
        GetTokenInformation(
            borrow_handle(&token),
            TokenElevation,
            Some((&raw mut elevation).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )?;
    }
    Ok(elevation.TokenIsElevated != 0)
}
