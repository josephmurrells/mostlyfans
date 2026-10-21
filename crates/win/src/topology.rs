use std::io;

use windows::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
use windows::Win32::System::SystemInformation::{
    GetLogicalProcessorInformationEx, RelationProcessorCore,
    SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX,
};

const RECORD_HEADER: usize = 8;
const EFFICIENCY_CLASS: usize = RECORD_HEADER + 1;
const GROUP_COUNT: usize = RECORD_HEADER + 22;
const GROUP_MASKS: usize = RECORD_HEADER + 24;
const GROUP_AFFINITY_LEN: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhysicalCore {
    pub group: u16,
    pub first_cpu: u8,
    pub efficiency_class: u8,
}

pub fn physical_cores() -> io::Result<Vec<PhysicalCore>> {
    let mut len = 0u32;
    let probe = unsafe { GetLogicalProcessorInformationEx(RelationProcessorCore, None, &mut len) };
    match probe {
        Err(error) if error.code() == ERROR_INSUFFICIENT_BUFFER.to_hresult() => {}
        Err(error) => return Err(error.into()),
        Ok(()) => return Ok(Vec::new()),
    }
    let mut buffer = vec![0u64; (len as usize).div_ceil(size_of::<u64>())];
    unsafe {
        GetLogicalProcessorInformationEx(
            RelationProcessorCore,
            Some(
                buffer
                    .as_mut_ptr()
                    .cast::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>(),
            ),
            &mut len,
        )
    }?;
    let bytes = unsafe { std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), len as usize) };
    Ok(parse_cores(bytes))
}

pub fn parse_cores(mut bytes: &[u8]) -> Vec<PhysicalCore> {
    let mut cores = Vec::new();
    while bytes.len() >= RECORD_HEADER {
        let size = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
        if size < RECORD_HEADER || size > bytes.len() {
            break;
        }
        let record = &bytes[..size];
        bytes = &bytes[size..];
        if record.len() < GROUP_MASKS + GROUP_AFFINITY_LEN
            || u16::from_le_bytes([record[GROUP_COUNT], record[GROUP_COUNT + 1]]) == 0
        {
            continue;
        }
        let mask = u64::from_le_bytes(
            record[GROUP_MASKS..GROUP_MASKS + 8]
                .try_into()
                .unwrap_or_default(),
        );
        if mask == 0 {
            continue;
        }
        cores.push(PhysicalCore {
            group: u16::from_le_bytes([record[GROUP_MASKS + 8], record[GROUP_MASKS + 9]]),
            first_cpu: mask.trailing_zeros() as u8,
            efficiency_class: record[EFFICIENCY_CLASS],
        });
    }
    cores
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(efficiency_class: u8, group: u16, mask: u64) -> Vec<u8> {
        let mut record = vec![0u8; GROUP_MASKS + GROUP_AFFINITY_LEN];
        record[0..4].copy_from_slice(&0u32.to_le_bytes());
        let size = record.len() as u32;
        record[4..8].copy_from_slice(&size.to_le_bytes());
        record[EFFICIENCY_CLASS] = efficiency_class;
        record[GROUP_COUNT..GROUP_COUNT + 2].copy_from_slice(&1u16.to_le_bytes());
        record[GROUP_MASKS..GROUP_MASKS + 8].copy_from_slice(&mask.to_le_bytes());
        record[GROUP_MASKS + 8..GROUP_MASKS + 10].copy_from_slice(&group.to_le_bytes());
        record
    }

    #[test]
    fn hyper_threading_siblings_are_one_core() {
        let bytes = [record(1, 0, 0b11), record(1, 0, 0b1100), record(0, 1, 0b1)].concat();
        assert_eq!(
            parse_cores(&bytes),
            vec![
                PhysicalCore {
                    group: 0,
                    first_cpu: 0,
                    efficiency_class: 1
                },
                PhysicalCore {
                    group: 0,
                    first_cpu: 2,
                    efficiency_class: 1
                },
                PhysicalCore {
                    group: 1,
                    first_cpu: 0,
                    efficiency_class: 0
                },
            ]
        );
    }

    #[test]
    fn truncated_records_stop_parsing() {
        let mut bytes = record(0, 0, 1);
        bytes.truncate(20);
        assert!(parse_cores(&bytes).is_empty());
    }

    #[test]
    fn this_machine_has_cores() {
        assert!(!physical_cores().unwrap().is_empty());
    }
}
