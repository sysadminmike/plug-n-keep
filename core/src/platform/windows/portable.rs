//! Phones and cameras through Windows Portable Devices (WPD), read-only.
//!
//! How the promise is kept:
//!
//! - the session is requested with `GENERIC_READ` access and the impersonation security level,
//!   and the open fails when either request is refused;
//! - only listing, property reads and read-mode streams are used, and `clippy.toml` turns every
//!   WPD call that could change a device into a build error;
//! - the COM objects never leave this file.
//!
//! The file has four parts: pure decisions (kind, drives, dates, storage folder names), small
//! COM helpers, listing, and the open session with its walk.

use std::collections::HashSet;
use std::io::{self, Read};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use log::{debug, error, info, warn};
use windows::Win32::Devices::PortableDevices::*;
use windows::Win32::Foundation::{GENERIC_READ, RPC_E_CHANGED_MODE, S_FALSE};
use windows::Win32::Storage::FileSystem::SECURITY_IMPERSONATION;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
    CoUninitialize, IStream, STGM_READ,
};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;
use windows::core::{GUID, PCWSTR, PWSTR};

use crate::counted;
use crate::device::{
    DeviceError, DeviceFile, DeviceIdentity, DeviceInfo, DeviceSource, Enumeration, ObjectInfo,
    ScanIssue, storage_folder_names, strip_direction_marks,
};
use crate::paths::MAX_COMPONENTS;
use crate::signals::DeviceKind;

// ---------------------------------------------------------------------------------------------
// Part 1: pure decisions. Nothing here touches COM, so each is tested with sample values.
// ---------------------------------------------------------------------------------------------

/// WPD's name for the root object of every device.
const DEVICE_ROOT: &str = "DEVICE";

/// The most bytes one `read` asks the device for, so a caller that checks for Stop between reads
/// notices it within a fraction of a second.
const READ_CHUNK: usize = 256 * 1024;

/// How many child IDs are asked for at a time.
const ID_BATCH: usize = 32;

/// What kind of device the two device properties describe, if they say. `device_type` is
/// `WPD_DEVICE_TYPE` and `protocol` is `WPD_DEVICE_PROTOCOL` (text such as `MTP: 1.00`). The
/// protocol wins; the type is the fallback. This is only a hint for defaults.
pub fn kind_from_properties(device_type: Option<i32>, protocol: &str) -> Option<DeviceKind> {
    let proto = protocol.trim().to_ascii_lowercase();
    if proto.starts_with("ptp") {
        return Some(DeviceKind::Ptp);
    }
    if proto.starts_with("mtp") {
        return Some(DeviceKind::Mtp);
    }
    match device_type? {
        t if t == WPD_DEVICE_TYPE_CAMERA.0 => Some(DeviceKind::Ptp),
        t if t == WPD_DEVICE_TYPE_PHONE.0
            || t == WPD_DEVICE_TYPE_MEDIA_PLAYER.0
            || t == WPD_DEVICE_TYPE_VIDEO.0
            || t == WPD_DEVICE_TYPE_PERSONAL_INFORMATION_MANAGER.0
            || t == WPD_DEVICE_TYPE_AUDIO_RECORDER.0 =>
        {
            Some(DeviceKind::Mtp)
        }
        _ => None,
    }
}

/// Whether the protocol text says mass storage: WPD's duplicate of a USB drive.
fn protocol_is_mass_storage(protocol: &str) -> bool {
    let p = protocol.trim().to_ascii_lowercase();
    p.starts_with("msc") || p.starts_with("ums") || p.contains("mass storage")
}

/// Whether a Plug and Play ID names USB mass storage (a stick, card or drive).
fn pnp_id_is_usb_storage(pnp_id: &str) -> bool {
    let id = pnp_id.to_ascii_lowercase();
    id.contains("usbstor") || id.contains("uaspstor")
}

/// Whether WPD's entry is only the duplicate of a USB drive. Those are read through the file
/// system, so they are not offered as phones. `protocol` is known only after an open; the
/// listing passes `None` and relies on the ID.
pub fn is_usb_drive_duplicate(pnp_id: &str, protocol: Option<&str>) -> bool {
    pnp_id_is_usb_storage(pnp_id) || protocol.is_some_and(protocol_is_mass_storage)
}

/// The kind the listing alone can suggest. Anything else is decided after an open.
pub fn kind_hint_from_listing(d: &DeviceInfo) -> Option<DeviceKind> {
    let text = format!("{} {}", d.friendly_name, d.description).to_lowercase();
    if d.looks_like_apple() || text.contains("camera") {
        Some(DeviceKind::Ptp)
    } else {
        None
    }
}

/// Days from the OLE date origin (1899-12-30) to 1970-01-01.
const OLE_DAYS_TO_UNIX: f64 = 25_569.0;
/// The OLE date range: 0100-01-01 to 9999-12-31.
const OLE_MIN: f64 = -657_434.0;
const OLE_MAX: f64 = 2_958_466.0;

