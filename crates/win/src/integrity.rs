use std::ffi::c_void;
use std::io;
use std::os::windows::io::OwnedHandle;

use windows::Win32::Foundation::{ERROR_SUCCESS, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT};
use windows::Win32::Security::{
    ACE_HEADER, ACL, GetAce, GetSidSubAuthority, GetSidSubAuthorityCount,
    LABEL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SYSTEM_MANDATORY_LABEL_ACE,
    TOKEN_MANDATORY_LABEL, TokenIntegrityLevel,
};

use crate::borrow_handle;
use crate::token::{process_token, token_information};

const SYSTEM_MANDATORY_LABEL_ACE_TYPE: u8 = 0x11;
const MEDIUM_INTEGRITY: u32 = 0x2000;

/// # Safety
/// `sid` must point to a valid SID.
unsafe fn last_sub_authority(sid: PSID) -> u32 {
    let count = unsafe { *GetSidSubAuthorityCount(sid) };
    match count.checked_sub(1) {
        Some(last) => unsafe { *GetSidSubAuthority(sid, u32::from(last)) },
        None => 0,
    }
}

pub fn process_integrity() -> io::Result<u32> {
    let token = process_token()?;
    let buffer = token_information(&token, TokenIntegrityLevel)?;
    let label = unsafe { &*buffer.as_ptr().cast::<TOKEN_MANDATORY_LABEL>() };
    Ok(unsafe { last_sub_authority(label.Label.Sid) })
}

/// # Safety
/// `sacl` must be null or point to a valid ACL.
unsafe fn label_in(sacl: *const ACL) -> Option<u32> {
    if sacl.is_null() {
        return None;
    }
    let count = unsafe { (*sacl).AceCount };
    (0..u32::from(count)).find_map(|index| {
        let mut ace: *mut c_void = std::ptr::null_mut();
        unsafe { GetAce(sacl, index, &mut ace) }.ok()?;
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        if header.AceType != SYSTEM_MANDATORY_LABEL_ACE_TYPE {
            return None;
        }
        let label = ace.cast::<SYSTEM_MANDATORY_LABEL_ACE>();
        let sid = PSID(unsafe { &raw mut (*label).SidStart }.cast());
        Some(unsafe { last_sub_authority(sid) })
    })
}

pub fn object_integrity(handle: &OwnedHandle) -> io::Result<u32> {
    let mut sacl: *mut ACL = std::ptr::null_mut();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    let status = unsafe {
        GetSecurityInfo(
            borrow_handle(handle),
            SE_KERNEL_OBJECT,
            LABEL_SECURITY_INFORMATION,
            None,
            None,
            None,
            Some(&mut sacl),
            Some(&mut descriptor),
        )
    };
    if status != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(status.0 as i32));
    }
    let level = unsafe { label_in(sacl) }.unwrap_or(MEDIUM_INTEGRITY);
    unsafe { LocalFree(Some(HLOCAL(descriptor.0.cast::<c_void>()))) };
    Ok(level)
}
