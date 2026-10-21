use std::os::windows::io::OwnedHandle;

use fanforge_win::{borrow_handle, own_handle, read_local_machine_string, wide};
use windows::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, GENERIC_READ, GENERIC_WRITE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::core::PCWSTR;

use crate::HwError;

pub const LPC_IO_MODULE: &[u8] = include_bytes!("../modules/LpcIO.bin");
pub const AMD_FAMILY_17_MODULE: &[u8] = include_bytes!("../modules/AMDFamily17.bin");
pub const INTEL_MSR_MODULE: &[u8] = include_bytes!("../modules/IntelMSR.bin");
pub const RYZEN_SMU_MODULE: &[u8] = include_bytes!("../modules/RyzenSMU.bin");

const DEVICE_PATH: &str = r"\\?\GLOBALROOT\Device\PawnIO";
const UNINSTALL_KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO";
const DEVICE_TYPE: u32 = 41394 << 16;
const IOCTL_LOAD_BINARY: u32 = DEVICE_TYPE | (0x821 << 2);
const IOCTL_EXECUTE: u32 = DEVICE_TYPE | (0x841 << 2);
const FUNCTION_NAME_LEN: usize = 32;

pub fn installed_version() -> Option<String> {
    read_local_machine_string(UNINSTALL_KEY, "DisplayVersion")
}

#[derive(Debug)]
pub struct PawnIoModule {
    handle: OwnedHandle,
}

fn open_device() -> Result<OwnedHandle, HwError> {
    let path = wide(DEVICE_PATH);
    let raw = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|error| match error.code() {
        code if code == ERROR_FILE_NOT_FOUND.to_hresult()
            || code == ERROR_PATH_NOT_FOUND.to_hresult() =>
        {
            HwError::PawnIoMissing
        }
        code if code == ERROR_ACCESS_DENIED.to_hresult() => HwError::AccessDenied,
        _ => HwError::Driver {
            call: "open".to_owned(),
            source: error,
        },
    })?;
    unsafe { own_handle(raw) }.ok_or(HwError::PawnIoMissing)
}

pub fn driver_present() -> bool {
    !matches!(open_device(), Err(HwError::PawnIoMissing))
}

impl PawnIoModule {
    pub fn load(blob: &[u8]) -> Result<Self, HwError> {
        let handle = open_device()?;
        unsafe {
            DeviceIoControl(
                borrow_handle(&handle),
                IOCTL_LOAD_BINARY,
                Some(blob.as_ptr().cast()),
                blob.len() as u32,
                None,
                0,
                None,
                None,
            )
        }
        .map_err(|source| HwError::Driver {
            call: "load module".to_owned(),
            source,
        })?;
        Ok(Self { handle })
    }

    pub fn execute(
        &self,
        function: &str,
        input: &[u64],
        output_len: usize,
    ) -> Result<Vec<u64>, HwError> {
        let request = encode_request(function, input);
        let mut output = vec![0u64; output_len];
        let mut returned = 0u32;
        unsafe {
            DeviceIoControl(
                borrow_handle(&self.handle),
                IOCTL_EXECUTE,
                Some(request.as_ptr().cast()),
                request.len() as u32,
                (output_len > 0).then_some(output.as_mut_ptr().cast()),
                (output_len * size_of::<u64>()) as u32,
                Some(&mut returned),
                None,
            )
        }
        .map_err(|source| HwError::Driver {
            call: function.to_owned(),
            source,
        })?;
        let got = returned as usize / size_of::<u64>();
        if got < output_len {
            return Err(HwError::ShortResult {
                call: function.to_owned(),
                expected: output_len,
                got,
            });
        }
        Ok(output)
    }
}

fn encode_request(function: &str, input: &[u64]) -> Vec<u8> {
    let mut request = vec![0u8; FUNCTION_NAME_LEN + size_of_val(input)];
    let name = function.as_bytes();
    let name_len = name.len().min(FUNCTION_NAME_LEN - 1);
    request[..name_len].copy_from_slice(&name[..name_len]);
    let (slots, _) = request[FUNCTION_NAME_LEN..].as_chunks_mut::<{ size_of::<u64>() }>();
    for (slot, value) in slots.iter_mut().zip(input) {
        *slot = value.to_le_bytes();
    }
    request
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_codes_match_the_driver() {
        assert_eq!(IOCTL_LOAD_BINARY, 0xA1B2_2084);
        assert_eq!(IOCTL_EXECUTE, 0xA1B2_2104);
    }

    #[test]
    fn request_layout_is_name_then_little_endian_arguments() {
        let request = encode_request("ioctl_pio_outb", &[0x2E, 0x87]);
        assert_eq!(request.len(), 48);
        assert_eq!(&request[..14], b"ioctl_pio_outb");
        assert!(request[14..32].iter().all(|&byte| byte == 0));
        assert_eq!(&request[32..40], &0x2Eu64.to_le_bytes());
        assert_eq!(&request[40..48], &0x87u64.to_le_bytes());
    }

    #[test]
    fn long_names_keep_a_terminator() {
        let request = encode_request(&"x".repeat(40), &[]);
        assert_eq!(request.len(), FUNCTION_NAME_LEN);
        assert_eq!(request[FUNCTION_NAME_LEN - 1], 0);
    }

    #[test]
    fn modules_are_embedded() {
        assert!(LPC_IO_MODULE.len() > 1024);
        assert!(AMD_FAMILY_17_MODULE.len() > 1024);
        assert!(INTEL_MSR_MODULE.len() > 1024);
        assert!(RYZEN_SMU_MODULE.len() > 1024);
    }
}
