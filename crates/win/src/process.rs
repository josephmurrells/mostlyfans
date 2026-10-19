use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use windows::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_ACCESS_DENIED, HANDLE,
};
use windows::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_TYPE_PIPE, GetFileType};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::JobObjects::IsProcessInJob;
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NO_WINDOW, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetCurrentProcessId, GetExitCodeProcess,
    GetProcessId, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROCESS_ACCESS_RIGHTS, PROCESS_CREATION_FLAGS,
    PROCESS_INFORMATION, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
};
use windows::core::{BOOL, PCWSTR, PWSTR};

use crate::sync::wait_handle;
use crate::{borrow_handle, from_wide, own_handle, wide};

static SPAWN_LOCK: Mutex<()> = Mutex::new(());

pub fn running_process_names() -> windows::core::Result<Vec<String>> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)? };
    let snapshot = unsafe { own_handle(snapshot) }.ok_or_else(windows::core::Error::from_thread)?;
    let handle = borrow_handle(&snapshot);
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut names = Vec::new();
    let mut next = unsafe { Process32FirstW(handle, &mut entry) };
    while next.is_ok() {
        names.push(from_wide(&entry.szExeFile));
        next = unsafe { Process32NextW(handle, &mut entry) };
    }
    Ok(names)
}

pub fn current_session_id() -> io::Result<u32> {
    let mut session = 0u32;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) }?;
    Ok(session)
}

pub fn current_process_in_job() -> io::Result<bool> {
    let mut result = BOOL::default();
    unsafe { IsProcessInJob(GetCurrentProcess(), None, &mut result) }?;
    Ok(result.as_bool())
}

pub fn duplicate_current_process(access: PROCESS_ACCESS_RIGHTS) -> io::Result<OwnedHandle> {
    let mut raw = HANDLE::default();
    unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            GetCurrentProcess(),
            GetCurrentProcess(),
            &mut raw,
            access.0,
            false,
            Default::default(),
        )
    }?;
    unsafe { own_handle(raw) }.ok_or_else(io::Error::last_os_error)
}

fn duplicate_inheritable(handle: &OwnedHandle) -> io::Result<OwnedHandle> {
    let mut raw = HANDLE::default();
    unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            borrow_handle(handle),
            GetCurrentProcess(),
            &mut raw,
            0,
            true,
            DUPLICATE_SAME_ACCESS,
        )
    }?;
    unsafe { own_handle(raw) }.ok_or_else(io::Error::last_os_error)
}

fn handle_value(handle: &OwnedHandle) -> usize {
    handle.as_raw_handle() as usize
}

fn raw_handle(value: usize) -> HANDLE {
    HANDLE(value as *mut c_void)
}

/// # Safety
/// `value` must be a process handle this process owns exclusively, such as one inherited at
/// creation, and must not be claimed twice.
pub unsafe fn inherited_process(value: usize) -> Option<Process> {
    if value == 0 {
        return None;
    }
    let id = unsafe { GetProcessId(raw_handle(value)) };
    if id == 0 {
        return None;
    }
    unsafe { own_handle(raw_handle(value)) }.map(|handle| Process::from_handle(handle, id))
}

/// # Safety
/// `value` must be a pipe handle this process owns exclusively, such as one inherited at
/// creation, and must not be claimed twice.
pub unsafe fn inherited_pipe(value: usize) -> Option<File> {
    if value == 0 || unsafe { GetFileType(raw_handle(value)) } != FILE_TYPE_PIPE {
        return None;
    }
    unsafe { own_handle(raw_handle(value)) }.map(File::from)
}

pub fn lock_against_replacement(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ.0)
        .open(path)
}

#[derive(Debug)]
pub struct Process {
    handle: OwnedHandle,
    id: u32,
}

impl Process {
    pub fn from_handle(handle: OwnedHandle, id: u32) -> Self {
        Self { handle, id }
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn handle(&self) -> &OwnedHandle {
        &self.handle
    }

    pub fn wait(&self, timeout: Option<Duration>) -> io::Result<bool> {
        wait_handle(&self.handle, timeout)
    }

    pub fn exit_code(&self) -> io::Result<Option<u32>> {
        if !self.wait(Some(Duration::ZERO))? {
            return Ok(None);
        }
        let mut code = 0u32;
        unsafe { GetExitCodeProcess(borrow_handle(&self.handle), &mut code) }?;
        Ok(Some(code))
    }

    pub fn terminate(&self, code: u32) -> io::Result<()> {
        unsafe { TerminateProcess(borrow_handle(&self.handle), code) }.map_err(io::Error::from)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Breakaway {
    Granted,
    Denied,
}

#[derive(Debug)]
pub struct Spawned {
    pub process: Process,
    pub breakaway: Breakaway,
}

pub fn quote_argument(argument: &str) -> String {
    if !argument.is_empty() && !argument.contains([' ', '\t', '"']) {
        return argument.to_owned();
    }
    let mut quoted = String::from('"');
    let mut backslashes = 0usize;
    for character in argument.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            other => {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                quoted.push(other);
                backslashes = 0;
            }
        }
    }
    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    quoted
}

struct AttributeList {
    buffer: Vec<u64>,
}

impl AttributeList {
    fn with_handles(handles: &[HANDLE]) -> io::Result<Self> {
        let mut size = 0usize;
        unsafe { InitializeProcThreadAttributeList(None, 1, None, &mut size) }.ok();
        let mut attributes = Self {
            buffer: vec![0u64; size.div_ceil(size_of::<u64>())],
        };
        let list = attributes.as_raw();
        unsafe { InitializeProcThreadAttributeList(Some(list), 1, None, &mut size) }?;
        if !handles.is_empty() {
            unsafe {
                UpdateProcThreadAttribute(
                    list,
                    0,
                    PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                    Some(handles.as_ptr().cast()),
                    size_of_val(handles),
                    None,
                    None,
                )
            }?;
        }
        Ok(attributes)
    }

