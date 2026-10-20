#[derive(Debug, PartialEq, Eq)]
pub struct ChipSpec {
    pub name: &'static str,
    pub verified: bool,
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
        }
    }

    pub fn temp_names(&self) -> Vec<Option<&'static str>> {
        match &self.map {
            ChipMap::Ite(map) => vec![None; map.temps],
            ChipMap::Nuvoton(map) => map.sources.iter().map(|source| Some(source.name)).collect(),
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
};

const fn ite(name: &'static str, chip_id: u16, map: IteMap) -> ChipSpec {
    ChipSpec {
        name,
        verified: false,
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
];

const SOURCES_6776: &[TempSource] = &[
    source(1, "SYSTIN", None),
    source(2, "CPUTIN", None),
    source(3, "AUXTIN", None),
    source(12, "PECI 0", None),
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
];

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
        verified: false,
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

pub static CHIPS: [ChipSpec; 24] = [
    ChipSpec {
        name: "ITE IT8655E",
        verified: true,
        ident: Ident::Ite(0x8655),
        map: ChipMap::Ite(IteMap {
            temps: 6,
            bank_select: true,
            dump_banks: 4,
            config_check: false,
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
            temps: 7,
            ..ITE
        },
    ),
    ite(
        "ITE IT8688E",
        0x8688,
        IteMap {
            fans: 5,
            temps: 6,
            index_echo: false,
            ..ITE
        },
    ),
    ite(
        "ITE IT8689E",
        0x8689,
        IteMap {
            fans: 6,
            temps: 6,
            dump_banks: 4,
            manual_control: Some(0x7F),
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
];
