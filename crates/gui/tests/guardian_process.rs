use std::fs::File;
use std::io::Write;
use std::os::windows::io::AsHandle;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use fanforge_control::guardian::Signal;
use fanforge_win::{AnonymousPipe, Process};

const GUARDIAN_WAIT: Duration = Duration::from_secs(10);

struct Guardian {
    process: Process,
    writer: Option<File>,
    parent: Child,
    marker: PathBuf,
    _dir: tempfile::TempDir,
}

fn dummy_parent() -> Child {
    Command::new(
        fanforge_win::system_dir()
            .expect("system directory")
            .join("ping.exe"),
    )
    .args(["-n", "120", "127.0.0.1"])
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .expect("dummy parent starts")
}

fn start_guardian() -> Guardian {
    let dir = tempfile::tempdir().expect("temporary directory");
    let marker = dir.path().join("released.marker");
    let parent = dummy_parent();
    let parent_handle = parent
        .as_handle()
        .try_clone_to_owned()
        .expect("parent handle");
    let pipe = AnonymousPipe::new().expect("pipe");
    let marker_argument = marker.to_string_lossy().into_owned();
    let spawned = fanforge_win::spawn_with_handles(
        Path::new(env!("CARGO_BIN_EXE_fanforge")),
        &[&parent_handle, &pipe.read],
        |values| {
            vec![
                "--guardian".to_owned(),
                "--parent-handle".to_owned(),
                values[0].to_string(),
                "--pipe-handle".to_owned(),
                values[1].to_string(),
                "--dry-run".to_owned(),
                marker_argument,
            ]
        },
    )
    .expect("guardian starts");
    drop(pipe.read);
    let mut writer = File::from(pipe.write);
    for signal in [
        Signal::Heartbeat {
            interval: Duration::from_secs(1),
        },
        Signal::Owning(true),
    ] {
        writer
            .write_all(&[signal.encode()])
            .expect("guardian pipe accepts signals");
    }
    Guardian {
        process: spawned.process,
        writer: Some(writer),
        parent,
        marker,
        _dir: dir,
    }
}

#[test]
fn a_killed_parent_has_its_fans_released() {
    let mut guardian = start_guardian();
    guardian.parent.kill().unwrap();
    guardian.parent.wait().unwrap();
    assert!(guardian.process.wait(Some(GUARDIAN_WAIT)).unwrap());
    assert_eq!(guardian.process.exit_code().unwrap(), Some(0));
    assert!(guardian.marker.exists());
    drop(guardian.writer.take());
}

#[test]
fn a_clean_exit_leaves_the_fans_alone() {
    let mut guardian = start_guardian();
    let mut writer = guardian.writer.take().unwrap();
    writer.write_all(&[Signal::Released.encode()]).unwrap();
    drop(writer);
    assert!(guardian.process.wait(Some(GUARDIAN_WAIT)).unwrap());
    assert_eq!(guardian.process.exit_code().unwrap(), Some(0));
    assert!(!guardian.marker.exists());
    guardian.parent.kill().ok();
    guardian.parent.wait().ok();
}

#[test]
fn a_closed_link_without_released_counts_as_a_crash() {
    let mut guardian = start_guardian();
    drop(guardian.writer.take());
    assert!(guardian.process.wait(Some(GUARDIAN_WAIT)).unwrap());
    assert!(guardian.marker.exists());
    assert!(guardian.parent.try_wait().unwrap().is_some());
}
