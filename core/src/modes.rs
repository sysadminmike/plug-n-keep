//! The app's two modes, Local and S3. Each has its own settings folder (and so its own list of
//! devices): Local uses the settings folder itself, exactly as it always did, and S3 uses the
//! `s3` folder inside it. The mode last used is kept in a tiny `mode.json` in the settings folder,
//! so no other file changes for someone who only uses folders.
#![forbid(unsafe_code)]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use log::{info, warn};
use serde::{Deserialize, Serialize};

use crate::state::{InProgress, StateDir};
use crate::store::Destination;

/// The S3 mode's folder, inside the settings folder.
pub const S3_DIR_NAME: &str = "s3";
const MODE_FILE: &str = "mode.json";
const DEVICES_FILE: &str = "devices.json";
const IN_PROGRESS_FILE: &str = "in_progress.json";
const CONFIG_FILE: &str = "config.json";
/// Added to the name of a S3 settings file kept as it was before the bucket became the source of truth.
const PRE_BUCKET_SUFFIX: &str = ".pre-bucket-source";
/// `devices.json` as it was before devices were moved out of it.
const PRE_MOVE_COPY: &str = "devices.json.pre-s3-move";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Local,
    S3,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Local => "Local",
            Mode::S3 => "S3",
        }
    }
}

#[derive(Serialize, Deserialize)]
struct ModeFile {
    mode: Mode,
}

/// The settings folder of the S3 mode, given the base settings folder.
pub fn s3_dir(base: &Path) -> PathBuf {
    base.join(S3_DIR_NAME)
}

/// The mode last used. A missing, unreadable or unknown value gives Local.
pub fn load_mode(base: &StateDir) -> Mode {
    fs::read_to_string(base.path().join(MODE_FILE))
        .ok()
        .and_then(|t| serde_json::from_str::<ModeFile>(&t).ok())
        .map_or(Mode::Local, |f| f.mode)
}

/// Remember the mode. Local removes nothing and writes nothing if no file was there before.
pub fn save_mode(base: &StateDir, mode: Mode) -> io::Result<()> {
    if mode == Mode::Local && !base.path().join(MODE_FILE).exists() {
        return Ok(());
    }
    let json = serde_json::to_vec_pretty(&ModeFile { mode }).map_err(io::Error::other)?;
    crate::state::write_atomic_at(&base.path().join(MODE_FILE), &json)
}

/// What [`move_s3_devices`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MoveReport {
    /// Names of the devices moved into the S3 list.
    pub moved: Vec<String>,
    /// Names left in the Local list because the S3 list already has a different device with
    /// that ID.
    pub skipped: Vec<String>,
}

fn read_devices(dir: &Path) -> Option<(serde_json::Value, Vec<serde_json::Value>)> {
    let text = fs::read_to_string(dir.join(DEVICES_FILE)).ok()?;
    let file: serde_json::Value = serde_json::from_str(&text).ok()?;
    let list = file.get("devices")?.as_array()?.clone();
    Some((file, list))
}

fn has_bucket(v: &serde_json::Value) -> bool {
    v.get("s3").is_some_and(|s| s.is_object())
}

/// Once, on load: devices in the Local list that back up to a bucket move to the S3 list, and the
/// default S3 location in the Local settings moves to the S3 settings (if they have none). Only
/// the app's own small files change; nothing in a backup folder or bucket is touched, and
/// nothing is deleted: the old `devices.json` is first copied to `devices.json.pre-s3-move`.
///
/// A device whose ID is already in the S3 list with a different record stays where it is (and is
/// reported in [`MoveReport::skipped`]): neither record is lost, the user sorts it out. One that is
/// there already with the same record is just taken off the Local list (a move that was cut short).
pub fn move_s3_devices(local: &StateDir, s3: &StateDir) -> io::Result<MoveReport> {
    let mut report = MoveReport::default();
    move_default_location(local, s3);
    let Some((mut local_file, local_list)) = read_devices(local.path()) else {
        return Ok(report);
    };
    if !local_list.iter().any(has_bucket) {
        return Ok(report);
    }
    // An S3 list that exists but can't be read is not touched: the registry sets it aside.
    let (mut s3_file, mut s3_list) = match read_devices(s3.path()) {
        Some(x) => x,
        None if s3.path().join(DEVICES_FILE).exists() => {
            warn!("The S3 device list can't be read, so no devices were moved into it");
            return Ok(report);
        }
        None => (
            serde_json::json!({ "version": 2, "devices": [] }),
            Vec::new(),
        ),
    };
    let mut keep = Vec::new();
    let mut moved_ids = Vec::new();
    for v in local_list {
        if !has_bucket(&v) {
            keep.push(v);
            continue;
        }
        let id = v.get("id").and_then(|i| i.as_str()).unwrap_or_default();
        let name = v
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("a device")
            .to_string();
        match s3_list
            .iter()
            .find(|o| o.get("id").and_then(|i| i.as_str()) == Some(id))
        {
            Some(same) if *same == v => moved_ids.push(id.to_string()),
            Some(_) => {
                warn!(
                    "\"{name}\" backs up to a bucket but the S3 list already has a different device with its ID, so it stays in the Local list"
                );
                report.skipped.push(name);
                keep.push(v);
            }
            None => {
                moved_ids.push(id.to_string());
                report.moved.push(name);
                s3_list.push(v);
            }
        }
    }
    if moved_ids.is_empty() {
        return Ok(report);
    }
    // Keep what is there before anything is rewritten; never replaced.
    let copy = local.path().join(PRE_MOVE_COPY);
    if !copy.exists()
        && let Ok(old) = fs::read(local.path().join(DEVICES_FILE))
    {
        crate::state::write_new_file(&copy, &old)?;
    }
    s3_file["devices"] = serde_json::Value::Array(s3_list);
    if s3_file.get("version").is_none() {
        s3_file["version"] = 2.into();
    }
    // The S3 list first: a cut-short move leaves a device in both, which the next load finishes.
    s3.write_atomic(
        DEVICES_FILE,
        &serde_json::to_vec_pretty(&s3_file).map_err(io::Error::other)?,
    )?;
    local_file["devices"] = serde_json::Value::Array(keep);
    local.write_atomic(
        DEVICES_FILE,
        &serde_json::to_vec_pretty(&local_file).map_err(io::Error::other)?,
    )?;
    move_crash_record(local, s3, &moved_ids);
    info!(
        "Moved {} that back up to a bucket from the Local list to the S3 list",
        crate::counted(report.moved.len(), "device", "devices")
    );
    Ok(report)
}

