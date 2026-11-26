//! Settings that belong to a whole bucket, not to one PC (DESIGN.md sections 9 and 11.7).
//!
//! In S3 mode the bucket is the source of truth for everything but the connection. This one small
//! object, `.plug-n-keep-settings.json` in the base folder, holds what new devices start with and
//! the devices that were forgotten. Every PC that uses the bucket reads it, so a new PC needs
//! only the connection. It is an app file: the scan never counts it as backup content
//! ([`crate::owner::is_marker_name`]).
//!
//! It is written only if it is as it was read (`If-Match`), or, the first time, only if it is
//! not there (`If-None-Match`). Another PC's change is never overwritten.
#![forbid(unsafe_code)]

use std::io;

use serde::{Deserialize, Serialize};

use crate::paths::SafeRelPath;
use crate::state::NewDeviceDefaults;
use crate::store::{BackupStore, Tag, is_conflict};

/// The settings object's name, in the base folder.
pub const SETTINGS_FILE: &str = ".plug-n-keep-settings.json";

/// How many times a change that is always safe to repeat (forgetting a device) tries again when
/// another PC wrote first.
const RETRIES: usize = 4;

/// What the bucket keeps for every PC that uses it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BucketSettings {
    pub version: u32,
    /// What a new device starts with.
    pub new_devices: NewDeviceDefaults,
    /// IDs of devices taken off the list. Their folders are left alone; "Add an existing backup…"
    /// brings them back.
    pub forgotten: Vec<String>,
}

/// The settings as read, with the tag they must still have to be replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    pub settings: BucketSettings,
    pub tag: Tag,
}

/// Why a write did not happen.
#[derive(Debug)]
pub enum WriteError {
    /// Another PC changed the settings since they were read (or made them first).
    Conflict,
    Other(io::Error),
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::Conflict => f.write_str("the settings were changed on another PC"),
            WriteError::Other(e) => write!(f, "{}", crate::store::explain(e)),
        }
    }
}

fn rel() -> SafeRelPath {
    SafeRelPath::parse(SETTINGS_FILE).expect("the settings name is a safe name")
}

/// Read the settings from the store at the base folder. `Ok(None)`: there are none yet. A file
/// that can't be used is an error (`InvalidData`), and is never replaced.
pub fn read(store: &dyn BackupStore) -> io::Result<Option<Loaded>> {
    let rel = rel();
    let Some((bytes, tag)) = store.read(&rel)? else {
        return Ok(None);
    };
    let bad = |e: String| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{SETTINGS_FILE} in the bucket can't be used ({e})"),
        )
    };
    let raw: serde_json::Value = serde_json::from_slice(&bytes).map_err(|e| bad(e.to_string()))?;
    let mut settings: BucketSettings =
        serde_json::from_value(raw.clone()).map_err(|e| bad(e.to_string()))?;
    settings.new_devices.deleted_files_not_understood = crate::registry::deleted_files_unknown(
        raw.get("new_devices").and_then(|n| n.get("deleted_files")),
    );
    Ok(Some(Loaded { settings, tag }))
}

/// Write the settings. `expect` is the tag they were read with, or `None` if they were not
/// there. Anything else in the bucket is a [`WriteError::Conflict`].
pub fn write(
    store: &dyn BackupStore,
    settings: &BucketSettings,
    expect: Option<&Tag>,
) -> Result<Tag, WriteError> {
    let mut out = settings.clone();
    out.version = 1;
    let json =
        serde_json::to_vec_pretty(&out).map_err(|e| WriteError::Other(io::Error::other(e)))?;
    let rel = rel();
    match expect {
        None => store.write_new(&rel, &json).map_err(|e| {
            if e.kind() == io::ErrorKind::AlreadyExists {
                WriteError::Conflict
            } else {
                WriteError::Other(e)
            }
        }),
        Some(tag) => store.write_replace(&rel, &json, Some(tag)).map_err(|e| {
            if is_conflict(&e) {
                WriteError::Conflict
            } else {
                WriteError::Other(e)
            }
        }),
    }
}

