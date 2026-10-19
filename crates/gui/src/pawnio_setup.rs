use std::fmt::Write as _;
use std::fs;
use std::io;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;

use eframe::egui;
use fanforge_hw::pawnio;
use fanforge_win::paths;
use sha2::{Digest, Sha256};

use crate::alerts::{Action, Alert, Severity};

pub const VERSION: &str = "2.2.0";
const URL: &str =
    "https://github.com/namazso/PawnIO.Setup/releases/download/2.2.0/PawnIO_setup.exe";
const SHA256: &str = "1f519a22e47187f70a1379a48ca604981c4fcf694f4e65b734aaa74a9fba3032";
const FILE_NAME: &str = "PawnIO_setup.exe";
const DOWNLOAD_TIMEOUT_SECS: &str = "180";
const REBOOT_REQUIRED: i32 = 3010;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Installed {
    Ready,
    RestartRequired,
}

fn installer_path() -> PathBuf {
    paths::data_dir().join("downloads").join(FILE_NAME)
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

fn download(target: &Path) -> io::Result<()> {
    if let Some(directory) = target.parent() {
        fs::create_dir_all(directory)?;
    }
    remove_if_present(target)?;
    let output = fanforge_win::run_captured(
        Command::new(fanforge_win::system_dir()?.join("curl.exe"))
            .args([
                "--fail",
                "--location",
                "--silent",
                "--show-error",
                "--proto",
                "=https",
                "--proto-redir",
                "=https",
                "--max-time",
                DOWNLOAD_TIMEOUT_SECS,
                "--output",
            ])
            .arg(target)
            .arg(URL)
            .creation_flags(CREATE_NO_WINDOW),
    )?;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "the download failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

fn verify(target: &Path) -> io::Result<()> {
    if sha256_hex(&fs::read(target)?) == SHA256 {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the downloaded installer does not match the official PawnIO release, so it was not run",
        ))
    }
}

fn run_installer(target: &Path) -> io::Result<Installed> {
    let output = fanforge_win::run_captured(
        Command::new(target)
            .args(["-install", "-silent"])
            .creation_flags(CREATE_NO_WINDOW),
    )?;
    match output.status.code() {
        Some(0) if pawnio::driver_present() => Ok(Installed::Ready),
        Some(0 | REBOOT_REQUIRED) => Ok(Installed::RestartRequired),
        code => Err(io::Error::other(format!(
            "the PawnIO installer failed (exit code {})",
            code.map_or_else(|| "unknown".to_owned(), |code| code.to_string())
        ))),
    }
}

pub fn install() -> io::Result<Installed> {
    let target = installer_path();
    tracing::info!(version = VERSION, "installing PawnIO");
    let result = download(&target)
        .and_then(|()| verify(&target))
        .and_then(|()| run_installer(&target));
    if let Err(error) = remove_if_present(&target) {
        tracing::warn!(%error, "could not delete the PawnIO installer");
    }
    match &result {
        Ok(installed) => tracing::info!(?installed, "PawnIO installed"),
        Err(error) => tracing::error!(%error, "PawnIO install failed"),
    }
    result
}

pub type OnInstalled = Arc<dyn Fn() + Send + Sync>;

pub struct PawnIoPanel {
    pending: Option<Receiver<io::Result<Installed>>>,
    outcome: Option<Result<Installed, String>>,
    on_installed: OnInstalled,
}

impl PawnIoPanel {
    pub fn new(on_installed: OnInstalled) -> Self {
        Self {
            pending: None,
            outcome: None,
            on_installed,
        }
    }

    pub fn start(&mut self, ctx: &egui::Context) {
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let ctx = ctx.clone();
        let on_installed = Arc::clone(&self.on_installed);
        let spawned = thread::Builder::new()
            .name("pawnio-setup".into())
            .spawn(move || {
                let result = install();
                if matches!(result, Ok(Installed::Ready)) {
                    on_installed();
                }
                done_tx.send(result).ok();
                ctx.request_repaint();
            });
        match spawned {
            Ok(_) => {
                self.outcome = None;
                self.pending = Some(done_rx);
            }
            Err(error) => self.outcome = Some(Err(error.to_string())),
        }
    }

    fn poll(&mut self) {
        let Some(pending) = &self.pending else {
            return;
        };
        let outcome = match pending.try_recv() {
            Ok(result) => result.map_err(|error| error.to_string()),
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("the installer stopped unexpectedly".to_owned()),
        };
        self.outcome = Some(outcome);
        self.pending = None;
    }

    pub fn alert(&mut self, driver_missing: bool) -> Option<Alert> {
        self.poll();
        if self.pending.is_some() {
            return Some(
                Alert::new(
                    Severity::Driver,
                    format!("Downloading and installing PawnIO {VERSION}…"),
                )
                .busy(),
            );
        }
        if self.outcome == Some(Ok(Installed::RestartRequired)) {
            return Some(Alert::new(
                Severity::Driver,
                "PawnIO is installed. Restart Windows to finish, then start FanForge again.",
            ));
        }
        if !driver_missing {
            return None;
        }
        let detail = match &self.outcome {
            Some(Err(error)) => format!("The last install failed: {error}"),
            _ => format!(
                "Downloads the official PawnIO {VERSION} installer and runs it only if its SHA-256 checksum matches."
            ),
        };
        Some(
            Alert::new(
                Severity::Driver,
                "The PawnIO driver is not installed. FanForge needs it to reach the fan controller.",
            )
            .detail(detail)
            .action(Action::InstallPawnIo),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_are_lowercase_hex() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(SHA256.len(), 64);
    }

    #[test]
    fn a_tampered_installer_is_never_run() {
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join(FILE_NAME);
        fs::write(&fake, b"not the real installer").unwrap();
        let error = verify(&fake).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn the_download_is_pinned_to_the_official_release() {
        assert!(URL.starts_with("https://github.com/namazso/PawnIO.Setup/releases/download/"));
        assert!(URL.contains(VERSION));
    }
}
