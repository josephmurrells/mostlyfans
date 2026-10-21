// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Derived from LibreHardwareMonitor (https://github.com/LibreHardwareMonitor/LibreHardwareMonitor).

#[derive(Debug, PartialEq, Eq)]
pub struct ChipSpec {
    pub name: &'static str,
    pub fan_control: bool,
    pub ident: Ident,
    pub map: ChipMap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ident {
    Ite(u16),
    Nuvoton { id: u8, revision: u8, mask: u8 },
}

#[derive(Debug, PartialEq, Eq)]
pub enum ChipMap {
    Ite(IteMap),
    Nuvoton(NctMap),
    Ec(EcMap),
}

#[derive(Debug, PartialEq, Eq)]
pub struct EcMap {
    pub fans: usize,
    pub temps: &'static [&'static str],
    pub done_command: u8,
}

#[derive(Debug, PartialEq, Eq)]
pub struct IteMap {
    pub fans: usize,
    pub temps: usize,
    pub pwm_control: [u8; 6],
    pub tach_low: [u8; 6],
    pub tach_high: [u8; 6],
    pub tach_enable: [Option<(u8, u8)>; 6],
    pub bank_select: bool,
    pub dump_banks: u8,
    pub index_echo: bool,
    pub config_check: bool,
    pub manual_control: Option<u8>,
    pub polarity_check: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct NctMap {
    pub fans: usize,
    pub tach: Tach,
    pub slots: &'static [TempSlot],
    pub sources: &'static [TempSource],
    pub io_lock: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Tach {
    Rpm {
        registers: &'static [u16],
        min_rpm: u16,
    },
    Count {
        registers: &'static [u16],
    },
}

#[derive(Debug, PartialEq, Eq)]
pub struct TempSlot {
    pub register: u16,
    pub half: Option<(u16, u8)>,
    pub source: u16,
}

#[derive(Debug, PartialEq, Eq)]
pub struct TempSource {
    pub id: u8,
    pub name: &'static str,
    pub alternate: Option<u16>,
}

impl ChipSpec {
    pub fn fan_count(&self) -> usize {
        match &self.map {
            ChipMap::Ite(map) => map.fans,
            ChipMap::Nuvoton(map) => map.fans,
            ChipMap::Ec(map) => map.fans,
        }
    }

    pub fn temp_names(&self) -> Vec<Option<&'static str>> {
        match &self.map {
            ChipMap::Ite(map) => vec![None; map.temps],
            ChipMap::Nuvoton(map) => map.sources.iter().map(|source| Some(source.name)).collect(),
            ChipMap::Ec(map) => map.temps.iter().copied().map(Some).collect(),
        }
    }

    pub fn forces_auto_without_snapshot(&self) -> bool {
        matches!(&self.map, ChipMap::Ite(map) if map.manual_control.is_none())
    }

    pub fn manual_control(&self) -> Option<u8> {
        match &self.map {
            ChipMap::Ite(map) => map.manual_control,
            ChipMap::Nuvoton(_) | ChipMap::Ec(_) => None,
        }
    }

    pub fn matches_ite(&self, chip_id: u16) -> bool {
        self.ident == Ident::Ite(chip_id)
    }

    pub fn matches_nuvoton(&self, chip_id: u8, chip_revision: u8) -> bool {
        matches!(
            self.ident,
            Ident::Nuvoton { id, revision, mask } if id == chip_id && chip_revision & mask == revision
        )
    }
}

const PWM_CONTROL: [u8; 6] = [0x15, 0x16, 0x17, 0x7F, 0xA7, 0xAF];
const PWM_CONTROL_ALT: [u8; 6] = [0x15, 0x16, 0x17, 0x1E, 0x1F, 0x92];
const TACH_LOW: [u8; 6] = [0x0D, 0x0E, 0x0F, 0x80, 0x82, 0x4C];
const TACH_HIGH: [u8; 6] = [0x18, 0x19, 0x1A, 0x81, 0x83, 0x4D];
const TACH_LOW_ALT: [u8; 6] = [0x0D, 0x0E, 0x0F, 0x80, 0x82, 0x93];
const TACH_HIGH_ALT: [u8; 6] = [0x18, 0x19, 0x1A, 0x81, 0x83, 0x94];
const TACH_ENABLE: [Option<(u8, u8)>; 6] = [
    None,
    None,
    None,
    Some((0x0C, 4)),
    Some((0x0C, 5)),
    Some((0x0C, 2)),
];
const TACH_ENABLE_8665: [Option<(u8, u8)>; 6] = [
    None,
    None,
    None,
    Some((0x0C, 4)),
    Some((0x0C, 5)),
    Some((0x0B, 3)),
];

const ITE: IteMap = IteMap {
    fans: 3,
    temps: 3,
    pwm_control: PWM_CONTROL,
    tach_low: TACH_LOW,
    tach_high: TACH_HIGH,
    tach_enable: TACH_ENABLE,
    bank_select: false,
    dump_banks: 1,
    index_echo: true,
    config_check: true,
    manual_control: None,
    polarity_check: true,
};

const fn ite(name: &'static str, chip_id: u16, map: IteMap) -> ChipSpec {
    ChipSpec {
        name,
        fan_control: true,
        ident: Ident::Ite(chip_id),
        map: ChipMap::Ite(map),
    }
}

const COUNT_TACH: Tach = Tach::Count {
    registers: &[0x4B0, 0x4B2, 0x4B4, 0x4B6, 0x4B8, 0x4BA, 0x4CC],
};

const fn slot(register: u16, half: Option<(u16, u8)>, source: u16) -> TempSlot {
    TempSlot {
        register,
        half,
        source,
    }
}

const fn source(id: u8, name: &'static str, alternate: Option<u16>) -> TempSource {
    TempSource {
        id,
        name,
        alternate,
    }
}

const SLOTS_6776: &[TempSlot] = &[
    slot(0x027, None, 0x621),
    slot(0x073, Some((0x074, 7)), 0x100),
    slot(0x075, Some((0x076, 7)), 0x200),
    slot(0x077, Some((0x078, 7)), 0x300),
    slot(0x150, Some((0x151, 7)), 0x622),
    slot(0x250, Some((0x251, 7)), 0x623),
    slot(0x62B, Some((0x62E, 0)), 0x624),
    slot(0x62C, Some((0x62E, 1)), 0x625),
    slot(0x62D, Some((0x62E, 2)), 0x626),
];

const SOURCES_6771: &[TempSource] = &[
    source(1, "SYSTIN", None),
    source(2, "CPUTIN", None),
    source(3, "AUXTIN", None),
    source(5, "PECI 0", None),
    source(4, "AMD SB-TSI", None),
    source(6, "PECI 1", None),
    source(13, "PCH CPU max", None),
    source(14, "PCH chip", None),
    source(15, "PCH CPU", None),
    source(16, "PCH MCH", None),
];

const SOURCES_6776: &[TempSource] = &[
    source(1, "SYSTIN", None),
    source(2, "CPUTIN", None),
    source(3, "AUXTIN", None),
    source(12, "PECI 0", None),
    source(4, "SMBUSMASTER 0", None),
    source(5, "SMBUSMASTER 1", None),
    source(13, "PECI 1", None),
    source(14, "PCH CPU max", None),
    source(15, "PCH chip", None),
    source(16, "PCH CPU", None),
    source(17, "PCH MCH", None),
];

const SLOTS_6779: &[TempSlot] = &[
    slot(0x027, None, 0x621),
    slot(0x073, Some((0x074, 7)), 0x100),
    slot(0x075, Some((0x076, 7)), 0x200),
    slot(0x077, Some((0x078, 7)), 0x300),
    slot(0x079, Some((0x07A, 7)), 0x800),
    slot(0x07B, Some((0x07C, 7)), 0x900),
    slot(0x150, Some((0x151, 7)), 0x622),
];

const SLOTS_6791: &[TempSlot] = &[
    slot(0x073, Some((0x074, 7)), 0x100),
    slot(0x075, Some((0x076, 7)), 0x200),
    slot(0x077, Some((0x078, 7)), 0x300),
    slot(0x079, Some((0x07A, 7)), 0x800),
    slot(0x07B, Some((0x07C, 7)), 0x900),
    slot(0x07D, Some((0x07E, 7)), 0xA00),
    slot(0x4A0, Some((0x49E, 6)), 0xB00),
    slot(0x027, None, 0x621),
    slot(0x150, Some((0x151, 7)), 0x622),
];

const SLOTS_6798: &[TempSlot] = &[
    slot(0x073, Some((0x074, 7)), 0x100),
    slot(0x075, Some((0x076, 7)), 0x200),
    slot(0x077, Some((0x078, 7)), 0x300),
    slot(0x079, Some((0x07A, 7)), 0x800),
    slot(0x07B, Some((0x07C, 7)), 0x900),
    slot(0x07D, Some((0x07E, 7)), 0xA00),
    slot(0x4A0, Some((0x49E, 6)), 0xB00),
    slot(0x027, None, 0x621),
    slot(0x4A2, Some((0x4A1, 7)), 0xC00),
    slot(0x150, Some((0x151, 7)), 0x622),
];

const SOURCES_6779: &[TempSource] = &[
    source(1, "SYSTIN", Some(0x490)),
    source(2, "CPUTIN", Some(0x491)),
    source(3, "AUXTIN0", Some(0x492)),
    source(4, "AUXTIN1", Some(0x493)),
    source(5, "AUXTIN2", Some(0x494)),
    source(6, "AUXTIN3", Some(0x495)),
    source(16, "PECI 0", None),
    source(8, "SMBUSMASTER 0", None),
    source(9, "SMBUSMASTER 1", None),
    source(17, "PECI 1", None),
    source(18, "PCH CPU max", None),
    source(19, "PCH chip", None),
    source(20, "PCH CPU", None),
    source(21, "PCH MCH", None),
    source(22, "DIMM 0", None),
    source(23, "DIMM 1", None),
    source(24, "DIMM 2", None),
    source(25, "DIMM 3", None),
];

const SOURCES_6796: &[TempSource] = &[
    source(1, "SYSTIN", Some(0x490)),
    source(2, "CPUTIN", Some(0x491)),
    source(3, "AUXTIN0", Some(0x492)),
    source(4, "AUXTIN1", Some(0x493)),
    source(5, "AUXTIN2", Some(0x494)),
    source(6, "AUXTIN3", Some(0x495)),
    source(7, "AUXTIN4", None),
    source(16, "PECI 0", None),
    source(8, "SMBUSMASTER 0", None),
    source(9, "SMBUSMASTER 1", None),
    source(17, "PECI 1", None),
    source(18, "PCH CPU max", None),
    source(19, "PCH chip", None),
    source(20, "PCH CPU", None),
    source(21, "PCH MCH", None),
    source(22, "DIMM 0", None),
    source(23, "DIMM 1", None),
    source(24, "DIMM 2", None),
    source(25, "DIMM 3", None),
];

const SOURCES_6798: &[TempSource] = &[
    source(1, "SYSTIN", Some(0x490)),
    source(2, "CPUTIN", Some(0x491)),
    source(3, "AUXTIN0", Some(0x492)),
    source(4, "AUXTIN1", Some(0x493)),
    source(5, "AUXTIN2", Some(0x494)),
    source(6, "AUXTIN3", Some(0x495)),
    source(7, "AUXTIN4", None),
    source(10, "TSENSOR", Some(0x496)),
    source(16, "PECI 0", None),
    source(8, "SMBUSMASTER 0", None),
    source(9, "SMBUSMASTER 1", None),
    source(17, "PECI 1", None),
    source(18, "PCH CPU max", None),
    source(19, "PCH chip", None),
    source(20, "PCH CPU", None),
    source(21, "PCH MCH", None),
    source(22, "DIMM 0", None),
    source(23, "DIMM 1", None),
    source(24, "DIMM 2", None),
    source(25, "DIMM 3", None),
];

const EC_DONE: u8 = 0x40;
const EC_TEMPS: &[&str] = &[
    "CPU",
    "System",
    "VRM MOS",
    "PCH",
    "CPU Socket",
    "PCIe x1",
    "M2_1",
];

pub static NCT6687D_MSI_800: ChipSpec = ec(
    "Nuvoton NCT6687D (MSI 800-series)",
    (0xD5, 0x92, EXACT),
    true,
    EcMap {
        fans: 2,
        temps: EC_TEMPS,
        done_command: EC_DONE,
    },
);

const fn ec(
    name: &'static str,
    (id, revision, mask): (u8, u8, u8),
    fan_control: bool,
    map: EcMap,
) -> ChipSpec {
    ChipSpec {
        name,
        fan_control,
        ident: Ident::Nuvoton { id, revision, mask },
        map: ChipMap::Ec(map),
    }
}

const fn nuvoton(
    name: &'static str,
    (id, revision, mask): (u8, u8, u8),
    fans: usize,
    tach: Tach,
    (slots, sources): (&'static [TempSlot], &'static [TempSource]),
    io_lock: bool,
) -> ChipSpec {
    ChipSpec {
        name,
        fan_control: true,
        ident: Ident::Nuvoton { id, revision, mask },
        map: ChipMap::Nuvoton(NctMap {
            fans,
            tach,
            slots,
            sources,
            io_lock,
        }),
    }
}

const EXACT: u8 = 0xFF;
const FAMILY: u8 = 0xF0;

pub static CHIPS: [ChipSpec; 28] = [
    ChipSpec {
        name: "ITE IT8655E",
        fan_control: true,
        ident: Ident::Ite(0x8655),
        map: ChipMap::Ite(IteMap {
            temps: 6,
            bank_select: true,
            dump_banks: 4,
            config_check: false,
            polarity_check: false,
            ..ITE
        }),
    },
    ite(
        "ITE IT8665E",
        0x8665,
        IteMap {
            fans: 6,
            temps: 6,
            pwm_control: PWM_CONTROL_ALT,
            tach_low: TACH_LOW_ALT,
            tach_high: TACH_HIGH_ALT,
            tach_enable: TACH_ENABLE_8665,
            bank_select: true,
            config_check: false,
            ..ITE
        },
    ),
    ite(
        "ITE IT8625E",
        0x8625,
        IteMap {
            fans: 6,
            bank_select: true,
            dump_banks: 4,
            pwm_control: PWM_CONTROL_ALT,
            tach_low: TACH_LOW_ALT,
            tach_high: TACH_HIGH_ALT,
            ..ITE
        },
    ),
    ite("ITE IT8620E", 0x8620, IteMap { fans: 5, ..ITE }),
    ite(
        "ITE IT8628E",
        0x8628,
        IteMap {
            fans: 6,
            temps: 6,
            ..ITE
        },
    ),
    ite(
        "ITE IT8686E",
        0x8686,
        IteMap {
            fans: 5,
            bank_select: true,
            dump_banks: 4,
            temps: 7,
            ..ITE
        },
    ),
    ite(
        "ITE IT8688E",
        0x8688,
        IteMap {
            fans: 5,
            bank_select: true,
            dump_banks: 4,
            temps: 6,
            index_echo: false,
            ..ITE
        },
    ),
    ChipSpec {
        name: "ITE IT8689E",
        fan_control: true,
        ident: Ident::Ite(0x8689),
        map: ChipMap::Ite(IteMap {
            fans: 6,
            temps: 6,
            bank_select: true,
            dump_banks: 4,
            manual_control: Some(0x7F),
            ..ITE
        }),
    },
    ite(
        "ITE IT8696E",
        0x8696,
        IteMap {
            fans: 6,
            bank_select: true,
            dump_banks: 4,
            temps: 6,
            ..ITE
        },
    ),
    ite("ITE IT8721F", 0x8721, ITE),
    ite("ITE IT8728F", 0x8728, ITE),
    ite("ITE IT8772E", 0x8772, ITE),
    nuvoton(
        "Nuvoton NCT6771F",
        (0xB4, 0x70, FAMILY),
        3,
        Tach::Rpm {
            registers: &[0x656, 0x658, 0x65A],
            min_rpm: 20,
        },
        (SLOTS_6776, SOURCES_6771),
        false,
    ),
    nuvoton(
        "Nuvoton NCT6776F",
        (0xC3, 0x30, FAMILY),
        3,
        Tach::Rpm {
            registers: &[0x656, 0x658, 0x65A],
            min_rpm: 164,
        },
        (SLOTS_6776, SOURCES_6776),
        false,
    ),
    nuvoton(
        "Nuvoton NCT6779D",
        (0xC5, 0x60, FAMILY),
        5,
        COUNT_TACH,
        (SLOTS_6779, SOURCES_6779),
        false,
    ),
    nuvoton(
        "Nuvoton NCT6791D",
        (0xC8, 0x03, EXACT),
        6,
        COUNT_TACH,
        (SLOTS_6791, SOURCES_6796),
        true,
    ),
    nuvoton(
        "Nuvoton NCT6792D",
        (0xC9, 0x11, EXACT),
        6,
        COUNT_TACH,
        (SLOTS_6791, SOURCES_6796),
        true,
    ),
    nuvoton(
        "Nuvoton NCT6792D-A",
        (0xC9, 0x13, EXACT),
        6,
        COUNT_TACH,
        (SLOTS_6779, SOURCES_6779),
        true,
    ),
    nuvoton(
        "Nuvoton NCT6793D",
        (0xD1, 0x21, EXACT),
        6,
        COUNT_TACH,
        (SLOTS_6791, SOURCES_6796),
        true,
    ),
    nuvoton(
        "Nuvoton NCT6795D",
        (0xD3, 0x52, EXACT),
        6,
        COUNT_TACH,
        (SLOTS_6791, SOURCES_6796),
        true,
    ),
    nuvoton(
        "Nuvoton NCT6796D",
        (0xD4, 0x23, EXACT),
        6,
        COUNT_TACH,
        (SLOTS_6791, SOURCES_6796),
        true,
    ),
    nuvoton(
        "Nuvoton NCT6796D-R or NCT5585D",
        (0xD4, 0x2A, EXACT),
        7,
        COUNT_TACH,
        (SLOTS_6791, SOURCES_6796),
        true,
    ),
    nuvoton(
        "Nuvoton NCT6797D",
        (0xD4, 0x51, EXACT),
        7,
        COUNT_TACH,
        (SLOTS_6791, SOURCES_6796),
        true,
    ),
    nuvoton(
        "Nuvoton NCT6798D",
        (0xD4, 0x2B, EXACT),
        7,
        COUNT_TACH,
        (SLOTS_6798, SOURCES_6798),
        true,
    ),
    nuvoton(
        "Nuvoton NCT6799D or NCT6796D-S",
        (0xD8, 0x02, EXACT),
        7,
        COUNT_TACH,
        (SLOTS_6798, SOURCES_6798),
        true,
    ),
    ec(
        "Nuvoton NCT6683D",
        (0xC7, 0x32, EXACT),
        false,
        EcMap {
            fans: 8,
            temps: EC_TEMPS,
            done_command: 0x00,
        },
    ),
    ec(
        "Nuvoton NCT6686D",
        (0xD4, 0x40, 0xFE),
        true,
        EcMap {
            fans: 8,
            temps: EC_TEMPS,
            done_command: EC_DONE,
        },
    ),
    ec(
        "Nuvoton NCT6687D",
        (0xD5, 0x92, EXACT),
        true,
        EcMap {
            fans: 8,
            temps: EC_TEMPS,
            done_command: EC_DONE,
        },
    ),
];

#[cfg(test)]
pub(crate) static MONITOR_ONLY_8686: ChipSpec = ChipSpec {
    name: "ITE IT8686E",
    fan_control: false,
    ident: Ident::Ite(0x8686),
    map: ChipMap::Ite(IteMap {
        fans: 5,
        temps: 7,
        ..ITE
    }),
};

#[cfg(test)]
pub(crate) static MONITOR_ONLY_8628: ChipSpec = ChipSpec {
    name: "ITE IT8628E",
    fan_control: false,
    ident: Ident::Ite(0x8628),
    map: ChipMap::Ite(IteMap {
        fans: 6,
        temps: 6,
        ..ITE
    }),
};

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(sources: &[TempSource]) -> Vec<(u8, &'static str)> {
        sources
            .iter()
            .map(|source| (source.id, source.name))
            .collect()
    }

