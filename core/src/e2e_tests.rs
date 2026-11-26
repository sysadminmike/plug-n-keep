//! Scan-and-sync tests with fake devices, against real temporary folders.
//! Each test's doc comment says which safety rule it guards.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use walkdir::WalkDir;

use crate::device::{DeviceSource, Guarded, ScanIssue};
use crate::fake::{FolderSource, Lagging, MemObject, MemSource};
use crate::owner::{self, OwnerState};
use crate::paths::TEMP_SUFFIX;
use crate::plan::{ARCHIVE_DIR, Caution, ScanOptions, ScanPlan, Status, Summary, scan};
use crate::registry::{ArchiveLayout, DeviceSettings, FileTypes, StorageLayout};
use crate::signals::{Signals, View};
use crate::state::{Scope, StateDir};
use crate::sync::{
    ArchiveEvent, ArchiveReport, CopyMode, SyncControl, SyncError, SyncEvent, SyncItem, SyncReport,
    archive_removed, rename_archive, run_sync,
};
use crate::testutil::make_junction;

struct Env {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    root: PathBuf,
    state: StateDir,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let root = base.join("backup");
    let state = StateDir::open(base.join("state")).unwrap();
    Env {
        _tmp: tmp,
        base,
        root,
        state,
    }
}

/// The settings most tests use: the whole device, every file type, and the archive's flat
/// layout. Tests of the selection and the layout say so themselves.
fn open_settings() -> DeviceSettings {
    DeviceSettings {
        folders: Vec::new(),
        file_types: FileTypes::All,
        always_left_out: Vec::new(),
        archive_layout: ArchiveLayout::Flat,
        ..DeviceSettings::default()
    }
}

fn open_opts() -> ScanOptions {
    ScanOptions {
        settings: open_settings(),
        weak_match: false,
    }
}

fn do_scan(src: &mut dyn DeviceSource, e: &Env) -> (ScanPlan, Summary) {
    do_scan_with(src, e, &open_opts())
}

fn do_scan_with(src: &mut dyn DeviceSource, e: &Env, opts: &ScanOptions) -> (ScanPlan, Summary) {
    let skip = e
        .state
        .skip_list(&Scope::new(&src.identity().key, &e.root))
        .unwrap();
    scan(
        src,
        &e.root,
        &skip,
        opts,
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .unwrap()
}

/// The dated archive folder every test sync uses.
const STAMP: &str = "2026-01-02 03-04-05";

fn pending(plan: &ScanPlan) -> Vec<SyncItem> {
    plan.entries
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e.status, Status::New | Status::Mismatch))
        .map(|(i, _)| SyncItem {
            entry: i,
            mode: CopyMode::NoReplace,
        })
        .collect()
}

fn do_sync(
    src: &mut dyn DeviceSource,
    plan: &ScanPlan,
    e: &Env,
    items: &[SyncItem],
    ctl: &SyncControl,
) -> (Result<SyncReport, SyncError>, Vec<SyncEvent>) {
    do_sync_as(src, plan, e, items, ctl, false)
}

/// A sync where the user has (or hasn't) said OK to the reasons archiving asks first.
fn do_sync_as(
    src: &mut dyn DeviceSource,
    plan: &ScanPlan,
    e: &Env,
    items: &[SyncItem],
    ctl: &SyncControl,
    confirmed: bool,
) -> (Result<SyncReport, SyncError>, Vec<SyncEvent>) {
    let mut events = Vec::new();
    let r = run_sync(
        src,
        plan,
        items,
        &e.state,
        ctl,
        STAMP,
        confirmed,
        &mut |ev| events.push(ev),
    );
    (r, events)
}

fn failures(events: &[SyncEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            SyncEvent::Failed { error, .. } => Some(error.clone()),
            _ => None,
        })
        .collect()
}

/// Every file under `dir`, as `/`-separated relative paths, except the folder-owner marker that
/// sync writes at the top (see `owner`) and the skip list kept beside it. [`files_under_all`]
/// includes them.
fn files_under(dir: &Path) -> BTreeSet<String> {
    let mut files = files_under_all(dir);
    files.remove(owner::MARKER);
    files.remove(crate::state::SKIP_NAME);
    files
}

fn files_under_all(dir: &Path) -> BTreeSet<String> {
    WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| {
            e.path()
                .strip_prefix(dir)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect()
}

fn set(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| s.to_string()).collect()
}

fn phone(objects: Vec<MemObject>) -> MemSource {
    MemSource::new("PHONE-A", objects)
}

/// The plan entry index of the device file at `path`.
fn entry_of(plan: &ScanPlan, path: &str) -> usize {
    plan.entries
        .iter()
        .position(|e| e.source.as_str() == path)
        .unwrap_or_else(|| panic!("{path} is not in the plan"))
}

/// Every file under `root` holds the data of the device file it is named after. Panics naming
/// the first that doesn't.
fn assert_own_data(root: &Path, src: &MemSource) {
    for f in files_under(root) {
        let obj = src
            .objects
            .iter()
            .find(|o| o.components.join("/") == f)
            .unwrap_or_else(|| panic!("{f} is not a device file"));
        assert_eq!(
            fs::read(root.join(&f)).unwrap(),
            obj.data,
            "{f} holds another file's data"
        );
    }
}

#[test]
fn copies_new_files_and_a_rescan_finds_them_backed_up() {
    let e = env();
    let mut src = phone(vec![
        MemObject::new("1", "DCIM/100APPLE/IMG_0001.HEIC", b"one"),
        MemObject::new("2", "DCIM/100APPLE/IMG_0002.MOV", &[7u8; 300_000]),
    ]);
    let (plan, summary) = do_scan(&mut src, &e);
    assert_eq!(summary.new, 2);
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().copied, 2, "{:?}", failures(&ev));
    assert_eq!(
        fs::read(e.root.join("DCIM/100APPLE/IMG_0001.HEIC")).unwrap(),
        b"one"
    );
    assert_eq!(
        fs::read(e.root.join("DCIM/100APPLE/IMG_0002.MOV"))
            .unwrap()
            .len(),
        300_000
    );
    assert_eq!(
        files_under(&e.root),
        set(&["DCIM/100APPLE/IMG_0001.HEIC", "DCIM/100APPLE/IMG_0002.MOV"])
    );
    let (plan2, summary2) = do_scan(&mut src, &e);
    assert_eq!(summary2.verified, 2);
    assert!(pending(&plan2).is_empty());
}

/// Hostile names are reported, never written, and nothing outside the backup changes.
#[test]
fn hostile_names_are_reported_and_nothing_outside_the_backup_changes() {
    let e = env();
    let victim = e.base.join("victim");
    fs::create_dir_all(&victim).unwrap();
    fs::write(victim.join("report.docx"), b"precious").unwrap();
    fs::create_dir_all(e.root.join("DCIM")).unwrap();
    fs::write(e.root.join("DCIM/IMG_0002.HEIC"), b"ORIGINAL").unwrap();
    let abs_victim = victim.join("report.docx").to_string_lossy().into_owned();
    let hostile: Vec<Vec<String>> = vec![
        vec!["..".into(), "victim".into(), "report.docx".into()],
        vec![r"..\victim\report.docx".into()],
        vec!["DCIM".into(), r"..\..\victim\report.docx".into()],
        vec![abs_victim.clone()],
        vec![abs_victim.replace('\\', "/")],
        vec!["DCIM".into(), "IMG_0002.HEIC.".into()],
        vec!["DCIM".into(), "IMG_0002.HEIC ".into()],
        vec!["DCIM".into(), "IMG_0003.HEIC:evil".into()],
        vec!["DCIM".into(), "CON".into()],
        vec!["DCIM".into(), "NUL.jpg".into()],
        vec!["DCIM".into(), format!("IMG_0004.HEIC{TEMP_SUFFIX}")],
    ];
    let mut objects: Vec<MemObject> = hostile
        .iter()
        .enumerate()
        .map(|(i, c)| MemObject::with_components(&format!("h{i}"), c.clone(), b"EVIL"))
        .collect();
    objects.push(MemObject::new("ok", "DCIM/IMG_0005.HEIC", b"fine"));
    let mut src = phone(objects);

    let (plan, summary) = do_scan(&mut src, &e);
    assert_eq!(
        plan.entries.len(),
        1,
        "only the safe file is planned: {:?}",
        plan.entries
    );
    assert_eq!(summary.issues, hostile.len(), "{:?}", plan.issues);
    assert!(
        plan.issues
            .iter()
            .all(|i| i.problem.starts_with("not copied"))
    );

    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().copied, 1, "{:?}", failures(&ev));
    assert_eq!(fs::read(victim.join("report.docx")).unwrap(), b"precious");
    assert_eq!(
        fs::read(e.root.join("DCIM/IMG_0002.HEIC")).unwrap(),
        b"ORIGINAL"
    );
    assert_eq!(
        files_under(&e.root),
        set(&["DCIM/IMG_0002.HEIC", "DCIM/IMG_0005.HEIC"])
    );
    assert_eq!(files_under(&victim), set(&["report.docx"]));
}

/// A file that turns up at the target after the scan is never replaced.
#[test]
fn a_file_that_appears_after_the_scan_is_never_overwritten() {
    let e = env();
    let mut src = phone(vec![
        MemObject::new("1", "DCIM/IMG_0001.HEIC", b"from phone B"),
        MemObject::new("2", "DCIM/IMG_0002.HEIC", b"same"),
    ]);
    let (plan, summary) = do_scan(&mut src, &e);
    assert_eq!(summary.new, 2);
    // Meanwhile (e.g. the folder box was pointed at phone A's backup), files appear.
    fs::create_dir_all(e.root.join("DCIM")).unwrap();
    fs::write(
        e.root.join("DCIM/IMG_0001.HEIC"),
        b"phone A's photo, keep me",
    )
    .unwrap();
    fs::write(e.root.join("DCIM/IMG_0002.HEIC"), b"same").unwrap();

    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    let r = r.unwrap();
    assert_eq!(
        (r.copied, r.already_backed_up, r.failed),
        (0, 1, 1),
        "{ev:?}"
    );
    assert!(failures(&ev)[0].contains("not overwritten"), "{ev:?}");
    assert_eq!(
        fs::read(e.root.join("DCIM/IMG_0001.HEIC")).unwrap(),
        b"phone A's photo, keep me"
    );
    assert_eq!(
        files_under(&e.root),
        set(&["DCIM/IMG_0001.HEIC", "DCIM/IMG_0002.HEIC"])
    );
}

/// A changed file (DESIGN.md 7.3): the old copy moves into the archive, the new one takes its
/// place, and nothing is lost. Files named `_new` in a backup are ordinary files.
#[test]
fn a_changed_file_moves_its_old_copy_into_the_archive_and_the_new_one_takes_its_place() {
    let e = env();
    fs::create_dir_all(e.root.join("DCIM")).unwrap();
    fs::write(e.root.join("DCIM/IMG.HEIC"), b"a").unwrap();
    fs::write(e.root.join("DCIM/IMG_new.HEIC"), b"bb").unwrap();
    let mut src = phone(vec![
        MemObject::new("1", "DCIM/IMG.HEIC", b"ccc"),
        MemObject::new("2", "DCIM/IMG_new.HEIC", b"bb"),
    ]);
    let (plan, summary) = do_scan(&mut src, &e);
    assert_eq!(summary.mismatch, 1);
    let i = entry_of(&plan, "DCIM/IMG.HEIC");
    assert_eq!(plan.entries[i].status, Status::Mismatch);
    // The planned place is the file's own: no `_new` name is made.
    assert_eq!(plan.entries[i].target.as_str(), "DCIM/IMG.HEIC");
    // The `_new` file is just a file: it is on the device here, so it is backed up.
    assert_eq!(
        plan.entries[entry_of(&plan, "DCIM/IMG_new.HEIC")].status,
        Status::Verified
    );
    let (r, ev) = do_sync_as(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
        true,
    );
    let r = r.unwrap();
    assert_eq!((r.copied, r.replaced), (1, 1), "{:?}", failures(&ev));
    assert_eq!(fs::read(e.root.join("DCIM/IMG.HEIC")).unwrap(), b"ccc");
    assert_eq!(fs::read(e.root.join("DCIM/IMG_new.HEIC")).unwrap(), b"bb");
    // Flat archive layout in these tests: the old copy is directly in the dated folder.
    assert_eq!(
        fs::read(e.root.join(format!("{ARCHIVE_DIR}/{STAMP}/IMG.HEIC"))).unwrap(),
        b"a"
    );
    assert_eq!(
        files_under(&e.root),
        set(&[
            "DCIM/IMG.HEIC",
            "DCIM/IMG_new.HEIC",
            &format!("{ARCHIVE_DIR}/{STAMP}/IMG.HEIC")
        ])
    );
    // The marker lists the archive name, and the next scan sees everything backed up.
    assert!(matches!(
        owner::read(&e.root),
        OwnerState::Owned(o) if o.archive_names == [ARCHIVE_DIR]
    ));
    let (plan2, s2) = do_scan(&mut src, &e);
    assert_eq!((s2.verified, s2.mismatch, s2.removed), (2, 0, 0));
    assert!(plan2.issues.is_empty(), "{:?}", plan2.issues);
    assert!(!e.state.path().join("in_progress.json").exists());
}

/// A plan can't be used with a different device.
#[test]
fn sync_refuses_a_different_device() {
    let e = env();
    let mut a = phone(vec![MemObject::new("1", "DCIM/IMG_0001.HEIC", b"a")]);
    let (plan, _) = do_scan(&mut a, &e);
    let mut b = MemSource::new(
        "PHONE-B",
        vec![MemObject::new("1", "DCIM/IMG_0001.HEIC", b"b")],
    );
    let (r, _) = do_sync(&mut b, &plan, &e, &pending(&plan), &SyncControl::default());
    assert!(matches!(r, Err(SyncError::DeviceChanged { .. })), "{r:?}");
    assert!(files_under(&e.root).is_empty());
}

/// If the backup folder now resolves somewhere else, sync refuses.
#[test]
fn sync_refuses_when_the_backup_folder_now_points_elsewhere() {
    let e = env();
    fs::create_dir_all(&e.root).unwrap();
    let mut src = phone(vec![MemObject::new("1", "DCIM/IMG_0001.HEIC", b"a")]);
    let (plan, _) = do_scan(&mut src, &e);
    fs::rename(&e.root, e.base.join("backup-moved")).unwrap();
    let elsewhere = e.base.join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    make_junction(&e.root, &elsewhere);
    let (r, _) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert!(matches!(r, Err(SyncError::RootChanged { .. })), "{r:?}");
    assert!(files_under(&elsewhere).is_empty());
}

/// Writes never go through a junction inside the backup folder.
#[test]
fn writes_never_pass_through_a_junction_inside_the_backup() {
    let e = env();
    fs::create_dir_all(&e.root).unwrap();
    let outside = e.base.join("outside");
    fs::create_dir_all(&outside).unwrap();
    make_junction(&e.root.join("DCIM"), &outside);
    let mut src = phone(vec![MemObject::new("1", "DCIM/IMG_0001.HEIC", b"a")]);
    let (plan, _) = do_scan(&mut src, &e);
    assert!(
        plan.issues
            .iter()
            .any(|i| i.problem.contains("link or junction")),
        "{:?}",
        plan.issues
    );
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().failed, 1);
    assert!(failures(&ev)[0].contains("link or junction"), "{ev:?}");
    assert!(files_under(&outside).is_empty());
}

/// Copying a file again is only done when asked, and never overwrites: the old copy moves into
/// the archive and the new one takes its place, the same as for a changed file.
#[test]
fn copying_a_file_again_archives_the_old_copy_and_never_overwrites() {
    let e = env();
    fs::create_dir_all(e.root.join("DCIM")).unwrap();
    fs::write(e.root.join("DCIM/IMG.HEIC"), b"old!").unwrap();
    let mut src = phone(vec![MemObject::new("1", "DCIM/IMG.HEIC", b"new!")]);
    let (plan, _) = do_scan(&mut src, &e);
    assert_eq!(plan.entries[0].status, Status::Verified);
    // Without Replace, a verified file is left alone.
    let (r, _) = do_sync(
        &mut src,
        &plan,
        &e,
        &[SyncItem {
            entry: 0,
            mode: CopyMode::NoReplace,
        }],
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().already_backed_up, 1);
    assert_eq!(fs::read(e.root.join("DCIM/IMG.HEIC")).unwrap(), b"old!");
    // With Replace, the old copy goes into the archive and the new one takes its place.
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &[SyncItem {
            entry: 0,
            mode: CopyMode::Replace,
        }],
        &SyncControl::default(),
    );
    let r = r.unwrap();
    assert_eq!((r.copied, r.replaced), (1, 1), "{:?}", failures(&ev));
    assert_eq!(fs::read(e.root.join("DCIM/IMG.HEIC")).unwrap(), b"new!");
    assert_eq!(
        fs::read(e.root.join(format!("{ARCHIVE_DIR}/{STAMP}/IMG.HEIC"))).unwrap(),
        b"old!",
        "the old copy must be in the archive"
    );
    assert_eq!(
        files_under(&e.root),
        set(&["DCIM/IMG.HEIC", &format!("{ARCHIVE_DIR}/{STAMP}/IMG.HEIC")])
    );
}

/// If the old copy can't be moved into the archive, it stays where it is, nothing is
/// overwritten, and the new copy is not put in its place. (Windows: a file held open without
/// sharing for delete can't be moved.)
#[cfg(windows)]
#[test]
fn a_failed_archive_move_keeps_the_old_backup_when_copying_a_file_again() {
    use std::os::windows::fs::OpenOptionsExt;
    let e = env();
    fs::create_dir_all(e.root.join("DCIM")).unwrap();
    let target = e.root.join("DCIM/IMG.HEIC");
    fs::write(&target, b"old!").unwrap();
    let mut src = phone(vec![MemObject::new("1", "DCIM/IMG.HEIC", b"new!")]);
    let (plan, _) = do_scan(&mut src, &e);
    // Share read and write, but not delete: a move of this file is refused.
    let _held = fs::OpenOptions::new()
        .read(true)
        .share_mode(0x1 | 0x2)
        .open(&target)
        .unwrap();
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &[SyncItem {
            entry: 0,
            mode: CopyMode::Replace,
        }],
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().failed, 1);
    assert!(
        failures(&ev)[0].contains("could not move the old copy into the archive"),
        "{ev:?}"
    );
    assert_eq!(fs::read(&target).unwrap(), b"old!");
    assert!(!e.root.join(format!("DCIM/IMG.HEIC{TEMP_SUFFIX}")).exists());
}

/// A source that, once a copy is under way, makes the backup folder unwritable. The temporary
/// copy already exists by then, so only the final move is refused.
#[cfg(unix)]
struct LocksFolderOnRead {
    inner: MemSource,
    folder: PathBuf,
}

#[cfg(unix)]
impl DeviceSource for LocksFolderOnRead {
    fn identity(&self) -> &crate::device::DeviceIdentity {
        self.inner.identity()
    }
    fn enumerate(
        &mut self,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(usize, &str),
    ) -> Result<crate::device::Enumeration, crate::device::DeviceError> {
        self.inner.enumerate(cancel, progress)
    }
    fn object_info(
        &mut self,
        id: &str,
    ) -> Result<crate::device::ObjectInfo, crate::device::DeviceError> {
        self.inner.object_info(id)
    }
    fn open_read(
        &mut self,
        id: &str,
    ) -> Result<Box<dyn std::io::Read + '_>, crate::device::DeviceError> {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&self.folder, fs::Permissions::from_mode(0o555)).unwrap();
        self.inner.open_read(id)
    }
}

/// The same rule on Linux and FreeBSD, where a folder that can't be written refuses the move of
/// the old copy out of it.
#[cfg(unix)]
#[test]
fn a_failed_archive_move_keeps_the_old_backup_when_copying_a_file_again_on_unix() {
    let e = env();
    let folder = e.root.join("DCIM");
    fs::create_dir_all(&folder).unwrap();
    let target = folder.join("IMG.HEIC");
    fs::write(&target, b"old!").unwrap();
    let mut src = LocksFolderOnRead {
        inner: phone(vec![MemObject::new("1", "DCIM/IMG.HEIC", b"new!")]),
        folder: folder.clone(),
    };
    // The source makes the folder read-only during the copy; the guard undoes that however the
    // test ends.
    let guard = crate::testutil::ReadOnlyDir::adopt(&folder);
    let (plan, _) = do_scan(&mut src, &e);
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &[SyncItem {
            entry: 0,
            mode: CopyMode::Replace,
        }],
        &SyncControl::default(),
    );
    // Running as root, nothing is refused: there is nothing to check.
    if !guard.is_enforced() {
        eprintln!("SKIPPED: running as root, which ignores folder permissions");
        return;
    }
    assert_eq!(r.unwrap().failed, 1);
    assert!(
        failures(&ev)[0].contains("could not move the old copy into the archive"),
        "{ev:?}"
    );
    assert_eq!(fs::read(&target).unwrap(), b"old!");
}

/// A source whose connection is lost from the second file on.
struct LostFromSecond {
    inner: MemSource,
    infos: usize,
}

impl DeviceSource for LostFromSecond {
    fn identity(&self) -> &crate::device::DeviceIdentity {
        self.inner.identity()
    }
    fn enumerate(
        &mut self,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(usize, &str),
    ) -> Result<crate::device::Enumeration, crate::device::DeviceError> {
        self.inner.enumerate(cancel, progress)
    }
    fn object_info(
        &mut self,
        id: &str,
    ) -> Result<crate::device::ObjectInfo, crate::device::DeviceError> {
        self.infos += 1;
        if self.infos > 1 {
            return Err(crate::device::DeviceError::Failed(
                "The device does not exist. (0x800701B1)".into(),
            ));
        }
        self.inner.object_info(id)
    }
    fn open_read(
        &mut self,
        id: &str,
    ) -> Result<Box<dyn std::io::Read + '_>, crate::device::DeviceError> {
        self.inner.open_read(id)
    }
}

/// A lost connection stops the sync at the file it happened on, and the report says so: the
/// rest would only fail the same way.
#[test]
fn a_lost_connection_stops_the_sync_early() {
    let e = env();
    let mut src = LostFromSecond {
        inner: phone(vec![
            MemObject::new("1", "DCIM/A.HEIC", b"one"),
            MemObject::new("2", "DCIM/B.HEIC", b"two"),
            MemObject::new("3", "DCIM/C.HEIC", b"three"),
        ]),
        infos: 0,
    };
    let (plan, _) = do_scan(&mut src, &e);
    src.infos = 0;
    let (r, _) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    let r = r.unwrap();
    assert_eq!(
        (r.copied, r.failed),
        (1, 1),
        "stopped after the first failure"
    );
    assert!(r.connection_lost.is_some());
}

