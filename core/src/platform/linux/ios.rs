//! iPhones through usbmuxd and AFC (libimobiledevice). Read-only by construction:
//!
//! - the only functions declared (in [`ffi`]) are the ones that list devices, connect, ask
//!   lockdown for values, start the file service, and read directory listings, file
//!   information and file data. No AFC call that writes, creates, renames, removes or links is
//!   declared anywhere, so nothing in the app can call it;
//! - files are opened with `AFC_FOPEN_RDONLY` only;
//! - a test reads this file and checks that [`ffi`] declares nothing outside a list of reading
//!   functions, and that the one file-open call passes `AFC_FOPEN_RDONLY`;
//! - the lockdown handshake that opens a session may begin pairing: the phone then asks its own
//!   "Trust This Computer?" question, and the pairing record it writes (on the PC and the phone)
//!   is the one state change a connection can cause. It is the phone's dialog and the user's
//!   answer, not something the app writes;
//! - every returned resource is freed: strings and lists, property-list nodes, service
//!   descriptors, file handles, and the lockdown, AFC and device handles.
//!
//! Nothing here is specific to Linux: FreeBSD can use this file as it is once its build links
//! the libraries from ports.

use std::collections::HashSet;
use std::ffi::{CStr, CString, c_char, c_int};
use std::io::{self, Read};
use std::marker::PhantomData;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use log::{debug, info, warn};

use crate::counted;
use crate::device::{
    DeviceError, DeviceFile, DeviceIdentity, DeviceSource, Enumeration, ObjectInfo, ScanIssue,
    strip_direction_marks,
};
use crate::paths::MAX_COMPONENTS;
use crate::platform::{ListedDevice, Transport};
use crate::signals::{DeviceKind, Signals, View};

/// The part of libimobiledevice and libplist this app uses, and no more. Every function here
/// only reads. There is deliberately no declaration for any AFC call that writes, and none for
/// pairing, activation, backup, or any other lockdown service.
mod ffi {
    use std::ffi::{c_char, c_int, c_void};

    /// A handle type the libraries own; only ever used through a pointer.
    #[repr(C)]
    pub struct Opaque {
        _private: [u8; 0],
    }

    #[repr(C)]
    pub struct DeviceInfo {
        pub udid: *mut c_char,
        pub conn_type: c_int,
        pub conn_data: *mut c_void,
    }

    pub const SUCCESS: c_int = 0;
    /// `IDEVICE_LOOKUP_USBMUX`: the USB connection only, not the network.
    pub const LOOKUP_USBMUX: c_int = 1 << 1;
    /// `CONNECTION_USBMUXD`.
    pub const CONNECTION_USB: c_int = 1;
    /// `AFC_FOPEN_RDONLY`.
    pub const FOPEN_RDONLY: c_int = 1;
    /// `PLIST_STRING`.
    pub const PLIST_STRING: c_int = 3;

    super::super::dynlib::dyn_api! {
        sonames: &["libimobiledevice-1.0.so.6", "libimobiledevice-1.0.so"],
        off: "iPhone support is off: libimobiledevice isn't installed",
        api: ImdApi,
        get: imd_api,
        load: load_imd_api;
        pub fn idevice_get_device_list_extended(
            devices: *mut *mut *mut DeviceInfo,
            count: *mut c_int,
        ) -> c_int;
        pub fn idevice_device_list_extended_free(devices: *mut *mut DeviceInfo) -> c_int;
        pub fn idevice_new_with_options(
            device: *mut *mut Opaque,
            udid: *const c_char,
            options: c_int,
        ) -> c_int;
        pub fn idevice_free(device: *mut Opaque) -> c_int;
        pub fn idevice_set_debug_level(level: c_int);
        pub fn lockdownd_client_new_with_handshake(
            device: *mut Opaque,
            client: *mut *mut Opaque,
            label: *const c_char,
        ) -> c_int;
        pub fn lockdownd_client_free(client: *mut Opaque) -> c_int;
        pub fn lockdownd_get_value(
            client: *mut Opaque,
            domain: *const c_char,
            key: *const c_char,
            value: *mut *mut Opaque,
        ) -> c_int;
        pub fn lockdownd_start_service(
            client: *mut Opaque,
            identifier: *const c_char,
            service: *mut *mut Opaque,
        ) -> c_int;
        pub fn lockdownd_service_descriptor_free(service: *mut Opaque) -> c_int;
        pub fn afc_client_new(
            device: *mut Opaque,
            service: *mut Opaque,
            client: *mut *mut Opaque,
        ) -> c_int;
        pub fn afc_client_free(client: *mut Opaque) -> c_int;
        pub fn afc_read_directory(
            client: *mut Opaque,
            path: *const c_char,
            list: *mut *mut *mut c_char,
        ) -> c_int;
        pub fn afc_get_file_info(
            client: *mut Opaque,
            path: *const c_char,
            info: *mut *mut *mut c_char,
        ) -> c_int;
        pub fn afc_file_open(
            client: *mut Opaque,
            path: *const c_char,
            mode: c_int,
            handle: *mut u64,
        ) -> c_int;
        pub fn afc_file_read(
            client: *mut Opaque,
            handle: u64,
            data: *mut c_char,
            length: u32,
            bytes_read: *mut u32,
        ) -> c_int;
        pub fn afc_file_close(client: *mut Opaque, handle: u64) -> c_int;
        pub fn afc_dictionary_free(list: *mut *mut c_char) -> c_int;
    }

