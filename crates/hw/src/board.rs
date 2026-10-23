use mostlyfans_core::board::BoardIdentity;
use mostlyfans_win::read_local_machine_string;

const BIOS_KEY: &str = r"HARDWARE\DESCRIPTION\System\BIOS";

pub fn detect_identity() -> BoardIdentity {
    BoardIdentity {
        manufacturer: read_local_machine_string(BIOS_KEY, "BaseBoardManufacturer")
            .unwrap_or_default(),
        product: read_local_machine_string(BIOS_KEY, "BaseBoardProduct").unwrap_or_default(),
    }
}
