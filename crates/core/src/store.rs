use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

pub const CONFIG_BACKUPS: usize = 5;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("failed to access {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("failed to parse {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("failed to serialize: {0}")]
    Serialize(#[from] toml::ser::Error),
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> StoreError + '_ {
    move |source| StoreError::Io {
        path: path.to_owned(),
        source,
    }
}

pub fn load_toml<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, StoreError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(path)(error)),
    };
    toml::from_str(&text)
        .map(Some)
        .map_err(|source| StoreError::Parse {
            path: path.to_owned(),
            source,
        })
}

pub fn save_toml_atomic<T: Serialize>(
    path: &Path,
    value: &T,
    backups: usize,
) -> Result<(), StoreError> {
    let text = toml::to_string_pretty(value)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(io_error(parent))?;
    }
    let temp = unique_temp(path);
    let written = File::options()
        .write(true)
        .create_new(true)
        .open(&temp)
        .and_then(|mut file| {
            file.write_all(text.as_bytes())?;
            file.sync_all()
        });
    if let Err(error) = written {
        fs::remove_file(&temp).ok();
        return Err(io_error(&temp)(error));
    }
    if backups > 0 && path.exists() {
        rotate_backups(path, backups)?;
    }
    fs::rename(&temp, path).map_err(|error| {
        fs::remove_file(&temp).ok();
        io_error(path)(error)
    })
}

fn unique_temp(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    sibling(
        path,
        &format!("{}-{nanos}-{sequence}.tmp", std::process::id()),
    )
}

fn rotate_backups(path: &Path, backups: usize) -> Result<(), StoreError> {
    for index in (1..backups).rev() {
        let from = sibling(path, &index.to_string());
        if from.exists() {
            let to = sibling(path, &(index + 1).to_string());
            fs::rename(&from, &to).map_err(io_error(&from))?;
        }
    }
    let first = sibling(path, "1");
    fs::copy(path, &first).map_err(io_error(&first))?;
    Ok(())
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".");
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn missing_file_loads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let loaded: Option<Config> = load_toml(&dir.path().join("config.toml")).unwrap();
        assert!(loaded.is_none());
    }

    #[test]
    fn saves_atomically_and_keeps_bounded_backups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut config = Config::default();
        for interval in [1000, 1100, 1200, 1300] {
            config.interval_ms = interval;
            save_toml_atomic(&path, &config, 2).unwrap();
        }
        let loaded: Config = load_toml(&path).unwrap().unwrap();
        assert_eq!(loaded.interval_ms, 1300);
        let backup: Config = load_toml(&sibling(&path, "1")).unwrap().unwrap();
        assert_eq!(backup.interval_ms, 1200);
        let oldest: Config = load_toml(&sibling(&path, "2")).unwrap().unwrap();
        assert_eq!(oldest.interval_ms, 1100);
        assert!(!sibling(&path, "3").exists());
        let leftovers = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn corrupt_files_report_parse_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "interval_ms = \"fast\"").unwrap();
        assert!(matches!(
            load_toml::<Config>(&path),
            Err(StoreError::Parse { .. })
        ));
    }
}
