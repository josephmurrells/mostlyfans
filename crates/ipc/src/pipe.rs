use std::io::{self, Read, Write};
use std::os::windows::io::OwnedHandle;

use fanforge_win::borrow_handle;
use windows::Win32::Foundation::{ERROR_BROKEN_PIPE, ERROR_NO_DATA};
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows::Win32::System::Pipes::GetNamedPipeServerProcessId;

#[derive(Debug)]
pub struct PipeStream {
    handle: OwnedHandle,
}

impl PipeStream {
    pub(crate) fn new(handle: OwnedHandle) -> Self {
        Self { handle }
    }

    pub fn server_process_id(&self) -> io::Result<u32> {
        let mut pid = 0u32;
        unsafe { GetNamedPipeServerProcessId(borrow_handle(&self.handle), &mut pid) }
            .map_err(io::Error::from)?;
        Ok(pid)
    }
}

fn is_disconnect(error: &windows::core::Error) -> bool {
    error.code() == ERROR_BROKEN_PIPE.to_hresult() || error.code() == ERROR_NO_DATA.to_hresult()
}

impl Read for PipeStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let mut read = 0u32;
        match unsafe {
            ReadFile(
                borrow_handle(&self.handle),
                Some(buffer),
                Some(&mut read),
                None,
            )
        } {
            Ok(()) => Ok(read as usize),
            Err(error) if is_disconnect(&error) => Ok(0),
            Err(error) => Err(error.into()),
        }
    }
}

impl Write for PipeStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let mut written = 0u32;
        match unsafe {
            WriteFile(
                borrow_handle(&self.handle),
                Some(buffer),
                Some(&mut written),
                None,
            )
        } {
            Ok(()) => Ok(written as usize),
            Err(error) if is_disconnect(&error) => Err(io::ErrorKind::BrokenPipe.into()),
            Err(error) => Err(error.into()),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