    super::super::dynlib::dyn_api! {
        sonames: &["libplist-2.0.so.4", "libplist-2.0.so.3", "libplist-2.0.so"],
        off: "iPhone support is off: libplist isn't installed",
        api: PlistApi,
        get: plist_api,
        load: load_plist_api;
        pub fn plist_get_node_type(node: *mut Opaque) -> c_int;
        pub fn plist_get_string_val(node: *mut Opaque, value: *mut *mut c_char);
        pub fn plist_free(node: *mut Opaque);
    }

    /// Whether both libraries were found and every function in them looked up.
    pub fn available() -> bool {
        imd_api().is_some() && plist_api().is_some()
    }
}

use ffi::Opaque;

/// The service lockdown starts for the media folder (`DCIM`, ...). Not `com.apple.afc2`,
/// which would show the whole system.
const AFC_SERVICE: &str = "com.apple.afc";
/// Names this app gives itself to the phone during the handshake.
const CLIENT_LABEL: &str = "Plug-n-Keep";
/// How much one AFC read asks for.
const READ_CHUNK: usize = 256 * 1024;

/// Loads the libraries (once) and quietens them. `false` when they aren't here: no iPhone can be
/// listed or opened then.
fn init_once() -> bool {
    static INIT: Once = Once::new();
    if !ffi::available() {
        return false;
    }
    INIT.call_once(|| {
        // Quiet unless something is wrong; the library's own messages go to stderr.
        // SAFETY: plain setting of a library-wide level.
        unsafe { ffi::idevice_set_debug_level(0) };
    });
    true
}

// ---------------------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------------------

/// What kind of thing an AFC entry is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AfcKind {
    Dir,
    File,
    Link,
    /// A device node, pipe, socket or the like.
    Other(String),
    /// The phone said nothing about the type.
    Unknown,
}

/// What AFC's file information says about one path.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AfcInfo {
    size: Option<u64>,
    /// Last-modified time, nanoseconds since 1970.
    mtime_ns: Option<u64>,
    kind: AfcKind,
}

/// Parses the flat key, value, key, value list `afc_get_file_info` returns (`st_size`,
/// `st_mtime` in nanoseconds, `st_ifmt`, ...). Unknown keys are ignored; a number that doesn't
/// parse counts as absent, and a trailing key with no value is dropped.
fn parse_file_info<S: AsRef<str>>(items: &[S]) -> AfcInfo {
    let mut info = AfcInfo {
        size: None,
        mtime_ns: None,
        kind: AfcKind::Unknown,
    };
    for pair in items.as_chunks::<2>().0 {
        let (key, value) = (pair[0].as_ref(), pair[1].as_ref());
        match key {
            "st_size" => info.size = value.trim().parse().ok(),
            "st_mtime" => info.mtime_ns = value.trim().parse().ok(),
            "st_ifmt" => {
                info.kind = match value {
                    "S_IFDIR" => AfcKind::Dir,
                    "S_IFREG" => AfcKind::File,
                    "S_IFLNK" => AfcKind::Link,
                    other => AfcKind::Other(other.to_string()),
                }
            }
            _ => {}
        }
    }
    info
}

/// A modified time from nanoseconds since 1970. Zero means the phone doesn't know.
fn mtime_from_ns(ns: u64) -> Option<SystemTime> {
    if ns == 0 {
        return None;
    }
    // Whole seconds and the nanoseconds left over, so a huge value can't overflow.
    let t = SystemTime::UNIX_EPOCH + Duration::new(ns / 1_000_000_000, (ns % 1_000_000_000) as u32);
    Some(t)
}

/// A child's AFC path: `/` + name at the top, else parent + `/` + name.
fn join_path(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}

/// The last part of an AFC path.
fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// A UDID as the USB serial writes it: without the dash newer phones' UDIDs carry.
fn serial_from_udid(udid: &str) -> String {
    udid.trim().replace('-', "")
}

/// The ID of a listed iPhone: its UDID.
fn device_id(udid: &str) -> String {
    format!("afc:{udid}")
}

fn parse_device_id(id: &str) -> Option<&str> {
    id.strip_prefix("afc:").filter(|u| !u.is_empty())
}

/// What the picker shows before the phone is opened.
fn list_label(udid: &str) -> String {
    let short: String = serial_from_udid(udid).chars().take(8).collect();
    format!("Apple iPhone or iPad ({short}...)")
}

