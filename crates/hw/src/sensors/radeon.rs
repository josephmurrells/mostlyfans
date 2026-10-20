use std::collections::BTreeSet;
use std::ffi::{CStr, c_void};
use std::time::Duration;

use fanforge_core::SensorId;
use fanforge_core::telemetry::SensorReading;
use fanforge_win::{RawSymbol, SystemLibrary};
use windows::Win32::System::Memory::{GetProcessHeap, HEAP_FLAGS, HeapAlloc};

use super::{SensorSource, unique_labels};

const LIBRARY: &str = "atiadlxx.dll";
const CADENCE: Duration = Duration::from_secs(1);
const ADL_OK: i32 = 0;
const AMD_VENDOR_ID: i32 = 0x1002;
const ADAPTER_INFO_LEN: usize = 1572;
const PMLOG_SENSORS: usize = 256;
const PMLOG_LEN: usize = 4 + PMLOG_SENSORS * 8;
const PMLOG_TEMPERATURE_EDGE: usize = 8;
const ODN_TEMPERATURE_EDGE: i32 = 1;
const MAX_ADAPTERS: i32 = 256;
const ASIC_INTEGRATED: i32 = 1 << 1;

type Context = *mut c_void;
type MallocCallback = unsafe extern "system" fn(i32) -> *mut c_void;
type ControlCreate = unsafe extern "C" fn(MallocCallback, i32, *mut Context) -> i32;
type ControlDestroy = unsafe extern "C" fn(Context) -> i32;
type AdapterCount = unsafe extern "C" fn(Context, *mut i32) -> i32;
type AdapterInfoGet = unsafe extern "C" fn(Context, *mut u8, i32) -> i32;
type PmLogGet = unsafe extern "C" fn(Context, i32, *mut u8) -> i32;
type OverdriveTemperature = unsafe extern "C" fn(Context, i32, i32, *mut i32) -> i32;
type AsicFamily = unsafe extern "C" fn(Context, i32, *mut i32, *mut i32) -> i32;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterInfo {
    pub index: i32,
    pub bus: i32,
    pub device: i32,
    pub function: i32,
    pub vendor: i32,
    pub name: String,
    pub present: bool,
}

#[derive(Debug)]
struct Gpu {
    adapter: i32,
    id: SensorId,
    label: String,
}

struct Functions {
    destroy: ControlDestroy,
    pmlog: Option<PmLogGet>,
    overdrive: Option<OverdriveTemperature>,
}

struct Adl {
    context: Context,
    functions: Functions,
    gpus: Vec<Gpu>,
    _library: SystemLibrary,
}

unsafe impl Send for Adl {}

#[derive(Default)]
pub struct RadeonSource {
    adl: Option<Adl>,
}

unsafe extern "system" fn allocate(size: i32) -> *mut c_void {
    let Ok(size) = usize::try_from(size) else {
        return std::ptr::null_mut();
    };
    match unsafe { GetProcessHeap() } {
        Ok(heap) => unsafe { HeapAlloc(heap, HEAP_FLAGS(0), size) },
        Err(_) => std::ptr::null_mut(),
    }
}

fn i32_at(buffer: &[u8], offset: usize) -> Option<i32> {
    let bytes = buffer.get(offset..offset + 4)?;
    Some(i32::from_le_bytes(bytes.try_into().ok()?))
}

fn c_string(buffer: &[u8]) -> String {
    let len = buffer
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..len]).trim().to_owned()
}

pub fn parse_adapter_info(buffer: &[u8]) -> Option<AdapterInfo> {
    if buffer.len() < ADAPTER_INFO_LEN {
        return None;
    }
    Some(AdapterInfo {
        index: i32_at(buffer, 4)?,
        bus: i32_at(buffer, 264)?,
        device: i32_at(buffer, 268)?,
        function: i32_at(buffer, 272)?,
        vendor: i32_at(buffer, 276)?,
        name: c_string(&buffer[280..536]),
        present: i32_at(buffer, 792)? != 0,
    })
}

pub fn edge_temperature(pmlog: &[u8]) -> Option<f32> {
    let offset = 4 + PMLOG_TEMPERATURE_EDGE * 8;
    let supported = i32_at(pmlog, offset)?;
    let value = i32_at(pmlog, offset + 4)?;
    (supported != 0 && value > 0).then_some(value as f32)
}

pub fn is_integrated(asic_types: i32, valid: i32) -> bool {
    asic_types & valid & ASIC_INTEGRATED != 0
}

pub fn physical_gpus(adapters: Vec<AdapterInfo>) -> Vec<AdapterInfo> {
    let mut seen = BTreeSet::new();
    adapters
        .into_iter()
        .filter(|adapter| adapter.vendor == AMD_VENDOR_ID && adapter.present)
        .filter(|adapter| seen.insert((adapter.bus, adapter.device, adapter.function)))
        .collect()
}

