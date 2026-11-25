//! Still cameras through libgphoto2. Read-only by construction:
//!
//! - the only functions declared (in [`ffi`]) detect cameras, open a session, list folders and
//!   files, read a file's information and download a file. Nothing that deletes, uploads,
//!   captures, makes or removes a folder, or changes a setting is declared anywhere, so nothing
//!   in the app can call it. The configuration calls that are declared only configure our own
//!   session (which camera driver and which port to use) or read one setting, the serial number;
//! - every returned resource is freed: lists, the camera, its context, widgets, files. The two
//!   lists that tell drivers and ports apart are loaded once and kept for the whole run;
//! - a file is downloaded through `gp_file_new_from_fd` into a pipe whose other end is the
//!   reader, so memory stays bounded however big the file is. The download runs on a helper
//!   thread; dropping the reader early closes the pipe, which stops the download, and waits for
//!   the helper.
//!
//! The libraries are loaded at run time ([`super::dynlib`]) and are optional.
//!
//! **Not listed twice.** libgphoto2 also drives MTP phones and iPhones. A device is skipped
//! when libmtp lists the same USB bus and device number, when it is not a still camera, when its
//! driver reads as MTP (so, with libmtp missing, an Android phone in file-transfer mode is not
//! offered as a camera: it is a phone, and the hint for libmtp is the one to show), when it is a
//! mass-storage drive (those are read through the file system) and when it is an Apple device
//! (the iPhone path owns those). See [`skip_reason`].

use std::collections::HashSet;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fs::File;
use std::io::{self, Read};
use std::marker::PhantomData;
use std::os::fd::FromRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::SystemTime;

use log::{debug, info, warn};

use super::mtp;
use crate::counted;
use crate::device::{
    DeviceError, DeviceFile, DeviceIdentity, DeviceSource, Enumeration, ObjectInfo, ScanIssue,
    storage_folder_names, strip_direction_marks,
};
use crate::paths::MAX_COMPONENTS;
use crate::platform::{ListedDevice, Transport};
use crate::signals::{DeviceKind, Signals, View};

// The structs below mirror libgphoto2 2.5's headers (`gphoto2-abilities-list.h`,
// `gphoto2-filesys.h`) field for field, with `const` checks of their sizes and of the offsets
// that are read. They hold 64-bit `time_t` and `uint64_t` sizes, so they are for 64-bit only.
#[cfg(not(target_pointer_width = "64"))]
compile_error!("the libgphoto2 struct mirrors are for 64-bit systems only");

/// libgphoto2's `CameraAbilities` (2.5 and later): driver description of one camera model. Only
/// the USB class and the device type are read; the whole struct is handed back by value to
/// `gp_camera_set_abilities`.
#[repr(C)]
#[derive(Clone)]
pub struct Abilities {
    model: [u8; 128],
    status: i32,
    port: i32,
    speed: [i32; 64],
    operations: i32,
    file_operations: i32,
    folder_operations: i32,
    usb_vendor: i32,
    usb_product: i32,
    usb_class: i32,
    usb_subclass: i32,
    usb_protocol: i32,
    library: [u8; 1024],
    id: [u8; 1024],
    device_type: i32,
    reserved: [i32; 7],
}

const _: () = {
    assert!(std::mem::size_of::<Abilities>() == 2504);
    assert!(std::mem::offset_of!(Abilities, usb_class) == 412);
    assert!(std::mem::offset_of!(Abilities, device_type) == 2472);
};

/// `GP_DEVICE_STILL_CAMERA`.
const DEVICE_STILL_CAMERA: i32 = 0;

impl Abilities {
    fn zeroed() -> Self {
        // SAFETY: plain integers and byte arrays, for which all zeroes is a valid value.
        unsafe { std::mem::zeroed() }
    }

    fn usb_class(&self) -> i32 {
        self.usb_class
    }

    fn device_type(&self) -> i32 {
        self.device_type
    }
}

/// `CameraFileInfoPreview`.
#[repr(C)]
#[derive(Clone, Copy)]
struct InfoPreview {
    fields: u32,
    status: u32,
    size: u64,
    kind: [u8; 64],
    width: u32,
    height: u32,
}

/// `CameraFileInfoFile`: the file's own details.
#[repr(C)]
#[derive(Clone, Copy)]
struct InfoFile {
    fields: u32,
    status: u32,
    size: u64,
    kind: [u8; 64],
    width: u32,
    height: u32,
    permissions: u32,
    mtime: i64,
}

/// `CameraFileInfoAudio`.
#[repr(C)]
#[derive(Clone, Copy)]
struct InfoAudio {
    fields: u32,
    status: u32,
    size: u64,
    kind: [u8; 64],
}

/// libgphoto2's `CameraFileInfo`, followed by spare room: if a libgphoto2 were ever to fill in
/// more than the mirror has, it lands in the spare bytes and not in the rest of the stack.
#[repr(C, align(8))]
struct FileInfoBuf {
    preview: InfoPreview,
    file: InfoFile,
    audio: InfoAudio,
    spare: [u8; 1024 - 272],
}

const _: () = {
    assert!(std::mem::size_of::<InfoPreview>() == 88);
    assert!(std::mem::size_of::<InfoFile>() == 104);
    assert!(std::mem::size_of::<InfoAudio>() == 80);
    // `file.fields` at 88, `file.size` at 96 and `file.mtime` at 184 of the `CameraFileInfo`.
    assert!(std::mem::offset_of!(FileInfoBuf, file) == 88);
    assert!(std::mem::offset_of!(InfoFile, size) == 8);
    assert!(std::mem::offset_of!(InfoFile, mtime) == 96);
    assert!(std::mem::offset_of!(FileInfoBuf, audio) == 192);
    assert!(std::mem::size_of::<FileInfoBuf>() == 1024);
};

const INFO_FIELD_SIZE: u32 = 1 << 2;
const INFO_FIELD_MTIME: u32 = 1 << 7;

/// What `gp_camera_file_get_info` said about a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileFacts {
    size: Option<u64>,
    mtime: i64,
}

impl FileInfoBuf {
    fn zeroed() -> Self {
        // SAFETY: plain integers and byte arrays, for which all zeroes is a valid value.
        unsafe { std::mem::zeroed() }
    }

    fn facts(&self) -> FileFacts {
        let f = &self.file;
        FileFacts {
            size: (f.fields & INFO_FIELD_SIZE != 0).then_some(f.size),
            mtime: if f.fields & INFO_FIELD_MTIME != 0 {
                f.mtime
            } else {
                0
            },
        }
    }
}

/// The part of libgphoto2 this app uses, and no more.
mod ffi {
    use std::ffi::{c_char, c_int, c_void};

    use super::Abilities;
    use super::FileInfoBuf;

    /// `GP_FILE_TYPE_NORMAL`: the file itself, not a preview or its metadata.
    pub const FILE_TYPE_NORMAL: c_int = 1;
    /// `GP_ERROR_IO_USB_CLAIM`, `GP_ERROR_IO_LOCK` and `GP_ERROR_CAMERA_BUSY`.
    pub const ERROR_USB_CLAIM: c_int = -53;
    pub const ERROR_IO_LOCK: c_int = -60;
    pub const ERROR_CAMERA_BUSY: c_int = -110;