/// Convert a `VT_DATE` value (days since 1899-12-30, the time of day in the fraction) to a
/// time, rounded to the second. Devices report their clock without a zone, so it is read as
/// UTC. Returns `None` for no value (0), values that are not numbers, and dates outside the
/// OLE range or before the system clock's start (1601). Before 1899-12-30 the fraction still counts forwards from midnight: -1.25 is
/// 06:00 on 1899-12-29.
pub fn ole_date_to_system_time(ole: f64) -> Option<SystemTime> {
    if !ole.is_finite() || ole == 0.0 || !(OLE_MIN..OLE_MAX).contains(&ole) {
        return None;
    }
    let whole = ole.trunc();
    let fraction = (ole - whole).abs();
    let secs = ((whole - OLE_DAYS_TO_UNIX) * 86_400.0 + fraction * 86_400.0).round() as i64;
    if secs >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(secs as u64))
    } else {
        UNIX_EPOCH.checked_sub(Duration::from_secs(secs.unsigned_abs()))
    }
}

/// The message for a device that shows no storage.
const NO_STORAGE_MESSAGE: &str = "This device shows no storage, so there are no files to back up. \
    On an Android phone, unlock it and set the USB mode to \"File transfer\" (not \"No data \
    transfer\" or charging only), then try again. An iPhone must be unlocked and must trust \
    this PC.";

// ---------------------------------------------------------------------------------------------
// Part 2: COM helpers.
// ---------------------------------------------------------------------------------------------

/// COM initialised (multithreaded) on the current thread. Not `Send`: it must be dropped on the
/// thread that created it. Every WPD entry point takes `&ComGuard`, so COM can't be used before
/// it is set up.
pub struct ComGuard {
    balance: bool,
    _same_thread: PhantomData<*const ()>,
}

impl ComGuard {
    pub fn init() -> Result<Self, String> {
        // SAFETY: plain per-thread COM setup; undone in Drop when it succeeded.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let balance = if hr.is_ok() {
            // S_OK and S_FALSE both need a matching CoUninitialize.
            true
        } else if hr == RPC_E_CHANGED_MODE {
            // The thread already runs single-threaded COM. WPD works there too; our call did
            // not initialise anything, so there is nothing to undo.
            debug!("COM already runs single-threaded on this thread; using it");
            false
        } else {
            return Err(format!(
                "Could not initialise COM: {}",
                windows::core::Error::from(hr)
            ));
        };
        Ok(Self {
            balance,
            _same_thread: PhantomData,
        })
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.balance {
            // SAFETY: pairs with the successful CoInitializeEx on this thread (`ComGuard` is
            // not `Send`).
            unsafe { CoUninitialize() };
        }
    }
}

/// A COM error as the user should read it: a lost connection in plain words, any other error as
/// Windows words it. The code stays in brackets at the end; the log has the raw text too.
fn com_text(e: &windows::core::Error) -> String {
    crate::device::plain_error(&e.to_string())
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A string Windows allocated for us. It is freed when this goes out of scope, so no path
/// through the code can leak one.
struct CoString(PWSTR);

impl CoString {
    fn text(&self) -> Option<String> {
        if self.0.is_null() {
            return None;
        }
        // SAFETY: a non-null PWSTR from WPD is NUL-terminated.
        unsafe { self.0.to_string() }.ok()
    }
}

impl Drop for CoString {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: WPD allocates these with CoTaskMemAlloc; we are the only owner.
            unsafe { CoTaskMemFree(Some(self.0.0 as *const _)) };
        }
    }
}

/// `VT_DATE` in the PROPVARIANT type tag.
const VT_DATE_CODE: u16 = 7;

/// A set of property values read from one object, with typed getters. A missing or
/// differently-typed value reads as `None`.
struct Bag(IPortableDeviceValues);

impl Bag {
    fn text(&self, key: &PROPERTYKEY) -> Option<String> {
        // SAFETY: the returned string is owned by us and freed by `CoString`.
        let s = CoString(unsafe { self.0.GetStringValue(key) }.ok()?);
        s.text()
    }

    /// A name or description: direction marks removed, ends trimmed, empty means missing.
    fn label(&self, key: &PROPERTYKEY) -> Option<String> {
        let t = self.text(key)?;
        let t = strip_direction_marks(&t).trim().to_string();
        (!t.is_empty()).then_some(t)
    }

    fn number(&self, key: &PROPERTYKEY) -> Option<u32> {
        // SAFETY: a plain value read.
        unsafe { self.0.GetUnsignedIntegerValue(key) }.ok()
    }

    fn big(&self, key: &PROPERTYKEY) -> Option<u64> {
        // SAFETY: a plain value read.
        unsafe { self.0.GetUnsignedLargeIntegerValue(key) }.ok()
    }

    fn guid(&self, key: &PROPERTYKEY) -> Option<GUID> {
        // SAFETY: a plain value read.
        unsafe { self.0.GetGuidValue(key) }.ok()
    }

    /// A `VT_DATE` value as a time.
    fn date(&self, key: &PROPERTYKEY) -> Option<SystemTime> {
        // SAFETY: a plain value read; the PROPVARIANT clears itself when dropped.
        let pv = unsafe { self.0.GetValue(key) }.ok()?;
        let raw = pv.as_raw();
        // SAFETY: `vt` is valid in every PROPVARIANT, and the date member is read only when
        // `vt` says the value is a date.
        let ole = unsafe {
            let inner = &raw.Anonymous.Anonymous;
            if inner.vt != VT_DATE_CODE {
                return None;
            }
            inner.Anonymous.date
        };
        ole_date_to_system_time(ole)
    }
}