/// A changed file is replaced under the name it has in the backup folder, not under the case the
/// device spells it in: on Linux the two are different names.
#[test]
fn a_changed_file_is_replaced_under_its_name_in_the_backup() {
    let e = env();
    fs::create_dir_all(e.root.join("dcim")).unwrap();
    fs::write(e.root.join("dcim/a.heic"), b"old").unwrap();
    let mut src = phone(vec![MemObject::new("1", "DCIM/A.HEIC", b"newer")]);
    claim(&e, &key_of(&src));
    let (plan, _) = do_scan(&mut src, &e);
    assert_eq!(plan.entries.len(), 1);
    assert_eq!(plan.entries[0].status, Status::Mismatch);
    assert_eq!(plan.entries[0].target.as_str(), "dcim/a.heic");
    let (r, ev) = do_sync_as(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
        true,
    );
    let (r, ev) = (r.unwrap(), ev);
    assert_eq!(r.copied, 1, "{r:?} {ev:?}");
    assert_eq!(fs::read(e.root.join("dcim/a.heic")).unwrap(), b"newer");
    let names: Vec<_> = fs::read_dir(&e.root)
        .unwrap()
        .map(|d| d.unwrap().file_name().to_string_lossy().to_lowercase())
        .filter(|n| n == "dcim")
        .collect();
    assert_eq!(names.len(), 1, "no second folder in another case");
}

/// A new file under a device folder that the backup has in other capitals goes into the
/// backup's folder, not a second one (on Linux the two are different names).
#[test]
fn a_new_file_goes_into_the_backups_spelling_of_its_folder() {
    let e = env();
    fs::create_dir_all(e.root.join("DCIM")).unwrap();
    fs::write(e.root.join("DCIM/a.heic"), b"one").unwrap();
    let mut src = phone(vec![
        MemObject::new("1", "dcim/a.heic", b"one"),
        MemObject::new("2", "dcim/New.jpg", b"fresh"),
    ]);
    claim(&e, &key_of(&src));
    let (plan, _) = do_scan(&mut src, &e);
    let new = plan
        .entries
        .iter()
        .find(|p| p.status == Status::New)
        .unwrap();
    assert_eq!(new.target.as_str(), "DCIM/New.jpg");
    let (r, ev) = do_sync_as(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
        true,
    );
    assert_eq!(r.unwrap().copied, 1, "{ev:?}");
    assert_eq!(fs::read(e.root.join("DCIM/New.jpg")).unwrap(), b"fresh");
    let names: Vec<_> = fs::read_dir(&e.root)
        .unwrap()
        .map(|d| d.unwrap().file_name().to_string_lossy().to_lowercase())
        .filter(|n| n == "dcim")
        .collect();
    assert_eq!(names.len(), 1, "no second folder in another case");
}

/// Two folders on the device that differ only in case are not merged by the rule above.
#[test]
fn device_folders_that_differ_only_in_case_are_not_merged() {
    let e = env();
    fs::create_dir_all(e.root.join("DCIM")).unwrap();
    fs::write(e.root.join("DCIM/a.heic"), b"one").unwrap();
    let mut src = phone(vec![
        MemObject::new("1", "dcim/a.heic", b"one"),
        MemObject::new("2", "DCIM/b.jpg", b"b"),
        MemObject::new("3", "dcim/c.jpg", b"c"),
    ]);
    claim(&e, &key_of(&src));
    let (plan, _) = do_scan(&mut src, &e);
    let target_of = |source: &str| {
        plan.entries
            .iter()
            .find(|p| p.source.as_str() == source)
            .unwrap()
            .target
            .as_str()
            .to_string()
    };
    assert_eq!(target_of("DCIM/b.jpg"), "DCIM/b.jpg");
    assert_eq!(target_of("dcim/c.jpg"), "dcim/c.jpg");
}

/// A user skip deletes the partial copy and applies to this phone and folder only.
/// Skipping part-way through a file abandons the transfer, so the sync stops there and asks
/// for a new connection.
#[test]
fn a_user_skip_cleans_up_and_is_scoped_to_this_phone_and_folder() {
    let e = env();
    let ctl = SyncControl::default();
    let mut obj = MemObject::new("1", "DCIM/IMG_0001.MOV", &[1u8; 100_000]);
    obj.trip = Some((10_000, ctl.skip_current.clone()));
    let mut src = phone(vec![obj, MemObject::new("2", "DCIM/IMG_0002.HEIC", b"two")]);
    let (plan, _) = do_scan(&mut src, &e);
    // The window names the file it shows as being copied.
    ctl.skip_entry
        .store(entry_of(&plan, "DCIM/IMG_0001.MOV"), Ordering::SeqCst);
    let (r, ev) = do_sync(&mut src, &plan, &e, &pending(&plan), &ctl);
    let r = r.unwrap();
    assert_eq!((r.skipped, r.copied), (1, 0), "{ev:?}");
    assert!(r.must_reconnect && !r.cancelled, "{r:?}");
    assert!(files_under(&e.root).is_empty(), "partial copy left behind");
    let here = e
        .state
        .skip_list(&Scope::new("serial:PHONE-A", &e.root))
        .unwrap();
    assert!(here.contains("DCIM/IMG_0001.MOV"));
    let other_phone = e
        .state
        .skip_list(&Scope::new("serial:PHONE-B", &e.root))
        .unwrap();
    assert!(other_phone.is_empty());
    let (plan2, _) = do_scan(&mut src, &e);
    assert_eq!(plan2.entries[0].status, Status::Skipped);
}

/// A read error mid-file leaves no partial file and doesn't skip the file.
#[test]
fn a_read_error_leaves_nothing_behind() {
    let e = env();
    let mut obj = MemObject::new("1", "DCIM/IMG_0001.MOV", &[1u8; 100_000]);
    obj.fail_after = Some(10_000);
    let mut src = phone(vec![obj]);
    let (plan, _) = do_scan(&mut src, &e);
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    let r = r.unwrap();
    assert_eq!(r.failed, 1);
    assert!(r.must_reconnect);
    assert!(
        failures(&ev)[0].contains("simulated device read failure"),
        "{ev:?}"
    );
    assert!(files_under(&e.root).is_empty());
    assert!(
        e.state
            .skip_list(&Scope::new("serial:PHONE-A", &e.root))
            .unwrap()
            .is_empty()
    );
}

/// If the object ID now names a different file, nothing is copied.
#[test]
fn a_file_changed_on_the_device_since_the_scan_is_not_copied() {
    let e = env();
    let mut src = phone(vec![
        MemObject::new("1", "DCIM/IMG_0001.HEIC", b"one"),
        MemObject::new("2", "DCIM/IMG_0002.HEIC", b"two"),
    ]);
    let (plan, _) = do_scan(&mut src, &e);
    src.object_mut("1").components = vec!["DCIM".into(), "IMG_0999.HEIC".into()];
    src.object_mut("2").reported_size = Some(12345);
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().failed, 2);
    assert!(
        failures(&ev)
            .iter()
            .all(|f| f.contains("changed since the scan")),
        "{ev:?}"
    );
    assert!(files_under(&e.root).is_empty());
}

#[test]
fn cancel_stops_the_sync_and_cleans_up() {
    let e = env();
    let ctl = SyncControl::default();
    let mut first = MemObject::new("1", "DCIM/A.MOV", &[1u8; 100_000]);
    first.trip = Some((10_000, ctl.cancel.clone()));
    let mut src = phone(vec![first, MemObject::new("2", "DCIM/B.HEIC", b"b")]);
    let (plan, _) = do_scan(&mut src, &e);
    let (r, ev) = do_sync(&mut src, &plan, &e, &pending(&plan), &ctl);
    let r = r.unwrap();
    assert!(r.cancelled);
    assert!(r.must_reconnect, "stopped part-way through a file");
    assert_eq!(r.copied, 0, "{ev:?}");
    assert!(files_under(&e.root).is_empty());
}

/// Stopping between files abandons no transfer: the connection and the scan stay usable.
#[test]
fn a_stop_between_files_needs_no_new_connection() {
    let e = env();
    let ctl = SyncControl::default();
    let mut src = phone(vec![MemObject::new("1", "DCIM/A.HEIC", b"a")]);
    let (plan, _) = do_scan(&mut src, &e);
    ctl.cancel.store(true, Ordering::Relaxed);
    let (r, _) = do_sync(&mut src, &plan, &e, &pending(&plan), &ctl);
    let r = r.unwrap();
    assert!(r.cancelled && !r.must_reconnect, "{r:?}");
}

/// A file that arrives short (or long) is not saved, and the sync stops there.
#[test]
fn a_file_of_the_wrong_length_is_not_saved() {
    let e = env();
    let mut short = MemObject::new("1", "DCIM/A.MOV", &[1u8; 50_000]);
    short.reported_size = Some(60_000);
    let mut src = Guarded::new(phone(vec![short, MemObject::new("2", "DCIM/B.HEIC", b"b")]));
    let (plan, _) = do_scan(&mut src, &e);
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    let r = r.unwrap();
    assert_eq!((r.failed, r.copied), (1, 0), "{ev:?}");
    assert!(r.must_reconnect);
    // The data was read to the end, so only the sync can tell the connection is now suspect.
    assert!(src.is_spoiled(), "a wrong length must spoil the connection");
    assert!(
        failures(&ev)[0].contains("sent 50000 bytes for this 60000-byte file"),
        "{ev:?}"
    );
    assert!(files_under(&e.root).is_empty());
}

/// Seen on a real iPhone: after Stop part-way through a file, the same
/// connection answered each later request with the file requested before it, and 227 photos
/// were saved under their neighbours' names. Now the sync asks for a new connection; and even
/// over the old one, a file whose length doesn't match is not saved.
#[test]
fn after_an_abandoned_transfer_no_file_is_saved_with_another_files_data() {
    let e = env();
    let ctl = SyncControl::default();
    let mut b = MemObject::new("2", "DCIM/B.MOV", &[2u8; 100_000]);
    b.trip = Some((10_000, ctl.cancel.clone()));
    let mut src = Lagging::new(phone(vec![
        MemObject::new("1", "DCIM/A.JPG", &[1u8; 1_000]),
        b,
        MemObject::new("3", "DCIM/C.JPG", &[3u8; 3_000]),
        MemObject::new("4", "DCIM/D.JPG", &[4u8; 4_000]),
    ]));
    let own_data = |root: &Path| {
        for f in files_under(root) {
            let want = match f.as_str() {
                "DCIM/A.JPG" => 1u8,
                "DCIM/B.MOV" => 2,
                "DCIM/C.JPG" => 3,
                "DCIM/D.JPG" => 4,
                other => panic!("unexpected file {other}"),
            };
            let data = fs::read(root.join(&f)).unwrap();
            assert!(
                data.iter().all(|&x| x == want),
                "{f} holds another file's data"
            );
        }
    };

    let (plan, _) = do_scan(&mut src, &e);
    let (r, _) = do_sync(&mut src, &plan, &e, &pending(&plan), &ctl);
    let r = r.unwrap();
    assert!(r.cancelled && r.must_reconnect, "{r:?}");
    assert_eq!(files_under(&e.root), set(&["DCIM/A.JPG"]));

    // Carry on over the same connection anyway, as the app did before this fix.
    ctl.cancel.store(false, Ordering::Relaxed);
    src.inner.object_mut("2").trip = None;
    let (plan2, _) = do_scan(&mut src, &e);
    let (r2, ev2) = do_sync(&mut src, &plan2, &e, &pending(&plan2), &ctl);
    let r2 = r2.unwrap();
    assert!(r2.must_reconnect, "{ev2:?}");
    assert_eq!(r2.failed, 1, "{ev2:?}");
    assert!(failures(&ev2)[0].contains("bytes for this"), "{ev2:?}");
    own_data(&e.root);
    assert!(
        !e.root.join("DCIM/D.JPG").exists(),
        "carried on after the bad file"
    );

    // A new connection: the rest arrives intact.
    src.reconnect();
    let (plan3, _) = do_scan(&mut src, &e);
    let (r3, ev3) = do_sync(&mut src, &plan3, &e, &pending(&plan3), &ctl);
    let r3 = r3.unwrap();
    assert!(!r3.must_reconnect && r3.failed == 0, "{ev3:?}");
    assert_eq!(
        files_under(&e.root),
        set(&["DCIM/A.JPG", "DCIM/B.MOV", "DCIM/C.JPG", "DCIM/D.JPG"])
    );
    own_data(&e.root);
}

/// Problems on the device and in the backup folder are reported, not dropped.
#[test]
fn device_and_backup_problems_are_reported() {
    let e = env();
    fs::create_dir_all(e.root.join("DCIM")).unwrap();
    fs::write(e.root.join(format!("DCIM/old.jpg{TEMP_SUFFIX}")), b"x").unwrap();
    let mut src = phone(vec![MemObject::new("1", "DCIM/a.jpg", b"a")]);
    src.issues.push(ScanIssue::new(
        "DCIM/101APPLE",
        "could not list this folder: test",
    ));
    let (plan, summary) = do_scan(&mut src, &e);
    assert_eq!(summary.issues, 2, "{:?}", plan.issues);
    assert!(
        plan.issues
            .iter()
            .any(|i| i.problem.contains("unfinished copy"))
    );
    assert!(plan.issues.iter().any(|i| i.location == "DCIM/101APPLE"));
}

/// Two device names that are the same file on Windows are not both copied.
#[test]
fn names_differing_only_in_case_are_not_both_copied() {
    let e = env();
    let mut src = phone(vec![
        MemObject::new("1", "DCIM/IMG.JPG", b"upper"),
        MemObject::new("2", "DCIM/img.jpg", b"lower"),
    ]);
    let (plan, summary) = do_scan(&mut src, &e);
    assert_eq!(
        (plan.entries.len(), summary.issues),
        (1, 1),
        "{:?}",
        plan.issues
    );
}

/// Crash recovery: the unfinished copy is removed and the file is simply copied next time.
#[test]
fn after_a_crash_the_file_is_copied_again_not_skipped() {
    let e = env();
    fs::create_dir_all(e.root.join("DCIM")).unwrap();
    let temp = e.root.join(format!("DCIM/IMG_0001.HEIC{TEMP_SUFFIX}"));
    fs::write(&temp, b"hal").unwrap();
    e.state
        .begin_copy(&crate::state::InProgress {
            device: "serial:PHONE-A".into(),
            root: e.root.clone(),
            source: "DCIM/IMG_0001.HEIC".into(),
            temp: temp.clone(),
            replace: None,
            destination: None,
        })
        .unwrap();
    let rec = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
    assert!(rec.temp_removed, "{}", rec.note);
    let mut src = phone(vec![MemObject::new(
        "1",
        "DCIM/IMG_0001.HEIC",
        b"half no more",
    )]);
    let (plan, _) = do_scan(&mut src, &e);
    assert_eq!(plan.entries[0].status, Status::New);
    let (r, _) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().copied, 1);
}

/// Fake mode: a folder on the PC acts as the phone and is only read.
#[test]
fn folder_source_is_read_only() {
    let e = env();
    let fake = e.base.join("fakephone");
    fs::create_dir_all(fake.join("DCIM/100APPLE")).unwrap();
    fs::write(fake.join("DCIM/100APPLE/IMG_0001.JPG"), b"jpeg").unwrap();
    let before = files_under(&fake);
    let mut src = FolderSource::new(&fake).unwrap();
    let (plan, summary) = do_scan(&mut src, &e);
    assert_eq!(summary.new, 1);
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().copied, 1, "{:?}", failures(&ev));
    assert_eq!(
        fs::read(e.root.join("DCIM/100APPLE/IMG_0001.JPG")).unwrap(),
        b"jpeg"
    );
    assert_eq!(files_under(&fake), before);
}

// ---------------------------------------------------------------------------------------------
// Archiving files deleted from the phone

fn put(root: &Path, rel: &str, data: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, data).unwrap();
}

fn do_archive(
    plan: &ScanPlan,
    stamp: &str,
) -> (Result<ArchiveReport, SyncError>, Vec<ArchiveEvent>) {
    let mut events = Vec::new();
    let r = archive_removed(plan, stamp, true, &mut |ev| events.push(ev));
    (r, events)
}

/// Archive without the explicit OK.
fn do_archive_unconfirmed(
    plan: &ScanPlan,
    stamp: &str,
) -> (Result<ArchiveReport, SyncError>, Vec<ArchiveEvent>) {
    let mut events = Vec::new();
    let r = archive_removed(plan, stamp, false, &mut |ev| events.push(ev));
    (r, events)
}

fn removed_paths(plan: &ScanPlan) -> Vec<&str> {
    plan.removed.iter().map(|r| r.path.as_str()).collect()
}

#[test]
fn files_deleted_from_the_phone_are_moved_into_a_dated_folder() {
    let e = env();
    put(&e.root, "DCIM/100APPLE/IMG_A.HEIC", b"a");
    put(&e.root, "DCIM/100APPLE/IMG_B.HEIC", b"bb-old");
    put(&e.root, "DCIM/100APPLE/IMG_B_new.HEIC", b"b"); // not on the phone: an ordinary file
    put(&e.root, "DCIM/100APPLE/IMG_C.HEIC", b"c"); // C was deleted from the phone...
    put(&e.root, "DCIM/100APPLE/IMG_C_new2.HEIC", b"cc"); // ...with this one
    let mut src = phone(vec![
        MemObject::new("1", "DCIM/100APPLE/IMG_A.HEIC", b"a"),
        MemObject::new("2", "DCIM/100APPLE/IMG_B.HEIC", b"b"),
    ]);
    let (plan, summary) = do_scan(&mut src, &e);
    assert_eq!(summary.removed, 3);
    assert_eq!(
        removed_paths(&plan),
        [
            "DCIM/100APPLE/IMG_B_new.HEIC",
            "DCIM/100APPLE/IMG_C.HEIC",
            "DCIM/100APPLE/IMG_C_new2.HEIC"
        ]
    );

    let (r, ev) = do_archive(&plan, "2026-01-02 03-04-05");
    let r = r.unwrap();
    assert_eq!((r.moved, r.failed), (3, 0), "{ev:?}");
    let dir = format!("{ARCHIVE_DIR}/2026-01-02 03-04-05");
    assert_eq!(r.folder.unwrap().as_str(), dir);
    let expected: Vec<String> = vec![
        "DCIM/100APPLE/IMG_A.HEIC".into(),
        "DCIM/100APPLE/IMG_B.HEIC".into(),
        // Flat: no device folders inside the dated folder.
        format!("{dir}/IMG_B_new.HEIC"),
        format!("{dir}/IMG_C.HEIC"),
        format!("{dir}/IMG_C_new2.HEIC"),
    ];
    assert_eq!(
        files_under(&e.root),
        expected.into_iter().collect::<BTreeSet<_>>()
    );
    assert_eq!(
        fs::read(e.root.join(&dir).join("IMG_C.HEIC")).unwrap(),
        b"c"
    );

    // Later scans leave the archive alone: it isn't counted, and nothing in it is "deleted".
    let (plan2, s2) = do_scan(&mut src, &e);
    assert_eq!((s2.removed, s2.backup_files), (0, 2));
    assert!(plan2.removed.is_empty());
}

#[test]
fn deleted_files_with_the_same_name_get_numbered_in_the_flat_archive() {
    let e = env();
    put(&e.root, "DCIM/100APPLE/IMG_0001.HEIC", b"first");
    put(&e.root, "DCIM/101APPLE/IMG_0001.HEIC", b"second");
    put(&e.root, "DCIM/102APPLE/IMG_0001.HEIC", b"third");
    let mut src = phone(vec![MemObject::new(
        "1",
        "DCIM/103APPLE/IMG_0002.HEIC",
        b"x",
    )]);
    let (plan, _) = do_scan(&mut src, &e);
    let (r, ev) = do_archive(&plan, "2026-01-02 03-04-05");
    assert_eq!(r.unwrap().moved, 3, "{ev:?}");
    let dir = e.root.join(ARCHIVE_DIR).join("2026-01-02 03-04-05");
    assert_eq!(fs::read(dir.join("IMG_0001.HEIC")).unwrap(), b"first");
    assert_eq!(fs::read(dir.join("IMG_0001 (2).HEIC")).unwrap(), b"second");
    assert_eq!(fs::read(dir.join("IMG_0001 (3).HEIC")).unwrap(), b"third");
    let moved: Vec<&str> = ev
        .iter()
        .filter_map(|e| match e {
            ArchiveEvent::Moved { to, .. } => to.file_name().into(),
            _ => None,
        })
        .collect();
    assert_eq!(
        moved,
        ["IMG_0001.HEIC", "IMG_0001 (2).HEIC", "IMG_0001 (3).HEIC"]
    );
}

#[test]
fn with_nothing_deleted_no_archive_folder_is_made() {
    let e = env();
    put(&e.root, "DCIM/A.HEIC", b"a");
    let mut src = phone(vec![MemObject::new("1", "DCIM/A.HEIC", b"a")]);
    let (plan, _) = do_scan(&mut src, &e);
    let (r, _) = do_archive(&plan, "2026-01-02 03-04-05");
    assert_eq!(r.unwrap(), ArchiveReport::default());
    assert!(!e.root.join(ARCHIVE_DIR).exists());
}

#[test]
fn files_under_a_device_folder_the_scan_could_not_read_are_never_archived() {
    let e = env();
    put(&e.root, "DCIM/100APPLE/X.HEIC", b"x");
    put(&e.root, "DCIM/101APPLE/Y.HEIC", b"y");
    let mut src = phone(vec![MemObject::new("1", "DCIM/100APPLE/A.HEIC", b"a")]);
    src.incomplete = vec!["DCIM/101APPLE".into()];
    let (plan, _) = do_scan(&mut src, &e);
    assert_eq!(removed_paths(&plan), ["DCIM/100APPLE/X.HEIC"]);
    assert!(
        plan.issues
            .iter()
            .any(|i| i.problem.contains("couldn't read their device folder")),
        "{:?}",
        plan.issues
    );
    // The whole device only partly listed: nothing counts as deleted.
    src.incomplete = vec![String::new()];
    let (plan, _) = do_scan(&mut src, &e);
    assert!(plan.removed.is_empty());
}

#[test]
fn a_locked_phone_showing_no_files_archives_nothing() {
    let e = env();
    put(&e.root, "DCIM/A.HEIC", b"a");
    let mut src = phone(vec![]);
    let (plan, _) = do_scan(&mut src, &e);
    assert!(plan.removed.is_empty());
    assert!(
        plan.issues
            .iter()
            .any(|i| i.problem.contains("is it locked"))
    );
}

#[test]
fn another_phones_backup_folder_is_never_archived() {
    let e = env();
    let ctl = SyncControl::default();
    let mut a = MemSource::new("PHONE-A", vec![MemObject::new("1", "DCIM/A.HEIC", b"a")]);
    let (plan_a, _) = do_scan(&mut a, &e);
    do_sync(&mut a, &plan_a, &e, &pending(&plan_a), &ctl)
        .0
        .unwrap();
    assert!(
        matches!(owner::read(&e.root), OwnerState::Owned(o) if o.device_id == "serial:PHONE-A")
    );

    // Phone B pointed at A's folder: A's file looks deleted, but must not be archived.
    let mut b = MemSource::new("PHONE-B", vec![MemObject::new("2", "DCIM/B.HEIC", b"b")]);
    let (plan_b, _) = do_scan(&mut b, &e);
    assert_eq!(removed_paths(&plan_b), ["DCIM/A.HEIC"]);
    assert!(plan_b.folder_blocked().is_some());
    let (r, _) = do_archive(&plan_b, "2026-01-02 03-04-05");
    assert!(matches!(r, Err(SyncError::WrongFolder(_))), "{r:?}");
    assert!(e.root.join("DCIM/A.HEIC").exists());
    assert!(!e.root.join(ARCHIVE_DIR).exists());

    // ...and B's files must not be copied into it either.
    let (r, ev) = do_sync(&mut b, &plan_b, &e, &pending(&plan_b), &ctl);
    assert!(matches!(r, Err(SyncError::WrongFolder(_))), "{r:?} {ev:?}");
    assert_eq!(files_under(&e.root), set(&["DCIM/A.HEIC"]));
    assert!(
        matches!(owner::read(&e.root), OwnerState::Owned(o) if o.device_id == "serial:PHONE-A")
    );
}

