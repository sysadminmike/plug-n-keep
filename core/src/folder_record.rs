//! The device's own record, kept in its backup folder (DESIGN.md sections 6.2 and 9).
//!
//! The backup folder is the source of truth for the device that backs up to it: its name, kind,
//! settings, what recognises it (the signal history), the answers "not this device", when it was
//! seen and last synced. They are in `.plug-n-keep-device.json`, a hidden file beside the owner
//! marker, and the sync history is in `.plug-n-keep-history.jsonl` ([`crate::history`]). Copy
//! the folder to another PC, point the app at it ([`crate::registry::Registry::find_my_backups`]),
//! and nothing is lost.
//!
//! The record is a file of its own, not part of the marker, because the two change at different
//! rates and for different reasons. The marker is a claim on the folder, written once without
//! replacing anything and changed only by a hand-over or an archive rename; the record is
//! rewritten whenever a setting changes or a device is seen. Keeping them apart means an
//! ordinary save never touches the claim, and a marker written by an earlier version is read
//! as it always was.
//!
//! Both files are the app's own: the scan never counts them as backup files, and nothing ever
//! archives or bins them ([`crate::owner::is_marker_name`]). Every write goes through a temporary
//! file and an atomic move.
#![forbid(unsafe_code)]

use std::io;
use std::path::{Path, PathBuf};

use log::{info, warn};
use serde::{Deserialize, Serialize};

use crate::owner::{self, OwnerState};
use crate::paths::SafeRelPath;
use crate::registry::KnownDevice;
use crate::store::{BackupStore, Kind, LocalStore};

/// How many damaged copies of one file are kept before giving up.
const MAX_DAMAGED: u32 = 99;

/// The device record, in the top of the backup folder.
pub const DEVICE_FILE: &str = ".plug-n-keep-device.json";
/// The sync history, in the top of the backup folder.
pub const HISTORY_FILE: &str = ".plug-n-keep-history.jsonl";

/// Whether a folder can hold a device's record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fit {
    /// It is a folder, and it isn't another device's.
    Ok,
    /// It isn't there (an unplugged drive, a deleted or renamed folder).
    NoFolder,
    /// It is there, but it is another device's, or its marker can't be read.
    Foreign(String),
}

/// Whether the folder `dir` can hold the record of the device `id`: it exists, and its marker (if
/// it has one yet) is that device's.
pub fn fit(dir: &Path, id: &str) -> Fit {
    if !dir.is_dir() {
        return Fit::NoFolder;
    }
    fit_in(&LocalStore::new(dir), id)
}

/// [`fit`] for a store: it is there, and its marker (if it has one yet) is that device's.
pub fn fit_in(store: &dyn BackupStore, id: &str) -> Fit {
    if !store.root_exists().unwrap_or(false) {
        return Fit::NoFolder;
    }
    match owner::read_in(store) {
        OwnerState::Unclaimed => Fit::Ok,
        OwnerState::Owned(m) if m.device_id == id => Fit::Ok,
        OwnerState::Owned(m) => Fit::Foreign(format!(
            "this folder belongs to another device ({})",
            m.device_name
        )),
        OwnerState::Unreadable(e) => Fit::Foreign(format!("its marker can't be read ({e})")),
    }
}

/// The file as stored: the device, without its backup folder (that is wherever the file is).
#[derive(Debug, Serialize, Deserialize)]
struct DeviceFile {
    version: u32,
    device: serde_json::Value,
}

fn device_rel() -> SafeRelPath {
    SafeRelPath::parse(DEVICE_FILE).expect("the record name is a safe name")
}

/// The record of device `id` in `dir`. `Ok(None)` if the folder has none; `Err` if there is one
/// that can't be used (damaged, not a plain file, or another device's). Only reads.
pub fn read_device(dir: &Path, id: &str) -> Result<Option<KnownDevice>, String> {
    let device = read_device_in(&LocalStore::new(dir), id)?;
    Ok(device.map(|mut d| {
        d.backup_dir = Some(dir.to_path_buf());
        d
    }))
}