/// What an opened iPhone reports about itself.
#[derive(Default)]
struct PhoneInfo {
    udid: String,
    name: String,
    product_type: String,
}

/// The signals an opened iPhone gives: its UDID (which is also its USB serial), name and
/// product type (`iPhone14,2`).
fn build_signals(p: &PhoneInfo) -> Signals {
    let some = |s: &str| {
        let s = strip_direction_marks(s);
        let s = s.trim();
        (!s.is_empty()).then(|| s.to_string())
    };
    let serial = some(&serial_from_udid(&p.udid));
    Signals {
        device_serial: serial.clone(),
        usb_serial: serial,
        usb_vendor: Some("05ac".to_string()),
        maker: Some("Apple Inc.".to_string()),
        model: some(&p.product_type),
        friendly_name: some(&p.name),
        ..Signals::default()
    }
}

const NOT_TRUSTED: &str = "The iPhone has not trusted this computer. Unlock the iPhone and tap \
    Trust on its screen (enter its passcode if asked), then try again.";

/// What to tell the user for a failure to start talking to the phone (libimobiledevice's
/// `idevice_error_t`).
fn connect_failure_message(code: i32) -> String {
    match code {
        -3 => "The iPhone could not be reached. Check that it is plugged in and that usbmuxd is \
               running (sudo systemctl start usbmuxd), then try again."
            .into(),
        c => format!(
            "Could not connect to the iPhone (error {c}). Unplug and replug it, and try again."
        ),
    }
}

/// What to tell the user for a failed lockdown handshake (`lockdownd_error_t`). The phone not
/// yet trusting this computer is the usual cause.
fn handshake_failure_message(code: i32) -> String {
    match code {
        // password protected, user denied, dialog pending, pairing failed, missing or invalid
        // pair record or host ID
        -17 | -18 | -19 | -4 | -20 | -21 | -29 | -31 => NOT_TRUSTED.into(),
        -7 | -8 => "The iPhone did not answer. Unlock it, check that usbmuxd is running, and \
                    try again."
            .into(),
        c => format!(
            "Could not start a session with the iPhone (error {c}). Unlock it and tap Trust if asked, then try again."
        ),
    }
}

/// What to tell the user when the file service can't be started (`lockdownd_error_t`).
fn service_failure_message(code: i32) -> String {
    match code {
        -17 | -18 | -19 | -4 => NOT_TRUSTED.into(),
        -34 => "The iPhone does not allow access to its files (a restriction or management \
                profile may block it)."
            .into(),
        c => format!(
            "Could not start the iPhone's file service (error {c}). Unlock the iPhone and try again."
        ),
    }
}

/// A readable name for an AFC error code.
fn afc_error_text(code: i32) -> String {
    let t = match code {
        1 => "unknown error",
        3 => "the phone has no resources free",
        4 => "read error",
        7 => "invalid argument",
        8 => "no such file or folder",
        9 => "that is a folder",
        10 => "permission denied",
        11 => "the file service is not connected",
        12 => "the phone did not answer in time",
        17 => "the file is busy",
        20 => "I/O error",
        21 => "the operation was interrupted",
        23 => "internal error",
        30 => "the USB connection failed",
        _ => "",
    };
    if t.is_empty() {
        format!("AFC error {code}")
    } else {
        format!("{t} (AFC error {code})")
    }
}

// ---------------------------------------------------------------------------------------
// Owned handles
// ---------------------------------------------------------------------------------------

/// Copy a NUL-terminated `char**` list into owned byte strings and free it.
///
/// # Safety
/// `list` must be null or a NULL-terminated array from libimobiledevice that nobody else frees.
unsafe fn take_list(list: *mut *mut c_char) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    if list.is_null() {
        return out;
    }
    let mut i = 0;
    loop {
        // SAFETY: the array is NULL-terminated, so entries up to and including the terminator
        // are readable.
        let p = unsafe { *list.add(i) };
        if p.is_null() {
            break;
        }
        // SAFETY: each entry is a NUL-terminated string.
        out.push(unsafe { CStr::from_ptr(p) }.to_bytes().to_vec());
        i += 1;
    }
    // SAFETY: the list is ours to free, once.
    unsafe { ffi::afc_dictionary_free(list) };
    out
}

struct Device(*mut Opaque);
impl Drop for Device {
    fn drop(&mut self) {
        // SAFETY: made by idevice_new_with_options; freed once.
        unsafe { ffi::idevice_free(self.0) };
    }
}

struct Lockdown(*mut Opaque);
impl Drop for Lockdown {
    fn drop(&mut self) {
        // SAFETY: made by lockdownd_client_new_with_handshake; freed once.
        unsafe { ffi::lockdownd_client_free(self.0) };
    }
}

struct Service(*mut Opaque);
impl Drop for Service {
    fn drop(&mut self) {
        // SAFETY: made by lockdownd_start_service; freed once.
        unsafe { ffi::lockdownd_service_descriptor_free(self.0) };
    }
}