/// Adds the IDs that can be used (not empty, and decoded) to `ids`, and returns how many could
/// not be: each one is an object that can't be read, so its folder is not completely listed.
fn keep_readable_ids(raw: impl Iterator<Item = Option<String>>, ids: &mut Vec<String>) -> usize {
    let mut bad = 0;
    for id in raw {
        match id {
            Some(s) if !s.is_empty() => ids.push(s),
            _ => bad += 1,
        }
    }
    bad
}

/// The child IDs of an object, plus the reason listing stopped early, if it did.
struct Children {
    ids: Vec<String>,
    stopped: Option<String>,
    /// How many children came back with an empty ID, or one that could not be decoded. They
    /// can't be read, so the folder is not completely listed.
    bad: usize,
}

// ---------------------------------------------------------------------------------------------
// Part 3: listing. This reads Windows' own device list and does not open any device.
// ---------------------------------------------------------------------------------------------

/// The portable devices that are plugged in, minus WPD's duplicates of USB drives.
pub fn list_devices(_com: &ComGuard) -> Result<Vec<DeviceInfo>, String> {
    // SAFETY: COM is initialised on this thread (`_com`); every out-pointer is a live local.
    unsafe {
        let manager: IPortableDeviceManager =
            CoCreateInstance(&PortableDeviceManager, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| format!("Could not create the portable device manager: {e}"))?;
        // Pick up anything plugged in since the manager was first used.
        let _ = manager.RefreshDeviceList();

        let owned = listed_ids(&manager)?;
        if owned.is_empty() {
            debug!("Windows lists no portable devices");
            return Ok(Vec::new());
        }

        let mut out = Vec::new();
        for entry in &owned {
            let Some(pnp_id) = entry.text() else { continue };
            if is_usb_drive_duplicate(&pnp_id, None) {
                debug!("Not listing {pnp_id}: a USB drive, read through the file system");
                continue;
            }
            let w = wide(&pnp_id);
            let id = PCWSTR(w.as_ptr());
            let friendly_name = manager_text(|b, n| manager.GetDeviceFriendlyName(id, b, n));
            let manufacturer = manager_text(|b, n| manager.GetDeviceManufacturer(id, b, n));
            let description = manager_text(|b, n| manager.GetDeviceDescription(id, b, n));
            debug!("Device \"{friendly_name}\" by \"{manufacturer}\" ({description}), ID {pnp_id}");
            out.push(DeviceInfo {
                pnp_id,
                friendly_name,
                manufacturer,
                description,
            });
        }
        debug!(
            "Windows lists {}",
            counted(out.len(), "portable device", "portable devices")
        );
        Ok(out)
    }
}

/// The IDs of the devices the manager lists. Each is owned as soon as it is returned, so none
/// leaks on any path.
fn listed_ids(manager: &IPortableDeviceManager) -> Result<Vec<CoString>, String> {
    let mut wanted: u32 = 0;
    // SAFETY: a null array with a count asks only for the number of devices.
    unsafe { manager.GetDevices(std::ptr::null_mut(), &mut wanted) }
        .map_err(|e| format!("Could not count devices: {e}"))?;
    if wanted == 0 {
        return Ok(Vec::new());
    }
    let mut slots = vec![PWSTR::null(); wanted as usize];
    // SAFETY: `slots` has room for `wanted` pointers, and `wanted` is a live local.
    let outcome = unsafe { manager.GetDevices(slots.as_mut_ptr(), &mut wanted) };
    let ids: Vec<CoString> = slots
        .drain(..)
        .take(wanted as usize)
        .map(CoString)
        .collect();
    outcome.map_err(|e| format!("Could not list devices: {e}"))?;
    Ok(ids)
}

/// The manager's string getters, which are called twice: once for the size, once for the text.
fn manager_text<F>(mut ask: F) -> String
where
    F: FnMut(PWSTR, *mut u32) -> windows::core::Result<()>,
{
    let mut needed: u32 = 0;
    let sized = ask(PWSTR::null(), &mut needed);
    if sized.is_err() || needed == 0 {
        return String::new();
    }
    let mut units = vec![0u16; needed as usize];
    if ask(PWSTR(units.as_mut_ptr()), &mut needed).is_err() {
        return String::new();
    }
    let text = units.split(|&u| u == 0).next().unwrap_or_default();
    strip_direction_marks(&String::from_utf16_lossy(text))
        .trim()
        .to_owned()
}

// ---------------------------------------------------------------------------------------------
// Part 4: the open session and the walk.
// ---------------------------------------------------------------------------------------------

/// An open WPD session; dropping it closes the session, so every early return is covered.
struct Session(IPortableDevice);

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: closes the session that `open_session` opened.
        let _ = unsafe { self.0.Close() };
    }
}

/// One storage of the device.
struct Storage {
    object_id: String,
    /// What the device calls it, made safe to be a folder name and different from the others'.
    /// This is what files report as their storage; the plan decides whether it becomes a folder.
    name: String,
}

/// What one object's properties say.
struct ObjectProps {
    parent_id: Option<String>,
    is_container: bool,
    name: String,
    size: Option<u64>,
    persistent_id: Option<String>,
    modified: Option<SystemTime>,
}

