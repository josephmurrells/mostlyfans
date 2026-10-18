use std::io;
use std::os::windows::io::OwnedHandle;

use fanforge_win::{SecurityAttributes, borrow_handle, own_handle, wide};
use windows::Win32::Foundation::ERROR_PIPE_CONNECTED;
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAGS_AND_ATTRIBUTES, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::core::PCWSTR;

use crate::PipeStream;

pub const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;OW)(A;;0x120083;;;IU)";
const MAX_INSTANCES: u32 = 8;
const BUFFER_SIZE: u32 = 64 * 1024;

#[derive(Debug)]
pub struct PipeListener {
    name: Vec<u16>,
    security: SecurityAttributes,
    pending: Option<OwnedHandle>,
}

impl PipeListener {
    pub fn bind(name: &str) -> io::Result<Self> {
        Self::bind_with_sddl(name, PIPE_SDDL)
    }

    pub fn bind_with_sddl(name: &str, sddl: &str) -> io::Result<Self> {
        let name = wide(name);
        let security = SecurityAttributes::from_sddl(sddl)?;
        let pending = Some(create_instance(&name, &security, true)?);
        Ok(Self {
            name,
            security,
            pending,
        })
    }

    pub fn accept(&mut self) -> io::Result<PipeStream> {
        let handle = match self.pending.take() {
            Some(handle) => handle,
            None => create_instance(&self.name, &self.security, false)?,
        };
        let connected = unsafe { ConnectNamedPipe(borrow_handle(&handle), None) };
        if let Err(error) = connected
            && error.code() != ERROR_PIPE_CONNECTED.to_hresult()
        {
            self.pending = Some(handle);
            return Err(error.into());
        }
        self.pending = create_instance(&self.name, &self.security, false).ok();
        Ok(PipeStream::new(handle))
    }
}

fn create_instance(
    name: &[u16],
    security: &SecurityAttributes,
    first: bool,
) -> io::Result<OwnedHandle> {
    let mut open_mode = PIPE_ACCESS_DUPLEX;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    let raw = unsafe {
        CreateNamedPipeW(
            PCWSTR(name.as_ptr()),
            FILE_FLAGS_AND_ATTRIBUTES(open_mode.0),
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            MAX_INSTANCES,
            BUFFER_SIZE,
            BUFFER_SIZE,
            0,
            Some(security.as_ptr()),
        )
    };
    unsafe { own_handle(raw) }.ok_or_else(io::Error::last_os_error)
}