struct Afc(*mut Opaque);
impl Drop for Afc {
    fn drop(&mut self) {
        // SAFETY: made by afc_client_new; freed once.
        unsafe { ffi::afc_client_free(self.0) };
    }
}

impl Lockdown {
    /// A string value from lockdown, if the phone has one.
    fn string(&self, key: &str) -> Option<String> {
        let key = CString::new(key).ok()?;
        let mut node: *mut Opaque = std::ptr::null_mut();
        // SAFETY: the client is live; `key` and `node` outlive the call. A null domain is
        // the global domain.
        let rc =
            unsafe { ffi::lockdownd_get_value(self.0, std::ptr::null(), key.as_ptr(), &mut node) };
        if rc != ffi::SUCCESS || node.is_null() {
            return None;
        }
        let mut out = None;
        // SAFETY: `node` is a live node we own; the string it gives is freed with `free`.
        unsafe {
            if ffi::plist_get_node_type(node) == ffi::PLIST_STRING {
                let mut s: *mut c_char = std::ptr::null_mut();
                ffi::plist_get_string_val(node, &mut s);
                if !s.is_null() {
                    out = Some(CStr::from_ptr(s).to_string_lossy().into_owned());
                    libc::free(s.cast());
                }
            }
            ffi::plist_free(node);
        }
        out
    }
}

// ---------------------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------------------

/// Whether the last [`list`] couldn't reach usbmuxd.
static MUXD_DOWN: AtomicBool = AtomicBool::new(false);

/// Said when a connection is asked for but the libraries are missing.
const MISSING: &str = "iPhone support is off: libimobiledevice isn't installed";

/// What to tell the user when no iPhone can ever be listed: the libraries are missing, or the
/// last listing couldn't reach usbmuxd. Either would otherwise look the same as no iPhone being
/// plugged in.
pub fn hint() -> Option<String> {
    if !ffi::available() {
        return Some(
            "iPhones need libimobiledevice (Ubuntu: sudo apt install libimobiledevice6)"
                .to_string(),
        );
    }
    muxd_hint(MUXD_DOWN.load(Ordering::Relaxed))
}

fn muxd_hint(down: bool) -> Option<String> {
    down.then(|| {
        "iPhone support needs usbmuxd running (sudo systemctl start usbmuxd). \
         Without it no iPhone is listed."
            .to_string()
    })
}

/// The iPhones that are plugged in, through usbmuxd. Opens no session with any of them.
pub fn list() -> Vec<ListedDevice> {
    if !init_once() {
        MUXD_DOWN.store(false, Ordering::Relaxed);
        return Vec::new();
    }
    let mut devs: *mut *mut ffi::DeviceInfo = std::ptr::null_mut();
    let mut count: c_int = 0;
    // SAFETY: both out-pointers point to live locals.
    let rc = unsafe { ffi::idevice_get_device_list_extended(&mut devs, &mut count) };
    MUXD_DOWN.store(rc != ffi::SUCCESS, Ordering::Relaxed);
    if rc != ffi::SUCCESS {
        // Most often usbmuxd is not running, which just means no iPhones.
        debug!("usbmuxd listed no devices (error {rc})");
        return Vec::new();
    }
    let mut out = Vec::new();
    if !devs.is_null() {
        for i in 0..usize::try_from(count).unwrap_or(0) {
            // SAFETY: the list holds `count` entries; the strings are copied, not kept.
            let (udid, conn) = unsafe {
                let p = *devs.add(i);
                if p.is_null() || (*p).udid.is_null() {
                    continue;
                }
                (
                    CStr::from_ptr((*p).udid).to_string_lossy().into_owned(),
                    (*p).conn_type,
                )
            };
            // Over the network a phone is not "plugged in"; only the USB connection counts.
            if conn != ffi::CONNECTION_USB || udid.is_empty() {
                continue;
            }
            out.push(ListedDevice {
                id: device_id(&udid),
                label: list_label(&udid),
                kind_hint: Some(DeviceKind::Ptp),
                transport: Transport::Afc,
            });
        }
        // SAFETY: the list was made by the call above; freed once.
        unsafe { ffi::idevice_device_list_extended_free(devs) };
    }
    out
}

// ---------------------------------------------------------------------------------------
// An open iPhone
// ---------------------------------------------------------------------------------------

pub struct AfcSource {
    // Declared in the order they must be released: the file service before the connection.
    afc: Afc,
    _device: Device,
    identity: DeviceIdentity,
}

