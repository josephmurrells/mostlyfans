use std::collections::VecDeque;
use std::time::Duration;

use fanforge_core::SensorId;
use fanforge_core::telemetry::{SensorReading, is_plausible};
use fanforge_win::topology::{self, PhysicalCore};

use super::SensorSource;
use crate::amd::{AmdCpu, CpuInfo, CpuTemps};
use crate::intel::IntelCpu;
use crate::smu::{CoreTemp, CoreTempLayout, RyzenSmu};

const CADENCE: Duration = Duration::from_secs(2);
const BELOW_REFERENCE: f32 = 25.0;
const ABOVE_REFERENCE: f32 = 15.0;
const CHECK_WINDOW: usize = 10;
const FAILED_CHECKS_ALLOWED: usize = 2;

#[derive(Debug, Default)]
pub struct LayoutCheck {
    recent: VecDeque<bool>,
}

impl LayoutCheck {
    pub fn record(&mut self, passed: bool) -> bool {
        if self.recent.len() == CHECK_WINDOW {
            self.recent.pop_front();
        }
        self.recent.push_back(passed);
        self.recent.iter().filter(|passed| !**passed).count() <= FAILED_CHECKS_ALLOWED
    }
}

pub fn layout_plausible(cores: &[CoreTemp], reference: &CpuTemps) -> bool {
    let all_equal = cores
        .windows(2)
        .all(|pair| pair[0].celsius == pair[1].celsius);
    if cores.is_empty() || all_equal {
        return false;
    }
    cores.iter().all(|core| {
        let ccd = reference
            .ccds
            .iter()
            .find(|(index, _)| *index as usize + 1 == core.ccd)
            .map_or(reference.tctl, |(_, celsius)| *celsius);
        is_plausible(core.celsius)
            && core.celsius >= ccd - BELOW_REFERENCE
            && core.celsius <= ccd.max(reference.tctl) + ABOVE_REFERENCE
    })
}

pub fn intel_labels(cores: &[PhysicalCore]) -> Vec<String> {
    let fastest = cores.iter().map(|core| core.efficiency_class).max();
    let hybrid = cores
        .iter()
        .any(|core| Some(core.efficiency_class) != fastest);
    cores
        .iter()
        .enumerate()
        .map(
            |(index, core)| match (hybrid, Some(core.efficiency_class) == fastest) {
                (false, _) => format!("Core {index}"),
                (true, true) => format!("P-core {index}"),
                (true, false) => format!("E-core {index}"),
            },
        )
        .collect()
}

#[derive(Debug)]
struct AmdCores {
    smu: RyzenSmu,
    cpu: AmdCpu,
    layout: CoreTempLayout,
    check: LayoutCheck,
}

#[derive(Debug)]
struct IntelCores {
    cpu: IntelCpu,
    cores: Vec<PhysicalCore>,
    labels: Vec<String>,
}

#[derive(Debug)]
enum Backend {
    Amd(Box<AmdCores>),
    Intel(IntelCores),
}

#[derive(Debug, Default)]
enum State {
    #[default]
    Closed,
    Open(Backend),
    Unsupported,
}

#[derive(Debug, Default)]
pub struct CoreTempSource {
    state: State,
}

fn unsupported(reason: &str) -> Result<State, String> {
    tracing::info!(reason, "per-core temperatures are not shown");
    Ok(State::Unsupported)
}

fn open() -> Result<State, String> {
    let cores = topology::physical_cores().map_err(|error| error.to_string())?;
    match CpuInfo::detect().vendor.as_str() {
        "AuthenticAMD" => {
            let smu = RyzenSmu::open().map_err(|error| error.to_string())?;
            let table = smu.pm_table();
            let Some(layout) = CoreTempLayout::for_version(table.version) else {
                return unsupported(&format!(
                    "no per-core layout for PM table version {:#08x}",
                    table.version
                ));
            };
            if layout.cores != cores.len() {
                return unsupported(&format!(
                    "PM table has {} core slots but the CPU has {} cores",
                    layout.cores,
                    cores.len()
                ));
            }
            let cpu = AmdCpu::open().map_err(|error| error.to_string())?;
            Ok(State::Open(Backend::Amd(Box::new(AmdCores {
                smu,
                cpu,
                layout,
                check: LayoutCheck::default(),
            }))))
        }
        "GenuineIntel" => {
            if cores.is_empty() {
                return unsupported("Windows reported no cores");
            }
            let cpu = IntelCpu::open().map_err(|error| error.to_string())?;
            Ok(State::Open(Backend::Intel(IntelCores {
                cpu,
                labels: intel_labels(&cores),
                cores,
            })))
        }
        vendor => unsupported(&format!("unsupported CPU vendor {vendor}")),
    }
}

fn reading(index: usize, label: String, celsius: Option<f32>) -> SensorReading {
    SensorReading {
        id: SensorId::core(index),
        label,
        celsius: celsius.filter(|celsius| is_plausible(*celsius)),
    }
}

