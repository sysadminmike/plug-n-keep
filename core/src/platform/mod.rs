//! Everything that talks to the operating system lives under here, behind the small functions
//! below. The rest of the crate is portable and never names a system.
//!
//! - **Moves.** [`rename_no_replace`] and [`rename_replace`] put a file into place. When they
//!   return without error the move has happened. The new file's data is whatever the caller
//!   flushed beforehand. The folders involved are flushed afterwards where the system allows
//!   it; a folder that can't be flushed is logged as a warning (quietly if the file system
//!   says it can't flush folders at all) and the move is still reported as done, so a power
//!   cut right after may undo it. On Linux and FreeBSD, where the move is a link then an
//!   unlink, the new name's folder is flushed before the old name goes. Where the system has
//!   no hard links the name is claimed with an empty placeholder first.
//! - **Flushing.** [`flush_file`] and [`flush_dir`].
//! - **The bin.** [`send_to_bin`] sends one file to the Recycle Bin or Trash, and never deletes.
//! - **Links.** [`is_link`] says whether metadata describes a link (a symlink, or on Windows a
//!   junction or other reparse point). Callers refuse to follow or write through one.
//! - **One copy of the app.** [`acquire_lock`].
//! - **Settings.** [`settings_dir`].
//! - **The clock.** [`local_stamp`], [`utc_stamp`], [`local_time_ms`].
//! - **Plug-in events.** [`notify::watch`].
//! - **Devices.** [`Session`] lists and opens devices of every [`Transport`].
//! - **Eject.** [`can_eject`] and [`eject`] let a stick or drive go safely (Windows and Linux).
//! - **HEIC photos.** [`heic_available`] and [`heic_decode`], through the system's own decoder.
//! - **Dates.** [`set_modified`].
//! - **Which volume.** [`volume_token`].
//!
//! `unsafe` is confined to this module's system-specific files.

#[cfg(target_os = "freebsd")]
pub mod freebsd;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(windows)]
pub mod windows;

#[cfg(any(test, target_os = "freebsd"))]
mod bsd_mounts;
pub mod poll;
#[cfg(unix)]
mod statvfs;
#[cfg(unix)]
mod unix;
#[cfg(unix)]
mod unix_trash;

#[cfg(target_os = "freebsd")]
use freebsd as sys;
#[cfg(target_os = "linux")]
use linux as sys;
#[cfg(windows)]
use windows as sys;

#[cfg(not(any(windows, target_os = "linux", target_os = "freebsd")))]
compile_error!("Plug-n-Keep supports Windows, Linux and FreeBSD");

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::device::DeviceSource;
use crate::signals::DeviceKind;

/// How a device is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Transport {
    /// Windows Portable Devices (WPD).
    Portable,
    /// A mounted file system: a stick, card or drive.
    Volume,
    /// An Android phone or player through libmtp.
    Mtp,
    /// An iPhone through usbmuxd and AFC.
    Afc,
    /// A still camera through libgphoto2.
    Camera,
    /// A test or debug stand-in that is not a device.
    Fake,
}

/// A device as listed, whatever its transport. Listing does not open it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedDevice {
    /// Identifies the device to [`Session::open`] within its transport (for WPD, the Plug and
    /// Play interface ID). Unique across the list.
    pub id: String,
    /// A name for the device picker.
    pub label: String,
    /// A guess at what the device is, if the listing can tell. Only a hint for the window.
    pub kind_hint: Option<DeviceKind>,
    pub transport: Transport,
}

impl ListedDevice {
    /// Whether the label says this is an Apple device. Only a hint for the window: any device
    /// can claim to be an iPhone, so nothing that keeps data safe depends on it.
    pub fn looks_like_apple(&self) -> bool {
        let label = self.label.to_lowercase();
        label.contains("apple") || label.contains("iphone")
    }
}

/// The device thread's hold on the system, kept for as long as it runs. `init` it on that
/// thread; dropping it lets go.
///
/// - `list_devices()`: the devices that are plugged in, of every transport, without opening any.
/// - `open(device)`: a read-only source for one of them; its `identity()` carries the signals
///   and view, whatever the transport.
pub struct Session {
    sys: sys::Session,
}

