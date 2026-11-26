//! The app's own small files in its app-data folder: skip lists, settings, the record of the copy
//! in progress (for crash recovery) and the single-instance lock.
//!
//! Not thread-safe: one owner (the GUI's worker thread) should do all reads and writes.
#![forbid(unsafe_code)]

use std::collections::{BTreeSet, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use log::{debug, info, warn};
use serde::{Deserialize, Deserializer, Serialize};

use crate::owner::OwnerState;
use crate::paths::{SafeRelPath, TEMP_SUFFIX, is_link};
use crate::registry::{
    ArchiveLayout, CLUTTER_NAMES, DEFAULT_ARCHIVE_NAME, DeletedFiles, DeviceSettings,
};
use crate::signals::{DeviceKind, Signals};
use crate::store::{BackupStore, Destination, Kind as StoreKind, LocalStore, S3Config, Watched};

/// The settings folder's name, inside the system's settings location.
pub const APP_DIR_NAME: &str = "plug-n-keep";
const SKIP_FILE: &str = "skip_lists.json";
const IN_PROGRESS_FILE: &str = "in_progress.json";
const INSTANCE_FILE: &str = "instance.lock";
const CONFIG_FILE: &str = "config.json";
/// An earlier version kept what new devices start with in this file. It is read once, into
/// [`Config::new_devices`], and not used again.
const OLD_DEFAULTS_FILE: &str = "defaults.json";

/// The settings folder: `%APPDATA%\plug-n-keep` on Windows, else `$XDG_CONFIG_HOME/plug-n-keep`
/// or `~/.config/plug-n-keep`.
pub fn default_state_dir() -> Option<PathBuf> {
    crate::platform::settings_dir(APP_DIR_NAME)
}

/// A skip list belongs to one device and one backup folder; skipping a file in one folder must not hide it from another.
///
/// It is kept in the backup folder (`.plug-n-keep-skip.json`, [`SKIP_NAME`]) when the folder is
/// there and isn't another device's, so it travels with the backup; until then, in the settings
/// folder, and it moves into the folder the first time the folder is used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    device: String,
    root: String,
    dir: PathBuf,
}

/// The skip list in a backup folder.
pub const SKIP_NAME: &str = ".plug-n-keep-skip.json";

impl Scope {
    pub fn new(device_key: &str, root: &Path) -> Self {
        let abs = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
        let root = abs
            .to_string_lossy()
            .trim_end_matches(['\\', '/'])
            .to_lowercase();
        Self {
            device: device_key.to_string(),
            root,
            dir: abs,
        }
    }
}

impl Scope {
    /// The scope for a backup that is not a folder: `label` (`s3://bucket/prefix`) is used as it
    /// is, never taken for a path.
    pub fn labelled(device_key: &str, label: &str) -> Self {
        Self {
            device: device_key.to_string(),
            root: label.trim_end_matches('/').to_lowercase(),
            dir: PathBuf::from(label),
        }
    }

    /// The scope of a device's backup in `store`: by the folder's path for a folder, by the
    /// label (`root` as given) for any other store.
    pub fn in_store(device_key: &str, store: &dyn BackupStore, root: &Path) -> Self {
        if store.as_local().is_some() {
            Self::new(device_key, root)
        } else {
            Self::labelled(device_key, &root.to_string_lossy())
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} in {}", self.device, self.root)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SkipFile {
    version: u32,
    scopes: Vec<ScopeEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ScopeEntry {
    device: String,
    root: String,
    paths: BTreeSet<String>,
}

fn skip_rel() -> SafeRelPath {
    SafeRelPath::parse(SKIP_NAME).expect("the skip-list name is a safe name")
}

/// A skip file's text, or the error for a damaged one (which is left as it is).
fn parse_skip_file(name: &str, bytes: &[u8]) -> io::Result<SkipFile> {
    serde_json::from_slice(bytes).map_err(|e| {
        warn!("{name} is damaged ({e}); it is left as it is");
        io::Error::other(format!("{name} is damaged ({e}); not changing it"))
    })
}

impl SkipFile {
    fn empty() -> Self {
        Self {
            version: 2,
            scopes: Vec::new(),
        }
    }

    /// The entry for `scope`: in a backup folder's file the device alone names it (the folder's
    /// path is wherever it is now), in the settings folder's the folder too.
    fn entry_for(&mut self, scope: &Scope, in_folder: bool) -> &mut ScopeEntry {
        if in_folder {
            self.entry_in_folder(scope)
        } else {
            self.entry_mut(scope)
        }
    }

    fn entry_in_folder(&mut self, scope: &Scope) -> &mut ScopeEntry {
        if let Some(i) = self.scopes.iter().position(|e| e.device == scope.device) {
            return &mut self.scopes[i];
        }
        self.scopes.push(ScopeEntry {
            device: scope.device.clone(),
            root: scope.root.clone(),
            paths: BTreeSet::new(),
        });
        self.scopes.last_mut().expect("just pushed")
    }

    fn entry_mut(&mut self, scope: &Scope) -> &mut ScopeEntry {
        if let Some(i) = self
            .scopes
            .iter()
            .position(|e| e.device == scope.device && e.root == scope.root)
        {
            return &mut self.scopes[i];
        }
        self.scopes.push(ScopeEntry {
            device: scope.device.clone(),
            root: scope.root.clone(),
            paths: BTreeSet::new(),
        });
        self.scopes.last_mut().expect("just pushed")
    }
}

/// The copy in progress, written before each file and removed after it. If the app dies
/// mid-copy, the next start finds it and cleans up.
///
/// The paths in a record are store-relative: `/`-separated, under `root` (the backup's folder
/// path, which recovery checks against the folders the app holds for the device). `temp` is the
/// upload's reference ([`crate::store::Upload::journal_ref`]): for a folder, the temp file's path
/// under the root. A record written by an earlier release holds absolute paths instead; recovery
/// cuts those down to the part under `root`, and acts only if they are inside it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InProgress {
    pub device: String,
    pub root: PathBuf,
    /// Device path of the file being copied.
    pub source: String,
    /// The unfinished upload's reference (the temporary file, relative to the root).
    pub temp: PathBuf,
    /// Set once the new copy is whole and the changed-file moves may begin ([`ReplaceStep`]).
    #[serde(default)]
    pub replace: Option<ReplaceStep>,
    /// Where the backup is when it is not a folder (`root` is then only its label). Absent for a
    /// folder, and in records from before it: `root` is the folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<Destination>,
}

impl InProgress {
    /// The backup this record is about.
    pub fn place(&self) -> Destination {
        self.destination
            .clone()
            .unwrap_or_else(|| Destination::Local(self.root.clone()))
    }
}

/// Where a changed file's old copy goes (DESIGN.md section 7.3), from the device's
/// [`DeletedFiles`] choice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OldCopyGoes {
    /// Into the archive folder.
    #[default]
    Archive,
    /// Renamed beside the new copy, in the same folder.
    Aside,
    /// Into the Recycle Bin or Trash. Recovery never needs it back.
    Bin,
}

/// The moves that replace a changed file (DESIGN.md section 7.3): the old copy out of the way
/// (into the archive, renamed beside the new one, or into the bin), then the new copy into the
/// old one's place. This is written, replacing the record, after the new copy is whole and
/// flushed and before the first move, so a crash at any later point can be finished: the old
/// copy is at its place, at its new place or in the bin, and the new copy is the temporary file
/// or in place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplaceStep {
    /// Where the file is in the backup (the old copy, then the new one), relative to the root.
    pub original: PathBuf,
    /// Where the old copy goes: in the archive, or beside the original, relative to the root.
    /// Empty for the bin.
    #[serde(default)]
    pub archived: PathBuf,
    /// What the old copy's move is. A record from before this field is an archive move.
    #[serde(default)]
    pub goes: OldCopyGoes,
    /// Set (to the new copy's size) when the new copy had been staged
    /// ([`crate::store::Upload::stage`]) before the old copy moved: an object store then holds
    /// all of it as an unfinished upload, which recovery can finish
    /// ([`BackupStore::finish_by_ref`]). Absent when it was not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub staged_size: Option<u64>,
}

