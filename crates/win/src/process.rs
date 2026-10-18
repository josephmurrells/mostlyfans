use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};

use crate::{borrow_handle, from_wide, own_handle};

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
