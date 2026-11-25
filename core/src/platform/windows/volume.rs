//! Sticks, SD cards and drives on Windows: the removable volumes, plus fixed volumes on a USB
//! bus, with what Windows says about each. The system drive is never offered.
//!
//! Everything here only asks questions:
//! - drive letters, type, label, file system, serial and size from the volume functions;
//! - the volume GUID path from the mount manager;
//! - the bus type, vendor, product and serial from a storage query;
//! - the GPT partition ID from a partition query;
//! - the USB vendor, product and serial from the USB parent of the disk (Plug and Play).
//!
//! Handles for the queries are opened with zero access rights, so no admin rights are needed
//! and nothing can be written through them. The only control codes sent are the three query
//! codes named below.

use std::mem::{size_of, zeroed};

use log::{debug, info};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Device_IDW, CM_Get_Parent, CR_SUCCESS, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW,
    GetVolumeNameForVolumeMountPointW, OPEN_EXISTING,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::System::Ioctl::{
    GUID_DEVINTERFACE_DISK, IOCTL_DISK_GET_PARTITION_INFO_EX, IOCTL_STORAGE_GET_DEVICE_NUMBER,
    IOCTL_STORAGE_QUERY_PROPERTY, PARTITION_INFORMATION_EX, PARTITION_STYLE_GPT,
    PropertyStandardQuery, STORAGE_DEVICE_DESCRIPTOR, STORAGE_DEVICE_NUMBER,
    STORAGE_PROPERTY_QUERY, StorageDeviceProperty,
};
use windows::core::{GUID, PCWSTR};

use crate::device::DeviceSource;
use crate::platform::ListedDevice;
use crate::signals::{Signals, UsbIds, parse_windows_usb_instance_id};
use crate::volume::{VolumeDetails, open_listed};

const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_FIXED: u32 = 3;
/// `STORAGE_BUS_TYPE` value for USB (`BusTypeUsb`).
const BUS_TYPE_USB: i32 = 7;

/// The mounted volumes that could be a source.
pub fn list() -> Vec<ListedDevice> {
    // SAFETY: plain query with no arguments.
    let mask = unsafe { GetLogicalDrives() };
    candidate_letters(mask, &system_drive())
        .into_iter()
        .filter_map(identify)
        .map(|v| v.listed())
        .collect()
}

/// Opens a listed volume as a read-only source. The volume is looked at again first, so only
/// a drive that is offered now (not the system drive) can be opened.
pub fn open(device: &ListedDevice) -> Result<Box<dyn DeviceSource>, String> {
    let letter = letter_of_root(&device.id)
        .filter(|l| candidate_letters(!0, &system_drive()).contains(l))
        .ok_or_else(|| format!("{} is not a drive that can be backed up", device.id))?;
    match identify(letter) {
        Some(v) => open_listed(v),
        None => Err(format!("{} is no longer available", device.id)),
    }
}

/// The system drive's letter, from `%SystemDrive%` (`C` when it isn't set).
fn system_drive() -> char {
    std::env::var("SystemDrive")
        .ok()
        .and_then(|s| s.chars().next())
        .map(|c| c.to_ascii_uppercase())
        .filter(char::is_ascii_uppercase)
        .unwrap_or('C')
}

/// The letters worth asking about: those in the drive mask, except the floppy letters and the
/// system drive.
fn candidate_letters(mask: u32, system: &char) -> Vec<char> {
    ('C'..='Z')
        .filter(|c| mask & (1 << (*c as u32 - 'A' as u32)) != 0 && c != system)
        .collect()
}

/// `E` for `E:\`, and nothing for any other text.
pub(super) fn letter_of_root(id: &str) -> Option<char> {
    let mut chars = id.chars();
    match (chars.next(), chars.next(), chars.next(), chars.next()) {
        (Some(l), Some(':'), Some('\\'), None) if l.is_ascii_uppercase() => Some(l),
        _ => None,
    }
}

/// Whether a drive of this type is offered: removable, or fixed but on a USB bus.
fn is_offered(drive_type: u32, bus_is_usb: bool) -> bool {
    drive_type == DRIVE_REMOVABLE || (drive_type == DRIVE_FIXED && bus_is_usb)
}

