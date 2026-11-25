//! Android phones through libmtp. Read-only by construction:
//!
//! - the only libmtp functions declared (in [`ffi`]) are the ones that detect, open, list, read
//!   metadata and read file data. The write, delete, send, rename and create functions are not
//!   declared anywhere, so nothing in the app can call them;
//! - every returned resource is freed: lists by walking them, strings with `free`, the device
//!   with `LIBMTP_Release_Device`;
//! - nothing here touches the PC's disk (it only lists the desktop's mount names, to explain a
//!   busy phone).
//!
//! The phone's files are read with libmtp's whole-file callback. That call blocks until the
//! file is done, so it runs on a helper thread that hands chunks to the reader through a small
//! bounded queue. Dropping the reader early cancels the transfer and waits for the helper.
//!
//! Nothing here is specific to Linux: FreeBSD can use this file as it is once its build links
//! libmtp from ports.

use std::collections::HashSet;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::io::{self, Read};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Once};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime};

use log::{debug, info, warn};

use crate::counted;
use crate::device::{
    DeviceError, DeviceFile, DeviceIdentity, DeviceSource, Enumeration, ObjectInfo, ScanIssue,
    storage_folder_names, strip_direction_marks,
};
use crate::paths::MAX_COMPONENTS;
use crate::platform::{ListedDevice, Transport};
use crate::signals::{DeviceKind, Signals, View};

/// The part of libmtp this app uses, and no more. Every function here only reads. There is
/// deliberately no declaration for anything that sends, creates, deletes, renames or sets.
mod ffi {
    use std::ffi::{c_char, c_int, c_void};

    #[repr(C)]
    pub struct DeviceEntry {
        pub vendor: *mut c_char,
        pub vendor_id: u16,
        pub product: *mut c_char,
        pub product_id: u16,
        pub device_flags: u32,
    }

    #[repr(C)]
    pub struct RawDevice {
        pub device_entry: DeviceEntry,
        pub bus_location: u32,
        pub devnum: u8,
    }

    #[repr(C)]
    pub struct DeviceStorage {
        pub id: u32,
        pub storage_type: u16,
        pub filesystem_type: u16,
        pub access_capability: u16,
        pub max_capacity: u64,
        pub free_space_in_bytes: u64,
        pub free_space_in_objects: u64,
        pub storage_description: *mut c_char,
        pub volume_identifier: *mut c_char,
        pub next: *mut DeviceStorage,
        pub prev: *mut DeviceStorage,
    }

    /// The start of libmtp's device struct: only as far as `storage`, which is read through a
    /// pointer libmtp gave us. Never created here.
    #[repr(C)]
    pub struct MtpDevice {
        pub object_bitsize: u8,
        pub params: *mut c_void,
        pub usbinfo: *mut c_void,
        pub storage: *mut DeviceStorage,
    }

    #[repr(C)]
    pub struct MtpFile {
        pub item_id: u32,
        pub parent_id: u32,
        pub storage_id: u32,
        pub filename: *mut c_char,
        pub filesize: u64,
        pub modificationdate: libc::time_t,
        pub filetype: c_int,
        pub next: *mut MtpFile,
    }

    #[repr(C)]
    pub struct MtpError {
        pub errornumber: c_int,
        pub error_text: *mut c_char,
        pub next: *mut MtpError,
    }

    /// `LIBMTP_ERROR_NO_DEVICE_ATTACHED`.
    pub const ERROR_NO_DEVICE_ATTACHED: c_int = 5;
    /// `LIBMTP_FILETYPE_FOLDER`.
    pub const FILETYPE_FOLDER: c_int = 0;
    /// `LIBMTP_FILES_AND_FOLDERS_ROOT`.
    pub const FILES_AND_FOLDERS_ROOT: u32 = 0xffff_ffff;
    pub const HANDLER_OK: u16 = 0;
    pub const HANDLER_CANCEL: u16 = 2;

    /// libmtp's `MTPDataPutFunc`: called with each piece of a file that is being read.
    pub type DataFunc = unsafe extern "C" fn(
        params: *mut c_void,
        private: *mut c_void,
        sendlen: u32,
        data: *mut u8,
        putlen: *mut u32,
    ) -> u16;

    super::super::dynlib::dyn_api! {
        sonames: &["libmtp.so.9", "libmtp.so"],
        off: "Android phone support is off: libmtp isn't installed",
        api: Api,
        get: api,
        load: load_api;
        pub fn LIBMTP_Init();
        pub fn LIBMTP_Detect_Raw_Devices(list: *mut *mut RawDevice, count: *mut c_int) -> c_int;
        pub fn LIBMTP_Open_Raw_Device_Uncached(raw: *mut RawDevice) -> *mut MtpDevice;
        pub fn LIBMTP_Release_Device(device: *mut MtpDevice);
        pub fn LIBMTP_Get_Manufacturername(device: *mut MtpDevice) -> *mut c_char;
        pub fn LIBMTP_Get_Modelname(device: *mut MtpDevice) -> *mut c_char;
        pub fn LIBMTP_Get_Serialnumber(device: *mut MtpDevice) -> *mut c_char;
        pub fn LIBMTP_Get_Friendlyname(device: *mut MtpDevice) -> *mut c_char;
        pub fn LIBMTP_Get_Errorstack(device: *mut MtpDevice) -> *mut MtpError;
        pub fn LIBMTP_Clear_Errorstack(device: *mut MtpDevice);
        pub fn LIBMTP_Get_Storage(device: *mut MtpDevice, sortby: c_int) -> c_int;
        pub fn LIBMTP_Get_Files_And_Folders(
            device: *mut MtpDevice,
            storage: u32,
            parent: u32,
        ) -> *mut MtpFile;
        pub fn LIBMTP_Get_Filemetadata(device: *mut MtpDevice, id: u32) -> *mut MtpFile;
        pub fn LIBMTP_Get_File_To_Handler(
            device: *mut MtpDevice,
            id: u32,
            func: DataFunc,
            private: *mut c_void,
            progress: *const c_void,
            progress_data: *const c_void,
        ) -> c_int;
        pub fn LIBMTP_destroy_file_t(file: *mut MtpFile);
    }