#[test]
fn a_folder_handed_to_a_new_phone_takes_its_files() {
    let e = env();
    let ctl = SyncControl::default();
    let mut old = MemSource::new("OLD", vec![MemObject::new("1", "DCIM/A.HEIC", b"a")]);
    let (plan, _) = do_scan(&mut old, &e);
    do_sync(&mut old, &plan, &e, &pending(&plan), &ctl)
        .0
        .unwrap();

    // The new phone is refused until the folder is made its own (what the window's
    // "Use this folder for this device" does), then it syncs and archives normally.
    let mut new = MemSource::new("NEW", vec![MemObject::new("2", "DCIM/B.HEIC", b"b")]);
    let (plan, _) = do_scan(&mut new, &e);
    assert!(plan.folder_blocked().is_some());
    owner::hand_over(
        &e.root,
        &new.identity().key,
        "new phone",
        Signals::default(),
        View::WindowsWpd,
        &plan.archive_name,
        "2026-01-01T00:00:00Z",
    )
    .unwrap();
    let (plan, _) = do_scan(&mut new, &e);
    assert!(plan.folder_blocked().is_none());
    let copied = do_sync(&mut new, &plan, &e, &pending(&plan), &ctl)
        .0
        .unwrap()
        .copied;
    assert_eq!(copied, 1);
    assert_eq!(do_archive(&plan, "2026-01-02 03-04-05").0.unwrap().moved, 1);
}

#[test]
fn a_folder_claimed_by_another_phone_after_the_scan_is_refused() {
    let e = env();
    let mut a = MemSource::new("PHONE-A", vec![MemObject::new("1", "DCIM/A.HEIC", b"a")]);
    let (plan, _) = do_scan(&mut a, &e);
    assert!(plan.folder_blocked().is_none());
    fs::create_dir_all(&e.root).unwrap();
    let root = fs::canonicalize(&e.root).unwrap();
    let other = owner::FolderOwner::new(
        "serial:SOMEONE-ELSE",
        "another phone",
        Signals::default(),
        View::WindowsWpd,
        "2026-01-01T00:00:00Z",
    );
    owner::write(&root, &other).unwrap();
    let (r, _) = do_sync(&mut a, &plan, &e, &pending(&plan), &SyncControl::default());
    assert!(matches!(r, Err(SyncError::WrongFolder(_))), "{r:?}");
    assert!(files_under(&e.root).is_empty());
}

#[test]
fn a_backup_file_that_changed_since_the_scan_is_left_in_place() {
    let e = env();
    put(&e.root, "DCIM/C.HEIC", b"c");
    let mut src = phone(vec![MemObject::new("1", "DCIM/A.HEIC", b"a")]);
    let (plan, _) = do_scan(&mut src, &e);
    fs::write(e.root.join("DCIM/C.HEIC"), b"edited since").unwrap();
    let (r, ev) = do_archive(&plan, "2026-01-02 03-04-05");
    assert_eq!(r.unwrap().moved, 0);
    assert!(
        matches!(&ev[..], [ArchiveEvent::Failed { error, .. }] if error.contains("changed since the scan"))
    );
    assert_eq!(
        fs::read(e.root.join("DCIM/C.HEIC")).unwrap(),
        b"edited since"
    );
    assert!(!e.root.join(ARCHIVE_DIR).exists());
}

#[test]
fn archiving_never_follows_a_junction_swapped_in_after_the_scan() {
    let e = env();
    let victim = e.base.join("victim");
    fs::create_dir_all(&victim).unwrap();
    fs::write(victim.join("C.HEIC"), b"c").unwrap();
    put(&e.root, "DCIM/OLD/C.HEIC", b"c");
    let mut src = phone(vec![MemObject::new("1", "DCIM/100APPLE/A.HEIC", b"a")]);
    let (plan, _) = do_scan(&mut src, &e);
    assert_eq!(removed_paths(&plan), ["DCIM/OLD/C.HEIC"]);
    fs::remove_dir_all(e.root.join("DCIM/OLD")).unwrap();
    make_junction(&e.root.join("DCIM/OLD"), &victim);
    let (r, _) = do_archive(&plan, "2026-01-02 03-04-05");
    assert_eq!(r.unwrap().moved, 0);
    assert_eq!(fs::read(victim.join("C.HEIC")).unwrap(), b"c");
}

#[test]
fn device_names_the_app_uses_in_the_backup_folder_are_not_copied() {
    let e = env();
    let mut src = phone(vec![
        MemObject::new("1", &format!("{ARCHIVE_DIR}/X.HEIC"), b"x"),
        MemObject::new("2", owner::MARKER, b"{}"),
        MemObject::new("3", "DCIM/A.HEIC", b"a"),
    ]);
    let (plan, _) = do_scan(&mut src, &e);
    let sources: Vec<&str> = plan.entries.iter().map(|e| e.source.as_str()).collect();
    assert_eq!(sources, ["DCIM/A.HEIC"]);
    assert_eq!(
        plan.issues
            .iter()
            .filter(|i| i.problem.contains("used by the app's own files"))
            .count(),
        2
    );
}

// ---------------------------------------------------------------------------------------------
// Further cases: gaps found by reading the tests above against the rules.

/// Without a reported size a copy can't be checked, so the file is never copied,
/// not even when asked for directly.
#[test]
fn a_file_without_a_reported_size_is_never_copied() {
    let e = env();
    let mut odd = MemObject::new("1", "DCIM/A.MOV", &[1u8; 50_000]);
    odd.reported_size = None;
    let mut src = phone(vec![odd, MemObject::new("2", "DCIM/B.HEIC", b"b")]);
    let (plan, _) = do_scan(&mut src, &e);
    let a = entry_of(&plan, "DCIM/A.MOV");
    assert!(
        matches!(&plan.entries[a].status, Status::Error(m) if m.contains("did not report")),
        "{:?}",
        plan.entries[a].status
    );
    assert_eq!(pending(&plan).len(), 1, "only B is offered for copying");
    let item = SyncItem {
        entry: a,
        mode: CopyMode::NoReplace,
    };
    let (r, ev) = do_sync(&mut src, &plan, &e, &[item], &SyncControl::default());
    let r = r.unwrap();
    assert_eq!((r.copied, r.failed), (0, 1), "{ev:?}");
    assert!(!r.must_reconnect, "no transfer was started");
    assert!(files_under(&e.root).is_empty());
}

/// The length check can't catch a neighbour of the same size. Over a lagging
/// connection with no guard, a later sync saves C holding B's data. This shows the danger
/// is real in the fake, and why the connection must not be read again.
#[test]
fn without_the_guard_a_same_size_neighbour_is_saved_with_the_wrong_data() {
    let e = env();
    let ctl = SyncControl::default();
    let mut src = Lagging::new(phone(same_size_neighbours(&ctl)));
    let (plan, _) = do_scan(&mut src, &e);
    let (r, _) = do_sync(&mut src, &plan, &e, &pending(&plan), &ctl);
    assert!(r.unwrap().must_reconnect);
    ctl.cancel.store(false, Ordering::SeqCst);
    src.inner.object_mut("2").trip = None;
    let (plan2, _) = do_scan(&mut src, &e);
    let (_r2, _) = do_sync(&mut src, &plan2, &e, &pending(&plan2), &ctl);
    assert_eq!(
        fs::read(e.root.join("DCIM/C.JPG")).unwrap(),
        vec![2u8; 100_000],
        "expected the lagging fake to save B's data as C (if this fails, the fake no longer \
         reproduces the iPhone, and the guarded test below proves nothing)"
    );
}

/// With the guard, nothing more is read over that connection after an
/// unfinished transfer, so no file of any size is saved with a neighbour's data. A new
/// connection then copies everything correctly.
#[test]
fn a_guarded_connection_reads_nothing_more_after_an_unfinished_transfer() {
    let e = env();
    let ctl = SyncControl::default();
    let mut src = Guarded::new(Lagging::new(phone(same_size_neighbours(&ctl))));
    let (plan, _) = do_scan(&mut src, &e);
    let (r, _) = do_sync(&mut src, &plan, &e, &pending(&plan), &ctl);
    let r = r.unwrap();
    assert!(r.cancelled && r.must_reconnect, "{r:?}");
    assert!(src.is_spoiled());

    // Carry on over the same connection anyway: not even a new scan is answered...
    ctl.cancel.store(false, Ordering::SeqCst);
    src.inner.inner.object_mut("2").trip = None;
    let rescan = scan(
        &mut src,
        &e.root,
        &Default::default(),
        &open_opts(),
        &AtomicBool::new(false),
        &mut |_| {},
    );
    assert!(rescan.is_err(), "a spoiled connection listed the device");
    // ...and a sync started from the earlier scan is refused at the first file.
    let plan2 = plan;
    let (r2, ev2) = do_sync(&mut src, &plan2, &e, &pending(&plan2), &ctl);
    let r2 = r2.unwrap();
    assert_eq!((r2.copied, r2.failed), (0, 1), "{ev2:?}");
    assert!(r2.must_reconnect);
    assert!(failures(&ev2)[0].contains("opened again"), "{ev2:?}");
    assert_own_data(&e.root, &src.inner.inner);

    // A new connection.
    let mut lagging = src.inner;
    lagging.reconnect();
    let mut src = Guarded::new(lagging);
    let (plan3, _) = do_scan(&mut src, &e);
    let (r3, ev3) = do_sync(&mut src, &plan3, &e, &pending(&plan3), &ctl);
    let r3 = r3.unwrap();
    assert!(!r3.must_reconnect && r3.failed == 0, "{ev3:?}");
    assert_eq!(files_under(&e.root).len(), 4);
    assert_own_data(&e.root, &src.inner.inner);
}

/// A, then B (Stop trips part-way through it), then C and D with B's size but other data.
fn same_size_neighbours(ctl: &SyncControl) -> Vec<MemObject> {
    let mut b = MemObject::new("2", "DCIM/B.MOV", &[2u8; 100_000]);
    b.trip = Some((10_000, ctl.cancel.clone()));
    vec![
        MemObject::new("1", "DCIM/A.JPG", &[1u8; 1_000]),
        b,
        MemObject::new("3", "DCIM/C.JPG", &[3u8; 100_000]),
        MemObject::new("4", "DCIM/D.JPG", &[4u8; 100_000]),
    ]
}

/// A skip request names the file the window showed. If the copy has already
/// moved on, the request doesn't hit the file being copied now.
#[test]
fn a_skip_request_for_an_earlier_file_does_not_skip_the_current_one() {
    let e = env();
    let ctl = SyncControl::default();
    let mut b = MemObject::new("2", "DCIM/B.MOV", &[2u8; 100_000]);
    b.trip = Some((10_000, ctl.skip_current.clone()));
    let mut src = phone(vec![
        MemObject::new("1", "DCIM/A.MOV", &[1u8; 100_000]),
        b,
        MemObject::new("3", "DCIM/C.HEIC", b"c"),
    ]);
    let (plan, _) = do_scan(&mut src, &e);
    // The window still showed A when the skip was pressed; the copy is already on B.
    ctl.skip_entry
        .store(entry_of(&plan, "DCIM/A.MOV"), Ordering::SeqCst);
    let (r, ev) = do_sync(&mut src, &plan, &e, &pending(&plan), &ctl);
    let r = r.unwrap();
    assert_eq!((r.copied, r.skipped), (3, 0), "{ev:?}");
    assert!(!r.must_reconnect);
    assert!(
        e.state
            .skip_list(&Scope::new("serial:PHONE-A", &e.root))
            .unwrap()
            .is_empty()
    );
    assert_own_data(&e.root, &src);
}

/// Replace only overwrites a backup that is still what the scan saw.
#[test]
fn replace_refuses_a_backup_that_changed_since_the_scan() {
    let e = env();
    put(&e.root, "DCIM/IMG.HEIC", b"old!");
    let mut src = phone(vec![MemObject::new("1", "DCIM/IMG.HEIC", b"new!")]);
    let (plan, _) = do_scan(&mut src, &e);
    assert_eq!(plan.entries[0].status, Status::Verified);
    // The user edits the backup after the scan.
    fs::write(e.root.join("DCIM/IMG.HEIC"), b"edited since the scan").unwrap();
    let item = SyncItem {
        entry: 0,
        mode: CopyMode::Replace,
    };
    let (r, ev) = do_sync(&mut src, &plan, &e, &[item], &SyncControl::default());
    assert_eq!(r.unwrap().failed, 1, "{ev:?}");
    assert!(
        failures(&ev)[0].contains("changed since the scan"),
        "{ev:?}"
    );
    assert_eq!(
        fs::read(e.root.join("DCIM/IMG.HEIC")).unwrap(),
        b"edited since the scan"
    );
}

/// Replace is refused for a file that wasn't backed up at scan time, even if
/// a file has turned up at its name since.
#[test]
fn replace_refuses_a_file_that_was_not_backed_up_when_scanned() {
    let e = env();
    let mut src = phone(vec![MemObject::new("1", "DCIM/IMG.HEIC", b"new!")]);
    let (plan, _) = do_scan(&mut src, &e);
    assert_eq!(plan.entries[0].status, Status::New);
    // Same size as the device file, so only the "was it backed up when scanned?" check can
    // stop the overwrite.
    put(&e.root, "DCIM/IMG.HEIC", b"NEW!");
    let item = SyncItem {
        entry: 0,
        mode: CopyMode::Replace,
    };
    let (r, ev) = do_sync(&mut src, &plan, &e, &[item], &SyncControl::default());
    assert_eq!(r.unwrap().failed, 1, "{ev:?}");
    assert!(
        failures(&ev)[0].contains("backed up when you scanned"),
        "{ev:?}"
    );
    assert_eq!(fs::read(e.root.join("DCIM/IMG.HEIC")).unwrap(), b"NEW!");
}

/// A backup folder inside this one that has its own owner marker (another
/// phone's) is left alone: nothing in it counts as deleted, and nothing is archived from it.
/// Archive folders at any depth are never treated as backup files either.
#[test]
fn a_backup_folder_nested_inside_this_one_is_left_alone() {
    let e = env();
    let ctl = SyncControl::default();
    let mut a = MemSource::new("PHONE-A", vec![MemObject::new("1", "DCIM/A.HEIC", b"a")]);
    let (plan, _) = do_scan(&mut a, &e);
    do_sync(&mut a, &plan, &e, &pending(&plan), &ctl).0.unwrap();

    let nested = e.root.join("PhoneB");
    put(&nested, "202611_a/IMG_B.JPG", b"phone b's photo");
    put(
        &nested,
        &format!("{ARCHIVE_DIR}/2026-01-01 00-00-00/OLD.JPG"),
        b"old",
    );
    owner::write(
        &fs::canonicalize(&nested).unwrap(),
        &owner::FolderOwner::new(
            "serial:PHONE-B",
            "Phone B",
            Signals::default(),
            View::WindowsWpd,
            "2026-01-01T00:00:00Z",
        ),
    )
    .unwrap();
    // An archive folder deeper down, with no marker: still not backup files.
    put(
        &e.root,
        &format!("Old/{ARCHIVE_DIR}/2025-01-01 00-00-00/GONE.JPG"),
        b"gone",
    );
    let before = files_under_all(&nested);

    let (plan2, _) = do_scan(&mut a, &e);
    assert!(
        removed_paths(&plan2).is_empty(),
        "{:?}",
        removed_paths(&plan2)
    );
    assert!(
        plan2
            .issues
            .iter()
            .any(|i| i.location.contains("PhoneB") && i.problem.contains("another backup folder")),
        "{:?}",
        plan2.issues
    );
    let (r, _) = do_archive(&plan2, "2026-01-02 03-04-05");
    assert_eq!(r.unwrap().moved, 0);
    assert_eq!(files_under_all(&nested), before);
    assert!(
        e.root
            .join(format!("Old/{ARCHIVE_DIR}/2025-01-01 00-00-00/GONE.JPG"))
            .exists()
    );
}

/// A device file using one of the app's names below the top level (a marker
/// would make its folder look like another phone's backup) is not copied either.
#[test]
fn device_names_the_app_uses_are_not_copied_at_any_depth() {
    let e = env();
    let mut src = phone(vec![
        MemObject::new("1", &format!("202611_a/{}", owner::MARKER), b"{}"),
        MemObject::new("2", &format!("202611_a/{ARCHIVE_DIR}/X.JPG"), b"x"),
        MemObject::new("3", "202611_a/A.JPG", b"a"),
    ]);
    let (plan, _) = do_scan(&mut src, &e);
    let sources: Vec<&str> = plan.entries.iter().map(|e| e.source.as_str()).collect();
    assert_eq!(sources, ["202611_a/A.JPG"]);
}

/// A file that turns up at the target while the copy is running is never overwritten: the
/// no-replace move fails, the finished copy is dropped, and the file is reported.
#[test]
fn a_file_that_turns_up_during_the_copy_is_never_overwritten() {
    let e = env();
    fs::create_dir_all(e.root.join("DCIM")).unwrap();
    let mut obj = MemObject::new("1", "DCIM/IMG.HEIC", b"from the phone");
    obj.on_first_read = Some((e.root.join("DCIM/IMG.HEIC"), b"theirs".to_vec()));
    let mut src = phone(vec![obj]);
    let (plan, _) = do_scan(&mut src, &e);
    assert_eq!(plan.entries[0].status, Status::New);
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().failed, 1, "{ev:?}");
    assert!(failures(&ev)[0].contains("not overwritten"), "{ev:?}");
    assert_eq!(fs::read(e.root.join("DCIM/IMG.HEIC")).unwrap(), b"theirs");
    assert_eq!(
        files_under(&e.root),
        set(&["DCIM/IMG.HEIC"]),
        "temporary file left behind"
    );
}

/// Archiving never moves files through a junction at the archive folder or
/// at the dated folder inside it.
#[test]
fn archiving_never_writes_through_a_junction_at_the_archive_folder() {
    let stamp = "2026-01-02 03-04-05";
    for junction_at in [ARCHIVE_DIR.to_string(), format!("{ARCHIVE_DIR}/{stamp}")] {
        let e = env();
        let outside = e.base.join("outside");
        fs::create_dir_all(&outside).unwrap();
        put(&e.root, "DCIM/OLD.HEIC", b"old");
        let mut src = phone(vec![MemObject::new("1", "DCIM/A.HEIC", b"a")]);
        let (plan, _) = do_scan(&mut src, &e);
        assert_eq!(removed_paths(&plan), ["DCIM/OLD.HEIC"]);
        let link = e.root.join(&junction_at);
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        make_junction(&link, &outside);
        let (r, _) = do_archive(&plan, stamp);
        assert_eq!(r.unwrap().moved, 0, "junction at {junction_at}");
        assert_eq!(fs::read(e.root.join("DCIM/OLD.HEIC")).unwrap(), b"old");
        assert!(
            fs::read_dir(&outside).unwrap().next().is_none(),
            "something landed outside via {junction_at}"
        );
    }
}

/// What is in the way of the temporary file. A stale temporary file of ours is
/// moved aside; a folder or a junction there is refused and left alone.
#[test]
fn something_at_the_temporary_name_is_handled_safely() {
    let temp_name = format!("DCIM/IMG.HEIC{TEMP_SUFFIX}");

    // A stale temporary file (no crash record): moved aside, and the copy completes.
    let e = env();
    put(&e.root, &temp_name, b"half");
    let mut src = phone(vec![MemObject::new("1", "DCIM/IMG.HEIC", b"whole")]);
    let (plan, _) = do_scan(&mut src, &e);
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().copied, 1, "{ev:?}");
    // The stale file may be a whole copy, so it is moved aside, never deleted.
    let aside = format!("DCIM/IMG.HEIC (kept 1){TEMP_SUFFIX}");
    assert_eq!(
        files_under(&e.root),
        set(&["DCIM/IMG.HEIC", aside.as_str()])
    );
    assert_eq!(fs::read(e.root.join("DCIM/IMG.HEIC")).unwrap(), b"whole");
    assert_eq!(fs::read(e.root.join(&aside)).unwrap(), b"half");

    // A folder at the temporary name: refused, folder kept.
    let e = env();
    put(&e.root, &format!("{temp_name}/keep.txt"), b"keep");
    let mut src = phone(vec![MemObject::new("1", "DCIM/IMG.HEIC", b"whole")]);
    let (plan, _) = do_scan(&mut src, &e);
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().failed, 1, "{ev:?}");
    assert!(failures(&ev)[0].contains("in the way"), "{ev:?}");
    assert_eq!(
        fs::read(e.root.join(format!("{temp_name}/keep.txt"))).unwrap(),
        b"keep"
    );

    // A junction at the temporary name: refused, its target untouched.
    let e = env();
    let outside = e.base.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("precious.txt"), b"precious").unwrap();
    fs::create_dir_all(e.root.join("DCIM")).unwrap();
    make_junction(&e.root.join(&temp_name), &outside);
    let mut src = phone(vec![MemObject::new("1", "DCIM/IMG.HEIC", b"whole")]);
    let (plan, _) = do_scan(&mut src, &e);
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().failed, 1, "{ev:?}");
    assert_eq!(fs::read(outside.join("precious.txt")).unwrap(), b"precious");
}

#[test]
fn a_folder_made_through_another_view_is_refused_for_copying_and_archiving() {
    let e = env();
    let mut phone = MemSource::new("PHONE-A", vec![MemObject::new("1", "DCIM/A.HEIC", b"a")]);
    let ctl = SyncControl::default();
    let (plan, _) = do_scan(&mut phone, &e);
    do_sync(&mut phone, &plan, &e, &pending(&plan), &ctl)
        .0
        .unwrap();
    // The folder is this very device's, made through the Windows driver.
    let root = fs::canonicalize(&e.root).unwrap();
    let same = owner::FolderOwner::new(
        &phone.identity().key,
        "phone",
        Signals::default(),
        View::WindowsWpd,
        "2026-01-01T00:00:00Z",
    );
    owner::write(&root, &same).unwrap();
    let (plan, _) = do_scan(&mut phone, &e);
    assert!(plan.folder_blocked().is_none());

    // The same folder, marked as made through AFC: refused with a reason naming both views.
    let other_view = owner::FolderOwner {
        view: Some(View::Afc),
        ..same
    };
    owner::write(&root, &other_view).unwrap();
    // A.HEIC has left the phone and B.HEIC is new: there is something to archive and to copy.
    phone.objects = vec![MemObject::new("2", "DCIM/B.HEIC", b"b")];
    let (plan2, _) = do_scan(&mut phone, &e);
    assert_eq!(plan2.removed.len(), 1);
    let why = plan2.folder_blocked().expect("a different view is refused");
    assert!(why.contains("afc") && why.contains("windows-wpd"), "{why}");
    let before = files_under(&e.root);
    let (r, _) = do_sync(&mut phone, &plan2, &e, &pending(&plan2), &ctl);
    assert!(matches!(r, Err(SyncError::WrongFolder(_))), "{r:?}");
    let (r, _) = do_archive(&plan2, "2026-01-02 03-04-05");
    assert!(matches!(r, Err(SyncError::WrongFolder(_))), "{r:?}");
    assert_eq!(
        files_under(&e.root),
        before,
        "nothing changed in the folder"
    );
}