pub fn sensor_id(adapter: &AdapterInfo) -> SensorId {
    SensorId::new(format!(
        "gpu/amd/pci-{}-{}-{}",
        adapter.bus, adapter.device, adapter.function
    ))
}

fn symbol(library: &SystemLibrary, name: &CStr) -> Result<RawSymbol, String> {
    library
        .symbol(name)
        .ok_or_else(|| format!("{LIBRARY} has no {}", name.to_string_lossy()))
}

fn open() -> Result<Adl, String> {
    let library = SystemLibrary::load(LIBRARY)
        .map_err(|error| format!("AMD driver not found ({LIBRARY}: {error})"))?;
    let create: ControlCreate =
        unsafe { std::mem::transmute(symbol(&library, c"ADL2_Main_Control_Create")?) };
    let destroy: ControlDestroy =
        unsafe { std::mem::transmute(symbol(&library, c"ADL2_Main_Control_Destroy")?) };
    let count: AdapterCount =
        unsafe { std::mem::transmute(symbol(&library, c"ADL2_Adapter_NumberOfAdapters_Get")?) };
    let info: AdapterInfoGet =
        unsafe { std::mem::transmute(symbol(&library, c"ADL2_Adapter_AdapterInfo_Get")?) };
    let pmlog: Option<PmLogGet> = library
        .symbol(c"ADL2_New_QueryPMLogData_Get")
        .map(|raw| unsafe { std::mem::transmute::<RawSymbol, PmLogGet>(raw) });
    let overdrive: Option<OverdriveTemperature> = library
        .symbol(c"ADL2_OverdriveN_Temperature_Get")
        .map(|raw| unsafe { std::mem::transmute::<RawSymbol, OverdriveTemperature>(raw) });
    let asic_family: Option<AsicFamily> = library
        .symbol(c"ADL2_Adapter_ASICFamilyType_Get")
        .map(|raw| unsafe { std::mem::transmute::<RawSymbol, AsicFamily>(raw) });

    let mut context: Context = std::ptr::null_mut();
    let status = unsafe { create(allocate, 1, &raw mut context) };
    if status != ADL_OK || context.is_null() {
        return Err(format!("ADL could not start (status {status})"));
    }
    let mut adl = Adl {
        context,
        functions: Functions {
            destroy,
            pmlog,
            overdrive,
        },
        gpus: Vec::new(),
        _library: library,
    };
    let mut adapters = 0;
    let status = unsafe { count(adl.context, &raw mut adapters) };
    if status != ADL_OK || !(0..=MAX_ADAPTERS).contains(&adapters) {
        return Err(format!("ADL adapter count failed (status {status})"));
    }
    let mut buffer = vec![0u8; adapters as usize * ADAPTER_INFO_LEN];
    if adapters > 0 {
        let status = unsafe { info(adl.context, buffer.as_mut_ptr(), buffer.len() as i32) };
        if status != ADL_OK {
            return Err(format!("ADL adapter list failed (status {status})"));
        }
    }
    let integrated = |adapter: i32| {
        let Some(asic_family) = asic_family else {
            return false;
        };
        let (mut types, mut valid) = (0, 0);
        let status = unsafe { asic_family(adl.context, adapter, &raw mut types, &raw mut valid) };
        status == ADL_OK && is_integrated(types, valid)
    };
    let gpus: Vec<AdapterInfo> = physical_gpus(
        buffer
            .as_chunks::<ADAPTER_INFO_LEN>()
            .0
            .iter()
            .filter_map(|chunk| parse_adapter_info(chunk))
            .collect(),
    )
    .into_iter()
    .filter(|gpu| !integrated(gpu.index))
    .collect();
    let labels = unique_labels(gpus.iter().map(|gpu| gpu.name.clone()).collect());
    adl.gpus = gpus
        .iter()
        .zip(labels)
        .map(|(gpu, label)| Gpu {
            adapter: gpu.index,
            id: sensor_id(gpu),
            label,
        })
        .collect();
    Ok(adl)
}

impl Adl {
    fn temperature(&self, adapter: i32) -> Option<f32> {
        if let Some(pmlog) = self.functions.pmlog {
            let mut buffer = vec![0u8; PMLOG_LEN];
            let status = unsafe { pmlog(self.context, adapter, buffer.as_mut_ptr()) };
            if status == ADL_OK
                && let Some(celsius) = edge_temperature(&buffer)
            {
                return Some(celsius);
            }
        }
        let overdrive = self.functions.overdrive?;
        let mut millidegrees = 0;
        let status = unsafe {
            overdrive(
                self.context,
                adapter,
                ODN_TEMPERATURE_EDGE,
                &raw mut millidegrees,
            )
        };
        (status == ADL_OK && millidegrees > 0).then(|| millidegrees as f32 / 1000.0)
    }

