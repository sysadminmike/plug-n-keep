//! The devices the app knows: `devices.json` in the app-data folder (DESIGN.md section 9), and
//! rebuilding it from the markers in the backup folders ("Find my backups", section 5.3).
//!
//! Each known device has an ID the app made (a random UUID, never changing), the signals it has
//! been seen with, its backup folder and its settings. Recognition against this list is in
//! [`crate::recognise`].
//!
//! Loading is lenient: a file that can't be read gives a warning and an empty list, and the
//! unreadable file is moved aside, never overwritten. Saving is atomic.
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use log::{info, warn};
use serde::{Deserialize, Serialize};

use crate::folder_record::{self, Fit};
use crate::history::History;
use crate::owner::{self, OwnerState};
use crate::paths::is_link;
use crate::signals::{DeviceKind, SignalKind, Signals, View};
use crate::state::StateDir;
use crate::store::{Destination, S3Config};

const DEVICES_FILE: &str = "devices.json";
/// The most values the history keeps for one kind of signal; the oldest go first. A memory card
/// that is swapped often would otherwise grow the list without end.
const HISTORY_LIMIT: usize = 32;
/// The most "not this device" answers kept for one device.
const NOT_THIS_LIMIT: usize = 64;

/// Every value each kind of signal has had for one device, oldest first. A phone renamed twice
/// keeps all three names, so any of them still matches.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SignalHistory(BTreeMap<SignalKind, Vec<String>>);

impl SignalHistory {
    pub fn from_signals(signals: &Signals) -> Self {
        let mut h = Self::default();
        h.learn(signals);
        h
    }

    /// Add the values in `signals` that the history doesn't have yet. Returns the kinds that
    /// gained a value.
    pub fn learn(&mut self, signals: &Signals) -> Vec<SignalKind> {
        let mut added = Vec::new();
        for (kind, value) in signals.values() {
            let list = self.0.entry(kind).or_default();
            if !list.contains(&value) {
                list.push(value);
                if list.len() > HISTORY_LIMIT {
                    list.remove(0);
                }
                added.push(kind);
            }
        }
        added
    }

    /// Add the values `other` has that this one doesn't (signals learned while the backup folder
    /// was out of reach, kept in memory only).
    pub fn merge(&mut self, other: &Self) {
        for (kind, values) in &other.0 {
            let list = self.0.entry(*kind).or_default();
            for v in values {
                if !list.contains(v) {
                    list.push(v.clone());
                    if list.len() > HISTORY_LIMIT {
                        list.remove(0);
                    }
                }
            }
        }
    }

    /// The (normalised) values seen for `kind`.
    pub fn values(&self, kind: SignalKind) -> &[String] {
        self.0.get(&kind).map_or(&[], Vec::as_slice)
    }

    pub fn contains(&self, kind: SignalKind, value: &str) -> bool {
        self.values(kind).iter().any(|v| v == value)
    }

    pub fn is_empty(&self) -> bool {
        self.0.values().all(Vec::is_empty)
    }

    /// Whether any value it has seen says Apple (as [`Signals::looks_like_apple`]).
    pub fn looks_like_apple(&self) -> bool {
        [
            SignalKind::Maker,
            SignalKind::Model,
            SignalKind::FriendlyName,
        ]
        .iter()
        .flat_map(|k| self.values(*k))
        .any(|v| {
            let v = v.to_lowercase();
            v.contains("apple") || v.contains("iphone")
        }) || self
            .values(SignalKind::UsbVendorProduct)
            .iter()
            .any(|v| v.to_lowercase().starts_with("05ac:"))
    }
}

/// How the files inside a dated archive folder are laid out (DESIGN.md section 6.3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveLayout {
    /// The device's sub-folders are kept inside the dated folder.
    #[default]
    KeepFolders,
    /// Every file directly in the dated folder.
    Flat,
}

/// What happens to a file that has left the device, and to the old copy of a changed one
/// (DESIGN.md section 6.3). Nothing here ever deletes anything for good.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeletedFiles {
    /// Moved into the archive folder inside the backup.
    #[default]
    Archive,
    /// A deleted file stays where it is; a changed file's old copy is renamed beside the new one.
    LeaveInPlace,
    /// Sent to the Recycle Bin (Windows) or the Trash (Linux and FreeBSD), where it can be restored.
    RecycleBin,
}

impl DeletedFiles {
    pub const ALL: [DeletedFiles; 3] = [
        DeletedFiles::Archive,
        DeletedFiles::LeaveInPlace,
        DeletedFiles::RecycleBin,
    ];

    /// Whether files that left the device are moved out of their place in the backup.
    pub fn moves_deleted(self) -> bool {
        self != DeletedFiles::LeaveInPlace
    }

    /// What happened to the file, for messages: "moved to the archive", "left in place",
    /// "sent to the Recycle Bin" (the Trash on Linux and FreeBSD).
    pub fn did(self) -> String {
        match self {
            DeletedFiles::Archive => "moved to the archive".into(),
            DeletedFiles::LeaveInPlace => "left in place".into(),
            DeletedFiles::RecycleBin => format!("sent to the {}", crate::platform::BIN_NAME),
        }
    }
}

/// Which files are backed up (DESIGN.md section 7.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileTypes {
    All,
    PhotosAndVideos,
}

/// Whether a device with several storages gets a folder for each (DESIGN.md section 6.1). The
/// choice is made once, at setup.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageLayout {
    /// Not chosen yet.
    #[default]
    Unchosen,
    /// One storage: its files go at the top of the backup folder.
    TopLevel,
    /// Each storage gets its own folder.
    PerStorage,
}

/// Reads a setting that is one of a few known words; anything this version doesn't understand
/// gives the type's default (with a warning) instead of failing the whole file.
pub(crate) fn lenient<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned + Default,
{
    let value = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(value.clone()).unwrap_or_else(|_| {
        warn!("A setting value {value} is not understood; using the default for it");
        T::default()
    }))
}

/// Reads `deleted_files`: a value this version doesn't understand (a newer version's, a typo, a
/// damaged file) gives [`DeletedFiles::LeaveInPlace`], the one choice that never moves a deleted
/// file, and not the default, which would start moving files. Warns; the settings pages say so
/// too ([`DeviceSettings::deleted_files_not_understood`]).
pub(crate) fn deleted_files_lenient<'de, D>(d: D) -> Result<DeletedFiles, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(value.clone()).unwrap_or_else(|_| {
        warn!(
            "The setting for deleted files {value} is not understood; deleted files will be left in place"
        );
        DeletedFiles::LeaveInPlace
    }))
}

/// Whether `value` (a settings object's `deleted_files`) is present but not one of the known
/// words.
pub(crate) fn deleted_files_unknown(value: Option<&serde_json::Value>) -> bool {
    value.is_some_and(|v| serde_json::from_value::<DeletedFiles>(v.clone()).is_err())
}

/// The note the settings pages show when `deleted_files` was not understood.
pub const DELETED_FILES_NOT_UNDERSTOOD: &str = "A setting in devices.json wasn't understood, so deleted files are left in place; check this device's settings.";

/// Per-device settings: scans and syncs take them from the device's entry in the registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceSettings {
    /// The archive folder's name in the backup folder.
    pub archive_name: String,
    #[serde(deserialize_with = "lenient")]
    pub archive_layout: ArchiveLayout,
    /// Where files deleted from the device go, and the old copy of a changed one. A value this
    /// version doesn't know loads as the default, so one odd value never loses the whole file.
    #[serde(deserialize_with = "deleted_files_lenient")]
    pub deleted_files: DeletedFiles,
    /// Set at load when the file's `deleted_files` was not understood (so it became
    /// `LeaveInPlace`); the device page then says so. Not stored.
    #[serde(skip)]
    pub deleted_files_not_understood: bool,
    /// Top-level device folders to back up. Empty means everything.
    pub folders: Vec<String>,
    pub file_types: FileTypes,
    /// Names the backup never includes, wherever they are.
    pub always_left_out: Vec<String>,
    /// Start a scan when the device is plugged in.
    pub scan_automatically: bool,
    /// Also compare modified dates, not only sizes.
    pub compare_by_date: bool,
    #[serde(deserialize_with = "lenient")]
    pub storage_layout: StorageLayout,
    /// With [`StorageLayout::TopLevel`]: the storage whose files sit at the top of the backup
    /// folder. A storage that turns up later gets its own folder. `None` if the device named
    /// no storage when it was set up.
    pub top_level_storage: Option<String>,
    /// Storages that have their own folder in the backup folder. A storage on this list that
    /// isn't connected (an SD card taken out) never makes its backup files look deleted.
    pub storage_folders: Vec<String>,
    /// The extensions (no dot, any case) that count as photos and videos, for
    /// [`FileTypes::PhotosAndVideos`].
    pub media_extensions: Vec<String>,
    /// File names of PC clutter: never indexed in the backup folder, so never archived, and
    /// never backed up (DESIGN.md section 6.4). Editable.
    pub ignore_names: Vec<String>,
    /// Make a JPG copy beside each HEIC photo in the backup (DESIGN.md section 7.6). On for an
    /// iPhone, off for everything else. Turning it off stops new JPGs only.
    pub jpeg_copies: bool,
}

/// The default archive folder name for a new device.
pub const DEFAULT_ARCHIVE_NAME: &str = "_Archive";

/// The extensions treated as photos and videos by default.
pub const PHOTO_VIDEO_EXTENSIONS: [&str; 33] = [
    "jpg", "jpeg", "png", "heic", "heif", "gif", "bmp", "tif", "tiff", "webp", "dng", "raw", "arw",
    "cr2", "cr3", "nef", "orf", "rw2", "raf", "srw", "mp4", "mov", "m4v", "avi", "mkv", "3gp",
    "3g2", "wmv", "mts", "m2ts", "mpg", "mpeg", "webm",
];

/// The file names ignored by default: Explorer's, Windows' and macOS's own clutter.
pub const CLUTTER_NAMES: [&str; 3] = ["desktop.ini", "Thumbs.db", ".DS_Store"];