    fn as_raw(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        LPPROC_THREAD_ATTRIBUTE_LIST(self.buffer.as_mut_ptr().cast())
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.as_raw()) };
    }
}

fn create_process(
    program: &Path,
    arguments: &[String],
    handles: &[HANDLE],
    flags: PROCESS_CREATION_FLAGS,
) -> io::Result<Process> {
    let program_text = program.to_string_lossy();
    let mut command_line = quote_argument(&program_text);
    for argument in arguments {
        command_line.push(' ');
        command_line.push_str(&quote_argument(argument));
    }
    let program_wide = wide(&program_text);
    let mut command_wide = wide(&command_line);
    let mut attributes = AttributeList::with_handles(handles)?;
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.lpAttributeList = attributes.as_raw();
    let mut information = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessW(
            PCWSTR(program_wide.as_ptr()),
            Some(PWSTR(command_wide.as_mut_ptr())),
            None,
            None,
            !handles.is_empty(),
            flags | EXTENDED_STARTUPINFO_PRESENT | CREATE_NO_WINDOW,
            None,
            PCWSTR::null(),
            &startup.StartupInfo,
            &mut information,
        )
    }?;
    drop(unsafe { own_handle(information.hThread) });
    let handle =
        unsafe { own_handle(information.hProcess) }.ok_or_else(io::Error::last_os_error)?;
    Ok(Process::from_handle(handle, information.dwProcessId))
}

pub fn spawn_with_handles(
    program: &Path,
    handles: &[&OwnedHandle],
    arguments: impl FnOnce(&[usize]) -> Vec<String>,
) -> io::Result<Spawned> {
    let _spawning = SPAWN_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let inherited = handles
        .iter()
        .map(|handle| duplicate_inheritable(handle))
        .collect::<io::Result<Vec<_>>>()?;
    let values: Vec<usize> = inherited.iter().map(handle_value).collect();
    let arguments = arguments(&values);
    let raw: Vec<HANDLE> = inherited.iter().map(borrow_handle).collect();
    match create_process(program, &arguments, &raw, CREATE_BREAKAWAY_FROM_JOB) {
        Ok(process) => Ok(Spawned {
            process,
            breakaway: Breakaway::Granted,
        }),
        Err(error) if error.raw_os_error() == Some(ERROR_ACCESS_DENIED.to_hresult().0) => {
            let process = create_process(program, &arguments, &raw, Default::default())?;
            Ok(Spawned {
                process,
                breakaway: Breakaway::Denied,
            })
        }
        Err(error) => Err(error),
    }
}

pub fn run_captured(command: &mut Command) -> io::Result<Output> {
    let child = {
        let _spawning = SPAWN_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?
    };
    child.wait_with_output()
}

#[cfg(test)]
mod tests {
    use windows::Win32::System::Threading::PROCESS_SYNCHRONIZE;

    use super::*;

    #[test]
    fn arguments_are_quoted_like_the_c_runtime_expects() {
        assert_eq!(quote_argument("--guardian"), "--guardian");
        assert_eq!(quote_argument(""), "\"\"");
        assert_eq!(
            quote_argument(r"C:\Program Files\x"),
            r#""C:\Program Files\x""#
        );
        assert_eq!(quote_argument(r#"a"b"#), r#""a\"b""#);
        assert_eq!(
            quote_argument(r"C:\dir with space\"),
            r#""C:\dir with space\\""#
        );
    }

    #[test]
    fn session_and_job_queries_succeed() {
        current_session_id().unwrap();
        current_process_in_job().unwrap();
    }

    #[test]
    fn a_running_process_has_no_exit_code() {
        let handle = duplicate_current_process(PROCESS_SYNCHRONIZE).unwrap();
        let process = Process::from_handle(handle, std::process::id());
        assert!(!process.wait(Some(Duration::ZERO)).unwrap());
    }

    #[test]
    fn inherited_handles_must_have_the_expected_type() {
        assert!(unsafe { inherited_process(0) }.is_none());
        assert!(unsafe { inherited_pipe(0) }.is_none());
        let process = duplicate_current_process(PROCESS_SYNCHRONIZE).unwrap();
        assert!(unsafe { inherited_pipe(handle_value(&process)) }.is_none());
    }

    #[test]
    fn a_locked_executable_cannot_be_renamed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("fanforge.exe");
        std::fs::write(&file, b"binary").unwrap();
        let _lock = lock_against_replacement(&file).unwrap();
        assert!(std::fs::rename(&file, dir.path().join("moved.exe")).is_err());
        assert!(std::fs::write(&file, b"replaced").is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"binary");
    }

    #[test]
    fn captured_commands_return_their_output() {
        let output = run_captured(Command::new("cmd").args(["/C", "echo", "hi"])).unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains("hi"));
    }
}