    fn read(&self) -> Result<Vec<SensorReading>, String> {
        let readings: Vec<SensorReading> = self
            .gpus
            .iter()
            .filter_map(|gpu| {
                self.temperature(gpu.adapter).map(|celsius| SensorReading {
                    id: gpu.id.clone(),
                    label: gpu.label.clone(),
                    celsius: Some(celsius),
                })
            })
            .collect();
        if readings.is_empty() && !self.gpus.is_empty() {
            return Err("no AMD GPU reported a temperature".to_owned());
        }
        Ok(readings)
    }
}

impl Drop for Adl {
    fn drop(&mut self) {
        unsafe { (self.functions.destroy)(self.context) };
    }
}

impl SensorSource for RadeonSource {
    fn name(&self) -> &'static str {
        "radeon"
    }

    fn cadence(&self) -> Duration {
        CADENCE
    }

    fn read(&mut self) -> Result<Vec<SensorReading>, String> {
        let adl = match self.adl.take() {
            Some(adl) => adl,
            None => open()?,
        };
        let result = adl.read();
        if result.is_ok() {
            self.adl = Some(adl);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter_bytes(index: i32, bus: i32, vendor: i32, name: &str, present: bool) -> Vec<u8> {
        let mut buffer = vec![0u8; ADAPTER_INFO_LEN];
        buffer[0..4].copy_from_slice(&(ADAPTER_INFO_LEN as i32).to_le_bytes());
        buffer[4..8].copy_from_slice(&index.to_le_bytes());
        buffer[264..268].copy_from_slice(&bus.to_le_bytes());
        buffer[276..280].copy_from_slice(&vendor.to_le_bytes());
        buffer[280..280 + name.len()].copy_from_slice(name.as_bytes());
        buffer[792..796].copy_from_slice(&i32::from(present).to_le_bytes());
        buffer
    }

    #[test]
    fn adapter_info_is_decoded_from_the_windows_layout() {
        let info = parse_adapter_info(&adapter_bytes(
            3,
            9,
            AMD_VENDOR_ID,
            "AMD Radeon RX 7800 XT",
            true,
        ))
        .unwrap();
        assert_eq!(
            info,
            AdapterInfo {
                index: 3,
                bus: 9,
                device: 0,
                function: 0,
                vendor: AMD_VENDOR_ID,
                name: "AMD Radeon RX 7800 XT".into(),
                present: true,
            }
        );
        assert_eq!(sensor_id(&info).as_str(), "gpu/amd/pci-9-0-0");
        assert_eq!(parse_adapter_info(&[0u8; 100]), None);
    }

    #[test]
    fn outputs_of_one_card_collapse_to_one_gpu() {
        let adapters = [
            adapter_bytes(0, 9, AMD_VENDOR_ID, "RX", true),
            adapter_bytes(1, 9, AMD_VENDOR_ID, "RX", true),
            adapter_bytes(2, 3, 0x10DE, "Other", true),
            adapter_bytes(3, 12, AMD_VENDOR_ID, "RX", false),
            adapter_bytes(4, 14, AMD_VENDOR_ID, "RX", true),
        ]
        .iter()
        .filter_map(|bytes| parse_adapter_info(bytes))
        .collect();
        let gpus = physical_gpus(adapters);
        assert_eq!(
            gpus.iter().map(|gpu| gpu.index).collect::<Vec<_>>(),
            vec![0, 4]
        );
    }

    #[test]
    fn pmlog_edge_temperature_needs_the_supported_flag() {
        let mut buffer = vec![0u8; PMLOG_LEN];
        let offset = 4 + PMLOG_TEMPERATURE_EDGE * 8;
        buffer[offset + 4..offset + 8].copy_from_slice(&61i32.to_le_bytes());
        assert_eq!(edge_temperature(&buffer), None);
        buffer[offset..offset + 4].copy_from_slice(&1i32.to_le_bytes());
        assert_eq!(edge_temperature(&buffer), Some(61.0));
        assert_eq!(edge_temperature(&buffer[..offset]), None);
    }

    #[test]
    fn a_zero_edge_temperature_is_a_failure() {
        let mut buffer = vec![0u8; PMLOG_LEN];
        let offset = 4 + PMLOG_TEMPERATURE_EDGE * 8;
        buffer[offset..offset + 4].copy_from_slice(&1i32.to_le_bytes());
        assert_eq!(edge_temperature(&buffer), None);
        buffer[offset + 4..offset + 8].copy_from_slice(&(-3i32).to_le_bytes());
        assert_eq!(edge_temperature(&buffer), None);
    }

    #[test]
    fn integrated_gpus_are_recognised_only_when_flagged_valid() {
        assert!(is_integrated(ASIC_INTEGRATED, ASIC_INTEGRATED));
        assert!(!is_integrated(ASIC_INTEGRATED, 1));
        assert!(!is_integrated(1, ASIC_INTEGRATED | 1));
    }

    #[test]
    fn a_missing_amd_driver_is_a_clean_error() {
        let mut source = RadeonSource::default();
        if let Err(error) = source.read() {
            assert!(!error.is_empty());
        }
    }
}
