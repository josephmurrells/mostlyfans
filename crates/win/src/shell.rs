use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::Win32::Storage::FileSystem::{MOVEFILE_DELAY_UNTIL_REBOOT, MoveFileExW};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::Win32::UI::Shell::{FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
use windows::core::{GUID, PCWSTR};

pub(crate) fn known_folder(id: &GUID) -> io::Result<PathBuf> {
    let raw = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None) }?;
    let path = unsafe { raw.to_string() }.map_err(io::Error::other);
    unsafe { CoTaskMemFree(Some(raw.0.cast_const().cast())) };
    path.map(PathBuf::from)
}

pub fn delete_at_reboot(path: &Path) -> io::Result<()> {
    let wide = wide_path(path);
    unsafe {
        MoveFileExW(
            PCWSTR(wide.as_ptr()),
            PCWSTR::null(),
            MOVEFILE_DELAY_UNTIL_REBOOT,
        )
    }?;
    Ok(())
}

pub fn system_dir() -> io::Result<PathBuf> {
    let mut buffer = [0u16; 260];
    let len = unsafe { GetSystemDirectoryW(Some(&mut buffer)) } as usize;
    if len == 0 || len >= buffer.len() {
        return Err(io::Error::last_os_error());
    }
    Ok(PathBuf::from(String::from_utf16_lossy(&buffer[..len])))
}

pub fn program_files_dir() -> io::Result<PathBuf> {
    known_folder(&FOLDERID_ProgramFiles)
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_folders_resolve() {
        assert!(program_files_dir().unwrap().is_absolute());
        assert!(system_dir().unwrap().join("schtasks.exe").is_file());
    }
}