/// An open, read-only session with one device.
pub struct WpdSource {
    content: IPortableDeviceContent,
    properties: IPortableDeviceProperties,
    transfer: IPortableDeviceResources,
    identity: DeviceIdentity,
    storages: Vec<Storage>,
    /// Problems found while opening, reported again by the first walk.
    open_issues: Vec<ScanIssue>,
    // Last, so the session closes after the interfaces above are released.
    _session: Session,
}

/// Create the device object (the free-threaded one when there is one) and open it read-only.
fn open_session(pnp_id: &str) -> Result<Session, String> {
    // SAFETY: COM is initialised by the caller's `ComGuard`; pointers are to live locals.
    unsafe {
        let client: IPortableDeviceValues =
            CoCreateInstance(&PortableDeviceValues, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| format!("Could not create the client information: {e}"))?;
        let device: IPortableDevice =
            match CoCreateInstance(&PortableDeviceFTM, None, CLSCTX_INPROC_SERVER) {
                Ok(free_threaded) => free_threaded,
                Err(_) => CoCreateInstance(&PortableDevice, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| format!("Could not create a device object: {e}"))?,
            };

        // The two requests that make the session read-only. If either is refused, stop.
        client
            .SetUnsignedIntegerValue(&WPD_CLIENT_DESIRED_ACCESS, GENERIC_READ.0)
            .map_err(|e| {
                error!("Not opening {pnp_id}: read-only access could not be requested: {e}");
                format!("Refusing to open the device: read-only access could not be requested: {e}")
            })?;
        client
            .SetUnsignedIntegerValue(
                &WPD_CLIENT_SECURITY_QUALITY_OF_SERVICE,
                SECURITY_IMPERSONATION.0,
            )
            .map_err(|e| {
                error!("Not opening {pnp_id}: the security level could not be set: {e}");
                format!("Refusing to open the device: the security level could not be set: {e}")
            })?;
        // Who is asking; a failure here changes nothing.
        let app = wide("Plug-n-Keep");
        let _ = client.SetStringValue(&WPD_CLIENT_NAME, PCWSTR(app.as_ptr()));
        let _ = client.SetUnsignedIntegerValue(&WPD_CLIENT_MAJOR_VERSION, 0);
        let _ = client.SetUnsignedIntegerValue(&WPD_CLIENT_MINOR_VERSION, 5);
        let _ = client.SetUnsignedIntegerValue(&WPD_CLIENT_REVISION, 0);

        let id = wide(pnp_id);
        device.Open(PCWSTR(id.as_ptr()), &client).map_err(|e| {
            format!(
                "Could not open the device: {e}. Unlock the phone. An iPhone must also trust this PC;                  an Android phone must be set to File transfer mode."
            )
        })?;
        Ok(Session(device))
    }
}

impl WpdSource {
    /// Open the device read-only and find its storages. Fails rather than fall back to
    /// anything that could write, and fails clearly for a device that shows no storage.
    pub fn open(_com: &ComGuard, pnp_id: &str) -> Result<Self, String> {
        let session = open_session(pnp_id)?;
        // SAFETY: plain interface lookups on the open device.
        let content = unsafe { session.0.Content() }
            .map_err(|e| format!("Could not read the device's contents: {e}"))?;
        // SAFETY: as above.
        let properties = unsafe { content.Properties() }
            .map_err(|e| format!("Could not read the device's properties: {e}"))?;
        // SAFETY: as above.
        let transfer = unsafe { content.Transfer() }
            .map_err(|e| format!("Could not open the device's transfers: {e}"))?;

        // The device's own description.
        // SAFETY: a plain property read.
        let device_bag = unsafe { properties.GetValues(WPD_DEVICE_OBJECT_ID, None) }
            .map(Bag)
            .map_err(|e| warn!("Could not read the device's name, kind and serial number: {e}"))
            .ok();
        let text = |key: &PROPERTYKEY| {
            device_bag
                .as_ref()
                .and_then(|b| b.label(key))
                .unwrap_or_default()
        };
        let protocol = text(&WPD_DEVICE_PROTOCOL);
        if is_usb_drive_duplicate(pnp_id, Some(&protocol)) {
            return Err(
                "This is a USB drive. It is read through the file system, not as a \
                        phone or camera."
                    .into(),
            );
        }
        let device_type = device_bag
            .as_ref()
            .and_then(|b| b.number(&WPD_DEVICE_TYPE))
            .and_then(|t| i32::try_from(t).ok());
        let serial = text(&WPD_DEVICE_SERIAL_NUMBER);
        let mut identity = DeviceIdentity::new(
            pnp_id,
            text(&WPD_DEVICE_FRIENDLY_NAME),
            text(&WPD_DEVICE_MANUFACTURER),
            text(&WPD_DEVICE_MODEL),
            (!serial.is_empty()).then_some(serial),
        );
        identity.kind_hint = kind_from_properties(device_type, &protocol);

        let mut source = WpdSource {
            content,
            properties,
            transfer,
            identity,
            storages: Vec::new(),
            open_issues: Vec::new(),
            _session: session,
        };
        source.find_storages();
        if source.storages.is_empty() {
            warn!(
                "\"{}\" shows no storage (protocol \"{protocol}\")",
                source.identity.friendly_name
            );
            return Err(NO_STORAGE_MESSAGE.into());
        }
        info!(
            "Opened \"{}\" read-only: {} {}, serial number {}, {}",
            source.identity.friendly_name,
            source.identity.manufacturer,
            source.identity.model,
            source.identity.serial.as_deref().unwrap_or("not reported"),
            counted(source.storages.len(), "storage", "storages")
        );
        debug!(
            "Device key {} (skip lists and the backup folder's owner use it)",
            source.identity.key
        );
        Ok(source)
    }