impl DeviceSettings {
    /// The defaults for a device of this kind (DESIGN.md sections 6.3, 7.1, 7.2 and 8).
    pub fn defaults_for(kind: DeviceKind) -> Self {
        let phone = kind != DeviceKind::Storage;
        Self {
            archive_name: DEFAULT_ARCHIVE_NAME.to_string(),
            archive_layout: ArchiveLayout::KeepFolders,
            deleted_files: DeletedFiles::Archive,
            deleted_files_not_understood: false,
            folders: if phone {
                ["DCIM", "Pictures", "Movies"].map(String::from).to_vec()
            } else {
                Vec::new()
            },
            file_types: if phone {
                FileTypes::PhotosAndVideos
            } else {
                FileTypes::All
            },
            always_left_out: if phone {
                Vec::new()
            } else {
                [
                    ".plug-n-keep-id",
                    "System Volume Information",
                    "$RECYCLE.BIN",
                    ".Trashes",
                    ".Spotlight-V100",
                    ".fseventsd",
                    "FOUND.000",
                ]
                .map(String::from)
                .to_vec()
            },
            scan_automatically: false,
            compare_by_date: !phone,
            storage_layout: StorageLayout::Unchosen,
            top_level_storage: None,
            storage_folders: Vec::new(),
            media_extensions: PHOTO_VIDEO_EXTENSIONS.map(String::from).to_vec(),
            ignore_names: CLUTTER_NAMES.map(String::from).to_vec(),
            jpeg_copies: false,
        }
    }

    /// The defaults for a device of this kind with these signals: as [`Self::defaults_for`], with
    /// JPG copies of HEIC photos on for an iPhone.
    pub fn defaults_for_device(kind: DeviceKind, signals: &Signals) -> Self {
        Self {
            jpeg_copies: kind != DeviceKind::Storage && signals.looks_like_apple(),
            ..Self::defaults_for(kind)
        }
    }
}

impl Default for DeviceSettings {
    fn default() -> Self {
        Self::defaults_for(DeviceKind::Ptp)
    }
}

/// What the last sync did, for the device's page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncSummary {
    /// When it finished, as UTC text such as `2026-11-17T18:00:00Z`.
    pub when: String,
    pub files: u64,
    pub bytes: u64,
}

/// One device the app has seen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownDevice {
    /// Made by the app (a random UUID); never changes. Skip lists and the backup folder's marker
    /// refer to the device by this.
    pub id: String,
    pub name: String,
    pub kind: DeviceKind,
    /// How it was last read ([`View`]); `None` until it has been opened.
    #[serde(default)]
    pub view: Option<View>,
    pub history: SignalHistory,
    /// Where its backup is, when it is a folder on this PC. `None` until one is chosen, and for a
    /// backup in a bucket (see [`KnownDevice::s3`]).
    #[serde(default)]
    pub backup_dir: Option<PathBuf>,
    /// Where its backup is, when it is a place in a bucket. No keys are kept here (the profile
    /// names them). Left out of the file for a folder device, so those files read and write as
    /// they always did. Never set together with `backup_dir`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3: Option<S3Config>,
    pub first_seen: String,
    pub last_seen: String,
    #[serde(default)]
    pub last_sync: Option<SyncSummary>,
    /// Things the user said are not this device, so they are not proposed again.
    #[serde(default)]
    pub not_this: Vec<Signals>,
    #[serde(default)]
    pub settings: DeviceSettings,
    /// Why the device's backup folder can't be used for now ("Backup folder not found: ..."): an
    /// unplugged drive, a deleted or renamed folder. Set at load, not stored. Such a device is
    /// listed from what `devices.json` remembers of it, and is neither scanned nor synced, nor
    /// are its settings changed, until the folder is found.
    #[serde(skip)]
    pub offline: Option<String>,
}

impl KnownDevice {
    /// A new device with a fresh ID and the defaults for its kind. `now` is UTC text.
    pub fn new(kind: DeviceKind, name: &str, signals: &Signals, now: &str) -> Self {
        Self::with_id(new_device_id(), kind, name, signals, now)
    }

    pub fn with_id(id: String, kind: DeviceKind, name: &str, signals: &Signals, now: &str) -> Self {
        Self {
            id,
            name: name.to_string(),
            kind,
            view: None,
            history: SignalHistory::from_signals(signals),
            backup_dir: None,
            s3: None,
            first_seen: now.to_string(),
            last_seen: now.to_string(),
            last_sync: None,
            not_this: Vec::new(),
            settings: DeviceSettings::defaults_for_device(kind, signals),
            offline: None,
        }
    }

    /// Where its backup is: the folder, or the place in a bucket. `None` until one is chosen.
    pub fn destination(&self) -> Option<Destination> {
        match (&self.s3, &self.backup_dir) {
            (Some(c), _) => Some(Destination::S3(c.clone())),
            (None, Some(p)) => Some(Destination::Local(p.clone())),
            (None, None) => None,
        }
    }

    /// Makes `dest` the backup place, clearing the other kind.
    pub fn set_destination(&mut self, dest: Destination) {
        match dest {
            Destination::Local(p) => {
                self.backup_dir = Some(p);
                self.s3 = None;
            }
            Destination::S3(c) => {
                self.backup_dir = None;
                self.s3 = Some(c);
            }
        }
    }

    /// The user confirmed this is the device: remember the signal values it has now (a new
    /// volume serial after a reformat, say) and when it was seen. Returns the kinds that gained
    /// a value.
    pub fn learn(&mut self, signals: &Signals, now: &str) -> Vec<SignalKind> {
        self.last_seen = now.to_string();
        self.history.learn(signals)
    }

    /// The user said what was plugged in is not this device: don't propose it again.
    pub fn reject(&mut self, signals: &Signals) {
        if !self.not_this.contains(signals) {
            self.not_this.push(signals.clone());
            if self.not_this.len() > NOT_THIS_LIMIT {
                self.not_this.remove(0);
            }
        }
    }
}

/// A new random device ID.
pub fn new_device_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// `devices.json`: the app-data folder's pointers (DESIGN.md section 9). A device that has a
/// backup folder is a [`PointerEntry`]; the device's record is in that folder
/// ([`crate::folder_record`]). A device with no folder yet, or whose folder can't hold a record
/// (it isn't made, or it is another device's), is a whole [`KnownDevice`], as an earlier version
/// stored every device.
#[derive(Debug, Serialize, Deserialize)]
struct DevicesFile {
    version: u32,
    devices: Vec<serde_json::Value>,
}

/// What `devices.json` keeps of a device whose record is in its backup folder: where the folder
/// is, and enough to list the device (and recognise it) while the folder is out of reach (an
/// unplugged drive). The folder's record is what counts; this is a cache of it, and losing it
/// loses nothing.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PointerEntry {
    id: String,
    name: String,
    kind: DeviceKind,
    #[serde(default)]
    view: Option<View>,
    #[serde(default)]
    history: SignalHistory,
    #[serde(default)]
    backup_dir: Option<PathBuf>,
    #[serde(default)]
    first_seen: String,
    #[serde(default)]
    last_seen: String,
    #[serde(default)]
    last_sync: Option<SyncSummary>,
    /// Always `true`: what tells this from a whole device record.
    in_folder: bool,
}

impl PointerEntry {
    fn of(d: &KnownDevice) -> Self {
        Self {
            id: d.id.clone(),
            name: d.name.clone(),
            kind: d.kind,
            view: d.view,
            history: d.history.clone(),
            backup_dir: d.backup_dir.clone(),
            first_seen: d.first_seen.clone(),
            last_seen: d.last_seen.clone(),
            last_sync: d.last_sync.clone(),
            in_folder: true,
        }
    }

    /// The device as far as the cache goes: the settings are the defaults.
    fn device(&self) -> KnownDevice {
        let mut d = KnownDevice::with_id(
            self.id.clone(),
            self.kind,
            &self.name,
            &Signals::default(),
            &self.first_seen,
        );
        d.view = self.view;
        d.history = self.history.clone();
        d.backup_dir = self.backup_dir.clone();
        d.last_seen = self.last_seen.clone();
        d.last_sync = self.last_sync.clone();
        d
    }
}

/// Where the old `devices.json` is kept when the records move into the backup folders.
const PRE_MOVE_BACKUP: &str = "devices.json.pre-0.1-move";

/// The list of known devices.
#[derive(Debug, Default)]
pub struct Registry {
    devices: Vec<KnownDevice>,
    /// Set when `devices.json` couldn't be read and couldn't be moved aside either: saving
    /// would overwrite it, so saving is refused.
    save_blocked: Option<String>,
    /// For a device whose backup folder is out of reach ([`KnownDevice::offline`]): its entry in
    /// `devices.json` as it was read, written back as it is. Nothing is lost while the folder is
    /// away, and the stale cache never overwrites the folder's record when it returns.
    frozen: BTreeMap<String, serde_json::Value>,
}

impl Registry {
    pub fn new(devices: Vec<KnownDevice>) -> Self {
        Self {
            devices,
            save_blocked: None,
            frozen: BTreeMap::new(),
        }
    }