    #[test]
    fn existing_sources_keep_their_positions_so_board_ids_stay_stable() {
        assert_eq!(
            ids(&SOURCES_6771[..4]),
            [(1, "SYSTIN"), (2, "CPUTIN"), (3, "AUXTIN"), (5, "PECI 0")]
        );
        assert_eq!(
            ids(&SOURCES_6776[..4]),
            [(1, "SYSTIN"), (2, "CPUTIN"), (3, "AUXTIN"), (12, "PECI 0")]
        );
        let auxtin = [
            (1, "SYSTIN"),
            (2, "CPUTIN"),
            (3, "AUXTIN0"),
            (4, "AUXTIN1"),
            (5, "AUXTIN2"),
            (6, "AUXTIN3"),
        ];
        assert_eq!(
            ids(&SOURCES_6779[..7]),
            [&auxtin[..], &[(16, "PECI 0")]].concat()
        );
        assert_eq!(
            ids(&SOURCES_6796[..8]),
            [&auxtin[..], &[(7, "AUXTIN4"), (16, "PECI 0")]].concat()
        );
        assert_eq!(
            ids(&SOURCES_6798[..9]),
            [
                &auxtin[..],
                &[(7, "AUXTIN4"), (10, "TSENSOR"), (16, "PECI 0")]
            ]
            .concat()
        );
    }

    #[test]
    fn no_table_names_a_source_twice_or_the_per_fan_virtual_source() {
        for table in [
            SOURCES_6771,
            SOURCES_6776,
            SOURCES_6779,
            SOURCES_6796,
            SOURCES_6798,
        ] {
            let mut seen: Vec<u8> = table.iter().map(|source| source.id).collect();
            seen.sort_unstable();
            seen.dedup();
            assert_eq!(seen.len(), table.len());
            assert!(!seen.contains(&31));
        }
    }
}