    /// Whether libmtp was found and every function in it looked up.
    pub fn available() -> bool {
        api().is_some()
    }
}

use ffi::{MtpDevice, MtpFile};

/// Loads libmtp (once) and starts it. `false` when libmtp isn't here: no Android phone can be
/// listed or opened then.
fn init_library() -> bool {
    static INIT: Once = Once::new();
    if !ffi::available() {
        return false;
    }
    // SAFETY: LIBMTP_Init takes no arguments and is called once before any other call.
    INIT.call_once(|| unsafe { ffi::LIBMTP_Init() });
    true
}

/// What to tell the user when libmtp is missing: without it no Android phone is ever listed.
pub fn hint() -> Option<String> {
    (!ffi::available())
        .then(|| "Android phones need libmtp (Ubuntu: sudo apt install libmtp9)".to_string())
}

/// Copy a libmtp-allocated string and free it.
///
/// # Safety
/// `p` must be null or a NUL-terminated string from `malloc` that nobody else frees.
unsafe fn take_string(p: *mut c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: per the contract, `p` is a valid NUL-terminated string.
    let s = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    // SAFETY: per the contract, we own the allocation and free it exactly once.
    unsafe { libc::free(p.cast()) };
    Some(s)
}

/// Copy a string libmtp still owns.
///
/// # Safety
/// `p` must be null or a valid NUL-terminated string.
unsafe fn copy_string(p: *const c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: per the contract.
    Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

// ---------------------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------------------

/// The ID of a listed phone: where it sits on the USB bus.
fn device_id(bus: u32, devnum: u8) -> String {
    format!("mtp:{bus}:{devnum}")
}

/// The bus and device number in an ID made by [`device_id`].
fn parse_device_id(id: &str) -> Option<(u32, u8)> {
    let rest = id.strip_prefix("mtp:")?;
    let (bus, dev) = rest.split_once(':')?;
    Some((bus.parse().ok()?, dev.parse().ok()?))
}

/// A phone's last-modified time from libmtp's `time_t`. Phones that don't know it report 0 (or
/// garbage), which is no date at all.
pub(super) fn mtp_time(secs: i64) -> Option<SystemTime> {
    // Up to the year 9999: beyond that is a corrupt value, not a date.
    const LATEST: i64 = 253_402_300_799;
    if secs <= 0 || secs > LATEST {
        return None;
    }
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64))
}

/// What an opened phone reports about itself.
#[derive(Default)]
struct PhoneInfo {
    serial: String,
    maker: String,
    model: String,
    friendly_name: String,
    usb_vendor: u16,
    usb_product: u16,
    storage_names: Vec<String>,
    capacity: u64,
}

/// The signals an opened phone gives. libmtp offers no USB serial, only the device's own.
fn build_signals(p: &PhoneInfo) -> Signals {
    let some = |s: &str| {
        let s = strip_direction_marks(s);
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_string())
    };
    Signals {
        device_serial: some(&p.serial),
        usb_vendor: (p.usb_vendor != 0).then(|| format!("{:04x}", p.usb_vendor)),
        usb_product: (p.usb_product != 0).then(|| format!("{:04x}", p.usb_product)),
        maker: some(&p.maker),
        model: some(&p.model),
        friendly_name: some(&p.friendly_name),
        capacity: (p.capacity > 0).then_some(p.capacity),
        storage_names: p.storage_names.clone(),
        ..Signals::default()
    }
}

/// Whether the names in the desktop's gvfs folder (`/run/user/<uid>/gvfs`) show that the
/// desktop has a phone mounted.
fn gvfs_has_phone<S: AsRef<str>>(names: &[S]) -> bool {
    names.iter().any(|n| {
        let n = n.as_ref();
        n.starts_with("mtp:") || n.starts_with("gphoto2:")
    })
}

/// What to tell the user when the phone could not be opened. `desktop_holds_it` says whether
/// the desktop's file manager has the phone mounted.
fn open_failure_message(desktop_holds_it: bool) -> String {
    if desktop_holds_it {
        "The phone is in use by the desktop's file manager. Close the phone's window in the file \
         manager, or unmount (eject) the phone there, then try again."
            .into()
    } else {
        "Could not open the phone. It may be in use by another program (a file manager, a phone \
         app), locked, or set to charge only. Unlock it, choose File transfer on the phone, close \
         anything else that uses it and try again. If that does not help, this account may lack \
         permission to the USB device."
            .into()
    }
}