/// A volume serial as `ABCD-1234`, the way FAT writes it (and Linux shows it).
fn format_volume_serial(serial: u32) -> String {
    format!("{:04X}-{:04X}", serial >> 16, serial & 0xFFFF)
}

fn guid_text(g: &GUID) -> String {
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        g.data1,
        g.data2,
        g.data3,
        g.data4[0],
        g.data4[1],
        g.data4[2],
        g.data4[3],
        g.data4[4],
        g.data4[5],
        g.data4[6],
        g.data4[7]
    )
}

fn from_wide(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The text at `offset` in a storage descriptor, trimmed (0 means there is none).
fn descriptor_text(buf: &[u8], offset: u32) -> Option<String> {
    let start = usize::try_from(offset).ok().filter(|&o| o > 0)?;
    let tail = buf.get(start..)?;
    let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
    let text = String::from_utf8_lossy(&tail[..end]).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// From the USB-bus ancestors of a disk's device (nearest first), the vendor, product and
/// serial of the USB device itself. An interface of a composite device (`...&MI_00`) has no
/// serial of its own, so the device above it is preferred, and the interface is the fallback.
fn pick_usb_ids(instance_ids: &[String]) -> UsbIds {
    let mut fallback = None;
    for id in instance_ids {
        let ids = parse_windows_usb_instance_id(id);
        if ids.vendor.is_none() {
            continue;
        }
        if id.to_ascii_uppercase().contains("&MI_") {
            fallback.get_or_insert(ids);
        } else {
            return ids;
        }
    }
    fallback.unwrap_or_default()
}

/// A handle that is closed when dropped.
struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateFileW and is closed once, here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// Opens a device or volume path with zero access rights: enough to ask questions, and not
/// enough to read or write anything. `path` is NUL-terminated UTF-16.
fn open_for_queries(path: &[u16]) -> Option<Handle> {
    // SAFETY: `path` is NUL-terminated and outlives the call. Zero desired access.
    unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            HANDLE::default(),
        )
    }
    .ok()
    .map(Handle)
}

/// Sends one of the query control codes, returns the bytes it wrote into `out`.
fn query(h: &Handle, code: u32, input: Option<&[u8]>, out: &mut [u8]) -> Option<usize> {
    let mut returned = 0u32;
    // SAFETY: the buffers are valid for their stated lengths for the duration of the call.
    unsafe {
        DeviceIoControl(
            h.0,
            code,
            input.map(|i| i.as_ptr().cast()),
            input.map_or(0, |i| i.len() as u32),
            Some(out.as_mut_ptr().cast()),
            out.len() as u32,
            Some(&mut returned),
            None,
        )
    }
    .ok()?;
    Some(returned as usize)
}

/// What a storage query said about the disk behind a volume.
#[derive(Default)]
pub(super) struct DiskFacts {
    pub(super) disk_number: Option<u32>,
    bus_is_usb: bool,
    vendor: Option<String>,
    product: Option<String>,
    serial: Option<String>,
    gpt_partition_id: Option<String>,
}