/// [`read_device`] on a store. The device's `backup_dir` is left unset.
pub fn read_device_in(store: &dyn BackupStore, id: &str) -> Result<Option<KnownDevice>, String> {
    let rel = device_rel();
    match store.stat(&rel).map_err(|e| e.to_string())? {
        None => return Ok(None),
        Some(s) if s.kind != Kind::File => return Err("it is not a plain file".into()),
        Some(_) => {}
    }
    let Some((bytes, _)) = store.read(&rel).map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let file: DeviceFile =
        serde_json::from_slice(&bytes).map_err(|e| format!("it is damaged ({e})"))?;
    let mut device: KnownDevice =
        serde_json::from_value(file.device.clone()).map_err(|e| format!("it is damaged ({e})"))?;
    if device.id != id {
        return Err(format!(
            "it is the record of another device ({})",
            device.name
        ));
    }
    crate::registry::fill_from_raw_one(&file.device, &mut device);
    Ok(Some(device))
}

/// Write the record of `device` into `dir`, in one step. Nothing is written if the file already
/// says the same. A record that is there and can't be read is kept beside it first as
/// `.plug-n-keep-device.json.damaged` (numbered if there already is one). Returns whether the
/// file was written.
pub fn write_device(dir: &Path, device: &KnownDevice) -> io::Result<bool> {
    write_device_in(&LocalStore::new(dir), device)
}

/// [`write_device`] on a store.
pub fn write_device_in(store: &dyn BackupStore, device: &KnownDevice) -> io::Result<bool> {
    let mut value = serde_json::to_value(device).map_err(io::Error::other)?;
    if let Some(o) = value.as_object_mut() {
        o.remove("backup_dir");
        o.remove("s3");
    }
    let json = serde_json::to_vec_pretty(&DeviceFile {
        version: 1,
        device: value,
    })
    .map_err(io::Error::other)?;
    let rel = device_rel();
    if let Some((old, _)) = store.read(&rel)? {
        if old == json {
            return Ok(false);
        }
        if serde_json::from_slice::<DeviceFile>(&old).is_err() {
            // The damaged file is only replaced once a copy of it is kept.
            keep_damaged(store, &rel, &old)?;
        }
    }
    store.write_replace(&rel, &json, None)?;
    Ok(true)
}

