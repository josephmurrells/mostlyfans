use std::time::Duration;

use crate::HwError;
use crate::amd::CpuInfo;
use crate::mutex::{NamedMutex, PCI_BUS};
use crate::pawnio::{PawnIoModule, RYZEN_SMU_MODULE};

const LOCK_TIMEOUT: Duration = Duration::from_secs(1);
const TABLE_CELLS: usize = 512;
const CORES_PER_CCD: usize = 8;
const CODE_NAMES: [&str; 42] = [
    "Colfax",
    "Renoir",
    "Picasso",
    "Matisse",
    "Threadripper",
    "CastlePeak",
    "RavenRidge",
    "RavenRidge2",
    "SummitRidge",
    "PinnacleRidge",
    "Rembrandt",
    "Vermeer",
    "Vangogh",
    "Cezanne",
    "Milan",
    "Dali",
    "Raphael",
    "GraniteRidge",
    "Naples",
    "FireFlight",
    "Rome",
    "Chagall",
    "Lucienne",
    "Phoenix",
    "Phoenix2",
    "Mendocino",
    "Genoa",
    "StormPeak",
    "DragonRange",
    "Mero",
    "HawkPoint",
    "StrixPoint",
    "StrixHalo",
    "KrackanPoint",
    "KrackanPoint2",
    "Turin",
    "TurinD",
    "Bergamo",
    "ShimadaPeak",
    "Carrizo",
    "BristolRidge",
    "StoneyRidge",
];
pub const CCD_FUSE: u32 = 0x0005_D218;
const CCD_ENABLE_SHIFT: u32 = 22;
const CORE_FUSE_BASE: u32 = 0x3008_1800;
const CORE_FUSE_CCD_STRIDE: u32 = 0x0200_0000;
const MATISSE_FUSES: CoreMap = CoreMap::Fuses { offset: 0x238 };
const VERMEER_FUSES: CoreMap = CoreMap::Fuses { offset: 0x598 };
const CEZANNE_FIELDS: CoreMap =
    CoreMap::ZeroWhenFusedOff(&[200, 208, 224, 232, 240, 248, 256, 264, 296]);
