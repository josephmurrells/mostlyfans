use std::arch::x86_64::__cpuid;

use fanforge_win::topology::PhysicalCore;
use windows::Win32::System::SystemInformation::GROUP_AFFINITY;
use windows::Win32::System::Threading::{
    GetActiveProcessorCount, GetCurrentThread, SetThreadGroupAffinity,
};

use crate::HwError;
use crate::amd::CpuInfo;
use crate::pawnio::{INTEL_MSR_MODULE, PawnIoModule};

const IA32_THERM_STATUS: u64 = 0x19C;
const IA32_TEMPERATURE_TARGET: u64 = 0x1A2;
const IA32_PACKAGE_THERM_STATUS: u64 = 0x1B1;
const THERMAL_LEAF: u32 = 6;
const DIGITAL_SENSOR: u32 = 1 << 0;
const PACKAGE_SENSOR: u32 = 1 << 6;
const READOUT_SHIFT: u64 = 16;
const READOUT_MASK: u64 = 0x7F;
const READING_VALID: u64 = 1 << 31;
const TJMAX_SHIFT: u64 = 16;
const TJMAX_MASK: u64 = 0xFF;
const DEFAULT_TJMAX: f32 = 100.0;
const MAX_PINNED_CPUS: u32 = usize::BITS;

pub fn decode_tjmax(raw: u64) -> f32 {
    match (raw >> TJMAX_SHIFT) & TJMAX_MASK {
        0 => DEFAULT_TJMAX,
        tjmax => tjmax as f32,
    }
}

pub fn decode_package(raw: u64, tjmax: f32) -> f32 {
    tjmax - ((raw >> READOUT_SHIFT) & READOUT_MASK) as f32
}

pub fn decode_core(raw: u64, tjmax: f32) -> Option<f32> {
    (raw & READING_VALID != 0).then(|| decode_package(raw, tjmax))
}

#[derive(Debug)]
pub struct IntelCpu {
    module: PawnIoModule,
    info: CpuInfo,
    tjmax: f32,
    package: bool,
}

impl IntelCpu {
    pub fn open() -> Result<Self, HwError> {
        let info = CpuInfo::detect();
        let thermal = if __cpuid(0).eax >= THERMAL_LEAF {
            __cpuid(THERMAL_LEAF).eax
        } else {
            0
        };
        if info.vendor != "GenuineIntel" || thermal & DIGITAL_SENSOR == 0 {
            return Err(HwError::UnsupportedCpu(info.brand));
        }
        let module = PawnIoModule::load(INTEL_MSR_MODULE)?;
        let mut cpu = Self {
            module,
            info,
            tjmax: DEFAULT_TJMAX,
            package: thermal & PACKAGE_SENSOR != 0,
        };
        match cpu.read_msr(IA32_TEMPERATURE_TARGET) {
            Ok(raw) => cpu.tjmax = decode_tjmax(raw),
            Err(error) => tracing::warn!(%error, "TjMax unreadable; assuming 100 °C"),
        }
        Ok(cpu)
    }

    pub fn info(&self) -> &CpuInfo {
        &self.info
    }

    pub fn has_package_sensor(&self) -> bool {
        self.package
    }

    fn read_msr(&self, msr: u64) -> Result<u64, HwError> {
        Ok(self.module.execute("ioctl_read_msr", &[msr], 1)?[0])
    }

    pub fn read(&self) -> Result<f32, HwError> {
        if self.package {
            return Ok(decode_package(
                self.read_msr(IA32_PACKAGE_THERM_STATUS)?,
                self.tjmax,
            ));
        }
        let cpus = unsafe { GetActiveProcessorCount(0) }.min(MAX_PINNED_CPUS);
        let mut hottest = None;
        let mut last_error = None;
        for cpu in 0..cpus {
            match on_cpu(0, cpu, || self.read_msr(IA32_THERM_STATUS)).and_then(|read| read) {
                Ok(raw) => {
                    if let Some(celsius) = decode_core(raw, self.tjmax) {
                        hottest = Some(hottest.map_or(celsius, |hot: f32| hot.max(celsius)));
                    }
                }
                Err(error) => last_error = Some(error),
            }
        }
        match (hottest, last_error) {
            (Some(celsius), _) => Ok(celsius),
            (None, Some(error)) => Err(error),
            (None, None) => Err(HwError::UnsupportedCpu(self.info.brand.clone())),
        }
    }

    pub fn core_temps(&self, cores: &[PhysicalCore]) -> Vec<Option<f32>> {
        cores
            .iter()
            .map(|core| {
                on_cpu(core.group, u32::from(core.first_cpu), || {
                    self.read_msr(IA32_THERM_STATUS)
                })
                .and_then(|read| read)
                .map_err(|error| tracing::debug!(%error, "core temperature read failed"))
                .ok()
                .and_then(|raw| decode_core(raw, self.tjmax))
            })
            .collect()
    }
}

fn on_cpu<T>(group: u16, cpu: u32, read: impl FnOnce() -> T) -> Result<T, HwError> {
    let thread = unsafe { GetCurrentThread() };
    let pinned = GROUP_AFFINITY {
        Mask: 1 << cpu,
        Group: group,
        Reserved: [0; 3],
    };
    let mut previous = GROUP_AFFINITY::default();
    if !unsafe { SetThreadGroupAffinity(thread, &pinned, Some(&mut previous)) }.as_bool() {
        return Err(HwError::Driver {
            call: "SetThreadGroupAffinity".to_owned(),
            source: windows::core::Error::from_thread(),
        });
    }
    let result = read();
    if !unsafe { SetThreadGroupAffinity(thread, &previous, None) }.as_bool() {
        tracing::warn!(error = %windows::core::Error::from_thread(), "could not restore thread affinity");
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tjmax_comes_from_bits_16_to_23_with_a_fallback() {
        assert_eq!(decode_tjmax(100 << 16), 100.0);
        assert_eq!(decode_tjmax((105 << 16) | (5 << 24)), 105.0);
        assert_eq!(decode_tjmax(0), DEFAULT_TJMAX);
    }

    #[test]
    fn readouts_count_down_from_tjmax() {
        assert_eq!(decode_package(40 << 16, 100.0), 60.0);
        assert_eq!(decode_core((40 << 16) | READING_VALID, 100.0), Some(60.0));
        assert_eq!(decode_core(40 << 16, 100.0), None);
    }
}
