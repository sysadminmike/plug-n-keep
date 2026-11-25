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

impl SkipFile {
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InProgress {
    pub device: String,
    pub root: PathBuf,
    /// Device path of the file being copied.
    pub source: String,
    /// The temporary file being written.
    pub temp: PathBuf,
    /// Set once the new copy is whole and the changed-file moves may begin ([`ReplaceStep`]).
    #[serde(default)]
    pub replace: Option<ReplaceStep>,
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
    /// Where the file is in the backup folder (the old copy, then the new one).
    pub original: PathBuf,
    /// Where the old copy goes: in the archive, or beside the original. Empty for the bin.
    #[serde(default)]
    pub archived: PathBuf,
    /// What the old copy's move is. A record from before this field is an archive move.
    #[serde(default)]
    pub goes: OldCopyGoes,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interrupted {
    pub record: Option<InProgress>,
    pub temp_removed: bool,
    /// An interrupted changed-file replacement was finished: the old copy is in the archive
    /// and the new one in place.
    pub completed: bool,
    pub note: String,
}

/// The app's settings, kept in `config.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub backup_dir: Option<PathBuf>,
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
        match fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).map_err(|e| {
                warn!("{name} is damaged ({e}); it is left as it is");
                io::Error::other(format!("{name} is damaged ({e}); not changing it"))
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(SkipFile {
                version: 2,
                scopes: Vec::new(),
            }),
            Err(e) => Err(e),
        }
    }

