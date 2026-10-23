use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use mostlyfans_win::paths::{self, APP_NAME};

pub const TASK_NAME: &str = "MostlyFans";
pub const EXECUTABLE_NAME: &str = "mostlyfans.exe";
pub const MINIMIZED_FLAG: &str = "--minimized";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const TASK_FILE: &str = "autostart-task.xml";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutostartState {
    Unavailable(String),
    Off,
    On,
    Mismatch(PathBuf),
}

pub fn managed_executable() -> io::Result<PathBuf> {
    Ok(mostlyfans_win::program_files_dir()?
        .join(APP_NAME)
        .join(EXECUTABLE_NAME))
}

fn normalized(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .to_lowercase()
}

pub fn same_path(first: &Path, second: &Path) -> bool {
    normalized(first) == normalized(second)
}

fn files_match(first: &Path, second: &Path) -> io::Result<bool> {
    if fs::metadata(first)?.len() != fs::metadata(second)?.len() {
        return Ok(false);
    }
    Ok(fs::read(first)? == fs::read(second)?)
}

fn refresh_copy(current: &Path, managed: &Path) -> io::Result<bool> {
    if same_path(current, managed) || (managed.exists() && files_match(current, managed)?) {
        return Ok(false);
    }
    if let Some(directory) = managed.parent() {
        fs::create_dir_all(directory)?;
    }
    fs::copy(current, managed)?;
    Ok(true)
}

fn remove_copy(current: &Path, managed: &Path) -> io::Result<()> {
    if same_path(current, managed) {
        mostlyfans_win::delete_at_reboot(managed)?;
    } else {
        match fs::remove_file(managed) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
    }
    if let Some(directory) = managed.parent() {
        fs::remove_dir(directory).ok();
    }
    Ok(())
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

pub fn task_xml(user_sid: &str, executable: &Path) -> String {
    let user = escape(user_sid);
    let command = escape(&executable.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Starts MostlyFans fan control when you sign in.</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{user}</UserId>
      <Delay>PT10S</Delay>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
    <Enabled>true</Enabled>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <Arguments>{MINIMIZED_FLAG}</Arguments>
    </Exec>
  </Actions>
</Task>
"#
    )
}

pub fn command_from_xml(xml: &str) -> Option<PathBuf> {
    let start = xml.find("<Command>")? + "<Command>".len();
    let end = start + xml[start..].find("</Command>")?;
    let command = unescape(xml[start..end].trim());
    Some(PathBuf::from(command.trim_matches('"')))
}

fn utf16_with_bom(text: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
    bytes
}

fn looks_like_utf16(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0xFE])
        || (bytes.len() >= 2
            && bytes
                .iter()
                .skip(1)
                .step_by(2)
                .take(16)
                .all(|&byte| byte == 0))
}

fn decode_output(bytes: &[u8]) -> String {
    if looks_like_utf16(bytes) {
        let (pairs, _) = bytes.as_chunks::<2>();
        let units: Vec<u16> = pairs.iter().map(|pair| u16::from_le_bytes(*pair)).collect();
        String::from_utf16_lossy(&units)
            .trim_start_matches('\u{feff}')
            .to_owned()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

fn schtasks<I, S>(arguments: I) -> io::Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    mostlyfans_win::run_captured(
        Command::new(mostlyfans_win::system_dir()?.join("schtasks.exe"))
            .args(arguments)
            .creation_flags(CREATE_NO_WINDOW),
    )
}

fn succeeded(output: Output) -> io::Result<()> {
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(
            decode_output(&output.stderr).trim().to_owned(),
        ))
    }
}

fn register(executable: &Path) -> io::Result<()> {
    let xml = task_xml(&mostlyfans_win::current_user_sid()?, executable);
    let file = paths::data_dir().join(TASK_FILE);
    fs::write(&file, utf16_with_bom(&xml))?;
    let result = schtasks([
        OsStr::new("/Create"),
        OsStr::new("/TN"),
        OsStr::new(TASK_NAME),
        OsStr::new("/XML"),
        file.as_os_str(),
        OsStr::new("/F"),
    ])
    .and_then(succeeded);
    fs::remove_file(&file).ok();
    result
}

fn unregister() -> io::Result<()> {
    if registered_command()?.is_none() {
        return Ok(());
    }
    schtasks(["/Delete", "/TN", TASK_NAME, "/F"]).and_then(succeeded)
}

pub fn registered_command() -> io::Result<Option<PathBuf>> {
    let output = schtasks(["/Query", "/TN", TASK_NAME, "/XML", "ONE"])?;
    if !output.status.success() {
        return Ok(None);
    }
    Ok(command_from_xml(&decode_output(&output.stdout)))
}

pub fn enable() -> io::Result<()> {
    let managed = managed_executable()?;
    refresh_copy(&env::current_exe()?, &managed)?;
    register(&managed)
}