    /// List the storage objects under the device root, name them, and record their capacity.
    fn find_storages(&mut self) {
        let children = match self.children(DEVICE_ROOT) {
            Ok(c) => c,
            Err(e) => {
                self.note_open_issue(format!("could not list the device's storages: {e}"));
                return;
            }
        };
        if let Some(e) = children.stopped {
            self.note_open_issue(format!("the list of storages stopped part-way: {e}"));
        }
        let mut found: Vec<(String, String, Option<u64>)> = Vec::new();
        for id in children.ids {
            let bag = match self.bag(&id) {
                Ok(b) => b,
                Err(e) => {
                    self.note_open_issue(format!(
                        "could not read object {id} under the device: {e}"
                    ));
                    continue;
                }
            };
            let is_storage = bag.guid(&WPD_OBJECT_CONTENT_TYPE)
                == Some(WPD_CONTENT_TYPE_FUNCTIONAL_OBJECT)
                && bag.guid(&WPD_FUNCTIONAL_OBJECT_CATEGORY)
                    == Some(WPD_FUNCTIONAL_CATEGORY_STORAGE);
            if !is_storage {
                continue;
            }
            let name = bag
                .label(&WPD_STORAGE_DESCRIPTION)
                .or_else(|| bag.label(&WPD_OBJECT_NAME))
                .unwrap_or_default();
            found.push((id, name, bag.big(&WPD_STORAGE_CAPACITY)));
        }

        let names: Vec<String> = found.iter().map(|(_, n, _)| n.clone()).collect();
        let folders = storage_folder_names(&names);
        let signals = &mut self.identity.signals;
        let mut total: u64 = 0;
        for (i, (object_id, _, capacity)) in found.into_iter().enumerate() {
            total = total.saturating_add(capacity.unwrap_or(0));
            // The cleaned name in both places, the same as on the other systems.
            signals.storage_names.push(folders[i].clone());
            self.storages.push(Storage {
                object_id,
                name: folders[i].clone(),
            });
        }
        signals.capacity = (total > 0).then_some(total);
    }

    fn note_open_issue(&mut self, problem: String) {
        warn!("Device: {problem}");
        self.open_issues.push(ScanIssue::new("(device)", problem));
    }

    /// All child IDs of `parent`, fetched in batches. Every string WPD returns is freed.
    fn children(&self, parent: &str) -> Result<Children, String> {
        let w = wide(parent);
        // SAFETY: `w` outlives the call.
        let enumerator = unsafe { self.content.EnumObjects(0, PCWSTR(w.as_ptr()), None) }
            .map_err(|e| com_text(&e))?;
        let mut ids = Vec::new();
        let mut bad = 0usize;
        loop {
            let mut slots = [PWSTR::null(); ID_BATCH];
            let mut fetched: u32 = 0;
            // SAFETY: `slots` and `fetched` are live locals of the sizes passed.
            let hr = unsafe { enumerator.Next(&mut slots, &mut fetched) };
            let fetched = (fetched as usize).min(slots.len());
            // Wrapping first frees the strings whatever happens next.
            let batch: Vec<CoString> = slots[..fetched].iter().map(|p| CoString(*p)).collect();
            bad += keep_readable_ids(batch.iter().map(CoString::text), &mut ids);
            if hr.is_err() {
                let stopped = Some(com_text(&windows::core::Error::from(hr)));
                return Ok(Children { ids, stopped, bad });
            }
            if fetched == 0 || hr == S_FALSE {
                return Ok(Children {
                    ids,
                    stopped: None,
                    bad,
                });
            }
        }
    }

    fn bag(&self, object_id: &str) -> Result<Bag, String> {
        let w = wide(object_id);
        // SAFETY: `w` outlives the call.
        unsafe { self.properties.GetValues(PCWSTR(w.as_ptr()), None) }
            .map(Bag)
            .map_err(|e| com_text(&e))
    }

    fn object(&self, object_id: &str) -> Result<ObjectProps, String> {
        let bag = self.bag(object_id)?;
        let content_type = bag.guid(&WPD_OBJECT_CONTENT_TYPE).unwrap_or_default();
        // The original file name is what the user sees; the object name is the fallback.
        let name = bag
            .text(&WPD_OBJECT_ORIGINAL_FILE_NAME)
            .filter(|s| !s.is_empty())
            .or_else(|| bag.text(&WPD_OBJECT_NAME))
            .map(|s| strip_direction_marks(&s))
            .unwrap_or_default();
        Ok(ObjectProps {
            parent_id: bag.text(&WPD_OBJECT_PARENT_ID).filter(|s| !s.is_empty()),
            is_container: content_type == WPD_CONTENT_TYPE_FOLDER
                || content_type == WPD_CONTENT_TYPE_FUNCTIONAL_OBJECT,
            name,
            size: bag.big(&WPD_OBJECT_SIZE),
            persistent_id: bag
                .text(&WPD_OBJECT_PERSISTENT_UNIQUE_ID)
                .filter(|s| !s.is_empty()),
            modified: bag.date(&WPD_OBJECT_DATE_MODIFIED),
        })
    }

