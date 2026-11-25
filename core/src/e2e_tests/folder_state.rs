//! Where the app keeps its state (DESIGN.md section 9), end to end: the backup folder is the
//! source of truth for the device that backs up to it, and the app-data folder only points at
//! it. Each test's doc comment says which promise it guards.

use super::*;
use crate::folder_record::{self, DEVICE_FILE, HISTORY_FILE};
use crate::history::{self, History, SyncRecord};
use crate::owner::FolderOwner;
use crate::recognise::{Verdict, recognise};
use crate::registry::{KnownDevice, Registry, SyncSummary};
use crate::signals::{DeviceKind, SignalKind};

const NOW: &str = "2026-11-17T18:00:00Z";
const OLD_BACKUP: &str = "devices.json.pre-0.1-move";

fn sig(serial: &str) -> Signals {
    Signals {
        volume_serial: Some(serial.into()),
        ..Signals::default()
    }
}

/// A stick with settings that are not the defaults, a backup folder under `e.base/Backups`
/// (not made), and a last sync.
fn stick(e: &Env, name: &str, serial: &str) -> KnownDevice {
    let mut d = KnownDevice::new(DeviceKind::Storage, name, &sig(serial), NOW);
    d.backup_dir = Some(e.base.join("Backups").join(name));
    d.view = Some(View::Files);
    d.last_sync = Some(SyncSummary {
        when: NOW.into(),
        files: 7,
        bytes: 700,
    });
    d.settings.scan_automatically = true;
    d.settings.archive_name = "_Old stuff".into();
    d.settings.always_left_out.push("cache".into());
    d.reject(&sig("ZZZZ-9999"));
    d
}

/// The device's backup folder, made and claimed by a marker, as a first sync leaves it.
fn claim(d: &KnownDevice) -> PathBuf {
    let dir = d.backup_dir.clone().unwrap();
    fs::create_dir_all(&dir).unwrap();
    let marker = FolderOwner::new(&d.id, &d.name, sig("AAAA-1111"), View::Files, NOW);
    owner::write(&fs::canonicalize(&dir).unwrap(), &marker).unwrap();
    dir
}

fn record(day: u32) -> SyncRecord {
    SyncRecord {
        started: format!("2026-11-{day:02}T09:00:00Z"),
        copied: u64::from(day),
        ..SyncRecord::default()
    }
}

fn profile_text(state: &StateDir) -> String {
    fs::read_to_string(state.path().join("devices.json")).unwrap()
}

/// The device's record in its folder, and what `devices.json` keeps of it, are two different
/// things: the folder has everything, the profile only points.
#[test]
fn the_device_record_round_trips_through_its_backup_folder() {
    let e = env();
    let d = stick(&e, "Work stick", "AAAA-1111");
    let dir = claim(&d);
    let mut reg = Registry::default();
    reg.add(d.clone());
    reg.save(&e.state).unwrap();

    let text = profile_text(&e.state);
    assert!(text.contains("\"in_folder\": true"), "{text}");
    assert!(
        !text.contains("\"settings\""),
        "settings are not in the profile"
    );
    assert!(!text.contains("always_left_out"));
    assert!(dir.join(DEVICE_FILE).is_file());
    let stored = folder_record::read_device(&dir, &d.id).unwrap().unwrap();
    assert_eq!(stored, d);

    let loaded = Registry::load(&e.state);
    assert_eq!(loaded.devices(), std::slice::from_ref(&d));
    // Saving again with nothing changed writes nothing new into the folder.
    let before = fs::read(dir.join(DEVICE_FILE)).unwrap();
    loaded.save(&e.state).unwrap();
    assert_eq!(fs::read(dir.join(DEVICE_FILE)).unwrap(), before);
    assert!(
        files_under_all(&dir)
            .iter()
            .all(|f| !f.ends_with(".tmp") && !f.contains(".damaged")),
        "no temporary file is left behind"
    );
}