/// Every archive name the marker lists is left out of the scan, not only the current one: after a
/// rename, the old archive must not look like files deleted from the device.
#[test]
fn every_archive_name_in_the_marker_is_left_out_of_the_scan() {
    let e = env();
    let mut a = MemSource::new("PHONE-A", vec![MemObject::new("1", "DCIM/A.HEIC", b"a")]);
    put(&e.root, "DCIM/A.HEIC", b"a");
    put(
        &e.root,
        "_Old archive/2025-01-01 00-00-00/GONE.JPG",
        b"gone",
    );
    put(
        &e.root,
        "Sub/_Old archive/2025-01-01 00-00-00/GONE2.JPG",
        b"gone",
    );
    let mut marker = owner::FolderOwner::new(
        &a.identity().key,
        "phone",
        Signals::default(),
        View::WindowsWpd,
        "2026-01-01T00:00:00Z",
    );
    marker.archive_names = vec!["_Old archive".into(), ARCHIVE_DIR.into()];
    owner::write(&fs::canonicalize(&e.root).unwrap(), &marker).unwrap();

    let (plan, _) = do_scan(&mut a, &e);
    assert!(
        removed_paths(&plan).is_empty(),
        "{:?}",
        removed_paths(&plan)
    );
    let (r, _) = do_archive(&plan, "2026-01-02 03-04-05");
    assert_eq!(r.unwrap().moved, 0);
    assert!(
        e.root
            .join("_Old archive/2025-01-01 00-00-00/GONE.JPG")
            .exists()
    );

    // And a device file using such a name is not copied into the backup.
    let mut b = MemSource::new(
        "PHONE-A",
        vec![MemObject::new("2", "_Old archive/X.JPG", b"x")],
    );
    let (plan, _) = do_scan(&mut b, &e);
    assert!(plan.entries.is_empty(), "{:?}", plan.entries.len());
}

// ---------------------------------------------------------------------------------------------
// Per-device settings, the archive, what to back up, changed files

fn key_of(src: &dyn DeviceSource) -> String {
    src.identity().key.clone()
}

/// Make the backup folder the device's own, as a first sync does.
fn claim(e: &Env, key: &str) {
    fs::create_dir_all(&e.root).unwrap();
    let me = owner::FolderOwner::new(
        key,
        "phone",
        Signals::default(),
        View::WindowsWpd,
        "2026-01-01T00:00:00Z",
    );
    owner::write(&fs::canonicalize(&e.root).unwrap(), &me).unwrap();
}

fn at(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(1_700_000_000 + secs)
}

fn set_mtime(path: &Path, t: SystemTime) {
    let f = fs::OpenOptions::new().write(true).open(path).unwrap();
    crate::platform::set_modified(&f, t).unwrap();
}

fn opts_with(settings: DeviceSettings) -> ScanOptions {
    ScanOptions {
        settings,
        weak_match: false,
    }
}

fn has_caution(plan: &ScanPlan, f: impl Fn(&Caution) -> bool) -> bool {
    plan.cautions.iter().any(f)
}

/// DESIGN.md 7.5: the first sync into a folder that already has files and no marker never
/// archives without an explicit OK.
#[test]
fn archiving_asks_first_on_the_first_sync_into_a_folder_that_already_has_files() {
    let e = env();
    put(&e.root, "DCIM/OLD.HEIC", b"old");
    let mut src = phone(vec![MemObject::new("1", "DCIM/A.HEIC", b"a")]);
    let (plan, _) = do_scan(&mut src, &e);
    assert_eq!(removed_paths(&plan), ["DCIM/OLD.HEIC"]);
    assert!(has_caution(&plan, |c| matches!(
        c,
        Caution::FirstSyncIntoFilledFolder { files: 1 }
    )));
    let (r, _) = do_archive_unconfirmed(&plan, STAMP);
    assert!(matches!(r, Err(SyncError::NeedsConfirmation(_))), "{r:?}");
    assert!(e.root.join("DCIM/OLD.HEIC").exists());
    assert!(!e.root.join(ARCHIVE_DIR).exists());
    // Confirmed, it archives.
    assert_eq!(do_archive(&plan, STAMP).0.unwrap().moved, 1);
    assert!(!e.root.join("DCIM/OLD.HEIC").exists());
}

/// Once the folder is this device's, the same situation needs no OK.
#[test]
fn a_folder_that_already_belongs_to_the_device_archives_without_asking() {
    let e = env();
    let mut src = phone(vec![MemObject::new("1", "DCIM/A.HEIC", b"a")]);
    claim(&e, &key_of(&src));
    put(&e.root, "DCIM/A.HEIC", b"a");
    put(&e.root, "DCIM/OLD.HEIC", b"old");
    let (plan, _) = do_scan(&mut src, &e);
    assert_eq!(removed_paths(&plan), ["DCIM/OLD.HEIC"]);
    assert!(plan.cautions.is_empty(), "{:?}", plan.cautions);
    let (r, _) = do_archive_unconfirmed(&plan, STAMP);
    assert_eq!(r.unwrap().moved, 1);
}

/// A device folder that lists empty while the backup holds files under it: those files wait for
/// a confirmation, the others are archived.
#[test]
fn files_under_a_folder_the_device_lists_as_empty_are_held_back() {
    let e = env();
    let mut objects: Vec<MemObject> = (0..6)
        .map(|i| MemObject::new(&format!("{i}"), &format!("DCIM/102APPLE/B{i}.HEIC"), b"b"))
        .collect();
    objects.push(MemObject::new("x", "DCIM/100APPLE/KEEP.HEIC", b"k"));
    let mut src = phone(objects);
    claim(&e, &key_of(&src));
    for i in 0..6 {
        put(&e.root, &format!("DCIM/102APPLE/B{i}.HEIC"), b"b");
    }
    put(&e.root, "DCIM/100APPLE/KEEP.HEIC", b"k");
    put(&e.root, "DCIM/100APPLE/GONE.HEIC", b"gone");
    put(&e.root, "DCIM/101APPLE/HELD.HEIC", b"held");
    src.empty_folders = vec!["DCIM/101APPLE".into()];
    let (plan, summary) = do_scan(&mut src, &e);
    assert_eq!(summary.removed, 2);
    assert_eq!(summary.held, 1);
    let held: Vec<&str> = plan
        .removed
        .iter()
        .filter(|r| r.held)
        .map(|r| r.path.as_str())
        .collect();
    assert_eq!(held, ["DCIM/101APPLE/HELD.HEIC"]);
    // Without the OK: the ordinary file goes, the held one stays.
    let (r, ev) = do_archive_unconfirmed(&plan, STAMP);
    let r = r.unwrap();
    assert_eq!((r.moved, r.held), (1, 1), "{ev:?}");
    assert!(e.root.join("DCIM/101APPLE/HELD.HEIC").exists());
    assert!(!e.root.join("DCIM/100APPLE/GONE.HEIC").exists());
    // With it, the rest follows.
    let (plan, _) = do_scan(&mut src, &e);
    let (r, _) = do_archive(&plan, STAMP);
    assert_eq!(r.unwrap().moved, 1);
    assert!(!e.root.join("DCIM/101APPLE/HELD.HEIC").exists());
}

/// A chosen folder the device lists no files under is treated the same way, without any help from
/// the source: an empty listing and a deleted folder look alike.
#[test]
fn files_under_a_chosen_folder_with_no_device_files_are_held_back() {
    let e = env();
    let mut settings = open_settings();
    settings.folders = vec!["DCIM".into(), "Pictures".into()];
    let mut src = phone(
        (0..5)
            .map(|i| MemObject::new(&format!("{i}"), &format!("DCIM/P{i}.JPG"), b"p"))
            .collect(),
    );
    claim(&e, &key_of(&src));
    for i in 0..5 {
        put(&e.root, &format!("DCIM/P{i}.JPG"), b"p");
    }
    put(&e.root, "Pictures/shot.png", b"s");
    let (plan, _) = do_scan_with(&mut src, &e, &opts_with(settings));
    assert_eq!(removed_paths(&plan), ["Pictures/shot.png"]);
    assert!(plan.removed[0].held);
    assert!(has_caution(&plan, |c| matches!(
        c,
        Caution::HeldBack { count: 1 }
    )));
}

/// The large-batch rule: at least 20 files and more than 1 in 20 of the backup.
#[test]
fn archiving_asks_first_for_a_large_batch() {
    for (gone, asks) in [(20usize, true), (19, false)] {
        let e = env();
        let total = 200;
        let mut src = phone(
            (gone..total)
                .map(|i| MemObject::new(&format!("{i}"), &format!("DCIM/F{i:03}.JPG"), b"x"))
                .collect(),
        );
        claim(&e, &key_of(&src));
        for i in 0..total {
            put(&e.root, &format!("DCIM/F{i:03}.JPG"), b"x");
        }
        let (plan, _) = do_scan(&mut src, &e);
        assert_eq!(plan.removed.len(), gone);
        assert_eq!(
            has_caution(&plan, |c| matches!(c, Caution::LargeBatch { .. })),
            asks,
            "{gone} of {total}: {:?}",
            plan.cautions
        );
        let (r, _) = do_archive_unconfirmed(&plan, STAMP);
        assert_eq!(r.is_err(), asks, "{gone} of {total}");
    }
}

/// Fewer than half of the backup's files on the device: ask first.
#[test]
fn archiving_asks_first_when_fewer_than_half_the_backup_is_on_the_device() {
    for (on_device, asks) in [(1usize, true), (2, false)] {
        let e = env();
        let mut src = phone(
            (0..on_device)
                .map(|i| MemObject::new(&format!("{i}"), &format!("DCIM/F{i}.JPG"), b"x"))
                .collect(),
        );
        claim(&e, &key_of(&src));
        for i in 0..4 {
            put(&e.root, &format!("DCIM/F{i}.JPG"), b"x");
        }
        let (plan, _) = do_scan(&mut src, &e);
        assert_eq!(
            has_caution(&plan, |c| matches!(c, Caution::FewOnDevice { .. })),
            asks,
            "{on_device} of 4: {:?}",
            plan.cautions
        );
        let (r, _) = do_archive_unconfirmed(&plan, STAMP);
        assert_eq!(r.is_err(), asks, "{on_device} of 4");
    }
}

/// A device matched only by weak signals: ask first. The worker passes the verdict in.
#[test]
fn archiving_asks_first_when_the_device_was_matched_only_by_weak_signals() {
    let e = env();
    let mut src = phone(vec![
        MemObject::new("1", "DCIM/A.JPG", b"a"),
        MemObject::new("2", "DCIM/B.JPG", b"b"),
    ]);
    claim(&e, &key_of(&src));
    put(&e.root, "DCIM/A.JPG", b"a");
    put(&e.root, "DCIM/B.JPG", b"b");
    put(&e.root, "DCIM/GONE.JPG", b"g");
    let (plan, _) = do_scan(&mut src, &e);
    assert!(plan.cautions.is_empty());
    let mut opts = open_opts();
    opts.weak_match = true;
    let (plan, _) = do_scan_with(&mut src, &e, &opts);
    assert!(has_caution(&plan, |c| matches!(c, Caution::WeakMatch)));
    let (r, _) = do_archive_unconfirmed(&plan, STAMP);
    assert!(matches!(r, Err(SyncError::NeedsConfirmation(_))), "{r:?}");
    assert!(e.root.join("DCIM/GONE.JPG").exists());
    assert_eq!(do_archive(&plan, STAMP).0.unwrap().moved, 1);
}

/// DESIGN.md 7.1: a backup file outside the selection never counts as deleted, and changing
/// the selection archives nothing.
#[test]
fn a_backup_file_outside_what_is_backed_up_is_never_deleted_or_archived() {
    let e = env();
    // DCIM, Pictures, Movies; photos and videos.
    let settings = DeviceSettings {
        archive_layout: ArchiveLayout::Flat,
        ..DeviceSettings::default()
    };
    let mut src = phone(vec![
        MemObject::new("1", "DCIM/a.jpg", b"a"),
        MemObject::new("2", "DCIM/b.txt", b"text"),
        MemObject::new("3", "Documents/z.jpg", b"z"),
        MemObject::new("4", "DCIM/c.JPG", b"c"),
        MemObject::new("5", "Movies/m.mp4", b"m"),
    ]);
    claim(&e, &key_of(&src));
    put(&e.root, "DCIM/a.jpg", b"a");
    put(&e.root, "DCIM/old.jpg", b"gone");
    put(&e.root, "DCIM/notes.txt", b"outside by type");
    put(&e.root, "Documents/x.jpg", b"outside by folder");
    let (plan, _) = do_scan_with(&mut src, &e, &opts_with(settings.clone()));
    // Only the chosen folders and file types are planned.
    let planned: Vec<&str> = plan.entries.iter().map(|x| x.source.as_str()).collect();
    assert_eq!(planned, ["DCIM/a.jpg", "DCIM/c.JPG", "Movies/m.mp4"]);
    assert_eq!(removed_paths(&plan), ["DCIM/old.jpg"]);
    assert_eq!(plan.outside, ["DCIM/notes.txt", "Documents/x.jpg"]);

    // Narrow the selection to another folder: everything that was backed up before stays.
    let mut narrow = settings.clone();
    narrow.folders = vec!["Movies".into()];
    let (plan, summary) = do_scan_with(&mut src, &e, &opts_with(narrow));
    assert!(plan.removed.is_empty(), "{:?}", removed_paths(&plan));
    assert_eq!(summary.outside, 3);
    assert_eq!(do_archive(&plan, STAMP).0.unwrap().moved, 0);
    assert_eq!(
        files_under(&e.root),
        set(&[
            "DCIM/a.jpg",
            "DCIM/old.jpg",
            "DCIM/notes.txt",
            "Documents/x.jpg"
        ])
    );
}

/// Always-left-out names are never backed up, and problems under them are dropped.
#[test]
fn left_out_names_are_not_backed_up_and_their_issues_are_dropped() {
    let e = env();
    let mut settings = open_settings();
    settings.always_left_out = vec!["System Volume Information".into(), "$RECYCLE.BIN".into()];
    let mut src = phone(vec![
        MemObject::new("1", "docs/a.txt", b"a"),
        MemObject::new("2", "System Volume Information/x.dat", b"x"),
        MemObject::new("3", "docs/$recycle.bin/y.txt", b"y"),
    ]);
    src.issues = vec![
        ScanIssue::new("System Volume Information/blocked", "access denied"),
        ScanIssue::new("docs/$RECYCLE.BIN/z", "access denied"),
        ScanIssue::new("docs/real-problem", "access denied"),
    ];
    let (plan, _) = do_scan_with(&mut src, &e, &opts_with(settings));
    assert_eq!(plan.entries.len(), 1);
    assert_eq!(
        plan.issues,
        [ScanIssue::new("docs/real-problem", "access denied")]
    );
}

/// DESIGN.md 6.4: PC clutter is never indexed, so it is never archived; the list is editable.
#[test]
fn pc_clutter_is_never_indexed_or_archived() {
    let e = env();
    let mut src = phone(vec![
        MemObject::new("1", "DCIM/A.JPG", b"a"),
        MemObject::new("2", "DCIM/Thumbs.db", b"device clutter"),
    ]);
    claim(&e, &key_of(&src));
    put(&e.root, "DCIM/A.JPG", b"a");
    put(&e.root, "desktop.ini", b"x");
    put(&e.root, "DCIM/Thumbs.db", b"y");
    put(&e.root, "Sub/.DS_Store", b"z");
    let (plan, summary) = do_scan(&mut src, &e);
    assert_eq!(summary.backup_files, 1);
    assert!(plan.removed.is_empty(), "{:?}", removed_paths(&plan));
    assert!(plan.outside.is_empty());
    // The device's own Thumbs.db is not backed up either.
    assert_eq!(plan.entries.len(), 1);
    assert_eq!(do_archive(&plan, STAMP).0.unwrap().moved, 0);
    assert!(e.root.join("desktop.ini").exists());

    // The list can be edited: with Thumbs.db off it, the file counts like any other.
    let mut settings = open_settings();
    settings.ignore_names = vec!["desktop.ini".into()];
    let (plan, summary) = do_scan_with(&mut src, &e, &opts_with(settings));
    assert_eq!(summary.backup_files, 3);
    assert_eq!(removed_paths(&plan), ["Sub/.DS_Store"]);
    assert_eq!(plan.entries.len(), 2);
}

/// DESIGN.md 7.3: a file that shrank to 0 bytes never replaces a non-empty copy.
#[test]
fn a_file_that_shrank_to_nothing_never_replaces_a_copy_that_has_content() {
    let e = env();
    put(&e.root, "DCIM/IMG.HEIC", b"precious");
    let mut src = phone(vec![MemObject::new("1", "DCIM/IMG.HEIC", b"")]);
    let (plan, summary) = do_scan(&mut src, &e);
    assert!(matches!(plan.entries[0].status, Status::Error(_)));
    assert_eq!(summary.errors, 1);
    assert!(
        plan.issues.iter().any(|i| i.problem.contains("0 bytes")),
        "{:?}",
        plan.issues
    );
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &[SyncItem {
            entry: 0,
            mode: CopyMode::NoReplace,
        }],
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().failed, 1, "{ev:?}");

    // Even a plan that calls it changed refuses at the copy.
    let mut forced = plan.clone();
    forced.entries[0].status = Status::Mismatch;
    forced.entries[0].existing_size = Some(8);
    let (r, ev) = do_sync(
        &mut src,
        &forced,
        &e,
        &[SyncItem {
            entry: 0,
            mode: CopyMode::NoReplace,
        }],
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().failed, 1, "{ev:?}");
    assert!(failures(&ev)[0].contains("0 bytes"), "{ev:?}");
    assert_eq!(fs::read(e.root.join("DCIM/IMG.HEIC")).unwrap(), b"precious");
    assert_eq!(files_under(&e.root), set(&["DCIM/IMG.HEIC"]));
}

/// DESIGN.md 7.5: where the device reports a modified time, a file that changed while it was
/// read is not saved.
#[test]
fn a_file_that_changed_while_copying_is_not_saved() {
    let e = env();
    let mut obj = MemObject::new("1", "DCIM/IMG.MOV", &[5u8; 50_000]);
    obj.modified = Some(at(0));
    obj.modified_after_read = Some(at(30));
    let mut src = phone(vec![obj]);
    let (plan, _) = do_scan(&mut src, &e);
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().failed, 1, "{ev:?}");
    assert!(failures(&ev)[0].contains("changed while copying"), "{ev:?}");
    assert!(
        files_under(&e.root).is_empty(),
        "{:?}",
        files_under(&e.root)
    );

    // The same for a changed file: its old copy stays where it is and nothing is archived.
    put(&e.root, "DCIM/IMG.MOV", b"old copy");
    let mut obj = MemObject::new("1", "DCIM/IMG.MOV", &[5u8; 50_000]);
    obj.modified = Some(at(0));
    obj.modified_after_read = Some(at(30));
    let mut src = phone(vec![obj]);
    let (plan, _) = do_scan(&mut src, &e);
    assert_eq!(plan.entries[0].status, Status::Mismatch);
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().failed, 1, "{ev:?}");
    assert_eq!(fs::read(e.root.join("DCIM/IMG.MOV")).unwrap(), b"old copy");
    assert_eq!(files_under(&e.root), set(&["DCIM/IMG.MOV"]));
}

/// A device that reports no modified time is not re-checked: the size check is all there is.
#[test]
fn without_a_reported_modified_time_the_file_is_saved_as_before() {
    let e = env();
    let mut src = phone(vec![MemObject::new("1", "DCIM/IMG.MOV", b"abc")]);
    let (plan, _) = do_scan(&mut src, &e);
    let (r, _) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().copied, 1);
}

/// DESIGN.md 7.4: the copy gets the device's modified time.
#[test]
fn a_copy_gets_the_modified_date_the_device_reports() {
    let e = env();
    let mut obj = MemObject::new("1", "DCIM/IMG.HEIC", b"abc");
    obj.modified = Some(at(1234));
    let mut src = phone(vec![obj]);
    let (plan, _) = do_scan(&mut src, &e);
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().copied, 1, "{:?}", failures(&ev));
    let m = fs::metadata(e.root.join("DCIM/IMG.HEIC"))
        .unwrap()
        .modified()
        .unwrap();
    assert!(
        crate::plan::same_modified(m, at(1234)),
        "the copy was dated {m:?}"
    );
    // And a changed file's new copy is dated the same way.
    src.object_mut("1").modified = Some(at(9999));
    src.object_mut("1").data = b"abcd".to_vec();
    src.object_mut("1").reported_size = Some(4);
    let (plan, _) = do_scan(&mut src, &e);
    assert_eq!(plan.entries[0].status, Status::Mismatch);
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().replaced, 1, "{:?}", failures(&ev));
    let m = fs::metadata(e.root.join("DCIM/IMG.HEIC"))
        .unwrap()
        .modified()
        .unwrap();
    assert!(crate::plan::same_modified(m, at(9999)), "{m:?}");
}

/// DESIGN.md 7.2: on storage, the modified date counts too, with the two allowances; on phones
/// it doesn't.
#[test]
fn storage_files_are_compared_by_date_and_phone_files_are_not() {
    let e = env();
    put(&e.root, "a.txt", b"abc");
    set_mtime(&e.root.join("a.txt"), at(0));
    let mut storage = open_settings();
    storage.compare_by_date = true;
    let phone_settings = open_settings();
    assert!(!phone_settings.compare_by_date);
    let mut obj = MemObject::new("1", "a.txt", b"abc");
    for (shift, same) in [(0, true), (2, true), (3, false), (3600, true), (60, false)] {
        obj.modified = Some(at(shift));
        let mut src = phone(vec![obj.clone()]);
        let (plan, _) = do_scan_with(&mut src, &e, &opts_with(storage.clone()));
        let want = if same {
            Status::Verified
        } else {
            Status::Mismatch
        };
        assert_eq!(plan.entries[0].status, want, "{shift} s apart");
        // A phone doesn't compare dates.
        let (plan, _) = do_scan_with(&mut src, &e, &opts_with(phone_settings.clone()));
        assert_eq!(plan.entries[0].status, Status::Verified, "{shift} s, phone");
    }
}

