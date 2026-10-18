use std::env;
use std::path::PathBuf;

pub const SERVICE_NAME: &str = "FanForge";
pub const PIPE_NAME: &str = r"\\.\pipe\fanforge";

pub fn data_dir() -> PathBuf {
    env::var_os("ProgramData")
        .map_or_else(|| PathBuf::from(r"C:\ProgramData"), PathBuf::from)
        .join(SERVICE_NAME)
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