/// What to tell the user when the phone shows no storage.
fn no_storage_message() -> String {
    "The phone shows no storage. Unlock it and, in the USB notification on the phone, choose \
     File transfer (the default, No data transfer, shares nothing). Then try again."
        .into()
}

/// Whether the desktop has a phone mounted. Reads only the names in the gvfs folder.
pub(super) fn desktop_holds_phone() -> bool {
    // SAFETY: getuid has no arguments and cannot fail.
    let uid = unsafe { libc::getuid() };
    let Ok(entries) = std::fs::read_dir(format!("/run/user/{uid}/gvfs")) else {
        return false;
    };
    let names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    gvfs_has_phone(&names)
}

// ---------------------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------------------

struct RawInfo {
    bus: u32,
    devnum: u8,
    vendor_id: u16,
    product_id: u16,
    vendor: String,
    product: String,
}

impl RawInfo {
    fn label(&self) -> String {
        let name = format!("{} {}", self.vendor, self.product);
        let name = name.trim();
        if name.is_empty() {
            format!(
                "Phone or player (MTP, {:04x}:{:04x})",
                self.vendor_id, self.product_id
            )
        } else {
            format!("{name} (MTP)")
        }
    }
}

/// Raw-device detection: reads the USB bus, opens nothing.
fn detect() -> Result<Vec<RawInfo>, String> {
    if !init_library() {
        return Ok(Vec::new());
    }
    let mut list: *mut ffi::RawDevice = std::ptr::null_mut();
    let mut count: c_int = 0;
    // SAFETY: both out-pointers point to live locals.
    let rc = unsafe { ffi::LIBMTP_Detect_Raw_Devices(&mut list, &mut count) };
    let mut out = Vec::new();
    if !list.is_null() {
        for i in 0..usize::try_from(count).unwrap_or(0) {
            // SAFETY: libmtp returned `count` consecutive entries at `list`; the strings in an
            // entry belong to libmtp and are copied, not freed.
            unsafe {
                let r = &*list.add(i);
                out.push(RawInfo {
                    bus: r.bus_location,
                    devnum: r.devnum,
                    vendor_id: r.device_entry.vendor_id,
                    product_id: r.device_entry.product_id,
                    vendor: copy_string(r.device_entry.vendor).unwrap_or_default(),
                    product: copy_string(r.device_entry.product).unwrap_or_default(),
                });
            }
        }
        // SAFETY: the array was allocated by libmtp with malloc and is freed exactly once.
        unsafe { libc::free(list.cast()) };
    }
    match rc {
        0 | ffi::ERROR_NO_DEVICE_ATTACHED => Ok(out),
        e => Err(format!("libmtp could not look for phones (error {e})")),
    }
}

/// The USB bus and device number of every phone libmtp detects, so the camera listing can leave
/// out a device that is already listed as a phone. Empty when libmtp isn't here.
pub(super) fn listed_positions() -> Vec<(u32, u8)> {
    detect()
        .map(|found| found.iter().map(|r| (r.bus, r.devnum)).collect())
        .unwrap_or_default()
}