impl Session {
    pub fn init() -> Result<Self, String> {
        Ok(Self {
            sys: sys::Session::init()?,
        })
    }

    pub fn list_devices(&self) -> Result<Vec<ListedDevice>, String> {
        let mut all = self.sys.list_portable()?;
        all.extend(sys::volume::list());
        #[cfg(target_os = "linux")]
        {
            all.extend(sys::mtp::list());
            all.extend(sys::ios::list());
            all.extend(sys::camera::list());
        }
        Ok(all)
    }

    /// A note for the device list when something that would list devices isn't working (on
    /// Linux, a missing phone library or usbmuxd not running means no phone of that kind can ever
    /// be listed). Valid after [`Session::list_devices`]. One line per note.
    pub fn list_hint(&self) -> Option<String> {
        #[cfg(target_os = "linux")]
        {
            let hints: Vec<String> = [sys::mtp::hint(), sys::ios::hint(), sys::camera::hint()]
                .into_iter()
                .flatten()
                .collect();
            (!hints.is_empty()).then(|| hints.join("\n"))
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    }

    pub fn open(&self, device: &ListedDevice) -> Result<Box<dyn DeviceSource>, String> {
        match device.transport {
            Transport::Portable => self.sys.open_portable(&device.id),
            Transport::Volume => sys::volume::open(device),
            #[cfg(target_os = "linux")]
            Transport::Mtp => sys::mtp::open(device),
            #[cfg(target_os = "linux")]
            Transport::Afc => sys::ios::open(device),
            #[cfg(target_os = "linux")]
            Transport::Camera => sys::camera::open(device),
            #[cfg(not(target_os = "linux"))]
            Transport::Camera => Err("Cameras through libgphoto2 are not available here".into()),
            #[cfg(not(target_os = "linux"))]
            Transport::Mtp => Err("Android phones (MTP) are not available yet".into()),
            #[cfg(not(target_os = "linux"))]
            Transport::Afc => Err("iPhones through AFC are not available yet".into()),
            Transport::Fake => Err(format!(
                "No device {} is available on this system",
                device.id
            )),
        }
    }
}

/// The mounted volumes only (sticks, cards, drives): the volume part of
/// [`Session::list_devices`], with no phone, portable-device or camera enumeration and no
/// system session. For command-line tools that must never touch a phone.
pub fn list_volumes() -> Vec<ListedDevice> {
    sys::volume::list()
}

/// Opens a listed volume as a read-only source, as [`Session::open`] does for a
/// [`Transport::Volume`] device, without needing a [`Session`].
pub fn open_volume(device: &ListedDevice) -> Result<Box<dyn DeviceSource>, String> {
    if device.transport != Transport::Volume {
        return Err(format!("{} is not a mounted volume", device.id));
    }
    sys::volume::open(device)
}

/// Whether the app can eject this device: a stick, card or drive (a mounted volume) on Windows
/// and Linux. Never a phone or a camera, and nothing on FreeBSD for now. The window shows its
/// Eject button only when this says yes.
pub fn can_eject(device: &ListedDevice) -> bool {
    cfg!(any(windows, target_os = "linux")) && device.transport == Transport::Volume
}

/// Lets a stick or drive go safely, the way the system's own Eject does:
///
/// - **Windows:** asks Plug and Play to remove the disk's device node (`CM_Request_Device_EjectW`,
///   the node Explorer's Eject uses). Nothing is opened, locked or dismounted by the app.
/// - **Linux:** asks udisks2 over D-Bus to unmount every mounted partition of the drive, then to
///   power the drive off (or eject it, where it can't be powered off).
///
/// The caller must have let go of the device first (its source dropped), or the system refuses
/// because the device is in use. On success the device is safe to unplug. The error is a sentence
/// for the user. Tests never reach the system: here the call is refused.
pub fn eject(device: &ListedDevice) -> Result<(), String> {
    if !can_eject(device) {
        return Err("This kind of device can't be ejected from here".into());
    }
    eject_now(device)
}

#[cfg(test)]
fn eject_now(_device: &ListedDevice) -> Result<(), String> {
    Err("tests never eject a real device".into())
}

#[cfg(all(not(test), any(windows, target_os = "linux")))]
fn eject_now(device: &ListedDevice) -> Result<(), String> {
    sys::eject::eject(device)
}

#[cfg(all(not(test), not(any(windows, target_os = "linux"))))]
fn eject_now(_device: &ListedDevice) -> Result<(), String> {
    Err("Eject is not available on this system yet".into())
}

/// Plug-in and removal detection (DESIGN.md section 4.1): [`notify::watch`] says when the list
/// of devices may have changed, so the caller lists them again.
pub mod notify {
    pub use super::poll::{Wake, Watcher};