/// Change the settings with `change` (which says whether it changed anything), reading them
/// again and trying again if another PC wrote in between. For changes that are safe to repeat,
/// such as adding or removing a forgotten ID. `start` is what a bucket with no settings yet
/// starts from. Returns the settings as they are now, with their tag.
pub fn update(
    store: &dyn BackupStore,
    start: &BucketSettings,
    change: &dyn Fn(&mut BucketSettings) -> bool,
) -> io::Result<Loaded> {
    for _ in 0..RETRIES {
        let (mut settings, tag) = match read(store)? {
            Some(l) => (l.settings, Some(l.tag)),
            None => (start.clone(), None),
        };
        let changed = change(&mut settings);
        match (changed, tag) {
            (false, Some(tag)) => return Ok(Loaded { settings, tag }),
            (_, tag) => match write(store, &settings, tag.as_ref()) {
                Ok(tag) => return Ok(Loaded { settings, tag }),
                Err(WriteError::Conflict) => {}
                Err(WriteError::Other(e)) => return Err(e),
            },
        }
    }
    Err(io::Error::other(
        "The settings in the bucket keep changing; try again in a moment",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::DeletedFiles;
    use crate::store::mem::MemStore;

    fn sample() -> BucketSettings {
        let mut s = BucketSettings::default();
        s.new_devices.archive_name = "_Old".into();
        s.new_devices.scan_automatically = true;
        s.new_devices.deleted_files = DeletedFiles::LeaveInPlace;
        s.new_devices.ignore_names = vec!["a.tmp".into()];
        s.forgotten = vec!["dev-1".into()];
        s
    }

    #[test]
    fn nothing_there_reads_as_none_and_a_round_trip_keeps_everything() {
        let store = MemStore::new();
        assert!(read(&store).unwrap().is_none());
        let tag = write(&store, &sample(), None).unwrap();
        let got = read(&store).unwrap().unwrap();
        assert_eq!(got.tag, tag);
        assert_eq!(got.settings.new_devices, sample().new_devices);
        assert_eq!(got.settings.forgotten, ["dev-1"]);
        assert_eq!(got.settings.version, 1);
    }

    #[test]
    fn a_write_that_lost_the_race_is_a_conflict_and_changes_nothing() {
        let store = MemStore::new();
        let first = write(&store, &sample(), None).unwrap();
        // The first write when it is already there.
        assert!(matches!(
            write(&store, &BucketSettings::default(), None),
            Err(WriteError::Conflict)
        ));
        // Another PC wrote after this one read.
        let mut theirs = sample();
        theirs.new_devices.archive_name = "_Theirs".into();
        write(&store, &theirs, Some(&first)).unwrap();
        let mut mine = sample();
        mine.new_devices.archive_name = "_Mine".into();
        assert!(matches!(
            write(&store, &mine, Some(&first)),
            Err(WriteError::Conflict)
        ));
        assert_eq!(
            read(&store)
                .unwrap()
                .unwrap()
                .settings
                .new_devices
                .archive_name,
            "_Theirs"
        );
    }

    #[test]
    fn a_damaged_file_is_refused_and_left_alone() {
        let store = MemStore::new();
        store.insert(SETTINGS_FILE, b"not json", None);
        let e = read(&store).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert!(update(&store, &BucketSettings::default(), &|_| true).is_err());
        let rel = rel();
        assert_eq!(store.read(&rel).unwrap().unwrap().0, b"not json");
    }

    #[test]
    fn an_unknown_deleted_files_value_is_flagged_not_fatal() {
        let store = MemStore::new();
        store.insert(
            SETTINGS_FILE,
            br#"{"new_devices":{"deleted_files":"shred"}}"#,
            None,
        );
        let l = read(&store).unwrap().unwrap();
        assert_eq!(
            l.settings.new_devices.deleted_files,
            DeletedFiles::LeaveInPlace
        );
        assert!(l.settings.new_devices.deleted_files_not_understood);
    }

    #[test]
    fn update_creates_the_file_and_adds_ids_once() {
        let store = MemStore::new();
        let add = |id: &'static str| {
            move |s: &mut BucketSettings| {
                if s.forgotten.iter().any(|f| f == id) {
                    false
                } else {
                    s.forgotten.push(id.to_string());
                    true
                }
            }
        };
        let l = update(&store, &sample_without_forgotten(), &add("x")).unwrap();
        assert_eq!(l.settings.forgotten, ["x"]);
        assert_eq!(l.settings.new_devices.archive_name, "_Old");
        let again = update(&store, &BucketSettings::default(), &add("x")).unwrap();
        assert_eq!(again.tag, l.tag, "nothing was written the second time");
        let more = update(&store, &BucketSettings::default(), &add("y")).unwrap();
        assert_eq!(more.settings.forgotten, ["x", "y"]);
    }

    fn sample_without_forgotten() -> BucketSettings {
        BucketSettings {
            forgotten: Vec::new(),
            ..sample()
        }
    }

    #[test]
    fn the_settings_object_is_never_backup_content() {
        assert!(crate::owner::is_marker_name(SETTINGS_FILE));
        assert!(crate::owner::is_marker_name(".Plug-N-Keep-Settings.json"));
    }
}