/// Losing the profile loses nothing: a fresh profile plus "Add an existing backup" gives back
/// the settings, the history and what recognises the device, and a device with the same signals
/// is recognised as before, including by a serial the app learned after setup.
#[test]
fn a_fresh_profile_and_an_added_backup_restore_settings_history_and_recognition() {
    let e = env();
    let mut d = stick(&e, "Work stick", "AAAA-1111");
    let dir = claim(&d);
    // A reformat the user confirmed: the old serial and the new one both recognise it.
    d.learn(&sig("BBBB-2222"), NOW);
    let mut reg = Registry::default();
    reg.add(d.clone());
    reg.save(&e.state).unwrap();
    for day in [1, 2, 3] {
        history::append_at(&reg.history_file(&d.id).unwrap(), &record(day)).unwrap();
    }

    // A new computer: only the backup folder came across.
    let fresh = StateDir::open(e.base.join("new-pc-state")).unwrap();
    let mut reg2 = Registry::load(&fresh);
    assert!(reg2.devices().is_empty());
    let report = reg2.find_my_backups(&dir, NOW);
    assert_eq!(report.added, ["Work stick"], "{report:?}");
    reg2.save(&fresh).unwrap();

    let restored = reg2.get(&d.id).unwrap();
    assert_eq!(restored, &d);
    assert_eq!(restored.settings.archive_name, "_Old stuff");
    assert!(restored.settings.scan_automatically);
    assert_eq!(restored.not_this, d.not_this);
    let hist = history::load_at(&reg2.history_file(&d.id).unwrap());
    assert_eq!(hist.len(), 3);
    assert_eq!(
        hist[0].copied, 3,
        "newest first, in the order they were synced"
    );
    for serial in ["AAAA-1111", "BBBB-2222"] {
        assert_eq!(
            recognise(&sig(serial), DeviceKind::Storage, reg2.devices()),
            Verdict::Recognised {
                id: d.id.clone(),
                by: SignalKind::VolumeSerial
            },
            "{serial}"
        );
    }
    // And a restart of the app on the new computer finds the same.
    let again = Registry::load(&fresh);
    assert_eq!(again.devices(), [d.clone()]);
    // The root that holds the backup works as well as the backup folder itself.
    let mut reg3 = Registry::default();
    let report = reg3.find_my_backups(&e.base.join("Backups"), NOW);
    assert_eq!(report.added, ["Work stick"]);
    assert_eq!(reg3.get(&d.id).unwrap(), &d);
}

/// A device whose folder is out of reach is still listed (never silently dropped), says why,
/// can't be used, keeps what the profile knew of it through saves, and is back when the folder
/// is.
#[test]
fn a_device_whose_folder_is_away_is_listed_not_usable_and_comes_back() {
    let e = env();
    let d = stick(&e, "Work stick", "AAAA-1111");
    let dir = claim(&d);
    let mut reg = Registry::default();
    reg.add(d.clone());
    reg.save(&e.state).unwrap();
    let away = e.base.join("Backups").join("unplugged");
    fs::rename(&dir, &away).unwrap();

    let mut loaded = Registry::load(&e.state);
    assert_eq!(loaded.devices().len(), 1);
    let listed = loaded.get(&d.id).unwrap();
    let why = listed.offline.clone().expect("offline");
    assert!(why.starts_with("Backup folder not found: "), "{why}");
    assert!(why.contains("Work stick"), "{why}");
    assert_eq!(listed.name, "Work stick");
    assert_eq!(listed.last_sync, d.last_sync);
    assert!(
        loaded.history_file(&d.id).is_none(),
        "no history is read or written while the folder is away"
    );
    // Saving does not change what the profile holds, and writes nothing into a folder.
    let before: serde_json::Value = serde_json::from_str(&profile_text(&e.state)).unwrap();
    loaded.save(&e.state).unwrap();
    let after: serde_json::Value = serde_json::from_str(&profile_text(&e.state)).unwrap();
    assert_eq!(before, after);
    assert!(!dir.exists(), "nothing recreates the folder");
    // The drive is plugged in again.
    assert!(!loaded.refresh_offline(&e.state));
    fs::rename(&away, &dir).unwrap();
    assert!(loaded.refresh_offline(&e.state));
    assert_eq!(loaded.get(&d.id).unwrap(), &d);
}

