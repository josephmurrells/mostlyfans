use std::ffi::CStr;
use std::io;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{FreeLibrary, HMODULE};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows::core::{HSTRING, PCSTR};

use crate::shell::system_dir;

pub type RawSymbol = unsafe extern "system" fn() -> isize;

#[derive(Debug)]
pub struct SystemLibrary {
    module: HMODULE,
    path: PathBuf,
}

unsafe impl Send for SystemLibrary {}
unsafe impl Sync for SystemLibrary {}

impl SystemLibrary {
    pub fn load(name: &str) -> io::Result<Self> {
        let path = system_dir()?.join(name);
        let module = unsafe {
            LoadLibraryExW(
                &HSTRING::from(path.as_path()),
                None,
                LOAD_LIBRARY_SEARCH_SYSTEM32,
            )
        }?;
        Ok(Self { module, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn symbol(&self, name: &CStr) -> Option<RawSymbol> {
        unsafe { GetProcAddress(self.module, PCSTR(name.as_ptr().cast())) }
    }
}

impl Drop for SystemLibrary {
    fn drop(&mut self) {
        unsafe { FreeLibrary(self.module) }.ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_system_libraries_and_resolves_symbols() {
        let library = SystemLibrary::load("kernel32.dll").unwrap();
        assert!(library.path().ends_with("kernel32.dll"));
        assert!(library.symbol(c"GetTickCount64").is_some());
        assert!(library.symbol(c"MostlyFansDoesNotExist").is_none());
    }

    #[test]
    fn missing_libraries_fail_to_load() {
        assert!(SystemLibrary::load("mostlyfans-missing.dll").is_err());
    }
}