pub(super) fn disk_facts(letter: char) -> DiskFacts {
    let mut facts = DiskFacts::default();
    let Some(h) = open_for_queries(&wide(&format!(r"\\.\{letter}:"))) else {
        debug!("Could not open {letter}: for queries");
        return facts;
    };
    // IOCTL_STORAGE_GET_DEVICE_NUMBER: which disk the volume is on.
    let mut num = [0u8; size_of::<STORAGE_DEVICE_NUMBER>()];
    if query(&h, IOCTL_STORAGE_GET_DEVICE_NUMBER, None, &mut num) == Some(num.len()) {
        // SAFETY: the buffer holds a whole STORAGE_DEVICE_NUMBER (plain integers).
        let n: STORAGE_DEVICE_NUMBER = unsafe { std::ptr::read_unaligned(num.as_ptr().cast()) };
        facts.disk_number = Some(n.DeviceNumber);
    }
    // IOCTL_STORAGE_QUERY_PROPERTY: bus type, vendor, product, serial.
    let q = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    // SAFETY: the query is plain data; viewed as bytes for the call.
    let q_bytes = unsafe {
        std::slice::from_raw_parts(
            (&raw const q).cast::<u8>(),
            size_of::<STORAGE_PROPERTY_QUERY>(),
        )
    };
    let mut buf = vec![0u8; 1024];
    if let Some(n) = query(&h, IOCTL_STORAGE_QUERY_PROPERTY, Some(q_bytes), &mut buf)
        && n >= size_of::<STORAGE_DEVICE_DESCRIPTOR>()
    {
        // SAFETY: the buffer holds at least one STORAGE_DEVICE_DESCRIPTOR (plain integers).
        let d: STORAGE_DEVICE_DESCRIPTOR = unsafe { std::ptr::read_unaligned(buf.as_ptr().cast()) };
        let data = &buf[..n.min(buf.len())];
        facts.bus_is_usb = d.BusType.0 == BUS_TYPE_USB;
        facts.vendor = descriptor_text(data, d.VendorIdOffset);
        facts.product = descriptor_text(data, d.ProductIdOffset);
        facts.serial = descriptor_text(data, d.SerialNumberOffset);
    }
    // IOCTL_DISK_GET_PARTITION_INFO_EX: the GPT partition ID, where the disk has one.
    let mut part = vec![0u8; size_of::<PARTITION_INFORMATION_EX>()];
    if query(&h, IOCTL_DISK_GET_PARTITION_INFO_EX, None, &mut part) == Some(part.len()) {
        // SAFETY: the buffer holds a whole PARTITION_INFORMATION_EX (plain data).
        let p: PARTITION_INFORMATION_EX = unsafe { std::ptr::read_unaligned(part.as_ptr().cast()) };
        if p.PartitionStyle == PARTITION_STYLE_GPT {
            // SAFETY: the style says the GPT member is the one that was filled in.
            let id = unsafe { p.Anonymous.Gpt.PartitionId };
            facts.gpt_partition_id = Some(guid_text(&id));
        }
    }
    facts
}

/// A device information set that is destroyed when dropped.
struct DevInfoSet(HDEVINFO);