/// Migration from an earlier version: each device whose folder is there gets its record and
/// history in it, the old `devices.json` is kept, a device whose folder is away is left as it
/// was, and doing it all again changes nothing.
#[test]
fn migration_moves_records_and_history_into_the_folders_once() {
    let e = env();
    let a = stick(&e, "Phone", "AAAA-1111");
    let mut b = stick(&e, "Stick B", "BBBB-2222");
    b.settings.compare_by_date = false;
    let mut c = KnownDevice::new(DeviceKind::Storage, "New stick", &sig("CCCC-3333"), NOW);
    c.backup_dir = None;
    let dir_a = claim(&a);
    // Old format: version 1, every device whole, history in the settings folder.
    let old = serde_json::json!({
        "version": 1,
        "devices": [a, b, c],
    });
    let old_text = serde_json::to_string_pretty(&old).unwrap();
    fs::write(e.state.path().join("devices.json"), &old_text).unwrap();
    let h = History::new(&e.state);
    for day in [1, 2] {
        h.append(&a.id, &record(day)).unwrap();
    }
    h.append(&b.id, &record(5)).unwrap();

    let first = Registry::load(&e.state);
    // A: record and history are in its folder; the settings folder holds neither.
    assert_eq!(
        folder_record::read_device(&dir_a, &a.id).unwrap().unwrap(),
        a
    );
    assert_eq!(history::load_at(&dir_a.join(HISTORY_FILE)).len(), 2);
    let profile_history = h.profile_file(&a.id);
    assert!(!profile_history.exists());
    assert!(profile_history.with_extension("jsonl.moved").exists());
    // The old file is kept, whole.
    assert_eq!(
        fs::read_to_string(e.state.path().join(OLD_BACKUP)).unwrap(),
        old_text
    );
    // The profile is pointers for A, and the old entries for B (folder away) and C (no folder).
    let new: serde_json::Value = serde_json::from_str(&profile_text(&e.state)).unwrap();
    let entries = new["devices"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0]["in_folder"], true);
    assert!(entries[0].get("settings").is_none());
    assert_eq!(entries[1], old["devices"][1]);
    assert_eq!(entries[2], old["devices"][2]);
    // B is listed as away, with all it had; B's history still waits in the settings folder.
    let listed_b = first.get(&b.id).unwrap();
    assert!(listed_b.offline.is_some());
    assert!(!listed_b.settings.compare_by_date);
    assert_eq!(h.load(&b.id).len(), 1);
    assert!(first.get(&c.id).unwrap().offline.is_none());

    // Again: nothing changes.
    let folder_before = fs::read(dir_a.join(DEVICE_FILE)).unwrap();
    let history_before = fs::read(dir_a.join(HISTORY_FILE)).unwrap();
    let profile_before = profile_text(&e.state);
    let second = Registry::load(&e.state);
    assert_eq!(second.devices(), first.devices());
    assert_eq!(fs::read(dir_a.join(DEVICE_FILE)).unwrap(), folder_before);
    assert_eq!(fs::read(dir_a.join(HISTORY_FILE)).unwrap(), history_before);
    assert_eq!(profile_text(&e.state), profile_before);
    assert_eq!(
        fs::read_to_string(e.state.path().join(OLD_BACKUP)).unwrap(),
        old_text,
        "the kept copy is never replaced"
    );

    // B's folder turns up: it is migrated then, with its history.
    let mut second = second;
    let dir_b = claim(&b);
    assert!(second.refresh_offline(&e.state));
    assert_eq!(second.get(&b.id).unwrap(), &b);
    assert_eq!(
        folder_record::read_device(&dir_b, &b.id).unwrap().unwrap(),
        b
    );
    assert_eq!(history::load_at(&dir_b.join(HISTORY_FILE)).len(), 1);
    assert!(h.load(&b.id).is_empty());
    let third = Registry::load(&e.state);
    assert_eq!(third.get(&b.id).unwrap(), &b);
}

