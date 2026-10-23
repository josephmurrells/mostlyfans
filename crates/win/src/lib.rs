#![cfg(windows)]

mod dialog;
mod directory;
mod handle;
mod integrity;
mod library;
pub mod paths;
mod pipe;
mod power;
mod process;
mod registry;
mod security;
mod session;
mod shell;
pub mod storage;
mod sync;
mod token;
pub mod topology;

pub use dialog::{DialogKind, message_box};
pub use directory::{PRIVATE_DATA_SDDL, secure_directory};
pub use handle::{borrow_handle, own_handle};
pub use library::{RawSymbol, SystemLibrary};
pub use pipe::{AnonymousPipe, nonblocking_writer};
pub use power::{PowerEvent, PowerSubscription, subscribe_power, unbiased_time};
pub use process::{
    Breakaway, Process, Spawned, current_process_in_job, current_session_id,
    duplicate_current_process, inherited_pipe, inherited_process, lock_against_replacement,
    quote_argument, run_captured, running_process_names, spawn_with_handles,
};
pub use registry::read_local_machine_string;
pub use security::SecurityAttributes;
pub use session::{SessionEvent, spawn_session_window};
pub use shell::{delete_at_reboot, program_files_dir, system_dir};
pub use sync::{ADMIN_ONLY_SDDL, Event, InstanceMutex};
pub use token::{current_user_sid, is_elevated};
pub use windows::Win32::System::Threading::{
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};

pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn from_wide(buffer: &[u16]) -> String {
    let len = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_strings_round_trip() {
        let encoded = wide(r"Global\Access_PCI");
        assert_eq!(encoded.last(), Some(&0));
        assert_eq!(from_wide(&encoded), r"Global\Access_PCI");
    }

    #[test]
    fn process_list_contains_this_process() {
        let names = running_process_names().unwrap();
        assert!(!names.is_empty());
    }

    #[test]
    fn missing_registry_values_are_none() {
        assert_eq!(
            read_local_machine_string(r"SOFTWARE\MostlyFansDoesNotExist", "Nope"),
            None
        );
        assert!(
            read_local_machine_string(r"HARDWARE\DESCRIPTION\System\BIOS", "BaseBoardProduct")
                .is_some()
        );
    }

    #[test]
    fn elevation_query_succeeds() {
        assert!(is_elevated().is_ok());
    }

    #[test]
    fn sddl_parses() {
        assert!(SecurityAttributes::from_sddl("D:(A;;GA;;;SY)").is_ok());
        assert!(SecurityAttributes::from_sddl(ADMIN_ONLY_SDDL).is_ok());
        assert!(SecurityAttributes::from_sddl("not sddl").is_err());
    }
}
