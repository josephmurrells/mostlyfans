use std::io;
use std::os::windows::io::OwnedHandle;

use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    IOCTL_STORAGE_PREDICT_FAILURE, IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery,
    STORAGE_PROPERTY_ID, STORAGE_PROPERTY_QUERY, StorageDeviceProperty,
    StorageDeviceSeekPenaltyProperty, StorageDeviceTemperatureProperty,
};
use windows::core::HSTRING;

use crate::handle::{borrow_handle, own_handle};

pub const BUS_TYPE_NVME: u32 = 17;
pub const MAX_PHYSICAL_DRIVES: u32 = 32;

const SMART_TEMPERATURE: u8 = 194;
const SMART_AIRFLOW_TEMPERATURE: u8 = 190;
const SMART_TABLE_OFFSET: usize = 4 + 2;
const SMART_ENTRIES: usize = 30;
const SMART_ENTRY_LEN: usize = 12;
const SMART_RAW_OFFSET: usize = 5;
const TEMPERATURE_HEADER_LEN: usize = 24;
const TEMPERATURE_INFO_LEN: usize = 16;
const DEVICE_PROTOCOL_SPECIFIC_PROPERTY: u32 = 50;
const PROTOCOL_TYPE_NVME: u32 = 3;
const NVME_DATA_TYPE_LOG_PAGE: u32 = 2;
const NVME_HEALTH_LOG: u32 = 2;
const PROTOCOL_DATA_LEN: usize = 40;
const QUERY_HEADER_LEN: usize = 8;
const HEALTH_LOG_LEN: usize = 512;
const HEALTH_SENSORS_OFFSET: usize = 200;
const HEALTH_SENSOR_COUNT: u16 = 8;
const KELVIN_OFFSET: i32 = 273;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriveDescriptor {
    pub model: String,
    pub serial: String,
    pub bus_type: u32,
}

#[derive(Debug)]
pub struct Drive {
    handle: OwnedHandle,
    index: u32,
}