    /// The name of the storage that holds an object, found by following its parents up.
    fn storage_name_of(&self, first_parent: Option<String>) -> Option<String> {
        let mut at = first_parent;
        for _ in 0..=MAX_COMPONENTS + 1 {
            let id = at?;
            if let Some(s) = self.storages.iter().find(|s| s.object_id == id) {
                return Some(s.name.clone());
            }
            at = self.object(&id).ok().and_then(|p| p.parent_id);
        }
        None
    }
}

/// Note a problem found while walking, in the log and in the result.
fn report(result: &mut Enumeration, location: &str, problem: impl Into<String>) {
    let issue = ScanIssue::new(location, problem);
    warn!("Device: {}: {}", issue.location, issue.problem);
    result.issues.push(issue);
}

/// A folder still to be listed.
struct Pending {
    id: String,
    path: Vec<String>,
    storage: usize,
}

impl DeviceSource for WpdSource {
    fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    /// Walks every storage with a work list (no recursion). A set of seen IDs and a depth limit
    /// of [`MAX_COMPONENTS`] mean a looping or very deep tree from a bad device can't run
    /// forever or overflow the stack.
    fn enumerate(
        &mut self,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(usize, &str),
    ) -> Result<Enumeration, DeviceError> {
        let mut result = Enumeration::default();
        result.issues.append(&mut self.open_issues);
        let mut seen: HashSet<String> = HashSet::new();
        let mut work: Vec<Pending> = Vec::new();
        for (i, s) in self.storages.iter().enumerate().rev() {
            seen.insert(s.object_id.clone());
            work.push(Pending {
                id: s.object_id.clone(),
                path: Vec::new(),
                storage: i,
            });
        }
        let mut folders = 0usize;

        while let Some(folder) = work.pop() {
            if cancel.load(Ordering::Relaxed) {
                return Err(DeviceError::Cancelled);
            }
            let storage_name = &self.storages[folder.storage].name;
            let key = folder.path.join("/");
            let here = if key.is_empty() {
                storage_name.clone()
            } else {
                key.clone()
            };
            progress(result.files.len(), &here);

            let listing = match self.children(&folder.id) {
                Ok(l) => l,
                Err(e) => {
                    report(
                        &mut result,
                        &here,
                        format!("could not list this folder: {e}"),
                    );
                    result.incomplete.push(key);
                    continue;
                }
            };
            folders += 1;
            debug!(
                "Listed {here}: {}",
                counted(listing.ids.len(), "item", "items")
            );
            if listing.ids.is_empty()
                && listing.stopped.is_none()
                && listing.bad == 0
                && !key.is_empty()
            {
                result.empty_folders.push(key.clone());
            }
            if listing.bad > 0 {
                report(
                    &mut result,
                    &here,
                    format!(
                        "{} with an ID that could not be read; left out",
                        counted(listing.bad, "item", "items")
                    ),
                );
                result.incomplete.push(key.clone());
            }
            if let Some(e) = listing.stopped {
                report(&mut result, &here, format!("listing stopped part-way: {e}"));
                result.incomplete.push(key.clone());
            }

            for id in listing.ids {
                if cancel.load(Ordering::Relaxed) {
                    return Err(DeviceError::Cancelled);
                }
                if !seen.insert(id.clone()) {
                    report(
                        &mut result,
                        &here,
                        format!("object {id} appears twice; ignored the repeat"),
                    );
                    // Which of the two is the real one is unknown, so a file that is missing
                    // from the listing must not count as deleted.
                    result.incomplete.push(key.clone());
                    continue;
                }
                let props = match self.object(&id) {
                    Ok(p) => p,
                    Err(e) => {
                        report(
                            &mut result,
                            &here,
                            format!("could not read object {id}: {e}"),
                        );
                        result.incomplete.push(key.clone());
                        continue;
                    }
                };
                if props.name.is_empty() {
                    report(&mut result, &here, format!("object {id} has no name"));
                    result.incomplete.push(key.clone());
                    continue;
                }
                let mut path = folder.path.clone();
                path.push(props.name);
                if props.is_container {
                    if path.len() >= MAX_COMPONENTS {
                        report(
                            &mut result,
                            &here,
                            "folders nested too deeply; not scanned further",
                        );
                        result.incomplete.push(path.join("/"));
                    } else {
                        work.push(Pending {
                            id,
                            path,
                            storage: folder.storage,
                        });
                    }
                } else {
                    result.files.push(DeviceFile {
                        object_id: id,
                        persistent_id: props.persistent_id,
                        components: path,
                        size: props.size,
                        modified: props.modified,
                        storage: Some(self.storages[folder.storage].name.clone()),
                    });
                }
            }
        }
        result.files.sort_by(|a, b| a.components.cmp(&b.components));
        debug!(
            "Walked the device: {} in {}",
            counted(result.files.len(), "file", "files"),
            counted(folders, "folder", "folders")
        );
        progress(result.files.len(), "");
        Ok(result)
    }