/// Opens a listed iPhone as a read-only source.
pub fn open(device: &ListedDevice) -> Result<Box<dyn DeviceSource>, String> {
    let udid = parse_device_id(&device.id)
        .ok_or_else(|| format!("{} is not an iPhone this app listed", device.id))?;
    if !init_once() {
        return Err(MISSING.into());
    }
    let c_udid = CString::new(udid).map_err(|_| "the iPhone's ID is not valid".to_string())?;
    let c_label = CString::new(CLIENT_LABEL).map_err(|e| e.to_string())?;

    let mut dev: *mut Opaque = std::ptr::null_mut();
    // SAFETY: the out-pointer and the string outlive the call.
    let rc =
        unsafe { ffi::idevice_new_with_options(&mut dev, c_udid.as_ptr(), ffi::LOOKUP_USBMUX) };
    if rc != ffi::SUCCESS || dev.is_null() {
        return Err(connect_failure_message(rc));
    }
    let device_handle = Device(dev);

    let mut lc: *mut Opaque = std::ptr::null_mut();
    // SAFETY: the device is live; the out-pointer and the label outlive the call.
    let rc = unsafe { ffi::lockdownd_client_new_with_handshake(dev, &mut lc, c_label.as_ptr()) };
    if rc != ffi::SUCCESS || lc.is_null() {
        return Err(handshake_failure_message(rc));
    }
    let lockdown = Lockdown(lc);

    let info = PhoneInfo {
        udid: lockdown
            .string("UniqueDeviceID")
            .unwrap_or_else(|| udid.to_string()),
        name: lockdown.string("DeviceName").unwrap_or_default(),
        product_type: lockdown.string("ProductType").unwrap_or_default(),
    };

    let c_service = CString::new(AFC_SERVICE).map_err(|e| e.to_string())?;
    let mut sd: *mut Opaque = std::ptr::null_mut();
    // SAFETY: the client is live; the out-pointer and the identifier outlive the call.
    let rc = unsafe { ffi::lockdownd_start_service(lc, c_service.as_ptr(), &mut sd) };
    if rc != ffi::SUCCESS || sd.is_null() {
        return Err(service_failure_message(rc));
    }
    let service = Service(sd);

    let mut ac: *mut Opaque = std::ptr::null_mut();
    // SAFETY: the device and the descriptor are live; the out-pointer outlives the call.
    let rc = unsafe { ffi::afc_client_new(dev, sd, &mut ac) };
    if rc != ffi::SUCCESS || ac.is_null() {
        return Err(format!(
            "Could not open the iPhone's file service: {}",
            afc_error_text(rc)
        ));
    }
    drop(service);
    drop(lockdown);

    let clean = |s: &str| strip_direction_marks(s).trim().to_string();
    let mut identity = DeviceIdentity::new(
        &device.id,
        clean(&info.name),
        "Apple Inc.".to_string(),
        clean(&info.product_type),
        Some(serial_from_udid(&info.udid)).filter(|s| !s.is_empty()),
    );
    identity.signals = build_signals(&info);
    identity.view = View::Afc;
    info!(
        "Opened \"{}\" through AFC: {}, UDID {}",
        identity.friendly_name, identity.model, info.udid
    );
    Ok(Box::new(AfcSource {
        afc: Afc(ac),
        _device: device_handle,
        identity,
    }))
}