    super::super::dynlib::dyn_api! {
        sonames: &["libgphoto2.so.6", "libgphoto2.so"],
        off: "Camera support is off: libgphoto2 isn't installed",
        api: Api,
        get: api,
        load: load_api;
        pub fn gp_context_new() -> *mut c_void;
        pub fn gp_context_unref(context: *mut c_void);
        pub fn gp_result_as_string(result: c_int) -> *const c_char;
        pub fn gp_list_new(list: *mut *mut c_void) -> c_int;
        pub fn gp_list_free(list: *mut c_void) -> c_int;
        pub fn gp_list_count(list: *mut c_void) -> c_int;
        pub fn gp_list_get_name(list: *mut c_void, index: c_int, name: *mut *const c_char) -> c_int;
        pub fn gp_list_get_value(list: *mut c_void, index: c_int, value: *mut *const c_char) -> c_int;
        pub fn gp_abilities_list_new(list: *mut *mut c_void) -> c_int;
        pub fn gp_abilities_list_load(list: *mut c_void, context: *mut c_void) -> c_int;
        pub fn gp_abilities_list_free(list: *mut c_void) -> c_int;
        pub fn gp_abilities_list_detect(
            list: *mut c_void,
            ports: *mut c_void,
            found: *mut c_void,
            context: *mut c_void,
        ) -> c_int;
        pub fn gp_abilities_list_lookup_model(list: *mut c_void, model: *const c_char) -> c_int;
        pub fn gp_abilities_list_get_abilities(
            list: *mut c_void,
            index: c_int,
            abilities: *mut Abilities,
        ) -> c_int;
        pub fn gp_camera_new(camera: *mut *mut c_void) -> c_int;
        pub fn gp_camera_unref(camera: *mut c_void) -> c_int;
        pub fn gp_camera_set_abilities(camera: *mut c_void, abilities: Abilities) -> c_int;
        pub fn gp_camera_set_port_info(camera: *mut c_void, info: *mut c_void) -> c_int;
        pub fn gp_camera_init(camera: *mut c_void, context: *mut c_void) -> c_int;
        pub fn gp_camera_exit(camera: *mut c_void, context: *mut c_void) -> c_int;
        pub fn gp_camera_folder_list_folders(
            camera: *mut c_void,
            folder: *const c_char,
            list: *mut c_void,
            context: *mut c_void,
        ) -> c_int;
        pub fn gp_camera_folder_list_files(
            camera: *mut c_void,
            folder: *const c_char,
            list: *mut c_void,
            context: *mut c_void,
        ) -> c_int;
        pub fn gp_camera_file_get_info(
            camera: *mut c_void,
            folder: *const c_char,
            file: *const c_char,
            info: *mut FileInfoBuf,
            context: *mut c_void,
        ) -> c_int;
        pub fn gp_camera_file_get(
            camera: *mut c_void,
            folder: *const c_char,
            file: *const c_char,
            kind: c_int,
            camera_file: *mut c_void,
            context: *mut c_void,
        ) -> c_int;
        pub fn gp_file_new_from_fd(file: *mut *mut c_void, fd: c_int) -> c_int;
        pub fn gp_file_free(file: *mut c_void) -> c_int;
        pub fn gp_camera_get_single_config(
            camera: *mut c_void,
            name: *const c_char,
            widget: *mut *mut c_void,
            context: *mut c_void,
        ) -> c_int;
        pub fn gp_widget_get_value(widget: *mut c_void, value: *mut c_void) -> c_int;
        pub fn gp_widget_free(widget: *mut c_void) -> c_int;
    }

    /// The same library's port half: which ports (USB, a folder) exist.
    pub mod port {
        use std::ffi::{c_char, c_int, c_void};

        super::super::super::dynlib::dyn_api! {
            sonames: &["libgphoto2_port.so.12", "libgphoto2_port.so"],
            off: "Camera support is off: libgphoto2_port isn't installed",
            api: PortApi,
            get: port_api,
            load: load_port_api;
            pub fn gp_port_info_list_new(list: *mut *mut c_void) -> c_int;
            pub fn gp_port_info_list_load(list: *mut c_void) -> c_int;
            pub fn gp_port_info_list_free(list: *mut c_void) -> c_int;
            pub fn gp_port_info_list_lookup_path(list: *mut c_void, path: *const c_char) -> c_int;
            pub fn gp_port_info_list_get_info(
                list: *mut c_void,
                index: c_int,
                info: *mut *mut c_void,
            ) -> c_int;
        }
    }

    /// Whether both libraries were found and every function in them looked up.
    pub fn available() -> bool {
        api().is_some() && port::port_api().is_some()
    }
}

/// What to tell the user when libgphoto2 is missing: without it no camera is ever listed.
pub fn hint() -> Option<String> {
    (!ffi::available()).then(|| {
        "Cameras need libgphoto2 (Ubuntu: sudo apt install libgphoto2-6t64, or libgphoto2-6 on \
         releases before 24.04)"
            .to_string()
    })
}

// ---------------------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------------------

/// The ID of a listed camera: where it sits on the USB bus.
fn device_id(bus: u32, devnum: u32) -> String {
    format!("cam:{bus}:{devnum}")
}

/// The bus and device number in an ID made by [`device_id`].
fn parse_device_id(id: &str) -> Option<(u32, u32)> {
    let rest = id.strip_prefix("cam:")?;
    let (bus, dev) = rest.split_once(':')?;
    Some((bus.parse().ok()?, dev.parse().ok()?))
}

/// The bus and device number in libgphoto2's port name for a USB device: `usb:001,004`.
fn parse_usb_port(path: &str) -> Option<(u32, u32)> {
    let rest = path.strip_prefix("usb:")?;
    let (bus, dev) = rest.split_once(',')?;
    Some((bus.parse().ok()?, dev.parse().ok()?))
}

/// What one autodetected camera entry says, joined with what the USB bus says about it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    model: String,
    port: String,
    bus: u32,
    devnum: u32,
    device_type: i32,
    usb_class: i32,
    usb: Option<UsbFacts>,
}

/// Why a detected device is not listed as a camera.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Skip {
    /// Not a still camera (an audio player, say).
    NotAStillCamera,
    /// libmtp lists it, so it is listed as a phone.
    ListedByMtp,
    /// Its driver is the MTP one: a phone, and libmtp is the way to read it.
    MtpDevice,
    /// A mass-storage drive: read through the file system.
    MassStorage,
    /// An Apple device: the iPhone path owns it.
    Apple,
}

/// `Some(why)` when the device must not be listed as a camera.
fn skip_reason(c: &Candidate, listed_by_mtp: &[(u32, u8)]) -> Option<Skip> {
    if c.device_type != DEVICE_STILL_CAMERA {
        return Some(Skip::NotAStillCamera);
    }
    if listed_by_mtp
        .iter()
        .any(|&(bus, dev)| bus == c.bus && u32::from(dev) == c.devnum)
    {
        return Some(Skip::ListedByMtp);
    }
    if c.model.contains("MTP") {
        return Some(Skip::MtpDevice);
    }
    if c.usb_class == 8 || c.model.contains("Mass Storage") {
        return Some(Skip::MassStorage);
    }
    let apple = c.usb.as_ref().is_some_and(|u| u.vendor == 0x05ac)
        || c.model.contains("iPhone")
        || c.model.contains("iPad")
        || c.model.contains("Apple");
    if apple {
        return Some(Skip::Apple);
    }
    None
}

/// The cameras to list: the candidates nothing skips, one per USB position (libgphoto2 may
/// offer several drivers for one device; the first is the one it ranks first).
fn select(candidates: Vec<Candidate>, listed_by_mtp: &[(u32, u8)]) -> Vec<Candidate> {
    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter(|c| {
            if let Some(why) = skip_reason(c, listed_by_mtp) {
                debug!("Not listing {} ({}) as a camera: {why:?}", c.model, c.port);
                return false;
            }
            seen.insert((c.bus, c.devnum))
        })
        .collect()
}

/// A camera's name for the device picker: its own name, or for the generic PTP driver what the
/// USB device calls itself.
fn label(c: &Candidate) -> String {
    let generic = c.model == "USB PTP Class Camera";
    let named = c.usb.as_ref().and_then(|u| {
        let t = format!(
            "{} {}",
            u.manufacturer.as_deref().unwrap_or(""),
            u.product_name.as_deref().unwrap_or("")
        );
        let t = t.trim();
        (!t.is_empty()).then(|| t.to_string())
    });
    match (generic, named) {
        (true, Some(n)) => format!("{n} (camera)"),
        (true, None) => match &c.usb {
            Some(u) => format!("Camera ({:04x}:{:04x})", u.vendor, u.product),
            None => "Camera (PTP)".to_string(),
        },
        (false, _) => format!("{} (camera)", c.model.trim()),
    }
}

