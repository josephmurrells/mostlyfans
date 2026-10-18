use fanforge_core::board::{BoardIdentity, BoardProfile};
use fanforge_win::read_local_machine_string;

use crate::ite::{FAN_CHANNELS, TEMP_CHANNELS};

const BIOS_KEY: &str = r"HARDWARE\DESCRIPTION\System\BIOS";
const EMBEDDED_PROFILES: [&str; 1] = [include_str!("../../../profiles/asus-prime-b350-plus.toml")];

pub fn detect_identity() -> BoardIdentity {
    BoardIdentity {
        manufacturer: read_local_machine_string(BIOS_KEY, "BaseBoardManufacturer")
            .unwrap_or_default(),
        product: read_local_machine_string(BIOS_KEY, "BaseBoardProduct").unwrap_or_default(),
    }
}

pub fn embedded_profiles() -> Vec<BoardProfile> {
    EMBEDDED_PROFILES
        .iter()
        .filter_map(|text| match toml::from_str(text) {
            Ok(profile) => Some(profile),
            Err(error) => {
                tracing::error!(%error, "embedded board profile is invalid");
                None
            }
        })
        .collect()
}

pub fn select_profile(identity: &BoardIdentity, chip_id: u16) -> BoardProfile {
    embedded_profiles()
        .into_iter()
        .find(|profile| profile.matches(identity, chip_id))
        .unwrap_or_else(|| BoardProfile::generic(chip_id, FAN_CHANNELS, TEMP_CHANNELS))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_profiles_parse_and_cover_all_channels() {
        let profiles = embedded_profiles();
        assert_eq!(profiles.len(), EMBEDDED_PROFILES.len());
        let prime = &profiles[0];
        assert_eq!(prime.fans.len(), FAN_CHANNELS);
        assert_eq!(prime.temps.len(), TEMP_CHANNELS);
        assert!(!prime.verified);
        assert!(prime.fans.iter().all(|fan| fan.index < FAN_CHANNELS));
    }

    #[test]
    fn unknown_boards_get_a_read_only_generic_profile() {
        let identity = BoardIdentity {
            manufacturer: "Other".into(),
            product: "Board".into(),
        };
        let profile = select_profile(&identity, 0x8655);
        assert!(profile.read_only);
        let identity = BoardIdentity {
            manufacturer: "ASUSTeK COMPUTER INC.".into(),
            product: "PRIME B350-PLUS".into(),
        };
        assert!(!select_profile(&identity, 0x8655).read_only);
    }
}