/// Keep the unreadable file `old` that was at `rel` beside it, under a name that is free.
pub(crate) fn keep_damaged(
    store: &dyn BackupStore,
    rel: &SafeRelPath,
    old: &[u8],
) -> io::Result<()> {
    let name = rel.file_name().to_string();
    for n in 1..=MAX_DAMAGED {
        let aside = if n == 1 {
            format!("{name}.damaged")
        } else {
            format!("{name}.damaged{n}")
        };
        let aside_rel = SafeRelPath::parse(&aside).map_err(io::Error::other)?;
        match store.write_new(&aside_rel, old) {
            Ok(_) => {
                warn!(
                    "{name} in {} was damaged; it is kept as {aside}",
                    store.describe()
                );
                return Ok(());
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other(format!(
        "{MAX_DAMAGED} damaged copies of {name} are already kept; not replacing it"
    )))
}

/// The history file in a backup folder.
pub fn history_file(dir: &Path) -> PathBuf {
    dir.join(HISTORY_FILE)
}

/// Put the history in `from` into the folder `dir`'s history file (merged with what is there).
/// `from` is left as it is. Returns whether the folder's file changed.
pub fn copy_history_into(from: &Path, dir: &Path) -> io::Result<bool> {
    let to = history_file(dir);
    if from == to {
        return Ok(false);
    }
    copy_history_into_in(from, &LocalStore::new(dir))
}

/// [`copy_history_into`] for a store: the history file `from` (on this PC) is merged into the
/// store's history file.
pub fn copy_history_into_in(from: &Path, store: &dyn BackupStore) -> io::Result<bool> {
    let changed = crate::history::merge_from_file(from, store)?;
    if changed {
        info!("Copied the sync history into {}", store.describe());
    }
    Ok(changed)
}

/// Set a settings-folder history file aside once its contents are in a backup folder: renamed
/// to `<name>.moved`, never deleted. No such file is nothing to do.
pub fn retire_profile_history(path: &Path) {
    if !path.is_file() {
        return;
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let to = path.with_file_name(format!("{name}.moved"));
    if let Err(e) = crate::platform::rename_replace(path, &to) {
        warn!("Could not set {} aside: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signals::{DeviceKind, Signals, View};
    use crate::store::mem::MemStore;

    fn device(id: &str, name: &str) -> KnownDevice {
        KnownDevice::with_id(
            id.into(),
            DeviceKind::Mtp,
            name,
            &Signals::default(),
            "2026-01-01T00:00:00Z",
        )
    }

    #[test]
    fn a_record_round_trips_and_an_identical_one_is_not_rewritten() {
        let store = MemStore::new();
        assert!(read_device_in(&store, "a").unwrap().is_none());
        let d = device("a", "My phone");
        assert!(write_device_in(&store, &d).unwrap());
        assert!(!write_device_in(&store, &d).unwrap());
        assert_eq!(store.calls("write_replace"), 1);
        let back = read_device_in(&store, "a").unwrap().unwrap();
        assert_eq!((back.id.as_str(), back.name.as_str()), ("a", "My phone"));
        assert!(back.backup_dir.is_none());
        assert!(write_device_in(&store, &device("a", "Renamed")).unwrap());
        assert_eq!(
            read_device_in(&store, "a").unwrap().unwrap().name,
            "Renamed"
        );
        // Another device's record is refused, not read.
        assert!(read_device_in(&store, "b").is_err());
    }

    #[test]
    fn a_damaged_record_is_set_aside_never_overwritten() {
        let store = MemStore::new();
        store.insert(DEVICE_FILE, b"garbage one", None);
        assert!(read_device_in(&store, "a").is_err());
        assert!(write_device_in(&store, &device("a", "x")).unwrap());
        let first = format!("{DEVICE_FILE}.damaged");
        let second = format!("{DEVICE_FILE}.damaged2");
        assert_eq!(store.get(&first).unwrap(), b"garbage one");
        // Damaged again: the next free name is used and the first copy stays.
        store.insert(DEVICE_FILE, b"garbage two", None);
        assert!(write_device_in(&store, &device("a", "y")).unwrap());
        assert_eq!(store.get(&first).unwrap(), b"garbage one");
        assert_eq!(store.get(&second).unwrap(), b"garbage two");
        assert_eq!(read_device_in(&store, "a").unwrap().unwrap().name, "y");
    }

    #[test]
    fn a_damaged_record_is_not_replaced_when_no_name_is_free() {
        let store = MemStore::new();
        store.insert(DEVICE_FILE, b"garbage", None);
        store.insert(&format!("{DEVICE_FILE}.damaged"), b"1", None);
        for n in 2..=MAX_DAMAGED {
            store.insert(&format!("{DEVICE_FILE}.damaged{n}"), b"n", None);
        }
        assert!(write_device_in(&store, &device("a", "x")).is_err());
        assert_eq!(store.get(DEVICE_FILE).unwrap(), b"garbage");
    }

    #[test]
    fn a_folder_fits_unless_it_is_another_devices_or_its_marker_is_bad() {
        let store = MemStore::new();
        assert_eq!(fit_in(&store, "a"), Fit::Ok);
        let m = owner::FolderOwner::new("a", "mine", Signals::default(), View::Files, "now");
        owner::write_in(&store, &m).unwrap();
        assert_eq!(fit_in(&store, "a"), Fit::Ok);
        assert!(matches!(fit_in(&store, "b"), Fit::Foreign(_)));
        store.insert(owner::MARKER, b"junk", None);
        assert!(matches!(fit_in(&store, "a"), Fit::Foreign(_)));
    }

    #[test]
    fn a_profile_history_is_copied_into_a_store_once() {
        let t = tempfile::tempdir().unwrap();
        let from = t.path().join("h.jsonl");
        std::fs::write(
            &from,
            "{\"started\":\"2026-01-01T00:00:00Z\",\"copied\":1}\n",
        )
        .unwrap();
        let store = MemStore::new();
        assert!(copy_history_into_in(&from, &store).unwrap());
        assert!(!copy_history_into_in(&from, &store).unwrap());
        let rel = SafeRelPath::parse(HISTORY_FILE).unwrap();
        assert_eq!(crate::history::load_in(&store, &rel).len(), 1);
    }
}