    /// Starts watching for devices coming and going. `wake` is called from the watcher's own
    /// thread, after a short quiet period, when the devices may have changed. The watcher stops
    /// when the returned handle is dropped.
    pub fn watch(wake: Wake) -> Result<Watcher, String> {
        super::sys::notify::watch(wake)
    }
}

/// A number that tells the volume the folder `root` is on from any other, for as long as it is
/// mounted: the volume's serial number and GUID path on Windows, the device number of the file system
/// elsewhere. Read when a stick is opened and again before it is read, so that a different
/// stick at the same place is noticed.
pub fn volume_token(root: &Path) -> io::Result<u64> {
    sys::volume_token(root)
}

/// Sets a file's last-modified time. The file must be open for writing (Windows refuses a
/// read-only handle).
pub fn set_modified(file: &File, time: SystemTime) -> io::Result<()> {
    file.set_modified(time)
}

/// Whether HEIC photos can be decoded on this computer (the system's own decoder: WIC on
/// Windows, `libheif` on Linux and FreeBSD). If not, the text says what to install.
pub fn heic_available() -> Result<(), String> {
    sys::heic::available()
}

/// Decodes the primary image of a HEIC or HEIF file to 8-bit RGB with the system's decoder.
pub fn heic_decode(path: &Path) -> Result<crate::heic::Decoded, crate::heic::DecodeError> {
    sys::heic::decode(path)
}

/// Moves `from` to `to` and fails with `ErrorKind::AlreadyExists`, changing nothing, if `to`
/// exists. Never replaces anything.
pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    sys::rename_no_replace(from, to)
}

/// Moves `from` to `to`, atomically replacing `to` if it exists. For the app's own files only.
pub fn rename_replace(from: &Path, to: &Path) -> io::Result<()> {
    sys::rename_replace(from, to)
}

/// What the bin is called here: the Recycle Bin on Windows, the Trash on Linux and FreeBSD.
pub const BIN_NAME: &str = if cfg!(windows) {
    "Recycle Bin"
} else {
    "Trash"
};

/// Sends one plain file to the Recycle Bin or Trash, where it can be restored. It is **never**
/// a permanent delete: if the file can't go to the bin (none on its drive, too big for it, a
/// network share), nothing happens and the error says why. The file is then still where it was.
/// `path` must be absolute.
pub fn send_to_bin(path: &Path) -> Result<(), String> {
    // Tests of the sync logic must never touch the real bin: they put a stand-in in place first.
    #[cfg(test)]
    return fake_bin::send(path);
    #[cfg(not(test))]
    sys::send_to_bin(path)
}

/// A stand-in for the bin, for tests of the code that uses [`send_to_bin`]. It is per thread, and
/// with none set [`send_to_bin`] panics instead of reaching the real bin.
#[cfg(test)]
pub(crate) mod fake_bin {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    #[derive(Clone)]
    pub enum Fake {
        /// Moves the file into this folder (a bin that works), under a free name.
        Into(PathBuf),
        /// A bin that can't take anything, as on a drive with none.
        Unavailable,
    }

    thread_local! {
        static FAKE: RefCell<Option<Fake>> = const { RefCell::new(None) };
    }

    /// Puts the stand-in in place until the guard is dropped.
    pub struct Guard;

