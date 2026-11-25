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

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use log::{info, warn};
use serde::{Deserialize, Serialize};

use crate::owner::{self, OwnerState};
use crate::paths::is_link;
use crate::registry::KnownDevice;

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
    match owner::read(dir) {
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

fn plain_file(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.to_string()),
        Ok(m) if is_link(&m) || !m.is_file() => Err("it is not a plain file".into()),
        Ok(_) => Ok(true),
    }
}

/// The record of device `id` in `dir`. `Ok(None)` if the folder has none; `Err` if there is one
/// that can't be used (damaged, not a plain file, or another device's). Only reads.
pub fn read_device(dir: &Path, id: &str) -> Result<Option<KnownDevice>, String> {
    let path = dir.join(DEVICE_FILE);
    if !plain_file(&path)? {
        return Ok(None);
    }
    let text = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let file: DeviceFile =
        serde_json::from_str(&text).map_err(|e| format!("it is damaged ({e})"))?;
    let mut device: KnownDevice =
        serde_json::from_value(file.device.clone()).map_err(|e| format!("it is damaged ({e})"))?;
    if device.id != id {
        return Err(format!(
            "it is the record of another device ({})",
            device.name
        ));
    }
    crate::registry::fill_from_raw_one(&file.device, &mut device);
    device.backup_dir = Some(dir.to_path_buf());
    Ok(Some(device))
}

/// Write the record of `device` into `dir`, in one step. Nothing is written if the file already
/// says the same. A record that is there and can't be read is kept beside it first as
/// `.plug-n-keep-device.json.damaged` (numbered if there already is one). Returns whether the
/// file was written.
pub fn write_device(dir: &Path, device: &KnownDevice) -> io::Result<bool> {
    let mut value = serde_json::to_value(device).map_err(io::Error::other)?;
    if let Some(o) = value.as_object_mut() {
        o.remove("backup_dir");
    }
    let json = serde_json::to_vec_pretty(&DeviceFile {
        version: 1,
        device: value,
    })
    .map_err(io::Error::other)?;
    let path = dir.join(DEVICE_FILE);
    match fs::read(&path) {
        Ok(old) if old == json => return Ok(false),
        Ok(old) => {
            if serde_json::from_slice::<DeviceFile>(&old).is_err() {
                keep_damaged(&path, &old);
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    crate::state::write_atomic_at(&path, &json)?;
    Ok(true)
}

/// Keep the unreadable file `old` that was at `path` beside it, under a name that is free.
fn keep_damaged(path: &Path, old: &[u8]) {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut aside = path.with_file_name(format!("{name}.damaged"));
    let mut n = 1;
    while aside.exists() {
        n += 1;
        aside = path.with_file_name(format!("{name}.damaged{n}"));
    }
    match crate::state::write_new_file(&aside, old) {
        Ok(()) => warn!(
            "{name} in {} was damaged; it is kept as {}",
            path.parent()
                .map_or_else(String::new, |p| p.display().to_string()),
            aside.display()
        ),
        Err(e) => warn!("Could not keep the damaged {name}: {e}"),
    }
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
    let changed = crate::history::merge_into(from, &to)?;
    if changed {
        info!("Copied the sync history into {}", dir.display());
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