/// A crash half way through a move (the folder has the record, the profile still has the whole
/// device, and the settings folder still has the history) is finished by the next start, with
/// nothing doubled.
#[test]
fn a_move_that_was_cut_short_is_finished_without_doubling_anything() {
    let e = env();
    let a = stick(&e, "Phone", "AAAA-1111");
    let dir = claim(&a);
    fs::write(
        e.state.path().join("devices.json"),
        serde_json::to_string(&serde_json::json!({"version": 1, "devices": [a]})).unwrap(),
    )
    .unwrap();
    let h = History::new(&e.state);
    for day in [1, 2, 3] {
        h.append(&a.id, &record(day)).unwrap();
    }
    // Cut short: the record and the first two history lines had already gone across.
    folder_record::write_device(&dir, &a).unwrap();
    for day in [1, 2] {
        history::append_at(&dir.join(HISTORY_FILE), &record(day)).unwrap();
    }
    let loaded = Registry::load(&e.state);
    assert_eq!(loaded.devices(), std::slice::from_ref(&a));
    let hist = history::load_at(&dir.join(HISTORY_FILE));
    assert_eq!(hist.iter().map(|r| r.copied).collect::<Vec<_>>(), [3, 2, 1]);
    assert!(!h.profile_file(&a.id).exists());
}