impl InProgress {
    /// The record of a lone move of an old copy (archiving a file that left the device, on an
    /// object store, where a move is a copy and then a delete): a replacement record with no
    /// upload, so `temp` is empty.
    pub fn is_move_only(&self) -> bool {
        self.replace.is_some() && self.temp.as_os_str().is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interrupted {
    pub record: Option<InProgress>,
    pub temp_removed: bool,
    /// An interrupted changed-file replacement was finished: the old copy is in the archive
    /// and the new one in place.
    pub completed: bool,
    pub note: String,
    /// The backup could not be reached, so the record was kept and the next start tries again.
    pub retry_later: bool,
}

/// The app's settings, kept in `config.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub backup_dir: Option<PathBuf>,
    /// The default place in a bucket for new devices (`prefix` is the base each device's own
    /// folder goes under), next to the default backup folder. No keys, only the profile name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_s3: Option<S3Config>,
    /// What the Log panel records. A file from before the setting, or with a value this version
    /// doesn't know, gives the default instead of failing to load.
    #[serde(default, deserialize_with = "lenient_level")]
    pub log_level: LogLevel,
    /// What a new device starts with.
    #[serde(default)]
    pub new_devices: NewDeviceDefaults,
}

/// What a new device starts with, on top of what its kind decides (DESIGN.md section 9). Any
/// field a file leaves out has its default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NewDeviceDefaults {
    /// The archive folder's name for a new device.
    pub archive_name: String,
    #[serde(deserialize_with = "crate::registry::lenient")]
    pub archive_layout: ArchiveLayout,
    /// Where files deleted from a new device go (and the old copy of a changed one).
    #[serde(deserialize_with = "crate::registry::deleted_files_lenient")]
    pub deleted_files: DeletedFiles,
    /// Set at load when the file's `deleted_files` was not understood (so it became
    /// `LeaveInPlace`); the settings page then says so. Not stored.
    #[serde(skip)]
    pub deleted_files_not_understood: bool,
    /// Whether a new device starts a scan when it is plugged in.
    pub scan_automatically: bool,
    /// The ignore list a new device starts with.
    pub ignore_names: Vec<String>,
}

impl Default for NewDeviceDefaults {
    fn default() -> Self {
        Self {
            archive_name: DEFAULT_ARCHIVE_NAME.to_string(),
            archive_layout: ArchiveLayout::KeepFolders,
            deleted_files: DeletedFiles::Archive,
            deleted_files_not_understood: false,
            scan_automatically: false,
            ignore_names: CLUTTER_NAMES.map(String::from).to_vec(),
        }
    }
}

impl NewDeviceDefaults {
    /// The settings of a new device of this kind: the kind's defaults, with these on top.
    pub fn settings_for(&self, kind: DeviceKind, signals: &Signals) -> DeviceSettings {
        let mut s = DeviceSettings::defaults_for_device(kind, signals);
        s.archive_name = self.archive_name.clone();
        s.archive_layout = self.archive_layout;
        s.deleted_files = self.deleted_files;
        s.deleted_files_not_understood = self.deleted_files_not_understood;
        s.scan_automatically = self.scan_automatically;
        s.ignore_names = self.ignore_names.clone();
        s
    }
}

/// How much the app records in its log: this level and the more serious ones.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
}

impl LogLevel {
    /// From the least to the most recorded.
    pub const ALL: [LogLevel; 4] = [
        LogLevel::Error,
        LogLevel::Warn,
        LogLevel::Info,
        LogLevel::Debug,
    ];

    pub fn filter(self) -> log::LevelFilter {
        match self {
            LogLevel::Error => log::LevelFilter::Error,
            LogLevel::Warn => log::LevelFilter::Warn,
            LogLevel::Info => log::LevelFilter::Info,
            LogLevel::Debug => log::LevelFilter::Debug,
        }
    }
}

/// `log_level` as saved, or the default if it is anything else (another type, a level this
/// version doesn't have), so one odd value can't reset the backup folder too.
fn lenient_level<'de, D: Deserializer<'de>>(d: D) -> Result<LogLevel, D::Error> {
    let value = serde_json::Value::deserialize(d)?;
    Ok(value
        .as_str()
        .and_then(|s| {
            LogLevel::ALL
                .into_iter()
                .find(|l| format!("{l:?}").eq_ignore_ascii_case(s.trim()))
        })
        .unwrap_or_default())
}