impl Drive {
    pub fn open(index: u32) -> io::Result<Self> {
        let path = HSTRING::from(format!(r"\\.\PhysicalDrive{index}"));
        let raw = unsafe {
            CreateFileW(
                &path,
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        }?;
        let handle = unsafe { own_handle(raw) }.ok_or_else(io::Error::last_os_error)?;
        Ok(Self { handle, index })
    }

    pub fn index(&self) -> u32 {
        self.index
    }

    pub fn describe(&self) -> io::Result<DriveDescriptor> {
        let buffer = self.query_property(StorageDeviceProperty, 1024)?;
        parse_device_descriptor(&buffer).ok_or_else(|| invalid("malformed device descriptor"))
    }

    pub fn incurs_seek_penalty(&self) -> io::Result<bool> {
        let buffer = self.query_property(StorageDeviceSeekPenaltyProperty, 12)?;
        parse_seek_penalty(&buffer).ok_or_else(|| invalid("malformed seek penalty descriptor"))
    }

    pub fn temperature(&self) -> io::Result<i16> {
        let reported = self
            .query_property(StorageDeviceTemperatureProperty, 512)
            .ok()
            .and_then(|buffer| parse_temperature_descriptor(&buffer));
        if let Some(celsius) = reported {
            return Ok(celsius);
        }
        let smart = self.control(IOCTL_STORAGE_PREDICT_FAILURE, None, 4 + 512)?;
        parse_smart_temperature(&smart).ok_or_else(|| invalid("drive reports no temperature"))
    }

    pub fn temperature_sensors(&self) -> io::Result<Vec<(u16, i16)>> {
        let buffer = self.query_property(StorageDeviceTemperatureProperty, 512)?;
        Ok(parse_temperature_entries(&buffer))
    }

    pub fn nvme_health_sensors(&self) -> io::Result<Vec<(u16, i16)>> {
        let mut query = vec![0u8; QUERY_HEADER_LEN + PROTOCOL_DATA_LEN + HEALTH_LOG_LEN];
        let put = |query: &mut Vec<u8>, offset: usize, value: u32| {
            query[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        };
        put(&mut query, 0, DEVICE_PROTOCOL_SPECIFIC_PROPERTY);
        put(&mut query, 4, PropertyStandardQuery.0 as u32);
        let data = QUERY_HEADER_LEN;
        put(&mut query, data, PROTOCOL_TYPE_NVME);
        put(&mut query, data + 4, NVME_DATA_TYPE_LOG_PAGE);
        put(&mut query, data + 8, NVME_HEALTH_LOG);
        put(&mut query, data + 16, PROTOCOL_DATA_LEN as u32);
        put(&mut query, data + 20, HEALTH_LOG_LEN as u32);
        let len = query.len();
        let output = self.control(IOCTL_STORAGE_QUERY_PROPERTY, Some(&query), len)?;
        parse_nvme_health(&output).ok_or_else(|| invalid("malformed NVMe health log"))
    }

    fn query_property(&self, property: STORAGE_PROPERTY_ID, len: usize) -> io::Result<Vec<u8>> {
        let query = STORAGE_PROPERTY_QUERY {
            PropertyId: property,
            QueryType: PropertyStandardQuery,
            AdditionalParameters: [0],
        };
        let input = unsafe {
            std::slice::from_raw_parts(
                (&raw const query).cast::<u8>(),
                size_of::<STORAGE_PROPERTY_QUERY>(),
            )
        };
        self.control(IOCTL_STORAGE_QUERY_PROPERTY, Some(input), len)
    }

    fn control(&self, code: u32, input: Option<&[u8]>, len: usize) -> io::Result<Vec<u8>> {
        let mut output = vec![0u8; len];
        let mut returned = 0u32;
        unsafe {
            DeviceIoControl(
                borrow_handle(&self.handle),
                code,
                input.map(|input| input.as_ptr().cast()),
                input.map_or(0, |input| input.len() as u32),
                Some(output.as_mut_ptr().cast()),
                output.len() as u32,
                Some(&mut returned),
                None,
            )
        }?;
        output.truncate(returned as usize);
        Ok(output)
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn u32_at(buffer: &[u8], offset: usize) -> Option<u32> {
    let bytes = buffer.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

fn u16_at(buffer: &[u8], offset: usize) -> Option<u16> {
    let bytes = buffer.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes(bytes.try_into().ok()?))
}

fn string_at(buffer: &[u8], offset: u32) -> String {
    let offset = offset as usize;
    if offset == 0 {
        return String::new();
    }
    let Some(tail) = buffer.get(offset..) else {
        return String::new();
    };
    let len = tail
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(tail.len());
    String::from_utf8_lossy(&tail[..len]).trim().to_owned()
}

pub fn parse_device_descriptor(buffer: &[u8]) -> Option<DriveDescriptor> {
    let vendor = string_at(buffer, u32_at(buffer, 12)?);
    let product = string_at(buffer, u32_at(buffer, 16)?);
    let serial = string_at(buffer, u32_at(buffer, 24)?);
    let bus_type = u32_at(buffer, 28)?;
    let model = [vendor, product]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    Some(DriveDescriptor {
        model,
        serial,
        bus_type,
    })
}

pub fn parse_seek_penalty(buffer: &[u8]) -> Option<bool> {
    buffer.get(8).map(|&flag| flag != 0)
}

pub fn parse_temperature_descriptor(buffer: &[u8]) -> Option<i16> {
    let count = usize::from(u16_at(buffer, 12)?);
    (0..count).find_map(|entry| {
        let offset = TEMPERATURE_HEADER_LEN + entry * TEMPERATURE_INFO_LEN;
        let info = buffer.get(offset..offset + TEMPERATURE_INFO_LEN)?;
        let index = u16::from_le_bytes([info[0], info[1]]);
        (index == 0).then(|| i16::from_le_bytes([info[2], info[3]]))
    })
}

pub fn parse_temperature_entries(buffer: &[u8]) -> Vec<(u16, i16)> {
    let count = u16_at(buffer, 12).map_or(0, usize::from);
    (0..count)
        .map_while(|entry| {
            let offset = TEMPERATURE_HEADER_LEN + entry * TEMPERATURE_INFO_LEN;
            let info = buffer.get(offset..offset + TEMPERATURE_INFO_LEN)?;
            Some((
                u16::from_le_bytes([info[0], info[1]]),
                i16::from_le_bytes([info[2], info[3]]),
            ))
        })
        .collect()
}

pub fn parse_nvme_health(buffer: &[u8]) -> Option<Vec<(u16, i16)>> {
    let offset = u32_at(buffer, QUERY_HEADER_LEN + 16)? as usize;
    let log = buffer.get(QUERY_HEADER_LEN + offset..)?;
    let words = (1..=HEALTH_SENSOR_COUNT)
        .map(|sensor| {
            u16_at(log, HEALTH_SENSORS_OFFSET + usize::from(sensor - 1) * 2)
                .map(|kelvin| (sensor, kelvin))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(
        words
            .into_iter()
            .filter(|(_, kelvin)| *kelvin != 0)
            .filter_map(|(sensor, kelvin)| {
                i16::try_from(i32::from(kelvin) - KELVIN_OFFSET)
                    .ok()
                    .map(|celsius| (sensor, celsius))
            })
            .collect(),
    )
}

pub fn parse_smart_temperature(buffer: &[u8]) -> Option<i16> {
    let table =
        buffer.get(SMART_TABLE_OFFSET..SMART_TABLE_OFFSET + SMART_ENTRIES * SMART_ENTRY_LEN)?;
    let raw = |id: u8| {
        table
            .as_chunks::<SMART_ENTRY_LEN>()
            .0
            .iter()
            .find(|entry| entry[0] == id)
            .map(|entry| entry[SMART_RAW_OFFSET])
            .filter(|&celsius| celsius != 0)
    };
    raw(SMART_TEMPERATURE)
        .or_else(|| raw(SMART_AIRFLOW_TEMPERATURE))
        .map(i16::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device_descriptor(vendor: &str, product: &str, serial: &str, bus: u32) -> Vec<u8> {
        let mut buffer = vec![0u8; 40];
        let put = |buffer: &mut Vec<u8>, field: usize, text: &str| {
            if text.is_empty() {
                return;
            }
            let offset = buffer.len() as u32;
            buffer[field..field + 4].copy_from_slice(&offset.to_le_bytes());
            buffer.extend_from_slice(text.as_bytes());
            buffer.push(0);
        };
        put(&mut buffer, 12, vendor);
        put(&mut buffer, 16, product);
        put(&mut buffer, 24, serial);
        buffer[28..32].copy_from_slice(&bus.to_le_bytes());
        buffer
    }

    fn temperature_descriptor(entries: &[(u16, i16)]) -> Vec<u8> {
        let mut buffer = vec![0u8; TEMPERATURE_HEADER_LEN];
        buffer[12..14].copy_from_slice(&(entries.len() as u16).to_le_bytes());
        for (index, celsius) in entries {
            let mut info = [0u8; TEMPERATURE_INFO_LEN];
            info[0..2].copy_from_slice(&index.to_le_bytes());
            info[2..4].copy_from_slice(&celsius.to_le_bytes());
            buffer.extend_from_slice(&info);
        }
        buffer
    }

    fn smart_table(attributes: &[(u8, u8)]) -> Vec<u8> {
        let mut buffer = vec![0u8; 4 + 512];
        for (slot, (id, raw)) in attributes.iter().enumerate() {
            let offset = SMART_TABLE_OFFSET + slot * SMART_ENTRY_LEN;
            buffer[offset] = *id;
            buffer[offset + 3] = 100;
            buffer[offset + SMART_RAW_OFFSET] = *raw;
        }
        buffer
    }

    fn health_log(sensors: &[u16]) -> Vec<u8> {
        let mut buffer = vec![0u8; QUERY_HEADER_LEN + PROTOCOL_DATA_LEN + HEALTH_LOG_LEN];
        buffer[QUERY_HEADER_LEN + 16..QUERY_HEADER_LEN + 20]
            .copy_from_slice(&(PROTOCOL_DATA_LEN as u32).to_le_bytes());
        let log = QUERY_HEADER_LEN + PROTOCOL_DATA_LEN;
        buffer[log + 1..log + 3].copy_from_slice(&312u16.to_le_bytes());
        for (slot, kelvin) in sensors.iter().enumerate() {
            let offset = log + HEALTH_SENSORS_OFFSET + slot * 2;
            buffer[offset..offset + 2].copy_from_slice(&kelvin.to_le_bytes());
        }
        buffer
    }

    #[test]
    fn every_temperature_entry_is_kept_with_its_index() {
        let buffer = temperature_descriptor(&[(0, 39), (2, 47), (1, 44)]);
        assert_eq!(
            parse_temperature_entries(&buffer),
            vec![(0, 39), (2, 47), (1, 44)]
        );
        let mut truncated = temperature_descriptor(&[(0, 39), (1, 44)]);
        truncated.truncate(TEMPERATURE_HEADER_LEN + TEMPERATURE_INFO_LEN);
        assert_eq!(parse_temperature_entries(&truncated), vec![(0, 39)]);
    }

    #[test]
    fn health_log_sensors_skip_unimplemented_slots_and_convert_kelvin() {
        assert_eq!(parse_nvme_health(&health_log(&[])), Some(Vec::new()));
        assert_eq!(
            parse_nvme_health(&health_log(&[318, 0, 314])),
            Some(vec![(1, 45), (3, 41)])
        );
        assert_eq!(
            parse_nvme_health(&health_log(&[318, 0xFFFF, 314])),
            Some(vec![(1, 45), (3, 41)])
        );
        assert_eq!(parse_nvme_health(&[0u8; 20]), None);
    }

    #[test]
    fn device_descriptor_strings_are_trimmed_and_joined() {
        let buffer = device_descriptor("", "  Sabrent Rocket Q4 ", "6479_A73A_5332_338D.", 17);
        assert_eq!(
            parse_device_descriptor(&buffer),
            Some(DriveDescriptor {
                model: "Sabrent Rocket Q4".to_owned(),
                serial: "6479_A73A_5332_338D.".to_owned(),
                bus_type: BUS_TYPE_NVME,
            })
        );
        let buffer = device_descriptor("ATA", "Samsung SSD", "S1DB", 11);
        assert_eq!(
            parse_device_descriptor(&buffer).unwrap().model,
            "ATA Samsung SSD"
        );
    }

    #[test]
    fn device_descriptor_offsets_out_of_range_give_empty_strings() {
        let mut buffer = device_descriptor("", "Model", "", 11);
        buffer[24..28].copy_from_slice(&5000u32.to_le_bytes());
        let descriptor = parse_device_descriptor(&buffer).unwrap();
        assert_eq!(descriptor.serial, "");
        assert_eq!(descriptor.model, "Model");
        assert_eq!(parse_device_descriptor(&buffer[..20]), None);
    }

    #[test]
    fn temperature_descriptor_picks_the_composite_entry() {
        assert_eq!(
            parse_temperature_descriptor(&temperature_descriptor(&[(1, 50), (0, 35)])),
            Some(35)
        );
        assert_eq!(
            parse_temperature_descriptor(&temperature_descriptor(&[(1, 50)])),
            None
        );
        assert_eq!(
            parse_temperature_descriptor(&temperature_descriptor(&[])),
            None
        );
    }

    #[test]
    fn temperature_descriptor_count_is_bounded_by_the_buffer() {
        let mut buffer = temperature_descriptor(&[(0, 35)]);
        buffer[12..14].copy_from_slice(&9u16.to_le_bytes());
        assert_eq!(parse_temperature_descriptor(&buffer), Some(35));
        buffer.truncate(TEMPERATURE_HEADER_LEN + 4);
        assert_eq!(parse_temperature_descriptor(&buffer), None);
        assert_eq!(parse_temperature_descriptor(&buffer[..10]), None);
    }

    #[test]
    fn smart_prefers_attribute_194_over_190() {
        assert_eq!(
            parse_smart_temperature(&smart_table(&[(190, 28), (194, 31)])),
            Some(31)
        );
        assert_eq!(
            parse_smart_temperature(&smart_table(&[(9, 77), (190, 28)])),
            Some(28)
        );
    }

    #[test]
    fn smart_without_a_usable_temperature_is_a_failure() {
        assert_eq!(parse_smart_temperature(&smart_table(&[])), None);
        assert_eq!(parse_smart_temperature(&smart_table(&[(194, 0)])), None);
        assert_eq!(
            parse_smart_temperature(&smart_table(&[(194, 0), (190, 30)])),
            Some(30)
        );
        assert_eq!(
            parse_smart_temperature(&smart_table(&[(190, 28)])[..100]),
            None
        );
    }

    #[test]
    fn seek_penalty_flag_is_decoded() {
        assert_eq!(
            parse_seek_penalty(&[12, 0, 0, 0, 12, 0, 0, 0, 1]),
            Some(true)
        );
        assert_eq!(
            parse_seek_penalty(&[12, 0, 0, 0, 12, 0, 0, 0, 0]),
            Some(false)
        );
        assert_eq!(parse_seek_penalty(&[12, 0, 0, 0]), None);
    }
}
