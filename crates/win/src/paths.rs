use std::path::PathBuf;

use windows::Win32::UI::Shell::FOLDERID_ProgramData;

use crate::shell::known_folder;

pub const APP_NAME: &str = "FanForge";
pub const INSTANCE_MUTEX: &str = r"Global\FanForge.Instance";
const FALLBACK_PROGRAM_DATA: &str = r"C:\ProgramData";

pub fn data_dir() -> PathBuf {
    known_folder(&FOLDERID_ProgramData)
        .unwrap_or_else(|_| PathBuf::from(FALLBACK_PROGRAM_DATA))
        .join(APP_NAME)
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.toml")
}

pub fn bios_snapshot_path() -> PathBuf {
    data_dir().join("bios_snapshot.toml")
}

pub fn log_dir() -> PathBuf {
    data_dir().join("logs")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_lives_under_program_data_whatever_the_environment_says() {
        let dir = data_dir();
        assert!(dir.is_absolute());
        assert!(dir.ends_with(APP_NAME));
        assert!(config_path().starts_with(&dir));
    }
}
