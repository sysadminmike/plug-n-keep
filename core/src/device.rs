//! What the rest of the app knows about a device: a read-only source of files.
//!
//! The real implementation is [`crate::platform::windows::portable::WpdSource`]. Tests (and the GUI's debug fake mode)
//! use the sources in `crate::fake`. Nothing here can write to a device: the trait has no
//! method that could.
#![forbid(unsafe_code)]

use std::fmt;
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use crate::signals::{DeviceKind, Signals, View, parse_windows_usb_instance_id};

/// A device as listed by Windows. Listing does not open a session with the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Plug and Play device interface ID, used to open the device.
    pub pnp_id: String,
    pub friendly_name: String,
    pub manufacturer: String,
    pub description: String,
}

impl DeviceInfo {
    /// Whether Windows describes this as an Apple device. This is only a hint for the UI: any
    /// device can claim to be an iPhone, so nothing security-relevant depends on it.
    pub fn looks_like_apple(&self) -> bool {
        let all = format!(
            "{} {} {}",
            self.friendly_name, self.manufacturer, self.description
        )
        .to_lowercase();
        all.contains("apple") || all.contains("iphone")
    }
}

/// Identity reported by an opened device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceIdentity {
    /// The device's ID, used to scope the skip list, to check the backup folder's owner and to
    /// check that a sync talks to the device that was scanned. A freshly opened device has a
    /// provisional one (`serial:<serial>` when it reports a serial, else `pnp:<id>`); the
    /// worker replaces it with the app-made ID of the known device ([`Guarded::with_device_id`]).
    pub key: String,
    /// What is known about the device without further reads (see [`crate::signals`]).
    pub signals: Signals,
    /// How this connection reads the device. [`DeviceIdentity::new`] says Windows Portable
    /// Devices, the only view there is so far; other sources set it afterwards.
    pub view: View,
    pub friendly_name: String,
    pub manufacturer: String,
    pub model: String,
    pub serial: Option<String>,
    /// What the device's own properties say it is, if they say. Only a hint for defaults.
    pub kind_hint: Option<DeviceKind>,
}

impl DeviceIdentity {
    pub fn new(
        pnp_id: &str,
        friendly_name: String,
        manufacturer: String,
        model: String,
        serial: Option<String>,
    ) -> Self {
        let key = match &serial {
            Some(s) if !s.trim().is_empty() => format!("serial:{}", s.trim()),
            _ => format!("pnp:{}", pnp_id.to_lowercase()),
        };
        let usb = parse_windows_usb_instance_id(pnp_id);
        let some = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_string());
        let signals = Signals {
            device_serial: serial.as_deref().and_then(some),
            usb_serial: usb.serial,
            usb_vendor: usb.vendor,
            usb_product: usb.product,
            maker: some(&manufacturer),
            model: some(&model),
            friendly_name: some(&friendly_name),
            ..Signals::default()
        };
        Self {
            key,
            signals,
            view: View::WindowsWpd,
            friendly_name,
            manufacturer,
            model,
            serial,
            kind_hint: None,
        }
    }

    /// One-line description for the UI, e.g. "Apple Inc. iPhone (serial 0000…)".
    pub fn label(&self) -> String {
        let mut s = format!("{} {}", self.manufacturer, self.model)
            .trim()
            .to_string();
        if s.is_empty() {
            s = self.friendly_name.clone();
        }
        if let Some(serial) = &self.serial {
            s.push_str(&format!(" (serial {serial})"));
        }
        s
    }
}

/// A file found on the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceFile {
    /// Opaque, session-specific object ID used to read the file.
    pub object_id: String,
    /// `WPD_OBJECT_PERSISTENT_UNIQUE_ID`, when the device reports one.
    pub persistent_id: Option<String>,
    /// Folder names then the file name, exactly as the device reported them (after removing
    /// invisible direction marks). Not yet validated: see [`crate::paths::SafeRelPath`].
    pub components: Vec<String>,
    /// Size the device reports, if any.
    pub size: Option<u64>,
    /// The device-reported last-modified time, if any.
    pub modified: Option<SystemTime>,
    /// The name of the storage or volume the file is on, if the device has more than one.
    pub storage: Option<String>,
}

impl DeviceFile {
    pub fn display_path(&self) -> String {
        self.components.join("/")
    }
}