    pub fn set(fake: Fake) -> Guard {
        if let Fake::Into(dir) = &fake {
            std::fs::create_dir_all(dir).unwrap();
        }
        FAKE.with(|f| *f.borrow_mut() = Some(fake));
        Guard
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            FAKE.with(|f| *f.borrow_mut() = None);
        }
    }

    pub(super) fn send(path: &Path) -> Result<(), String> {
        match FAKE.with(|f| f.borrow().clone()) {
            None => panic!("a test reached the real bin: set a fake_bin first"),
            Some(Fake::Unavailable) => Err("the fake bin is unavailable".into()),
            Some(Fake::Into(dir)) => {
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                for n in 0..1000 {
                    let to = dir.join(if n == 0 {
                        name.clone()
                    } else {
                        format!("{n}-{name}")
                    });
                    match super::rename_no_replace(path, &to) {
                        Ok(()) => return Ok(()),
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                        Err(e) => return Err(e.to_string()),
                    }
                }
                Err("no free name in the fake bin".into())
            }
        }
    }
}

/// The size of a disk and what is free on it for this user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskSpace {
    pub total: u64,
    pub free: u64,
}

impl DiskSpace {
    /// The part in use.
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.free)
    }
}

/// The size and free space of the disk (or volume) that holds `path`, or `None` if it can't be
/// asked. `GetDiskFreeSpaceExW` on Windows, `statvfs` on Linux and FreeBSD. Only reads.
pub fn disk_space(path: &Path) -> Option<DiskSpace> {
    #[cfg(test)]
    if let Some(fake) = fake_space::get() {
        return fake;
    }
    sys::disk_space(path)
}

/// A stand-in for the disk's free space, for tests of the code that checks it. Per thread; with
/// none set, [`disk_space`] asks the real disk.
#[cfg(test)]
pub(crate) mod fake_space {
    use super::DiskSpace;
    use std::cell::Cell;

    thread_local! {
        static FAKE: Cell<Option<Option<DiskSpace>>> = const { Cell::new(None) };
    }

    pub struct Guard;

    /// Every `disk_space` on this thread answers `space` until the guard is dropped.
    pub fn set(space: Option<DiskSpace>) -> Guard {
        FAKE.with(|f| f.set(Some(space)));
        Guard
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            FAKE.with(|f| f.set(None));
        }
    }

    pub(super) fn get() -> Option<Option<DiskSpace>> {
        FAKE.with(Cell::get)
    }
}

/// Forces a file's data to disk.
pub fn flush_file(file: &File) -> io::Result<()> {
    sys::flush_file(file)
}

/// Forces a folder's entries to disk, where that matters (not on Windows, whose moves are
/// write-through).
pub fn flush_dir(dir: &Path) -> io::Result<()> {
    sys::flush_dir(dir)
}

/// Whether metadata from `symlink_metadata` describes a link: a symlink, or on Windows also a
/// junction or any other reparse point.
pub fn is_link(meta: &std::fs::Metadata) -> bool {
    sys::is_link(meta)
}

/// Locks `path` (created if needed) for the life of the returned file. `Ok(None)` if another
/// process holds it.
pub fn acquire_lock(path: &Path) -> io::Result<Option<File>> {
    sys::acquire_lock(path)
}

/// The settings folder: `%APPDATA%\<app_dir>` on Windows, else `$XDG_CONFIG_HOME/<app_dir>` or
/// `~/.config/<app_dir>`. `None` if the system doesn't say where.
pub fn settings_dir(app_dir: &str) -> Option<PathBuf> {
    sys::settings_dir(app_dir)
}

/// The local date and time as a folder name, e.g. `2026-11-16 15-30-05` (no `:`).
pub fn local_stamp() -> String {
    sys::local_stamp()
}

/// The current UTC time as text, e.g. `2026-11-17T18:00:00Z`.
pub fn utc_stamp() -> String {
    sys::utc_stamp()
}

