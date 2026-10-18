use std::fs::File;
use std::io;
use std::os::windows::io::OwnedHandle;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Pipes::{CreatePipe, PIPE_NOWAIT, SetNamedPipeHandleState};

use crate::{borrow_handle, own_handle};

#[derive(Debug)]
pub struct AnonymousPipe {
    pub read: OwnedHandle,
    pub write: OwnedHandle,
}

impl AnonymousPipe {
    pub fn new() -> io::Result<Self> {
        let mut read = HANDLE::default();
        let mut write = HANDLE::default();
        unsafe { CreatePipe(&mut read, &mut write, None, 0) }?;
        let read = unsafe { own_handle(read) }.ok_or_else(io::Error::last_os_error)?;
        let write = unsafe { own_handle(write) }.ok_or_else(io::Error::last_os_error)?;
        Ok(Self { read, write })
    }
}

pub fn nonblocking_writer(handle: OwnedHandle) -> io::Result<File> {
    let mode = PIPE_NOWAIT;
    unsafe { SetNamedPipeHandleState(borrow_handle(&handle), Some(&mode), None, None) }?;
    Ok(File::from(handle))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::*;

    #[test]
    fn bytes_flow_and_a_closed_writer_reads_as_end_of_file() {
        let pipe = AnonymousPipe::new().unwrap();
        let mut reader = File::from(pipe.read);
        let mut writer = nonblocking_writer(pipe.write).unwrap();
        assert_eq!(writer.write(&[7]).unwrap(), 1);
        let mut byte = [0u8; 1];
        assert_eq!(reader.read(&mut byte).unwrap(), 1);
        assert_eq!(byte, [7]);
        drop(writer);
        assert_eq!(reader.read(&mut byte).unwrap(), 0);
    }

    #[test]
    fn a_full_pipe_never_blocks_the_writer() {
        let pipe = AnonymousPipe::new().unwrap();
        let mut writer = nonblocking_writer(pipe.write).unwrap();
        let chunk = [1u8; 4096];
        let mut accepted = 0usize;
        for _ in 0..64 {
            accepted += writer.write(&chunk).unwrap_or(0);
        }
        assert!(accepted < 64 * chunk.len());
        drop(pipe.read);
    }
}