/// Properties of one object, re-read just before copying it, so a file that was replaced since the scan is not copied by mistake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectInfo {
    pub name: String,
    pub size: Option<u64>,
    pub persistent_id: Option<String>,
    /// The device-reported last-modified time, if any.
    pub modified: Option<SystemTime>,
    /// The name of the storage or volume the file is on, if any.
    pub storage: Option<String>,
}

/// Something that could not be read or used. Scans report these instead of silently dropping
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanIssue {
    /// Where: a device path, a backup path, or an object ID.
    pub location: String,
    pub problem: String,
}

impl ScanIssue {
    pub fn new(location: impl Into<String>, problem: impl Into<String>) -> Self {
        Self {
            location: location.into(),
            problem: problem.into(),
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct Enumeration {
    pub files: Vec<DeviceFile>,
    pub issues: Vec<ScanIssue>,
    /// Device folders that could not be listed completely, as path parts joined with `/`
    /// (`""` is the whole device). Nothing under them counts as deleted from the device.
    pub incomplete: Vec<String>,
    /// Device folders that were listed and came back empty, as path parts joined with `/`. A
    /// source that can tell fills this in. Backup files under such a folder are held back from
    /// archiving until confirmed: an empty listing is often a glitch.
    pub empty_folders: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceError {
    Cancelled,
    Failed(String),
}

impl fmt::Display for DeviceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeviceError::Cancelled => write!(f, "cancelled"),
            DeviceError::Failed(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for DeviceError {}

/// What to do when a phone stops answering.
const GONE_ADVICE: &str =
    "The phone stopped answering. Unplug it, unlock it, plug it back in, then scan again.";

/// The 8-digit hex code in `0x` form that ends a Windows error text, e.g. `0x800701B1`, as a
/// number (the last one in the text).
fn error_code(text: &str) -> Option<u32> {
    let bytes = text.as_bytes();
    let mut found = None;
    let mut i = 0;
    while i + 10 <= bytes.len() {
        if bytes[i] == b'0'
            && (bytes[i + 1] == b'x' || bytes[i + 1] == b'X')
            && let Some(hex) = text.get(i + 2..i + 10)
            && let Ok(n) = u32::from_str_radix(hex, 16)
            && hex.bytes().all(|b| b.is_ascii_hexdigit())
        {
            found = Some(n);
            i += 10;
        } else {
            i += 1;
        }
    }
    found
}

/// What a Windows or WPD error code means for the user, if it is one of the ways a phone's
/// connection goes wrong. The codes are Win32 errors wrapped as HRESULTs (0x8007xxxx, the Win32
/// number in the low 16 bits) and the portable-device range 0x80042000 to 0x8004200F (what a
/// device returns when its session is gone or its storage stopped answering).
fn plain_meaning(code: u32) -> Option<&'static str> {
    Some(match code {
        // 433 ERROR_NO_SUCH_DEVICE, 55 ERROR_DEV_NOT_EXIST, 1167 ERROR_DEVICE_NOT_CONNECTED,
        // 31 ERROR_GEN_FAILURE (the device is not functioning), 121 ERROR_SEM_TIMEOUT (it
        // timed out), 1117 ERROR_IO_DEVICE, 995 ERROR_OPERATION_ABORTED (the I/O was cut off),
        // 6 ERROR_INVALID_HANDLE (the session's handle is dead).
        0x8007_01B1 | 0x8007_0037 | 0x8007_048F | 0x8007_001F | 0x8007_0079 | 0x8007_045D
        | 0x8007_03E3 | 0x8007_0006 => GONE_ADVICE,
        // The portable-device range.
        0x8004_2000..=0x8004_200F => GONE_ADVICE,
        // RPC to the portable-device service failed or dropped: 1722, 1726, 1727.
        0x8007_06BA | 0x8007_06BE | 0x8007_06BF | 0x8001_0108 => GONE_ADVICE,
        // 21 ERROR_NOT_READY: the phone is there but won't talk (locked).
        0x8007_0015 => "The phone isn't ready. Unlock it, tap Trust if it asks, then scan again.",
        _ => return None,
    })
}

/// Whether `text` ends in the code of a lost phone connection (see [`plain_error`]): the session
/// with the device can no longer be trusted.
pub fn is_connection_lost(text: &str) -> bool {
    error_code(text).and_then(plain_meaning).is_some()
}

/// A device error in plain words, with its code kept in brackets at the end: `A device which
/// does not exist was specified. (0x800701B1)` becomes `The phone stopped answering. Unplug it,
/// unlock it, plug it back in, then scan again. (0x800701B1)`. Text with no known code is
/// returned as it is.
pub fn plain_error(text: &str) -> String {
    match error_code(text).map(|c| (c, plain_meaning(c))) {
        Some((code, Some(plain))) => format!("{plain} (0x{code:08X})"),
        _ => text.to_string(),
    }
}

/// A read-only source of files. There is deliberately no method that writes.
pub trait DeviceSource {
    fn identity(&self) -> &DeviceIdentity;

    /// List every file. Problems with individual folders or objects go into
    /// [`Enumeration::issues`]; only cancellation or total failure return `Err`.
    /// `progress` receives (files found so far, folder being listed).
    fn enumerate(
        &mut self,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(usize, &str),
    ) -> Result<Enumeration, DeviceError>;

    /// Re-read one object's name, size and persistent ID.
    fn object_info(&mut self, object_id: &str) -> Result<ObjectInfo, DeviceError>;

    /// Open the object's contents for reading.
    fn open_read(&mut self, object_id: &str) -> Result<Box<dyn Read + '_>, DeviceError>;

    /// A transfer over this connection didn't finish cleanly. [`Guarded`] then refuses to read
    /// any more file data; other sources ignore it.
    fn spoil(&mut self) {}

    /// Whether this connection has been spoiled ([`DeviceSource::spoil`]); only [`Guarded`]
    /// keeps track.
    fn is_spoiled(&self) -> bool {
        false
    }

    /// The folder on this PC that the source reads, if it reads one (a mounted stick or drive).
    /// Later wiring refuses a backup folder inside it, or it inside the backup folder.
    fn local_root(&self) -> Option<&std::path::Path> {
        None
    }
}

impl<T: DeviceSource + ?Sized> DeviceSource for Box<T> {
    fn identity(&self) -> &DeviceIdentity {
        (**self).identity()
    }

    fn enumerate(
        &mut self,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(usize, &str),
    ) -> Result<Enumeration, DeviceError> {
        (**self).enumerate(cancel, progress)
    }

    fn object_info(&mut self, object_id: &str) -> Result<ObjectInfo, DeviceError> {
        (**self).object_info(object_id)
    }

    fn open_read(&mut self, object_id: &str) -> Result<Box<dyn Read + '_>, DeviceError> {
        (**self).open_read(object_id)
    }

    fn spoil(&mut self) {
        (**self).spoil()
    }

    fn is_spoiled(&self) -> bool {
        (**self).is_spoiled()
    }

    fn local_root(&self) -> Option<&std::path::Path> {
        (**self).local_root()
    }
}

/// Wraps a device connection so that no file data is read over it once a transfer has gone
/// wrong: a reader dropped before the end of the data, or [`DeviceSource::spoil`]. An iPhone
/// has been seen sending each later file's predecessor after Stop part-way through a file, so
/// reading on would save photos under their neighbours' names. A size
/// check can't catch neighbours of the same size; this can. Open the device again for a new
/// connection.
pub struct Guarded<S> {
    pub inner: S,
    spoiled: Arc<AtomicBool>,
    /// The inner identity with the device's app-made ID, once it is known.
    identified: Option<DeviceIdentity>,
}

impl<S> Guarded<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            spoiled: Arc::new(AtomicBool::new(false)),
            identified: None,
        }
    }