/// The Android phones that are plugged in. Opens no session.
pub fn list() -> Vec<ListedDevice> {
    match detect() {
        Ok(found) => found
            .iter()
            .map(|r| ListedDevice {
                id: device_id(r.bus, r.devnum),
                label: r.label(),
                kind_hint: Some(DeviceKind::Mtp),
                transport: Transport::Mtp,
            })
            .collect(),
        Err(e) => {
            debug!("{e}");
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------------------
// An open phone
// ---------------------------------------------------------------------------------------

/// An owned libmtp device, released on drop.
struct Dev(*mut MtpDevice);

impl Drop for Dev {
    fn drop(&mut self) {
        // SAFETY: the pointer came from LIBMTP_Open_Raw_Device_Uncached and is released once.
        unsafe { ffi::LIBMTP_Release_Device(self.0) };
    }
}

/// A device pointer handed to the transfer thread. The thread has the device to itself for as
/// long as it runs: the reader that started it borrows the source exclusively and joins the
/// thread before giving the borrow back.
struct SendDev(*mut MtpDevice);
// SAFETY: see above: access is never concurrent.
unsafe impl Send for SendDev {}

struct Storage {
    id: u32,
    /// What files on it report as their storage: the phone's name for it, cleaned to be a safe
    /// folder name ([`storage_folder_names`]), the same as on Windows.
    name: String,
}

/// A folder's entries, and the error libmtp raised while listing them, if any. With an error
/// the list may be partial: the folder is then not completely listed.
struct Listing {
    entries: Vec<Entry>,
    error: Option<String>,
}

/// What a listing and its error stack add up to.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// Nothing came back and there is an error: the folder could not be listed.
    Failed(String),
    /// A list (possibly empty), with the error that came with it, if any.
    Listed(Option<String>),
}

/// An error beside a non-empty list means the list may stop part-way; beside an empty one it
/// means the folder could not be listed at all.
fn listing_outcome(empty: bool, error: Option<String>) -> Outcome {
    match (empty, error) {
        (true, Some(e)) => Outcome::Failed(e),
        (_, error) => Outcome::Listed(error),
    }
}

/// One entry of a folder listing, copied out of libmtp's list.
struct Entry {
    id: u32,
    storage_id: u32,
    name: Option<String>,
    size: u64,
    modified: i64,
    is_folder: bool,
}

/// Copies a libmtp file list into owned entries and frees every node.
///
/// # Safety
/// `head` must be null or the head of a list returned by libmtp that nobody else frees.
unsafe fn take_files(head: *mut MtpFile) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut p = head;
    while !p.is_null() {
        // SAFETY: `p` is a live node of the list; its fields are read before it is destroyed.
        let (entry, next) = unsafe {
            let f = &*p;
            (
                Entry {
                    id: f.item_id,
                    storage_id: f.storage_id,
                    name: copy_string(f.filename),
                    size: f.filesize,
                    #[allow(clippy::unnecessary_cast)] // time_t is 32 bits on some systems
                    modified: f.modificationdate as i64,
                    is_folder: f.filetype == ffi::FILETYPE_FOLDER,
                },
                f.next,
            )
        };
        out.push(entry);
        // SAFETY: frees this one node (and its name); the list is walked by the `next` saved
        // above.
        unsafe { ffi::LIBMTP_destroy_file_t(p) };
        p = next;
    }
    out
}

pub struct MtpSource {
    dev: Dev,
    identity: DeviceIdentity,
    storages: Vec<Storage>,
}

/// Opens a listed phone as a read-only source.
pub fn open(device: &ListedDevice) -> Result<Box<dyn DeviceSource>, String> {
    let (bus, devnum) = parse_device_id(&device.id)
        .ok_or_else(|| format!("{} is not a phone this app listed", device.id))?;
    if !init_library() {
        return Err("Android phone support is off: libmtp isn't installed".into());
    }
    let mut list: *mut ffi::RawDevice = std::ptr::null_mut();
    let mut count: c_int = 0;
    // SAFETY: both out-pointers point to live locals.
    let rc = unsafe { ffi::LIBMTP_Detect_Raw_Devices(&mut list, &mut count) };
    let n = if list.is_null() {
        0
    } else {
        usize::try_from(count).unwrap_or(0)
    };
    let mut found: *mut ffi::RawDevice = std::ptr::null_mut();
    for i in 0..n {
        // SAFETY: `i` is below the entry count libmtp reported.
        let r = unsafe { list.add(i) };
        // SAFETY: as above; only plain integers are read.
        if unsafe { ((*r).bus_location, (*r).devnum) } == (bus, devnum) {
            found = r;
            break;
        }
    }
    let result = if found.is_null() {
        if rc != 0 && rc != ffi::ERROR_NO_DEVICE_ATTACHED {
            Err(format!("libmtp could not look for phones (error {rc})"))
        } else {
            Err("The phone is no longer plugged in".to_string())
        }
    } else {
        // SAFETY: `found` points into the live array; only plain integers are read.
        let (vendor_id, product_id) = unsafe {
            (
                (*found).device_entry.vendor_id,
                (*found).device_entry.product_id,
            )
        };
        // SAFETY: `found` is a valid raw device entry from the detection above.
        let dev = unsafe { ffi::LIBMTP_Open_Raw_Device_Uncached(found) };
        if dev.is_null() {
            let busy = desktop_holds_phone();
            warn!(
                "Could not open the phone at {} (the desktop has a phone mounted: {busy})",
                device.id
            );
            Err(open_failure_message(busy))
        } else {
            MtpSource::start(Dev(dev), &device.id, vendor_id, product_id)
        }
    };
    if !list.is_null() {
        // SAFETY: allocated by libmtp with malloc; freed once, after the last use of `found`.
        unsafe { libc::free(list.cast()) };
    }
    Ok(Box::new(result?))
}

impl MtpSource {
    fn start(dev: Dev, id: &str, vendor_id: u16, product_id: u16) -> Result<Self, String> {
        let p = dev.0;
        // SAFETY: `p` is an open device; each returned string is owned by us and freed by
        // `take_string`.
        let (serial, maker, model, friendly) = unsafe {
            (
                take_string(ffi::LIBMTP_Get_Serialnumber(p)),
                take_string(ffi::LIBMTP_Get_Manufacturername(p)),
                take_string(ffi::LIBMTP_Get_Modelname(p)),
                take_string(ffi::LIBMTP_Get_Friendlyname(p)),
            )
        };
        // SAFETY: `p` is an open device. The storage list stays owned by libmtp; its strings
        // are copied before anything else is called on the device.
        let raw_storages: Vec<(u32, String, u64)> = unsafe {
            let rc = ffi::LIBMTP_Get_Storage(p, 0);
            let mut v = Vec::new();
            if rc >= 0 {
                let mut s = (*p).storage;
                while !s.is_null() {
                    let st = &*s;
                    let name = copy_string(st.storage_description)
                        .filter(|n| !n.trim().is_empty())
                        .or_else(|| copy_string(st.volume_identifier))
                        .unwrap_or_default();
                    v.push((st.id, name, st.max_capacity));
                    s = st.next;
                }
            }
            v
        };
        if raw_storages.is_empty() {
            return Err(no_storage_message());
        }
        let names: Vec<String> = raw_storages.iter().map(|(_, n, _)| n.clone()).collect();
        let storages: Vec<Storage> = raw_storages
            .iter()
            .zip(storage_folder_names(&names))
            .map(|((id, _, _), name)| Storage { id: *id, name })
            .collect();
        let info = PhoneInfo {
            serial: serial.unwrap_or_default(),
            maker: maker.unwrap_or_default(),
            model: model.unwrap_or_default(),
            friendly_name: friendly.unwrap_or_default(),
            usb_vendor: vendor_id,
            usb_product: product_id,
            storage_names: storages.iter().map(|s| s.name.clone()).collect(),
            capacity: raw_storages.iter().map(|(_, _, c)| *c).sum(),
        };
        let clean = |s: &str| strip_direction_marks(s).trim().to_string();
        let mut identity = DeviceIdentity::new(
            id,
            clean(&info.friendly_name),
            clean(&info.maker),
            clean(&info.model),
            Some(clean(&info.serial)).filter(|s| !s.is_empty()),
        );
        identity.signals = build_signals(&info);
        identity.view = View::Mtp;
        info!(
            "Opened \"{}\" through MTP: {} {}, serial number {}, {}",
            identity.friendly_name,
            identity.manufacturer,
            identity.model,
            identity.serial.as_deref().unwrap_or("not reported"),
            counted(storages.len(), "storage", "storages")
        );
        Ok(Self {
            dev,
            identity,
            storages,
        })
    }

    /// The first error libmtp holds for the device, if any; the stack is cleared.
    fn take_error(&self) -> Option<String> {
        // SAFETY: the device is open; the error list is read, then cleared.
        unsafe {
            let e = ffi::LIBMTP_Get_Errorstack(self.dev.0);
            let text = (!e.is_null()).then(|| {
                copy_string((*e).error_text).unwrap_or_else(|| "unknown error".to_string())
            });
            ffi::LIBMTP_Clear_Errorstack(self.dev.0);
            text
        }
    }

    /// The files and folders directly in `parent` on `storage`. A list that came back with an
    /// error, even a part of one, says so ([`Listing::error`]).
    fn children(&self, storage: u32, parent: u32) -> Result<Listing, String> {
        // SAFETY: the device is open; the list is consumed by `take_files`.
        let list = unsafe {
            ffi::LIBMTP_Clear_Errorstack(self.dev.0);
            take_files(ffi::LIBMTP_Get_Files_And_Folders(
                self.dev.0, storage, parent,
            ))
        };
        let error = self.take_error();
        match listing_outcome(list.is_empty(), error) {
            Outcome::Failed(e) => Err(e),
            Outcome::Listed(error) => Ok(Listing {
                entries: list,
                error,
            }),
        }
    }

    /// The one entry with this object ID.
    fn metadata(&self, id: u32) -> Result<Entry, String> {
        // SAFETY: the device is open; the single node is consumed by `take_files`.
        let mut v = unsafe {
            ffi::LIBMTP_Clear_Errorstack(self.dev.0);
            take_files(ffi::LIBMTP_Get_Filemetadata(self.dev.0, id))
        };
        match v.pop() {
            Some(e) => Ok(e),
            None => Err(self
                .take_error()
                .unwrap_or_else(|| "the phone has no such object".to_string())),
        }
    }

    /// The storage's name (what `DeviceFile::storage` carries), if the ID is one of the phone's.
    fn storage_label(&self, id: u32) -> Option<String> {
        self.storages
            .iter()
            .find(|s| s.id == id)
            .map(|s| s.name.clone())
    }
}

/// Record a problem found while walking the phone, and log it.
fn report(result: &mut Enumeration, here: &str, problem: impl Into<String>) {
    let issue = ScanIssue::new(here, problem);
    warn!("Phone: {}: {}", issue.location, issue.problem);
    result.issues.push(issue);
}

impl DeviceSource for MtpSource {
    fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    /// Walks each storage iteratively (no recursion), with a depth limit and a seen-set, so a
    /// deep or cyclic tree from a hostile phone can't overflow the stack or loop forever.
    fn enumerate(
        &mut self,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(usize, &str),
    ) -> Result<Enumeration, DeviceError> {
        let mut result = Enumeration::default();
        let mut seen: HashSet<u32> = HashSet::new();
        // (storage, parent object, path so far, depth)
        let mut stack: Vec<(u32, u32, Vec<String>, usize)> = self
            .storages
            .iter()
            .rev()
            .map(|s| (s.id, ffi::FILES_AND_FOLDERS_ROOT, Vec::new(), 0))
            .collect();
        let mut folders = 0usize;

        while let Some((storage, parent, comps, depth)) = stack.pop() {
            if cancel.load(Ordering::Relaxed) {
                return Err(DeviceError::Cancelled);
            }
            let here = if comps.is_empty() {
                "(phone)".to_string()
            } else {
                comps.join("/")
            };
            progress(result.files.len(), &here);
            let listing = match self.children(storage, parent) {
                Ok(v) => v,
                Err(e) => {
                    report(
                        &mut result,
                        &here,
                        format!("could not list this folder: {e}"),
                    );
                    result.incomplete.push(comps.join("/"));
                    continue;
                }
            };
            folders += 1;
            let Listing { entries, error } = listing;
            debug!("Listed {here}: {}", counted(entries.len(), "item", "items"));
            if let Some(e) = &error {
                report(
                    &mut result,
                    &here,
                    format!(
                        "the phone reported an error while listing this folder, so the list may be incomplete: {e}"
                    ),
                );
                result.incomplete.push(comps.join("/"));
            } else if entries.is_empty() && !comps.is_empty() {
                result.empty_folders.push(comps.join("/"));
            }
            for e in entries {
                if cancel.load(Ordering::Relaxed) {
                    return Err(DeviceError::Cancelled);
                }
                if !seen.insert(e.id) {
                    report(
                        &mut result,
                        &here,
                        format!("object {} appears twice; ignored the repeat", e.id),
                    );
                    // Which of the two is the real one is unknown, so a file missing from the
                    // listing must not count as deleted.
                    result.incomplete.push(comps.join("/"));
                    continue;
                }
                let name = e
                    .name
                    .as_deref()
                    .map(strip_direction_marks)
                    .unwrap_or_default();
                if name.is_empty() {
                    report(&mut result, &here, format!("object {} has no name", e.id));
                    result.incomplete.push(comps.join("/"));
                    continue;
                }
                let mut path = comps.clone();
                path.push(name);
                if e.is_folder {
                    if depth + 1 > MAX_COMPONENTS {
                        report(
                            &mut result,
                            &here,
                            "folders nested too deeply; not scanned further",
                        );
                        result.incomplete.push(path.join("/"));
                        continue;
                    }
                    stack.push((e.storage_id, e.id, path, depth + 1));
                } else {
                    result.files.push(DeviceFile {
                        object_id: e.id.to_string(),
                        persistent_id: None,
                        components: path,
                        size: Some(e.size),
                        modified: mtp_time(e.modified),
                        storage: self.storage_label(e.storage_id),
                    });
                }
            }
        }
        result.files.sort_by(|a, b| a.components.cmp(&b.components));
        debug!(
            "Walked the phone: {} in {}",
            counted(result.files.len(), "file", "files"),
            counted(folders, "folder", "folders")
        );
        progress(result.files.len(), "");
        Ok(result)
    }

    fn object_info(&mut self, object_id: &str) -> Result<ObjectInfo, DeviceError> {
        let id = parse_object_id(object_id)?;
        let e = self.metadata(id).map_err(|e| {
            warn!("Could not read the properties of object {id} on the phone: {e}");
            DeviceError::Failed(e)
        })?;
        Ok(ObjectInfo {
            name: e
                .name
                .as_deref()
                .map(strip_direction_marks)
                .unwrap_or_default(),
            size: Some(e.size),
            persistent_id: None,
            modified: mtp_time(e.modified),
            storage: self.storage_label(e.storage_id),
        })
    }

    fn open_read(&mut self, object_id: &str) -> Result<Box<dyn Read + '_>, DeviceError> {
        let id = parse_object_id(object_id)?;
        let e = self.metadata(id).map_err(DeviceError::Failed)?;
        if e.is_folder {
            return Err(DeviceError::Failed("that is a folder, not a file".into()));
        }
        Ok(Box::new(MtpReader::start(self, id)))
    }
}