/// DESIGN.md 6.1: one storage stays at the top, several get a folder each, and the choice is made
/// once; a storage that turns up later gets its own folder and nothing moves.
#[test]
fn storages_are_laid_out_by_the_choice_made_at_setup() {
    let on = |id: &str, path: &str, storage: &str| {
        let mut o = MemObject::new(id, path, b"x");
        o.storage = Some(storage.into());
        o
    };
    // One storage at setup: flat, and the choice is reported for storing.
    let e = env();
    let mut src = phone(vec![on("1", "DCIM/a.jpg", "Internal")]);
    let (plan, _) = do_scan(&mut src, &e);
    assert_eq!(plan.entries[0].source.as_str(), "DCIM/a.jpg");
    let choice = plan.storage_choice.clone().unwrap();
    assert_eq!(choice.layout, StorageLayout::TopLevel);
    assert_eq!(choice.top_level_storage.as_deref(), Some("Internal"));

    // With the choice stored, an SD card that turns up later gets its own folder.
    let mut settings = open_settings();
    settings.storage_layout = choice.layout;
    settings.top_level_storage = choice.top_level_storage;
    let mut src = phone(vec![
        on("1", "DCIM/a.jpg", "Internal"),
        on("2", "DCIM/b.jpg", "SD card"),
    ]);
    let (plan, _) = do_scan_with(&mut src, &e, &opts_with(settings.clone()));
    assert!(plan.storage_choice.is_none(), "already chosen");
    assert_eq!(plan.storage_folders, ["SD card"]);
    // What the worker stores after the scan.
    settings.storage_folders = plan.storage_folders.clone();
    let planned: Vec<&str> = plan.entries.iter().map(|x| x.source.as_str()).collect();
    assert_eq!(planned, ["DCIM/a.jpg", "SD card/DCIM/b.jpg"]);
    claim(&e, &key_of(&src));
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().copied, 2, "{:?}", failures(&ev));
    // Ejecting the card never makes its backup look deleted.
    let mut src = phone(vec![on("1", "DCIM/a.jpg", "Internal")]);
    let (plan, summary) = do_scan_with(&mut src, &e, &opts_with(settings));
    assert!(plan.removed.is_empty(), "{:?}", removed_paths(&plan));
    assert_eq!(plan.outside, ["SD card/DCIM/b.jpg"]);
    assert_eq!(summary.verified, 1);

    // Two storages at setup: a folder each.
    let e = env();
    let mut src = phone(vec![
        on("1", "DCIM/a.jpg", "Internal"),
        on("2", "DCIM/b.jpg", "SD card"),
    ]);
    let (plan, _) = do_scan(&mut src, &e);
    assert_eq!(
        plan.storage_choice.as_ref().unwrap().layout,
        StorageLayout::PerStorage
    );
    let planned: Vec<&str> = plan.entries.iter().map(|x| x.source.as_str()).collect();
    assert_eq!(planned, ["Internal/DCIM/a.jpg", "SD card/DCIM/b.jpg"]);
}

/// DESIGN.md 6.3: the archive's name comes from the settings, and the marker lists every name
/// used.
#[test]
fn the_archive_name_comes_from_the_settings_and_the_marker_lists_every_name() {
    let e = env();
    let mut settings = open_settings();
    settings.archive_name = "_Deleted from phone".into();
    let mut src = phone(vec![MemObject::new("1", "DCIM/A.JPG", b"a")]);
    put(&e.root, "DCIM/A.JPG", b"old");
    let (plan, _) = do_scan_with(&mut src, &e, &opts_with(settings.clone()));
    assert_eq!(plan.archive_name, "_Deleted from phone");
    let (r, ev) = do_sync_as(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
        true,
    );
    assert_eq!(r.unwrap().replaced, 1, "{:?}", failures(&ev));
    let old = e.root.join(format!("_Deleted from phone/{STAMP}/A.JPG"));
    assert_eq!(fs::read(old).unwrap(), b"old");
    assert!(!e.root.join(ARCHIVE_DIR).exists());
    let OwnerState::Owned(o) = owner::read(&e.root) else {
        panic!("no marker")
    };
    assert_eq!(o.archive_names, ["_Deleted from phone"]);
    // The archive is not backup content.
    let (plan, summary) = do_scan_with(&mut src, &e, &opts_with(settings.clone()));
    assert_eq!((summary.backup_files, summary.removed), (1, 0));
    assert!(plan.issues.is_empty());

    // A new name later: the marker keeps the old one, and both stay out of the scan.
    settings.archive_name = "Gone".into();
    put(&e.root, "DCIM/GONE.JPG", b"gone");
    let (plan, _) = do_scan_with(&mut src, &e, &opts_with(settings.clone()));
    assert_eq!(removed_paths(&plan), ["DCIM/GONE.JPG"]);
    let (r, _) = do_archive(&plan, "2026-02-02 02-02-02");
    assert_eq!(r.unwrap().moved, 1);
    let OwnerState::Owned(o) = owner::read(&e.root) else {
        panic!("no marker")
    };
    assert_eq!(o.archive_names, ["_Deleted from phone", "Gone"]);
    let (plan, summary) = do_scan_with(&mut src, &e, &opts_with(settings));
    assert_eq!((summary.backup_files, summary.removed), (1, 0));
    assert!(plan.removed.is_empty());
}

/// The name is one safe name: the rules of `SafeRelPath`, not the marker's, not the temp suffix.
#[test]
fn an_unsafe_archive_name_is_refused_by_the_scan() {
    let e = env();
    for name in [
        "",
        ".",
        "..",
        "a/b",
        "a\\b",
        "bad:name",
        "CON",
        "trailing.",
        "trailing ",
        "x.pnk-part",
        ".plug-n-keep.json",
        ".plug-n-keep.json.tmp",
        &"n".repeat(256),
    ] {
        let mut settings = open_settings();
        settings.archive_name = name.to_string();
        let mut src = phone(vec![MemObject::new("1", "DCIM/A.JPG", b"a")]);
        let r = scan(
            &mut src,
            &e.root,
            &Default::default(),
            &opts_with(settings),
            &AtomicBool::new(false),
            &mut |_| {},
        );
        assert!(r.is_err(), "{name:?} was accepted");
    }
}

/// A name the device uses at the top of the backup is reported.
#[test]
fn a_device_folder_with_the_archive_name_is_reported() {
    let e = env();
    let mut settings = open_settings();
    settings.archive_name = "Photos".into();
    let mut src = phone(vec![
        MemObject::new("1", "photos/a.jpg", b"a"),
        MemObject::new("2", "DCIM/b.jpg", b"b"),
    ]);
    let (plan, _) = do_scan_with(&mut src, &e, &opts_with(settings));
    assert!(plan.archive_clash.is_some());
    assert!(
        plan.issues.iter().any(|i| i.location == "(archive folder)"),
        "{:?}",
        plan.issues
    );
    // Its files are not copied into the archive's own folder.
    let planned: Vec<&str> = plan.entries.iter().map(|x| x.source.as_str()).collect();
    assert_eq!(planned, ["DCIM/b.jpg"]);
    let (plan, _) = do_scan(&mut src, &e);
    assert!(plan.archive_clash.is_none());
}

/// DESIGN.md 6.3: keep folders is the default; flat numbers clashing names.
#[test]
fn the_archive_keeps_the_devices_folders_by_default() {
    let e = env();
    let mut settings = DeviceSettings::default();
    assert_eq!(settings.archive_layout, ArchiveLayout::KeepFolders);
    settings.folders = Vec::new();
    settings.file_types = FileTypes::All;
    let mut src = phone(vec![MemObject::new("1", "DCIM/100APPLE/KEEP.HEIC", b"k")]);
    claim(&e, &key_of(&src));
    put(&e.root, "DCIM/100APPLE/KEEP.HEIC", b"k");
    put(&e.root, "DCIM/100APPLE/IMG_1.HEIC", b"one");
    put(&e.root, "DCIM/101APPLE/IMG_1.HEIC", b"two");
    let (plan, _) = do_scan_with(&mut src, &e, &opts_with(settings));
    let (r, ev) = do_archive(&plan, STAMP);
    assert_eq!(r.unwrap().moved, 2, "{ev:?}");
    let dir = e.root.join(ARCHIVE_DIR).join(STAMP);
    assert_eq!(
        fs::read(dir.join("DCIM/100APPLE/IMG_1.HEIC")).unwrap(),
        b"one"
    );
    assert_eq!(
        fs::read(dir.join("DCIM/101APPLE/IMG_1.HEIC")).unwrap(),
        b"two"
    );
}

/// A changed file's old copy goes into the archive in the device's folder layout too.
#[test]
fn a_changed_files_old_copy_keeps_its_folders_in_the_archive() {
    let e = env();
    let mut settings = open_settings();
    settings.archive_layout = ArchiveLayout::KeepFolders;
    put(&e.root, "Docs/Sub/a.txt", b"old");
    let mut src = phone(vec![MemObject::new("1", "Docs/Sub/a.txt", b"newer")]);
    let (plan, _) = do_scan_with(&mut src, &e, &opts_with(settings));
    let (r, ev) = do_sync_as(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
        true,
    );
    assert_eq!(r.unwrap().replaced, 1, "{:?}", failures(&ev));
    assert_eq!(
        fs::read(e.root.join(format!("{ARCHIVE_DIR}/{STAMP}/Docs/Sub/a.txt"))).unwrap(),
        b"old"
    );
    assert_eq!(fs::read(e.root.join("Docs/Sub/a.txt")).unwrap(), b"newer");
}

/// Renaming the archive folder: one move, the old name kept in the marker, nothing archived
/// twice.
#[test]
fn renaming_the_archive_moves_the_folder_and_keeps_the_old_name_in_the_marker() {
    let e = env();
    let mut src = phone(vec![MemObject::new("1", "DCIM/A.JPG", b"a")]);
    claim(&e, &key_of(&src));
    put(&e.root, "DCIM/A.JPG", b"a");
    put(
        &e.root,
        &format!("{ARCHIVE_DIR}/2025-01-01 00-00-00/X.JPG"),
        b"x",
    );
    let (plan, _) = do_scan(&mut src, &e);
    let r = rename_archive(&plan, "Deleted from phone").unwrap();
    assert!(r.moved);
    assert_eq!(
        fs::read(e.root.join("Deleted from phone/2025-01-01 00-00-00/X.JPG")).unwrap(),
        b"x"
    );
    assert!(!e.root.join(ARCHIVE_DIR).exists());
    let OwnerState::Owned(o) = owner::read(&e.root) else {
        panic!("no marker")
    };
    assert_eq!(o.archive_names, [ARCHIVE_DIR, "Deleted from phone"]);

    // With the new name in the settings, the archive is still not backup content.
    let mut settings = open_settings();
    settings.archive_name = "Deleted from phone".into();
    let (plan, summary) = do_scan_with(&mut src, &e, &opts_with(settings));
    assert_eq!((summary.backup_files, summary.removed), (1, 0));
    assert!(plan.issues.is_empty());
    assert_eq!(do_archive(&plan, STAMP).0.unwrap().moved, 0);
}

#[test]
fn renaming_the_archive_is_refused_when_it_would_be_unsafe() {
    let e = env();
    let mut src = phone(vec![
        MemObject::new("1", "DCIM/A.JPG", b"a"),
        MemObject::new("2", "Taken/b.jpg", b"b"),
    ]);
    claim(&e, &key_of(&src));
    put(&e.root, "DCIM/A.JPG", b"a");
    put(
        &e.root,
        &format!("{ARCHIVE_DIR}/2025-01-01 00-00-00/X.JPG"),
        b"x",
    );
    put(&e.root, "Occupied/file.txt", b"o");
    let (plan, _) = do_scan(&mut src, &e);
    // Not a safe name; the same name; a name the device uses; a folder already there.
    for name in [
        "",
        "a/b",
        "CON",
        ".plug-n-keep.json",
        "x.pnk-part",
        "_archive",
        "taken",
        "Occupied",
    ] {
        let r = rename_archive(&plan, name);
        assert!(r.is_err(), "{name:?}: {r:?}");
    }
    assert!(
        e.root
            .join(format!("{ARCHIVE_DIR}/2025-01-01 00-00-00/X.JPG"))
            .exists()
    );
    assert_eq!(files_under(&e.root).len(), 3);

    // A folder that belongs to another device is not touched.
    let mut other = MemSource::new("OTHER", vec![MemObject::new("1", "DCIM/A.JPG", b"a")]);
    let (plan, _) = do_scan(&mut other, &e);
    let r = rename_archive(&plan, "Fine");
    assert!(matches!(r, Err(SyncError::WrongFolder(_))), "{r:?}");
    assert!(e.root.join(ARCHIVE_DIR).exists());

    // A link in the old name's place is not renamed.
    let e = env();
    let mut src = phone(vec![MemObject::new("1", "DCIM/A.JPG", b"a")]);
    claim(&e, &key_of(&src));
    let outside = e.base.join("outside");
    fs::create_dir_all(&outside).unwrap();
    make_junction(&e.root.join(ARCHIVE_DIR), &outside);
    let (plan, _) = do_scan(&mut src, &e);
    let r = rename_archive(&plan, "Fine");
    assert!(r.is_err(), "{r:?}");
    assert!(outside.exists() && !e.root.join("Fine").exists());
}

/// The backup folders the app holds for any device: the test's one.
fn known(e: &Env) -> impl Fn(&str) -> Vec<PathBuf> + use<> {
    let root = e.root.clone();
    move |_| vec![root.clone()]
}

fn crash_state(
    e: &Env,
    original: Option<&[u8]>,
    archived: Option<&[u8]>,
    temp: Option<&[u8]>,
    with_step: bool,
) -> (PathBuf, PathBuf, PathBuf) {
    let o = e.root.join("DCIM/IMG.HEIC");
    let a = e.root.join(format!("{ARCHIVE_DIR}/{STAMP}/IMG.HEIC"));
    let t = e.root.join(format!("DCIM/IMG.HEIC{TEMP_SUFFIX}"));
    fs::create_dir_all(o.parent().unwrap()).unwrap();
    fs::create_dir_all(a.parent().unwrap()).unwrap();
    owner::write(
        &fs::canonicalize(&e.root).unwrap(),
        &owner::FolderOwner::new(
            "serial:PHONE-A",
            "phone",
            Signals::default(),
            View::WindowsWpd,
            "2026-01-01T00:00:00Z",
        ),
    )
    .unwrap();
    for (p, data) in [(&o, original), (&a, archived), (&t, temp)] {
        if let Some(d) = data {
            fs::write(p, d).unwrap();
        }
    }
    e.state
        .begin_copy(&crate::state::InProgress {
            device: "serial:PHONE-A".into(),
            root: e.root.clone(),
            source: "DCIM/IMG.HEIC".into(),
            temp: t.clone(),
            replace: with_step.then(|| crate::state::ReplaceStep {
                original: o.clone(),
                archived: a.clone(),
                goes: crate::state::OldCopyGoes::Archive,
                staged_size: None,
            }),
            destination: None,
        })
        .unwrap();
    (o, a, t)
}

/// DESIGN.md 7.3: a crash at any point of replacing a changed file is finished by the next
/// start, and both copies always exist.
#[test]
fn recovery_finishes_a_replacement_interrupted_at_any_step() {
    // Before the first move: the old copy in place, the new one whole in the temporary file.
    let e = env();
    let (o, a, t) = crash_state(&e, Some(b"old"), None, Some(b"new"), true);
    let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
    assert!(got.completed, "{}", got.note);
    assert_eq!(fs::read(&o).unwrap(), b"new");
    assert_eq!(fs::read(&a).unwrap(), b"old");
    assert!(!t.exists());

    // After the first move: the old copy archived, the new one still in the temporary file.
    let e = env();
    let (o, a, t) = crash_state(&e, None, Some(b"old"), Some(b"new"), true);
    let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
    assert!(got.completed, "{}", got.note);
    assert_eq!(fs::read(&o).unwrap(), b"new");
    assert_eq!(fs::read(&a).unwrap(), b"old");
    assert!(!t.exists());

    // After both moves, before the record was dropped: nothing left to do.
    let e = env();
    let (o, a, _) = crash_state(&e, Some(b"new"), Some(b"old"), None, true);
    let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
    assert!(got.completed, "{}", got.note);
    assert_eq!(fs::read(&o).unwrap(), b"new");
    assert_eq!(fs::read(&a).unwrap(), b"old");

    // Before the new copy was whole (no replacement step yet): the half copy goes, the old
    // copy is untouched.
    let e = env();
    let (o, a, t) = crash_state(&e, Some(b"old"), None, Some(b"ne"), false);
    let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
    assert!(got.temp_removed && !got.completed, "{}", got.note);
    assert_eq!(fs::read(&o).unwrap(), b"old");
    assert!(!a.exists() && !t.exists());

    // Something unexpected: every file is left, nothing is deleted or overwritten.
    let e = env();
    let (o, a, t) = crash_state(&e, Some(b"x"), Some(b"y"), Some(b"z"), true);
    let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
    assert!(!got.completed && !got.temp_removed, "{}", got.note);
    assert_eq!(fs::read(&o).unwrap(), b"x");
    assert_eq!(fs::read(&a).unwrap(), b"y");
    assert_eq!(fs::read(&t).unwrap(), b"z");
}

/// A record that points outside the backup folder is not acted on.
#[test]
fn recovery_does_not_move_files_outside_the_backup_folder() {
    let e = env();
    let (o, _, t) = crash_state(&e, Some(b"old"), None, Some(b"new"), true);
    // Rewrite the record so the archive path leads outside the backup folder.
    let outside = e.base.join("outside");
    fs::create_dir_all(&outside).unwrap();
    e.state
        .begin_copy(&crate::state::InProgress {
            device: "serial:PHONE-A".into(),
            root: e.root.clone(),
            source: "DCIM/IMG.HEIC".into(),
            temp: t.clone(),
            replace: Some(crate::state::ReplaceStep {
                original: o.clone(),
                archived: outside.join("IMG.HEIC"),
                goes: crate::state::OldCopyGoes::Archive,
                staged_size: None,
            }),
            destination: None,
        })
        .unwrap();
    let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
    assert!(!got.completed, "{}", got.note);
    assert_eq!(fs::read(&o).unwrap(), b"old");
    assert_eq!(fs::read(&t).unwrap(), b"new");
    assert!(!outside.join("IMG.HEIC").exists());
}

fn record(e: &Env, root: &Path, temp: &Path, step: Option<(&Path, &Path)>) {
    e.state
        .begin_copy(&crate::state::InProgress {
            device: "serial:PHONE-A".into(),
            root: root.to_path_buf(),
            source: "DCIM/IMG.HEIC".into(),
            temp: temp.to_path_buf(),
            replace: step.map(|(o, a)| crate::state::ReplaceStep {
                original: o.to_path_buf(),
                archived: a.to_path_buf(),
                goes: crate::state::OldCopyGoes::Archive,
                staged_size: None,
            }),
            destination: None,
        })
        .unwrap();
}

/// A forged crash record can't be used to move or delete files: its backup folder must be one
/// the app holds for the device, and its files must fit that folder.
#[test]
fn recovery_does_not_trust_the_records_own_root() {
    // A record whose root is a big folder the app doesn't know (here, the folder holding the
    // backup and the app's data): neither the delete nor the replacement is done.
    let e = env();
    let (o, a, t) = crash_state(&e, Some(b"old"), None, Some(b"new"), true);
    let forged_root = e.base.clone();
    record(&e, &forged_root, &t, Some((&o, &a)));
    let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
    assert!(!got.completed && !got.temp_removed, "{}", got.note);
    assert_eq!(fs::read(&o).unwrap(), b"old");
    assert_eq!(fs::read(&t).unwrap(), b"new");
    assert!(!a.exists());

    let e = env();
    let (_, _, t) = crash_state(&e, Some(b"old"), None, Some(b"half"), false);
    record(&e, &e.base.clone(), &t, None);
    let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
    assert!(!got.temp_removed, "{}", got.note);
    assert!(t.exists());

    // The right root, but a temporary file that isn't the original's name plus the suffix.
    let e = env();
    let (o, a, t) = crash_state(&e, Some(b"old"), None, Some(b"new"), true);
    let other = e.root.join(format!("DCIM/OTHER.HEIC{TEMP_SUFFIX}"));
    fs::rename(&t, &other).unwrap();
    record(&e, &e.root.clone(), &other, Some((&o, &a)));
    let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
    assert!(!got.completed, "{}", got.note);
    assert_eq!(fs::read(&o).unwrap(), b"old");
    assert!(other.exists() && !a.exists());

    // The right root, but an "archive" that is not under one of the marker's archive names.
    let e = env();
    let (o, _, t) = crash_state(&e, Some(b"old"), None, Some(b"new"), true);
    let not_archive = e.root.join("DCIM/IMG.HEIC.old");
    record(&e, &e.root.clone(), &t, Some((&o, &not_archive)));
    let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
    assert!(!got.completed, "{}", got.note);
    assert_eq!(fs::read(&o).unwrap(), b"old");
    assert!(!not_archive.exists());

    // A folder whose marker belongs to another device.
    let e = env();
    let (o, a, _) = crash_state(&e, Some(b"old"), None, Some(b"new"), true);
    owner::write(
        &fs::canonicalize(&e.root).unwrap(),
        &owner::FolderOwner::new(
            "serial:OTHER",
            "other",
            Signals::default(),
            View::WindowsWpd,
            "2026-01-01T00:00:00Z",
        ),
    )
    .unwrap();
    let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
    assert!(!got.completed, "{}", got.note);
    assert_eq!(fs::read(&o).unwrap(), b"old");
    assert!(!a.exists());
}

/// A crash while a failed replacement was being rolled back (the old copy is back in place, the
/// new one is still whole in the temporary file), and a record that is out of date (nothing it
/// names is there): neither loses a file.
#[test]
fn recovery_copes_with_a_crash_during_rollback_and_a_stale_record() {
    let e = env();
    let (o, a, t) = crash_state(&e, Some(b"old"), None, Some(b"new"), true);
    // Exactly what a rollback leaves: both copies exist, the old one where it began.
    let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
    assert!(got.completed, "{}", got.note);
    assert_eq!(fs::read(&o).unwrap(), b"new");
    assert_eq!(fs::read(&a).unwrap(), b"old");
    assert!(!t.exists());

    // Stale: everything it names is gone. Nothing happens, and the record is dropped.
    let e = env();
    let (o, a, t) = crash_state(&e, None, None, None, true);
    let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
    assert!(!got.completed && !got.temp_removed, "{}", got.note);
    assert!(!o.exists() && !a.exists() && !t.exists());
    assert!(e.state.recover_interrupted(&known(&e)).unwrap().is_none());
}

// ---------------------------------------------------------------------------------------------
// Changed files wait for the same OK as archiving, and other hardening

/// DESIGN.md 7.5: a changed file's old copy goes into the archive, so on a first sync into a
/// filled folder with no marker, and under a weak match, it asks first like any archiving.
#[test]
fn a_changed_file_waits_for_the_ok_when_archiving_would_ask() {
    // The first sync into a folder that already has files and no marker.
    let e = env();
    put(&e.root, "DCIM/IMG.HEIC", b"old");
    let mut src = phone(vec![MemObject::new("1", "DCIM/IMG.HEIC", b"newer")]);
    let (plan, _) = do_scan(&mut src, &e);
    assert_eq!(plan.entries[0].status, Status::Mismatch);
    assert!(
        plan.cautions
            .iter()
            .any(|c| matches!(c, Caution::FirstSyncIntoFilledFolder { .. })),
        "{:?}",
        plan.cautions
    );
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    let r = r.unwrap();
    assert_eq!((r.copied, r.replaced, r.failed), (0, 0, 1), "{ev:?}");
    assert!(failures(&ev)[0].contains("needs confirmation"), "{ev:?}");
    assert_eq!(fs::read(e.root.join("DCIM/IMG.HEIC")).unwrap(), b"old");
    assert!(!e.root.join(ARCHIVE_DIR).exists(), "nothing was archived");
    assert_eq!(files_under(&e.root), set(&["DCIM/IMG.HEIC"]));
    // With the OK, the change is made.
    let (r, ev) = do_sync_as(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
        true,
    );
    assert_eq!(r.unwrap().replaced, 1, "{:?}", failures(&ev));
    assert_eq!(fs::read(e.root.join("DCIM/IMG.HEIC")).unwrap(), b"newer");

    // A device recognised only by weak signals, in a folder that is its own.
    let e = env();
    let mut src = phone(vec![MemObject::new("1", "DCIM/IMG.HEIC", b"newer")]);
    claim(&e, &key_of(&src));
    put(&e.root, "DCIM/IMG.HEIC", b"old");
    let weak = ScanOptions {
        weak_match: true,
        ..open_opts()
    };
    let (plan, _) = do_scan_with(&mut src, &e, &weak);
    assert!(
        plan.cautions.contains(&Caution::WeakMatch),
        "{:?}",
        plan.cautions
    );
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().failed, 1, "{ev:?}");
    assert!(failures(&ev)[0].contains("needs confirmation"), "{ev:?}");
    assert_eq!(fs::read(e.root.join("DCIM/IMG.HEIC")).unwrap(), b"old");
    assert!(!e.root.join(ARCHIVE_DIR).exists());

    // A new file is not held back by either, and with no reason to ask a change goes through.
    let e = env();
    let mut src = phone(vec![MemObject::new("1", "DCIM/NEW.HEIC", b"new")]);
    put(&e.root, "DCIM/OTHER.HEIC", b"other");
    let (plan, _) = do_scan(&mut src, &e);
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().copied, 1, "{ev:?}");
}