    pub fn is_spoiled(&self) -> bool {
        self.spoiled.load(Ordering::Relaxed)
    }
}

impl<S: DeviceSource> Guarded<S> {
    /// After a spoil nothing is asked of the device over this connection: an iPhone's late
    /// replies can answer a request with the previous request's data, so a listing or a
    /// re-check is as unreliable as a read.
    fn refuse_if_spoiled(&self) -> Result<(), DeviceError> {
        if self.is_spoiled() {
            return Err(DeviceError::Failed(
                "an earlier transfer over this connection didn't finish cleanly, so nothing \
                 more is read over it; the device must be opened again"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Make [`DeviceSource::identity`] report `id` as the device's key: the app-made ID of the
    /// known device this connection was recognised as (or registered as).
    pub fn with_device_id(mut self, id: &str) -> Self {
        let mut identity = self.inner.identity().clone();
        identity.key = id.to_string();
        self.identified = Some(identity);
        self
    }
}

impl<S: DeviceSource> DeviceSource for Guarded<S> {
    fn identity(&self) -> &DeviceIdentity {
        self.identified
            .as_ref()
            .unwrap_or_else(|| self.inner.identity())
    }

    fn enumerate(
        &mut self,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(usize, &str),
    ) -> Result<Enumeration, DeviceError> {
        self.refuse_if_spoiled()?;
        self.inner.enumerate(cancel, progress)
    }

    fn object_info(&mut self, object_id: &str) -> Result<ObjectInfo, DeviceError> {
        self.refuse_if_spoiled()?;
        self.inner.object_info(object_id)
    }

    fn open_read(&mut self, object_id: &str) -> Result<Box<dyn Read + '_>, DeviceError> {
        self.refuse_if_spoiled()?;
        let reader = self.inner.open_read(object_id)?;
        Ok(Box::new(GuardedReader {
            inner: reader,
            at_end: false,
            spoiled: self.spoiled.clone(),
        }))
    }

    fn spoil(&mut self) {
        self.spoiled.store(true, Ordering::Relaxed);
        self.inner.spoil();
    }

    fn is_spoiled(&self) -> bool {
        Guarded::is_spoiled(self)
    }

    fn local_root(&self) -> Option<&std::path::Path> {
        self.inner.local_root()
    }
}

struct GuardedReader<R> {
    inner: R,
    at_end: bool,
    spoiled: Arc<AtomicBool>,
}

impl<R: Read> Read for GuardedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let r = self.inner.read(buf);
        if !buf.is_empty() && matches!(r, Ok(0)) {
            self.at_end = true;
        }
        r
    }
}

impl<R> Drop for GuardedReader<R> {
    fn drop(&mut self) {
        if !self.at_end {
            self.spoiled.store(true, Ordering::Relaxed);
        }
    }
}

/// Whether `c` is an invisible direction control: LEFT-TO-RIGHT MARK, RIGHT-TO-LEFT MARK, or
/// one of the embedding and override controls U+202A to U+202E.
fn is_direction_mark(c: char) -> bool {
    c == '\u{200E}' || c == '\u{200F}' || ('\u{202A}'..='\u{202E}').contains(&c)
}

/// A device-supplied name without the invisible direction marks some devices put in names.
pub fn strip_direction_marks(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    out.extend(name.chars().filter(|&c| !is_direction_mark(c)));
    out
}

/// A folder name for each storage, made from what the device calls it. The name is cleaned
/// ([`clean_storage_name`]); if it still can't be a safe name, `Storage <n>` is used. No two come
/// out the same ignoring case. Every device reads its storages through this, so the same
/// phone gets the same folder names whichever system it is plugged into.
pub fn storage_folder_names(names: &[String]) -> Vec<String> {
    let mut taken: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(names.len());
    for (i, raw) in names.iter().enumerate() {
        let base = clean_storage_name(raw)
            .filter(|n| crate::paths::validate_component(n).is_ok())
            .unwrap_or_else(|| format!("Storage {}", i + 1));
        let mut candidate = base.clone();
        let mut n = 2;
        while !taken.insert(candidate.to_lowercase()) {
            candidate = format!("{base} ({n})");
            n += 1;
        }
        out.push(candidate);
    }
    out
}

/// Replace what Windows forbids in a name and trim what it silently removes.
fn clean_storage_name(raw: &str) -> Option<String> {
    let replaced: String = strip_direction_marks(raw)
        .chars()
        .map(|c| {
            if (c as u32) < 0x20 || c == '\u{7f}' || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = replaced.trim().trim_end_matches('.').trim_end();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod storage_name_tests {
    use super::storage_folder_names;
    use crate::paths::validate_component;

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn storage_folders_are_safe_and_distinct() {
        let f = storage_folder_names(&names(&["Internal shared storage", "SD card"]));
        assert_eq!(f, ["Internal shared storage", "SD card"]);
        // The same ignoring case: later ones get a number.
        let f = storage_folder_names(&names(&["Card", "card", "CARD"]));
        assert_eq!(f, ["Card", "card (2)", "CARD (3)"]);
        // Forbidden characters are replaced; direction marks and trailing dots dropped.
        let f = storage_folder_names(&names(&["SD: card?.", "\u{200E}Phone\u{200F}"]));
        assert_eq!(f, ["SD_ card_", "Phone"]);
        // Names that can't be made safe fall back to a numbered one.
        let f = storage_folder_names(&names(&["", "  ", "CON", "..."]));
        assert_eq!(f, ["Storage 1", "Storage 2", "Storage 3", "Storage 4"]);
        let long = "x".repeat(300);
        for n in storage_folder_names(&names(&["a/b", long.as_str(), "nul.txt"])) {
            assert!(validate_component(&n).is_ok(), "{n}");
        }
        // Every kind of forbidden character, and the separators, are gone.
        for n in storage_folder_names(&names(&["a<b>c:d\"e/f\\g|h?i*j", "\u{1}ctl"])) {
            assert!(
                !n.contains(['<', '>', ':', '"', '/', '\\', '|', '?', '*']),
                "{n}"
            );
            assert!(validate_component(&n).is_ok(), "{n}");
        }
    }
}

#[cfg(test)]
mod direction_mark_tests {
    use super::strip_direction_marks;

    #[test]
    fn direction_marks_are_removed_and_everything_else_is_kept() {
        assert_eq!(
            strip_direction_marks("\u{200E}IMG_0001\u{202B}.HEIC\u{200F}"),
            "IMG_0001.HEIC"
        );
        assert_eq!(
            strip_direction_marks("caf\u{e9} \u{202A}\u{202E}1"),
            "caf\u{e9} 1"
        );
        // The characters either side of the U+202A..=U+202E range stay.
        assert_eq!(
            strip_direction_marks("a\u{2029}b\u{202F}c"),
            "a\u{2029}b\u{202F}c"
        );
    }
}

#[cfg(test)]
mod identity_tests {
    use super::*;
    use crate::fake::MemSource;

    #[test]
    fn the_signals_come_from_what_the_device_reports() {
        let id = DeviceIdentity::new(
            r"\\?\usb#vid_05ac&pid_12a8#00008030000a1234#{6ac27878-a6fa-4155-ba85-f98f491d4f33}",
            "My phone".into(),
            "Apple Inc.".into(),
            "iPhone".into(),
            Some("00008030000A1234".into()),
        );
        let s = &id.signals;
        assert_eq!(s.device_serial.as_deref(), Some("00008030000A1234"));
        assert_eq!(s.usb_serial.as_deref(), Some("00008030000a1234"));
        assert_eq!(s.usb_vendor.as_deref(), Some("05ac"));
        assert_eq!(s.maker.as_deref(), Some("Apple Inc."));
        assert_eq!(s.model.as_deref(), Some("iPhone"));
        assert_eq!(s.friendly_name.as_deref(), Some("My phone"));
        let blank = DeviceIdentity::new("x", " ".into(), "".into(), "".into(), Some(" ".into()));
        assert_eq!(blank.signals, Signals::default());
    }

    #[test]
    fn a_spoiled_connection_answers_nothing_more() {
        use crate::fake::MemObject;
        let mut g = Guarded::new(MemSource::new(
            "S1",
            vec![MemObject::new("1", "DCIM/A.JPG", b"abc")],
        ));
        let cancel = AtomicBool::new(false);
        assert!(g.enumerate(&cancel, &mut |_, _| {}).is_ok());
        assert!(g.object_info("1").is_ok());
        g.spoil();
        assert!(g.enumerate(&cancel, &mut |_, _| {}).is_err());
        assert!(g.object_info("1").is_err());
        assert!(g.open_read("1").is_err());
    }

    #[test]
    fn a_guarded_connection_reports_the_app_made_id() {
        let src = MemSource::new("S1", Vec::new());
        assert_eq!(src.identity().key, "serial:S1");
        let g = Guarded::new(src).with_device_id("5f1c");
        assert_eq!(g.identity().key, "5f1c");
        assert_eq!(g.identity().signals.device_serial.as_deref(), Some("S1"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phone_connection_errors_are_put_in_plain_words_with_the_code_kept() {
        let t = plain_error("A device which does not exist was specified. (0x800701B1)");
        assert_eq!(
            t,
            "The phone stopped answering. Unplug it, unlock it, plug it back in, then scan again. (0x800701B1)"
        );
        assert!(plain_error("0x80042008").ends_with("scan again. (0x80042008)"));
        assert!(plain_error("x (0x8007001f)").contains("stopped answering"));
        assert!(plain_error("(0x80070079)").contains("stopped answering"));
        assert!(plain_error("(0x80070015)").contains("isn't ready"));
        assert!(is_connection_lost(
            "could not list this folder: (0x80042008)"
        ));
    }

    #[test]
    fn other_text_is_left_alone() {
        for t in [
            "Access is denied. (0x80070005)",
            "no code here",
            "0x8004",
            "0x8004200G",
            "",
        ] {
            assert_eq!(plain_error(t), t);
            assert!(!is_connection_lost(t));
        }
    }
}
