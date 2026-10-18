use std::ffi::c_void;
use std::fmt;

use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::core::PCWSTR;

use crate::wide;

pub struct SecurityAttributes {
    descriptor: PSECURITY_DESCRIPTOR,
    attributes: SECURITY_ATTRIBUTES,
}

impl SecurityAttributes {
    pub fn from_sddl(sddl: &str) -> windows::core::Result<Self> {
        let text = wide(sddl);
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(text.as_ptr()),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )?;
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        Ok(Self {
            descriptor,
            attributes,
        })
    }

    pub fn descriptor(&self) -> PSECURITY_DESCRIPTOR {
        self.descriptor
    }

    pub fn as_ptr(&self) -> *const SECURITY_ATTRIBUTES {
        &self.attributes
    }
}

impl Drop for SecurityAttributes {
    fn drop(&mut self) {
        unsafe {
            LocalFree(Some(HLOCAL(self.descriptor.0.cast::<c_void>())));
        }
    }
}

impl fmt::Debug for SecurityAttributes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecurityAttributes").finish_non_exhaustive()
    }
}

unsafe impl Send for SecurityAttributes {}
unsafe impl Sync for SecurityAttributes {}