/// DESIGN.md 7.5: the large-batch question is for at least 20 files and more than 1 in 20 of
/// the backup. 20 of 400 is exactly 1 in 20, so it doesn't ask; 21 of 400 does.
#[test]
fn the_large_batch_question_starts_above_one_in_twenty() {
    for (gone, asks) in [(20usize, false), (21, true)] {
        let e = env();
        let mut src = phone(
            (gone..400)
                .map(|i| MemObject::new(&i.to_string(), &format!("DCIM/IMG_{i:04}.JPG"), b"x"))
                .collect(),
        );
        claim(&e, &key_of(&src));
        for i in 0..400 {
            put(&e.root, &format!("DCIM/IMG_{i:04}.JPG"), b"x");
        }
        let (plan, _) = do_scan(&mut src, &e);
        assert_eq!(plan.removed.len(), gone);
        let large = plan
            .cautions
            .iter()
            .any(|c| matches!(c, Caution::LargeBatch { .. }));
        assert_eq!(large, asks, "{gone} of 400: {:?}", plan.cautions);
        let (r, _) = do_archive_unconfirmed(&plan, STAMP);
        assert_eq!(r.is_err(), asks, "{gone} of 400");
    }
}

/// DESIGN.md 7.5: "Leave them" stays left. A sync that copies but doesn't archive the files that
/// were already in the folder doesn't make the next scan forget to ask about them.
#[test]
fn leaving_the_files_already_in_a_folder_stays_left() {
    let e = env();
    put(&e.root, "Other/keep.txt", b"not from the device");
    let mut src = phone(
        (0..30)
            .map(|i| MemObject::new(&i.to_string(), &format!("DCIM/IMG_{i:04}.JPG"), b"x"))
            .collect(),
    );
    let (plan, _) = do_scan(&mut src, &e);
    assert!(
        plan.cautions
            .iter()
            .any(|c| matches!(c, Caution::FirstSyncIntoFilledFolder { .. }))
    );
    // Copy only (the user chose to leave them).
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().copied, 30, "{:?}", failures(&ev));
    // The folder has a marker now, but the one foreign file (far under 1 in 20) still asks.
    let (plan, _) = do_scan(&mut src, &e);
    assert_eq!(removed_paths(&plan), ["Other/keep.txt"]);
    assert!(
        plan.cautions
            .iter()
            .any(|c| matches!(c, Caution::FirstSyncIntoFilledFolder { .. })),
        "{:?}",
        plan.cautions
    );
    let (r, _) = do_archive_unconfirmed(&plan, STAMP);
    assert!(matches!(r, Err(SyncError::NeedsConfirmation(_))), "{r:?}");
    assert!(e.root.join("Other/keep.txt").exists());
    // Once an archive has been confirmed, a later stray file no longer asks.
    let (r, _) = do_archive(&plan, STAMP);
    assert_eq!(r.unwrap().moved, 1);
    put(&e.root, "Other/later.txt", b"x");
    let (plan, _) = do_scan(&mut src, &e);
    assert_eq!(removed_paths(&plan), ["Other/later.txt"]);
    assert!(plan.cautions.is_empty(), "{:?}", plan.cautions);
    let (r, _) = do_archive_unconfirmed(&plan, "2026-01-02 03-04-06");
    assert_eq!(r.unwrap().moved, 1);
}

/// DESIGN.md 7.5: the pre-copy re-check also compares the modified date, and the copy is
/// stamped with the date read just before copying.
#[test]
fn a_file_whose_date_changed_since_the_scan_is_not_copied_and_the_copy_gets_the_fresh_date() {
    let e = env();
    let mut obj = MemObject::new("1", "Docs/a.txt", b"same size");
    obj.modified = Some(at(0));
    let mut src = phone(vec![obj]);
    claim(&e, &key_of(&src));
    let (plan, _) = do_scan(&mut src, &e);
    // Edited on the device after the scan, at the same size.
    src.object_mut("1").modified = Some(at(100));
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().failed, 1, "{ev:?}");
    assert!(failures(&ev)[0].contains("modified date"), "{ev:?}");
    assert!(!e.root.join("Docs/a.txt").exists());

    // No date at the scan but one now: the copy gets the date read just before copying.
    let e = env();
    let mut src = phone(vec![MemObject::new("1", "Docs/a.txt", b"same size")]);
    claim(&e, &key_of(&src));
    let (plan, _) = do_scan(&mut src, &e);
    src.object_mut("1").modified = Some(at(50));
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().copied, 1, "{ev:?}");
    let got = fs::metadata(e.root.join("Docs/a.txt"))
        .unwrap()
        .modified()
        .unwrap();
    assert!(crate::plan::same_modified(got, at(50)), "{got:?}");
}

/// DESIGN.md section 2 (promises 2 and 3): a finished temporary file left by a failed final move is moved aside by the
/// next sync, under a free numbered name, and never deleted.
#[test]
fn successive_leftover_temporary_files_are_each_kept() {
    let e = env();
    put(&e.root, &format!("DCIM/IMG.HEIC{TEMP_SUFFIX}"), b"first");
    put(
        &e.root,
        &format!("DCIM/IMG.HEIC (kept 1){TEMP_SUFFIX}"),
        b"earlier",
    );
    let mut src = phone(vec![MemObject::new("1", "DCIM/IMG.HEIC", b"whole")]);
    let (plan, _) = do_scan(&mut src, &e);
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().copied, 1, "{ev:?}");
    let read = |name: &str| fs::read(e.root.join(name)).unwrap();
    assert_eq!(
        read(&format!("DCIM/IMG.HEIC (kept 1){TEMP_SUFFIX}")),
        b"earlier"
    );
    assert_eq!(
        read(&format!("DCIM/IMG.HEIC (kept 2){TEMP_SUFFIX}")),
        b"first"
    );
    assert_eq!(read("DCIM/IMG.HEIC"), b"whole");
}

/// A backup folder written with `..` in it can't be used to hide that it lies inside the folder
/// the device is read from.
#[test]
fn a_backup_folder_with_dot_dot_is_refused() {
    let e = env();
    let stick = e.base.join("stick");
    fs::create_dir_all(&stick).unwrap();
    let mut src = FolderSource::new(&stick).unwrap();
    // Looks like a folder elsewhere; it is really inside the stick.
    // (Built as text: joining ".." onto a Windows verbatim path would fold it away.)
    let sep = std::path::MAIN_SEPARATOR;
    let sneaky = PathBuf::from(format!(
        "{}{sep}nowhere{sep}..{sep}stick{sep}b",
        e.base.display()
    ));
    let r = scan(
        &mut src,
        &sneaky,
        &Default::default(),
        &open_opts(),
        &AtomicBool::new(false),
        &mut |_| {},
    );
    let msg = format!("{:?}", r.expect_err("refused"));
    assert!(msg.contains(".."), "{msg}");
}

/// The new archive name is also checked against the backup's own top-level folders when there is
/// no archive folder to move.
#[test]
fn renaming_an_archive_that_does_not_exist_still_checks_the_backup_for_the_name() {
    let e = env();
    let mut src = phone(vec![MemObject::new("1", "DCIM/A.JPG", b"a")]);
    claim(&e, &key_of(&src));
    put(&e.root, "Notes/readme.txt", b"mine");
    let (plan, _) = do_scan(&mut src, &e);
    for name in ["Notes", "NOTES"] {
        let r = rename_archive(&plan, name);
        assert!(r.is_err(), "{name}: {r:?}");
    }
    assert!(e.root.join("Notes/readme.txt").exists());
    // With a free name there is nothing to move, and the name is recorded.
    assert!(!rename_archive(&plan, "Gone").unwrap().moved);
}

// ---------------------------------------------------------------------------------------------
// Storages are named by the source and laid out by the plan, once

/// DESIGN.md 6.1: a source reports a file's storage and its path relative to the storage's root.
/// The plan alone decides whether the storage's name becomes a folder, so it can't be applied
/// twice, whether the phone has one storage or several.
#[test]
fn a_storage_folder_is_applied_exactly_once() {
    let on = |id: &str, path: &str, storage: &str| {
        let mut o = MemObject::new(id, path, id.as_bytes());
        o.storage = Some(storage.into());
        o
    };
    // A single storage, as an iPhone shows it: month folders at the top.
    let e = env();
    let mut src = phone(vec![
        on("1", "2024-05/IMG_1.HEIC", "Internal Storage"),
        on("2", "2024-06/IMG_2.HEIC", "Internal Storage"),
    ]);
    claim(&e, &key_of(&src));
    let (plan, _) = do_scan(&mut src, &e);
    let planned: Vec<&str> = plan.entries.iter().map(|x| x.source.as_str()).collect();
    assert_eq!(planned, ["2024-05/IMG_1.HEIC", "2024-06/IMG_2.HEIC"]);
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().copied, 2, "{:?}", failures(&ev));
    assert_eq!(
        files_under(&e.root),
        set(&["2024-05/IMG_1.HEIC", "2024-06/IMG_2.HEIC"])
    );

    // Several storages: one folder each, with the storage's name once and no further.
    let e = env();
    let mut src = phone(vec![
        on("1", "DCIM/a.jpg", "Internal"),
        on("2", "DCIM/b.jpg", "SD card"),
    ]);
    claim(&e, &key_of(&src));
    let (plan, _) = do_scan(&mut src, &e);
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().copied, 2, "{:?}", failures(&ev));
    assert_eq!(
        files_under(&e.root),
        set(&["Internal/DCIM/a.jpg", "SD card/DCIM/b.jpg"])
    );
    // A second scan with the choice made finds everything backed up, not new under a second
    // level of the same folder.
    let (plan, summary) = do_scan(&mut src, &e);
    assert_eq!(
        (summary.new, summary.verified),
        (0, 2),
        "{:?}",
        plan.entries
    );
}

/// A folder that has the storage's name, inside the storage, is the device's own folder and keeps
/// its place: the plan never strips a name it did not add.
#[test]
fn a_device_folder_named_like_its_storage_is_kept() {
    let e = env();
    let on = |id: &str, path: &str, storage: &str| {
        let mut o = MemObject::new(id, path, b"x");
        o.storage = Some(storage.into());
        o
    };
    let mut src = phone(vec![
        on("1", "Internal/a.jpg", "Internal"),
        on("2", "x/b.jpg", "SD card"),
    ]);
    let (plan, _) = do_scan(&mut src, &e);
    let planned: Vec<&str> = plan.entries.iter().map(|x| x.source.as_str()).collect();
    assert_eq!(planned, ["Internal/Internal/a.jpg", "SD card/x/b.jpg"]);
}

// ---------------------------------------------------------------------------------------------
// Empty folders come from the sources

/// A source that can tell fills in the folders it lists as empty, so the held-back rule works
/// for it.
#[test]
fn a_folder_source_reports_its_empty_folders_and_their_backup_files_are_held() {
    let e = env();
    let fake = e.base.join("fakestick");
    fs::create_dir_all(fake.join("DCIM/101APPLE")).unwrap();
    fs::create_dir_all(fake.join("DCIM/100APPLE")).unwrap();
    fs::write(fake.join("DCIM/100APPLE/KEEP.HEIC"), b"k").unwrap();
    let mut src = FolderSource::new(&fake).unwrap();
    let listing = src
        .enumerate(&AtomicBool::new(false), &mut |_, _| {})
        .unwrap();
    assert_eq!(listing.empty_folders, ["DCIM/101APPLE"]);
    claim(&e, &key_of(&src));
    put(&e.root, "DCIM/100APPLE/KEEP.HEIC", b"k");
    put(&e.root, "DCIM/101APPLE/HELD.HEIC", b"held");
    let (plan, _) = do_scan(&mut src, &e);
    let held: Vec<&str> = plan
        .removed
        .iter()
        .filter(|r| r.held)
        .map(|r| r.path.as_str())
        .collect();
    assert_eq!(held, ["DCIM/101APPLE/HELD.HEIC"]);
}

// ---------------------------------------------------------------------------------------------
// A backup folder and the folder a source reads never contain one another

/// DESIGN.md section 4 (Storage): a backup inside the device's own folder would be read as part of the device,
/// and the device's folder inside the backup would be read as backup content. Both are refused
/// before anything is listed.
#[test]
fn a_backup_folder_inside_the_source_folder_is_refused() {
    let e = env();
    let stick = e.base.join("stick");
    fs::create_dir_all(stick.join("DCIM")).unwrap();
    fs::write(stick.join("DCIM/a.jpg"), b"a").unwrap();
    let mut src = FolderSource::new(&stick).unwrap();
    let inside = stick.join("backup");
    let err = scan_into(&mut src, &e, &inside).unwrap_err();
    assert!(
        err.to_string()
            .contains("is inside the folder the device is read from"),
        "{err}"
    );
    assert!(!inside.exists(), "nothing is created for a refused scan");
    // The same, spelled with another letter case, on Windows.
    #[cfg(windows)]
    {
        let upper = PathBuf::from(inside.to_string_lossy().to_uppercase());
        assert!(scan_into(&mut src, &e, &upper).is_err());
    }
    // And through a wrapper that forwards the folder.
    let mut lagging = Lagging::new(FolderSource::new(&stick).unwrap());
    assert!(scan_into(&mut lagging, &e, &inside).is_err());
    // A backup folder beside it is fine.
    assert!(scan_into(&mut src, &e, &e.base.join("backup-beside")).is_ok());
}

#[test]
fn a_source_folder_inside_the_backup_folder_is_refused() {
    let e = env();
    let stick = e.root.join("mounted-stick");
    fs::create_dir_all(stick.join("DCIM")).unwrap();
    fs::write(stick.join("DCIM/a.jpg"), b"a").unwrap();
    let mut src = FolderSource::new(&stick).unwrap();
    let err = scan_into(&mut src, &e, &e.root).unwrap_err();
    assert!(
        err.to_string()
            .contains("contains the folder the device is read from"),
        "{err}"
    );
    // The very same folder is refused too.
    assert!(scan_into(&mut src, &e, &stick).is_err());
}

fn scan_into(
    src: &mut dyn DeviceSource,
    e: &Env,
    backup: &Path,
) -> Result<(ScanPlan, Summary), crate::device::DeviceError> {
    let skip = e
        .state
        .skip_list(&Scope::new(&src.identity().key, backup))
        .unwrap();
    scan(
        src,
        backup,
        &skip,
        &open_opts(),
        &AtomicBool::new(false),
        &mut |_| {},
    )
}

// ---------------------------------------------------------------------------------------------
// What happens to files deleted from the device, and to the old copy of a changed one
// (DESIGN.md section 6.3): archive, leave in place, or the Recycle Bin / Trash.

use crate::platform::fake_bin::{self, Fake};
use crate::registry::DeletedFiles;
use crate::state::{OldCopyGoes, ReplaceStep};
use crate::sync::OldCopy;

fn opts_for(mode: DeletedFiles) -> ScanOptions {
    ScanOptions {
        settings: DeviceSettings {
            deleted_files: mode,
            ..open_settings()
        },
        weak_match: false,
    }
}

/// The aside name for a changed `CHG.HEIC` in the sync stamped [`STAMP`].
const ASIDE: &str = "DCIM/CHG (old 2026-01-02 03-04).HEIC";

/// A claimed backup folder holding SAME (still on the device), CHG (changed on the device) and
/// GONE (deleted from it), and the device.
fn mixed(e: &Env) -> MemSource {
    let src = phone(vec![
        MemObject::new("1", "DCIM/SAME.HEIC", b"same"),
        MemObject::new("2", "DCIM/CHG.HEIC", b"newer"),
    ]);
    claim(e, &key_of(&src));
    put(&e.root, "DCIM/SAME.HEIC", b"same");
    put(&e.root, "DCIM/CHG.HEIC", b"old");
    put(&e.root, "DCIM/GONE.HEIC", b"gone");
    src
}

type Moved = (
    Result<SyncReport, SyncError>,
    Vec<SyncEvent>,
    Result<ArchiveReport, SyncError>,
    Vec<ArchiveEvent>,
);

/// The whole sync as the window does it: copy, then deal with what left the device.
fn sync_and_move(src: &mut MemSource, plan: &ScanPlan, e: &Env, confirmed: bool) -> Moved {
    let (r, ev) = do_sync_as(
        src,
        plan,
        e,
        &pending(plan),
        &SyncControl::default(),
        confirmed,
    );
    let mut aev = Vec::new();
    let a = archive_removed(plan, STAMP, confirmed, &mut |x| aev.push(x));
    (r, ev, a, aev)
}

fn old_copies(events: &[SyncEvent]) -> Vec<Option<OldCopy>> {
    events
        .iter()
        .filter_map(|e| match e {
            SyncEvent::Copied { old_copy, .. } => Some(old_copy.clone()),
            _ => None,
        })
        .collect()
}

/// Archive: a deleted file and the old copy of a changed one go into the dated archive folder.
#[test]
fn archive_mode_moves_a_deleted_file_and_a_changed_files_old_copy_into_the_archive() {
    let e = env();
    let mut src = mixed(&e);
    let (plan, summary) = do_scan_with(&mut src, &e, &opts_for(DeletedFiles::Archive));
    assert_eq!((summary.removed, summary.mismatch), (1, 1));
    assert!(plan.cautions.is_empty(), "{:?}", plan.cautions);
    let (r, ev, a, aev) = sync_and_move(&mut src, &plan, &e, false);
    let (r, a) = (r.unwrap(), a.unwrap());
    assert_eq!((r.copied, r.replaced, r.failed), (1, 1, 0), "{ev:?}");
    assert_eq!(
        (a.mode, a.moved, a.failed),
        (DeletedFiles::Archive, 1, 0),
        "{aev:?}"
    );
    assert!(matches!(&old_copies(&ev)[..], [Some(OldCopy::Archived(_))]));
    assert_eq!(fs::read(e.root.join("DCIM/CHG.HEIC")).unwrap(), b"newer");
    assert_eq!(
        fs::read(e.root.join(format!("{ARCHIVE_DIR}/{STAMP}/CHG.HEIC"))).unwrap(),
        b"old"
    );
    assert_eq!(
        fs::read(e.root.join(format!("{ARCHIVE_DIR}/{STAMP}/GONE.HEIC"))).unwrap(),
        b"gone"
    );
    assert_eq!(
        files_under(&e.root),
        set(&[
            "DCIM/SAME.HEIC",
            "DCIM/CHG.HEIC",
            &format!("{ARCHIVE_DIR}/{STAMP}/CHG.HEIC"),
            &format!("{ARCHIVE_DIR}/{STAMP}/GONE.HEIC"),
        ])
    );
}

/// Leave in place: a deleted file stays; a changed file's old copy is renamed beside the new
/// one, with the sync's local date and time, never replacing anything.
#[test]
fn leave_in_place_keeps_a_deleted_file_and_renames_a_changed_files_old_copy_beside_it() {
    let e = env();
    let mut src = mixed(&e);
    let (plan, _) = do_scan_with(&mut src, &e, &opts_for(DeletedFiles::LeaveInPlace));
    assert!(plan.cautions.is_empty(), "{:?}", plan.cautions);
    let (r, ev, a, aev) = sync_and_move(&mut src, &plan, &e, false);
    let (r, a) = (r.unwrap(), a.unwrap());
    assert_eq!((r.copied, r.replaced, r.failed), (1, 1, 0), "{ev:?}");
    assert_eq!(
        (a.mode, a.moved, a.failed, a.folder.is_none()),
        (DeletedFiles::LeaveInPlace, 0, 0, true),
        "{aev:?}"
    );
    assert!(aev.is_empty());
    assert!(
        matches!(&old_copies(&ev)[..], [Some(OldCopy::RenamedAside(p))] if p.as_str() == ASIDE),
        "{:?}",
        old_copies(&ev)
    );
    assert_eq!(fs::read(e.root.join("DCIM/CHG.HEIC")).unwrap(), b"newer");
    assert_eq!(fs::read(e.root.join(ASIDE)).unwrap(), b"old");
    assert_eq!(fs::read(e.root.join("DCIM/GONE.HEIC")).unwrap(), b"gone");
    assert!(
        !e.root.join(ARCHIVE_DIR).exists(),
        "no archive folder is made"
    );
    assert_eq!(
        files_under(&e.root),
        set(&["DCIM/SAME.HEIC", "DCIM/CHG.HEIC", ASIDE, "DCIM/GONE.HEIC"])
    );
}

/// An aside name that is taken gets a number, and the file already there is never replaced.
#[test]
fn a_taken_aside_name_is_numbered_not_replaced() {
    let e = env();
    let mut src = mixed(&e);
    put(&e.root, ASIDE, b"an earlier old copy");
    let (plan, _) = do_scan_with(&mut src, &e, &opts_for(DeletedFiles::LeaveInPlace));
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().replaced, 1, "{ev:?}");
    assert_eq!(
        fs::read(e.root.join(ASIDE)).unwrap(),
        b"an earlier old copy"
    );
    assert_eq!(
        fs::read(e.root.join("DCIM/CHG (old 2026-01-02 03-04) (2).HEIC")).unwrap(),
        b"old"
    );
    assert_eq!(fs::read(e.root.join("DCIM/CHG.HEIC")).unwrap(), b"newer");
}

/// Leave in place makes `(old ...)` copies in the backup. They are the backup's own doing, so a
/// scan never lists them as deleted from the device, in any mode, and switching the device to
/// Archive afterwards does not move them.
#[test]
fn old_copies_kept_beside_their_files_are_never_treated_as_deleted_from_the_device() {
    let e = env();
    let mut src = mixed(&e);
    let (plan, _) = do_scan_with(&mut src, &e, &opts_for(DeletedFiles::LeaveInPlace));
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().replaced, 1, "{ev:?}");
    assert!(e.root.join(ASIDE).exists());
    for mode in [
        DeletedFiles::LeaveInPlace,
        DeletedFiles::Archive,
        DeletedFiles::RecycleBin,
    ] {
        let (plan, _) = do_scan_with(&mut src, &e, &opts_for(mode));
        let gone: Vec<&str> = plan.removed.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(gone, ["DCIM/GONE.HEIC"], "{mode:?}");
        assert!(plan.outside.iter().any(|p| p == ASIDE), "{mode:?}");
    }
    // Switch to Archive and carry on: only the real deletion moves.
    let (plan, _) = do_scan_with(&mut src, &e, &opts_for(DeletedFiles::Archive));
    let (a, aev) = do_archive(&plan, STAMP);
    assert_eq!(a.unwrap().moved, 1, "{aev:?}");
    assert_eq!(fs::read(e.root.join(ASIDE)).unwrap(), b"old");
    assert!(!e.root.join("DCIM/GONE.HEIC").exists());
}