    /// Read `devices.json`, and each device's record from its backup folder (DESIGN.md section 9).
    ///
    /// - A missing file gives an empty list. An unreadable one gives a warning and an empty list,
    ///   and the file is kept aside as `devices.json.damaged` (numbered if there already is
    ///   one), never overwritten.
    /// - A device the file keeps in full (from an earlier version, or one with no backup folder
    ///   yet) whose backup folder is there is moved into it: the record is written to the folder,
    ///   the sync history from the settings folder is merged into the folder's, and
    ///   `devices.json` is then rewritten as pointers. The old file is first copied to
    ///   `devices.json.pre-0.1-move`, which is never replaced or deleted. If the folder already
    ///   holds a record of the device, that one counts. Doing it again changes nothing.
    /// - A device whose folder is out of reach is listed from what the file remembers of it
    ///   ([`KnownDevice::offline`]) and is moved when its folder is found.
    pub fn load(state: &StateDir) -> Self {
        state.migrate_archive_deleted();
        let path = state.path().join(DEVICES_FILE);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                info!("No {DEVICES_FILE} yet: no known devices");
                return Self::default();
            }
            Err(e) => return Self::set_aside(&path, &format!("it can't be read ({e})")),
        };
        let file = match serde_json::from_str::<DevicesFile>(&text) {
            Ok(f) => f,
            Err(e) => return Self::set_aside(&path, &format!("it is damaged ({e})")),
        };
        info!(
            "Loaded {} from {DEVICES_FILE}",
            crate::counted(file.devices.len(), "known device", "known devices")
        );
        let mut reg = Self::default();
        let mut changed = false;
        let mut moved = 0;
        for raw in file.devices {
            let outcome = if raw.get("in_folder") == Some(&serde_json::Value::Bool(true)) {
                match serde_json::from_value::<PointerEntry>(raw.clone()) {
                    Ok(p) => reg.load_pointer(&p, &raw),
                    Err(e) => {
                        return Self::set_aside(&path, &format!("it is damaged ({e})"));
                    }
                }
            } else {
                match serde_json::from_value::<KnownDevice>(raw.clone()) {
                    Ok(d) => reg.load_whole(d, &raw, state),
                    Err(e) => {
                        return Self::set_aside(&path, &format!("it is damaged ({e})"));
                    }
                }
            };
            changed |= outcome.changed;
            if outcome.moved {
                moved += 1;
            }
        }
        if moved > 0 {
            info!(
                "Moved {} into their backup folders",
                crate::counted(moved, "device's record", "devices' records")
            );
        }
        if (changed || moved > 0)
            && let Err(e) = reg.save(state)
        {
            warn!("Could not save {DEVICES_FILE}: {e}");
        }
        reg
    }

    /// Add the device a pointer entry stands for, from its backup folder if that can be read.
    fn load_pointer(&mut self, p: &PointerEntry, raw: &serde_json::Value) -> Loaded {
        let cache = p.device();
        let Some(dir) = p.backup_dir.clone() else {
            self.devices.push(cache);
            return Loaded::default();
        };
        match crate::folder_record::fit(&dir, &p.id) {
            Fit::NoFolder => {
                self.push_offline(
                    cache,
                    raw,
                    format!("Backup folder not found: {}", dir.display()),
                );
                Loaded::default()
            }
            Fit::Foreign(why) => {
                self.push_offline(
                    cache,
                    raw,
                    format!("Backup folder not usable: {} ({why})", dir.display()),
                );
                Loaded::default()
            }
            Fit::Ok => {
                let (device, changed) = from_folder_or(&dir, cache);
                self.devices.push(device);
                Loaded {
                    changed,
                    moved: false,
                }
            }
        }
    }

    /// Add a device the file keeps in full: move it into its backup folder if that can hold it.
    fn load_whole(
        &mut self,
        mut d: KnownDevice,
        raw: &serde_json::Value,
        state: &StateDir,
    ) -> Loaded {
        let mut changed = fill_from_raw_one(raw, &mut d);
        let Some(dir) = d.backup_dir.clone() else {
            self.devices.push(d);
            return Loaded {
                changed,
                moved: false,
            };
        };
        match crate::folder_record::fit(&dir, &d.id) {
            Fit::Ok => match move_into_folder(&d, &dir, state) {
                Some(device) => {
                    changed |= device != d;
                    self.devices.push(device);
                    Loaded {
                        changed,
                        moved: true,
                    }
                }
                None => {
                    self.devices.push(d);
                    Loaded {
                        changed,
                        moved: false,
                    }
                }
            },
            Fit::NoFolder if d.last_sync.is_some() => {
                let why = format!("Backup folder not found: {}", dir.display());
                self.push_offline(d, raw, why);
                Loaded::default()
            }
            // A folder not made yet, or another device's (the window says so on a scan).
            _ => {
                self.devices.push(d);
                Loaded {
                    changed,
                    moved: false,
                }
            }
        }
    }

    fn push_offline(&mut self, mut d: KnownDevice, raw: &serde_json::Value, why: String) {
        warn!("\"{}\" is listed without its backup folder: {why}", d.name);
        d.offline = Some(why);
        self.frozen.insert(d.id.clone(), raw.clone());
        self.devices.push(d);
    }

    /// Look again at the devices whose backup folder was out of reach: one that is back (the
    /// drive plugged in) is read from its folder. Returns whether any came back.
    pub fn refresh_offline(&mut self, state: &StateDir) -> bool {
        let mut back = false;
        for i in 0..self.devices.len() {
            let d = &self.devices[i];
            let (Some(_), Some(dir)) = (&d.offline, d.backup_dir.clone()) else {
                continue;
            };
            if crate::folder_record::fit(&dir, &d.id) != Fit::Ok {
                continue;
            }
            let id = d.id.clone();
            let cache_only = self.cache_only(&id);
            self.frozen.remove(&id);
            let mut device = self.devices[i].clone();
            device.offline = None;
            self.devices[i] = if cache_only {
                let learned = device.history.clone();
                let mut back = from_folder_or(&dir, device).0;
                // What was learned while the folder was away is not lost.
                back.history.merge(&learned);
                back
            } else {
                // A whole record from the old file: it moves into the folder now.
                match move_into_folder(&device, &dir, state) {
                    Some(moved) => moved,
                    None => device,
                }
            };
            self.settle(state);
            info!("The backup folder {} is back", dir.display());
            back = true;
        }
        back
    }

    /// Write `devices.json` after a change made while loading; says so if it can't be written.
    fn settle(&self, state: &StateDir) {
        if let Err(e) = self.save(state) {
            warn!("Could not save {DEVICES_FILE}: {e}");
        }
    }

    /// Whether `id`'s entry in the old file is only pointers (the whole record is in the folder).
    fn cache_only(&self, id: &str) -> bool {
        self.frozen
            .get(id)
            .is_none_or(|raw| raw.get("in_folder") == Some(&serde_json::Value::Bool(true)))
    }

    fn set_aside(path: &Path, why: &str) -> Self {
        let dir = path.parent().unwrap_or(Path::new("."));
        let mut aside = dir.join(format!("{DEVICES_FILE}.damaged"));
        let mut n = 1;
        while aside.exists() {
            n += 1;
            aside = dir.join(format!("{DEVICES_FILE}.damaged{n}"));
        }
        match crate::platform::rename_no_replace(path, &aside) {
            Ok(()) => {
                warn!(
                    "{DEVICES_FILE} was not used: {why}. It is kept as {}; starting with no known devices",
                    aside.display()
                );
                Self::default()
            }
            Err(e) => {
                let why = format!(
                    "{DEVICES_FILE} was not used ({why}) and could not be moved aside ({e}); \
                     it is left as it is and the list is not saved"
                );
                warn!("{why}");
                Self {
                    save_blocked: Some(why),
                    ..Self::default()
                }
            }
        }
    }

    /// Write each device's record into its backup folder, and `devices.json` through a temporary
    /// file and a rename: pointers for the devices that have a record in a folder, the whole
    /// record for the rest. A device whose folder is out of reach is written back as it was
    /// read. If a record can't be written to its folder (a read-only drive), the whole record
    /// goes into `devices.json` instead, so nothing is lost, and the error is returned.
    pub fn save(&self, state: &StateDir) -> io::Result<()> {
        if let Some(why) = &self.save_blocked {
            return Err(io::Error::other(why.clone()));
        }
        let mut entries = Vec::new();
        let mut first_error = None;
        for d in &self.devices {
            if d.offline.is_some() {
                let kept = self
                    .frozen
                    .get(&d.id)
                    .cloned()
                    .or_else(|| serde_json::to_value(PointerEntry::of(d)).ok());
                entries.extend(kept);
                continue;
            }
            let in_folder = match &d.backup_dir {
                Some(dir) if crate::folder_record::fit(dir, &d.id) == Fit::Ok => {
                    match folder_record::write_device(dir, d) {
                        Ok(_) => true,
                        Err(e) => {
                            warn!(
                                "Could not write the record of \"{}\" into {}: {e}",
                                d.name,
                                dir.display()
                            );
                            first_error.get_or_insert(io::Error::other(format!(
                                "Could not write the record of \"{}\" into {}: {e}",
                                d.name,
                                dir.display()
                            )));
                            false
                        }
                    }
                }
                _ => false,
            };
            let value = if in_folder {
                serde_json::to_value(PointerEntry::of(d))
            } else {
                serde_json::to_value(d)
            };
            entries.push(value.map_err(io::Error::other)?);
        }
        let file = DevicesFile {
            version: 2,
            devices: entries,
        };
        let json = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
        keep_old_file(state, &file)?;
        state.write_atomic(DEVICES_FILE, &json)?;
        first_error.map_or(Ok(()), Err)
    }

    /// Where the device's sync history is: in its backup folder if its record is kept there,
    /// else (`None`) in the settings folder.
    pub fn history_file(&self, id: &str) -> Option<PathBuf> {
        let d = self.get(id)?;
        let dir = d.backup_dir.as_ref()?;
        (d.offline.is_none() && crate::folder_record::fit(dir, id) == Fit::Ok)
            .then(|| folder_record::history_file(dir))
    }

    /// Where the device's sync history is kept: in its folder, in its bucket, or (when its record
    /// is not in a backup place yet) in the settings folder.
    pub fn history_home(&self, id: &str) -> HistoryHome {
        match self.get(id).and_then(KnownDevice::destination) {
            Some(Destination::S3(c)) => HistoryHome::Store(Destination::S3(c)),
            _ => self
                .history_file(id)
                .map_or(HistoryHome::Profile, HistoryHome::Folder),
        }
    }

    /// "Change…" to any place: a folder as [`Self::set_backup_dir`], a bucket the same way (the
    /// record goes into it and the history is copied across, when the bucket can be reached; if
    /// not, the place is still changed and the next sync writes them). The old place's files are
    /// left as they are. Returns what went wrong, if anything.
    pub fn set_destination(
        &mut self,
        state: &StateDir,
        id: &str,
        dest: Destination,
    ) -> Result<(), String> {
        let cfg = match dest {
            Destination::Local(dir) => return self.set_backup_dir(state, id, dir),
            Destination::S3(cfg) => cfg,
        };
        let from = self
            .history_file(id)
            .unwrap_or_else(|| History::new(state).profile_file(id));
        let Some(d) = self.get_mut(id) else {
            return Ok(());
        };
        d.set_destination(Destination::S3(cfg.clone()));
        d.offline = None;
        let d = d.clone();
        self.frozen.remove(id);
        let store = Destination::S3(cfg)
            .open()
            .map_err(|e| format!("Could not open the bucket: {e}"))?;
        if folder_record::fit_in(&*store, id) != Fit::Ok {
            return Ok(());
        }
        folder_record::write_device_in(&*store, &d)
            .map_err(|e| format!("Could not write the record into {}: {e}", store.describe()))?;
        folder_record::copy_history_into_in(&from, &*store).map_err(|e| {
            format!(
                "Could not copy the sync history into {}: {e}",
                store.describe()
            )
        })?;
        if from == History::new(state).profile_file(id) {
            folder_record::retire_profile_history(&from);
        }
        Ok(())
    }

    /// "Change…": the backup folder is now `dir`. If `dir` can hold the device's record, it is
    /// written there and the sync history is copied across (merged with any already there). The
    /// old folder's files are left as they are. Returns what went wrong, if anything; the
    /// folder is changed either way.
    pub fn set_backup_dir(
        &mut self,
        state: &StateDir,
        id: &str,
        dir: PathBuf,
    ) -> Result<(), String> {
        let from = self
            .history_file(id)
            .unwrap_or_else(|| History::new(state).profile_file(id));
        let Some(d) = self.get_mut(id) else {
            return Ok(());
        };
        d.backup_dir = Some(dir.clone());
        d.s3 = None;
        d.offline = None;
        let d = d.clone();
        self.frozen.remove(id);
        if crate::folder_record::fit(&dir, id) != Fit::Ok {
            return Ok(());
        }
        folder_record::write_device(&dir, &d)
            .map_err(|e| format!("Could not write the record into {}: {e}", dir.display()))?;
        folder_record::copy_history_into(&from, &dir).map_err(|e| {
            format!(
                "Could not copy the sync history into {}: {e}",
                dir.display()
            )
        })?;
        if from == History::new(state).profile_file(id) {
            folder_record::retire_profile_history(&from);
        }
        Ok(())
    }

    /// The folder was found: `dir` is the device's backup folder, and the record in it counts
    /// (it is the source of truth) if it has one. Returns what went wrong reading it.
    pub fn rehome(&mut self, id: &str, dir: &Path) -> Option<String> {
        let cache_only = self.cache_only(id);
        let d = self.get_mut(id)?;
        d.backup_dir = Some(dir.to_path_buf());
        d.s3 = None;
        d.offline = None;
        let was = d.clone();
        self.frozen.remove(id);
        match folder_record::read_device(dir, id) {
            Ok(Some(mut f)) => {
                // What was learned while the folder was away is not lost.
                f.history.merge(&was.history);
                *self.get_mut(id)? = f;
                None
            }
            Ok(None) => cache_only.then(|| {
                format!(
                    "{} has no record of the device; its settings are the defaults",
                    dir.display()
                )
            }),
            Err(e) => {
                *self.get_mut(id)? = was;
                Some(format!(
                    "The record in {} can't be used ({e})",
                    dir.display()
                ))
            }
        }
    }

    /// [`Self::rehome`] for any place: for a bucket, the record in it (if it has one) is the
    /// device's from now on. Returns what went wrong reading it.
    pub fn rehome_dest(&mut self, id: &str, dest: &Destination) -> Option<String> {
        let cfg = match dest {
            Destination::Local(dir) => return self.rehome(id, dir),
            Destination::S3(cfg) => cfg,
        };
        let d = self.get_mut(id)?;
        d.set_destination(Destination::S3(cfg.clone()));
        d.offline = None;
        let was = d.clone();
        self.frozen.remove(id);
        let label = dest.label().display().to_string();
        let store = match dest.open() {
            Ok(s) => s,
            Err(e) => return Some(format!("Could not open {label} ({e})")),
        };
        match folder_record::read_device_in(&*store, id) {
            Ok(Some(mut f)) => {
                f.history.merge(&was.history);
                f.set_destination(dest.clone());
                *self.get_mut(id)? = f;
                None
            }
            Ok(None) => None,
            Err(e) => {
                *self.get_mut(id)? = was;
                Some(format!("The record in {label} can't be used ({e})"))
            }
        }
    }

    /// One backup found in a bucket (what "Find my backups" and "Add an existing backup" read
    /// there): a device not in the list is added from the record in `dest` (or, for a backup
    /// made before the record existed, from its marker); a known device whose place is elsewhere
    /// is reported in `found.moved`, as for a folder.
    pub fn found_in_store(
        &mut self,
        dest: &Destination,
        device_id: &str,
        device_name: &str,
        now: &str,
        found: &mut FoundBackups,
    ) {
        let label = dest.label();
        if !is_sane_device_id(device_id) {
            found.problems.push(format!(
                "{} in {} has no usable device ID",
                owner::MARKER,
                label.display()
            ));
            return;
        }
        if self.get(device_id).is_some() {
            self.found_known(device_id, dest, label, found);
            return;
        }
        if let Some(other) = self.overlapping(dest) {
            found.problems.push(overlap_problem(&label, &other));
            return;
        }
        let store = match dest.open() {
            Ok(s) => s,
            Err(e) => {
                found
                    .problems
                    .push(format!("could not open {} ({e})", label.display()));
                return;
            }
        };
        let device = match folder_record::read_device_in(&*store, device_id) {
            Ok(Some(mut d)) => {
                d.set_destination(dest.clone());
                d
            }
            other => {
                if let Err(e) = other {
                    warn!(
                        "The record in {} can't be used ({e}); it is rebuilt from the marker",
                        label.display()
                    );
                }
                let (kind, signals, view, archive) = match owner::read_in(&*store) {
                    OwnerState::Owned(m) => (
                        DeviceKind::guess_from(&m.signals),
                        m.signals.clone(),
                        m.view,
                        m.archive_names.last().cloned(),
                    ),
                    _ => (DeviceKind::Ptp, Signals::default(), None, None),
                };
                let mut d =
                    KnownDevice::with_id(device_id.to_string(), kind, device_name, &signals, now);
                d.view = view;
                if let Some(name) = archive.filter(|n| is_plain_folder_name(n)) {
                    d.settings.archive_name = name;
                }
                d.set_destination(dest.clone());
                d
            }
        };
        info!(
            "Found the backup of \"{}\" in {}",
            device.name,
            label.display()
        );
        found.added.push(device.name.clone());
        found.added_at.push((device.id.clone(), label));
        self.devices.push(device);
    }

    /// The device whose backup place overlaps `dest` (a folder inside another's, or the same
    /// bucket prefix, and so on), if one does.
    fn overlapping(&self, dest: &Destination) -> Option<KnownDevice> {
        self.devices
            .iter()
            .find(|d| {
                d.destination()
                    .is_some_and(|o| crate::store::dest::overlaps(&o, dest))
            })
            .cloned()
    }

    /// A backup found for a device that is already in the list: the same place is known; a
    /// different place is a move, unless that is not clear (the device has been found in two
    /// places in this look, or its recorded place still holds its backup): then it is a
    /// problem line and the device stays where it is.
    fn found_known(&self, id: &str, dest: &Destination, label: PathBuf, found: &mut FoundBackups) {
        let Some(known) = self.get(id) else { return };
        let recorded = known.destination();
        let same_folder_text = matches!(
            (&recorded, dest),
            (Some(Destination::Local(a)), Destination::Local(b)) if same_path(a, b)
        );
        if same_folder_text || recorded.as_ref().is_some_and(|k| k.same_place(dest)) {
            found.already_known.push(known.name.clone());
            return;
        }
        let two_places = |a: &Path, b: &Path| {
            format!(
                "\"{}\" has backups in {} and {}; pick one with Find it…",
                known.name,
                a.display(),
                b.display()
            )
        };
        // Found earlier in this look as a new device: that place was taken, this is the second.
        if let Some((_, first)) = found.added_at.iter().find(|(i, _)| i == id) {
            found.problems.push(two_places(first, &label));
            return;
        }
        // Found earlier in this look as a move: neither place is taken.
        if let Some(i) = found.moved.iter().position(|m| m.device_id == id) {
            let first = found.moved.remove(i);
            found.problems.push(two_places(&first.found, &label));
            return;
        }
        if let Some(r) = recorded {
            let still_there = r.open().is_ok_and(
                |s| matches!(owner::read_in(&*s), OwnerState::Owned(m) if m.device_id == id),
            );
            if still_there {
                found.problems.push(two_places(&r.label(), &label));
                return;
            }
        }
        found.moved.push(MovedBackup {
            device_id: known.id.clone(),
            name: known.name.clone(),
            recorded: known.destination().map(|d| d.label()),
            found: label,
            destination: matches!(dest, Destination::S3(_)).then(|| dest.clone()),
        });
    }

    pub fn devices(&self) -> &[KnownDevice] {
        &self.devices
    }

    pub fn get(&self, id: &str) -> Option<&KnownDevice> {
        self.devices.iter().find(|d| d.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut KnownDevice> {
        self.devices.iter_mut().find(|d| d.id == id)
    }

    pub fn add(&mut self, device: KnownDevice) {
        self.devices.push(device);
    }

    /// Remove a device from the list. Its backup folder and the device itself are not touched.
    pub fn forget(&mut self, id: &str) -> Option<KnownDevice> {
        let i = self.devices.iter().position(|d| d.id == id)?;
        self.frozen.remove(id);
        Some(self.devices.remove(i))
    }

    /// "Find my backups" and "Add an existing backup" (DESIGN.md section 5.3): read the backups
    /// in `root`. If `root` itself has a marker it is the one backup; otherwise the markers one
    /// level below it are. Only reads. A device not in the list is added from its folder's
    /// record ([`crate::folder_record`]), or, for a folder made before the record existed, from
    /// the marker, with the defaults for its kind. A known device whose folder is elsewhere is
    /// reported in [`FoundBackups::moved`] and left alone: the caller decides ([`Self::rehome`]).
    pub fn find_my_backups(&mut self, root: &Path, now: &str) -> FoundBackups {
        let mut found = FoundBackups::default();
        if root.join(owner::MARKER).exists() {
            self.found_in(root, now, &mut found);
            return found;
        }
        let entries = match fs::read_dir(root) {
            Ok(e) => e,
            Err(e) => {
                found
                    .problems
                    .push(format!("could not read {}: {e}", root.display()));
                return found;
            }
        };
        let mut dirs: Vec<PathBuf> = Vec::new();
        for entry in entries {
            match entry {
                Ok(e) => {
                    // Links are not followed: a link could lead anywhere.
                    let is_plain_dir = e.metadata().is_ok_and(|m| m.is_dir() && !is_link(&m));
                    if is_plain_dir {
                        dirs.push(e.path());
                    }
                }
                Err(e) => found
                    .problems
                    .push(format!("could not read in {}: {e}", root.display())),
            }
        }
        dirs.sort();
        for dir in dirs {
            self.found_in(&dir, now, &mut found);
        }
        found
    }

    /// One folder's part of [`Self::find_my_backups`].
    fn found_in(&mut self, dir: &Path, now: &str, found: &mut FoundBackups) {
        let dir = dir.to_path_buf();
        {
            let marker = match owner::read(&dir) {
                OwnerState::Unclaimed => return,
                OwnerState::Unreadable(e) => {
                    found.problems.push(format!(
                        "{} in {} can't be read ({e})",
                        owner::MARKER,
                        dir.display()
                    ));
                    return;
                }
                OwnerState::Owned(m) => m,
            };
            if !is_sane_device_id(&marker.device_id) {
                found.problems.push(format!(
                    "{} in {} has no usable device ID",
                    owner::MARKER,
                    dir.display()
                ));
                return;
            }
            let place = Destination::Local(dir.clone());
            match self.get(&marker.device_id) {
                Some(_) => self.found_known(&marker.device_id, &place, dir.clone(), found),
                None => {
                    if let Some(other) = self.overlapping(&place) {
                        found.problems.push(overlap_problem(&dir, &other));
                        return;
                    }
                    let kind = DeviceKind::guess_from(&marker.signals);
                    let from_marker = KnownDevice::with_id(
                        marker.device_id.clone(),
                        kind,
                        &marker.device_name,
                        &marker.signals,
                        now,
                    );
                    // The folder's own record if it has one; else what the marker says. The
                    // newest archive name is the last one the marker lists. It came from a
                    // file, so only a plain single-folder name is taken.
                    let (device, _) = from_folder_or(&dir, from_marker);
                    info!(
                        "Found the backup of \"{}\" in {}",
                        device.name,
                        dir.display()
                    );
                    found.added.push(device.name.clone());
                    found.added_at.push((device.id.clone(), dir.clone()));
                    self.devices.push(device);
                }
            }
        }
    }
}

/// Why a found backup was not added: its place overlaps another device's.
fn overlap_problem(place: &Path, other: &KnownDevice) -> String {
    let at = other
        .destination()
        .map(|d| d.label().display().to_string())
        .unwrap_or_default();
    format!(
        "{} was not added: it overlaps the backup of \"{}\" ({at}). Each device needs a place of its own, with none inside another's",
        place.display(),
        other.name
    )
}

/// Before `devices.json` is written with a device as pointers that it had whole (the old
/// format), keep what is there as `devices.json.pre-0.1-move`. It is only made once, and never
/// replaced or deleted. If it can't be made, nothing is written (`Err`).
fn keep_old_file(state: &StateDir, new: &DevicesFile) -> io::Result<()> {
    let path = state.path().join(DEVICES_FILE);
    let backup = state.path().join(PRE_MOVE_BACKUP);
    let Ok(old) = fs::read(&path) else {
        return Ok(());
    };
    let whole = |v: &serde_json::Value| v.get("in_folder").is_none() && v.get("settings").is_some();
    let Ok(old_file) = serde_json::from_slice::<DevicesFile>(&old) else {
        return Ok(());
    };
    let slimmed = old_file.devices.iter().filter(|v| whole(v)).any(|v| {
        let id = v.get("id");
        new.devices
            .iter()
            .any(|n| n.get("id") == id && n.get("in_folder").is_some())
    });
    if !slimmed || backup.exists() {
        return Ok(());
    }
    crate::state::write_new_file(&backup, &old).map_err(|e| {
        io::Error::other(format!(
            "could not keep a copy of {DEVICES_FILE} as {PRE_MOVE_BACKUP} first ({e})"
        ))
    })?;
    info!("The old {DEVICES_FILE} is kept as {PRE_MOVE_BACKUP}");
    Ok(())
}

/// Move a device that `devices.json` kept whole into its backup folder `dir`, which can hold it:
/// the record is written there, and the sync history in the settings folder is merged into the
/// folder's and set aside. A record already in the folder counts instead (a crash after a move
/// began, or a folder copied from another PC). `None` if the record could not be written there:
/// the device stays as it is.
fn move_into_folder(d: &KnownDevice, dir: &Path, state: &StateDir) -> Option<KnownDevice> {
    let theirs = match folder_record::read_device(dir, &d.id) {
        Ok(f) => f,
        Err(e) => {
            warn!("The record in {} can't be used ({e})", dir.display());
            None
        }
    };
    let device = theirs.unwrap_or_else(|| d.clone());
    if let Err(e) = folder_record::write_device(dir, &device) {
        warn!(
            "Could not move the record of \"{}\" into {}: {e}; it stays here",
            d.name,
            dir.display()
        );
        return None;
    }
    let profile = History::new(state).profile_file(&d.id);
    match folder_record::copy_history_into(&profile, dir) {
        Ok(_) => folder_record::retire_profile_history(&profile),
        Err(e) => warn!(
            "Could not move the sync history of \"{}\" into {}: {e}; it stays here",
            d.name,
            dir.display()
        ),
    }
    info!(
        "The record of \"{}\" is now in {}",
        device.name,
        dir.display()
    );
    Some(device)
}

/// What loading one entry of `devices.json` did.
#[derive(Debug, Default)]
struct Loaded {
    /// Something was worked out that should be saved.
    changed: bool,
    /// The record moved into its backup folder.
    moved: bool,
}

/// The record of the device in `dir`. If the folder has none that can be used, the device as the
/// cache (or the old file) knew it, with what the marker adds. Returns whether the record is
/// worth saving into the folder.
fn from_folder_or(dir: &Path, cache: KnownDevice) -> (KnownDevice, bool) {
    match folder_record::read_device(dir, &cache.id) {
        Ok(Some(d)) => (d, false),
        Ok(None) => {
            warn!(
                "{} has no {} for \"{}\"; it is rebuilt from what is known",
                dir.display(),
                folder_record::DEVICE_FILE,
                cache.name
            );
            (rebuilt(dir, cache), true)
        }
        Err(e) => {
            warn!(
                "The record of \"{}\" in {} can't be used ({e}); it is rebuilt from what is known",
                cache.name,
                dir.display()
            );
            (rebuilt(dir, cache), true)
        }
    }
}

/// `d` with its folder set, and the signals and archive name its marker adds.
fn rebuilt(dir: &Path, mut d: KnownDevice) -> KnownDevice {
    if let OwnerState::Owned(m) = owner::read(dir) {
        d.history.learn(&m.signals);
        if d.view.is_none() {
            d.view = m.view;
        }
        if let Some(name) = m.archive_names.last()
            && is_plain_folder_name(name)
        {
            d.settings.archive_name = name.clone();
        }
    }
    d.backup_dir = Some(dir.to_path_buf());
    d
}

/// What the raw record says about a device that the typed form can't: a `deleted_files` that
/// was not understood is flagged (so the device page can say so), and a device with no
/// `jpeg_copies` key (from before the setting) gets its kind's default, which is on for an
/// iPhone, from what it has been seen to be. `raw` is the record (or, in an old
/// `devices.json`, the entry). Returns whether a setting was filled in.
pub(crate) fn fill_from_raw_one(raw: &serde_json::Value, d: &mut KnownDevice) -> bool {
    let settings = raw.get("settings");
    if deleted_files_unknown(settings.and_then(|s| s.get("deleted_files"))) {
        d.settings.deleted_files_not_understood = true;
    }
    if settings.and_then(|s| s.get("jpeg_copies")).is_none() {
        d.settings.jpeg_copies = d.kind != DeviceKind::Storage && d.history.looks_like_apple();
        return true;
    }
    false
}

/// What [`Registry::find_my_backups`] found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FoundBackups {
    /// Names of devices added to the list.
    pub added: Vec<String>,
    /// Names of devices already in the list, with the folder found.
    pub already_known: Vec<String>,
    /// Known devices whose marker was found somewhere other than the recorded folder.
    pub moved: Vec<MovedBackup>,
    /// Folders with a marker that couldn't be used, and read errors.
    pub problems: Vec<String>,
    /// The id and place of each device added in this look (to spot a second place for it).
    pub added_at: Vec<(String, PathBuf)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovedBackup {
    pub device_id: String,
    pub name: String,
    pub recorded: Option<PathBuf>,
    pub found: PathBuf,
    /// Where it was found, when that is a bucket (`found` is then its `s3://` label).
    pub destination: Option<Destination>,
}

/// Where a device's sync history is kept ([`Registry::history_home`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryHome {
    /// A file in the backup folder.
    Folder(PathBuf),
    /// A file in the bucket, through its store.
    Store(Destination),
    /// The settings folder: the device has no record in a backup place yet.
    Profile,
}

fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        p.to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_lowercase()
    };
    norm(a) == norm(b)
}

/// A device ID read from a file: short, printable, no spaces.
pub(crate) fn is_sane_device_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_graphic())
}

/// One plain folder name: no separators, no `:`, no dots-only, no control characters. The full
/// rules for archive names come with the settings step; this only keeps a marker file from
/// pointing outside the backup folder.
pub(crate) fn is_plain_folder_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.ends_with(['.', ' '])
        && !name.chars().any(|c| {
            c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: &str = "2026-11-17T18:00:00Z";

    fn serial(s: &str) -> Signals {
        Signals {
            device_serial: Some(s.into()),
            ..Signals::default()
        }
    }

    fn state() -> (tempfile::TempDir, StateDir) {
        let t = tempfile::tempdir().unwrap();
        let s = StateDir::open(t.path().join("state")).unwrap();
        (t, s)
    }

    /// A `devices.json` as an earlier version wrote it: no `deleted_files` in any device's
    /// settings.
    fn write_old_devices(state: &StateDir, devices: &[KnownDevice]) {
        let mut json: serde_json::Value = serde_json::from_slice(
            &serde_json::to_vec(&DevicesFile {
                version: 1,
                devices: devices
                    .iter()
                    .map(|d| serde_json::to_value(d).unwrap())
                    .collect(),
            })
            .unwrap(),
        )
        .unwrap();
        for d in json["devices"].as_array_mut().unwrap() {
            d["settings"]
                .as_object_mut()
                .unwrap()
                .remove("deleted_files");
        }
        fs::write(
            state.path().join(DEVICES_FILE),
            serde_json::to_vec_pretty(&json).unwrap(),
        )
        .unwrap();
    }

    fn two_devices() -> Vec<KnownDevice> {
        vec![
            KnownDevice::new(DeviceKind::Ptp, "A", &serial("A"), NOW),
            KnownDevice::new(DeviceKind::Storage, "B", &serial("B"), NOW),
        ]
    }

    /// DESIGN.md section 9: a `devices.json` from before the choice existed loads, and every
    /// device archives, as it always did.
    #[test]
    fn a_devices_file_without_the_choice_loads_as_archive() {
        let (_t, state) = state();
        write_old_devices(&state, &two_devices());
        let text = fs::read_to_string(state.path().join(DEVICES_FILE)).unwrap();
        assert!(!text.contains("deleted_files"));
        let r = Registry::load(&state);
        assert_eq!(r.devices().len(), 2);
        for d in r.devices() {
            assert_eq!(d.settings.deleted_files, DeletedFiles::Archive);
        }
        // Saved with the field, and read back.
        let mut r = r;
        r.get_mut(&r.devices()[0].id.clone())
            .unwrap()
            .settings
            .deleted_files = DeletedFiles::RecycleBin;
        r.save(&state).unwrap();
        assert_eq!(
            Registry::load(&state).devices()[0].settings.deleted_files,
            DeletedFiles::RecycleBin
        );
    }

    /// An old `config.json` with `archive_deleted: false` (archiving turned off for everything)
    /// makes every existing device leave deleted files in place, once; then the old key is gone
    /// and nothing is written for it again.
    #[test]
    fn the_old_global_off_switch_becomes_leave_in_place_for_existing_devices_once() {
        let (_t, state) = state();
        let mut devices = two_devices();
        write_old_devices(&state, &devices);
        fs::write(
            state.path().join("config.json"),
            br#"{ "backup_dir": "C:\\B", "archive_deleted": false }"#,
        )
        .unwrap();
        // Whichever file is read first does the migration.
        let config = state.config();
        assert_eq!(config.backup_dir, Some(PathBuf::from(r"C:\B")));
        assert_eq!(
            config.new_devices.deleted_files,
            DeletedFiles::Archive,
            "new devices still start with the default"
        );
        let text = fs::read_to_string(state.path().join("config.json")).unwrap();
        assert!(!text.contains("archive_deleted"), "{text}");
        assert!(text.contains("backup_dir"), "{text}");
        let mut r = Registry::load(&state);
        for d in r.devices() {
            assert_eq!(
                d.settings.deleted_files,
                DeletedFiles::LeaveInPlace,
                "{}",
                d.name
            );
        }
        // Once: a device that later chooses something else keeps it.
        let id = r.devices()[0].id.clone();
        r.get_mut(&id).unwrap().settings.deleted_files = DeletedFiles::Archive;
        r.save(&state).unwrap();
        assert_eq!(
            Registry::load(&state)
                .get(&id)
                .unwrap()
                .settings
                .deleted_files,
            DeletedFiles::Archive
        );
        // Saving the settings never writes the old key.
        state.save_config(&state.config()).unwrap();
        assert!(
            !fs::read_to_string(state.path().join("config.json"))
                .unwrap()
                .contains("archive_deleted")
        );

        // The registry read first does it too, and a device that already has a choice keeps it.
        let (_t, state) = state_with_devices(&mut devices);
        let r = Registry::load(&state);
        assert_eq!(
            r.devices()[0].settings.deleted_files,
            DeletedFiles::RecycleBin
        );
        assert_eq!(
            r.devices()[1].settings.deleted_files,
            DeletedFiles::LeaveInPlace
        );
        assert!(
            !fs::read_to_string(state.path().join("config.json"))
                .unwrap()
                .contains("archive_deleted")
        );
    }

    /// Two devices where the first already has its own choice written and the second doesn't,
    /// and a `config.json` with archiving off.
    fn state_with_devices(devices: &mut [KnownDevice]) -> (tempfile::TempDir, StateDir) {
        let (t, state) = state();
        devices[0].settings.deleted_files = DeletedFiles::RecycleBin;
        let r = Registry::new(devices.to_vec());
        r.save(&state).unwrap();
        // Take the field out of the second device only.
        let mut json: serde_json::Value =
            serde_json::from_slice(&fs::read(state.path().join(DEVICES_FILE)).unwrap()).unwrap();
        json["devices"][1]["settings"]
            .as_object_mut()
            .unwrap()
            .remove("deleted_files");
        fs::write(
            state.path().join(DEVICES_FILE),
            serde_json::to_vec(&json).unwrap(),
        )
        .unwrap();
        fs::write(
            state.path().join("config.json"),
            br#"{ "archive_deleted": false }"#,
        )
        .unwrap();
        (t, state)
    }

    /// `archive_deleted: true` (the old default) changes nothing for any device.
    #[test]
    fn the_old_global_on_switch_changes_nothing() {
        let (_t, state) = state();
        write_old_devices(&state, &two_devices());
        fs::write(
            state.path().join("config.json"),
            br#"{ "archive_deleted": true }"#,
        )
        .unwrap();
        let r = Registry::load(&state);
        for d in r.devices() {
            assert_eq!(d.settings.deleted_files, DeletedFiles::Archive);
        }
        let _ = state.config();
        assert!(
            !fs::read_to_string(state.path().join("config.json"))
                .unwrap()
                .contains("archive_deleted")
        );
    }

    /// If `devices.json` can't be rewritten, the old key stays, so the next start tries again.
    #[test]
    #[cfg(unix)]
    fn the_old_key_stays_if_the_devices_cannot_be_updated() {
        use std::os::unix::fs::PermissionsExt;
        let (_t, state) = state();
        write_old_devices(&state, &two_devices());
        fs::write(
            state.path().join("config.json"),
            br#"{ "archive_deleted": false }"#,
        )
        .unwrap();
        // The folder can't take the temporary file the atomic write needs.
        fs::set_permissions(state.path(), fs::Permissions::from_mode(0o500)).unwrap();
        let writable = fs::File::create(state.path().join("probe")).is_ok();
        if writable {
            // Running as root, which ignores folder permissions: the failure can't be made.
            fs::set_permissions(state.path(), fs::Permissions::from_mode(0o700)).unwrap();
            eprintln!("SKIPPED: this folder stays writable here (running as root?)");
            return;
        }
        let _ = state.config();
        fs::set_permissions(state.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let text = fs::read_to_string(state.path().join("config.json")).unwrap();
        assert!(text.contains("archive_deleted"), "{text}");
    }

    /// A value this version doesn't know (a newer version's, or a typo) gives that field's
    /// default and leaves every other setting and device alone.
    #[test]
    fn an_unknown_setting_value_does_not_make_devices_json_unreadable() {
        let (_t, state) = state();
        let mut devices = two_devices();
        devices[0].settings.deleted_files = DeletedFiles::RecycleBin;
        devices[0].settings.archive_name = "Kept".into();
        Registry::new(devices).save(&state).unwrap();
        let mut json: serde_json::Value =
            serde_json::from_slice(&fs::read(state.path().join(DEVICES_FILE)).unwrap()).unwrap();
        json["devices"][0]["settings"]["deleted_files"] = "shred_it".into();
        json["devices"][1]["settings"]["archive_layout"] = 7.into();
        json["devices"][1]["settings"]["storage_layout"] = "somewhere".into();
        fs::write(
            state.path().join(DEVICES_FILE),
            serde_json::to_vec(&json).unwrap(),
        )
        .unwrap();
        let r = Registry::load(&state);
        assert_eq!(r.devices().len(), 2);
        let (a, b) = (&r.devices()[0].settings, &r.devices()[1].settings);
        // The one choice that never moves a deleted file, not the default that would.
        assert_eq!(a.deleted_files, DeletedFiles::LeaveInPlace);
        assert!(a.deleted_files_not_understood);
        assert!(!b.deleted_files_not_understood);
        assert_eq!(a.archive_name, "Kept");
        assert_eq!(b.archive_layout, ArchiveLayout::KeepFolders);
        assert_eq!(b.storage_layout, StorageLayout::Unchosen);
    }

    #[test]
    fn new_devices_get_random_distinct_ids() {
        let a = KnownDevice::new(DeviceKind::Ptp, "A", &Signals::default(), NOW);
        let b = KnownDevice::new(DeviceKind::Ptp, "B", &Signals::default(), NOW);
        assert_ne!(a.id, b.id);
        assert_eq!(a.id.len(), 36, "{}", a.id);
    }

    #[test]
    fn settings_default_by_kind() {
        let phone = DeviceSettings::defaults_for(DeviceKind::Ptp);
        assert_eq!(phone.archive_name, "_Archive");
        assert_eq!(phone.archive_layout, ArchiveLayout::KeepFolders);
        assert_eq!(phone.folders, ["DCIM", "Pictures", "Movies"]);
        assert_eq!(phone.file_types, FileTypes::PhotosAndVideos);
        assert!(phone.always_left_out.is_empty());
        assert!(!phone.scan_automatically);
        assert!(!phone.compare_by_date);
        assert_eq!(phone.storage_layout, StorageLayout::Unchosen);
        assert_eq!(phone, DeviceSettings::defaults_for(DeviceKind::Mtp));
        assert_eq!(phone, DeviceSettings::defaults_for(DeviceKind::Camera));
        assert!(phone.media_extensions.iter().any(|e| e == "heic"));
        assert!(phone.media_extensions.iter().any(|e| e == "mp4"));
        assert_eq!(
            phone.ignore_names,
            ["desktop.ini", "Thumbs.db", ".DS_Store"]
        );
        assert_eq!(phone.top_level_storage, None);
        assert!(phone.storage_folders.is_empty());

        let stick = DeviceSettings::defaults_for(DeviceKind::Storage);
        assert!(stick.folders.is_empty());
        assert_eq!(stick.file_types, FileTypes::All);
        assert!(stick.compare_by_date);
        assert!(!stick.scan_automatically);
        for name in [
            ".plug-n-keep-id",
            "System Volume Information",
            "$RECYCLE.BIN",
            ".Trashes",
            ".Spotlight-V100",
            ".fseventsd",
            "FOUND.000",
        ] {
            assert!(stick.always_left_out.iter().any(|n| n == name), "{name}");
        }
    }

    #[test]
    fn a_device_file_with_missing_settings_loads_with_defaults() {
        let s: DeviceSettings = serde_json::from_str(r#"{"scan_automatically": true}"#).unwrap();
        assert!(s.scan_automatically);
        assert_eq!(s.archive_name, "_Archive");
    }

    #[test]
    fn history_keeps_every_value_and_learns_new_ones() {
        let mut h = SignalHistory::from_signals(&Signals {
            volume_serial: Some("AAAA-1111".into()),
            ..Signals::default()
        });
        let added = h.learn(&Signals {
            volume_serial: Some("BBBB-2222".into()),
            ..Signals::default()
        });
        assert_eq!(added, [SignalKind::VolumeSerial]);
        assert!(h.contains(SignalKind::VolumeSerial, "aaaa-1111"));
        assert!(h.contains(SignalKind::VolumeSerial, "bbbb-2222"));
        // Learning the same value again adds nothing.
        assert!(
            h.learn(&Signals {
                volume_serial: Some("BBBB-2222".into()),
                ..Signals::default()
            })
            .is_empty()
        );
        assert_eq!(h.values(SignalKind::VolumeSerial).len(), 2);
    }

    #[test]
    fn history_is_capped() {
        let mut h = SignalHistory::default();
        for i in 0..(HISTORY_LIMIT + 5) {
            h.learn(&Signals {
                capacity: Some(i as u64),
                ..Signals::default()
            });
        }
        assert_eq!(h.values(SignalKind::Capacity).len(), HISTORY_LIMIT);
        assert!(!h.contains(SignalKind::Capacity, "0"));
    }

    #[test]
    fn the_registry_round_trips_through_devices_json() {
        let (t, state) = state();
        let mut d = KnownDevice::new(DeviceKind::Storage, "Work stick", &serial("ABC12345"), NOW);
        // No folder yet: the whole record is in devices.json.
        d.backup_dir = Some(t.path().join("not made yet"));
        d.reject(&serial("OTHER123"));
        d.settings.scan_automatically = true;
        let mut r = Registry::default();
        r.add(d.clone());
        r.save(&state).unwrap();
        // No temporary file is left behind.
        assert!(!state.path().join("devices.json.tmp").exists());
        let loaded = Registry::load(&state);
        assert_eq!(loaded.devices(), [d.clone()]);
        assert_eq!(loaded.get(&d.id), Some(&d));
    }

    #[test]
    fn a_missing_file_is_an_empty_list() {
        let (_t, state) = state();
        let r = Registry::load(&state);
        assert!(r.devices().is_empty());
        // And it can be saved.
        r.save(&state).unwrap();
    }

    #[test]
    fn a_damaged_file_gives_an_empty_list_and_is_kept_aside() {
        let (_t, state) = state();
        let file = state.path().join("devices.json");
        fs::write(&file, b"{ this is not json").unwrap();
        let r = Registry::load(&state);
        assert!(r.devices().is_empty());
        assert!(!file.exists());
        let aside = state.path().join("devices.json.damaged");
        assert_eq!(fs::read(&aside).unwrap(), b"{ this is not json");
        // Saving now does not touch the kept file.
        r.save(&state).unwrap();
        assert_eq!(fs::read(&aside).unwrap(), b"{ this is not json");
        // A second damaged file does not replace the first.
        fs::write(&file, b"second").unwrap();
        let _ = Registry::load(&state);
        assert_eq!(fs::read(&aside).unwrap(), b"{ this is not json");
        assert_eq!(
            fs::read(state.path().join("devices.json.damaged2")).unwrap(),
            b"second"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_file_that_cannot_be_moved_aside_is_never_overwritten() {
        let (_t, state) = state();
        let file = state.path().join("devices.json");
        fs::write(&file, b"garbage").unwrap();
        // Hold the file open without sharing, so it can be neither read nor moved.
        use std::os::windows::fs::OpenOptionsExt;
        let _held = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&file)
            .unwrap();
        let r = Registry::load(&state);
        assert!(r.devices().is_empty());
        assert!(r.save(&state).is_err());
        drop(_held);
        assert_eq!(fs::read(&file).unwrap(), b"garbage");
    }

    #[cfg(unix)]
    #[test]
    fn a_file_that_cannot_be_moved_aside_is_never_overwritten_on_unix() {
        let (_t, state) = state();
        let file = state.path().join("devices.json");
        fs::write(&file, b"garbage").unwrap();
        // A folder nobody can write to: the file can be read but not moved aside. The guard
        // makes it writable again however the test ends.
        let locked = crate::testutil::ReadOnlyDir::new(state.path());
        if !locked.is_enforced() {
            eprintln!("SKIPPED: running as root, which ignores folder permissions");
            return;
        }
        let r = Registry::load(&state);
        assert!(r.devices().is_empty());
        assert!(r.save(&state).is_err());
        drop(locked);
        assert_eq!(fs::read(&file).unwrap(), b"garbage");
    }
    #[test]
    fn forget_removes_only_the_entry() {
        let mut r = Registry::default();
        let d = KnownDevice::new(DeviceKind::Ptp, "A", &Signals::default(), NOW);
        let id = d.id.clone();
        r.add(d);
        assert!(r.forget(&id).is_some());
        assert!(r.forget(&id).is_none());
        assert!(r.devices().is_empty());
    }

    fn write_marker(dir: &Path, id: &str, name: &str, signals: Signals, archives: &[&str]) {
        fs::create_dir_all(dir).unwrap();
        let mut m = owner::FolderOwner::new(id, name, signals, View::Files, NOW);
        m.archive_names = archives.iter().map(|s| s.to_string()).collect();
        owner::write(&fs::canonicalize(dir).unwrap(), &m).unwrap();
    }

    #[test]
    fn find_my_backups_rebuilds_entries_one_level_down() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path();
        write_marker(
            &root.join("My phone"),
            "id-phone",
            "My phone",
            Signals {
                device_serial: Some("00008030000A1234".into()),
                maker: Some("Apple Inc.".into()),
                ..Signals::default()
            },
            &["_Archive", "_Deleted from phone"],
        );
        write_marker(
            &root.join("Stick"),
            "id-stick",
            "Stick",
            Signals {
                volume_serial: Some("1A2B-3C4D".into()),
                ..Signals::default()
            },
            &["_Archive"],
        );
        // Two levels down is not looked at; neither is a folder without a marker.
        write_marker(
            &root.join("deeper").join("inner"),
            "id-deep",
            "Deep",
            Signals::default(),
            &["_Archive"],
        );
        fs::create_dir_all(root.join("plain")).unwrap();
        fs::write(root.join("a file"), b"x").unwrap();

        let mut reg = Registry::default();
        let found = reg.find_my_backups(root, NOW);
        let mut added = found.added.clone();
        added.sort();
        assert_eq!(added, ["My phone", "Stick"]);
        assert!(found.problems.is_empty(), "{:?}", found.problems);
        let phone = reg.get("id-phone").unwrap();
        assert_eq!(phone.kind, DeviceKind::Ptp);
        assert_eq!(phone.settings.archive_name, "_Deleted from phone");
        assert_eq!(
            phone.backup_dir.as_deref(),
            Some(root.join("My phone").as_path())
        );
        assert!(
            phone
                .history
                .contains(SignalKind::DeviceSerial, "00008030000a1234")
        );
        assert_eq!(reg.get("id-stick").unwrap().kind, DeviceKind::Storage);
        assert!(reg.get("id-deep").is_none());

        // Running it again finds nothing new.
        let again = reg.find_my_backups(root, NOW);
        assert!(again.added.is_empty() && again.moved.is_empty());
        assert_eq!(again.already_known.len(), 2);
        assert_eq!(reg.devices().len(), 2);
    }

    #[test]
    fn find_my_backups_reports_a_moved_folder_and_leaves_the_entry() {
        let t = tempfile::tempdir().unwrap();
        write_marker(
            &t.path().join("New place"),
            "id-1",
            "Phone",
            Signals::default(),
            &["_Archive"],
        );
        let mut d = KnownDevice::with_id(
            "id-1".into(),
            DeviceKind::Ptp,
            "Phone",
            &Signals::default(),
            NOW,
        );
        d.backup_dir = Some(PathBuf::from(r"Z:\Old place"));
        let mut reg = Registry::new(vec![d]);
        let found = reg.find_my_backups(t.path(), NOW);
        assert_eq!(found.moved.len(), 1);
        assert_eq!(found.moved[0].device_id, "id-1");
        assert_eq!(found.moved[0].found, t.path().join("New place"));
        assert_eq!(
            reg.get("id-1").unwrap().backup_dir.as_deref(),
            Some(Path::new(r"Z:\Old place"))
        );
    }

    #[test]
    fn find_my_backups_reports_a_broken_marker_and_a_hostile_archive_name() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join("bad")).unwrap();
        fs::write(t.path().join("bad").join(owner::MARKER), b"not json").unwrap();
        write_marker(
            &t.path().join("sneaky"),
            "id-s",
            "Sneaky",
            Signals::default(),
            &["_Archive", r"..\..\Windows"],
        );
        let mut reg = Registry::default();
        let found = reg.find_my_backups(t.path(), NOW);
        assert_eq!(found.problems.len(), 1, "{:?}", found.problems);
        // The odd archive name was not taken into the settings.
        assert_eq!(reg.get("id-s").unwrap().settings.archive_name, "_Archive");
    }

    #[test]
    fn find_my_backups_on_a_missing_root_is_a_problem_not_a_crash() {
        let t = tempfile::tempdir().unwrap();
        let mut reg = Registry::default();
        let found = reg.find_my_backups(&t.path().join("nope"), NOW);
        assert_eq!(found.problems.len(), 1);
    }

    fn bucket_place(prefix: &str) -> Destination {
        Destination::S3(S3Config {
            provider: Default::default(),
            account_id: None,
            endpoint: "http://s3.example.test:9000".into(),
            region: "us-east-1".into(),
            bucket: "b".into(),
            prefix: prefix.into(),
            profile: None,
            credentials_file: None,
            path_style: true,
            ca_file: None,
        })
    }

    fn device_at(id: &str, name: &str, dest: Destination) -> KnownDevice {
        let mut d =
            KnownDevice::with_id(id.into(), DeviceKind::Ptp, name, &Signals::default(), NOW);
        d.set_destination(dest);
        d
    }

    #[test]
    fn a_found_folder_inside_another_devices_folder_is_not_added() {
        let t = tempfile::tempdir().unwrap();
        let outer = t.path().join("outer");
        write_marker(&outer, "id-1", "Outer", Signals::default(), &[]);
        write_marker(
            &outer.join("inner"),
            "id-2",
            "Inner",
            Signals::default(),
            &[],
        );
        let mut reg = Registry::new(vec![device_at(
            "id-1",
            "Outer",
            Destination::Local(outer.clone()),
        )]);
        let found = reg.find_my_backups(&outer.join("inner"), NOW);
        assert!(found.added.is_empty(), "{found:?}");
        assert_eq!(found.problems.len(), 1);
        assert!(found.problems[0].contains("overlaps"), "{found:?}");
        assert_eq!(reg.devices().len(), 1);
    }

    #[test]
    fn a_found_bucket_place_that_overlaps_another_device_is_not_added() {
        let mut reg = Registry::new(vec![device_at("id-1", "Phone", bucket_place("phones/a"))]);
        let mut found = FoundBackups::default();
        // Inside the other's prefix, and the same prefix: both refused, before anything is opened.
        for prefix in ["phones/a/b", "phones/a", "phones"] {
            reg.found_in_store(&bucket_place(prefix), "id-2", "Other", NOW, &mut found);
        }
        assert!(found.added.is_empty(), "{found:?}");
        assert_eq!(found.problems.len(), 3, "{found:?}");
        assert!(found.problems.iter().all(|p| p.contains("overlaps")));
        assert_eq!(reg.devices().len(), 1);
    }

    #[test]
    fn a_device_found_in_two_places_is_not_moved_to_either() {
        let t = tempfile::tempdir().unwrap();
        write_marker(
            &t.path().join("A"),
            "id-1",
            "Phone",
            Signals::default(),
            &[],
        );
        write_marker(
            &t.path().join("B"),
            "id-1",
            "Phone",
            Signals::default(),
            &[],
        );
        let mut known = device_at(
            "id-1",
            "Phone",
            Destination::Local(PathBuf::from(r"Z:\Gone")),
        );
        known.backup_dir = Some(t.path().join("nowhere"));
        let mut reg = Registry::new(vec![known]);
        let found = reg.find_my_backups(t.path(), NOW);
        assert!(found.moved.is_empty(), "{found:?}");
        assert_eq!(found.problems.len(), 1, "{found:?}");
        assert!(found.problems[0].contains("has backups in"));
        assert!(found.problems[0].contains("Find it"));
        assert_eq!(
            reg.get("id-1").unwrap().backup_dir.as_deref(),
            Some(t.path().join("nowhere").as_path())
        );
    }

    #[test]
    fn a_new_device_found_in_two_places_keeps_the_first_and_reports_the_second() {
        let t = tempfile::tempdir().unwrap();
        write_marker(
            &t.path().join("A"),
            "id-1",
            "Phone",
            Signals::default(),
            &[],
        );
        write_marker(
            &t.path().join("B"),
            "id-1",
            "Phone",
            Signals::default(),
            &[],
        );
        let mut reg = Registry::default();
        let found = reg.find_my_backups(t.path(), NOW);
        assert_eq!(found.added.len(), 1, "{found:?}");
        assert!(found.moved.is_empty());
        assert_eq!(found.problems.len(), 1, "{found:?}");
    }

    #[test]
    fn a_found_place_for_a_device_whose_recorded_place_still_has_its_backup_is_a_problem() {
        let t = tempfile::tempdir().unwrap();
        write_marker(
            &t.path().join("A"),
            "id-1",
            "Phone",
            Signals::default(),
            &[],
        );
        write_marker(
            &t.path().join("B"),
            "id-1",
            "Phone",
            Signals::default(),
            &[],
        );
        let mut reg = Registry::new(vec![device_at(
            "id-1",
            "Phone",
            Destination::Local(t.path().join("A")),
        )]);
        let found = reg.find_my_backups(&t.path().join("B"), NOW);
        assert!(found.moved.is_empty(), "{found:?}");
        assert_eq!(found.problems.len(), 1, "{found:?}");
    }

    #[test]
    fn two_bucket_places_with_one_device_id_are_not_applied() {
        let mut reg = Registry::new(vec![device_at(
            "id-1",
            "Phone",
            Destination::Local(PathBuf::from(r"Z:\Gone")),
        )]);
        let mut found = FoundBackups::default();
        reg.found_in_store(&bucket_place("x/a"), "id-1", "Phone", NOW, &mut found);
        assert_eq!(found.moved.len(), 1);
        reg.found_in_store(&bucket_place("x/b"), "id-1", "Phone", NOW, &mut found);
        assert!(found.moved.is_empty(), "{found:?}");
        assert_eq!(found.problems.len(), 1);
    }

    #[test]
    fn plain_folder_names() {
        assert!(is_plain_folder_name("_Deleted from phone"));
        for bad in ["", ".", "..", "a/b", r"a\b", "a:b", "x.", "x ", "a*b"] {
            assert!(!is_plain_folder_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn jpg_copies_are_on_for_an_iphone_and_off_for_everything_else() {
        let iphone = Signals {
            maker: Some("Apple Inc.".into()),
            model: Some("iPhone".into()),
            ..Signals::default()
        };
        let android = Signals {
            maker: Some("Google".into()),
            ..Signals::default()
        };
        assert!(DeviceSettings::defaults_for_device(DeviceKind::Ptp, &iphone).jpeg_copies);
        assert!(
            KnownDevice::new(DeviceKind::Ptp, "P", &iphone, NOW)
                .settings
                .jpeg_copies
        );
        assert!(!DeviceSettings::defaults_for_device(DeviceKind::Mtp, &android).jpeg_copies);
        assert!(!DeviceSettings::defaults_for_device(DeviceKind::Storage, &iphone).jpeg_copies);
        assert!(!DeviceSettings::default().jpeg_copies);
        // Apple's USB vendor ID counts too.
        let by_usb = Signals {
            usb_vendor: Some("05AC".into()),
            ..Signals::default()
        };
        assert!(DeviceSettings::defaults_for_device(DeviceKind::Ptp, &by_usb).jpeg_copies);
        // A settings file from before the setting existed loads with it off ...
        let s: DeviceSettings = serde_json::from_str(r#"{"scan_automatically": true}"#).unwrap();
        assert!(!s.jpeg_copies);
    }

    /// ... but a known device in `devices.json` with no `jpeg_copies` key gets its kind's
    /// default (on for an iPhone), which is then saved as an explicit value. A key that is there
    /// is kept, whatever it says.
    #[test]
    fn a_device_from_before_jpg_copies_gets_the_kinds_default_and_keeps_an_explicit_choice() {
        let (_t, state) = state();
        let iphone = Signals {
            maker: Some("Apple Inc.".into()),
            ..Signals::default()
        };
        let android = Signals {
            maker: Some("Google".into()),
            ..Signals::default()
        };
        let mut explicit_off = KnownDevice::new(DeviceKind::Ptp, "Chosen", &iphone, NOW);
        explicit_off.settings.jpeg_copies = false;
        Registry::new(vec![
            KnownDevice::new(DeviceKind::Ptp, "Phone", &iphone, NOW),
            KnownDevice::new(DeviceKind::Mtp, "Other", &android, NOW),
            explicit_off,
        ])
        .save(&state)
        .unwrap();
        let path = state.path().join(DEVICES_FILE);
        let mut json: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        for i in [0, 1] {
            json["devices"][i]["settings"]
                .as_object_mut()
                .unwrap()
                .remove("jpeg_copies");
        }
        fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        let r = Registry::load(&state);
        assert!(r.devices()[0].settings.jpeg_copies);
        assert!(!r.devices()[1].settings.jpeg_copies);
        assert!(
            !r.devices()[2].settings.jpeg_copies,
            "an explicit off stays off"
        );
        // Written back as explicit values.
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("\"jpeg_copies\"").count(), 3, "{text}");
    }
}