/// Write `path` via a temporary file beside it (flushed) and a rename over it, so a crash never
/// leaves a half-written file.
pub(crate) fn write_atomic_at(path: &Path, data: &[u8]) -> io::Result<()> {
    // A name of its own per call (process and counter), so two threads writing the same
    // file never share a temporary file. The name starts with the file's own, so the app
    // recognises a leftover as its own.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = path.with_file_name(format!("{name}.{}-{n}.tmp", std::process::id()));
    let written = (|| {
        let mut f = File::create(&tmp)?;
        f.write_all(data)?;
        crate::platform::flush_file(&f)?;
        drop(f);
        crate::platform::rename_replace(&tmp, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

/// Write `data` to a new file `path` and flush it. It never replaces a file: an existing one is
/// an error (`AlreadyExists`). For the copies the app keeps of its own files.
pub(crate) fn write_new_file(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    f.write_all(data)?;
    crate::platform::flush_file(&f)
}

#[derive(Debug, Clone)]
pub struct StateDir {
    dir: PathBuf,
}

impl StateDir {
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }

    fn file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// Write via a temporary file and rename, so a crash never leaves a half-written file.
    pub(crate) fn write_atomic(&self, name: &str, data: &[u8]) -> io::Result<()> {
        write_atomic_at(&self.file(name), data)
    }

    fn read_skip_file(&self) -> io::Result<SkipFile> {
        self.read_skip_file_at(&self.file(SKIP_FILE))
    }

    fn read_skip_file_at(&self, path: &Path) -> io::Result<SkipFile> {
        let name = path
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        match fs::read(path) {
            Ok(b) => parse_skip_file(&name, &b),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(SkipFile::empty()),
            Err(e) => Err(e),
        }
    }

    /// The skip file in a backup folder; none is an empty list.
    fn read_skip_file_in(store: &dyn BackupStore) -> io::Result<SkipFile> {
        match store.read(&skip_rel())? {
            Some((b, _)) => parse_skip_file(SKIP_NAME, &b),
            None => Ok(SkipFile::empty()),
        }
    }

    fn write_skip_file(&self, f: &SkipFile) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(f).map_err(io::Error::other)?;
        self.write_atomic(SKIP_FILE, &json)
    }

    /// Whether `scope`'s skip list is kept in the backup folder `store`: when it can hold one
    /// (it is there and isn't another device's), else in the settings folder.
    fn skip_in_backup(store: &dyn BackupStore, scope: &Scope) -> bool {
        crate::folder_record::fit_in(store, &scope.device) == crate::folder_record::Fit::Ok
    }

    /// The skip file that holds `scope`'s list, and whether it is the backup folder's. When it
    /// is, whatever the settings folder kept for this scope is taken over into it first (written
    /// to the folder, then removed from the settings folder).
    fn skip_load(&self, scope: &Scope, store: &dyn BackupStore) -> io::Result<(SkipFile, bool)> {
        if !Self::skip_in_backup(store, scope) {
            return Ok((self.read_skip_file()?, false));
        }
        let mut file = Self::read_skip_file_in(store)?;
        let mut profile = self.read_skip_file()?;
        if let Some(i) = profile
            .scopes
            .iter()
            .position(|e| e.device == scope.device && e.root == scope.root)
        {
            let old = profile.scopes[i].paths.clone();
            file.entry_in_folder(scope).paths.extend(old);
            let json = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
            store.write_replace(&skip_rel(), &json, None)?;
            profile.scopes.remove(i);
            match self.write_skip_file(&profile) {
                Ok(()) => info!("The skip list for {scope} moved into the backup folder"),
                Err(e) => warn!("Could not take the skip list for {scope} out of {SKIP_FILE}: {e}"),
            }
        }
        Ok((file, true))
    }

    fn skip_save(
        &self,
        file: &SkipFile,
        store: &dyn BackupStore,
        in_backup: bool,
    ) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(file).map_err(io::Error::other)?;
        if in_backup {
            store.write_replace(&skip_rel(), &json, None).map(|_| ())
        } else {
            self.write_atomic(SKIP_FILE, &json)
        }
    }

    /// The skip list for `scope` (empty if it has none).
    pub fn skip_list(&self, scope: &Scope) -> io::Result<HashSet<String>> {
        self.skip_list_in(scope, &LocalStore::new(&scope.dir))
    }

    /// [`StateDir::skip_list`] with the backup folder held in `store`.
    pub fn skip_list_in(
        &self,
        scope: &Scope,
        store: &dyn BackupStore,
    ) -> io::Result<HashSet<String>> {
        let (file, in_backup) = self.skip_load(scope, store)?;
        let found = if in_backup {
            file.scopes.iter().find(|e| e.device == scope.device)
        } else {
            file.scopes
                .iter()
                .find(|e| e.device == scope.device && e.root == scope.root)
        };
        Ok(found
            .map(|e| e.paths.iter().cloned().collect())
            .unwrap_or_default())
    }

    pub fn skip_add(&self, scope: &Scope, path: &SafeRelPath) -> io::Result<()> {
        self.skip_add_in(scope, &LocalStore::new(&scope.dir), path)
    }

    /// [`StateDir::skip_add`] with the backup folder held in `store`.
    pub fn skip_add_in(
        &self,
        scope: &Scope,
        store: &dyn BackupStore,
        path: &SafeRelPath,
    ) -> io::Result<()> {
        let (mut file, in_backup) = self.skip_load(scope, store)?;
        file.entry_for(scope, in_backup)
            .paths
            .insert(path.as_str().to_string());
        self.skip_save(&file, store, in_backup)?;
        info!("Added {path} to the skip list for {scope}");
        Ok(())
    }

    pub fn skip_remove(&self, scope: &Scope, path: &str) -> io::Result<()> {
        self.skip_remove_in(scope, &LocalStore::new(&scope.dir), path)
    }

    /// [`StateDir::skip_remove`] with the backup folder held in `store`.
    pub fn skip_remove_in(
        &self,
        scope: &Scope,
        store: &dyn BackupStore,
        path: &str,
    ) -> io::Result<()> {
        let (mut file, in_backup) = self.skip_load(scope, store)?;
        let was_there = file.entry_for(scope, in_backup).paths.remove(path);
        self.skip_save(&file, store, in_backup)?;
        if was_there {
            info!("Removed {path} from the skip list for {scope}");
        } else {
            debug!("{path} was not on the skip list for {scope}");
        }
        Ok(())
    }

    pub fn skip_clear(&self, scope: &Scope) -> io::Result<()> {
        self.skip_clear_in(scope, &LocalStore::new(&scope.dir))
    }

    /// [`StateDir::skip_clear`] with the backup folder held in `store`.
    pub fn skip_clear_in(&self, scope: &Scope, store: &dyn BackupStore) -> io::Result<()> {
        let (mut file, in_backup) = self.skip_load(scope, store)?;
        let paths = &mut file.entry_for(scope, in_backup).paths;
        let n = paths.len();
        paths.clear();
        self.skip_save(&file, store, in_backup)?;
        info!(
            "Cleared the skip list for {scope} ({})",
            crate::counted(n, "entry", "entries")
        );
        Ok(())
    }

    /// The saved settings. A missing or unreadable file gives the defaults; a damaged file is
    /// only replaced when the settings are next saved. A file from an earlier version loads too:
    /// what it lacks has its default, and if it has no `new_devices` yet, a `defaults.json` from
    /// that version is read, once, into it.
    pub fn config(&self) -> Config {
        self.migrate_archive_deleted();
        let path = self.file(CONFIG_FILE);
        let text = match fs::read_to_string(&path) {
            Ok(t) => Some(t),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                debug!("No {CONFIG_FILE} yet; using the default settings");
                None
            }
            Err(e) => {
                warn!(
                    "Could not read {} ({e}); using the default settings",
                    path.display()
                );
                return Config::default();
            }
        };
        let (mut config, has_new_devices) = match &text {
            None => (Config::default(), false),
            Some(t) => match serde_json::from_str::<serde_json::Value>(t)
                .and_then(|v| serde_json::from_value::<Config>(v.clone()).map(|c| (c, v)))
            {
                Ok((mut c, v)) => {
                    c.new_devices.deleted_files_not_understood =
                        crate::registry::deleted_files_unknown(
                            v.get("new_devices").and_then(|n| n.get("deleted_files")),
                        );
                    (c, v.get("new_devices").is_some())
                }
                Err(e) => {
                    warn!(
                        "{} is damaged ({e}); using the default settings until they are saved again",
                        path.display()
                    );
                    return Config::default();
                }
            },
        };
        if !has_new_devices {
            self.take_over_old_defaults(&mut config);
        }
        config
    }

    /// Reads `defaults.json`, if an earlier version left one, into `config.new_devices` and
    /// saves the settings so the file is not needed again; it is then renamed out of the way.
    /// If the settings can't be saved, nothing is renamed, and the file is read again next time.
    fn take_over_old_defaults(&self, config: &mut Config) {
        let old = self.file(OLD_DEFAULTS_FILE);
        let Ok(text) = fs::read_to_string(&old) else {
            return;
        };
        match serde_json::from_str::<NewDeviceDefaults>(&text) {
            Ok(d) => config.new_devices = d,
            Err(e) => {
                warn!("{OLD_DEFAULTS_FILE} is damaged ({e}); ignoring it");
                return;
            }
        }
        match self.save_config(config) {
            Ok(()) => {
                let done = self.file(&format!("{OLD_DEFAULTS_FILE}.migrated"));
                if let Err(e) = fs::rename(&old, &done) {
                    warn!("Could not set {OLD_DEFAULTS_FILE} aside: {e}");
                }
                info!(
                    "The settings for new devices moved from {OLD_DEFAULTS_FILE} into {CONFIG_FILE}"
                );
            }
            Err(e) => warn!("Could not save the settings from {OLD_DEFAULTS_FILE}: {e}"),
        }
    }

    /// An earlier version had one global setting, `archive_deleted`, in `config.json`. The
    /// choice is per device now ([`DeletedFiles`]). If the old setting was off, every device in
    /// `devices.json` that has no choice of its own yet gets [`DeletedFiles::LeaveInPlace`], which is
    /// what off did. Then the old key is taken out of `config.json`, so this happens once and
    /// is never written again. If `devices.json` can't be rewritten, the key stays and the next
    /// start tries again. Called before either file is read.
    pub(crate) fn migrate_archive_deleted(&self) {
        // The window and the device thread both read these files at start.
        static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let path = self.file(CONFIG_FILE);
        let Ok(text) = fs::read_to_string(&path) else {
            return;
        };
        let Ok(mut config) = serde_json::from_str::<serde_json::Value>(&text) else {
            return;
        };
        let Some(old) = config
            .as_object_mut()
            .and_then(|o| o.remove("archive_deleted"))
        else {
            return;
        };
        if old == serde_json::Value::Bool(false) {
            match self.leave_old_devices_in_place() {
                Ok(n) => info!(
                    "Archiving deleted files was off: {} now leave them in place",
                    crate::counted(n, "device", "devices")
                ),
                Err(e) => {
                    warn!("Could not carry the old archiving setting over to the devices: {e}");
                    return;
                }
            }
        }
        match serde_json::to_vec_pretty(&config)
            .map_err(io::Error::other)
            .and_then(|json| self.write_atomic(CONFIG_FILE, &json))
        {
            Ok(()) => debug!("The old archive_deleted setting was taken out of {CONFIG_FILE}"),
            Err(e) => warn!("Could not update {CONFIG_FILE}: {e}"),
        }
    }

    /// Give each device in `devices.json` that has no `deleted_files` yet `leave_in_place`.
    /// Returns how many devices changed.
    fn leave_old_devices_in_place(&self) -> io::Result<usize> {
        let path = self.file("devices.json");
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        // A damaged file is the registry's to set aside, not ours.
        let Ok(mut file) = serde_json::from_str::<serde_json::Value>(&text) else {
            return Ok(0);
        };
        let mut changed = 0;
        for device in file
            .get_mut("devices")
            .and_then(|d| d.as_array_mut())
            .into_iter()
            .flatten()
        {
            let Some(device) = device.as_object_mut() else {
                continue;
            };
            let settings = device
                .entry("settings")
                .or_insert_with(|| serde_json::json!({}));
            if let Some(settings) = settings.as_object_mut()
                && !settings.contains_key("deleted_files")
            {
                settings.insert("deleted_files".into(), "leave_in_place".into());
                changed += 1;
            }
        }
        if changed > 0 {
            let json = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
            self.write_atomic("devices.json", &json)?;
        }
        Ok(changed)
    }

    pub fn save_config(&self, config: &Config) -> io::Result<()> {
        self.migrate_archive_deleted();
        let json = serde_json::to_vec_pretty(config).map_err(io::Error::other)?;
        self.write_atomic(CONFIG_FILE, &json)?;
        debug!(
            "Saved the settings: backup folder {}, log level {:?}",
            config
                .backup_dir
                .as_ref()
                .map_or_else(|| "(none)".to_string(), |p| p.display().to_string()),
            config.log_level
        );
        Ok(())
    }

    pub fn begin_copy(&self, rec: &InProgress) -> io::Result<()> {
        let json = serde_json::to_vec(rec).map_err(io::Error::other)?;
        self.write_atomic(IN_PROGRESS_FILE, &json)
    }

    pub fn end_copy(&self) -> io::Result<()> {
        match fs::remove_file(self.file(IN_PROGRESS_FILE)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// After a crash: delete the interrupted copy's temporary file and drop the record.
    /// The file is NOT added to the skip list; the next sync copies it again.
    ///
    /// The record is not trusted: it is only acted on if its backup folder is one of
    /// `known_roots(device)`, the folders the app holds for that device, and (for a replacement)
    /// the folder's marker belongs to the device and lists the archive name the record uses.
    pub fn recover_interrupted(
        &self,
        known_roots: &dyn Fn(&str) -> Vec<PathBuf>,
    ) -> io::Result<Option<Interrupted>> {
        self.recover_interrupted_in(known_roots, &|root| Box::new(LocalStore::new(root)))
    }

    /// [`recover_interrupted`](Self::recover_interrupted), with the store that holds the backup
    /// at the record's root.
    pub fn recover_interrupted_in(
        &self,
        known_roots: &dyn Fn(&str) -> Vec<PathBuf>,
        open_store: &dyn Fn(&Path) -> Box<dyn BackupStore>,
    ) -> io::Result<Option<Interrupted>> {
        self.recover_interrupted_for(
            &|device| {
                known_roots(device)
                    .into_iter()
                    .map(Destination::Local)
                    .collect()
            },
            &|place| match place {
                Destination::Local(root) => Ok(open_store(root)),
                other => other.open(),
            },
        )
    }

    /// The general form: `known(device)` are the backups the app holds for the device (folders
    /// and buckets), and `open` makes the store for one. A record is acted on only if its backup
    /// is the same place as one of them. A backup that can't be reached keeps the record (see
    /// [`Interrupted::retry_later`]). Afterwards the store's own unfinished uploads older than a
    /// day are cleaned up.
    #[allow(clippy::type_complexity)]
    pub fn recover_interrupted_for(
        &self,
        known: &dyn Fn(&str) -> Vec<Destination>,
        open: &dyn Fn(&Destination) -> Result<Box<dyn BackupStore>, String>,
    ) -> io::Result<Option<Interrupted>> {
        let path = self.file(IN_PROGRESS_FILE);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                debug!("No interrupted copy to clean up");
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let result = match serde_json::from_str::<InProgress>(&text) {
            Ok(rec) => {
                let place = rec.place();
                let held = known(&rec.device).iter().any(|k| k.same_place(&place));
                let (temp_removed, completed, note, retry_later) = if !held {
                    (
                        false,
                        false,
                        format!(
                            "left everything alone: {} is not a backup this app holds for the device",
                            rec.root.display()
                        ),
                        false,
                    )
                } else {
                    match open(&place) {
                        Err(e) => (
                            false,
                            false,
                            format!("could not open the backup {}: {e}", rec.root.display()),
                            true,
                        ),
                        Ok(store) => {
                            let watched = Watched::new(store.as_ref());
                            let (removed, completed, note) = match &rec.replace {
                                None => {
                                    let (removed, note) = remove_temp_if_ours(&rec, &watched);
                                    (removed, false, note)
                                }
                                Some(step) => finish_replace(&rec, step, &watched),
                            };
                            clean_stale(&watched);
                            let offline = watched.offline();
                            let note = match &offline {
                                Some(why) => format!("{why}; {note}"),
                                None => note,
                            };
                            (removed, completed, note, offline.is_some())
                        }
                    }
                };
                Interrupted {
                    record: Some(rec),
                    temp_removed,
                    completed,
                    note,
                    retry_later,
                }
            }
            Err(e) => Interrupted {
                record: None,
                temp_removed: false,
                completed: false,
                note: format!("the record of the interrupted copy was damaged ({e})"),
                retry_later: false,
            },
        };
        match &result.record {
            Some(rec) => warn!(
                "The last copy was interrupted ({} into {}): {}",
                rec.source,
                rec.root.display(),
                result.note
            ),
            None => warn!("The last copy was interrupted: {}", result.note),
        }
        if result.retry_later {
            warn!("The backup could not be reached; the record is kept for the next start");
        } else {
            fs::remove_file(&path)?;
        }
        Ok(Some(result))
    }
}

/// How old an unfinished upload must be before start-up cleaning aborts it: a day, so another
/// copy of the app that is uploading right now is never cut off.
const STALE_UPLOAD_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Aborts the unfinished uploads in `store` older than a day (an object store keeps them until
/// told). A failure is only logged.
fn clean_stale(store: &dyn BackupStore) -> usize {
    match store.clean_stale_uploads(STALE_UPLOAD_AGE) {
        Ok(0) => 0,
        Ok(n) => {
            info!(
                "Aborted {n} unfinished upload(s) older than a day in {}",
                store.describe()
            );
            n
        }
        Err(e) => {
            warn!(
                "Could not clean up old unfinished uploads in {}: {e}",
                store.describe()
            );
            0
        }
    }
}

/// Start-up cleaning of unfinished uploads older than a day in each of `places` that is not a
/// folder (the backups the app holds), whether or not a crash record was left. Returns how many
/// were aborted.
#[allow(clippy::type_complexity)]
pub fn clean_stale_uploads(
    places: &[Destination],
    open: &dyn Fn(&Destination) -> Result<Box<dyn BackupStore>, String>,
) -> usize {
    places
        .iter()
        .filter(|p| !matches!(p, Destination::Local(_)))
        .filter_map(|p| open(p).ok())
        .map(|s| clean_stale(s.as_ref()))
        .sum()
}

/// Whether `path` is a plain file (not a link) that exists. `Err` if it can't be told.
fn plain_file(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(m) if is_link(&m) || !m.is_file() => Err(format!(
            "{} is not a plain file, so nothing was changed",
            path.display()
        )),
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("could not check {}: {e}", path.display())),
    }
}

/// Whether the file `rel` is in the store as a plain file. `Err` if it can't be told.
fn plain_in(store: &dyn BackupStore, rel: &SafeRelPath) -> Result<bool, String> {
    match store.stat(rel) {
        Ok(Some(st)) if st.kind != StoreKind::File => {
            Err(format!("{rel} is not a plain file, so nothing was changed"))
        }
        Ok(Some(_)) => Ok(true),
        Ok(None) => Ok(false),
        Err(e) => Err(format!("could not check {rel}: {e}")),
    }
}

/// Whether `path`'s folder resolves inside the backup folder `root`.
fn is_inside(root: &Path, path: &Path) -> bool {
    match (
        path.parent().and_then(|p| fs::canonicalize(p).ok()),
        fs::canonicalize(root).ok(),
    ) {
        (Some(parent), Some(root)) => parent.starts_with(&root),
        _ => false,
    }
}

/// The path `rel` (`/`-separated, relative to the root) under `root`.
fn under(root: &Path, rel: &str) -> PathBuf {
    let mut p = root.to_path_buf();
    for c in rel.split('/') {
        p.push(c);
    }
    p
}

/// On a folder store, whether the folder of `rel` still resolves inside the backup folder (no
/// junction swapped in since). Other stores have no folders.
fn inside_ok(store: &dyn BackupStore, rel: &str) -> bool {
    match store.as_local() {
        Some(local) => is_inside(&local.root, &under(&local.root, rel)),
        None => true,
    }
}

/// A path from a record as `/`-separated and relative to the backup folder `root`. Records hold
/// relative paths; one written by an earlier release holds absolute paths, which are cut down to
/// what is under the root. `None` if it is not under the root, or not a plain relative path.
fn rel_in_root(root: &Path, p: &Path) -> Option<String> {
    let rel = if p.is_absolute() {
        let canon = fs::canonicalize(root).ok();
        let cut = |base: &Path, q: &Path| q.strip_prefix(base).ok().map(Path::to_path_buf);
        cut(root, p)
            .or_else(|| canon.as_deref().and_then(|c| cut(c, p)))
            .or_else(|| {
                // Spelled another way (a short name, a link): by where its folder resolves.
                let parent = fs::canonicalize(p.parent()?).ok()?;
                cut(canon.as_deref()?, &parent.join(p.file_name()?))
            })?
    } else {
        p.to_path_buf()
    };
    let mut parts = Vec::new();
    for c in rel.components() {
        match c {
            std::path::Component::Normal(n) => parts.push(n.to_str()?.to_string()),
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// `temp` is `original` plus the temporary suffix, in the same folder.
fn temp_is_originals(temp: &Path, original: &Path) -> bool {
    let (Some(t), Some(o)) = (
        temp.file_name().and_then(|n| n.to_str()),
        original.file_name().and_then(|n| n.to_str()),
    ) else {
        return false;
    };
    temp.parent() == original.parent()
        && t.to_lowercase() == format!("{o}{TEMP_SUFFIX}").to_lowercase()
}

/// `archived` is under `<archive name>/`, where the marker in the recorded folder names the
/// archive and belongs to the recorded device. On a folder store the archive folder, with links
/// resolved, must also still be a folder of that name straight under the backup folder.
fn is_in_archive(rec: &InProgress, archived: &SafeRelPath, store: &dyn BackupStore) -> bool {
    let OwnerState::Owned(marker) = crate::owner::read_in(store) else {
        return false;
    };
    if marker.device_id != rec.device {
        return false;
    }
    let mut parts = archived.components();
    let (Some(first), Some(_)) = (parts.next(), parts.next()) else {
        return false;
    };
    if !marker.has_archive_name(first) {
        return false;
    }
    let Some(local) = store.as_local() else {
        return true;
    };
    let (Some(parent), Ok(root)) = (
        under(&local.root, archived.as_str())
            .parent()
            .and_then(|p| fs::canonicalize(p).ok()),
        fs::canonicalize(&local.root),
    ) else {
        return false;
    };
    parent
        .strip_prefix(&root)
        .ok()
        .and_then(|rel| rel.components().next())
        .and_then(|c| c.as_os_str().to_str())
        .is_some_and(|resolved| marker.has_archive_name(resolved))
}

/// `aside` is a name beside `original` that [`crate::sync`] gives an old copy it renames: the
/// original's name with ` (old <date and time>)` before the extension, maybe numbered.
fn aside_is_originals(aside: &Path, original: &Path) -> bool {
    let (Some(a), Some(o)) = (
        aside.file_name().and_then(|n| n.to_str()),
        original.file_name().and_then(|n| n.to_str()),
    ) else {
        return false;
    };
    aside.parent() == original.parent()
        && crate::paths::aside_original_name(a).as_deref() == Some(o)
}

/// What a replacement record names, checked: all relative to the backup folder.
struct Places {
    /// The unfinished upload (a temp file's path under the root on a folder store).
    temp: String,
    original: SafeRelPath,
    /// Where the old copy went; `None` for the bin.
    archived: Option<SafeRelPath>,
}

/// The upload's reference from a record: the temp file's path under the root for a folder; for
/// any other store the reference as it was written (the store checks it is its own).
fn temp_of(rec: &InProgress, store: &dyn BackupStore) -> Option<String> {
    if store.as_local().is_some() {
        rel_in_root(&rec.root, &rec.temp)
    } else {
        rec.temp.to_str().map(str::to_string)
    }
}

/// The record's places, if they are the shape the app itself makes and inside the backup.
fn places_of(rec: &InProgress, step: &ReplaceStep, store: &dyn BackupStore) -> Option<Places> {
    let temp = temp_of(rec, store)?;
    let original = SafeRelPath::parse(&rel_in_root(&rec.root, &step.original)?).ok()?;
    if store.as_local().is_some()
        && !(temp_is_originals(Path::new(&temp), Path::new(original.as_str()))
            && inside_ok(store, &temp)
            && inside_ok(store, original.as_str()))
    {
        return None;
    }
    let archived = match step.goes {
        OldCopyGoes::Bin => {
            if !step.archived.as_os_str().is_empty() {
                return None;
            }
            None
        }
        OldCopyGoes::Archive => {
            let a = SafeRelPath::parse(&rel_in_root(&rec.root, &step.archived)?).ok()?;
            if !is_in_archive(rec, &a, store) || !inside_ok(store, a.as_str()) {
                return None;
            }
            Some(a)
        }
        OldCopyGoes::Aside => {
            let a = SafeRelPath::parse(&rel_in_root(&rec.root, &step.archived)?).ok()?;
            if !aside_is_originals(Path::new(a.as_str()), Path::new(original.as_str()))
                || !inside_ok(store, a.as_str())
            {
                return None;
            }
            Some(a)
        }
    };
    Some(Places {
        temp,
        original,
        archived,
    })
}

/// Finish (or, where nothing had moved yet, leave) an interrupted changed-file replacement.
/// Every move is without replacing anything, and nothing is deleted: the old copy is at
/// `original`, or moved (to `archived`, or to the bin), the new copy at `temp` or `original`,
/// whichever step the crash came after. Recovery never needs anything back from the bin: if the
/// original is gone and the whole new copy is waiting, the new copy is moved into place.
/// Returns (temp removed, completed, what was done).
fn finish_replace(
    rec: &InProgress,
    step: &ReplaceStep,
    store: &dyn BackupStore,
) -> (bool, bool, String) {
    let Some(places) = places_of(rec, step, store) else {
        return (
            false,
            false,
            "left everything alone: the record does not match this backup folder's files and archive"
                .into(),
        );
    };
    let Some(local) = store.as_local() else {
        return finish_replace_remote(&places, step, store);
    };
    let temp_path = under(&local.root, &places.temp);
    let original = &places.original;
    let bin = step.goes == OldCopyGoes::Bin;
    let state = (|| -> Result<(bool, bool, bool), String> {
        Ok((
            plain_in(store, original)?,
            match &places.archived {
                Some(a) if !bin => plain_in(store, a)?,
                _ => false,
            },
            plain_file(&temp_path)?,
        ))
    })();
    let (orig, arch, temp) = match state {
        Ok(s) => s,
        Err(e) => return (false, false, e),
    };
    let mv = |from: &SafeRelPath, to: &SafeRelPath| {
        store
            .move_no_replace(from, to)
            .map_err(|e| format!("could not move {from} to {to}: {e}; both copies are kept"))
    };
    // The whole new copy waiting as a temporary file goes into place.
    let mv_temp = || {
        crate::platform::rename_no_replace(&temp_path, &original.under(&local.root)).map_err(|e| {
            format!(
                "could not move {} to {original}: {e}; both copies are kept",
                temp_path.display()
            )
        })
    };
    let (where_old, bin_name) = match step.goes {
        OldCopyGoes::Archive => ("in the archive", ""),
        OldCopyGoes::Aside => ("renamed beside it", ""),
        OldCopyGoes::Bin => ("in the bin", crate::platform::BIN_NAME),
    };
    let finished = format!(
        "finished replacing the changed file: the old copy is {where_old}{} and the new one is in place",
        if bin {
            format!(" ({bin_name})")
        } else {
            String::new()
        }
    );
    let result = if bin {
        match (orig, temp) {
            // The old copy is gone and the new one is whole: finish the move into place.
            (false, true) => mv_temp().map(|()| finished),
            // The old copy is still in place: nothing was moved. The unfinished new copy is
            // only a copy of the device's file, so it is removed and the next sync makes it
            // again.
            (true, true) => {
                let (removed, note) = remove_temp_if_ours(rec, store);
                return (
                    removed,
                    false,
                    format!("the old copy had not been moved, so it was left in place; {note}"),
                );
            }
            (true, false) => Ok(
                "the changed file had been replaced: the old copy is in the bin and the new one is in place"
                    .to_string(),
            ),
            (false, false) => Err(format!(
                "the changed file {original} is gone from the backup and no new copy was left; the old copy should be in the {bin_name}"
            )),
        }
    } else {
        let Some(archived) = &places.archived else {
            return (
                false,
                false,
                "left everything alone: the record has no place for the old copy".into(),
            );
        };
        match (orig, arch, temp) {
            // Before the first move: the old copy is in place, the whole new copy is waiting.
            (true, false, true) => mv(original, archived)
                .and_then(|()| mv_temp())
                .map(|()| finished),
            // After the first move.
            (false, true, true) => mv_temp().map(|()| finished),
            // After both moves.
            (true, true, false) => Ok(format!(
                "the changed file had been replaced: the old copy is {where_old} and the new one is in place"
            )),
            _ => Err(format!(
                "the changed file {original} was left as it is (the old copy {}, the moved copy {}, the new copy {}); nothing was deleted",
                if orig { "is there" } else { "is missing" },
                if arch { "is there" } else { "is missing" },
                if temp { "is there" } else { "is missing" },
            )),
        }
    };
    match result {
        Ok(note) => (false, true, note),
        Err(note) => (false, false, note),
    }
}

/// What was found of the old copy of an interrupted replacement (or move) on an object store.
enum OldFound {
    /// Still in place: nothing moved.
    InPlace,
    /// Moved: only the moved copy is there.
    Moved,
    /// Both were there, and the original was removed because the copy was shown to match.
    OriginalRemoved,
    /// Both are there and stay; why.
    BothKept(String),
    /// Neither is there.
    Gone,
    /// Could not tell.
    Unknown(String),
}

/// An interrupted replacement in a store whose unfinished uploads are invisible. The old copy is
/// where it was or moved. A move cut between its copy and its delete leaves both; the original
/// is then deleted only if the store shows the moved copy has the same size and modified time
/// ([`BackupStore::remove_verified_original`]), else both stay.
///
/// The new copy: when it had been staged (`step.staged_size`) and the old copy is out of the
/// way, the store holds all of it, so it is finished into place ([`BackupStore::finish_by_ref`])
/// and the replacement is done, like the roll forward on a folder. Otherwise, or if that does
/// not work, the upload is dropped and the next sync copies the file again (it is on the device).
/// A record with no upload (a lone move) only deals with the old copy.
fn finish_replace_remote(
    places: &Places,
    step: &ReplaceStep,
    store: &dyn BackupStore,
) -> (bool, bool, String) {
    let move_only = places.temp.is_empty();
    let orig = plain_in(store, &places.original);
    let arch = match &places.archived {
        Some(a) => plain_in(store, a),
        None => Ok(false),
    };
    let where_old = match step.goes {
        OldCopyGoes::Aside => "renamed beside it",
        _ => "in the archive",
    };
    let old = match (orig, arch) {
        (Ok(true), Ok(false)) => OldFound::InPlace,
        (Ok(false), Ok(true)) => OldFound::Moved,
        (Ok(true), Ok(true)) => {
            let copy = places.archived.as_ref().expect("a copy that was found");
            match store.remove_verified_original(&places.original, copy) {
                Ok(true) => OldFound::OriginalRemoved,
                Ok(false) => {
                    OldFound::BothKept(format!("the one {where_old} could not be shown to match"))
                }
                Err(e) => OldFound::BothKept(e.to_string()),
            }
        }
        (Ok(false), Ok(false)) => OldFound::Gone,
        (Err(e), _) | (_, Err(e)) => OldFound::Unknown(e),
    };
    let both_kept = |why: &str| {
        format!(
            "the old copy's move was cut short: both copies exist ({why}), so both were left as they are"
        )
    };
    if move_only {
        let note = match &old {
            OldFound::InPlace => {
                "the old copy had not been moved, so it was left in place".to_string()
            }
            OldFound::Moved => format!("the old copy is {where_old}"),
            OldFound::OriginalRemoved => format!(
                "the old copy's move was cut short: the copy {where_old} has the same size and date, so the original was removed"
            ),
            OldFound::BothKept(why) => both_kept(why),
            OldFound::Gone => format!("{} is gone from the backup", places.original),
            OldFound::Unknown(e) => e.clone(),
        };
        return (false, false, note);
    }
    // The new copy goes into place only when the old one is out of the way.
    let away = matches!(old, OldFound::Moved | OldFound::OriginalRemoved);
    let mut finish_failed = None;
    if away && let Some(size) = step.staged_size {
        match store.finish_by_ref(&places.temp, size) {
            Ok(_) => {
                let how = if matches!(old, OldFound::OriginalRemoved) {
                    "the old copy's move was cut short and was finished: "
                } else {
                    ""
                };
                return (
                    false,
                    true,
                    format!(
                        "finished replacing the changed file: {how}the old copy is {where_old} and the new one, which was whole in the store, is in place"
                    ),
                );
            }
            Err(e) => finish_failed = Some(e),
        }
    }
    let aborted = store.abort_by_ref(&places.temp);
    let mut dropped = match &aborted {
        Ok(()) => "the unfinished upload was dropped".to_string(),
        Err(e) => format!("the unfinished upload could not be dropped ({e})"),
    };
    if let Some(e) = finish_failed {
        dropped = format!("it could not be finished ({e}); {dropped}");
    }
    let note = match &old {
        OldFound::InPlace => {
            format!("the old copy had not been moved, so it was left in place; {dropped}")
        }
        OldFound::Moved => format!(
            "the old copy is {where_old} and the new copy was not finished; {dropped}, and the next sync copies the file again"
        ),
        OldFound::OriginalRemoved => format!(
            "the old copy's move was cut short: the copy {where_old} has the same size and date, so the original was removed; {dropped}, and the next sync copies the file again"
        ),
        OldFound::BothKept(why) => format!("{}; {dropped}", both_kept(why)),
        OldFound::Gone => format!(
            "the changed file {} is gone from the backup and no new copy was finished; {dropped}",
            places.original
        ),
        OldFound::Unknown(e) => format!("{e}; {dropped}"),
    };
    (aborted.is_ok(), false, note)
}

/// Remove the unfinished copy `rec.temp` only if it is plainly this app's temporary file: our
/// suffix, a regular file (not a link), inside the recorded backup folder. In a store without
/// files, the upload it names is dropped (the store refuses anything that is not this app's).
fn remove_temp_if_ours(rec: &InProgress, store: &dyn BackupStore) -> (bool, String) {
    let Some(local) = store.as_local() else {
        let Some(temp) = temp_of(rec, store) else {
            return (
                false,
                format!(
                    "left {} alone: not an upload of this app",
                    rec.temp.display()
                ),
            );
        };
        return match store.abort_by_ref(&temp) {
            Ok(()) => (true, format!("dropped the unfinished upload {temp}")),
            Err(e) => (false, format!("could not drop the upload {temp}: {e}")),
        };
    };
    let name_ok = rec
        .temp
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.to_ascii_lowercase().ends_with(TEMP_SUFFIX));
    if !name_ok {
        return (
            false,
            format!(
                "left {} alone: not a temporary file of this app",
                rec.temp.display()
            ),
        );
    }
    let Some(temp) = rel_in_root(&rec.root, &rec.temp) else {
        return (
            false,
            format!(
                "left {} alone: not inside the backup folder",
                rec.temp.display()
            ),
        );
    };
    let path = under(&local.root, &temp);
    let meta = match fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return (false, "no temporary file was left".into());
        }
        Err(e) => {
            return (false, format!("could not check {}: {e}", path.display()));
        }
    };
    if is_link(&meta) || !meta.is_file() {
        return (
            false,
            format!("left {} alone: not a plain file", path.display()),
        );
    }
    if !inside_ok(store, &temp) {
        return (
            false,
            format!(
                "left {} alone: not inside the backup folder",
                path.display()
            ),
        );
    }
    match store.abort_by_ref(&temp) {
        Ok(()) => (
            true,
            format!("removed the unfinished copy {}", path.display()),
        ),
        Err(e) => (false, format!("could not remove {}: {e}", path.display())),
    }
}

/// Held for the app's lifetime so a second copy of the app can't sync at the same time.
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    /// `Ok(None)` if another instance holds the lock.
    pub fn acquire(state: &StateDir) -> io::Result<Option<Self>> {
        Ok(crate::platform::acquire_lock(&state.file(INSTANCE_FILE))?.map(|f| Self { _file: f }))
    }
}