fn parse_object_id(object_id: &str) -> Result<u32, DeviceError> {
    object_id
        .parse()
        .map_err(|_| DeviceError::Failed(format!("{object_id} is not a phone object ID")))
}

// ---------------------------------------------------------------------------------------
// Reading a file
// ---------------------------------------------------------------------------------------

/// What the transfer thread sends to the reader.
enum Msg {
    Chunk(Vec<u8>),
    /// The whole file has been delivered.
    Done,
    /// The transfer stopped early.
    Failed(String),
}

/// Handed to libmtp as the callback's private pointer, on the transfer thread's stack.
struct Sink {
    tx: SyncSender<Msg>,
    stop: Arc<AtomicBool>,
}

/// libmtp's data callback: queue the piece, or cancel if the reader has gone.
///
/// # Safety
/// `private` must point to the live [`Sink`] passed to `LIBMTP_Get_File_To_Handler`; `data`
/// must be valid for `sendlen` bytes and `putlen` writable, as libmtp guarantees.
unsafe extern "C" fn on_data(
    _params: *mut c_void,
    private: *mut c_void,
    sendlen: u32,
    data: *mut u8,
    putlen: *mut u32,
) -> u16 {
    // SAFETY: per the contract.
    let sink = unsafe { &*(private as *const Sink) };
    if sink.stop.load(Ordering::Relaxed) {
        return ffi::HANDLER_CANCEL;
    }
    // SAFETY: libmtp gives `sendlen` readable bytes at `data`.
    let chunk = unsafe { std::slice::from_raw_parts(data, sendlen as usize) }.to_vec();
    if sink.tx.send(Msg::Chunk(chunk)).is_err() {
        return ffi::HANDLER_CANCEL;
    }
    // SAFETY: `putlen` is writable per the contract.
    unsafe { *putlen = sendlen };
    ffi::HANDLER_OK
}

