use std::ffi::c_void;
use std::fs;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::Win32::Foundation::{ERROR_SUCCESS, HLOCAL, LocalFree, WIN32_ERROR};
use windows::Win32::Security::Authorization::{
    GetNamedSecurityInfoW, SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, IsWellKnownSid,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    WinBuiltinAdministratorsSid, WinLocalSystemSid,
};
use windows::core::{BOOL, PCWSTR};

use crate::SecurityAttributes;

pub const PRIVATE_DATA_SDDL: &str = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FRFX;;;BU)";

fn status(code: WIN32_ERROR) -> io::Result<()> {
    if code == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(code.0 as i32))
    }
}

pub fn secure_directory(path: &Path, sddl: &str) -> io::Result<()> {
    fs::create_dir_all(path)?;
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let name = PCWSTR(name.as_ptr());

    let mut owner = PSID::default();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    status(unsafe {
        GetNamedSecurityInfoW(
            name,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&mut owner),
            None,
            None,
            None,
            &mut descriptor,
        )
    })?;
    let trusted = unsafe {
        IsWellKnownSid(owner, WinLocalSystemSid).as_bool()
            || IsWellKnownSid(owner, WinBuiltinAdministratorsSid).as_bool()
    };
    unsafe {
        LocalFree(Some(HLOCAL(descriptor.0.cast::<c_void>())));
    }
    if !trusted {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is owned by an untrusted account; delete it and start the service again",
                path.display()
            ),
        ));
    }

    let wanted = SecurityAttributes::from_sddl(sddl)?;
    let mut present = BOOL::default();
    let mut defaulted = BOOL::default();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    unsafe {
        GetSecurityDescriptorDacl(wanted.descriptor(), &mut present, &mut dacl, &mut defaulted)?;
    }
    status(unsafe {
        SetNamedSecurityInfoW(
            name,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(dacl),
            None,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directories_owned_by_ordinary_users_are_rejected_unless_elevated() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("data");
        match secure_directory(&target, PRIVATE_DATA_SDDL) {
            Ok(()) => assert!(target.is_dir()),
            Err(error) => assert_eq!(error.kind(), io::ErrorKind::PermissionDenied),
        }
    }
}
