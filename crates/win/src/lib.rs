#![cfg(windows)]

mod directory;
mod handle;
mod process;
mod registry;
mod security;
mod token;

pub use directory::{PRIVATE_DATA_SDDL, secure_directory};
pub use handle::{borrow_handle, own_handle};
pub use process::running_process_names;
pub use registry::read_local_machine_string;
pub use security::SecurityAttributes;
pub use token::is_elevated;

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
            read_local_machine_string(r"SOFTWARE\FanForgeDoesNotExist", "Nope"),
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
        assert!(SecurityAttributes::from_sddl("not sddl").is_err());
    }
}