    fn write_skip_file(&self, f: &SkipFile) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(f).map_err(io::Error::other)?;
        self.write_atomic(SKIP_FILE, &json)
    }

    /// Where `scope`'s skip list is kept: in the backup folder if it can hold one, else in the
    /// settings folder (`None`).
    fn skip_path(scope: &Scope) -> Option<PathBuf> {
        (crate::folder_record::fit(&scope.dir, &scope.device) == crate::folder_record::Fit::Ok)
            .then(|| scope.dir.join(SKIP_NAME))
    }

    /// The skip file that holds `scope`'s list, and where it is. When it is the backup folder's,
    /// whatever the settings folder kept for this scope is taken over into it first (written to
    /// the folder, then removed from the settings folder).
    fn skip_load(&self, scope: &Scope) -> io::Result<(SkipFile, Option<PathBuf>)> {
        let Some(path) = Self::skip_path(scope) else {
            return Ok((self.read_skip_file()?, None));
        };
        let mut file = self.read_skip_file_at(&path)?;
        let mut profile = self.read_skip_file()?;
        if let Some(i) = profile
            .scopes
            .iter()
            .position(|e| e.device == scope.device && e.root == scope.root)
        {
            let old = profile.scopes[i].paths.clone();
            file.entry_in_folder(scope).paths.extend(old);
            let json = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
            crate::state::write_atomic_at(&path, &json)?;
            profile.scopes.remove(i);
            match self.write_skip_file(&profile) {
                Ok(()) => info!("The skip list for {scope} moved into the backup folder"),
                Err(e) => warn!("Could not take the skip list for {scope} out of {SKIP_FILE}: {e}"),
            }
        }
        Ok((file, Some(path)))
    }

    fn skip_save(&self, file: &SkipFile, at: Option<&Path>) -> io::Result<()> {
        let json = serde_json::to_vec_pretty(file).map_err(io::Error::other)?;
        match at {
            Some(path) => crate::state::write_atomic_at(path, &json),
            None => self.write_atomic(SKIP_FILE, &json),
        }
    }

    /// The skip list for `scope` (empty if it has none).
    pub fn skip_list(&self, scope: &Scope) -> io::Result<HashSet<String>> {
        let (file, at) = self.skip_load(scope)?;
        let found = if at.is_some() {
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
        let (mut file, at) = self.skip_load(scope)?;
        file.entry_for(scope, at.is_some())
            .paths
            .insert(path.as_str().to_string());
        self.skip_save(&file, at.as_deref())?;
        info!("Added {path} to the skip list for {scope}");
        Ok(())
    }

    pub fn skip_remove(&self, scope: &Scope, path: &str) -> io::Result<()> {
        let (mut file, at) = self.skip_load(scope)?;
        let was_there = file.entry_for(scope, at.is_some()).paths.remove(path);
        self.skip_save(&file, at.as_deref())?;
        if was_there {
            info!("Removed {path} from the skip list for {scope}");
        } else {
            debug!("{path} was not on the skip list for {scope}");
        }
        Ok(())
    }

    pub fn skip_clear(&self, scope: &Scope) -> io::Result<()> {
        let (mut file, at) = self.skip_load(scope)?;
        let paths = &mut file.entry_for(scope, at.is_some()).paths;
        let n = paths.len();
        paths.clear();
        self.skip_save(&file, at.as_deref())?;
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
                let (temp_removed, completed, note) = if !is_known_root(&rec, known_roots) {
                    (
                        false,
                        false,
                        format!(
                            "left everything alone: {} is not a backup folder this app holds for the device",
                            rec.root.display()
                        ),
                    )
                } else {
                    match &rec.replace {
                        None => {
                            let (removed, note) = remove_temp_if_ours(&rec);
                            (removed, false, note)
                        }
                        Some(step) => finish_replace(&rec, step),
                    }
                };
                Interrupted {
                    record: Some(rec),
                    temp_removed,
                    completed,
                    note,
                }
            }
            Err(e) => Interrupted {
                record: None,
                temp_removed: false,
                completed: false,
                note: format!("the record of the interrupted copy was damaged ({e})"),
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
        fs::remove_file(&path)?;
        Ok(Some(result))
    }
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

/// Whether the recorded backup folder is one the app holds for the recorded device.
fn is_known_root(rec: &InProgress, known_roots: &dyn Fn(&str) -> Vec<PathBuf>) -> bool {
    let Ok(root) = fs::canonicalize(&rec.root) else {
        return false;
    };
    known_roots(&rec.device)
        .iter()
        .filter_map(|k| fs::canonicalize(k).ok())
        .any(|k| k == root)
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

/// `archived` is under `<root>/<archive name>/`, where the marker in the recorded folder names
/// the archive and belongs to the recorded device.
fn is_in_archive(rec: &InProgress, archived: &Path) -> bool {
    let OwnerState::Owned(marker) = crate::owner::read(&rec.root) else {
        return false;
    };
    if marker.device_id != rec.device || !is_inside(&rec.root, archived) {
        return false;
    }
    let (Some(parent), Ok(root)) = (
        archived.parent().and_then(|p| fs::canonicalize(p).ok()),
        fs::canonicalize(&rec.root),
    ) else {
        return false;
    };
    let Ok(rel) = parent.strip_prefix(&root) else {
        return false;
    };
    rel.components()
        .next()
        .and_then(|c| c.as_os_str().to_str())
        .is_some_and(|first| marker.has_archive_name(first))
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

/// Finish (or, where nothing had moved yet, leave) an interrupted changed-file replacement.
/// Every move is without replacing anything, and nothing is deleted: the old copy is at
/// `original`, or moved (to `archived`, or to the bin), the new copy at `temp` or `original`,
/// whichever step the crash came after. Recovery never needs anything back from the bin: if the
/// original is gone and the whole new copy is waiting, the new copy is moved into place.
/// Returns (temp removed, completed, what was done).
fn finish_replace(rec: &InProgress, step: &ReplaceStep) -> (bool, bool, String) {
    let places_ok = temp_is_originals(&rec.temp, &step.original)
        && is_inside(&rec.root, &rec.temp)
        && is_inside(&rec.root, &step.original)
        && match step.goes {
            OldCopyGoes::Archive => is_in_archive(rec, &step.archived),
            OldCopyGoes::Aside => {
                aside_is_originals(&step.archived, &step.original)
                    && is_inside(&rec.root, &step.archived)
            }
            OldCopyGoes::Bin => step.archived.as_os_str().is_empty(),
        };
    if !places_ok {
        return (
            false,
            false,
            "left everything alone: the record does not match this backup folder's files and archive"
                .into(),
        );
    }
    let bin = step.goes == OldCopyGoes::Bin;
    let state = (|| -> Result<(bool, bool, bool), String> {
        Ok((
            plain_file(&step.original)?,
            if bin {
                false
            } else {
                plain_file(&step.archived)?
            },
            plain_file(&rec.temp)?,
        ))
    })();
    let (orig, arch, temp) = match state {
        Ok(s) => s,
        Err(e) => return (false, false, e),
    };
    let mv = |from: &Path, to: &Path| {
        crate::platform::rename_no_replace(from, to).map_err(|e| {
            format!(
                "could not move {} to {}: {e}; both copies are kept",
                from.display(),
                to.display()
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
            (false, true) => mv(&rec.temp, &step.original).map(|()| finished),
            // The old copy is still in place: nothing was moved. The unfinished new copy is
            // only a copy of the device's file, so it is removed and the next sync makes it
            // again.
            (true, true) => {
                let (removed, note) = remove_temp_if_ours(rec);
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
                "the changed file {} is gone from the backup and no new copy was left; the old copy should be in the {bin_name}",
                step.original.display()
            )),
        }
    } else {
        match (orig, arch, temp) {
            // Before the first move: the old copy is in place, the whole new copy is waiting.
            (true, false, true) => mv(&step.original, &step.archived)
                .and_then(|()| mv(&rec.temp, &step.original))
                .map(|()| finished),
            // After the first move.
            (false, true, true) => mv(&rec.temp, &step.original).map(|()| finished),
            // After both moves.
            (true, true, false) => Ok(format!(
                "the changed file had been replaced: the old copy is {where_old} and the new one is in place"
            )),
            _ => Err(format!(
                "the changed file {} was left as it is (the old copy {}, the moved copy {}, the new copy {}); nothing was deleted",
                step.original.display(),
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

/// Delete `rec.temp` only if it is plainly this app's temporary file: our suffix, a regular
/// file (not a link), inside the recorded backup folder.
fn remove_temp_if_ours(rec: &InProgress) -> (bool, String) {
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
    let meta = match fs::symlink_metadata(&rec.temp) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return (false, "no temporary file was left".into());
        }
        Err(e) => {
            return (
                false,
                format!("could not check {}: {e}", rec.temp.display()),
            );
        }
    };
    if is_link(&meta) || !meta.is_file() {
        return (
            false,
            format!("left {} alone: not a plain file", rec.temp.display()),
        );
    }
    let inside = match (
        rec.temp.parent().and_then(|p| fs::canonicalize(p).ok()),
        fs::canonicalize(&rec.root).ok(),
    ) {
        (Some(parent), Some(root)) => parent.starts_with(&root),
        _ => false,
    };
    if !inside {
        return (
            false,
            format!(
                "left {} alone: not inside the backup folder",
                rec.temp.display()
            ),
        );
    }
    match fs::remove_file(&rec.temp) {
        Ok(()) => (
            true,
            format!("removed the unfinished copy {}", rec.temp.display()),
        ),
        Err(e) => (
            false,
            format!("could not remove {}: {e}", rec.temp.display()),
        ),
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