/// A running read of one file. Dropping it before the end cancels the transfer and waits for
/// it to stop, so the phone is never touched from two threads.
struct MtpReader<'a> {
    rx: Option<Receiver<Msg>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pending: Vec<u8>,
    pos: usize,
    finished: bool,
    _source: PhantomData<&'a mut MtpSource>,
}

/// Chunks waiting between the transfer thread and the reader.
const QUEUE_CHUNKS: usize = 8;

impl<'a> MtpReader<'a> {
    fn start(source: &'a mut MtpSource, id: u32) -> Self {
        let (tx, rx) = sync_channel(QUEUE_CHUNKS);
        let stop = Arc::new(AtomicBool::new(false));
        let dev = SendDev(source.dev.0);
        let sink_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            // The whole struct, not just its pointer field, so the `Send` wrapper travels.
            let dev = dev;
            let sink = Sink {
                tx,
                stop: sink_stop,
            };
            // SAFETY: the device is open and, until this thread is joined, used by nobody
            // else. `sink` outlives the call. No progress callback.
            let rc = unsafe {
                ffi::LIBMTP_Clear_Errorstack(dev.0);
                ffi::LIBMTP_Get_File_To_Handler(
                    dev.0,
                    id,
                    on_data,
                    (&raw const sink).cast_mut().cast(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            };
            let end = if rc == 0 {
                Msg::Done
            } else {
                // SAFETY: as above.
                let text = unsafe {
                    let e = ffi::LIBMTP_Get_Errorstack(dev.0);
                    let t = if e.is_null() {
                        None
                    } else {
                        copy_string((*e).error_text)
                    };
                    ffi::LIBMTP_Clear_Errorstack(dev.0);
                    t
                };
                Msg::Failed(text.unwrap_or_else(|| "the transfer failed".to_string()))
            };
            // The reader may be gone already.
            let _ = sink.tx.send(end);
        });
        Self {
            rx: Some(rx),
            stop,
            thread: Some(thread),
            pending: Vec::new(),
            pos: 0,
            finished: false,
            _source: PhantomData,
        }
    }
}

