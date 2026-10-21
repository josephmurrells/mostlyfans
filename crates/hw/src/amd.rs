use std::arch::x86_64::__cpuid;

use crate::HwError;
use crate::mutex::{DEFAULT_TIMEOUT, NamedMutex, PCI_BUS};
use crate::pawnio::{AMD_FAMILY_17_MODULE, PawnIoModule};

const THM_TCON_CUR_TMP: u32 = 0x0005_9800;
const CUR_TEMP_SHIFT: u32 = 21;
const RANGE_SEL_MASK: u32 = 1 << 19;
const TJ_SEL_MASK: u32 = 0b11 << 16;
const CCD_VALID: u32 = 1 << 11;
const CCD_TEMP_MASK: u32 = 0x7FF;
const RANGE_OFFSET: f32 = 49.0;
const TCTL_OFFSETS: [(&str, f32); 6] = [
    ("1600X", 20.0),
    ("1700X", 20.0),
    ("1800X", 20.0),
    ("2700X", 10.0),
    ("Threadripper 19", 27.0),
    ("Threadripper 29", 27.0),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CpuInfo {
    pub vendor: String,
    pub brand: String,
    pub family: u32,
    pub model: u32,
}

impl CpuInfo {
    pub fn detect() -> Self {
        let vendor_leaf = __cpuid(0);
        let vendor = [vendor_leaf.ebx, vendor_leaf.edx, vendor_leaf.ecx]
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .collect::<Vec<_>>();
        let signature = __cpuid(1).eax;
        let base_family = (signature >> 8) & 0xF;
        let family = if base_family == 0xF {
            base_family + ((signature >> 20) & 0xFF)
        } else {
            base_family
        };
        let model = ((signature >> 4) & 0xF) | (((signature >> 16) & 0xF) << 4);
        let brand = (0x8000_0002..=0x8000_0004u32)
            .flat_map(|leaf| {
                let regs = __cpuid(leaf);
                [regs.eax, regs.ebx, regs.ecx, regs.edx]
            })
            .flat_map(u32::to_le_bytes)
            .collect::<Vec<_>>();
        Self {
            vendor: String::from_utf8_lossy(&vendor).into_owned(),
            brand: String::from_utf8_lossy(&brand)
                .trim_matches(char::from(0))
                .trim()
                .to_owned(),
            family,
            model,
        }
    }

    pub fn tctl_offset(&self) -> f32 {
        if self.family != 0x17 {
            return 0.0;
        }
        TCTL_OFFSETS
            .iter()
            .find(|(needle, _)| self.brand.contains(needle))
            .map_or(0.0, |(_, offset)| *offset)
    }

    fn ccd_layout(&self) -> Option<(u32, u32)> {
        let (offset, count) = match (self.family, self.model) {
            (0x17 | 0x18, 0x01 | 0x08 | 0x11 | 0x18) => (0x154, 4),
            (0x17 | 0x18, 0x31 | 0x47 | 0x60 | 0x68 | 0x71) => (0x154, 8),
            (0x17 | 0x18, 0xA0..=0xAF) => (0x300, 8),
            (0x19, 0x00..=0x01 | 0x08 | 0x21 | 0x50..=0x5F) => (0x154, 8),
            (0x19, 0x40..=0x4F) => (0x300, 8),
            (0x19, 0x60..=0x7F) => (0x308, 8),
            (0x19, 0x10..=0x1F | 0xA0..=0xAF) => (0x300, 12),
            (0x1A, 0x00..=0x1F) => (0x1F0, 16),
            (0x1A, 0x40..=0x4F) => (0x308, 8),
            _ => return None,
        };
        Some((THM_TCON_CUR_TMP + offset, count))
    }
}

pub fn decode_tctl(raw: u32) -> f32 {
    let celsius = (raw >> CUR_TEMP_SHIFT) as f32 * 0.125;
    if raw & RANGE_SEL_MASK != 0 || raw & TJ_SEL_MASK == TJ_SEL_MASK {
        celsius - RANGE_OFFSET
    } else {
        celsius
    }
}

pub fn decode_ccd(raw: u32) -> Option<f32> {
    (raw & CCD_VALID != 0).then_some((raw & CCD_TEMP_MASK) as f32 * 0.125 - RANGE_OFFSET)
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CpuTemps {
    pub tctl: f32,
    pub tdie: Option<f32>,
    pub ccds: Vec<(u32, f32)>,
}

#[derive(Debug)]
pub struct AmdCpu {
    module: PawnIoModule,
    lock: NamedMutex,
    info: CpuInfo,
    ccd_base: u32,
    ccds: Vec<u32>,
}

impl AmdCpu {
    pub fn open() -> Result<Self, HwError> {
        let info = CpuInfo::detect();
        if info.vendor != "AuthenticAMD" || !matches!(info.family, 0x17 | 0x19 | 0x1A) {
            return Err(HwError::UnsupportedCpu(format!(
                "{} family {:#x}",
                info.brand, info.family
            )));
        }
        let module = PawnIoModule::load(AMD_FAMILY_17_MODULE)?;
        let lock = NamedMutex::open(PCI_BUS)?;
        let (ccd_base, ccd_count) = info.ccd_layout().unwrap_or_default();
        let mut cpu = Self {
            module,
            lock,
            info,
            ccd_base,
            ccds: Vec::new(),
        };
        let raw = {
            let _guard = cpu.lock.acquire(DEFAULT_TIMEOUT)?;
            (0..ccd_count)
                .map(|index| cpu.read_smn(ccd_base + index * 4))
                .collect::<Result<Vec<_>, _>>()?
        };
        cpu.ccds = (0..ccd_count)
            .zip(raw)
            .filter(|(_, raw)| decode_ccd(*raw).is_some())
            .map(|(index, _)| index)
            .collect();
        Ok(cpu)
    }

    pub fn info(&self) -> &CpuInfo {
        &self.info
    }

    fn read_smn(&self, address: u32) -> Result<u32, HwError> {
        let output = self
            .module
            .execute("ioctl_read_smn", &[u64::from(address)], 1)?;
        Ok(output[0] as u32)
    }

    pub fn read(&self) -> Result<CpuTemps, HwError> {
        let _guard = self.lock.acquire(DEFAULT_TIMEOUT)?;
        let tctl = decode_tctl(self.read_smn(THM_TCON_CUR_TMP)?);
        let offset = self.info.tctl_offset();
        let ccds = self
            .ccds
            .iter()
            .filter_map(|&index| match self.read_smn(self.ccd_base + index * 4) {
                Ok(raw) => decode_ccd(raw).map(|celsius| (index, celsius)),
                Err(error) => {
                    tracing::debug!(index, %error, "CCD temperature read failed");
                    None
                }
            })
            .collect();
        Ok(CpuTemps {
            tctl,
            tdie: (offset > 0.0).then_some(tctl - offset),
            ccds,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tctl_decoding_handles_range_select() {
        let raw = (400u32) << CUR_TEMP_SHIFT;
        assert_eq!(decode_tctl(raw), 50.0);
        assert_eq!(decode_tctl(raw | RANGE_SEL_MASK), 1.0);
        assert_eq!(decode_tctl(raw | TJ_SEL_MASK), 1.0);
        assert_eq!(decode_tctl(raw | (1 << 16)), 50.0);
    }

    #[test]
    fn ccd_decoding_requires_valid_bit() {
        assert_eq!(decode_ccd(792), None);
        assert_eq!(decode_ccd(792 | CCD_VALID), Some(50.0));
    }

    #[test]
    fn offsets_follow_k10temp_table() {
        let mut info = CpuInfo {
            vendor: "AuthenticAMD".into(),
            brand: String::new(),
            family: 0x17,
            model: 1,
        };
        info.brand = "AMD Ryzen 7 1700X Eight-Core Processor".into();
        assert_eq!(info.tctl_offset(), 20.0);
        info.brand = "AMD Ryzen 7 5700X3D 8-Core Processor".into();
        assert_eq!(info.tctl_offset(), 0.0);
        info.brand = "AMD Ryzen 7 1700X Eight-Core Processor".into();
        info.family = 0x19;
        assert_eq!(info.tctl_offset(), 0.0);
    }

    #[test]
    fn ccd_layouts_follow_k10temp() {
        let layout = |family, model| {
            CpuInfo {
                vendor: "AuthenticAMD".into(),
                brand: String::new(),
                family,
                model,
            }
            .ccd_layout()
        };
        assert_eq!(layout(0x17, 0x08), Some((0x5_9954, 4)));
        assert_eq!(layout(0x17, 0x71), Some((0x5_9954, 8)));
        assert_eq!(layout(0x19, 0x21), Some((0x5_9954, 8)));
        assert_eq!(layout(0x19, 0x44), Some((0x5_9B00, 8)));
        assert_eq!(layout(0x19, 0x61), Some((0x5_9B08, 8)));
        assert_eq!(layout(0x19, 0x11), Some((0x5_9B00, 12)));
        assert_eq!(layout(0x1A, 0x44), Some((0x5_9B08, 8)));
        assert_eq!(layout(0x1A, 0x02), Some((0x5_99F0, 16)));
        assert_eq!(layout(0x19, 0x30), None);
    }

    #[test]
    fn detects_this_cpu() {
        let info = CpuInfo::detect();
        assert!(!info.vendor.is_empty());
        assert!(info.family > 0);
    }
}