/// What the USB bus says about a device, from sysfs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UsbFacts {
    vendor: u16,
    product: u16,
    serial: Option<String>,
    manufacturer: Option<String>,
    product_name: Option<String>,
}

fn read_text(path: &Path) -> Option<String> {
    let t = std::fs::read_to_string(path).ok()?;
    let t = t.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// The USB device at `bus`/`devnum` under a sysfs USB folder (`/sys/bus/usb/devices`).
fn usb_facts(root: &Path, bus: u32, devnum: u32) -> Option<UsbFacts> {
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let dir = entry.path();
        let number = |name: &str| read_text(&dir.join(name))?.parse::<u32>().ok();
        if number("busnum") != Some(bus) || number("devnum") != Some(devnum) {
            continue;
        }
        let hex = |name: &str| u16::from_str_radix(&read_text(&dir.join(name))?, 16).ok();
        return Some(UsbFacts {
            vendor: hex("idVendor")?,
            product: hex("idProduct")?,
            serial: read_text(&dir.join("serial")),
            manufacturer: read_text(&dir.join("manufacturer")),
            product_name: read_text(&dir.join("product")),
        });
    }
    None
}

/// Where the storages of a camera are: the folders at its root named `store_` and eight hex
/// digits (the PTP storages) become storages named by their order; a camera with none has one
/// storage, its root.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Root {
    folder: String,
    name: String,
}

fn plan_roots(root_folders: &[String]) -> Vec<Root> {
    let mut stores: Vec<&String> = root_folders
        .iter()
        .filter(|f| {
            f.strip_prefix("store_")
                .is_some_and(|h| h.len() == 8 && h.chars().all(|c| c.is_ascii_hexdigit()))
        })
        .collect();
    stores.sort();
    if stores.is_empty() {
        return vec![Root {
            folder: "/".into(),
            name: "Camera".into(),
        }];
    }
    let names = storage_folder_names(&vec![String::new(); stores.len()]);
    stores
        .into_iter()
        .zip(names)
        .map(|(f, name)| Root {
            folder: format!("/{f}"),
            name,
        })
        .collect()
}

/// `/DCIM` + `100CANON` is `/DCIM/100CANON`; the root's children sit directly under `/`.
fn join_folder(folder: &str, name: &str) -> String {
    if folder == "/" {
        format!("/{name}")
    } else {
        format!("{folder}/{name}")
    }
}

/// Splits a camera path into its folder and file name.
fn split_object_id(id: &str) -> Option<(&str, &str)> {
    let i = id.rfind('/')?;
    let (folder, name) = (&id[..i], &id[i + 1..]);
    (!name.is_empty() && id.starts_with('/'))
        .then_some((if folder.is_empty() { "/" } else { folder }, name))
}

/// A file's last-modified time. The camera's clock has no time zone: libgphoto2 reads it as
/// this PC's local time. A camera that doesn't know gives none.
fn camera_time(secs: i64) -> Option<SystemTime> {
    mtp::mtp_time(secs)
}

/// What to tell the user when the camera could not be opened. `code` is libgphoto2's error,
/// `desktop_holds_it` says whether the desktop's file manager has the camera mounted.
fn open_failure_message(code: c_int, text: &str, desktop_holds_it: bool) -> String {
    let claimed = matches!(
        code,
        ffi::ERROR_USB_CLAIM | ffi::ERROR_IO_LOCK | ffi::ERROR_CAMERA_BUSY
    );
    if desktop_holds_it {
        "The camera is in use by the desktop's file manager. Close the camera's window in the \
         file manager, or unmount (eject) the camera there, then try again."
            .into()
    } else if claimed {
        format!(
            "The camera is in use by another program ({text}). Close anything else that uses it \
             (a file manager, a photo app) and try again."
        )
    } else {
        format!(
            "Could not open the camera ({text}). Switch it on, wake it up, and set its USB mode \
             to PTP or file transfer if it has one. If that does not help, this account may lack \
             permission to the USB device."
        )
    }
}

// ---------------------------------------------------------------------------------------
// The libraries
// ---------------------------------------------------------------------------------------

/// A raw libgphoto2 pointer that may move to another thread.
struct SendPtr(*mut c_void);
// SAFETY: a pointer is only used by one thread at a time (see where each is moved).
unsafe impl Send for SendPtr {}

/// The two lists that tell drivers and ports apart, loaded once and kept for the whole run.
struct Lists {
    abilities: SendPtr,
    ports: SendPtr,
    /// The port info found for each path, so a path is looked up once: looking up a USB path
    /// can add an entry to the shared list each time, and the list lives as long as the app.
    port_infos: std::collections::HashMap<String, SendPtr>,
}

static LISTS: Mutex<Option<Lists>> = Mutex::new(None);

/// The lists, loaded on first use. The guard is held while they are used, so only one thread
/// uses them at a time.
fn lists(ctx: *mut c_void) -> Result<MutexGuard<'static, Option<Lists>>, String> {
    let mut guard = LISTS.lock().unwrap_or_else(|p| p.into_inner());
    if guard.is_none() {
        let mut abilities: *mut c_void = std::ptr::null_mut();
        let mut ports: *mut c_void = std::ptr::null_mut();
        // SAFETY: out-pointers to live locals; the context is live. A list that was made is
        // freed again if a later step fails, so nothing is left behind.
        unsafe {
            if ffi::gp_abilities_list_new(&mut abilities) < 0 || abilities.is_null() {
                return Err("libgphoto2 could not load its camera drivers".into());
            }
            if ffi::gp_abilities_list_load(abilities, ctx) < 0 {
                ffi::gp_abilities_list_free(abilities);
                return Err("libgphoto2 could not load its camera drivers".into());
            }
            if ffi::port::gp_port_info_list_new(&mut ports) < 0 || ports.is_null() {
                ffi::gp_abilities_list_free(abilities);
                return Err("libgphoto2 could not load its ports".into());
            }
            if ffi::port::gp_port_info_list_load(ports) < 0 {
                ffi::port::gp_port_info_list_free(ports);
                ffi::gp_abilities_list_free(abilities);
                return Err("libgphoto2 could not load its ports".into());
            }
        }
        *guard = Some(Lists {
            abilities: SendPtr(abilities),
            ports: SendPtr(ports),
            port_infos: std::collections::HashMap::new(),
        });
    }
    Ok(guard)
}

/// A libgphoto2 context, released on drop.
struct Context(*mut c_void);

impl Context {
    /// `None` if libgphoto2 could not make one (it returns null when out of memory).
    fn new() -> Option<Self> {
        // SAFETY: takes nothing.
        let p = unsafe { ffi::gp_context_new() };
        (!p.is_null()).then_some(Self(p))
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: made by gp_context_new and released once.
        unsafe { ffi::gp_context_unref(self.0) };
    }
}