impl Read for MtpReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.pos >= self.pending.len() {
            if self.finished {
                return Ok(0);
            }
            let Some(rx) = &self.rx else {
                return Ok(0);
            };
            match rx.recv() {
                Ok(Msg::Chunk(c)) => {
                    self.pending = c;
                    self.pos = 0;
                }
                Ok(Msg::Done) => {
                    self.finished = true;
                    return Ok(0);
                }
                Ok(Msg::Failed(e)) => {
                    warn!("Reading a file from the phone failed: {e}");
                    return Err(io::Error::other(format!("phone read failed: {e}")));
                }
                Err(_) => {
                    return Err(io::Error::other("the phone's transfer ended unexpectedly"));
                }
            }
        }
        let n = buf.len().min(self.pending.len() - self.pos);
        buf[..n].copy_from_slice(&self.pending[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Dropping a reader before its file is finished stops the transfer and waits for the helper
/// thread. The wait has no time limit, on purpose: the thread is inside a libmtp call that uses
/// the open device, and the device must not be released or used from anywhere else while that
/// call is running. A phone that stops answering part-way through a file can therefore hold
/// this up until it is unplugged, which makes libmtp's call return with an error.
impl Drop for MtpReader<'_> {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Let go of the queue first, so a transfer blocked on a full queue wakes and cancels.
        self.rx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_device_id_round_trips_and_junk_is_refused() {
        let id = device_id(3, 17);
        assert_eq!(id, "mtp:3:17");
        assert_eq!(parse_device_id(&id), Some((3, 17)));
        for bad in [
            "",
            "mtp:",
            "mtp:3",
            "mtp:a:b",
            "mtp:3:256",
            "usb:3:1",
            "3:17",
        ] {
            assert_eq!(parse_device_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn dates_convert_and_absent_ones_are_none() {
        assert_eq!(mtp_time(0), None);
        assert_eq!(mtp_time(-5), None);
        assert_eq!(mtp_time(i64::MAX), None);
        assert_eq!(
            mtp_time(1_500_000_000),
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000))
        );
        assert!(mtp_time(253_402_300_799).is_some());
        assert_eq!(mtp_time(253_402_300_800), None);
    }

    /// Storage names are cleaned by the one function every system shares, so the folder names
    /// are the same whichever way the phone is plugged in.
    #[test]
    fn storage_names_are_cleaned_like_everywhere_else() {
        let names: Vec<String> = ["Internal shared storage", "SD: card?", "", "sd: CARD?"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            storage_folder_names(&names),
            [
                "Internal shared storage",
                "SD_ card_",
                "Storage 3",
                "sd_ CARD_ (2)"
            ]
        );
    }

    /// DESIGN.md 10.3: only reading functions are declared, so nothing here can write to a phone.
    #[test]
    fn only_reading_functions_are_declared() {
        const ALLOWED: [&str; 15] = [
            "LIBMTP_Init",
            "LIBMTP_Detect_Raw_Devices",
            "LIBMTP_Open_Raw_Device_Uncached",
            "LIBMTP_Release_Device",
            "LIBMTP_Get_Manufacturername",
            "LIBMTP_Get_Modelname",
            "LIBMTP_Get_Serialnumber",
            "LIBMTP_Get_Friendlyname",
            "LIBMTP_Get_Errorstack",
            "LIBMTP_Clear_Errorstack",
            "LIBMTP_Get_Storage",
            "LIBMTP_Get_Files_And_Folders",
            "LIBMTP_Get_Filemetadata",
            "LIBMTP_Get_File_To_Handler",
            "LIBMTP_destroy_file_t",
        ];
        super::super::dynlib::check::assert_read_only(include_str!("mtp.rs"), &ALLOWED);
    }

    /// Without libmtp (or with the test hook set) nothing is listed and the hint says why.
    #[test]
    fn without_libmtp_nothing_is_listed() {
        if ffi::available() {
            assert!(hint().is_none());
            return;
        }
        assert!(list().is_empty());
        assert!(hint().unwrap().contains("libmtp9"));
        let phone = ListedDevice {
            id: device_id(1, 2),
            label: String::new(),
            kind_hint: Some(DeviceKind::Mtp),
            transport: Transport::Mtp,
        };
        assert!(open(&phone).is_err());
    }

    /// An error beside a list is not ignored: a partial list makes the folder incomplete.
    #[test]
    fn an_error_beside_a_list_is_not_ignored() {
        let e = || Some("PTP error".to_string());
        assert_eq!(
            listing_outcome(true, e()),
            Outcome::Failed("PTP error".into())
        );
        assert_eq!(listing_outcome(false, e()), Outcome::Listed(e()));
        assert_eq!(listing_outcome(false, None), Outcome::Listed(None));
        assert_eq!(listing_outcome(true, None), Outcome::Listed(None));
    }

    #[test]
    fn signals_carry_what_the_phone_reports() {
        let s = build_signals(&PhoneInfo {
            serial: "R58M1234ABC".into(),
            maker: "Google".into(),
            model: "Pixel 7".into(),
            friendly_name: "My phone".into(),
            usb_vendor: 0x18d1,
            usb_product: 0x4ee1,
            storage_names: vec!["Internal shared storage".into(), "SD card".into()],
            capacity: 128_000_000_000,
        });
        assert_eq!(s.device_serial.as_deref(), Some("R58M1234ABC"));
        assert_eq!(s.usb_serial, None);
        assert_eq!(s.usb_vendor.as_deref(), Some("18d1"));
        assert_eq!(s.usb_product.as_deref(), Some("4ee1"));
        assert_eq!(s.maker.as_deref(), Some("Google"));
        assert_eq!(s.model.as_deref(), Some("Pixel 7"));
        assert_eq!(s.friendly_name.as_deref(), Some("My phone"));
        assert_eq!(s.capacity, Some(128_000_000_000));
        assert_eq!(s.storage_names.len(), 2);
        // Blank answers are no signal.
        let blank = build_signals(&PhoneInfo {
            serial: " ".into(),
            model: " ".into(),
            friendly_name: "\u{200e}".into(),
            ..PhoneInfo::default()
        });
        assert_eq!(blank, Signals::default());
    }

    #[test]
    fn a_phone_held_by_the_desktop_is_told_to_close_its_window() {
        assert!(gvfs_has_phone(&["mtp:host=Google_Pixel_7_R58M", "other"]));
        assert!(gvfs_has_phone(&["gphoto2:host=x"]));
        assert!(!gvfs_has_phone(&["smb-share:server=nas", "sftp:host=x"]));
        assert!(!gvfs_has_phone::<&str>(&[]));
        let busy = open_failure_message(true);
        assert!(
            busy.contains("file manager") && busy.contains("unmount"),
            "{busy}"
        );
        let other = open_failure_message(false);
        assert!(
            other.contains("File transfer") && !other.contains("unmount"),
            "{other}"
        );
    }

    #[test]
    fn a_phone_with_no_storage_is_told_to_choose_file_transfer() {
        let m = no_storage_message();
        assert!(
            m.contains("File transfer") && m.contains("No data transfer"),
            "{m}"
        );
    }

    #[test]
    fn the_label_names_the_phone_or_falls_back_to_the_usb_ids() {
        let r = |v: &str, p: &str| RawInfo {
            bus: 1,
            devnum: 2,
            vendor_id: 0x18d1,
            product_id: 0x4ee1,
            vendor: v.into(),
            product: p.into(),
        };
        assert_eq!(r("Google", "Pixel 7").label(), "Google Pixel 7 (MTP)");
        assert_eq!(r("", "").label(), "Phone or player (MTP, 18d1:4ee1)");
    }

    #[test]
    fn an_object_id_must_be_a_number() {
        assert_eq!(parse_object_id("42").unwrap(), 42);
        assert!(parse_object_id("-1").is_err());
        assert!(parse_object_id("/DCIM").is_err());
    }

    #[test]
    #[cfg(all(target_pointer_width = "64", target_os = "linux"))]
    fn the_declared_structs_match_libmtps_layout() {
        use std::mem::{offset_of, size_of};
        assert_eq!(size_of::<ffi::RawDevice>(), 40);
        assert_eq!(offset_of!(ffi::RawDevice, bus_location), 32);
        assert_eq!(size_of::<ffi::DeviceStorage>(), 72);
        assert_eq!(offset_of!(ffi::MtpDevice, storage), 24);
        assert_eq!(size_of::<ffi::MtpFile>(), 56);
        assert_eq!(offset_of!(ffi::MtpFile, modificationdate), 32);
        assert_eq!(offset_of!(ffi::MtpFile, next), 48);
    }
}
