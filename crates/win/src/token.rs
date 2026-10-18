use std::ffi::c_void;
use std::os::windows::io::OwnedHandle;

use windows::Win32::Foundation::{HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{
    GetTokenInformation, TOKEN_ELEVATION, TOKEN_INFORMATION_CLASS, TOKEN_QUERY, TOKEN_USER,
    TokenElevation, TokenUser,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::PWSTR;

use crate::{borrow_handle, own_handle};

pub(crate) fn process_token() -> windows::core::Result<OwnedHandle> {
    let mut raw = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw)? };
    unsafe { own_handle(raw) }.ok_or_else(windows::core::Error::from_thread)
}

pub(crate) fn token_information(
    token: &OwnedHandle,
    class: TOKEN_INFORMATION_CLASS,
) -> windows::core::Result<Vec<u64>> {
    let mut needed = 0u32;
    unsafe { GetTokenInformation(borrow_handle(token), class, None, 0, &mut needed) }.ok();
    let mut buffer = vec![0u64; (needed as usize).div_ceil(size_of::<u64>()).max(1)];
    unsafe {
        GetTokenInformation(
            borrow_handle(token),
            class,
            Some(buffer.as_mut_ptr().cast()),
            (buffer.len() * size_of::<u64>()) as u32,
            &mut needed,
        )
    }?;
    Ok(buffer)
}

pub fn is_elevated() -> windows::core::Result<bool> {
    let token = process_token()?;
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

pub fn current_user_sid() -> windows::core::Result<String> {
    let token = process_token()?;
    let buffer = token_information(&token, TokenUser)?;
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let mut text = PWSTR::null();
    unsafe { ConvertSidToStringSidW(user.User.Sid, &mut text) }?;
    let sid = unsafe { text.to_string() }.map_err(|_| windows::core::Error::from_thread());
    unsafe { LocalFree(Some(HLOCAL(text.0.cast::<c_void>()))) };
    sid
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_current_user_has_a_sid() {
        assert!(current_user_sid().unwrap().starts_with("S-1-"));
    }
}