#[cfg(test)]
mod skip_store_tests {
    use super::*;
    use crate::store::mem::MemStore;

    fn state() -> (tempfile::TempDir, StateDir) {
        let t = tempfile::tempdir().unwrap();
        let s = StateDir::open(t.path().join("state")).unwrap();
        (t, s)
    }

    fn p(s: &str) -> SafeRelPath {
        SafeRelPath::parse(s).unwrap()
    }

    fn scope() -> Scope {
        Scope::new("dev", Path::new("/not/used"))
    }

    #[test]
    fn the_list_lives_in_the_backup_store_and_travels_with_it() {
        let (_t, s) = state();
        let mem = MemStore::new();
        let sc = scope();
        assert!(s.skip_list_in(&sc, &mem).unwrap().is_empty());
        s.skip_add_in(&sc, &mem, &p("DCIM/a.jpg")).unwrap();
        s.skip_add_in(&sc, &mem, &p("DCIM/b.jpg")).unwrap();
        assert_eq!(mem.keys(), [SKIP_NAME]);
        assert_eq!(s.skip_list_in(&sc, &mem).unwrap().len(), 2);
        s.skip_remove_in(&sc, &mem, "DCIM/a.jpg").unwrap();
        assert_eq!(
            s.skip_list_in(&sc, &mem).unwrap(),
            HashSet::from(["DCIM/b.jpg".to_string()])
        );
        s.skip_clear_in(&sc, &mem).unwrap();
        assert!(s.skip_list_in(&sc, &mem).unwrap().is_empty());
        // Nothing was kept in the settings folder.
        assert!(!s.file(SKIP_FILE).exists());
    }