impl AfcSource {
    /// The names in a folder, without `.` and `..`. Names that aren't valid UTF-8 come back
    /// as `Err` (the raw name, lossily) so the walk can report them.
    fn read_dir(&self, path: &str) -> Result<Vec<Result<String, String>>, String> {
        let c = CString::new(path).map_err(|_| "the path has a NUL in it".to_string())?;
        let mut list: *mut *mut c_char = std::ptr::null_mut();
        // SAFETY: the client is live; the path and out-pointer outlive the call.
        let rc = unsafe { ffi::afc_read_directory(self.afc.0, c.as_ptr(), &mut list) };
        // SAFETY: the list (if any) is consumed and freed here.
        let names = unsafe { take_list(list) };
        if rc != ffi::SUCCESS {
            return Err(afc_error_text(rc));
        }
        Ok(names
            .into_iter()
            .filter(|n| n != b"." && n != b"..")
            .map(|n| {
                String::from_utf8(n).map_err(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
            })
            .collect())
    }

    fn file_info(&self, path: &str) -> Result<AfcInfo, String> {
        let c = CString::new(path).map_err(|_| "the path has a NUL in it".to_string())?;
        let mut list: *mut *mut c_char = std::ptr::null_mut();
        // SAFETY: the client is live; the path and out-pointer outlive the call.
        let rc = unsafe { ffi::afc_get_file_info(self.afc.0, c.as_ptr(), &mut list) };
        // SAFETY: the list (if any) is consumed and freed here.
        let items = unsafe { take_list(list) };
        if rc != ffi::SUCCESS {
            return Err(afc_error_text(rc));
        }
        let items: Vec<String> = items
            .into_iter()
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .collect();
        Ok(parse_file_info(&items))
    }
}

/// Record a problem found while walking the phone, and log it.
fn report(result: &mut Enumeration, here: &str, problem: impl Into<String>) {
    let issue = ScanIssue::new(here, problem);
    warn!("iPhone: {}: {}", issue.location, issue.problem);
    result.issues.push(issue);
}

impl DeviceSource for AfcSource {
    fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    /// Walks the folders from `/` iteratively (no recursion), with a depth limit and a
    /// seen-set. Links are reported and never followed.
    fn enumerate(
        &mut self,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(usize, &str),
    ) -> Result<Enumeration, DeviceError> {
        let mut result = Enumeration::default();
        let mut seen: HashSet<String> = HashSet::from(["/".to_string()]);
        // (AFC path, path parts so far, depth)
        let mut stack: Vec<(String, Vec<String>, usize)> = vec![("/".to_string(), Vec::new(), 0)];
        let mut folders = 0usize;

        while let Some((dir, comps, depth)) = stack.pop() {
            if cancel.load(Ordering::Relaxed) {
                return Err(DeviceError::Cancelled);
            }
            let here = if comps.is_empty() {
                "(phone)".to_string()
            } else {
                comps.join("/")
            };
            progress(result.files.len(), &here);
            let names = match self.read_dir(&dir) {
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
            debug!("Listed {here}: {}", counted(names.len(), "item", "items"));
            if names.is_empty() && !comps.is_empty() {
                result.empty_folders.push(comps.join("/"));
            }
            for name in names {
                if cancel.load(Ordering::Relaxed) {
                    return Err(DeviceError::Cancelled);
                }
                let name = match name {
                    Ok(n) => n,
                    Err(lossy) => {
                        report(
                            &mut result,
                            &here,
                            format!("the name \"{lossy}\" is not valid text; skipped"),
                        );
                        result.incomplete.push(comps.join("/"));
                        continue;
                    }
                };
                let child = join_path(&dir, &name);
                if !seen.insert(child.clone()) {
                    report(
                        &mut result,
                        &here,
                        format!("{child} appears twice; ignored the repeat"),
                    );
                    continue;
                }
                let shown = strip_direction_marks(&name);
                if shown.is_empty() {
                    report(&mut result, &here, format!("{child} has no visible name"));
                    result.incomplete.push(comps.join("/"));
                    continue;
                }
                let info = match self.file_info(&child) {
                    Ok(i) => i,
                    Err(e) => {
                        report(&mut result, &here, format!("could not read {child}: {e}"));
                        result.incomplete.push(comps.join("/"));
                        continue;
                    }
                };
                let mut path = comps.clone();
                path.push(shown);
                match info.kind {
                    AfcKind::Dir => {
                        if depth + 1 > MAX_COMPONENTS {
                            report(
                                &mut result,
                                &here,
                                "folders nested too deeply; not scanned further",
                            );
                            result.incomplete.push(path.join("/"));
                            continue;
                        }
                        stack.push((child, path, depth + 1));
                    }
                    AfcKind::File => result.files.push(DeviceFile {
                        object_id: child,
                        persistent_id: None,
                        components: path,
                        size: info.size,
                        modified: info.mtime_ns.and_then(mtime_from_ns),
                        storage: None,
                    }),
                    AfcKind::Link => {
                        report(
                            &mut result,
                            &here,
                            format!("{child} is a link; not followed"),
                        );
                        result.incomplete.push(path.join("/"));
                    }
                    AfcKind::Other(kind) => {
                        report(
                            &mut result,
                            &here,
                            format!("{child} is not a file or folder ({kind}); skipped"),
                        );
                        result.incomplete.push(path.join("/"));
                    }
                    AfcKind::Unknown => {
                        report(
                            &mut result,
                            &here,
                            format!("the phone did not say what {child} is; skipped"),
                        );
                        result.incomplete.push(path.join("/"));
                    }
                }
            }
        }
        result.files.sort_by(|a, b| a.components.cmp(&b.components));
        debug!(
            "Walked the iPhone: {} in {}",
            counted(result.files.len(), "file", "files"),
            counted(folders, "folder", "folders")
        );
        progress(result.files.len(), "");
        Ok(result)
    }

    fn object_info(&mut self, object_id: &str) -> Result<ObjectInfo, DeviceError> {
        let info = self.file_info(object_id).map_err(|e| {
            warn!("Could not read the properties of {object_id} on the iPhone: {e}");
            DeviceError::Failed(e)
        })?;
        if info.kind != AfcKind::File {
            return Err(DeviceError::Failed(
                "the iPhone says that is no longer a plain file".into(),
            ));
        }
        Ok(ObjectInfo {
            name: strip_direction_marks(base_name(object_id)),
            size: info.size,
            persistent_id: None,
            modified: info.mtime_ns.and_then(mtime_from_ns),
            storage: None,
        })
    }

    fn open_read(&mut self, object_id: &str) -> Result<Box<dyn Read + '_>, DeviceError> {
        let c = CString::new(object_id)
            .map_err(|_| DeviceError::Failed("the path has a NUL in it".into()))?;
        let mut handle: u64 = 0;
        // SAFETY: the client is live; the path and handle outlive the call. Read-only mode.
        let rc =
            unsafe { ffi::afc_file_open(self.afc.0, c.as_ptr(), ffi::FOPEN_RDONLY, &mut handle) };
        if rc != ffi::SUCCESS {
            warn!(
                "Could not open {object_id} on the iPhone for reading: {}",
                afc_error_text(rc)
            );
            return Err(DeviceError::Failed(format!(
                "could not open the file on the iPhone: {}",
                afc_error_text(rc)
            )));
        }
        Ok(Box::new(AfcReader {
            client: self.afc.0,
            handle,
            path: object_id.to_string(),
            _source: PhantomData,
        }))
    }
}

/// An open file on the phone, closed on drop, whether it was read to the end or not. A reader
/// dropped early leaves the connection in doubt, which `Guarded` turns into a reopen.
struct AfcReader<'a> {
    client: *mut Opaque,
    handle: u64,
    /// For log lines.
    path: String,
    _source: PhantomData<&'a mut AfcSource>,
}