/// What S3 mode keeps on this PC is now only the connection, the crash record and a cache of the
/// device list and of the settings the bucket holds. Before the app first rewrites those files in
/// the new way, each is copied once beside itself (`config.json.pre-bucket-source`,
/// `devices.json.pre-bucket-source`). A copy that is there is never replaced, and nothing is
/// deleted. Failures are only logged.
pub fn keep_pre_bucket_copies(s3: &StateDir) {
    for name in [CONFIG_FILE, DEVICES_FILE] {
        let from = s3.path().join(name);
        let to = s3.path().join(format!("{name}{PRE_BUCKET_SUFFIX}"));
        if to.exists() {
            continue;
        }
        let Ok(old) = fs::read(&from) else {
            continue;
        };
        match crate::state::write_new_file(&to, &old) {
            Ok(()) => info!("Kept a copy of the S3 {name} as {}", to.display()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => warn!("Could not keep a copy of the S3 {name}: {e}"),
        }
    }
}

/// The default S3 location in the Local settings becomes the S3 mode's connection, if that has
/// none yet. Failures are only logged.
fn move_default_location(local: &StateDir, s3: &StateDir) {
    let mut lc = local.config();
    let Some(loc) = lc.default_s3.clone() else {
        return;
    };
    let mut sc = s3.config();
    if sc.default_s3.is_some() {
        return;
    }
    sc.default_s3 = Some(loc);
    lc.default_s3 = None;
    if let Err(e) = s3.save_config(&sc) {
        warn!("Could not move the default S3 location to the S3 settings: {e}");
        return;
    }
    if let Err(e) = local.save_config(&lc) {
        warn!("Could not take the default S3 location out of the Local settings: {e}");
    }
}

/// A crash record in the Local settings about a bucket device that moved goes with it.
fn move_crash_record(local: &StateDir, s3: &StateDir, moved_ids: &[String]) {
    let from = local.path().join(IN_PROGRESS_FILE);

    let to = s3.path().join(IN_PROGRESS_FILE);
    let Ok(text) = fs::read_to_string(&from) else {
        return;
    };
    let Ok(rec) = serde_json::from_str::<InProgress>(&text) else {
        return;
    };
    if !matches!(rec.place(), Destination::S3(_)) || !moved_ids.contains(&rec.device) || to.exists()
    {
        return;
    }
    if let Err(e) = crate::state::write_new_file(&to, text.as_bytes()) {
        warn!("Could not move the crash record to the S3 settings: {e}");
        return;
    }
    if let Err(e) = local.end_copy() {
        warn!("Could not remove the crash record after moving it: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::S3Config;

    fn dirs() -> (tempfile::TempDir, StateDir, StateDir) {
        let t = tempfile::tempdir().unwrap();
        let local = StateDir::open(t.path().join("settings")).unwrap();
        let s3 = StateDir::open(s3_dir(local.path())).unwrap();
        (t, local, s3)
    }

    fn bucket() -> serde_json::Value {
        serde_json::json!({"endpoint": "http://h:9000", "region": "us-east-1", "bucket": "b", "prefix": "p/x/", "path_style": true})
    }

    fn write(dir: &StateDir, devices: Vec<serde_json::Value>) {
        let file = serde_json::json!({"version": 2, "devices": devices});
        fs::write(
            dir.path().join(DEVICES_FILE),
            serde_json::to_vec_pretty(&file).unwrap(),
        )
        .unwrap();
    }

    fn ids(dir: &StateDir) -> Vec<String> {
        read_devices(dir.path())
            .map(|(_, l)| {
                l.iter()
                    .map(|v| v["id"].as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn dev(id: &str, s3: bool) -> serde_json::Value {
        let mut v = serde_json::json!({"id": id, "name": format!("Dev {id}"), "settings": {}});
        if s3 {
            v["s3"] = bucket();
        } else {
            v["backup_dir"] = "C:/b".into();
        }
        v
    }

    #[test]
    fn the_mode_defaults_to_local_and_is_remembered() {
        let (_t, local, _s3) = dirs();
        assert_eq!(load_mode(&local), Mode::Local);
        // Choosing Local when nothing was saved writes nothing.
        save_mode(&local, Mode::Local).unwrap();
        assert!(!local.path().join(MODE_FILE).exists());
        save_mode(&local, Mode::S3).unwrap();
        assert_eq!(load_mode(&local), Mode::S3);
        save_mode(&local, Mode::Local).unwrap();
        assert_eq!(load_mode(&local), Mode::Local);
        fs::write(local.path().join(MODE_FILE), "{\"mode\":\"tape\"}").unwrap();
        assert_eq!(load_mode(&local), Mode::Local);
    }

    #[test]
    fn bucket_devices_move_once_and_folder_devices_stay() {
        let (_t, local, s3) = dirs();
        write(&local, vec![dev("a", false), dev("b", true)]);
        let r = move_s3_devices(&local, &s3).unwrap();
        assert_eq!(r.moved, ["Dev b"]);
        assert_eq!(ids(&local), ["a"]);
        assert_eq!(ids(&s3), ["b"]);
        assert!(local.path().join(PRE_MOVE_COPY).exists());
        // Again: nothing more.
        assert_eq!(move_s3_devices(&local, &s3).unwrap(), MoveReport::default());
        assert_eq!(ids(&s3), ["b"]);
    }

    #[test]
    fn a_device_the_s3_list_has_differently_stays_put_and_is_reported() {
        let (_t, local, s3) = dirs();
        let mut other = dev("b", true);
        other["name"] = "Renamed".into();
        write(&s3, vec![other.clone()]);
        write(&local, vec![dev("b", true)]);
        let r = move_s3_devices(&local, &s3).unwrap();
        assert!(r.moved.is_empty());
        assert_eq!(r.skipped, ["Dev b"]);
        assert_eq!(ids(&local), ["b"]);
        assert_eq!(read_devices(s3.path()).unwrap().1, [other]);
    }

    #[test]
    fn a_cut_short_move_is_finished() {
        let (_t, local, s3) = dirs();
        write(&s3, vec![dev("b", true)]);
        write(&local, vec![dev("b", true)]);
        move_s3_devices(&local, &s3).unwrap();
        assert!(ids(&local).is_empty());
        assert_eq!(ids(&s3), ["b"]);
    }

    #[test]
    fn the_s3_files_are_copied_once_before_they_are_used_the_new_way() {
        let (_t, _local, s3) = dirs();
        // Nothing there: nothing made.
        keep_pre_bucket_copies(&s3);
        assert!(!s3.path().join("config.json.pre-bucket-source").exists());
        fs::write(s3.path().join("config.json"), "{\"a\":1}").unwrap();
        write(&s3, vec![dev("b", true)]);
        keep_pre_bucket_copies(&s3);
        let kept = s3.path().join("config.json.pre-bucket-source");
        assert_eq!(fs::read_to_string(&kept).unwrap(), "{\"a\":1}");
        assert!(s3.path().join("devices.json.pre-bucket-source").exists());
        // Changed since: the first copy stays as it was.
        fs::write(s3.path().join("config.json"), "{\"a\":2}").unwrap();
        keep_pre_bucket_copies(&s3);
        assert_eq!(fs::read_to_string(&kept).unwrap(), "{\"a\":1}");
        assert_eq!(
            fs::read_to_string(s3.path().join("config.json")).unwrap(),
            "{\"a\":2}"
        );
    }

    #[test]
    fn the_default_location_moves_to_the_s3_settings() {
        let (_t, local, s3) = dirs();
        let mut c = local.config();
        c.default_s3 = Some(S3Config {
            provider: Default::default(),
            account_id: None,
            endpoint: "http://h:9000".into(),
            region: "us-east-1".into(),
            bucket: "b".into(),
            prefix: "base/".into(),
            profile: None,
            credentials_file: None,
            path_style: true,
            ca_file: None,
        });
        local.save_config(&c).unwrap();
        move_s3_devices(&local, &s3).unwrap();
        assert!(local.config().default_s3.is_none());
        assert_eq!(s3.config().default_s3.unwrap().prefix, "base/");
        let text = fs::read_to_string(local.path().join("config.json")).unwrap();
        assert!(!text.contains("s3"), "{text}");
    }
}