/// The local time of day in milliseconds since midnight, for time-stamping log lines.
pub fn local_time_ms() -> u32 {
    sys::local_time_ms()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn no_replace_never_overwrites_and_keeps_both_files() {
        let t = tempfile::tempdir().unwrap();
        let (from, to) = (t.path().join("new"), t.path().join("old"));
        fs::write(&from, b"new").unwrap();
        fs::write(&to, b"old").unwrap();
        let err = rename_no_replace(&from, &to).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
        assert_eq!(fs::read(&from).unwrap(), b"new");
        assert_eq!(fs::read(&to).unwrap(), b"old");
        // Onto a free name it moves.
        let free = t.path().join("free");
        rename_no_replace(&from, &free).unwrap();
        assert!(!from.exists());
        assert_eq!(fs::read(&free).unwrap(), b"new");
    }

    #[test]
    #[cfg(unix)]
    fn no_replace_refuses_a_name_taken_by_a_link_and_leaves_its_target() {
        let t = tempfile::tempdir().unwrap();
        let (from, to, victim) = (
            t.path().join("new"),
            t.path().join("link"),
            t.path().join("victim"),
        );
        fs::write(&from, b"new").unwrap();
        fs::write(&victim, b"precious").unwrap();
        std::os::unix::fs::symlink(&victim, &to).unwrap();
        assert!(rename_no_replace(&from, &to).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"precious");
        assert_eq!(fs::read(&from).unwrap(), b"new");
    }

    #[test]
    fn replace_swaps_in_the_new_file() {
        let t = tempfile::tempdir().unwrap();
        let (from, to) = (t.path().join("new"), t.path().join("old"));
        fs::write(&from, b"new").unwrap();
        fs::write(&to, b"old").unwrap();
        rename_replace(&from, &to).unwrap();
        assert!(!from.exists());
        assert_eq!(fs::read(&to).unwrap(), b"new");
    }

    #[test]
    fn a_folder_link_is_detected_and_a_plain_folder_is_not() {
        let t = tempfile::tempdir().unwrap();
        let (real, link) = (t.path().join("real"), t.path().join("link"));
        fs::create_dir(&real).unwrap();
        crate::testutil::make_junction(&link, &real);
        assert!(is_link(&fs::symlink_metadata(&link).unwrap()));
        assert!(!is_link(&fs::symlink_metadata(&real).unwrap()));
    }

    #[test]
    fn flushing_a_file_and_a_folder_works() {
        let t = tempfile::tempdir().unwrap();
        let f = File::create(t.path().join("f")).unwrap();
        flush_file(&f).unwrap();
        flush_dir(t.path()).unwrap();
    }

    #[test]
    fn a_utc_stamp_looks_like_iso_8601() {
        let s = utc_stamp();
        assert_eq!(s.len(), "2026-11-17T18:00:00Z".len(), "{s}");
        assert!(s.ends_with('Z') && s.as_bytes()[10] == b'T', "{s}");
    }

    #[test]
    fn a_stamp_has_no_characters_windows_refuses() {
        let s = local_stamp();
        assert_eq!(s.len(), "2026-11-16 15-30-05".len(), "{s}");
        assert!(!s.contains([':', '/', '\\']), "{s}");
    }

    #[test]
    fn a_files_modified_time_can_be_set() {
        use std::time::Duration;
        let t = tempfile::tempdir().unwrap();
        let path = t.path().join("f");
        let f = File::create(&path).unwrap();
        let when = SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000);
        set_modified(&f, when).unwrap();
        drop(f);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), when);
    }

    // Windows only: its open() refuses a bad ID before looking at any drive. The other systems
    // read the real mount table first, which the tests here never do.
    #[test]
    #[cfg(windows)]
    fn a_volume_is_refused_when_its_id_is_not_a_drive() {
        let s = Session::init().unwrap();
        let v = ListedDevice {
            id: "this-folder-does-not-exist".into(),
            label: "x".into(),
            kind_hint: None,
            transport: Transport::Volume,
        };
        assert!(s.open(&v).is_err());
    }

    #[test]
    fn the_settings_folder_is_found_and_named_for_the_app() {
        let d = settings_dir("plug-n-keep").expect("HOME or APPDATA is set");
        assert!(d.ends_with("plug-n-keep"), "{}", d.display());
    }
}