/// The device record, the history and the skip list are the app's own: the scan never counts
/// them as backup files, never lists them as deleted from the device, and no archive name can
/// be one of them.
#[test]
fn the_apps_own_files_are_never_indexed_or_archived() {
    let e = env();
    let d = stick(&e, "Work stick", "AAAA-1111");
    let dir = claim(&d);
    let mut reg = Registry::default();
    reg.add(d.clone());
    reg.save(&e.state).unwrap();
    history::append_at(&reg.history_file(&d.id).unwrap(), &record(1)).unwrap();
    e.state
        .skip_add(
            &Scope::new(&d.id, &dir),
            &crate::paths::SafeRelPath::parse("a.txt").unwrap(),
        )
        .unwrap();
    let skip_name = crate::state::SKIP_NAME;
    for own in [DEVICE_FILE, HISTORY_FILE, skip_name, owner::MARKER] {
        assert!(dir.join(own).is_file(), "{own}");
    }
    // Leftovers of a write in progress are the app's too.
    fs::write(dir.join(format!("{DEVICE_FILE}.123-0.tmp")), b"x").unwrap();
    fs::write(dir.join(format!("{HISTORY_FILE}.damaged")), b"x").unwrap();
    fs::write(dir.join("photo.jpg"), b"p").unwrap();

    let index = crate::plan::index_backup(
        &fs::canonicalize(&dir).unwrap(),
        "_Old stuff",
        &[],
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .unwrap();
    let paths: Vec<String> = index.files().map(|f| f.path.clone()).collect();
    assert_eq!(paths, ["photo.jpg"]);

    let mut src = phone(vec![MemObject::new("1", "photo.jpg", b"p")]);
    let mut opts = open_opts();
    opts.settings.archive_name = "_Old stuff".into();
    let e2 = Env { ..env_at(&dir, &e) };
    let (plan, summary) = do_scan_with(&mut src, &e2, &opts);
    assert!(plan.removed.is_empty(), "{:?}", plan.removed);
    assert!(plan.outside.is_empty(), "{:?}", plan.outside);
    assert_eq!((summary.verified, summary.removed), (1, 0));
    for own in [DEVICE_FILE, HISTORY_FILE, skip_name] {
        assert!(owner::validate_archive_name(own).is_err(), "{own}");
    }
}

/// An `Env` that scans the backup folder `root` and uses `e`'s settings folder.
fn env_at(root: &Path, e: &Env) -> Env {
    Env {
        _tmp: tempfile::tempdir().unwrap(),
        base: e.base.clone(),
        root: root.to_path_buf(),
        state: e.state.clone(),
    }
}

/// "Change…": the record goes into the new folder and the history is copied across, and the old
/// folder keeps its files.
#[test]
fn changing_the_backup_folder_writes_the_record_and_copies_the_history() {
    let e = env();
    let d = stick(&e, "Work stick", "AAAA-1111");
    let old = claim(&d);
    let mut reg = Registry::default();
    reg.add(d.clone());
    reg.save(&e.state).unwrap();
    for day in [1, 2] {
        history::append_at(&reg.history_file(&d.id).unwrap(), &record(day)).unwrap();
    }
    let new = e.base.join("elsewhere");
    fs::create_dir_all(&new).unwrap();
    reg.set_backup_dir(&e.state, &d.id, new.clone()).unwrap();
    reg.save(&e.state).unwrap();

    let moved = folder_record::read_device(&new, &d.id).unwrap().unwrap();
    assert_eq!(moved.backup_dir.as_deref(), Some(new.as_path()));
    assert_eq!(moved.settings, d.settings);
    assert_eq!(history::load_at(&new.join(HISTORY_FILE)).len(), 2);
    // The old folder is left exactly as it was.
    assert!(old.join(DEVICE_FILE).is_file());
    assert_eq!(history::load_at(&old.join(HISTORY_FILE)).len(), 2);
    assert!(old.join(owner::MARKER).is_file());
    // A new sync's history goes into the new folder only.
    history::append_at(&reg.history_file(&d.id).unwrap(), &record(3)).unwrap();
    assert_eq!(history::load_at(&new.join(HISTORY_FILE)).len(), 3);
    assert_eq!(history::load_at(&old.join(HISTORY_FILE)).len(), 2);
    // A device that had no folder yet and kept its history in the settings folder.
    let e = env();
    let mut p = stick(&e, "Pending", "PPPP-0000");
    p.backup_dir = None;
    let mut reg = Registry::default();
    reg.add(p.clone());
    reg.save(&e.state).unwrap();
    History::new(&e.state).append(&p.id, &record(1)).unwrap();
    let dir = e.base.join("first-folder");
    fs::create_dir_all(&dir).unwrap();
    reg.set_backup_dir(&e.state, &p.id, dir.clone()).unwrap();
    assert_eq!(history::load_at(&dir.join(HISTORY_FILE)).len(), 1);
    assert!(History::new(&e.state).load(&p.id).is_empty());
    assert!(dir.join(DEVICE_FILE).is_file());
}

/// A folder that is added while the device is known with another folder is a move of its folder,
/// and the folder's own record counts.
#[test]
fn adding_a_backup_of_a_known_device_from_another_folder_is_a_move() {
    let e = env();
    let d = stick(&e, "Work stick", "AAAA-1111");
    let old = claim(&d);
    let mut reg = Registry::default();
    reg.add(d.clone());
    reg.save(&e.state).unwrap();
    // The folder copied elsewhere, where its settings were changed on another PC.
    let copy = e.base.join("copy");
    fs::create_dir_all(&copy).unwrap();
    for f in [owner::MARKER, DEVICE_FILE] {
        fs::copy(old.join(f), copy.join(f)).unwrap();
    }
    let mut changed = folder_record::read_device(&copy, &d.id).unwrap().unwrap();
    changed.settings.scan_automatically = false;
    folder_record::write_device(&copy, &changed).unwrap();

    let report = reg.find_my_backups(&copy, NOW);
    assert!(report.added.is_empty());
    assert_eq!(report.moved.len(), 1);
    assert_eq!(report.moved[0].device_id, d.id);
    assert!(reg.rehome(&d.id, &copy).is_none());
    let now = reg.get(&d.id).unwrap();
    assert_eq!(now.backup_dir.as_deref(), Some(copy.as_path()));
    assert!(
        !now.settings.scan_automatically,
        "the folder's record counts"
    );
    // Nothing known and nothing found: not a backup.
    let empty = e.base.join("empty");
    fs::create_dir_all(&empty).unwrap();
    assert_eq!(reg.find_my_backups(&empty, NOW), Default::default());
}

/// A record that can't be read is kept beside it, and the device is rebuilt from the marker and
/// what the profile remembers.
#[test]
fn a_damaged_record_is_kept_aside_and_the_device_rebuilt() {
    let e = env();
    let d = stick(&e, "Work stick", "AAAA-1111");
    let dir = claim(&d);
    let mut reg = Registry::default();
    reg.add(d.clone());
    reg.save(&e.state).unwrap();
    fs::write(dir.join(DEVICE_FILE), b"{ not json").unwrap();
    let loaded = Registry::load(&e.state);
    let got = loaded.get(&d.id).unwrap();
    assert_eq!(got.name, "Work stick");
    assert!(got.offline.is_none());
    assert!(got.history.contains(SignalKind::VolumeSerial, "aaaa-1111"));
    // It was saved again, and the unreadable one is kept.
    assert!(folder_record::read_device(&dir, &d.id).unwrap().is_some());
    assert_eq!(
        fs::read(dir.join(format!("{DEVICE_FILE}.damaged"))).unwrap(),
        b"{ not json"
    );
}

/// The skip list travels with the backup folder, and one the settings folder kept moves in.
#[test]
fn the_skip_list_lives_in_the_backup_folder() {
    let e = env();
    let d = stick(&e, "Work stick", "AAAA-1111");
    let dir = claim(&d);
    let file = |p: &str| crate::paths::SafeRelPath::parse(p).unwrap();
    // From an earlier version: in the settings folder, for this device and folder.
    let scope = Scope::new(&d.id, &dir);
    let old = serde_json::json!({
        "version": 2,
        "scopes": [{"device": d.id, "root": dir.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase(), "paths": ["old.txt"]}],
    });
    fs::write(
        e.state.path().join("skip_lists.json"),
        serde_json::to_vec(&old).unwrap(),
    )
    .unwrap();
    assert_eq!(e.state.skip_list(&scope).unwrap().len(), 1, "{scope}");
    e.state.skip_add(&scope, &file("new.txt")).unwrap();
    assert!(dir.join(crate::state::SKIP_NAME).is_file());
    let profile: serde_json::Value =
        serde_json::from_slice(&fs::read(e.state.path().join("skip_lists.json")).unwrap()).unwrap();
    assert!(profile["scopes"].as_array().unwrap().is_empty());
    // Another computer, another path to the same backup.
    let other = StateDir::open(e.base.join("other-state")).unwrap();
    let moved = e.base.join("moved-backup");
    fs::create_dir_all(&moved).unwrap();
    for f in [owner::MARKER, crate::state::SKIP_NAME] {
        fs::copy(dir.join(f), moved.join(f)).unwrap();
    }
    let list = other.skip_list(&Scope::new(&d.id, &moved)).unwrap();
    assert_eq!(list, set(&["old.txt", "new.txt"]).into_iter().collect());
}

/// "Change…" works for a device whose folder is missing: the record (with what the app learned
/// meanwhile) goes into the new folder, copying history from the unreachable one fails softly
/// (nothing to copy, no error), and the device is usable again.
#[test]
fn changing_the_folder_of_a_missing_one_writes_the_record_into_the_new_folder() {
    let e = env();
    let d = stick(&e, "Work stick", "AAAA-1111");
    let dir = claim(&d);
    let mut reg = Registry::default();
    reg.add(d.clone());
    reg.save(&e.state).unwrap();
    fs::rename(&dir, e.base.join("unplugged")).unwrap();

    let mut loaded = Registry::load(&e.state);
    assert!(loaded.get(&d.id).unwrap().offline.is_some());
    // A signal learned while the folder is away is only in memory.
    loaded.get_mut(&d.id).unwrap().learn(&sig("CCCC-3333"), NOW);
    let new = e.base.join("fresh");
    fs::create_dir_all(&new).unwrap();
    loaded.set_backup_dir(&e.state, &d.id, new.clone()).unwrap();
    loaded.save(&e.state).unwrap();

    let got = loaded.get(&d.id).unwrap();
    assert!(got.offline.is_none());
    assert_eq!(got.backup_dir.as_deref(), Some(new.as_path()));
    let written = folder_record::read_device(&new, &d.id).unwrap().unwrap();
    // The old folder's settings were out of reach: the cache only had the defaults.
    assert_eq!(written.name, "Work stick");
    assert!(
        written
            .history
            .contains(SignalKind::VolumeSerial, "cccc-3333")
    );
    assert!(
        written
            .history
            .contains(SignalKind::VolumeSerial, "aaaa-1111")
    );
    assert!(loaded.history_file(&d.id).is_some());
}

/// Signals learned while the folder was away are kept when it is found again, whether it came
/// back by itself or was pointed to.
#[test]
fn signals_learned_while_the_folder_was_away_survive_its_return() {
    let e = env();
    let d = stick(&e, "Work stick", "AAAA-1111");
    let dir = claim(&d);
    let mut reg = Registry::default();
    reg.add(d.clone());
    reg.save(&e.state).unwrap();
    let away = e.base.join("unplugged");
    fs::rename(&dir, &away).unwrap();

    let mut loaded = Registry::load(&e.state);
    loaded.get_mut(&d.id).unwrap().learn(&sig("CCCC-3333"), NOW);
    // Plugged in again by itself.
    fs::rename(&away, &dir).unwrap();
    assert!(loaded.refresh_offline(&e.state));
    let got = loaded.get(&d.id).unwrap();
    assert!(got.history.contains(SignalKind::VolumeSerial, "cccc-3333"));
    assert!(got.history.contains(SignalKind::VolumeSerial, "aaaa-1111"));

    // Pointed to its new place ("Find it…").
    let mut loaded = Registry::load(&e.state);
    loaded.get_mut(&d.id).unwrap().learn(&sig("DDDD-4444"), NOW);
    let moved = e.base.join("moved");
    fs::rename(&dir, &moved).unwrap();
    assert!(loaded.rehome(&d.id, &moved).is_none());
    let got = loaded.get(&d.id).unwrap();
    assert!(got.history.contains(SignalKind::VolumeSerial, "dddd-4444"));
    assert!(got.history.contains(SignalKind::VolumeSerial, "aaaa-1111"));
}

/// Forget only drops the device from the list: its backup folder, with the record and history in
/// it, is left exactly as it was, and "Add an existing backup…" brings it back.
#[test]
fn forgetting_a_device_leaves_its_backup_folder_alone() {
    let e = env();
    let d = stick(&e, "Work stick", "AAAA-1111");
    let dir = claim(&d);
    fs::write(dir.join("photo.jpg"), b"x").unwrap();
    let mut reg = Registry::default();
    reg.add(d.clone());
    reg.save(&e.state).unwrap();
    history::append_at(&reg.history_file(&d.id).unwrap(), &record(1)).unwrap();
    let marker = fs::read(dir.join(owner::MARKER)).unwrap();

    assert!(reg.forget(&d.id).is_some());
    reg.save(&e.state).unwrap();
    assert!(reg.get(&d.id).is_none());
    assert_eq!(fs::read(dir.join(owner::MARKER)).unwrap(), marker);
    assert!(dir.join("photo.jpg").is_file());
    assert!(dir.join(DEVICE_FILE).is_file());
    assert_eq!(history::load_at(&dir.join(HISTORY_FILE)).len(), 1);

    let report = reg.find_my_backups(&dir, NOW);
    assert_eq!(report.added, ["Work stick"]);
    assert!(reg.get(&d.id).is_some());
}