/// libgphoto2's text for an error code.
fn error_text(code: c_int) -> String {
    // SAFETY: returns a static string (or null for a code it doesn't know).
    let p = unsafe { ffi::gp_result_as_string(code) };
    if p.is_null() {
        return format!("error {code}");
    }
    // SAFETY: non-null, NUL-ended, static.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// A libgphoto2 list, freed on drop.
struct List(*mut c_void);

impl List {
    fn new() -> Option<Self> {
        let mut p = std::ptr::null_mut();
        // SAFETY: out-pointer to a live local.
        (unsafe { ffi::gp_list_new(&mut p) } >= 0 && !p.is_null()).then_some(Self(p))
    }

    /// Every (name, value) pair; the strings are copied.
    fn pairs(&self) -> Vec<(String, String)> {
        // SAFETY: the list is live.
        let n = unsafe { ffi::gp_list_count(self.0) };
        let mut out = Vec::new();
        for i in 0..n.max(0) {
            let (mut name, mut value): (*const c_char, *const c_char) =
                (std::ptr::null(), std::ptr::null());
            // SAFETY: index below the count; the strings belong to the list and are copied now.
            let ok = unsafe {
                // A folder's value is empty, and its lookup may say so with an error.
                let _ = ffi::gp_list_get_value(self.0, i, &mut value);
                ffi::gp_list_get_name(self.0, i, &mut name) >= 0
            };
            if !ok || name.is_null() {
                continue;
            }
            // SAFETY: non-null NUL-ended strings owned by the list.
            let text = |p: *const c_char| {
                if p.is_null() {
                    String::new()
                } else {
                    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
                }
            };
            out.push((text(name), text(value)));
        }
        out
    }
}

impl Drop for List {
    fn drop(&mut self) {
        // SAFETY: made by gp_list_new and freed once.
        unsafe { ffi::gp_list_free(self.0) };
    }
}

fn c(s: &str) -> Option<CString> {
    CString::new(s).ok()
}

// ---------------------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------------------

/// The USB cameras libgphoto2 finds right now, before any is skipped.
fn detect() -> Result<Vec<Candidate>, String> {
    if !ffi::available() {
        return Ok(Vec::new());
    }
    let ctx = Context::new().ok_or("libgphoto2 could not make a context")?;
    let guard = lists(ctx.0)?;
    let l = guard.as_ref().expect("the lists were just loaded");
    let found = List::new().ok_or("libgphoto2 could not make a list")?;
    // SAFETY: both lists are live and used by this thread only (the guard is held).
    let rc = unsafe { ffi::gp_abilities_list_detect(l.abilities.0, l.ports.0, found.0, ctx.0) };
    if rc < 0 {
        return Err(format!(
            "libgphoto2 could not look for cameras: {}",
            error_text(rc)
        ));
    }
    let mut out = Vec::new();
    for (model, port) in found.pairs() {
        let Some((bus, devnum)) = parse_usb_port(&port) else {
            continue; // not USB, or a port that can't be told apart
        };
        let Some(model_c) = c(&model) else { continue };
        let mut ab = Abilities::zeroed();
        // SAFETY: the list is live; `ab` is as big as libgphoto2's struct.
        let ok = unsafe {
            let idx = ffi::gp_abilities_list_lookup_model(l.abilities.0, model_c.as_ptr());
            idx >= 0 && ffi::gp_abilities_list_get_abilities(l.abilities.0, idx, &mut ab) >= 0
        };
        if !ok {
            continue;
        }
        out.push(Candidate {
            model,
            port,
            bus,
            devnum,
            device_type: ab.device_type(),
            usb_class: ab.usb_class(),
            usb: usb_facts(Path::new("/sys/bus/usb/devices"), bus, devnum),
        });
    }
    Ok(out)
}

fn listed_cameras() -> Result<Vec<Candidate>, String> {
    Ok(select(detect()?, &mtp::listed_positions()))
}

/// The still cameras that are plugged in. Opens no session.
pub fn list() -> Vec<ListedDevice> {
    match listed_cameras() {
        Ok(found) => found
            .iter()
            .map(|c| ListedDevice {
                id: device_id(c.bus, c.devnum),
                label: label(c),
                kind_hint: Some(DeviceKind::Camera),
                transport: Transport::Camera,
            })
            .collect(),
        Err(e) => {
            debug!("{e}");
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------------------
// An open camera
// ---------------------------------------------------------------------------------------

/// An open camera session, closed on drop.
struct Cam {
    camera: *mut c_void,
    ctx: Context,
}

impl Drop for Cam {
    fn drop(&mut self) {
        // SAFETY: made by gp_camera_new and initialised; closed and released once.
        unsafe {
            ffi::gp_camera_exit(self.camera, self.ctx.0);
            ffi::gp_camera_unref(self.camera);
        }
    }
}

/// Opens the camera driven by `model`'s driver on `port_path` (`usb:001,004`).
fn open_camera(model: &str, port_path: &str) -> Result<Cam, (c_int, String)> {
    let fail = |text: &str| (c_int::MIN, text.to_string());
    let ctx = Context::new().ok_or_else(|| fail("libgphoto2 could not make a context"))?;
    let model_c = c(model).ok_or_else(|| fail("bad model name"))?;
    let port_c = c(port_path).ok_or_else(|| fail("bad port name"))?;
    let mut ab = Abilities::zeroed();
    let mut port_info: *mut c_void = std::ptr::null_mut();
    {
        let mut guard = lists(ctx.0).map_err(|e| fail(&e))?;
        let l = guard.as_mut().expect("the lists were just loaded");
        // SAFETY: the lists are live and used by this thread only; `ab` is as big as the struct.
        unsafe {
            let idx = ffi::gp_abilities_list_lookup_model(l.abilities.0, model_c.as_ptr());
            if idx < 0 || ffi::gp_abilities_list_get_abilities(l.abilities.0, idx, &mut ab) < 0 {
                return Err(fail("the camera's driver is not known"));
            }
            if let Some(known) = l.port_infos.get(port_path) {
                port_info = known.0;
            } else {
                let p = ffi::port::gp_port_info_list_lookup_path(l.ports.0, port_c.as_ptr());
                if p < 0 || ffi::port::gp_port_info_list_get_info(l.ports.0, p, &mut port_info) < 0
                {
                    return Err(fail("the camera's USB port is not known"));
                }
                l.port_infos
                    .insert(port_path.to_string(), SendPtr(port_info));
            }
        }
    }
    let mut camera: *mut c_void = std::ptr::null_mut();
    // SAFETY: out-pointer to a live local.
    if unsafe { ffi::gp_camera_new(&mut camera) } < 0 || camera.is_null() {
        return Err(fail("could not make a camera"));
    }
    // From here the camera is released by `Cam::drop` or, before `init` succeeds, below.
    // SAFETY: the camera is new; the port info belongs to the kept port list.
    let rc = unsafe {
        let a = ffi::gp_camera_set_abilities(camera, ab);
        let b = ffi::gp_camera_set_port_info(camera, port_info);
        if a < 0 {
            a
        } else if b < 0 {
            b
        } else {
            ffi::gp_camera_init(camera, ctx.0)
        }
    };
    if rc < 0 {
        // SAFETY: made above; released once. No `exit`: the session never started.
        unsafe { ffi::gp_camera_unref(camera) };
        return Err((rc, error_text(rc)));
    }
    Ok(Cam { camera, ctx })
}

/// The camera's own serial number, if it will say (read-only).
fn read_serial(cam: &Cam) -> Option<String> {
    for name in ["serialnumber", "eosserialnumber"] {
        let name_c = c(name)?;
        let mut widget: *mut c_void = std::ptr::null_mut();
        // SAFETY: the camera is open; the out-pointer is a live local.
        let rc = unsafe {
            ffi::gp_camera_get_single_config(cam.camera, name_c.as_ptr(), &mut widget, cam.ctx.0)
        };
        if rc < 0 || widget.is_null() {
            continue;
        }
        let mut value: *const c_char = std::ptr::null();
        // SAFETY: a text widget stores a `char *` that the widget owns; it is copied before the
        // widget is freed.
        let text = unsafe {
            let rc = ffi::gp_widget_get_value(widget, (&raw mut value).cast());
            let text = (rc >= 0 && !value.is_null())
                .then(|| CStr::from_ptr(value).to_string_lossy().into_owned());
            ffi::gp_widget_free(widget);
            text
        };
        if let Some(t) = text.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) {
            return Some(t);
        }
    }
    None
}

pub struct CameraSource {
    cam: Cam,
    identity: DeviceIdentity,
    /// The storages, found on first use.
    roots: Option<Vec<Root>>,
}

/// Opens a listed camera as a read-only source.
pub fn open(device: &ListedDevice) -> Result<Box<dyn DeviceSource>, String> {
    let (bus, devnum) = parse_device_id(&device.id)
        .ok_or_else(|| format!("{} is not a camera this app listed", device.id))?;
    if !ffi::available() {
        return Err("Camera support is off: libgphoto2 isn't installed".into());
    }
    let found = listed_cameras()?
        .into_iter()
        .find(|c| (c.bus, c.devnum) == (bus, devnum))
        .ok_or("The camera is no longer plugged in")?;
    let cam = open_camera(&found.model, &found.port).map_err(|(code, text)| {
        let busy = mtp::desktop_holds_phone();
        warn!(
            "Could not open the camera at {} ({text}; the desktop has a phone or camera mounted: {busy})",
            device.id
        );
        open_failure_message(code, &text, busy)
    })?;
    Ok(Box::new(CameraSource::start(cam, &device.id, &found)))
}

impl CameraSource {
    fn start(cam: Cam, id: &str, found: &Candidate) -> Self {
        let serial = read_serial(&cam);
        let clean = |s: &str| strip_direction_marks(s).trim().to_string();
        let usb = found.usb.as_ref();
        let maker = usb
            .and_then(|u| u.manufacturer.clone())
            .or_else(|| found.model.split_whitespace().next().map(str::to_string))
            .unwrap_or_default();
        let model = usb
            .and_then(|u| u.product_name.clone())
            .unwrap_or_else(|| found.model.clone());
        let mut identity = DeviceIdentity::new(
            id,
            clean(&label(found).replace(" (camera)", "")),
            clean(&maker),
            clean(&model),
            serial.clone().filter(|s| !s.trim().is_empty()),
        );
        let some = |s: &str| {
            let s = clean(s);
            (!s.is_empty()).then_some(s)
        };
        identity.signals = Signals {
            device_serial: serial.as_deref().and_then(some),
            usb_serial: usb.and_then(|u| u.serial.as_deref()).and_then(some),
            usb_vendor: usb.map(|u| format!("{:04x}", u.vendor)),
            usb_product: usb.map(|u| format!("{:04x}", u.product)),
            maker: some(&maker),
            model: some(&model),
            friendly_name: some(&found.model),
            ..Signals::default()
        };
        identity.view = View::Camera;
        info!(
            "Opened \"{}\" as a camera: {} {}, serial number {}",
            identity.friendly_name,
            identity.manufacturer,
            identity.model,
            identity.serial.as_deref().unwrap_or("not reported"),
        );
        Self {
            cam,
            identity,
            roots: None,
        }
    }

    /// The names of the folders (`folders`) or files directly in `folder`.
    fn names(&self, folder: &str, folders: bool) -> Result<Vec<String>, String> {
        let folder_c = c(folder).ok_or("a folder name has a NUL in it")?;
        let list = List::new().ok_or("libgphoto2 could not make a list")?;
        // SAFETY: the camera is open, the list is live.
        let rc = unsafe {
            if folders {
                ffi::gp_camera_folder_list_folders(
                    self.cam.camera,
                    folder_c.as_ptr(),
                    list.0,
                    self.cam.ctx.0,
                )
            } else {
                ffi::gp_camera_folder_list_files(
                    self.cam.camera,
                    folder_c.as_ptr(),
                    list.0,
                    self.cam.ctx.0,
                )
            }
        };
        if rc < 0 {
            return Err(error_text(rc));
        }
        Ok(list.pairs().into_iter().map(|(n, _)| n).collect())
    }

    /// What the camera says about one file.
    fn facts(&self, folder: &str, name: &str) -> Result<FileFacts, String> {
        let (folder_c, name_c) = (
            c(folder).ok_or("bad folder name")?,
            c(name).ok_or("bad file name")?,
        );
        let mut info = FileInfoBuf::zeroed();
        // SAFETY: the camera is open; the buffer is at least as big as libgphoto2's struct.
        let rc = unsafe {
            ffi::gp_camera_file_get_info(
                self.cam.camera,
                folder_c.as_ptr(),
                name_c.as_ptr(),
                &mut info,
                self.cam.ctx.0,
            )
        };
        if rc < 0 {
            return Err(error_text(rc));
        }
        Ok(info.facts())
    }

    fn roots(&mut self) -> Result<&[Root], String> {
        if self.roots.is_none() {
            let top = self.names("/", true)?;
            self.roots = Some(plan_roots(&top));
        }
        Ok(self.roots.as_deref().unwrap_or_default())
    }

    /// The storage's name for a camera path, if it is under one.
    fn storage_of(&self, path: &str) -> Option<String> {
        let roots = self.roots.as_deref()?;
        roots
            .iter()
            .find(|r| r.folder == "/" || path.starts_with(&format!("{}/", r.folder)))
            .map(|r| r.name.clone())
    }
}

fn report(result: &mut Enumeration, here: &str, problem: impl Into<String>) {
    let issue = ScanIssue::new(here, problem);
    warn!("Camera: {}: {}", issue.location, issue.problem);
    result.issues.push(issue);
}

impl DeviceSource for CameraSource {
    fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    /// Walks each storage iteratively, with a depth limit and a seen-set, so a deep or cyclic
    /// tree from a hostile camera can't overflow the stack or loop forever.
    fn enumerate(
        &mut self,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(usize, &str),
    ) -> Result<Enumeration, DeviceError> {
        let mut result = Enumeration::default();
        let roots: Vec<Root> = self.roots().map_err(DeviceError::Failed)?.to_vec();
        let mut seen: HashSet<String> = HashSet::new();
        // (camera folder, the path parts below its storage, depth, storage name)
        let mut stack: Vec<(String, Vec<String>, usize, String)> = roots
            .iter()
            .rev()
            .map(|r| (r.folder.clone(), Vec::new(), 0, r.name.clone()))
            .collect();
        let mut folders = 0usize;

        while let Some((folder, comps, depth, storage)) = stack.pop() {
            if cancel.load(Ordering::Relaxed) {
                return Err(DeviceError::Cancelled);
            }
            let here = if comps.is_empty() {
                "(camera)".to_string()
            } else {
                comps.join("/")
            };
            progress(result.files.len(), &here);
            if !seen.insert(folder.clone()) {
                report(
                    &mut result,
                    &here,
                    "this folder appears twice; ignored the repeat",
                );
                result.incomplete.push(comps.join("/"));
                continue;
            }
            let (subfolders, files) = match (self.names(&folder, true), self.names(&folder, false))
            {
                (Ok(d), Ok(f)) => (d, f),
                (d, f) => {
                    let why = d.err().or(f.err()).unwrap_or_default();
                    report(
                        &mut result,
                        &here,
                        format!("could not list this folder: {why}"),
                    );
                    result.incomplete.push(comps.join("/"));
                    continue;
                }
            };
            folders += 1;
            debug!(
                "Listed {here}: {} and {}",
                counted(subfolders.len(), "folder", "folders"),
                counted(files.len(), "file", "files")
            );
            if subfolders.is_empty() && files.is_empty() && !comps.is_empty() {
                result.empty_folders.push(comps.join("/"));
            }
            for name in subfolders {
                let clean = strip_direction_marks(&name);
                if clean.is_empty() || clean.contains('\u{fffd}') {
                    report(
                        &mut result,
                        &here,
                        format!("a folder called {name:?} can't be read"),
                    );
                    result.incomplete.push(comps.join("/"));
                    continue;
                }
                if depth + 1 > MAX_COMPONENTS {
                    report(
                        &mut result,
                        &here,
                        "folders nested too deeply; not scanned further",
                    );
                    result.incomplete.push(comps.join("/"));
                    continue;
                }
                let mut path = comps.clone();
                path.push(clean);
                stack.push((
                    join_folder(&folder, &name),
                    path,
                    depth + 1,
                    storage.clone(),
                ));
            }
            for name in files {
                if cancel.load(Ordering::Relaxed) {
                    return Err(DeviceError::Cancelled);
                }
                let clean = strip_direction_marks(&name);
                if clean.is_empty() || clean.contains('\u{fffd}') {
                    report(
                        &mut result,
                        &here,
                        format!("a file called {name:?} can't be read"),
                    );
                    result.incomplete.push(comps.join("/"));
                    continue;
                }
                let facts = match self.facts(&folder, &name) {
                    Ok(f) => f,
                    Err(e) => {
                        report(
                            &mut result,
                            &here,
                            format!("could not read the details of {clean}: {e}"),
                        );
                        result.incomplete.push(comps.join("/"));
                        continue;
                    }
                };
                let mut path = comps.clone();
                path.push(clean);
                result.files.push(DeviceFile {
                    object_id: join_folder(&folder, &name),
                    persistent_id: None,
                    components: path,
                    size: facts.size,
                    modified: camera_time(facts.mtime),
                    storage: Some(storage.clone()),
                });
            }
        }
        result.files.sort_by(|a, b| a.components.cmp(&b.components));
        debug!(
            "Walked the camera: {} in {}",
            counted(result.files.len(), "file", "files"),
            counted(folders, "folder", "folders")
        );
        progress(result.files.len(), "");
        Ok(result)
    }

    fn object_info(&mut self, object_id: &str) -> Result<ObjectInfo, DeviceError> {
        let (folder, name) = split_object_id(object_id)
            .ok_or_else(|| DeviceError::Failed(format!("{object_id} is not a camera path")))?;
        self.roots().map_err(DeviceError::Failed)?;
        let facts = self.facts(folder, name).map_err(|e| {
            warn!("Could not read the details of {object_id} on the camera: {e}");
            DeviceError::Failed(e)
        })?;
        Ok(ObjectInfo {
            name: strip_direction_marks(name),
            size: facts.size,
            persistent_id: None,
            modified: camera_time(facts.mtime),
            storage: self.storage_of(object_id),
        })
    }

    fn open_read(&mut self, object_id: &str) -> Result<Box<dyn Read + '_>, DeviceError> {
        let (folder, name) = split_object_id(object_id)
            .ok_or_else(|| DeviceError::Failed(format!("{object_id} is not a camera path")))?;
        // What the camera says the file's size is: the download must be exactly that.
        let expected = self.facts(folder, name).ok().and_then(|f| f.size);
        Ok(Box::new(
            CameraReader::start(self, folder, name, expected).map_err(DeviceError::Failed)?,
        ))
    }
}

// ---------------------------------------------------------------------------------------
// Reading a file
// ---------------------------------------------------------------------------------------

/// A running download of one file. The helper thread writes the file into a pipe through
/// libgphoto2; this reads the other end. At the end of the pipe the helper's result is checked,
/// so a download that stopped early is an error and never a short file. Dropping the reader
/// before the end closes the pipe, which makes the download fail, and waits for the helper:
/// the camera must not be used from two threads, and it is not released while a call is on it.
struct CameraReader<'a> {
    pipe: Option<File>,
    thread: Option<JoinHandle<Result<(), String>>>,
    finished: bool,
    /// The size the camera reported for the file, if it did, and how many bytes have come.
    expected: Option<u64>,
    received: u64,
    _source: PhantomData<&'a mut CameraSource>,
}

impl<'a> CameraReader<'a> {
    fn start(
        source: &'a mut CameraSource,
        folder: &str,
        name: &str,
        expected: Option<u64>,
    ) -> Result<Self, String> {
        let (folder_c, name_c) = (
            c(folder).ok_or("bad folder name")?,
            c(name).ok_or("bad file name")?,
        );
        let mut fds = [0 as c_int; 2];
        // SAFETY: `fds` has room for the two descriptors.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(format!(
                "could not make a pipe: {}",
                io::Error::last_os_error()
            ));
        }
        let (read_end, write_end) = (fds[0], fds[1]);
        let camera = SendPtr(source.cam.camera);
        let ctx = SendPtr(source.cam.ctx.0);
        let thread = std::thread::spawn(move || {
            // The whole structs, not just their fields, so the `Send` wrappers travel.
            let (camera, ctx) = (camera, ctx);
            let mut file: *mut c_void = std::ptr::null_mut();
            // SAFETY: the write end is ours; once the file is made, freeing the file closes it.
            if unsafe { ffi::gp_file_new_from_fd(&mut file, write_end) } < 0 || file.is_null() {
                // SAFETY: the descriptor was never handed over.
                unsafe { libc::close(write_end) };
                return Err("libgphoto2 could not start the download".to_string());
            }
            // SAFETY: the camera is open and, until this thread is joined, used by nobody
            // else (the reader borrows the source exclusively). The strings outlive the call.
            let rc = unsafe {
                ffi::gp_camera_file_get(
                    camera.0,
                    folder_c.as_ptr(),
                    name_c.as_ptr(),
                    ffi::FILE_TYPE_NORMAL,
                    file,
                    ctx.0,
                )
            };
            // Closes the write end, which is what ends the reader's file.
            // SAFETY: made above; freed once.
            unsafe { ffi::gp_file_free(file) };
            if rc < 0 { Err(error_text(rc)) } else { Ok(()) }
        });
        // SAFETY: the read end is ours and nothing else owns it.
        let pipe = unsafe { File::from_raw_fd(read_end) };
        Ok(Self {
            pipe: Some(pipe),
            thread: Some(thread),
            finished: false,
            expected,
            received: 0,
            _source: PhantomData,
        })
    }

    /// The helper's result once the pipe has ended.
    fn finish(&mut self) -> io::Result<()> {
        self.finished = true;
        let Some(t) = self.thread.take() else {
            return Ok(());
        };
        match t.join() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                warn!("Reading a file from the camera failed: {e}");
                Err(io::Error::other(format!("camera read failed: {e}")))
            }
            Err(_) => Err(io::Error::other("the camera's transfer ended unexpectedly")),
        }
    }
}

