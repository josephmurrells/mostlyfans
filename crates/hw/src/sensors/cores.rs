use std::time::Duration;

use fanforge_core::SensorId;
use fanforge_core::telemetry::{SensorReading, is_plausible};
use fanforge_win::topology::{self, PhysicalCore};

use super::SensorSource;
use crate::HwError;
use crate::amd::{AmdCpu, CpuInfo, CpuTemps};
use crate::intel::IntelCpu;
use crate::smu::{CoreTemp, CoreTempLayout, RyzenSmu};

const CADENCE: Duration = Duration::from_secs(2);
const CORE_FLOOR: f32 = 10.0;
const HOTTEST_BELOW_REFERENCE: f32 = 15.0;
const HOTTEST_ABOVE_REFERENCE: f32 = 15.0;
const FAILED_CHECKS_ALLOWED: u32 = 2;
const UNCHECKED_ROUNDS_ALLOWED: u32 = 2;

#[derive(Debug, Default)]
pub struct LayoutCheck {
    failures: u32,
}

impl LayoutCheck {
    pub fn record(&mut self, passed: bool) -> bool {
        self.failures = if passed { 0 } else { self.failures + 1 };
        self.failures <= FAILED_CHECKS_ALLOWED
    }
}

pub fn layout_plausible(cores: &[CoreTemp], reference: &CpuTemps) -> bool {
    let all_equal = cores
        .windows(2)
        .all(|pair| pair[0].celsius == pair[1].celsius);
    if cores.len() < 2
        || all_equal
        || cores
            .iter()
            .any(|core| !is_plausible(core.celsius) || core.celsius < CORE_FLOOR)
    {
        return false;
    }
    let mut ccds: Vec<usize> = cores.iter().map(|core| core.ccd).collect();
    ccds.dedup();
    ccds.into_iter().all(|ccd| {
        let hottest = cores
            .iter()
            .filter(|core| core.ccd == ccd)
            .map(|core| core.celsius)
            .fold(f32::MIN, f32::max);
        let measured = reference
            .ccds
            .iter()
            .find(|(index, _)| *index as usize + 1 == ccd)
            .map_or(reference.tctl, |(_, celsius)| *celsius);
        hottest >= measured - HOTTEST_BELOW_REFERENCE
            && hottest <= measured.max(reference.tctl) + HOTTEST_ABOVE_REFERENCE
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
    unchecked: u32,
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

enum AmdRead {
    Readings(Vec<SensorReading>),
    WrongLayout,
}

fn unsupported(reason: &str) -> Result<State, String> {
    tracing::info!(reason, "per-core temperatures are not shown");
    Ok(State::Unsupported)
}

pub fn worth_retrying(error: &HwError) -> bool {
    matches!(
        error,
        HwError::PawnIoMissing
            | HwError::AccessDenied
            | HwError::LockTimeout(_)
            | HwError::Mutex { .. }
    )
}

fn opened<T>(result: Result<T, HwError>) -> Result<Result<T, State>, String> {
    match result {
        Ok(value) => Ok(Ok(value)),
        Err(error) if worth_retrying(&error) => Err(error.to_string()),
        Err(error) => unsupported(&error.to_string()).map(Err),
    }
}

fn open() -> Result<State, String> {
    let cores = topology::physical_cores().map_err(|error| error.to_string())?;
    match CpuInfo::detect().vendor.as_str() {
        "AuthenticAMD" => {
            let smu = match opened(RyzenSmu::open())? {
                Ok(smu) => smu,
                Err(state) => return Ok(state),
            };
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
            let cpu = match opened(AmdCpu::open())? {
                Ok(cpu) => cpu,
                Err(state) => return Ok(state),
            };
            Ok(State::Open(Backend::Amd(Box::new(AmdCores {
                smu,
                cpu,
                layout,
                check: LayoutCheck::default(),
                unchecked: 0,
            }))))
        }
        "GenuineIntel" => {
            if cores.is_empty() {
                return unsupported("Windows reported no cores");
            }
            let cpu = match opened(IntelCpu::open())? {
                Ok(cpu) => cpu,
                Err(state) => return Ok(state),
            };
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
    fn read(&mut self) -> Result<AmdRead, HwError> {
        let reference = self.cpu.read();
        let table = self.smu.read_pm_table()?;
        let Some(cores) = self.layout.core_temps(&table) else {
            return Ok(AmdRead::WrongLayout);
        };
        match reference {
            Ok(reference) => {
                self.unchecked = 0;
                if !self.check.record(layout_plausible(&cores, &reference)) {
                    return Ok(AmdRead::WrongLayout);
                }
            }
            Err(error) => {
                tracing::debug!(%error, "no CPU reference for the per-core check");
                self.unchecked += 1;
                if self.unchecked > UNCHECKED_ROUNDS_ALLOWED {
                    return Ok(AmdRead::Readings(Vec::new()));
                }
            }
        }
        Ok(AmdRead::Readings(
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
                Ok(AmdRead::Readings(readings)) => readings,
                Ok(AmdRead::WrongLayout) => {
                    tracing::warn!(
                        "per-core temperatures don't match the CPU temperature; the PM table layout looks wrong, so they are turned off until FanForge restarts"
                    );
                    self.state = State::Unsupported;
                    Vec::new()
                }
                Err(error) => {
                    if !matches!(error, HwError::LockTimeout(_)) {
                        self.state = State::Closed;
                    }
                    return Err(error.to_string());
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
    fn one_hot_core_with_idle_siblings_passes_the_check() {
        let single_thread = cores(&[82.0, 44.0, 43.0, 42.0, 41.0, 41.0, 40.0, 42.0]);
        assert!(layout_plausible(&single_thread, &reference(84.0, 80.0)));
    }

    #[test]
    fn neighbouring_fields_fail_the_check() {
        let power = cores(&[2.1, 3.4, 0.9, 1.2, 0.8, 0.7, 0.5, 1.1]);
        assert!(!layout_plausible(&power, &reference(47.0, 39.2)));
        let clocks = cores(&[
            4050.0, 4050.0, 3600.0, 3600.0, 3600.0, 4050.0, 3600.0, 4050.0,
        ]);
        assert!(!layout_plausible(&clocks, &reference(47.0, 39.2)));
        let residency = cores(&[100.0, 98.0, 97.0, 99.0, 95.0, 96.0, 99.0, 100.0]);
        assert!(!layout_plausible(&residency, &reference(47.0, 39.2)));
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
    fn three_failed_checks_in_a_row_turn_per_core_off() {
        let mut check = LayoutCheck::default();
        assert!(check.record(false));
        assert!(check.record(false));
        assert!(check.record(true));
        assert!(check.record(false));
        assert!(check.record(false));
        assert!(!check.record(false));
    }

    #[test]
    fn only_missing_access_is_retried() {
        assert!(worth_retrying(&HwError::PawnIoMissing));
        assert!(worth_retrying(&HwError::LockTimeout("pci".into())));
        assert!(!worth_retrying(&HwError::UnsupportedCpu("x".into())));
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