pub fn disable() -> io::Result<()> {
    unregister()?;
    remove_copy(&env::current_exe()?, &managed_executable()?)
}

pub fn sync() -> AutostartState {
    let state = state();
    if state == AutostartState::On {
        let refreshed =
            env::current_exe().and_then(|current| refresh_copy(&current, &managed_executable()?));
        match refreshed {
            Ok(true) => tracing::info!("updated the autostart copy to this version"),
            Ok(false) => {}
            Err(error) => tracing::warn!(%error, "could not update the autostart copy"),
        }
    }
    state
}

pub fn state() -> AutostartState {
    let managed = match managed_executable() {
        Ok(managed) => managed,
        Err(error) => return AutostartState::Unavailable(error.to_string()),
    };
    match registered_command() {
        Ok(None) => AutostartState::Off,
        Ok(Some(command)) if same_path(&command, &managed) => AutostartState::On,
        Ok(Some(command)) => AutostartState::Mismatch(command),
        Err(error) => AutostartState::Unavailable(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_task_never_expires_and_ignores_batteries() {
        let xml = task_xml(
            "S-1-5-21-1-2-3-1001",
            Path::new(r"C:\Program Files\MostlyFans\mostlyfans.exe"),
        );
        for expected in [
            "<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>",
            "<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>",
            "<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>",
            "<RunLevel>HighestAvailable</RunLevel>",
            "<LogonType>InteractiveToken</LogonType>",
            "<Delay>PT10S</Delay>",
            "<Priority>4</Priority>",
            "<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>",
            "<UserId>S-1-5-21-1-2-3-1001</UserId>",
            "<Arguments>--minimized</Arguments>",
        ] {
            assert!(xml.contains(expected), "missing {expected}");
        }
        assert_eq!(
            command_from_xml(&xml),
            Some(PathBuf::from(r"C:\Program Files\MostlyFans\mostlyfans.exe"))
        );
    }

    #[test]
    fn markup_in_values_is_escaped() {
        let xml = task_xml("a<b", Path::new(r"C:\x&y\<z>.exe"));
        assert!(xml.contains("<UserId>a&lt;b</UserId>"));
        assert_eq!(
            command_from_xml(&xml),
            Some(PathBuf::from(r"C:\x&y\<z>.exe"))
        );
    }

    #[test]
    fn quoted_commands_are_read_back() {
        let xml = r#"<Exec><Command>"C:\Program Files\MostlyFans\mostlyfans.exe"</Command></Exec>"#;
        assert_eq!(
            command_from_xml(xml),
            Some(PathBuf::from(r"C:\Program Files\MostlyFans\mostlyfans.exe"))
        );
        assert_eq!(command_from_xml("<Task/>"), None);
    }

    #[test]
    fn paths_compare_case_insensitively() {
        assert!(same_path(
            Path::new(r"C:\Program Files\MostlyFans\mostlyfans.exe"),
            Path::new(r"c:\program files\mostlyfans\MOSTLYFANS.EXE")
        ));
        assert!(!same_path(
            Path::new(r"C:\Program Files\MostlyFans\mostlyfans.exe"),
            Path::new(r"C:\Users\me\Downloads\mostlyfans.exe")
        ));
    }

    #[test]
    fn the_protected_copy_follows_the_running_exe() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("portable").join(EXECUTABLE_NAME);
        let managed = dir
            .path()
            .join("Program Files")
            .join(APP_NAME)
            .join(EXECUTABLE_NAME);
        fs::create_dir_all(current.parent().unwrap()).unwrap();
        fs::write(&current, b"version 1").unwrap();
        assert!(refresh_copy(&current, &managed).unwrap());
        assert!(!refresh_copy(&current, &managed).unwrap());
        fs::write(&current, b"version 2").unwrap();
        assert!(refresh_copy(&current, &managed).unwrap());
        assert_eq!(fs::read(&managed).unwrap(), b"version 2");
        assert!(!refresh_copy(&managed, &managed).unwrap());
        remove_copy(&current, &managed).unwrap();
        assert!(!managed.exists());
        assert!(!managed.parent().unwrap().exists());
        remove_copy(&current, &managed).unwrap();
    }

    #[test]
    fn the_task_targets_the_protected_copy() {
        let managed = managed_executable().unwrap();
        assert!(managed.starts_with(mostlyfans_win::program_files_dir().unwrap()));
        assert!(managed.ends_with(Path::new(APP_NAME).join(EXECUTABLE_NAME)));
    }

    #[test]
    fn utf16_output_is_decoded() {
        let encoded = utf16_with_bom("<Command>C:\\a.exe</Command>");
        assert_eq!(decode_output(&encoded), "<Command>C:\\a.exe</Command>");
        assert_eq!(decode_output(b"plain"), "plain");
    }
}