/// Recycle bin: a deleted file and the old copy of a changed one go to the bin, and the new
/// copy then takes its place. Nothing is deleted: the bin holds both whole.
#[test]
fn bin_mode_sends_a_deleted_file_and_a_changed_files_old_copy_to_the_bin() {
    let e = env();
    let bin = e.base.join("bin");
    let _g = fake_bin::set(Fake::Into(bin.clone()));
    let mut src = mixed(&e);
    let (plan, _) = do_scan_with(&mut src, &e, &opts_for(DeletedFiles::RecycleBin));
    assert!(plan.cautions.is_empty(), "{:?}", plan.cautions);
    let (r, ev, a, aev) = sync_and_move(&mut src, &plan, &e, false);
    let (r, a) = (r.unwrap(), a.unwrap());
    assert_eq!((r.copied, r.replaced, r.failed), (1, 1, 0), "{ev:?}");
    assert_eq!(
        (a.mode, a.moved, a.failed, a.folder.is_none()),
        (DeletedFiles::RecycleBin, 1, 0, true),
        "{aev:?}"
    );
    assert!(
        matches!(&aev[..], [ArchiveEvent::Binned { removed: 0 }]),
        "{aev:?}"
    );
    assert_eq!(old_copies(&ev), [Some(OldCopy::Binned)]);
    assert_eq!(fs::read(e.root.join("DCIM/CHG.HEIC")).unwrap(), b"newer");
    assert_eq!(
        files_under(&e.root),
        set(&["DCIM/SAME.HEIC", "DCIM/CHG.HEIC"])
    );
    assert_eq!(files_under(&bin), set(&["CHG.HEIC", "GONE.HEIC"]));
    assert_eq!(fs::read(bin.join("CHG.HEIC")).unwrap(), b"old");
    assert_eq!(fs::read(bin.join("GONE.HEIC")).unwrap(), b"gone");
    assert!(!e.root.join(ARCHIVE_DIR).exists());
}

/// The cautions gate every move out of place. Without the OK nothing goes to the archive or the
/// bin, and a changed file's old copy is neither archived, binned nor renamed.
#[test]
fn cautions_block_every_move_without_the_ok() {
    for mode in [
        DeletedFiles::Archive,
        DeletedFiles::LeaveInPlace,
        DeletedFiles::RecycleBin,
    ] {
        // A first sync into a folder that already has files and no marker.
        let e = env();
        let bin = e.base.join("bin");
        let _g = fake_bin::set(Fake::Into(bin.clone()));
        put(&e.root, "DCIM/CHG.HEIC", b"old");
        put(&e.root, "DCIM/GONE.HEIC", b"gone");
        let mut src = phone(vec![MemObject::new("1", "DCIM/CHG.HEIC", b"newer")]);
        let (plan, _) = do_scan_with(&mut src, &e, &opts_for(mode));
        assert!(
            plan.cautions
                .iter()
                .any(|c| matches!(c, Caution::FirstSyncIntoFilledFolder { .. })),
            "{mode:?}: {:?}",
            plan.cautions
        );
        let (r, ev, a, _) = sync_and_move(&mut src, &plan, &e, false);
        let r = r.unwrap();
        assert_eq!(
            (r.copied, r.replaced, r.failed),
            (0, 0, 1),
            "{mode:?}: {ev:?}"
        );
        assert!(
            failures(&ev)[0].contains("needs confirmation"),
            "{mode:?}: {ev:?}"
        );
        match mode {
            // Nothing moves, so nothing needs asking about, but the changed file still did.
            DeletedFiles::LeaveInPlace => assert_eq!(a.unwrap().moved, 0),
            _ => assert!(
                matches!(a, Err(SyncError::NeedsConfirmation(_))),
                "{mode:?}: {a:?}"
            ),
        }
        assert_eq!(
            fs::read(e.root.join("DCIM/CHG.HEIC")).unwrap(),
            b"old",
            "{mode:?}"
        );
        assert_eq!(
            fs::read(e.root.join("DCIM/GONE.HEIC")).unwrap(),
            b"gone",
            "{mode:?}"
        );
        assert_eq!(
            files_under(&e.root),
            set(&["DCIM/CHG.HEIC", "DCIM/GONE.HEIC"]),
            "{mode:?}: nothing was moved, renamed or made"
        );
        assert!(files_under(&bin).is_empty(), "{mode:?}");
        // With the OK the changed file's change goes through.
        let (r, ev, _, _) = sync_and_move(&mut src, &plan, &e, true);
        assert_eq!(r.unwrap().replaced, 1, "{mode:?}: {:?}", failures(&ev));
    }
}

/// A wiped or swapped device must never send the backup to the bin without the OK.
#[test]
fn a_wiped_device_does_not_send_the_backup_to_the_bin_without_the_ok() {
    let wiped = |e: &Env| {
        let src = phone(vec![MemObject::new("1", "DCIM/KEEP.HEIC", b"k")]);
        claim(e, &key_of(&src));
        put(&e.root, "DCIM/KEEP.HEIC", b"k");
        for i in 0..30 {
            put(&e.root, &format!("DCIM/IMG_{i:03}.HEIC"), b"x");
        }
        src
    };
    let e = env();
    let bin = e.base.join("bin");
    let _g = fake_bin::set(Fake::Into(bin.clone()));
    let mut src = wiped(&e);
    let (plan, _) = do_scan_with(&mut src, &e, &opts_for(DeletedFiles::RecycleBin));
    assert_eq!(plan.removed.len(), 30);
    assert!(
        plan.cautions
            .iter()
            .any(|c| matches!(c, Caution::LargeBatch { .. })),
        "{:?}",
        plan.cautions
    );
    let (r, aev) = do_archive_unconfirmed(&plan, STAMP);
    assert!(
        matches!(r, Err(SyncError::NeedsConfirmation(_))),
        "{r:?} {aev:?}"
    );
    assert!(aev.is_empty());
    assert_eq!(
        files_under(&e.root).len(),
        31,
        "every file is still in the backup"
    );
    assert!(files_under(&bin).is_empty(), "nothing reached the bin");
    // With the OK, all of them go.
    let (r, _) = do_archive(&plan, STAMP);
    assert_eq!(r.unwrap().moved, 30);
    assert_eq!(files_under(&bin).len(), 30);
    // The same wiped device with Leave in place has nothing to ask about: nothing moves.
    let e = env();
    let mut src = wiped(&e);
    let (plan, _) = do_scan_with(&mut src, &e, &opts_for(DeletedFiles::LeaveInPlace));
    assert!(plan.cautions.is_empty(), "{:?}", plan.cautions);
    let (r, _) = do_archive_unconfirmed(&plan, STAMP);
    assert_eq!(r.unwrap().moved, 0);
    assert_eq!(files_under(&e.root).len(), 31);
}

/// A bin that can't take a file never turns into a delete: a deleted file stays, and a changed
/// file's old copy stays in place, the new copy is not put over it, and no temporary file is
/// left behind.
#[test]
fn an_unavailable_bin_leaves_everything_in_place() {
    let e = env();
    let _g = fake_bin::set(Fake::Unavailable);
    let mut src = mixed(&e);
    let (plan, _) = do_scan_with(&mut src, &e, &opts_for(DeletedFiles::RecycleBin));
    let (r, ev, a, aev) = sync_and_move(&mut src, &plan, &e, false);
    let r = r.unwrap();
    assert_eq!((r.copied, r.replaced, r.failed), (0, 0, 1), "{ev:?}");
    let msg = &failures(&ev)[0];
    assert!(
        msg.contains("left in place") && msg.contains(crate::platform::BIN_NAME),
        "{msg}"
    );
    let a = a.unwrap();
    assert_eq!((a.moved, a.failed), (0, 1), "{aev:?}");
    assert!(
        matches!(&aev[..], [ArchiveEvent::Failed { error, .. }] if error.contains("left in place")),
        "{aev:?}"
    );
    assert_eq!(fs::read(e.root.join("DCIM/CHG.HEIC")).unwrap(), b"old");
    assert_eq!(fs::read(e.root.join("DCIM/GONE.HEIC")).unwrap(), b"gone");
    assert_eq!(
        files_under(&e.root),
        set(&["DCIM/SAME.HEIC", "DCIM/CHG.HEIC", "DCIM/GONE.HEIC"]),
        "no data lost, no leftovers"
    );
    assert!(
        !e.state.path().join("in_progress.json").exists(),
        "the crash record is cleared"
    );
}

fn write_record(e: &Env, o: &Path, t: &Path, moved: &Path, goes: OldCopyGoes) {
    e.state
        .begin_copy(&crate::state::InProgress {
            device: "serial:PHONE-A".into(),
            root: e.root.clone(),
            source: "DCIM/IMG.HEIC".into(),
            temp: t.to_path_buf(),
            replace: Some(ReplaceStep {
                original: o.to_path_buf(),
                archived: moved.to_path_buf(),
                goes,
                staged_size: None,
            }),
            destination: None,
        })
        .unwrap();
}

/// The crash record of a changed file in Leave-in-place mode: the old copy beside the original.
fn aside_crash(
    e: &Env,
    original: Option<&[u8]>,
    aside: Option<&[u8]>,
    temp: Option<&[u8]>,
) -> (PathBuf, PathBuf, PathBuf) {
    let dir = e.root.join("DCIM");
    let (o, a, t) = (
        dir.join("IMG.HEIC"),
        dir.join("IMG (old 2026-01-02 03-04).HEIC"),
        dir.join(format!("IMG.HEIC{TEMP_SUFFIX}")),
    );
    fs::create_dir_all(&dir).unwrap();
    claim(e, "serial:PHONE-A");
    for (p, data) in [(&o, original), (&a, aside), (&t, temp)] {
        if let Some(d) = data {
            fs::write(p, d).unwrap();
        }
    }
    write_record(e, &o, &t, &a, OldCopyGoes::Aside);
    (o, a, t)
}

/// Recovery after a crash at each step of a changed file, with the old copy renamed beside it.
#[test]
fn recovery_finishes_a_leave_in_place_replacement_at_every_step() {
    let both = |o: &Path, a: &Path| (fs::read(o).unwrap(), fs::read(a).unwrap());
    let (new, old) = (b"new".to_vec(), b"old".to_vec());
    // Before the rename: the old copy in place, the new one whole in the temporary file.
    let e = env();
    let (o, a, t) = aside_crash(&e, Some(b"old"), None, Some(b"new"));
    let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
    assert!(got.completed, "{}", got.note);
    assert_eq!(both(&o, &a), (new.clone(), old.clone()));
    assert!(!t.exists());
    // After the rename: the old copy beside, the new one in the temporary file.
    let e = env();
    let (o, a, t) = aside_crash(&e, None, Some(b"old"), Some(b"new"));
    let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
    assert!(got.completed, "{}", got.note);
    assert_eq!(both(&o, &a), (new.clone(), old.clone()));
    assert!(!t.exists());
    // After both: nothing to do.
    let e = env();
    let (o, a, _) = aside_crash(&e, Some(b"new"), Some(b"old"), None);
    let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
    assert!(got.completed, "{}", got.note);
    assert_eq!(both(&o, &a), (new, old));
    // A record whose "aside" is not a sibling of the original (or not named like one) is not
    // acted on: every file is left.
    for bad in ["../elsewhere.bin", "IMG.HEIC.other"] {
        let e = env();
        let (o, _, t) = aside_crash(&e, Some(b"old"), None, Some(b"new"));
        let moved = e.root.join("DCIM").join(bad);
        write_record(&e, &o, &t, &moved, OldCopyGoes::Aside);
        let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
        assert!(!got.completed && !got.temp_removed, "{bad}: {}", got.note);
        assert_eq!(fs::read(&o).unwrap(), b"old");
        assert_eq!(fs::read(&t).unwrap(), b"new");
        assert!(!moved.exists());
    }
}

/// A forged record is refused whatever kind it claims to be: an Aside record whose destination is
/// in another folder, a different file's aside name, or the archive; an Archive record whose
/// destination is beside the original; a Bin record that names a destination. Every file is
/// left, and nothing moves.
#[test]
fn recovery_refuses_forged_records_of_every_kind() {
    let in_archive = format!("{ARCHIVE_DIR}/{STAMP}/IMG.HEIC");
    let cases: [(&str, OldCopyGoes); 7] = [
        // Aside, but not a sibling of the original.
        (
            "DCIM/sub/IMG (old 2026-01-02 03-04).HEIC",
            OldCopyGoes::Aside,
        ),
        ("IMG (old 2026-01-02 03-04).HEIC", OldCopyGoes::Aside),
        // Aside, a sibling, but another file's stem.
        ("DCIM/OTHER (old 2026-01-02 03-04).HEIC", OldCopyGoes::Aside),
        ("DCIM/IMG (old).HEIC", OldCopyGoes::Aside),
        // An archive path, recorded as an aside or a bin move.
        (&in_archive, OldCopyGoes::Aside),
        (&in_archive, OldCopyGoes::Bin),
        // An aside name, recorded as an archive move.
        ("DCIM/IMG (old 2026-01-02 03-04).HEIC", OldCopyGoes::Archive),
    ];
    for (dest, goes) in cases {
        let e = env();
        let (o, _, t) = crash_state(&e, Some(b"old"), None, Some(b"new"), false);
        let moved = e.root.join(dest);
        write_record(&e, &o, &t, &moved, goes);
        let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
        assert!(
            !got.completed && !got.temp_removed,
            "{dest} as {goes:?}: {}",
            got.note
        );
        assert_eq!(fs::read(&o).unwrap(), b"old", "{dest} as {goes:?}");
        assert_eq!(fs::read(&t).unwrap(), b"new", "{dest} as {goes:?}");
        assert!(!moved.exists(), "{dest} as {goes:?}");
    }
}

/// The crash record of a changed file in bin mode: there is no destination to record.
fn bin_crash(e: &Env, original: Option<&[u8]>, temp: Option<&[u8]>) -> (PathBuf, PathBuf) {
    let dir = e.root.join("DCIM");
    let (o, t) = (
        dir.join("IMG.HEIC"),
        dir.join(format!("IMG.HEIC{TEMP_SUFFIX}")),
    );
    fs::create_dir_all(&dir).unwrap();
    claim(e, "serial:PHONE-A");
    for (p, data) in [(&o, original), (&t, temp)] {
        if let Some(d) = data {
            fs::write(p, d).unwrap();
        }
    }
    write_record(e, &o, &t, Path::new(""), OldCopyGoes::Bin);
    (o, t)
}

/// Recovery after a crash at each step of a changed file whose old copy goes to the bin. It
/// never needs anything back from the bin: if the original is gone and the new copy is whole,
/// the new copy is moved into place.
#[test]
fn recovery_finishes_a_bin_replacement_at_every_step_without_the_bin() {
    // The old copy already in the bin: only the new copy is left to move into place.
    let e = env();
    let (o, t) = bin_crash(&e, None, Some(b"new"));
    let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
    assert!(got.completed, "{}", got.note);
    assert_eq!(fs::read(&o).unwrap(), b"new");
    assert!(!t.exists());
    // Before the old copy moved: it stays where it is; the unfinished new copy is dropped (it is
    // on the device) and the next sync makes it again. Recovery sends nothing to the bin.
    let e = env();
    let (o, t) = bin_crash(&e, Some(b"old"), Some(b"new"));
    let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
    assert!(!got.completed && got.temp_removed, "{}", got.note);
    assert_eq!(fs::read(&o).unwrap(), b"old");
    assert!(!t.exists());
    // After both: nothing to do.
    let e = env();
    let (o, _) = bin_crash(&e, Some(b"new"), None);
    let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
    assert!(got.completed, "{}", got.note);
    assert_eq!(fs::read(&o).unwrap(), b"new");
    // Nothing left of either: reported, and nothing invented.
    let e = env();
    let (o, _) = bin_crash(&e, None, None);
    let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
    assert!(!got.completed, "{}", got.note);
    assert!(!o.exists());
    // A bin record that names a destination is not one of ours: left alone.
    let e = env();
    let (o, t) = bin_crash(&e, None, Some(b"new"));
    write_record(
        &e,
        &o,
        &t,
        &e.root.join("DCIM/other.HEIC"),
        OldCopyGoes::Bin,
    );
    let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
    assert!(!got.completed && !got.temp_removed, "{}", got.note);
    assert_eq!(fs::read(&t).unwrap(), b"new");
    assert!(!o.exists());
}

/// A crash record from before the choice existed (no `goes`) is an archive move.
#[test]
fn an_old_crash_record_is_an_archive_move() {
    let json = r#"{"device":"d","root":"r","source":"s","temp":"t",
        "replace":{"original":"o","archived":"a"}}"#;
    let rec: crate::state::InProgress = serde_json::from_str(json).unwrap();
    assert_eq!(rec.replace.unwrap().goes, OldCopyGoes::Archive);
}

/// `text` with every test module (`#[cfg(test)]` or `#[cfg(all(test, ...))]` over `mod name {`) cut out (found by matching braces,
/// skipping strings, characters and comments), so production code after a test module is kept.
fn without_test_modules(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    let mut keep_from = 0;
    // `mod x`, `pub mod x`, `pub(super) mod x` and `pub(crate) mod x` all start a module.
    let next_mod = |from: usize| {
        [
            "\nmod ",
            "\npub mod ",
            "\npub(super) mod ",
            "\npub(crate) mod ",
        ]
        .iter()
        .filter_map(|p| text[from..].find(p))
        .min()
    };
    while let Some(off) = next_mod(i) {
        // A test module is `mod` right under `#[cfg(test)]` or `#[cfg(all(test, ...))]`.
        let mod_at = i + off;
        // The attribute lines right above the `mod` (there may be several, in any order).
        let mut start = mod_at;
        let mut is_test = false;
        let mut end = mod_at;
        loop {
            let ls = text[..end].rfind('\n').map_or(0, |n| n + 1);
            let attr = &text[ls..end];
            if !attr.trim_start().starts_with("#[") {
                break;
            }
            is_test |=
                attr.starts_with("#[cfg(") && attr.contains("test") && !attr.contains("not(test");
            start = ls;
            if ls == 0 {
                break;
            }
            end = ls - 1;
        }
        if !is_test {
            i = mod_at + 1;
            continue;
        }
        // The block starts at the first `{` after the module's name (a `mod x;` has none).
        let Some(open) = text[start..].find(['{', ';']).map(|o| start + o) else {
            break;
        };
        if b[open] == b';' {
            i = open;
            continue;
        }
        let mut depth = 0usize;
        let mut j = open;
        while j < b.len() {
            match b[j] {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        j += 1;
                        break;
                    }
                }
                b'/' if b.get(j + 1) == Some(&b'/') => {
                    while j < b.len() && b[j] != b'\n' {
                        j += 1;
                    }
                    continue;
                }
                b'/' if b.get(j + 1) == Some(&b'*') => {
                    j += 2;
                    while j + 1 < b.len() && !(b[j] == b'*' && b[j + 1] == b'/') {
                        j += 1;
                    }
                    j += 1;
                }
                b'r' if matches!(b.get(j + 1), Some(b'"') | Some(b'#'))
                    && (!b[j.saturating_sub(1)].is_ascii_alphanumeric()
                        // `br"..."` and `br#"..."#`, raw byte strings.
                        || (b[j.saturating_sub(1)] == b'b'
                            && !b[j.saturating_sub(2)].is_ascii_alphanumeric())) =>
                {
                    // A raw string: r"..." or r#"..."#.
                    let mut k = j + 1;
                    let mut hashes = 0;
                    while b.get(k) == Some(&b'#') {
                        hashes += 1;
                        k += 1;
                    }
                    if b.get(k) == Some(&b'"') {
                        let end = format!("\"{}", "#".repeat(hashes));
                        j = text[k + 1..]
                            .find(&end)
                            .map_or(b.len(), |e| k + 1 + e + end.len());
                        continue;
                    }
                }
                b'"' => {
                    j += 1;
                    while j < b.len() && b[j] != b'"' {
                        if b[j] == b'\\' {
                            j += 1;
                        }
                        j += 1;
                    }
                }
                b'\'' => {
                    // A character literal ('{', '\n', '\''), not a lifetime.
                    let rest = &text[j + 1..];
                    let len = if rest.starts_with('\\') {
                        rest.find('\'').map(|e| {
                            // '\'' has its quote escaped.
                            if rest[..=e].ends_with("\\'") {
                                rest[e + 1..].find('\'').map_or(e, |x| e + 1 + x)
                            } else {
                                e
                            }
                        })
                    } else {
                        rest.char_indices()
                            .nth(1)
                            .filter(|(_, c)| *c == '\'')
                            .map(|(k, _)| k)
                    };
                    if let Some(len) = len {
                        j += len + 1;
                    }
                }
                _ => {}
            }
            j += 1;
        }
        out.push_str(&text[keep_from..start]);
        keep_from = j.min(b.len());
        i = keep_from;
    }
    out.push_str(&text[keep_from..]);
    out
}

/// Every `.rs` file of both crates with its production code (test modules cut out), as
/// `(path relative to the repository, code)`: `core/src/x.rs` or `app/src/x.rs`, so a rule that
/// names a file names exactly one.
fn production_sources() -> Vec<(String, String)> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut out = Vec::new();
    for (crate_dir, dir) in [
        ("core", manifest.join("src")),
        ("app", manifest.join("../app/src")),
    ] {
        for entry in WalkDir::new(&dir) {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let in_tests = path
                .to_string_lossy()
                .replace('\\', "/")
                .contains("/e2e_tests/");
            if in_tests || path.ends_with("e2e_tests.rs") || path.ends_with("testutil.rs") {
                continue; // whole files of test code
            }
            let text = fs::read_to_string(path).unwrap().replace("\r\n", "\n");
            let code = without_test_modules(&text);
            let rel = format!(
                "{crate_dir}/src/{}",
                path.strip_prefix(&dir)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .replace('\\', "/")
            );
            out.push((rel, code));
        }
    }
    out
}