    fn object_info(&mut self, object_id: &str) -> Result<ObjectInfo, DeviceError> {
        let p = self.object(object_id).map_err(|e| {
            warn!("Could not re-read object {object_id} on the device: {e}");
            DeviceError::Failed(e)
        })?;
        let storage = self.storage_name_of(p.parent_id);
        Ok(ObjectInfo {
            name: p.name,
            size: p.size,
            persistent_id: p.persistent_id,
            modified: p.modified,
            storage,
        })
    }

    fn open_read(&mut self, object_id: &str) -> Result<Box<dyn Read + '_>, DeviceError> {
        let name = wide(object_id);
        let mut opened: Option<IStream> = None;
        let mut preferred_chunk: u32 = 0;
        // SAFETY: every pointer is to a live local. STGM_READ asks for a read-only stream.
        let status = unsafe {
            self.transfer.GetStream(
                PCWSTR(name.as_ptr()),
                &WPD_RESOURCE_DEFAULT,
                STGM_READ.0,
                &mut preferred_chunk,
                &mut opened,
            )
        };
        if let Err(e) = status {
            warn!("Could not open object {object_id} for reading: {e}");
            return Err(DeviceError::Failed(format!(
                "could not open the file on the device: {}",
                com_text(&e)
            )));
        }
        let stream = opened.ok_or_else(|| {
            warn!("The device gave no stream for object {object_id}");
            DeviceError::Failed("the device returned no stream".into())
        })?;
        Ok(Box::new(FileReader {
            stream,
            object_id: object_id.to_string(),
        }))
    }
}

/// A file's contents, read in chunks. The caller checks for Stop between reads; one read never
/// asks the device for more than [`READ_CHUNK`] bytes.
struct FileReader {
    stream: IStream,
    /// For the log.
    object_id: String,
}