impl AmdCores {
    fn read(&mut self) -> Result<Option<Vec<SensorReading>>, String> {
        let reference = self.cpu.read();
        let table = self
            .smu
            .read_pm_table()
            .map_err(|error| error.to_string())?;
        let cores = self
            .layout
            .core_temps(&table)
            .ok_or_else(|| "the PM table is shorter than its layout".to_owned())?;
        match reference {
            Ok(reference) => {
                if !self.check.record(layout_plausible(&cores, &reference)) {
                    return Ok(None);
                }
            }
            Err(error) => tracing::debug!(%error, "no CPU reference for the per-core check"),
        }
        Ok(Some(
            cores
                .iter()
                .map(|core| {
                    reading(
                        core.core,
                        format!("Core{} (CCD{})", core.core, core.ccd),
                        Some(core.celsius),
                    )
                })
                .collect(),
        ))
    }
}

impl IntelCores {
    fn read(&self) -> Vec<SensorReading> {
        self.cpu
            .core_temps(&self.cores)
            .into_iter()
            .zip(&self.labels)
            .enumerate()
            .map(|(index, (celsius, label))| reading(index, label.clone(), celsius))
            .collect()
    }
}

impl SensorSource for CoreTempSource {
    fn name(&self) -> &'static str {
        "cpu cores"
    }

    fn cadence(&self) -> Duration {
        CADENCE
    }

    fn read(&mut self) -> Result<Vec<SensorReading>, String> {
        if matches!(self.state, State::Closed) {
            self.state = open()?;
        }
        let readings = match &mut self.state {
            State::Open(Backend::Amd(amd)) => match amd.read() {
                Ok(Some(readings)) => readings,
                Ok(None) => {
                    tracing::warn!(
                        "per-core temperatures don't match the CPU temperature; the PM table layout looks wrong, so they are turned off until FanForge restarts"
                    );
                    self.state = State::Unsupported;
                    Vec::new()
                }
                Err(error) => {
                    self.state = State::Closed;
                    return Err(error);
                }
            },
            State::Open(Backend::Intel(intel)) => intel.read(),
            State::Closed | State::Unsupported => Vec::new(),
        };
        Ok(readings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cores(values: &[f32]) -> Vec<CoreTemp> {
        values
            .iter()
            .enumerate()
            .map(|(core, &celsius)| CoreTemp {
                core,
                ccd: core / 8 + 1,
                celsius,
            })
            .collect()
    }

    fn reference(tctl: f32, ccd: f32) -> CpuTemps {
        CpuTemps {
            tctl,
            tdie: None,
            ccds: vec![(0, ccd)],
        }
    }

    #[test]
    fn real_core_temperatures_pass_the_check() {
        let idle = cores(&[41.1, 41.5, 39.9, 39.3, 38.5, 38.3, 37.1, 39.5]);
        assert!(layout_plausible(&idle, &reference(47.0, 39.2)));
        let loaded = cores(&[78.0, 52.0, 50.0, 49.0, 48.0, 48.0, 47.0, 49.0]);
        assert!(layout_plausible(&loaded, &reference(72.0, 64.0)));
    }

    #[test]
    fn neighbouring_fields_fail_the_check() {
        let power = cores(&[2.1, 3.4, 0.9, 1.2, 0.8, 0.7, 0.5, 1.1]);
        assert!(!layout_plausible(&power, &reference(47.0, 39.2)));
        let clocks = cores(&[
            4050.0, 4050.0, 3600.0, 3600.0, 3600.0, 4050.0, 3600.0, 4050.0,
        ]);
        assert!(!layout_plausible(&clocks, &reference(47.0, 39.2)));
        let flat = cores(&[40.0; 8]);
        assert!(!layout_plausible(&flat, &reference(47.0, 39.2)));
        assert!(!layout_plausible(&[], &reference(47.0, 39.2)));
    }

    #[test]
    fn cores_without_a_ccd_sensor_use_tctl() {
        let second = cores(&[
            40.0, 41.0, 40.0, 41.0, 40.0, 41.0, 40.0, 41.0, 60.0, 61.0, 60.0, 61.0, 60.0, 61.0,
            60.0, 61.0,
        ]);
        assert!(layout_plausible(&second, &reference(62.0, 40.0)));
    }

    #[test]
    fn three_failed_checks_in_ten_turn_per_core_off() {
        let mut check = LayoutCheck::default();
        assert!(check.record(false));
        assert!(check.record(true));
        assert!(check.record(false));
        for _ in 0..7 {
            assert!(check.record(true));
        }
        assert!(check.record(false));
        let mut check = LayoutCheck::default();
        check.record(false);
        check.record(false);
        assert!(!check.record(false));
    }

    #[test]
    fn intel_labels_split_p_and_e_cores_only_on_hybrid_cpus() {
        let core = |efficiency_class| PhysicalCore {
            group: 0,
            first_cpu: 0,
            efficiency_class,
        };
        assert_eq!(intel_labels(&[core(0), core(0)]), vec!["Core 0", "Core 1"]);
        assert_eq!(
            intel_labels(&[core(1), core(1), core(0)]),
            vec!["P-core 0", "P-core 1", "E-core 2"]
        );
    }

    #[test]
    fn implausible_values_are_reported_as_missing() {
        assert_eq!(reading(2, "Core2".into(), Some(400.0)).celsius, None);
        assert_eq!(reading(2, "Core2".into(), Some(40.0)).id, SensorId::core(2));
    }
}