    #[test]
    fn the_settings_folders_list_moves_into_the_store_once() {
        let (_t, s) = state();
        let sc = scope();
        let other = MemStore::new();
        other.insert(
            crate::owner::MARKER,
            br#"{"device_id":"someone else","device_name":"x"}"#,
            None,
        );
        // Another device's folder: the list stays in the settings folder.
        s.skip_add_in(&sc, &other, &p("old.jpg")).unwrap();
        assert!(other.get(SKIP_NAME).is_none());
        assert!(s.file(SKIP_FILE).exists());
        // A folder that fits takes it over.
        let mem = MemStore::new();
        assert!(s.skip_list_in(&sc, &mem).unwrap().contains("old.jpg"));
        assert!(mem.get(SKIP_NAME).is_some());
        assert!(s.skip_list_in(&sc, &other).unwrap().is_empty());
    }

    #[test]
    fn a_damaged_list_in_the_store_is_reported_and_left_alone() {
        let (_t, s) = state();
        let mem = MemStore::new();
        mem.insert(SKIP_NAME, b"{ nope", None);
        assert!(s.skip_add_in(&scope(), &mem, &p("a.jpg")).is_err());
        assert_eq!(mem.get(SKIP_NAME).unwrap(), b"{ nope");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> (tempfile::TempDir, StateDir) {
        let t = tempfile::tempdir().unwrap();
        let s = StateDir::open(t.path().join("state")).unwrap();
        (t, s)
    }

    fn p(s: &str) -> SafeRelPath {
        SafeRelPath::parse(s).unwrap()
    }

    /// Threads writing the same file at once don't collide on a shared temporary file, and no
    /// temporary file is left behind.
    #[test]
    fn simultaneous_atomic_writes_do_not_collide() {
        let (_t, s) = state();
        let s = std::sync::Arc::new(s);
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let s = s.clone();
                std::thread::spawn(move || {
                    for j in 0..25 {
                        // Windows may refuse a move onto a file another thread is moving onto
                        // at that instant; that is fine, but no temporary file may be left.
                        let _ = s.write_atomic("same.json", format!("{i}-{j}").as_bytes());
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        let text = fs::read_to_string(s.path().join("same.json")).unwrap();
        assert!(text.contains('-') && text.len() <= 5, "{text}");
        let left: Vec<_> = fs::read_dir(s.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn config_round_trips_and_a_damaged_file_gives_defaults() {
        let (_t, s) = state();
        assert_eq!(s.config(), Config::default());
        assert_eq!(
            Config::default().new_devices.deleted_files,
            DeletedFiles::Archive,
            "archiving is the default"
        );
        let c = Config {
            backup_dir: Some(PathBuf::from(r"D:\Photos\Backup")),
            log_level: LogLevel::Debug,
            new_devices: NewDeviceDefaults::default(),
            default_s3: None,
        };
        s.save_config(&c).unwrap();
        assert_eq!(s.config(), c);
        // A settings file from before the option existed: archiving on.
        fs::write(s.path().join(CONFIG_FILE), br#"{"backup_dir": "D:\\B"}"#).unwrap();
        assert_eq!(s.config().new_devices.deleted_files, DeletedFiles::Archive);
        fs::write(s.path().join(CONFIG_FILE), b"{ not json").unwrap();
        assert_eq!(s.config(), Config::default());
    }

    /// An odd `deleted_files` in the settings for new devices must not turn into archiving
    /// (which moves files): it becomes "leave in place", and the page is told.
    #[test]
    fn an_unknown_deleted_files_value_for_new_devices_leaves_files_in_place() {
        let (_t, s) = state();
        fs::write(
            s.path().join(CONFIG_FILE),
            br#"{"new_devices": {"deleted_files": "shred_it", "archive_name": "Mine"}}"#,
        )
        .unwrap();
        let c = s.config();
        assert_eq!(c.new_devices.deleted_files, DeletedFiles::LeaveInPlace);
        assert!(c.new_devices.deleted_files_not_understood);
        assert_eq!(c.new_devices.archive_name, "Mine");
        // A device made from it starts with files left in place, and says why.
        let d = c
            .new_devices
            .settings_for(DeviceKind::Ptp, &Signals::default());
        assert_eq!(d.deleted_files, DeletedFiles::LeaveInPlace);
        assert!(d.deleted_files_not_understood);
        // A known value is no trouble.
        fs::write(
            s.path().join(CONFIG_FILE),
            br#"{"new_devices": {"deleted_files": "recycle_bin"}}"#,
        )
        .unwrap();
        let c = s.config();
        assert_eq!(c.new_devices.deleted_files, DeletedFiles::RecycleBin);
        assert!(!c.new_devices.deleted_files_not_understood);
    }

    #[test]
    fn what_new_devices_start_with_is_part_of_the_one_settings_file() {
        let (_t, s) = state();
        // An old config.json, without the new section, still loads with the defaults.
        fs::write(
            s.path().join(CONFIG_FILE),
            br#"{ "backup_dir": "C:\\B", "archive_deleted": false }"#,
        )
        .unwrap();
        let c = s.config();
        assert_eq!(c.new_devices, NewDeviceDefaults::default());
        assert_eq!(c.backup_dir, Some(PathBuf::from(r"C:\B")));
        // Saved, read back, and a half-written section keeps the rest at its defaults.
        let c = Config {
            new_devices: NewDeviceDefaults {
                archive_name: "Old".into(),
                archive_layout: ArchiveLayout::Flat,
                deleted_files: DeletedFiles::RecycleBin,
                deleted_files_not_understood: false,
                scan_automatically: true,
                ignore_names: vec!["x.tmp".into()],
            },
            ..c
        };
        s.save_config(&c).unwrap();
        assert_eq!(s.config(), c);
        fs::write(
            s.path().join(CONFIG_FILE),
            br#"{ "new_devices": { "scan_automatically": true } }"#,
        )
        .unwrap();
        let c = s.config();
        assert!(c.new_devices.scan_automatically);
        assert_eq!(c.new_devices.archive_name, DEFAULT_ARCHIVE_NAME);
        // The kind decides what the defaults don't.
        let settings = c
            .new_devices
            .settings_for(DeviceKind::Ptp, &Signals::default());
        assert!(settings.scan_automatically);
        assert_eq!(settings.folders, ["DCIM", "Pictures", "Movies"]);
    }

    /// A `defaults.json` from an earlier version is read once into the settings file, and never
    /// again.
    #[test]
    fn an_old_defaults_file_is_read_once_and_then_left_alone() {
        let (_t, s) = state();
        fs::write(
            s.file(OLD_DEFAULTS_FILE),
            br#"{ "archive_name": "Gone", "scan_automatically": true }"#,
        )
        .unwrap();
        fs::write(
            s.path().join(CONFIG_FILE),
            br#"{ "archive_deleted": false }"#,
        )
        .unwrap();
        let c = s.config();
        assert_eq!(c.new_devices.archive_name, "Gone");
        assert!(c.new_devices.scan_automatically);
        // It is in config.json now, and the old file is out of the way.
        let saved = fs::read_to_string(s.file(CONFIG_FILE)).unwrap();
        assert!(
            saved.contains("\"new_devices\"") && saved.contains("Gone"),
            "{saved}"
        );
        assert!(!s.file(OLD_DEFAULTS_FILE).exists());
        // A new old-style file isn't read any more.
        fs::write(s.file(OLD_DEFAULTS_FILE), br#"{ "archive_name": "Later" }"#).unwrap();
        assert_eq!(s.config().new_devices.archive_name, "Gone");
        // With no settings file at all, it is read too, and a damaged one is ignored.
        let (_t, s) = state();
        fs::write(
            s.file(OLD_DEFAULTS_FILE),
            br#"{ "archive_layout": "flat" }"#,
        )
        .unwrap();
        assert_eq!(s.config().new_devices.archive_layout, ArchiveLayout::Flat);
        let (_t, s) = state();
        fs::write(s.file(OLD_DEFAULTS_FILE), b"{ not json").unwrap();
        assert_eq!(s.config(), Config::default());
        assert!(s.file(OLD_DEFAULTS_FILE).exists());
    }

    #[test]
    fn the_log_level_is_saved_and_old_or_odd_files_still_load() {
        let (_t, s) = state();
        assert_eq!(Config::default().log_level, LogLevel::Info);
        // A settings file from before the option existed, as PowerShell's ConvertTo-Json writes it.
        fs::write(
            s.path().join(CONFIG_FILE),
            br#"{ "backup_dir": "C:\\B", "archive_deleted": false }"#,
        )
        .unwrap();
        let c = s.config();
        assert_eq!(c.backup_dir, Some(PathBuf::from(r"C:\B")));
        assert_eq!(c.log_level, LogLevel::Info);
        // Saved as a lower-case word, and read back.
        for level in LogLevel::ALL {
            let c = Config {
                log_level: level,
                ..c.clone()
            };
            s.save_config(&c).unwrap();
            let text = fs::read_to_string(s.path().join(CONFIG_FILE)).unwrap();
            let word = format!("{level:?}").to_lowercase();
            assert!(
                text.contains(&format!(r#""log_level": "{word}""#)),
                "{text}"
            );
            assert_eq!(s.config(), c);
        }
        // Case doesn't matter. An unknown level or another type gives the default, and the
        // rest of the file still loads.
        for (value, want) in [
            (r#""DEBUG""#, LogLevel::Debug),
            (r#""trace""#, LogLevel::Info),
            ("3", LogLevel::Info),
            ("null", LogLevel::Info),
        ] {
            let json = format!(r#"{{ "backup_dir": "C:\\B", "log_level": {value} }}"#);
            fs::write(s.path().join(CONFIG_FILE), json).unwrap();
            let c = s.config();
            assert_eq!(c.log_level, want, "{value}");
            assert_eq!(c.backup_dir, Some(PathBuf::from(r"C:\B")), "{value}");
        }
        assert_eq!(LogLevel::Debug.filter(), log::LevelFilter::Debug);
        assert_eq!(LogLevel::Error.filter(), log::LevelFilter::Error);
    }

    #[test]
    fn skip_lists_are_per_device_and_folder() {
        let (_t, s) = state();
        let a = Scope::new("serial:A", Path::new(r"C:\Backup\PhoneA"));
        let b = Scope::new("serial:B", Path::new(r"C:\Backup\PhoneA"));
        let a2 = Scope::new("serial:A", Path::new(r"D:\Other"));
        s.skip_add(&a, &p("DCIM/100APPLE/IMG_0001.HEIC")).unwrap();
        assert!(
            s.skip_list(&a)
                .unwrap()
                .contains("DCIM/100APPLE/IMG_0001.HEIC")
        );
        assert!(
            s.skip_list(&b).unwrap().is_empty(),
            "skip leaked to another phone"
        );
        assert!(
            s.skip_list(&a2).unwrap().is_empty(),
            "skip leaked to another folder"
        );
        // Same folder written differently is the same scope.
        let a_again = Scope::new("serial:A", Path::new(r"c:\backup\phonea\"));
        assert_eq!(s.skip_list(&a_again).unwrap().len(), 1);
        s.skip_remove(&a, "DCIM/100APPLE/IMG_0001.HEIC").unwrap();
        assert!(s.skip_list(&a).unwrap().is_empty());
    }

    #[test]
    fn files_left_by_the_old_app_are_neither_read_nor_touched() {
        let (_t, s) = state();
        fs::write(s.file("skip_list.json"), r#"["DCIM/IMG_1.HEIC"]"#).unwrap();
        fs::write(s.file("current_copying.tmp"), b"DCIM/IMG_1.HEIC").unwrap();
        let a = Scope::new("serial:A", Path::new(r"C:\B"));
        assert!(s.skip_list(&a).unwrap().is_empty());
        assert!(s.recover_interrupted(&|_| Vec::new()).unwrap().is_none());
        assert!(s.file("skip_list.json").exists());
        assert!(s.file("current_copying.tmp").exists());
    }

    #[test]
    fn a_damaged_skip_file_is_reported_not_overwritten() {
        let (_t, s) = state();
        fs::write(s.file(SKIP_FILE), b"{not json").unwrap();
        let a = Scope::new("serial:A", Path::new(r"C:\B"));
        assert!(s.skip_list(&a).is_err());
        assert!(s.skip_add(&a, &p("x.jpg")).is_err());
        assert_eq!(fs::read(s.file(SKIP_FILE)).unwrap(), b"{not json");
    }

    #[test]
    fn crash_recovery_removes_our_temp_file_and_does_not_skip() {
        let (t, s) = state();
        let root = t.path().join("backup");
        fs::create_dir_all(root.join("DCIM")).unwrap();
        let temp = root.join("DCIM").join(format!("IMG_1.HEIC{TEMP_SUFFIX}"));
        fs::write(&temp, b"half").unwrap();
        let rec = InProgress {
            device: "serial:A".into(),
            root: root.clone(),
            source: "DCIM/IMG_1.HEIC".into(),
            temp: temp.clone(),
            replace: None,
            destination: None,
        };
        s.begin_copy(&rec).unwrap();
        let got = s
            .recover_interrupted(&|_| vec![root.clone()])
            .unwrap()
            .expect("interrupted copy found");
        assert!(got.temp_removed, "{}", got.note);
        assert!(!temp.exists());
        assert!(
            s.recover_interrupted(&|_| vec![root.clone()])
                .unwrap()
                .is_none(),
            "record not cleared"
        );
        let scope = Scope::new("serial:A", &root);
        assert!(
            s.skip_list(&scope).unwrap().is_empty(),
            "crash must not skip the file"
        );
    }

    #[test]
    fn crash_recovery_never_deletes_files_that_are_not_ours() {
        let (t, s) = state();
        let root = t.path().join("backup");
        fs::create_dir_all(&root).unwrap();
        let precious = root.join("IMG_1.HEIC");
        fs::write(&precious, b"photo").unwrap();
        let outside = t.path().join(format!("x{TEMP_SUFFIX}"));
        fs::write(&outside, b"not in root").unwrap();
        for temp in [precious.clone(), outside.clone()] {
            s.begin_copy(&InProgress {
                device: "d".into(),
                root: root.clone(),
                source: "x".into(),
                temp,
                replace: None,
                destination: None,
            })
            .unwrap();
            let got = s
                .recover_interrupted(&|_| vec![root.clone()])
                .unwrap()
                .unwrap();
            assert!(!got.temp_removed, "{}", got.note);
        }
        assert!(precious.exists() && outside.exists());
    }

    #[test]
    fn only_one_instance_holds_the_lock() {
        let (_t, s) = state();
        let first = InstanceLock::acquire(&s).unwrap();
        assert!(first.is_some());
        assert!(InstanceLock::acquire(&s).unwrap().is_none());
        drop(first);
        assert!(InstanceLock::acquire(&s).unwrap().is_some());
    }
}
