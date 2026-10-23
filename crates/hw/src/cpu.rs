pub use mostlyfans_core::config::CPU_SENSOR;

use crate::HwError;
use crate::amd::AmdCpu;
use crate::intel::IntelCpu;

#[derive(Clone, Debug, PartialEq)]
pub struct CpuReading {
    pub id: String,
    pub label: String,
    pub celsius: f32,
}

#[derive(Debug)]
pub enum Cpu {
    Amd(AmdCpu),
    Intel(IntelCpu),
}

impl Cpu {
    pub fn open() -> Result<Self, HwError> {
        match AmdCpu::open() {
            Err(HwError::UnsupportedCpu(_)) => IntelCpu::open().map(Self::Intel),
            result => result.map(Self::Amd),
        }
    }

    pub fn read(&self) -> Result<Vec<CpuReading>, HwError> {
        match self {
            Self::Amd(cpu) => {
                let temps = cpu.read()?;
                let mut readings = vec![reading(CPU_SENSOR, "CPU Tctl", temps.tctl)];
                if let Some(tdie) = temps.tdie {
                    readings.push(reading("cpu/tdie", "CPU Tdie", tdie));
                }
                readings.extend(temps.ccds.into_iter().map(|(index, celsius)| {
                    reading(
                        &format!("cpu/ccd{}", index + 1),
                        &format!("CPU CCD{}", index + 1),
                        celsius,
                    )
                }));
                Ok(readings)
            }
            Self::Intel(cpu) => {
                let label = if cpu.has_package_sensor() {
                    "CPU package"
                } else {
                    "CPU (hottest core)"
                };
                Ok(vec![reading(CPU_SENSOR, label, cpu.read()?)])
            }
        }
    }
}

fn reading(id: &str, label: &str, celsius: f32) -> CpuReading {
    CpuReading {
        id: id.to_owned(),
        label: label.to_owned(),
        celsius,
    }
}