const CORE_TEMP_LAYOUTS: [CoreTempLayout; 7] = [
    CoreTempLayout {
        version: 0x0024_0803,
        offset: 179,
        cores: 16,
        map: MATISSE_FUSES,
    },
    CoreTempLayout {
        version: 0x0024_0903,
        offset: 163,
        cores: 8,
        map: MATISSE_FUSES,
    },
    CoreTempLayout {
        version: 0x0038_0804,
        offset: 201,
        cores: 16,
        map: VERMEER_FUSES,
    },
    CoreTempLayout {
        version: 0x0038_0805,
        offset: 204,
        cores: 16,
        map: VERMEER_FUSES,
    },
    CoreTempLayout {
        version: 0x0038_0904,
        offset: 185,
        cores: 8,
        map: VERMEER_FUSES,
    },
    CoreTempLayout {
        version: 0x0038_0905,
        offset: 188,
        cores: 8,
        map: VERMEER_FUSES,
    },
    CoreTempLayout {
        version: 0x0040_0005,
        offset: 216,
        cores: 8,
        map: CEZANNE_FIELDS,
    },
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreMap {
    Fuses { offset: u32 },
    ZeroWhenFusedOff(&'static [usize]),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoreTempLayout {
    pub version: u32,
    pub offset: usize,
    pub cores: usize,
    pub map: CoreMap,
}

impl CoreTempLayout {
    pub fn for_version(version: u32) -> Option<Self> {
        CORE_TEMP_LAYOUTS
            .into_iter()
            .find(|layout| layout.version == version)
    }

    pub fn all_slots(&self) -> Vec<usize> {
        (0..self.cores).collect()
    }

    pub fn ccds(&self) -> usize {
        self.cores.div_ceil(CORES_PER_CCD)
    }

    pub fn core_temps(&self, table: &[f32], slots: &[usize]) -> Option<Vec<CoreTemp>> {
        slots
            .iter()
            .enumerate()
            .map(|(core, &slot)| {
                let celsius = *table
                    .get(self.offset + slot)
                    .filter(|_| slot < self.cores)?;
                Some(CoreTemp {
                    core,
                    ccd: slot / CORES_PER_CCD + 1,
                    celsius,
                })
            })
            .collect()
    }

    pub fn active_slots(&self, table: &[f32], fields: &[usize]) -> Option<Vec<usize>> {
        let mut slots = Vec::new();
        for slot in 0..self.cores {
            let mut values = fields.iter().map(|field| table.get(field + slot).copied());
            let active = values.try_fold(false, |active, value| Some(active || value? != 0.0))?;
            if active {
                slots.push(slot);
            }
        }
        Some(slots)
    }
}

pub fn core_fuse_addresses(offset: u32, ccd_fuse: u32, ccds: usize) -> Vec<Option<u32>> {
    let enabled = ccd_fuse >> CCD_ENABLE_SHIFT;
    (0..ccds as u32)
        .map(|ccd| {
            (enabled & (1 << ccd) != 0)
                .then(|| (CORE_FUSE_BASE + offset) | (ccd * CORE_FUSE_CCD_STRIDE))
        })
        .collect()
}

pub fn enabled_slots_from_fuses(core_fuses: &[Option<u32>]) -> Vec<usize> {
    core_fuses
        .iter()
        .enumerate()
        .flat_map(|(ccd, fuse)| {
            (0..CORES_PER_CCD)
                .filter(move |core| fuse.is_some_and(|fuse| fuse & (1 << core) == 0))
                .map(move |core| ccd * CORES_PER_CCD + core)
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CoreTemp {
    pub core: usize,
    pub ccd: usize,
    pub celsius: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PmTableInfo {
    pub version: u32,
    pub base: u64,
}

pub fn code_name(index: u64) -> Option<&'static str> {
    usize::try_from(index)
        .ok()
        .and_then(|index| CODE_NAMES.get(index).copied())
}

pub fn format_smu_version(raw: u32) -> String {
    let [patch, minor, major, _] = raw.to_le_bytes();
    format!("{major}.{minor}.{patch}")
}

pub fn split_cells(cells: &[u64]) -> Vec<f32> {
    cells
        .iter()
        .flat_map(|cell| [*cell as u32, (cell >> 32) as u32])
        .map(f32::from_bits)
        .collect()
}

#[derive(Debug)]
pub struct RyzenSmu {
    module: PawnIoModule,
    lock: NamedMutex,
    info: CpuInfo,
    table: PmTableInfo,
}

impl RyzenSmu {
    pub fn open() -> Result<Self, HwError> {
        let info = CpuInfo::detect();
        if info.vendor != "AuthenticAMD" || !matches!(info.family, 0x17 | 0x19 | 0x1A) {
            return Err(HwError::UnsupportedCpu(format!(
                "{} family {:#x}",
                info.brand, info.family
            )));
        }
        let module = PawnIoModule::load(RYZEN_SMU_MODULE)?;
        let lock = NamedMutex::open(PCI_BUS)?;
        let table = {
            let _guard = lock.acquire(LOCK_TIMEOUT)?;
            let output = module.execute("ioctl_resolve_pm_table", &[], 2)?;
            PmTableInfo {
                version: output[0] as u32,
                base: output[1],
            }
        };
        Ok(Self {
            module,
            lock,
            info,
            table,
        })
    }

    pub fn info(&self) -> &CpuInfo {
        &self.info
    }

    pub fn code_name(&self) -> Result<u64, HwError> {
        Ok(self.module.execute("ioctl_get_code_name", &[], 1)?[0])
    }

    pub fn smu_version(&self) -> Result<u32, HwError> {
        let _guard = self.lock.acquire(LOCK_TIMEOUT)?;
        Ok(self.module.execute("ioctl_get_smu_version", &[], 1)?[0] as u32)
    }

    pub fn pm_table(&self) -> PmTableInfo {
        self.table
    }

    pub fn read_pm_table(&self) -> Result<Vec<f32>, HwError> {
        let _guard = self.lock.acquire(LOCK_TIMEOUT)?;
        self.module.execute("ioctl_update_pm_table", &[], 0)?;
        let cells = self
            .module
            .execute("ioctl_read_pm_table", &[], TABLE_CELLS)?;
        Ok(split_cells(&cells))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_names_follow_the_module_enum() {
        assert_eq!(code_name(0), Some("Colfax"));
        assert_eq!(code_name(3), Some("Matisse"));
        assert_eq!(code_name(11), Some("Vermeer"));
        assert_eq!(code_name(17), Some("GraniteRidge"));
        assert_eq!(code_name(41), Some("StoneyRidge"));
        assert_eq!(code_name(42), None);
        assert_eq!(code_name(u64::MAX), None);
    }

    #[test]
    fn smu_version_reads_major_minor_patch() {
        assert_eq!(format_smu_version(0x0038_4F00), "56.79.0");
        assert_eq!(format_smu_version(0x0141_5203), "65.82.3");
    }

    #[test]
    fn cells_hold_two_floats_low_half_first() {
        let cell = u64::from(40.5f32.to_bits()) | (u64::from(61.25f32.to_bits()) << 32);
        assert_eq!(split_cells(&[cell, 0]), vec![40.5, 61.25, 0.0, 0.0]);
    }

    #[test]
    fn layouts_follow_ryzen_monitor() {
        let layout = CoreTempLayout::for_version(0x0038_0904).unwrap();
        assert_eq!((layout.offset, layout.cores), (185, 8));
        assert_eq!(
            CoreTempLayout::for_version(0x0038_0805).unwrap().offset,
            204
        );
        assert_eq!(
            CoreTempLayout::for_version(0x0024_0903).unwrap().offset,
            163
        );
        assert_eq!(
            CoreTempLayout::for_version(0x0040_0005).unwrap().offset,
            216
        );
        assert_eq!(CoreTempLayout::for_version(0x0040_0004), None);
    }

    #[test]
    fn core_temps_map_cores_to_ccds() {
        let mut table = vec![0.0; 300];
        table[201] = 45.0;
        table[216] = 52.0;
        let layout = CoreTempLayout::for_version(0x0038_0804).unwrap();
        let temps = layout.core_temps(&table, &layout.all_slots()).unwrap();
        assert_eq!(temps.len(), 16);
        assert_eq!(
            temps[0],
            CoreTemp {
                core: 0,
                ccd: 1,
                celsius: 45.0
            }
        );
        assert_eq!(
            temps[15],
            CoreTemp {
                core: 15,
                ccd: 2,
                celsius: 52.0
            }
        );
    }

    #[test]
    fn a_short_table_has_no_core_temps() {
        let layout = CoreTempLayout::for_version(0x0038_0904).unwrap();
        assert_eq!(layout.core_temps(&[0.0; 190], &layout.all_slots()), None);
        assert_eq!(layout.core_temps(&[0.0; 300], &[8]), None);
    }

    #[test]
    fn a_twelve_core_part_reads_six_cores_from_each_ccd() {
        let ccd_fuse = 0b11 << 22;
        let addresses = core_fuse_addresses(0x598, ccd_fuse, 2);
        assert_eq!(addresses, [Some(0x3008_1D98), Some(0x3208_1D98)]);
        let slots = enabled_slots_from_fuses(&[Some(0b1100_0000), Some(0xFFFF_FF81)]);
        assert_eq!(slots, [0, 1, 2, 3, 4, 5, 9, 10, 11, 12, 13, 14]);

        let layout = CoreTempLayout::for_version(0x0038_0805).unwrap();
        let mut table = vec![0.0; 300];
        table[204 + 9] = 55.0;
        let temps = layout.core_temps(&table, &slots).unwrap();
        assert_eq!(temps.len(), 12);
        assert_eq!(
            temps[6],
            CoreTemp {
                core: 6,
                ccd: 2,
                celsius: 55.0
            }
        );
    }

    #[test]
    fn a_ccd_the_fuses_leave_out_has_no_cores() {
        let addresses = core_fuse_addresses(0x238, 0b01 << 22, 2);
        assert_eq!(addresses, [Some(0x3008_1A38), None]);
        assert_eq!(
            enabled_slots_from_fuses(&[Some(0), None]),
            (0..8).collect::<Vec<_>>()
        );
    }

    #[test]
    fn cezanne_finds_fused_off_cores_from_all_zero_fields() {
        let layout = CoreTempLayout::for_version(0x0040_0005).unwrap();
        let CoreMap::ZeroWhenFusedOff(fields) = layout.map else {
            panic!("Cezanne reads its core map from the PM table");
        };
        let mut table = vec![0.0; 372];
        for slot in [0, 1, 2, 3, 4, 7] {
            table[232 + slot] = 1.5;
        }
        assert_eq!(
            layout.active_slots(&table, fields),
            Some(vec![0, 1, 2, 3, 4, 7])
        );
        assert_eq!(layout.active_slots(&table[..250], fields), None);
    }
}