impl Read for CameraReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.finished || buf.is_empty() {
            return Ok(0);
        }
        let Some(pipe) = self.pipe.as_mut() else {
            return Ok(0);
        };
        let n = loop {
            match pipe.read(buf) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                other => break other?,
            }
        };
        self.received += n as u64;
        if n == 0 {
            self.finish()?;
            // A file the camera said was bigger is a short file, not a whole one.
            if let Some(want) = self.expected
                && self.received != want
            {
                return Err(io::Error::other(format!(
                    "the camera sent {} bytes of a {want}-byte file",
                    self.received
                )));
            }
        }
        Ok(n)
    }
}

/// Dropping a reader before its file is finished closes the pipe and waits for the helper. The
/// wait has no time limit, on purpose (the same as for a phone), so **Stop cannot interrupt a
/// camera that has stopped answering**: the app waits until the camera answers or is unplugged: the helper is inside a
/// libgphoto2 call that uses the open camera, which must not be released or used from anywhere
/// else meanwhile. A camera that stops answering part-way can hold this up until it is
/// unplugged, which makes the call return with an error.
impl Drop for CameraReader<'_> {
    fn drop(&mut self) {
        self.pipe = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(model: &str, bus: u32, devnum: u32) -> Candidate {
        Candidate {
            model: model.into(),
            port: format!("usb:{bus:03},{devnum:03}"),
            bus,
            devnum,
            device_type: DEVICE_STILL_CAMERA,
            usb_class: 6,
            usb: None,
        }
    }

    #[test]
    fn a_device_id_round_trips_and_junk_is_refused() {
        let id = device_id(3, 17);
        assert_eq!(id, "cam:3:17");
        assert_eq!(parse_device_id(&id), Some((3, 17)));
        for bad in ["", "cam:", "cam:3", "cam:a:b", "mtp:3:17", "3:17"] {
            assert_eq!(parse_device_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_usb_port_name_gives_the_bus_and_device() {
        assert_eq!(parse_usb_port("usb:001,004"), Some((1, 4)));
        assert_eq!(parse_usb_port("usb:12,130"), Some((12, 130)));
        for bad in [
            "usb:",
            "usb:001",
            "serial:/dev/ttyS0",
            "disk:/x",
            "",
            "usb:a,b",
        ] {
            assert_eq!(parse_usb_port(bad), None, "{bad}");
        }
    }

    /// DESIGN.md 10.3: only reading functions are declared, so nothing here can change a camera.
    const ALLOWED: [&str; 34] = [
        "gp_context_new",
        "gp_context_unref",
        "gp_result_as_string",
        "gp_list_new",
        "gp_list_free",
        "gp_list_count",
        "gp_list_get_name",
        "gp_list_get_value",
        "gp_abilities_list_new",
        "gp_abilities_list_load",
        "gp_abilities_list_free",
        "gp_abilities_list_detect",
        "gp_abilities_list_lookup_model",
        "gp_abilities_list_get_abilities",
        "gp_camera_new",
        "gp_camera_unref",
        "gp_camera_set_abilities",
        "gp_camera_set_port_info",
        "gp_camera_init",
        "gp_camera_exit",
        "gp_camera_folder_list_folders",
        "gp_camera_folder_list_files",
        "gp_camera_file_get_info",
        "gp_camera_file_get",
        "gp_file_new_from_fd",
        "gp_file_free",
        "gp_camera_get_single_config",
        "gp_widget_get_value",
        "gp_widget_free",
        "gp_port_info_list_new",
        "gp_port_info_list_load",
        "gp_port_info_list_free",
        "gp_port_info_list_lookup_path",
        "gp_port_info_list_get_info",
    ];

    #[test]
    fn only_reading_functions_are_declared() {
        super::super::dynlib::check::assert_read_only(include_str!("camera.rs"), &ALLOWED);
    }

    /// The check bites: this module's own source with any one writing function declared in it
    /// is refused.
    #[test]
    fn the_allow_list_check_rejects_every_writing_function() {
        let src = include_str!("camera.rs");
        for bad in super::super::dynlib::check::FORBIDDEN
            .iter()
            .filter(|n| n.starts_with("gp_"))
        {
            let declared = src.replacen(
                "        pub fn gp_widget_free(widget: *mut c_void) -> c_int;\n",
                &format!("        pub fn gp_widget_free(widget: *mut c_void) -> c_int;\n        pub fn {bad}();\n"),
                1,
            );
            assert_ne!(declared, src, "the test's edit did not apply");
            let r = std::panic::catch_unwind(|| {
                super::super::dynlib::check::assert_read_only(&declared, &ALLOWED)
            });
            assert!(r.is_err(), "{bad} was not refused");
        }
    }

    #[test]
    fn a_camera_is_listed_unless_something_else_owns_it() {
        let phones = [(1u32, 5u8)];
        // A still camera on its own position is kept.
        assert_eq!(skip_reason(&candidate("Canon EOS 5D", 1, 7), &phones), None);
        // libmtp lists the same bus and device number: skipped, whatever its driver says.
        assert_eq!(
            skip_reason(&candidate("Canon EOS 5D", 1, 5), &phones),
            Some(Skip::ListedByMtp)
        );
        // The same device number on another bus is another device.
        assert_eq!(skip_reason(&candidate("Canon EOS 5D", 2, 5), &phones), None);
        // Not a still camera.
        let mut player = candidate("Some Player", 1, 8);
        player.device_type = 1;
        assert_eq!(skip_reason(&player, &[]), Some(Skip::NotAStillCamera));
        // With libmtp missing an MTP phone has a gphoto2 entry: it is not a camera.
        assert_eq!(
            skip_reason(&candidate("Samsung Galaxy models (MTP)", 1, 9), &[]),
            Some(Skip::MtpDevice)
        );
        // Mass storage is read through the file system.
        let mut stick = candidate("USB Mass Storage direct IO", 1, 10);
        stick.usb_class = 8;
        assert_eq!(skip_reason(&stick, &[]), Some(Skip::MassStorage));
        // An iPhone is the iPhone path's, found by vendor or by name.
        let mut iphone = candidate("USB PTP Class Camera", 1, 11);
        iphone.usb = Some(UsbFacts {
            vendor: 0x05ac,
            product: 0x12a8,
            serial: None,
            manufacturer: None,
            product_name: None,
        });
        assert_eq!(skip_reason(&iphone, &[]), Some(Skip::Apple));
        assert_eq!(
            skip_reason(&candidate("Apple iPhone (PTP mode)", 1, 12), &[]),
            Some(Skip::Apple)
        );
    }

    #[test]
    fn one_camera_is_listed_once_even_with_several_drivers() {
        let kept = select(
            vec![
                candidate("Canon EOS 5D", 1, 7),
                candidate("USB PTP Class Camera", 1, 7),
                candidate("Nikon D90", 1, 8),
                candidate("Samsung Galaxy models (MTP)", 1, 9),
                candidate("Canon EOS 5D", 1, 5),
            ],
            &[(1, 5)],
        );
        let ids: Vec<String> = kept.iter().map(|c| device_id(c.bus, c.devnum)).collect();
        assert_eq!(ids, ["cam:1:7", "cam:1:8"]);
        assert_eq!(kept[0].model, "Canon EOS 5D");
    }

    #[test]
    fn the_label_names_the_camera() {
        let mut generic = candidate("USB PTP Class Camera", 1, 7);
        assert_eq!(label(&generic), "Camera (PTP)");
        generic.usb = Some(UsbFacts {
            vendor: 0x04a9,
            product: 0x3234,
            serial: Some("0001".into()),
            manufacturer: Some("Canon Inc.".into()),
            product_name: Some("Canon Digital Camera".into()),
        });
        assert_eq!(label(&generic), "Canon Inc. Canon Digital Camera (camera)");
        assert_eq!(
            label(&candidate("Nikon DSC D90 (PTP mode)", 1, 8)),
            "Nikon DSC D90 (PTP mode) (camera)"
        );
    }

    #[test]
    fn the_usb_facts_come_from_the_matching_sysfs_folder() {
        let t = tempfile::tempdir().unwrap();
        let w = |dir: &str, name: &str, text: &str| {
            let d = t.path().join(dir);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(name), text).unwrap();
        };
        for (dir, bus, dev, vid, pid) in [
            ("1-1", "1", "7", "04a9", "3234"),
            ("2-3", "2", "7", "05ac", "12a8"),
        ] {
            w(dir, "busnum", &format!("{bus}\n"));
            w(dir, "devnum", &format!("{dev}\n"));
            w(dir, "idVendor", &format!("{vid}\n"));
            w(dir, "idProduct", &format!("{pid}\n"));
        }
        w("1-1", "serial", "ABC123\n");
        w("1-1", "manufacturer", "Canon Inc.\n");
        let f = usb_facts(t.path(), 1, 7).unwrap();
        assert_eq!((f.vendor, f.product), (0x04a9, 0x3234));
        assert_eq!(f.serial.as_deref(), Some("ABC123"));
        assert_eq!(f.manufacturer.as_deref(), Some("Canon Inc."));
        assert_eq!(f.product_name, None);
        assert_eq!(usb_facts(t.path(), 2, 7).unwrap().vendor, 0x05ac);
        assert_eq!(usb_facts(t.path(), 3, 7), None);
        assert_eq!(usb_facts(&t.path().join("missing"), 1, 7), None);
    }

    #[test]
    fn storages_are_the_store_folders_or_the_root() {
        let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let r = plan_roots(&names(&["store_00020001", "store_00010001", "DCIM"]));
        assert_eq!(
            r,
            [
                Root {
                    folder: "/store_00010001".into(),
                    name: "Storage 1".into()
                },
                Root {
                    folder: "/store_00020001".into(),
                    name: "Storage 2".into()
                }
            ]
        );
        for none in [
            &["DCIM", "MISC"][..],
            &[][..],
            &["store_1", "store_0001000g"][..],
        ] {
            assert_eq!(
                plan_roots(&names(none)),
                [Root {
                    folder: "/".into(),
                    name: "Camera".into()
                }]
            );
        }
    }

    #[test]
    fn camera_paths_split_and_join() {
        assert_eq!(join_folder("/", "DCIM"), "/DCIM");
        assert_eq!(join_folder("/DCIM", "100CANON"), "/DCIM/100CANON");
        assert_eq!(
            split_object_id("/store_00010001/DCIM/100CANON/IMG_1.JPG"),
            Some(("/store_00010001/DCIM/100CANON", "IMG_1.JPG"))
        );
        assert_eq!(split_object_id("/IMG_1.JPG"), Some(("/", "IMG_1.JPG")));
        for bad in ["", "IMG_1.JPG", "/DCIM/", "/"] {
            assert_eq!(split_object_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn dates_convert_and_absent_ones_are_none() {
        assert_eq!(camera_time(0), None);
        assert_eq!(camera_time(-1), None);
        assert!(camera_time(1_500_000_000).is_some());
    }

    #[test]
    fn a_camera_held_by_the_desktop_is_told_to_close_its_window() {
        let busy =
            open_failure_message(ffi::ERROR_USB_CLAIM, "Could not claim the USB device", true);
        assert!(
            busy.contains("file manager") && busy.contains("unmount"),
            "{busy}"
        );
        let claimed = open_failure_message(
            ffi::ERROR_USB_CLAIM,
            "Could not claim the USB device",
            false,
        );
        assert!(claimed.contains("another program"), "{claimed}");
        let other = open_failure_message(-1, "Unspecified error", false);
        assert!(
            other.contains("PTP") && !other.contains("another program"),
            "{other}"
        );
    }

    /// The bytes libgphoto2 fills in are read at fixed places.
    #[test]
    fn the_declared_structs_match_libgphoto2s_layout() {
        let mut a = Abilities::zeroed();
        a.usb_class = 6;
        a.device_type = 1;
        assert_eq!((a.usb_class(), a.device_type()), (6, 1));
        let mut info = FileInfoBuf::zeroed();
        assert_eq!(
            info.facts(),
            FileFacts {
                size: None,
                mtime: 0
            }
        );
        info.file.fields = INFO_FIELD_SIZE | INFO_FIELD_MTIME;
        info.file.size = 1234;
        info.file.mtime = 1_500_000_000;
        assert_eq!(
            info.facts(),
            FileFacts {
                size: Some(1234),
                mtime: 1_500_000_000
            }
        );
        // The same fields at the byte offsets libgphoto2's header gives them (the const checks
        // above hold the sizes; the live "Directory Browse" test reads real values through them).
        let base = (&raw const info).cast::<u8>();
        // SAFETY: inside the struct.
        unsafe {
            assert_eq!(
                base.add(88).cast::<u32>().read(),
                INFO_FIELD_SIZE | INFO_FIELD_MTIME
            );
            assert_eq!(base.add(96).cast::<u64>().read(), 1234);
            assert_eq!(base.add(184).cast::<i64>().read(), 1_500_000_000);
        }
    }

    /// Without libgphoto2 (or with the test hook set) nothing is listed and the hint says why.
    #[test]
    fn without_libgphoto2_nothing_is_listed() {
        if ffi::available() {
            assert!(hint().is_none());
            return;
        }
        assert!(list().is_empty());
        let h = hint().unwrap();
        assert!(
            h.contains("libgphoto2-6t64") && h.contains("libgphoto2-6 "),
            "{h}"
        );
        let camera = ListedDevice {
            id: device_id(1, 2),
            label: String::new(),
            kind_hint: Some(DeviceKind::Camera),
            transport: Transport::Camera,
        };
        assert!(open(&camera).is_err());
    }

    /// With the library there, but a function missing from it, the library is unavailable as a
    /// whole and nothing is listed: no crash.
    #[test]
    fn a_missing_function_means_no_cameras_not_a_crash() {
        #[allow(dead_code)]
        mod with_a_missing_function {
            crate::platform::linux::dynlib::dyn_api! {
                sonames: &["libgphoto2.so.6", "libgphoto2.so"],
                off: "test",
                api: Api,
                get: api,
                load: load_api;
                pub fn gp_context_new() -> *mut std::ffi::c_void;
                pub fn gp_pnk_no_such_function();
            }
        }
        if ffi::available() {
            let e = with_a_missing_function::load_api(&["libgphoto2.so.6", "libgphoto2.so"])
                .err()
                .expect("a missing function is an error");
            assert!(e.contains("gp_pnk_no_such_function"), "{e}");
            assert!(with_a_missing_function::api().is_none());
        }
    }

    /// With the real library but no camera: libgphoto2's own "Directory Browse" driver serves a
    /// folder as a camera, so listing, details, dates and downloads run through the library
    /// itself. Nothing is plugged in and nothing is written.
    #[test]
    fn a_folder_served_as_a_camera_is_read_whole_and_unchanged() {
        if !ffi::available() {
            return;
        }
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("cam");
        std::fs::create_dir_all(dir.join("DCIM/100TEST")).unwrap();
        let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.join("DCIM/100TEST/BIG.JPG"), &big).unwrap();
        std::fs::write(dir.join("DCIM/100TEST/SMALL.JPG"), b"small").unwrap();
        let when =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        File::options()
            .write(true)
            .open(dir.join("DCIM/100TEST/SMALL.JPG"))
            .unwrap()
            .set_modified(when)
            .unwrap();
        let before = snapshot(&dir);

        let port = format!("disk:{}", dir.display());
        let cam = match open_camera("Directory Browse", &port) {
            Ok(c) => c,
            Err((code, text)) => panic!("could not open the folder as a camera: {code} {text}"),
        };
        let found = candidate("Directory Browse", 1, 1);
        let mut src = CameraSource::start(cam, "cam:1:1", &found);
        assert_eq!(src.identity().view, View::Camera);
        let cancel = AtomicBool::new(false);
        let listing = src.enumerate(&cancel, &mut |_, _| {}).unwrap();
        assert!(listing.issues.is_empty(), "{:?}", listing.issues);
        let names: Vec<String> = listing
            .files
            .iter()
            .map(|f| f.components.join("/"))
            .collect();
        assert_eq!(names, ["DCIM/100TEST/BIG.JPG", "DCIM/100TEST/SMALL.JPG"]);
        assert_eq!(listing.files[0].size, Some(big.len() as u64));
        assert_eq!(listing.files[1].modified, Some(when));
        assert_eq!(listing.files[1].storage.as_deref(), Some("Camera"));

        for f in &listing.files {
            let info = src.object_info(&f.object_id).unwrap();
            assert_eq!(info.size, f.size);
            assert_eq!(info.modified, f.modified);
        }
        // The whole file arrives, bit for bit, through a bounded pipe.
        let mut got = Vec::new();
        src.open_read(&listing.files[0].object_id)
            .unwrap()
            .read_to_end(&mut got)
            .unwrap();
        assert!(got == big, "the download differs from the file");
        // A reader dropped part-way stops the download, and the camera still works after it.
        {
            let mut r = src.open_read(&listing.files[0].object_id).unwrap();
            let mut some = [0u8; 1000];
            r.read_exact(&mut some).unwrap();
        }
        let mut again = Vec::new();
        src.open_read(&listing.files[1].object_id)
            .unwrap()
            .read_to_end(&mut again)
            .unwrap();
        assert_eq!(again, b"small");
        // A file that isn't there is an error, not an empty file.
        let missing = src.open_read("/DCIM/100TEST/NOPE.JPG").and_then(|mut r| {
            let mut v = Vec::new();
            r.read_to_end(&mut v)
                .map_err(|e| DeviceError::Failed(e.to_string()))
        });
        assert!(missing.is_err());
        drop(src);
        assert_eq!(snapshot(&dir), before, "reading changed the folder");
    }

    /// Every file under `dir` with its bytes and modified time, to compare before and after.
    fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>, SystemTime)> {
        let mut out = Vec::new();
        for e in walkdir::WalkDir::new(dir).sort_by_file_name() {
            let e = e.unwrap();
            if e.file_type().is_file() {
                out.push((
                    e.path().display().to_string(),
                    std::fs::read(e.path()).unwrap(),
                    e.metadata().unwrap().modified().unwrap(),
                ));
            }
        }
        out
    }
}