impl Read for AfcReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let len = buf.len().min(READ_CHUNK) as u32;
        let mut got: u32 = 0;
        // SAFETY: the client is live (the reader borrows its source); `buf` is valid for `len`
        // bytes and `got` is a live local.
        let rc = unsafe {
            ffi::afc_file_read(
                self.client,
                self.handle,
                buf.as_mut_ptr().cast(),
                len,
                &mut got,
            )
        };
        if rc != ffi::SUCCESS {
            warn!(
                "Reading {} from the iPhone failed: {}",
                self.path,
                afc_error_text(rc)
            );
            return Err(io::Error::other(format!(
                "iPhone read failed: {}",
                afc_error_text(rc)
            )));
        }
        Ok((got as usize).min(buf.len()))
    }
}

impl Drop for AfcReader<'_> {
    fn drop(&mut self) {
        // SAFETY: closes the handle opened for this reader, once; the client is still live.
        unsafe { ffi::afc_file_close(self.client, self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn file_info_gives_size_nanosecond_mtime_and_type() {
        let info = parse_file_info(&strs(&[
            "st_size",
            "2481236",
            "st_blocks",
            "4848",
            "st_nlink",
            "1",
            "st_ifmt",
            "S_IFREG",
            "st_mtime",
            "1700000000123456789",
            "st_birthtime",
            "1699999999000000000",
        ]));
        assert_eq!(info.size, Some(2_481_236));
        assert_eq!(info.mtime_ns, Some(1_700_000_000_123_456_789));
        assert_eq!(info.kind, AfcKind::File);
    }

    #[test]
    fn file_info_types_and_odd_input() {
        let kind = |t: &str| parse_file_info(&["st_ifmt", t]).kind;
        assert_eq!(kind("S_IFDIR"), AfcKind::Dir);
        assert_eq!(kind("S_IFLNK"), AfcKind::Link);
        assert_eq!(kind("S_IFSOCK"), AfcKind::Other("S_IFSOCK".into()));
        // Nothing, a missing type, a bad number and a dangling key are all harmless.
        let none: [&str; 0] = [];
        let empty = parse_file_info(&none);
        assert_eq!(
            empty,
            AfcInfo {
                size: None,
                mtime_ns: None,
                kind: AfcKind::Unknown
            }
        );
        let odd = parse_file_info(&["st_size", "-4", "st_mtime", "abc", "st_ifmt"]);
        assert_eq!(
            (odd.size, odd.mtime_ns, odd.kind),
            (None, None, AfcKind::Unknown)
        );
        // A later value wins, as a repeated key would on any list.
        assert_eq!(
            parse_file_info(&["st_size", "1", "st_size", "2"]).size,
            Some(2)
        );
    }

    #[test]
    fn nanoseconds_become_a_time_and_zero_is_none() {
        assert_eq!(mtime_from_ns(0), None);
        assert_eq!(
            mtime_from_ns(1_500_000_000_500_000_000),
            Some(SystemTime::UNIX_EPOCH + Duration::new(1_500_000_000, 500_000_000))
        );
        // The largest value still converts without overflow.
        assert!(mtime_from_ns(u64::MAX).is_some());
    }

    #[test]
    fn paths_join_from_the_top_and_have_a_base_name() {
        assert_eq!(join_path("/", "DCIM"), "/DCIM");
        assert_eq!(join_path("/DCIM", "100APPLE"), "/DCIM/100APPLE");
        assert_eq!(base_name("/DCIM/100APPLE/IMG_0001.HEIC"), "IMG_0001.HEIC");
        assert_eq!(base_name("/DCIM"), "DCIM");
    }

    #[test]
    fn device_ids_round_trip() {
        let id = device_id("00008030-000A12345678802E");
        assert_eq!(parse_device_id(&id), Some("00008030-000A12345678802E"));
        assert_eq!(parse_device_id("afc:"), None);
        assert_eq!(parse_device_id("mtp:1:2"), None);
    }

    #[test]
    fn signals_carry_the_udid_name_and_product_type() {
        let s = build_signals(&PhoneInfo {
            udid: "00008030-000A12345678802E".into(),
            name: "Test iPhone".into(),
            product_type: "iPhone14,2".into(),
        });
        assert_eq!(s.device_serial.as_deref(), Some("00008030000A12345678802E"));
        assert_eq!(s.usb_serial, s.device_serial);
        assert_eq!(s.usb_vendor.as_deref(), Some("05ac"));
        assert_eq!(s.maker.as_deref(), Some("Apple Inc."));
        assert_eq!(s.model.as_deref(), Some("iPhone14,2"));
        assert_eq!(s.friendly_name.as_deref(), Some("Test iPhone"));
        assert_eq!(s.capacity, None);
        // An old 40-character UDID has no dash and is kept whole.
        let old = build_signals(&PhoneInfo {
            udid: "a".repeat(40),
            ..PhoneInfo::default()
        });
        assert_eq!(old.device_serial, Some("a".repeat(40)));
        assert_eq!(old.model, None);
    }

    #[test]
    fn an_untrusted_phone_is_told_to_unlock_and_tap_trust() {
        for code in [-17, -18, -19, -4, -29, -31, -21, -20] {
            let m = handshake_failure_message(code);
            assert!(m.contains("Unlock") && m.contains("Trust"), "{code}: {m}");
        }
        for code in [-17, -18, -19, -4] {
            assert!(service_failure_message(code).contains("Trust"), "{code}");
        }
        // Other failures are not blamed on trust.
        assert!(!handshake_failure_message(-8).contains("Trust"));
        assert!(!handshake_failure_message(-1).contains("tap Trust on its screen"));
        assert!(service_failure_message(-34).contains("restriction"));
    }

    #[test]
    fn a_missing_usbmuxd_is_said_so_in_the_device_list() {
        assert!(muxd_hint(false).is_none());
        assert!(muxd_hint(true).unwrap().contains("usbmuxd"));
    }

    /// DESIGN.md 10.3: only reading functions are declared, so nothing here can write to a phone.
    #[test]
    fn only_reading_functions_are_declared() {
        const ALLOWED: [&str; 21] = [
            "idevice_get_device_list_extended",
            "idevice_device_list_extended_free",
            "idevice_new_with_options",
            "idevice_free",
            "idevice_set_debug_level",
            "lockdownd_client_new_with_handshake",
            "lockdownd_client_free",
            "lockdownd_get_value",
            "lockdownd_start_service",
            "lockdownd_service_descriptor_free",
            "afc_client_new",
            "afc_client_free",
            "afc_read_directory",
            "afc_get_file_info",
            "afc_file_open",
            "afc_file_read",
            "afc_file_close",
            "afc_dictionary_free",
            "plist_get_node_type",
            "plist_get_string_val",
            "plist_free",
        ];
        super::super::dynlib::check::assert_read_only(include_str!("ios.rs"), &ALLOWED);
        // The one file-open call asks for read-only.
        let production = super::super::dynlib::check::production(include_str!("ios.rs"));
        for (i, _) in production.match_indices("ffi::afc_file_open(") {
            let call = &production[i..i + production[i..].find(';').unwrap()];
            assert!(call.contains("ffi::FOPEN_RDONLY"), "{call}");
        }
    }

    /// Without the libraries (or with the test hook set) nothing is listed and the hint says why.
    #[test]
    fn without_the_libraries_nothing_is_listed() {
        if ffi::available() {
            assert!(hint().is_none() || MUXD_DOWN.load(Ordering::Relaxed));
            return;
        }
        assert!(list().is_empty());
        assert!(hint().unwrap().contains("libimobiledevice6"));
        let phone = ListedDevice {
            id: device_id("x"),
            label: String::new(),
            kind_hint: Some(DeviceKind::Ptp),
            transport: Transport::Afc,
        };
        assert!(open(&phone).is_err());
    }

    #[test]
    fn a_missing_phone_points_at_usbmuxd() {
        let m = connect_failure_message(-3);
        assert!(m.contains("usbmuxd"), "{m}");
        assert!(connect_failure_message(-2).contains("-2"));
    }

    #[test]
    fn afc_errors_are_readable_and_always_name_the_code() {
        assert!(afc_error_text(8).contains("no such file"));
        assert!(afc_error_text(10).contains("permission"));
        assert_eq!(afc_error_text(99), "AFC error 99");
        assert!(afc_error_text(99).contains("99") && afc_error_text(12).contains("12"));
    }

    #[test]
    fn the_listing_label_is_short_and_says_apple() {
        let l = list_label("00008030-000A12345678802E");
        assert_eq!(l, "Apple iPhone or iPad (00008030...)");
        let listed = ListedDevice {
            id: device_id("x"),
            label: l,
            kind_hint: Some(DeviceKind::Ptp),
            transport: Transport::Afc,
        };
        assert!(listed.looks_like_apple());
    }
}