impl Drop for DevInfoSet {
    fn drop(&mut self) {
        // SAFETY: the set came from SetupDiGetClassDevsW and is destroyed once, here.
        let _ = unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

pub(super) fn instance_id(devinst: u32) -> Option<String> {
    let mut buf = [0u16; 512];
    // SAFETY: the buffer is valid for its length.
    let r = unsafe { CM_Get_Device_IDW(devinst, &mut buf, 0) };
    (r == CR_SUCCESS).then(|| from_wide(&buf))
}

/// The USB device above the disk with this number: the instance IDs of the disk's ancestors that
/// sit on the USB bus, nearest first.
fn usb_ancestors_of_disk(disk_number: u32) -> Vec<String> {
    let Some(disk) = disk_devinst(disk_number) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    let mut inst = disk;
    for _ in 0..10 {
        let mut parent = 0u32;
        // SAFETY: plain query.
        if unsafe { CM_Get_Parent(&mut parent, inst, 0) } != CR_SUCCESS {
            break;
        }
        inst = parent;
        if let Some(id) = instance_id(inst)
            && id.to_ascii_uppercase().starts_with("USB\\")
        {
            found.push(id);
        }
    }
    found
}

/// The Plug and Play device node of the disk with this number (a query only).
pub(super) fn disk_devinst(disk_number: u32) -> Option<u32> {
    // SAFETY: plain query; the set is destroyed by the guard.
    let set = unsafe {
        SetupDiGetClassDevsW(
            Some(&GUID_DEVINTERFACE_DISK),
            PCWSTR::null(),
            HWND::default(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    };
    let Ok(set) = set.map(DevInfoSet) else {
        return None;
    };
    for index in 0.. {
        // SAFETY: zeroed plain data with its size field set, as the call requires.
        let mut iface: SP_DEVICE_INTERFACE_DATA = unsafe { zeroed() };
        iface.cbSize = size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
        // SAFETY: `iface` is valid for the call.
        if unsafe {
            SetupDiEnumDeviceInterfaces(set.0, None, &GUID_DEVINTERFACE_DISK, index, &mut iface)
        }
        .is_err()
        {
            break;
        }
        let mut needed = 0u32;
        // SAFETY: asks only for the size; fails with "insufficient buffer" by design.
        let _ = unsafe {
            SetupDiGetDeviceInterfaceDetailW(set.0, &iface, None, 0, Some(&mut needed), None)
        };
        if (needed as usize) < size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() {
            continue;
        }
        // 8-byte aligned storage for the variable-length detail structure.
        let mut storage = vec![0u64; (needed as usize).div_ceil(8)];
        let detail = storage
            .as_mut_ptr()
            .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
        // SAFETY: `storage` is at least `needed` bytes and aligned for the structure.
        unsafe { (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32 };
        // SAFETY: zeroed plain data with its size field set.
        let mut info: SP_DEVINFO_DATA = unsafe { zeroed() };
        info.cbSize = size_of::<SP_DEVINFO_DATA>() as u32;
        // SAFETY: `detail` points at `needed` writable bytes; `info` is valid.
        if unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                set.0,
                &iface,
                Some(detail),
                needed,
                None,
                Some(&mut info),
            )
        }
        .is_err()
        {
            continue;
        }
        // SAFETY: the call filled DevicePath with a NUL-terminated string inside `storage`.
        let path: Vec<u16> = unsafe {
            let start = (&raw const (*detail).DevicePath).cast::<u16>();
            let max = (needed as usize - 4) / 2;
            let mut v = Vec::new();
            for i in 0..max {
                let c = *start.add(i);
                v.push(c);
                if c == 0 {
                    break;
                }
            }
            v
        };
        let Some(h) = open_for_queries(&path) else {
            continue;
        };
        let mut num = [0u8; size_of::<STORAGE_DEVICE_NUMBER>()];
        if query(&h, IOCTL_STORAGE_GET_DEVICE_NUMBER, None, &mut num) != Some(num.len()) {
            continue;
        }
        // SAFETY: the buffer holds a whole STORAGE_DEVICE_NUMBER (plain integers).
        let n: STORAGE_DEVICE_NUMBER = unsafe { std::ptr::read_unaligned(num.as_ptr().cast()) };
        if n.DeviceNumber != disk_number {
            continue;
        }
        return Some(info.DevInst);
    }
    None
}

/// Everything known about the volume on this letter, if it is one to offer.
fn identify(letter: char) -> Option<VolumeDetails> {
    let root = format!("{letter}:\\");
    let root_w = wide(&root);
    // SAFETY: NUL-terminated path.
    let drive_type = unsafe { GetDriveTypeW(PCWSTR(root_w.as_ptr())) };
    if drive_type != DRIVE_REMOVABLE && drive_type != DRIVE_FIXED {
        return None;
    }
    let mut label = [0u16; 261];
    let mut fs_name = [0u16; 64];
    let mut serial = 0u32;
    // SAFETY: the buffers are valid for their lengths. Fails when there is no disk in the drive.
    unsafe {
        GetVolumeInformationW(
            PCWSTR(root_w.as_ptr()),
            Some(&mut label),
            Some(&mut serial),
            None,
            None,
            Some(&mut fs_name),
        )
    }
    .ok()?;
    let facts = disk_facts(letter);
    if !is_offered(drive_type, facts.bus_is_usb) {
        return None;
    }
    let usb = facts
        .disk_number
        .map(usb_ancestors_of_disk)
        .map(|ids| pick_usb_ids(&ids))
        .unwrap_or_default();
    let mut guid_path = [0u16; 64];
    // SAFETY: NUL-terminated path; the buffer is valid for its length.
    let volume_guid =
        unsafe { GetVolumeNameForVolumeMountPointW(PCWSTR(root_w.as_ptr()), &mut guid_path) }
            .ok()
            .map(|()| from_wide(&guid_path))
            .filter(|s| !s.is_empty());
    let mut total = 0u64;
    // SAFETY: NUL-terminated path; `total` is valid for the call.
    let capacity =
        unsafe { GetDiskFreeSpaceExW(PCWSTR(root_w.as_ptr()), None, Some(&mut total), None) }
            .ok()
            .map(|()| total)
            .filter(|&t| t > 0);
    let label_text = from_wide(&label);
    let label = (!label_text.trim().is_empty()).then_some(label_text);
    info!("Found the volume {root} ({:?})", label);
    Some(VolumeDetails {
        root: root.into(),
        label: label.clone(),
        file_system: Some(from_wide(&fs_name)).filter(|s| !s.is_empty()),
        maker: facts.vendor,
        model: facts.product,
        signals: Signals {
            volume_serial: Some(format_volume_serial(serial)),
            volume_guid,
            gpt_partition_id: facts.gpt_partition_id,
            usb_serial: usb.serial.or(facts.serial.filter(|s| !s.contains('&'))),
            usb_vendor: usb.vendor,
            usb_product: usb.product,
            label,
            capacity,
            ..Signals::default()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_skip_the_floppies_and_the_system_drive() {
        // A, C, D, E present.
        let mask = 0b11101;
        assert_eq!(candidate_letters(mask, &'C'), ['D', 'E']);
        assert_eq!(candidate_letters(mask, &'D'), ['C', 'E']);
        assert!(candidate_letters(0, &'C').is_empty());
    }

    #[test]
    fn only_a_plain_drive_root_names_a_letter() {
        assert_eq!(letter_of_root("E:\\"), Some('E'));
        for bad in ["e:\\", "E:", "E:\\x", "\\\\?\\E:\\", "", "1:\\"] {
            assert_eq!(letter_of_root(bad), None, "{bad}");
        }
    }

    #[test]
    fn removable_drives_and_usb_fixed_drives_are_offered() {
        assert!(is_offered(DRIVE_REMOVABLE, false));
        assert!(is_offered(DRIVE_FIXED, true));
        assert!(!is_offered(DRIVE_FIXED, false));
        assert!(!is_offered(5, true)); // optical
        assert!(!is_offered(4, true)); // network
    }

    #[test]
    fn serials_and_guids_are_written_the_usual_way() {
        assert_eq!(format_volume_serial(0xABCD_1234), "ABCD-1234");
        assert_eq!(format_volume_serial(0x0000_00FF), "0000-00FF");
        let g = GUID::from_u128(0x0a1b2c3d_4e5f_6071_8293_a4b5c6d7e8f9);
        assert_eq!(guid_text(&g), "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9");
    }

    #[test]
    fn descriptor_strings_are_read_at_their_offsets() {
        let mut buf = vec![0u8; 64];
        buf[40..47].copy_from_slice(b"Kingsto");
        buf[47..50].copy_from_slice(b"n  ");
        buf[52..56].copy_from_slice(b"SN12");
        assert_eq!(descriptor_text(&buf, 40).as_deref(), Some("Kingston"));
        assert_eq!(descriptor_text(&buf, 52).as_deref(), Some("SN12"));
        assert_eq!(descriptor_text(&buf, 0), None);
        assert_eq!(descriptor_text(&buf, 99), None);
        assert_eq!(descriptor_text(&buf, 60), None);
    }

    #[test]
    fn the_usb_device_above_a_composite_interface_is_preferred() {
        let ids = [
            r"USB\VID_0951&PID_1666&MI_00\6&2a5b4c3d&0&0000".to_string(),
            r"USB\VID_0951&PID_1666\001CC0EC3412B0A1".to_string(),
            r"USB\ROOT_HUB30\4&1a2b3c4d&0&0".to_string(),
        ];
        let u = pick_usb_ids(&ids);
        assert_eq!(u.vendor.as_deref(), Some("0951"));
        assert_eq!(u.product.as_deref(), Some("1666"));
        assert_eq!(u.serial.as_deref(), Some("001CC0EC3412B0A1"));
        // Only an interface: vendor and product, no serial.
        let u = pick_usb_ids(&ids[..1]);
        assert_eq!(u.vendor.as_deref(), Some("0951"));
        assert_eq!(u.serial, None);
        // A hub is no device of the stick's.
        assert_eq!(pick_usb_ids(&ids[2..]), UsbIds::default());
        // A made-up serial (it has an &) is dropped by the parser.
        let u = pick_usb_ids(&[r"USB\VID_1234&PID_0001\5&2f3e4d&0&2".to_string()]);
        assert_eq!(u.serial, None);
        assert_eq!(u.vendor.as_deref(), Some("1234"));
    }

    #[test]
    fn opening_refuses_the_system_drive_and_anything_that_is_not_a_drive_root() {
        let sys = system_drive();
        for id in [format!("{sys}:\\"), "not a drive".into(), "..\\..".into()] {
            let d = ListedDevice {
                id,
                label: String::new(),
                kind_hint: None,
                transport: crate::platform::Transport::Volume,
            };
            assert!(open(&d).is_err());
        }
    }
}