/// `code` with comments, string literals and character literals blanked out (newlines kept), so
/// a word in a message or a comment is not taken for a call.
fn without_literals(code: &str) -> String {
    let b = code.as_bytes();
    let mut out = String::with_capacity(code.len());
    let mut i = 0;
    let blank = |out: &mut String, from: usize, to: usize| {
        for c in code[from..to].chars() {
            out.push(if c == '\n' { '\n' } else { ' ' });
        }
    };
    while i < b.len() {
        let rest = &code[i..];
        let after_word = i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_');
        let end = if rest.starts_with("//") {
            Some(rest.find('\n').map_or(code.len(), |e| i + e))
        } else if rest.starts_with("/*") {
            Some(rest.find("*/").map_or(code.len(), |e| i + e + 2))
        } else if b[i] == b'"' || (after_word && rest.starts_with("b\"")) {
            let mut j = i + if b[i] == b'b' { 2 } else { 1 };
            while j < b.len() && b[j] != b'"' {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            Some((j + 1).min(b.len()))
        } else if after_word && (rest.starts_with('r') || rest.starts_with("br")) && {
            let t = rest.trim_start_matches('b').trim_start_matches('r');
            t.trim_start_matches('#').starts_with('"')
        } {
            // A raw string: r"..", r#".."#, br#".."#.
            let t = rest.trim_start_matches('b').trim_start_matches('r');
            let hashes = t.len() - t.trim_start_matches('#').len();
            let close = format!("\"{}", "#".repeat(hashes));
            let open = rest.find('"').unwrap() + 1;
            Some(
                rest[open..]
                    .find(&close)
                    .map_or(code.len(), |e| i + open + e + close.len()),
            )
        } else if b[i] == b'\'' {
            // A character literal ('x', '\n', '\''), not a lifetime.
            let r = &code[i + 1..];
            let len = if r.starts_with('\\') {
                r.char_indices()
                    .skip(2)
                    .find(|(_, c)| *c == '\'')
                    .map(|(k, _)| k)
            } else {
                r.char_indices()
                    .nth(1)
                    .filter(|(_, c)| *c == '\'')
                    .map(|(k, _)| k)
            };
            len.map(|l| i + 1 + l + 1)
        } else {
            None
        };
        if let Some(end) = end {
            blank(&mut out, i, end);
            i = end;
        } else {
            let c = rest.chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

/// Where `needle` is used in `code` (already without literals). A needle ending in `*` matches
/// any word that starts with it; otherwise it must be a whole word (so `rename` does not match
/// `rename_no_replace`). A method or function the file defines itself (`fn rename`,
/// `.rename(`) and an attribute line (`#[serde(rename = ..)]`) are not uses of the std call.
fn uses_of(code: &str, needle: &str) -> Vec<usize> {
    let (word, prefix) = needle
        .strip_suffix('*')
        .map_or((needle, false), |w| (w, true));
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    code.match_indices(word)
        .map(|(i, _)| i)
        .filter(|&i| {
            let before = code[..i].chars().next_back();
            let after = code[i + word.len()..].chars().next();
            if before.is_some_and(ident) || (!prefix && after.is_some_and(ident)) {
                return false;
            }
            let line = &code[code[..i].rfind('\n').map_or(0, |n| n + 1)..i];
            let head = code[..i].trim_end();
            !(line.trim_start().starts_with("#[")
                || (word == "rename" && (head.ends_with('.') || head.ends_with("fn"))))
        })
        .collect()
}

/// Where a production file may use a call that is otherwise forbidden, and why. A call is only
/// allowed in the files listed for it, each named by its whole path.
const ALLOWED_USES: &[(&str, &[&str], &str)] = &[
    // Removing the app's own temporary or state files, never a backup file:
    (
        "remove_file",
        &[
            "core/src/store/local.rs",         // an unfinished `.pnk-part` upload
            "core/src/state.rs",               // the crash record
            "core/src/idfile.rs",              // the temp of the ID file it was writing
            "core/src/heic/mod.rs", // its own `.pnk-jpg-part` temp of a JPG copy, and the `plug-n-keep-<pid>-<n>.<ext>` temp of a HEIC read from a device (guard drop, and the start-up sweep of old ones)
            "core/src/platform/unix.rs", // the link-then-unlink move and its undo
            "core/src/platform/unix_trash.rs", // only the `.trashinfo` it has just written
        ],
        "app-made temporary and state files",
    ),
    // Every move of a backup file goes through the no-replace helper; a plain rename
    // (which replaces) is only the settings files' own:
    (
        "rename",
        &[
            "core/src/platform/unix.rs", // the helper's own file: the replacing rename for atomic writes
            "core/src/state.rs",         // setting `defaults.json` aside after reading it
        ],
        "the no-replace helper and the settings folder",
    ),
    // The raw routes to a replacing move:
    (
        "MoveFileEx*",
        &["core/src/platform/windows/fs.rs"],
        "the no-replace and the replace-the-settings helpers",
    ),
    (
        "MOVEFILE_REPLACE_EXISTING",
        &["core/src/platform/windows/fs.rs"],
        "the replace-the-settings helper only",
    ),
    (
        "SYS_renameat2",
        &["core/src/platform/linux/fs.rs"],
        "the no-replace helper (RENAME_NOREPLACE)",
    ),
    ("SYS_renameat", &[], "a replacing rename"),
    ("SYS_rename", &[], "a replacing rename"),
    // Overwriting or truncating:
    (
        "File::create",
        &["core/src/state.rs"],
        "the temp of a settings file, then moved over it",
    ),
    (
        "fs::write",
        &["core/src/fake.rs"],
        "the fake device writing into its own test folder",
    ),
    ("set_len", &[], "truncating or extending a file"),
];

/// Files that open a file for writing without `create_new` (which would refuse an existing
/// file), and why that is not an overwrite of a backup file.
const OPEN_FOR_WRITE_OK: &[(&str, &str)] = &[
    (
        "core/src/platform/unix.rs",
        "opens an existing file only to flush it to disk",
    ),
    (
        "core/src/platform/windows/fs.rs",
        "opens an existing file only to flush it to disk",
    ),
    ("core/src/volume.rs", "opens a device or a probe"),
];

/// Every use in `sources` of a call that no rule allows there, as messages.
fn forbidden_uses(sources: &[(String, String)]) -> Vec<String> {
    let mut bad = Vec::new();
    for (path, code) in sources {
        let code = without_literals(code);
        for (needle, files, _why) in ALLOWED_USES {
            if !uses_of(&code, needle).is_empty() && !files.contains(&path.as_str()) {
                bad.push(format!(
                    "{path} uses {needle}, which is only allowed in {files:?}"
                ));
            }
        }
        // An alias hides a use: `use std::fs::remove_file as rm`.
        for needle in ["remove_file", "rename"] {
            for at in uses_of(&code, needle) {
                let after = code[at + needle.len()..].trim_start();
                if after.starts_with("as ") {
                    bad.push(format!("{path} renames the import of {needle}"));
                }
            }
        }
        // Opening for writing must refuse an existing file.
        for opener in ["OpenOptions::new", "File::options"] {
            for at in uses_of(&code, opener) {
                let tail = &code[at + opener.len()..(at + 500).min(code.len())];
                // Up to the next opener, so one chain's flags are not another's.
                let cut = ["OpenOptions::new", "File::options"]
                    .iter()
                    .filter_map(|o| tail.find(o))
                    .min()
                    .unwrap_or(tail.len());
                let window = &tail[..cut];
                let writes = window.contains(".write(true)") || window.contains(".append(true)");
                if writes
                    && !window.contains("create_new(true)")
                    && !OPEN_FOR_WRITE_OK.iter().any(|(f, _)| f == path)
                {
                    bad.push(format!(
                        "{path} opens a file for writing without create_new"
                    ));
                }
            }
        }
    }
    bad
}

/// DESIGN.md 10.3: nothing in the backup is ever deleted for good. Only the two bin modules
/// may use the system's recycle/trash calls, and nowhere may the app call a permanent delete of
/// a folder tree, the shell's file-operation API or a raw unlink. Test modules are cut out; all
/// other production code, wherever it sits in a file, is scanned.
#[test]
fn only_the_bin_modules_touch_the_recycle_or_trash_calls_and_nothing_deletes_for_good() {
    let bin_calls = ["IFileOperation", "DeleteItem", "FOFX_RECYCLEONDELETE"];
    let never = [
        "SHFileOperation",
        "DeleteFile", // DeleteFileA, DeleteFileW, DeleteFileTransactedW
        "RemoveDirectory",
        "remove_dir", // remove_dir and remove_dir_all
        "remove_dir_all",
        "unlinkat",
        "libc::unlink",
        "libc::remove(",
        "libc::rmdir",
        "fs::copy(",
        "NtDeleteFile",
        "truncate(true)",
        "DELETE_ON_CLOSE",
        "FileDispositionInfo",
    ];
    let mut windows_bin = None;
    let mut unix_bin = None;
    let sources = production_sources();
    let bad = forbidden_uses(&sources);
    assert!(bad.is_empty(), "{}", bad.join("\n"));
    for (path, code) in sources {
        for needle in never {
            assert!(!code.contains(needle), "{path} uses {needle}");
        }
        let is_windows_bin = path.ends_with("platform/windows/trash.rs");
        if is_windows_bin {
            windows_bin = Some(code);
            continue;
        }
        if path.ends_with("platform/unix_trash.rs") {
            unix_bin = Some(code);
            continue;
        }
        for needle in bin_calls {
            assert!(!code.contains(needle), "{path} uses {needle}");
        }
    }
    // The Windows bin module asks for the Recycle Bin, refuses anything else, and removes nothing.
    let w = windows_bin.expect("platform/windows/trash.rs exists");
    for must in [
        "FOF_ALLOWUNDO",
        "FOFX_RECYCLEONDELETE",
        "FOFX_EARLYFAILURE",
        "TSF_DELETE_RECYCLE_IF_POSSIBLE",
        "fn PreDeleteItem",
        "Err(E_ABORT.into())",
        "fn PostDeleteItem",
        "bin_item.is_some()",
        "op.DeleteItem(&item, &sink)",
        "op.Advise(&sink)",
    ] {
        assert!(w.contains(must), "the Windows bin module lacks {must}");
    }
    assert!(
        !w.contains("remove_file"),
        "the Windows bin module removes a file"
    );
    // The Trash module only ever removes the note it has just written.
    let u = unix_bin.expect("platform/unix_trash.rs exists");
    let calls: Vec<&str> = u
        .match_indices("remove_file(")
        .map(|(i, _)| &u[i..i + "remove_file(&info_path)".len()])
        .collect();
    assert!(
        !calls.is_empty() && calls.iter().all(|c| *c == "remove_file(&info_path)"),
        "{calls:?}"
    );
    assert!(!u.contains("remove_dir") && !u.contains("fs::copy"));
}

/// The scan must see through the ways a forbidden call can be written without its literal
/// `remove_file(` or `fs::rename(`: an import renamed, a function passed on, a bare call after
/// a `use`, and the overwrite routes. And it must not take a file's own `fn rename` or a word
/// in a message for one, nor let one file inherit another's permission by the end of its name.
#[test]
fn the_scan_catches_indirect_and_overwriting_routes() {
    let scan = |path: &str, code: &str| forbidden_uses(&[(path.to_string(), code.to_string())]);
    for code in [
        "use std::fs::remove_file as rm;\nfn a() { rm(p) }",
        "fn a() { v.iter().map(fs::remove_file) }",
        "use std::fs::rename;\nfn a() { rename(a, b) }",
        "use std::fs::{self, rename as mv};",
        "fn a() { let _ = File::create(p); }",
        "fn a() { fs::write(p, b); }",
        "fn a() { f.set_len(0); }",
        "fn a() { libc::rename(a, b); }",
        "fn a() { MoveFileExW(a, b, F); }",
        "const F: u32 = MOVEFILE_REPLACE_EXISTING.0;",
        "fn a() { OpenOptions::new().write(true).open(p) }",
        "fn a() { File::options().append(true).open(p) }",
    ] {
        assert!(!scan("core/src/sneaky.rs", code).is_empty(), "{code}");
    }
    // Allowed where listed, by whole path, and with `create_new`.
    assert!(scan("core/src/state.rs", "fn a() { fs::remove_file(p) }").is_empty());
    assert!(!scan("app/src/state.rs", "fn a() { fs::remove_file(p) }").is_empty());
    assert!(!scan("core/src/xstate.rs", "fn a() { fs::remove_file(p) }").is_empty());
    assert!(
        scan(
            "core/src/sneaky.rs",
            "fn a() { OpenOptions::new().write(true).create_new(true).open(p) }"
        )
        .is_empty()
    );
    // Its own method, an attribute, a message and a comment are not the std call.
    assert!(
        scan(
            "app/src/worker.rs",
            "#[serde(rename = \"x\")]\nfn rename(&mut self) { self.rename(); }\n// remove_file\nfn b() { log(\"could not rename it\"); rename_no_replace(a, b); }"
        )
        .is_empty()
    );
}

/// The scanner that cuts the test modules out must keep what follows one, and cope with braces
/// in strings, characters and comments.
#[test]
fn production_code_after_a_test_module_is_still_scanned() {
    let text = "fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() { let s = \"}\"; let c = '}'; /* } */ // }\n        let r = r#\"}\"#; }\n    mod inner { }\n}\nfn after() { forbidden() }\n";
    let kept = without_test_modules(text);
    assert!(
        kept.contains("fn a()") && kept.contains("forbidden()"),
        "{kept}"
    );
    assert!(
        !kept.contains("fn t()") && !kept.contains("inner"),
        "{kept}"
    );
    // `mod x;` has no block to cut.
    let kept = without_test_modules("#[cfg(test)]\nmod x;\nfn b() {}\n");
    assert!(kept.contains("fn b()"));
    // A test module with a visibility is cut too.
    let kept = without_test_modules(
        "fn a() {}\n#[cfg(test)]\npub(super) mod check {\n    fn x() {}\n}\nfn b() {}\n",
    );
    assert!(kept.contains("fn a()") && kept.contains("fn b()") && !kept.contains("fn x()"));
}

/// A backup file that changed since the scan is not sent to the bin (the same check as for
/// archiving).
#[test]
fn a_file_that_changed_since_the_scan_is_not_sent_to_the_bin() {
    let e = env();
    let bin = e.base.join("bin");
    let _g = fake_bin::set(Fake::Into(bin.clone()));
    let mut src = mixed(&e);
    let (plan, _) = do_scan_with(&mut src, &e, &opts_for(DeletedFiles::RecycleBin));
    put(&e.root, "DCIM/GONE.HEIC", b"edited since the scan");
    let (r, aev) = do_archive(&plan, STAMP);
    assert_eq!(r.unwrap().moved, 0, "{aev:?}");
    assert!(
        matches!(&aev[..], [ArchiveEvent::Failed { error, .. }] if error.contains("changed since the scan")),
        "{aev:?}"
    );
    assert!(files_under(&bin).is_empty());
    assert_eq!(
        fs::read(e.root.join("DCIM/GONE.HEIC")).unwrap(),
        b"edited since the scan"
    );
}

// ---------------------------------------------------------------------------------------------
// Room on the backup disk (DESIGN.md section 7.5): refuse a sync that won't fit, and stop cleanly
// if the disk fills up anyway.

use crate::platform::{DiskSpace, fake_space};

const GIB: u64 = 1 << 30;

/// A claimed backup folder with SAME (backed up, 4 bytes), CHG (changed: the device's copy is
/// 300 bytes, the backup's 3) and the device's new files NEW1 (1000 bytes) and NEW2 (50).
fn space_fixture(e: &Env) -> MemSource {
    let src = phone(vec![
        MemObject::new("1", "DCIM/SAME.HEIC", b"same"),
        MemObject::new("2", "DCIM/CHG.HEIC", &[b'c'; 300]),
        MemObject::new("3", "DCIM/NEW1.HEIC", &[b'n'; 1000]),
        MemObject::new("4", "DCIM/NEW2.HEIC", &[b'm'; 50]),
    ]);
    claim(e, &key_of(&src));
    put(&e.root, "DCIM/SAME.HEIC", b"same");
    put(&e.root, "DCIM/CHG.HEIC", b"old");
    src
}

/// What a sync writes: every new file and a changed file's new copy (the temporary file is that
/// copy). A file already backed up adds nothing unless it is copied again.
#[test]
fn the_bytes_a_sync_will_write_are_new_files_plus_changed_files_new_copies() {
    let e = env();
    let mut src = space_fixture(&e);
    let (plan, _) = do_scan(&mut src, &e);
    let items = pending(&plan);
    assert_eq!(items.len(), 3);
    assert_eq!(crate::space::bytes_to_write(&plan, &items), 1000 + 50 + 300);
    // Copying the backed-up file again writes a whole new copy of it.
    let mut again = items.clone();
    again.push(SyncItem {
        entry: entry_of(&plan, "DCIM/SAME.HEIC"),
        mode: CopyMode::Replace,
    });
    assert_eq!(
        crate::space::bytes_to_write(&plan, &again),
        1000 + 50 + 300 + 4
    );
    // The same file in the ordinary mode has nothing to copy; nothing at all is nothing.
    let same_only = [SyncItem {
        entry: entry_of(&plan, "DCIM/SAME.HEIC"),
        mode: CopyMode::NoReplace,
    }];
    assert_eq!(crate::space::bytes_to_write(&plan, &same_only), 0);
    assert_eq!(crate::space::bytes_to_write(&plan, &[]), 0);
}

/// A sync that won't fit, with the margin kept, is refused before anything is written; one that
/// just fits goes ahead. A disk that can't be asked doesn't stop it.
#[test]
fn a_sync_that_will_not_fit_is_refused_before_anything_is_copied() {
    let e = env();
    let mut src = space_fixture(&e);
    let (plan, _) = do_scan(&mut src, &e);
    let items = pending(&plan);
    let before = files_under(&e.root);
    let need = 1350;
    // 1 GiB must stay free: one byte short is refused.
    {
        let _g = fake_space::set(Some(DiskSpace {
            total: 50 * GIB,
            free: GIB + need - 1,
        }));
        let (r, ev) = do_sync(&mut src, &plan, &e, &items, &SyncControl::default());
        let err = r.unwrap_err();
        assert!(matches!(err, SyncError::NotEnoughSpace(_)), "{err}");
        let text = err.to_string();
        assert!(text.contains("This sync needs 1.3 KB"), "{text}");
        assert!(text.contains("Nothing was copied"), "{text}");
        assert!(ev.is_empty(), "{ev:?}");
        assert_eq!(files_under(&e.root), before, "nothing was written");
    }
    // Exactly enough with the margin: it runs.
    {
        let _g = fake_space::set(Some(DiskSpace {
            total: 50 * GIB,
            free: GIB + need,
        }));
        let (r, ev) = do_sync(&mut src, &plan, &e, &items, &SyncControl::default());
        assert_eq!(r.unwrap().copied, 3, "{ev:?}");
    }
    // On a big disk the margin is 2 % of it: 2 % of 500 GiB is 10 GiB.
    let e = env();
    let mut src = space_fixture(&e);
    let (plan, _) = do_scan(&mut src, &e);
    let _g = fake_space::set(Some(DiskSpace {
        total: 500 * GIB,
        free: 5 * GIB,
    }));
    let (r, _) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert!(matches!(r, Err(SyncError::NotEnoughSpace(_))));
    // No figure for the disk: not refused.
    drop(_g);
    let _g = fake_space::set(None);
    let (r, _) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().copied, 3);
}

/// A sync that only moves files (nothing to copy) needs no room at all.
#[test]
fn a_sync_with_nothing_to_copy_needs_no_room() {
    let e = env();
    let mut src = space_fixture(&e);
    let (plan, _) = do_scan(&mut src, &e);
    let _g = fake_space::set(Some(DiskSpace {
        total: 100 * GIB,
        free: 0,
    }));
    let (r, _) = do_sync(&mut src, &plan, &e, &[], &SyncControl::default());
    assert_eq!(r.unwrap().copied, 0);
}

/// The disk fills up part-way through a file: that file's temporary copy is removed, the sync
/// stops there and says the disk is full, and nothing already in the backup is touched.
#[test]
fn a_full_disk_mid_copy_removes_the_temporary_file_and_stops_the_sync() {
    let e = env();
    let mut src = phone(vec![
        MemObject::new("1", "DCIM/A_BIG.MOV", &[7u8; 300_000]),
        MemObject::new("2", "DCIM/B_SMALL.HEIC", b"small"),
    ]);
    claim(&e, &key_of(&src));
    put(&e.root, "DCIM/ALREADY.HEIC", b"was here");
    let (plan, _) = do_scan(&mut src, &e);
    let _g = fake_space::set(None);
    let _full = crate::sync::fail_writes_after(100);
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    let r = r.unwrap();
    assert!(r.disk_full, "{r:?}");
    assert_eq!((r.copied, r.failed), (0, 1), "{ev:?}");
    let said = failures(&ev);
    assert!(
        said.len() == 1 && said[0].contains("disk is full") && said[0].contains("unfinished copy"),
        "{said:?}"
    );
    // The temporary file is gone, the big file is not in the backup, the second never started,
    // and what was there is untouched.
    assert_eq!(
        files_under(&e.root),
        set(&["DCIM/ALREADY.HEIC"]),
        "no temporary file is left behind"
    );
    assert_eq!(
        fs::read(e.root.join("DCIM/ALREADY.HEIC")).unwrap(),
        b"was here"
    );
    // No record of a copy in progress is left either.
    assert!(e.state.recover_interrupted(&known(&e)).unwrap().is_none());
}

/// The OS's own disk-full codes are recognised: Windows' `ERROR_DISK_FULL` and Unix's `ENOSPC`.
#[test]
fn the_systems_disk_full_errors_are_recognised() {
    let full = std::io::Error::from_raw_os_error(if cfg!(windows) { 112 } else { 28 });
    assert!(crate::sync::is_disk_full_for_tests(&full));
    assert!(!crate::sync::is_disk_full_for_tests(
        &std::io::Error::other("something else")
    ));
}

/// 39 is a disk-full code on Windows only; on Linux it is `ENOTEMPTY`.
#[cfg(not(windows))]
#[test]
fn a_windows_only_disk_full_code_is_not_one_elsewhere() {
    assert!(!crate::sync::is_disk_full_for_tests(
        &std::io::Error::from_raw_os_error(39)
    ));
}

/// If an unfinished copy can't be removed after a Stop, its crash record stays, so the next
/// start removes it; otherwise a later sync would keep it forever as a partial copy.
#[test]
fn a_temp_that_cannot_be_removed_keeps_its_crash_record() {
    let e = env();
    let ctl = SyncControl::default();
    let mut obj = MemObject::new("1", "DCIM/IMG_0001.MOV", &[1u8; 100_000]);
    obj.trip = Some((10_000, ctl.skip_current.clone()));
    let mut src = phone(vec![obj]);
    let (plan, _) = do_scan(&mut src, &e);
    ctl.skip_entry
        .store(entry_of(&plan, "DCIM/IMG_0001.MOV"), Ordering::SeqCst);
    {
        let _fail = crate::sync::fail_temp_removal();
        let (r, ev) = do_sync(&mut src, &plan, &e, &pending(&plan), &ctl);
        assert_eq!(r.unwrap().skipped, 1, "{ev:?}");
    }
    assert_eq!(
        files_under(&e.root).len(),
        1,
        "the partial copy is still there"
    );
    // The next start finds the record and removes the partial copy.
    let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
    assert!(got.temp_removed, "{}", got.note);
    assert!(files_under(&e.root).is_empty());
    assert!(e.state.recover_interrupted(&known(&e)).unwrap().is_none());
}

/// A file that is already at the temp's name when the copy tries to create it (someone else's,
/// or a whole copy kept earlier) is never removed by this copy.
#[test]
fn a_file_at_the_temps_name_that_this_copy_did_not_create_is_left_alone() {
    let e = env();
    let mut src = phone(vec![MemObject::new("1", "DCIM/IMG.MOV", b"from the phone")]);
    let (plan, _) = do_scan(&mut src, &e);
    let _hook = crate::sync::before_temp_create(|temp| {
        fs::write(temp, b"not ours").unwrap();
    });
    let (r, ev) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().failed, 1, "{ev:?}");
    let temp = e
        .root
        .join(format!("DCIM/IMG.MOV{}", crate::paths::TEMP_SUFFIX));
    assert_eq!(fs::read(&temp).unwrap(), b"not ours");
    assert!(!e.root.join("DCIM/IMG.MOV").exists());
}

mod folder_state;
mod jpgs;
mod mem_store;