impl Read for FileReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let want = buf.len().min(READ_CHUNK);
        if want == 0 {
            return Ok(0);
        }
        let mut got: u32 = 0;
        // SAFETY: `buf` is valid for `want` bytes, and `want` is at most READ_CHUNK, so it
        // fits in a u32.
        let hr = unsafe {
            self.stream
                .Read(buf.as_mut_ptr().cast(), want as u32, Some(&mut got))
        };
        if hr.is_err() {
            let e = windows::core::Error::from(hr);
            warn!("Reading object {} failed: {e}", self.object_id);
            return Err(io::Error::other(format!(
                "device read failed: {}",
                com_text(&e)
            )));
        }
        Ok((got as usize).min(want))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(t: SystemTime) -> i64 {
        match t.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_secs() as i64,
            Err(e) => -(e.duration().as_secs() as i64),
        }
    }

    #[test]
    fn the_protocol_decides_the_kind_and_the_type_is_the_fallback() {
        let t = |x: WPD_DEVICE_TYPES| Some(x.0);
        assert_eq!(
            kind_from_properties(None, "PTP: 1.00"),
            Some(DeviceKind::Ptp)
        );
        assert_eq!(
            kind_from_properties(None, " mtp: 1.00 "),
            Some(DeviceKind::Mtp)
        );
        // The protocol beats the type.
        assert_eq!(
            kind_from_properties(t(WPD_DEVICE_TYPE_PHONE), "PTP"),
            Some(DeviceKind::Ptp)
        );
        assert_eq!(
            kind_from_properties(t(WPD_DEVICE_TYPE_CAMERA), ""),
            Some(DeviceKind::Ptp)
        );
        assert_eq!(
            kind_from_properties(t(WPD_DEVICE_TYPE_PHONE), ""),
            Some(DeviceKind::Mtp)
        );
        assert_eq!(
            kind_from_properties(t(WPD_DEVICE_TYPE_MEDIA_PLAYER), "other"),
            Some(DeviceKind::Mtp)
        );
        assert_eq!(kind_from_properties(t(WPD_DEVICE_TYPE_GENERIC), ""), None);
        assert_eq!(kind_from_properties(None, ""), None);
        assert_eq!(kind_from_properties(Some(99), "x"), None);
    }

    #[test]
    fn a_usb_drive_is_recognised_by_protocol_or_by_id() {
        assert!(is_usb_drive_duplicate(
            r"\\?\usbstor#disk&ven_sandisk&prod_cruzer#0401abc&0#{53f56307-b6bf-11d0-94f2-00a0c91efb8b}",
            None
        ));
        assert!(is_usb_drive_duplicate(
            r"\\?\SWD#WPDBUSENUM#_??_USBSTOR#DISK&VEN_X#7&1",
            None
        ));
        assert!(is_usb_drive_duplicate(
            r"\\?\scsi#disk&ven_x#uaspstor#1",
            None
        ));
        assert!(is_usb_drive_duplicate("x", Some("MSC: 1.0")));
        assert!(is_usb_drive_duplicate("x", Some("USB Mass Storage")));
        // Phones and cameras stay.
        assert!(!is_usb_drive_duplicate(
            r"\\?\usb#vid_05ac&pid_12a8#00008030000a1234#{6ac27878-a6fa-4155-ba85-f98f491d4f33}",
            Some("PTP: 1.00")
        ));
        assert!(!is_usb_drive_duplicate(
            r"\\?\usb#vid_18d1&pid_4ee1#abc#{6ac27878}",
            Some("MTP: 1.00")
        ));
        assert!(!is_usb_drive_duplicate("x", None));
    }

    #[test]
    fn the_listing_suggests_ptp_for_apple_and_cameras_only() {
        let info = |friendly: &str, maker: &str, desc: &str| DeviceInfo {
            pnp_id: "x".into(),
            friendly_name: friendly.into(),
            manufacturer: maker.into(),
            description: desc.into(),
        };
        assert_eq!(
            kind_hint_from_listing(&info("Apple iPhone", "Apple Inc.", "")),
            Some(DeviceKind::Ptp)
        );
        assert_eq!(
            kind_hint_from_listing(&info("EOS", "Canon", "Digital Camera")),
            Some(DeviceKind::Ptp)
        );
        assert_eq!(
            kind_hint_from_listing(&info("Pixel 8", "Google", "MTP Device")),
            None
        );
    }

    #[test]
    fn ole_dates_become_times() {
        assert_eq!(secs(ole_date_to_system_time(25569.5).unwrap()), 12 * 3600);
        // 2000-01-01 00:00:00 is OLE 36526 and Unix 946684800.
        assert_eq!(secs(ole_date_to_system_time(36526.0).unwrap()), 946_684_800);
        // 2024-02-29 18:30:00: Unix 1709231400.
        let t = ole_date_to_system_time(45351.0 + (18.0 * 3600.0 + 1800.0) / 86400.0).unwrap();
        assert_eq!(secs(t), 1_709_231_400);
    }

    #[test]
    fn ole_dates_before_the_unix_clock_and_before_1899_work() {
        // 1969-12-31 00:00:00 is OLE 25568.
        assert_eq!(secs(ole_date_to_system_time(25568.0).unwrap()), -86_400);
        // 1899-12-30 06:00 is OLE 0.25; the odd negative form: -1.25 is 06:00 on 12-29.
        assert_eq!(
            secs(ole_date_to_system_time(0.25).unwrap()),
            -25_569 * 86_400 + 6 * 3600
        );
        assert_eq!(
            secs(ole_date_to_system_time(-1.25).unwrap()),
            -25_570 * 86_400 + 6 * 3600
        );
    }

    #[test]
    fn bad_ole_dates_give_nothing() {
        for v in [
            0.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            -700_000.0,
            3_000_000.0,
            1e300,
        ] {
            assert_eq!(ole_date_to_system_time(v), None, "{v}");
        }
        // The ends of the range: a time the system clock can't hold (before 1601) gives nothing.
        assert_eq!(ole_date_to_system_time(-657_433.5), None);
        assert!(ole_date_to_system_time(-109_000.0).is_some()); // year 1601
        assert!(ole_date_to_system_time(2_958_465.99).is_some());
    }

    #[test]
    fn float_noise_in_a_date_does_not_change_the_second() {
        let exact = 45351.0 + 66_600.0 / 86_400.0;
        let noisy = exact + 1e-9; // about 0.0001 s
        assert_eq!(
            ole_date_to_system_time(exact),
            ole_date_to_system_time(noisy)
        );
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_storage_keeps_its_own_name_and_the_source_adds_no_folder_level() {
        // The source reports the storage's name; whether it becomes a folder is the plan's call.
        let one = storage_folder_names(&names(&["Internal Storage"]));
        assert_eq!(one, ["Internal Storage"]);
        let two = storage_folder_names(&names(&["Internal", "SD card"]));
        assert_eq!(two, ["Internal", "SD card"]);
    }

    /// DESIGN.md 10.3: the session asks only for read access and streams open read-only.
    #[test]
    fn the_session_asks_only_for_read_access_and_streams_open_read_only() {
        let src = include_str!("portable.rs");
        let production = &src[..src.find("#[cfg(test)]").unwrap()];
        // Every request for access names GENERIC_READ, and no other access right appears.
        let key = "WPD_CLIENT_DESIRED_ACCESS";
        let uses: Vec<usize> = production.match_indices(key).map(|(i, _)| i).collect();
        assert_eq!(uses.len(), 1, "one request for access");
        let statement = &production[uses[0]..uses[0] + production[uses[0]..].find(';').unwrap()];
        assert!(statement.contains("GENERIC_READ.0"), "{statement}");
        for other in ["GENERIC_WRITE", "GENERIC_ALL", "GENERIC_EXECUTE"] {
            assert!(!production.contains(other), "{other}");
        }
        // Every storage mode named is STGM_READ.
        for (i, _) in production.match_indices("STGM_") {
            let word: String = production[i..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            assert_eq!(word, "STGM_READ");
        }
        // And the one stream opened is opened with it.
        let calls: Vec<&str> = production
            .match_indices(".GetStream(")
            .map(|(i, _)| &production[i..i + production[i..].find("&mut opened").unwrap()])
            .collect();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(calls[0].contains("STGM_READ.0"), "{}", calls[0]);
    }

    /// An ID that is empty or can't be decoded is counted, not dropped without a word.
    #[test]
    fn ids_that_cannot_be_read_are_counted() {
        let mut ids = Vec::new();
        let raw = [
            Some("o1".to_string()),
            None,
            Some(String::new()),
            Some("o2".into()),
        ];
        assert_eq!(keep_readable_ids(raw.into_iter(), &mut ids), 2);
        assert_eq!(ids, ["o1", "o2"]);
    }

    #[test]
    fn the_no_storage_message_tells_what_to_do() {
        assert!(NO_STORAGE_MESSAGE.contains("File transfer"));
    }
}
