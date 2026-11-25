//! JPG copies of HEIC photos in a sync (DESIGN.md section 7.6), end to end: a fake device, a
//! real backup folder, and a stand-in for the system's decoder (so these run on every machine).
//! Each test's doc comment says which rule it guards.

use super::*;
use crate::heic::DecodeError;
use crate::heic::fake_decoder;
use crate::heic::made::{MADE_NAME, MadeEntry, MadeList, facts_of, to_ms};
use crate::jpgs::{JpgEvent, JpgReport, backfill, forget_failures, make_jpegs, retry_failures};
use crate::paths::{JPG_TEMP_SUFFIX, SafeRelPath};
use crate::platform::{DiskSpace, fake_space};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::AtomicBool;

const A: &[u8] = include_bytes!("../heic/fixtures/plain.heic");
const B: &[u8] = include_bytes!("../heic/fixtures/rotated.heic");
const C: &[u8] = include_bytes!("../heic/fixtures/mirrored.heic");
const D: &[u8] = include_bytes!("../heic/fixtures/icc.heic");

fn jopts(mode: DeletedFiles) -> ScanOptions {
    ScanOptions {
        settings: DeviceSettings {
            deleted_files: mode,
            jpeg_copies: true,
            archive_layout: ArchiveLayout::KeepFolders,
            ..open_settings()
        },
        weak_match: false,
    }
}

/// The files under the backup folder, without the made list.
fn visible(dir: &Path) -> BTreeSet<String> {
    let mut f = files_under(dir);
    f.remove(MADE_NAME);
    f
}

struct Round {
    copy: SyncReport,
    copy_events: Vec<SyncEvent>,
    archive: Option<Result<ArchiveReport, SyncError>>,
    jpg: JpgReport,
    jpg_events: Vec<JpgEvent>,
}

/// A whole sync as the window does it: scan, copy, move what left the device, make the JPGs.
fn round(src: &mut MemSource, e: &Env, mode: DeletedFiles, confirmed: bool) -> (ScanPlan, Round) {
    let (plan, _) = do_scan_with(src, e, &jopts(mode));
    let r = finish(src, &plan, e, confirmed, &AtomicBool::new(false));
    (plan, r)
}

fn finish(
    src: &mut MemSource,
    plan: &ScanPlan,
    e: &Env,
    confirmed: bool,
    cancel: &AtomicBool,
) -> Round {
    let (copy, copy_events) = do_sync_as(
        src,
        plan,
        e,
        &pending(plan),
        &SyncControl::default(),
        confirmed,
    );
    let archive =
        (!plan.removed.is_empty()).then(|| archive_removed(plan, STAMP, confirmed, &mut |_| {}));
    let mut jpg_events = Vec::new();
    let jpg = make_jpegs(plan, cancel, &mut |ev| jpg_events.push(ev)).unwrap();
    Round {
        copy: copy.unwrap(),
        copy_events,
        archive,
        jpg,
        jpg_events,
    }
}

/// The entry the app leaves for the JPG that is at `jpg` under `root` now (its size and
/// modified time as they are), made from the HEIC at `heic`.
fn entry_for(root: &Path, jpg: &str, heic: &str, heic_size: u64) -> MadeEntry {
    let (size, modified) = facts_of(&root.join(jpg)).expect("the JPG is there");
    MadeEntry {
        jpg: jpg.into(),
        heic: heic.into(),
        heic_size,
        heic_modified_ms: 1,
        made: "x".into(),
        jpg_size: Some(size),
        jpg_modified_ms: Some(modified),
    }
}

fn made_of(e: &Env) -> MadeList {
    MadeList::load(&fs::canonicalize(&e.root).unwrap())
}

fn dims(path: &Path) -> (u16, u16) {
    let bytes = fs::read(path).unwrap();
    let mut d = jpeg_decoder::Decoder::new(&bytes[..]);
    d.decode().unwrap();
    let i = d.info().unwrap();
    (i.width, i.height)
}

fn heic(id: &str, path: &str, data: &[u8]) -> MemObject {
    MemObject::new(id, path, data)
}

/// Another photo besides, so the device never lists as empty and the "few files left" caution
/// stays out of the way.
fn filler(n: usize) -> Vec<MemObject> {
    (0..n)
        .map(|i| {
            MemObject::new(
                &format!("f{i}"),
                &format!("DCIM/F{i}.MOV"),
                format!("video {i}").as_bytes(),
            )
        })
        .collect()
}

/// A HEIC on the device gets a JPG beside it in the backup, with the HEIC's date, recorded in
/// the made list; a second sync finds it up to date. With the setting off nothing is made.
#[test]
fn a_heic_gets_a_jpg() {
    let _d = fake_decoder::working();
    let e = env();
    let mut src = phone(vec![
        heic("1", "DCIM/100APPLE/IMG_0001.HEIC", A),
        heic("2", "DCIM/100APPLE/IMG_0002.heic", B),
    ]);
    src.objects[0].modified = Some(at(10));
    let (plan, r) = round(&mut src, &e, DeletedFiles::Archive, false);
    assert!(plan.jpg.enabled);
    assert_eq!(plan.jpg.to_make, 2);
    assert_eq!(summary_make(&plan), 2);
    assert_eq!(
        (r.copy.copied, r.copy.failed),
        (2, 0),
        "{:?}",
        r.copy_events
    );
    assert_eq!((r.jpg.made, r.jpg.failed), (2, 0), "{:?}", r.jpg_events);
    assert_eq!(
        visible(&e.root),
        set(&[
            "DCIM/100APPLE/IMG_0001.HEIC",
            "DCIM/100APPLE/IMG_0001.JPG",
            "DCIM/100APPLE/IMG_0002.heic",
            // the case of the HEIC's extension
            "DCIM/100APPLE/IMG_0002.jpg",
        ])
    );
    assert_eq!(dims(&e.root.join("DCIM/100APPLE/IMG_0001.JPG")), (64, 48));
    // The JPG has the HEIC's date, and the made list knows it.
    let heic_time = fs::metadata(e.root.join("DCIM/100APPLE/IMG_0001.HEIC"))
        .unwrap()
        .modified()
        .unwrap();
    let jpg_time = fs::metadata(e.root.join("DCIM/100APPLE/IMG_0001.JPG"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(heic_time, jpg_time);
    let made = made_of(&e);
    let m = made.by_jpg("DCIM/100APPLE/IMG_0001.JPG").unwrap();
    assert_eq!(m.heic, "DCIM/100APPLE/IMG_0001.HEIC");
    assert_eq!(m.heic_size, A.len() as u64);
    // The JPGs are not listed as deleted from the device and the next sync has nothing to do.
    let (plan2, r2) = round(&mut src, &e, DeletedFiles::Archive, false);
    assert!(plan2.removed.is_empty(), "{:?}", removed_paths(&plan2));
    assert_eq!(plan2.jpg.to_make, 0);
    assert_eq!((r2.jpg.made, r2.jpg.up_to_date), (0, 2));
    // The summary counts neither the made JPGs as backup files outside the plan nor as part of
    // "in the backup folder": both are about the device's files.
    let sum = plan2.summary(plan2.backup_files);
    assert_eq!(plan2.made_jpgs, 2);
    assert_eq!(sum.outside, 0, "{:?}", plan2.outside);
    assert_eq!(sum.backup_files, plan2.backup_files - 2);
    assert_eq!(sum.backup_files, sum.device_files);
    assert_eq!(visible(&e.root).len(), 4);

    // With the setting off, nothing is made.
    let e2 = env();
    let mut src2 = phone(vec![heic("1", "DCIM/IMG_0001.HEIC", A)]);
    let off = ScanOptions {
        settings: DeviceSettings {
            jpeg_copies: false,
            ..jopts(DeletedFiles::Archive).settings
        },
        weak_match: false,
    };
    let (plan, _) = do_scan_with(&mut src2, &e2, &off);
    assert!(!plan.jpg.enabled && plan.jpg.candidates.is_empty());
    let r = finish(&mut src2, &plan, &e2, false, &AtomicBool::new(false));
    assert_eq!(r.jpg, JpgReport::default());
    assert_eq!(visible(&e2.root), set(&["DCIM/IMG_0001.HEIC"]));
}

fn summary_make(plan: &ScanPlan) -> usize {
    plan.summary(0).jpg_to_make
}

/// If the device has its own JPG of a HEIC (same folder, same name, `.jpg` or `.jpeg` in any
/// case), nothing is made. The device's own files are never second-guessed.
#[test]
fn a_heic_with_a_device_jpg_gets_none() {
    let _d = fake_decoder::working();
    let e = env();
    let mut src = phone(vec![
        heic("1", "DCIM/IMG_0001.HEIC", A),
        heic("2", "DCIM/IMG_0001.JPG", b"device jpg"),
        heic("3", "DCIM/IMG_0002.HEIC", B),
        heic("4", "DCIM/IMG_0002.JpEg", b"device jpeg"),
        heic("5", "DCIM/IMG_0003.HEIC", C),
        // Same name in another folder is not the same photo.
        heic("6", "DCIM/OTHER/IMG_0003.JPG", b"elsewhere"),
    ]);
    let (plan, r) = round(&mut src, &e, DeletedFiles::Archive, false);
    assert_eq!(plan.jpg.device_has_jpg, 2);
    assert_eq!(plan.jpg.to_make, 1);
    assert_eq!((r.jpg.made, r.jpg.failed), (1, 0), "{:?}", r.jpg_events);
    assert_eq!(
        visible(&e.root),
        set(&[
            "DCIM/IMG_0001.HEIC",
            "DCIM/IMG_0001.JPG",
            "DCIM/IMG_0002.HEIC",
            "DCIM/IMG_0002.JpEg",
            "DCIM/IMG_0003.HEIC",
            "DCIM/IMG_0003.JPG",
            "DCIM/OTHER/IMG_0003.JPG",
        ])
    );
    assert_eq!(
        fs::read(e.root.join("DCIM/IMG_0001.JPG")).unwrap(),
        b"device jpg"
    );
    assert_eq!(made_of(&e).len(), 1);
    assert!(made_of(&e).made_by_us("DCIM/IMG_0003.JPG"));
}

/// A JPG of that name that the app didn't make is never overwritten: nothing is made and the
/// reason is recorded. A taken name is not a sticky failure: once the other file has gone (here
/// the user moves it away), the JPG is made without "Try
/// again".
#[test]
fn a_taken_name_is_not_overwritten_and_not_retried() {
    let calls = Rc::new(Cell::new(0));
    let c = calls.clone();
    let _d = fake_decoder::set(move |p| {
        c.set(c.get() + 1);
        Ok(fake_decoder::picture_for(p))
    });
    let e = env();
    let mut src = phone(vec![heic("1", "DCIM/IMG_0001.HEIC", A)]);
    claim(&e, &key_of(&src));
    put(&e.root, "DCIM/IMG_0001.JPG", b"somebody else's file");
    let (plan, r) = round(&mut src, &e, DeletedFiles::Archive, false);
    // The scan already knows the name is taken.
    assert_eq!(plan.jpg.to_make, 0);
    assert_eq!((r.copy.copied, r.jpg.made, r.jpg.failed), (1, 0, 1));
    assert_eq!(calls.get(), 0, "nothing was decoded");
    assert_eq!(
        fs::read(e.root.join("DCIM/IMG_0001.JPG")).unwrap(),
        b"somebody else's file"
    );
    let made = made_of(&e);
    assert!(made.is_empty());
    let f = made.failed_for("DCIM/IMG_0001.HEIC").unwrap();
    assert!(f.reason.contains("already there"), "{}", f.reason);
    assert_eq!(f.heic_size, A.len() as u64);
    assert_eq!(visible(&e.root).len(), 2);
    // While the other file is still there, the next scan says the same again, and nothing is
    // decoded: use LeaveInPlace so it stays.
    let (_, r_same) = round(&mut src, &e, DeletedFiles::LeaveInPlace, false);
    assert_eq!(
        (
            r_same.jpg.made,
            r_same.jpg.failed,
            r_same.jpg.already_failed
        ),
        (0, 0, 1)
    );
    assert_eq!(calls.get(), 0, "nothing decoded while the name is taken");
    // A JPG beside a HEIC that is still on the device is never called deleted from it (the
    // user may have edited their own), so Archive leaves it and the name stays taken.
    let (plan3, r2) = round(&mut src, &e, DeletedFiles::Archive, false);
    assert!(plan3.removed.is_empty());
    assert_eq!(
        (r2.jpg.made, r2.jpg.failed, r2.jpg.already_failed),
        (0, 0, 1)
    );
    assert_eq!(
        fs::read(e.root.join("DCIM/IMG_0001.JPG")).unwrap(),
        b"somebody else's file"
    );
    // Once the user has moved it away themselves, the name is free and the JPG is made.
    fs::remove_file(e.root.join("DCIM/IMG_0001.JPG")).unwrap();
    let (_, r3) = round(&mut src, &e, DeletedFiles::Archive, false);
    assert_eq!(
        (r3.jpg.made, r3.jpg.failed, r3.jpg.already_failed),
        (1, 0, 0)
    );
    assert!(made_of(&e).failed_for("DCIM/IMG_0001.HEIC").is_none());
    assert_eq!(calls.get(), 1, "decoded once, when the name was free");
    assert!(e.root.join("DCIM/IMG_0001.JPG").exists());
}

/// The app's own JPG and a second HEIC of the same name (`IMG.HEIC` and `IMG.HEIF`) don't clash
/// destructively: the first gets the JPG, the second is recorded.
#[test]
fn two_heics_with_one_jpg_name_make_only_one() {
    let _d = fake_decoder::working();
    let e = env();
    let mut src = phone(vec![
        heic("1", "DCIM/IMG_0001.HEIC", A),
        heic("2", "DCIM/IMG_0001.HEIF", B),
    ]);
    let (_, r) = round(&mut src, &e, DeletedFiles::Archive, false);
    assert_eq!((r.jpg.made, r.jpg.failed), (1, 1), "{:?}", r.jpg_events);
    assert_eq!(visible(&e.root).len(), 3);
}

/// When the device gets a JPG of its own where the app made one, the device's wins: the made one
/// goes the way a changed file's old copy goes, the device's is copied in, and the entry leaves
/// the list. It doesn't count in the cautions.
#[test]
fn the_devices_own_jpg_wins_over_a_made_one() {
    for mode in [
        DeletedFiles::Archive,
        DeletedFiles::LeaveInPlace,
        DeletedFiles::RecycleBin,
    ] {
        let _d = fake_decoder::working();
        let e = env();
        let bin = e.base.join("bin");
        let _g = fake_bin::set(Fake::Into(bin.clone()));
        let mut src = phone(vec![heic("1", "DCIM/IMG_0001.HEIC", A)]);
        src.objects.extend(filler(2));
        let (_, r) = round(&mut src, &e, mode, false);
        assert_eq!(r.jpg.made, 1, "{mode:?}");
        let made_bytes = fs::read(e.root.join("DCIM/IMG_0001.JPG")).unwrap();
        // The device now has a JPG of its own.
        src.objects
            .push(heic("9", "DCIM/IMG_0001.JPG", b"the device's own jpg"));
        let (plan, _) = do_scan_with(&mut src, &e, &jopts(mode));
        let i = entry_of(&plan, "DCIM/IMG_0001.JPG");
        assert!(plan.entries[i].made_jpg, "{mode:?}");
        assert_eq!(plan.entries[i].status, Status::Mismatch);
        assert!(plan.cautions.is_empty(), "{mode:?}: {:?}", plan.cautions);
        assert!(plan.removed.is_empty(), "{mode:?}");
        let r = finish(&mut src, &plan, &e, false, &AtomicBool::new(false));
        assert_eq!(
            (r.copy.copied, r.copy.replaced, r.copy.failed),
            (1, 1, 0),
            "{mode:?}: {:?}",
            r.copy_events
        );
        assert_eq!(
            fs::read(e.root.join("DCIM/IMG_0001.JPG")).unwrap(),
            b"the device's own jpg",
            "{mode:?}"
        );
        // The made copy is where an old copy goes.
        match mode {
            DeletedFiles::Archive => assert_eq!(
                fs::read(
                    e.root
                        .join(format!("{ARCHIVE_DIR}/{STAMP}/DCIM/IMG_0001.JPG"))
                )
                .unwrap(),
                made_bytes
            ),
            DeletedFiles::LeaveInPlace => assert_eq!(
                fs::read(e.root.join("DCIM/IMG_0001 (old 2026-01-02 03-04).JPG")).unwrap(),
                made_bytes
            ),
            DeletedFiles::RecycleBin => {
                assert_eq!(fs::read(bin.join("IMG_0001.JPG")).unwrap(), made_bytes)
            }
        }
        assert!(made_of(&e).is_empty(), "{mode:?}: the entry left the list");
        // And no JPG is made again over the device's.
        assert_eq!(r.jpg.made, 0, "{mode:?}");
        // A later scan sees the device's file as backed up, and the aside copy as the app's.
        let (plan, _) = do_scan_with(&mut src, &e, &jopts(mode));
        assert!(
            plan.removed.is_empty(),
            "{mode:?}: {:?}",
            removed_paths(&plan)
        );
    }
}

/// If the device's own JPG is already what's in the backup (same size, so it was backed up
/// without anything moving), the list stops calling it the app's, and nothing moves.
#[test]
fn a_made_entry_for_a_file_the_device_has_is_dropped_without_moving_it() {
    let _d = fake_decoder::working();
    let e = env();
    let mut src = phone(vec![
        heic("1", "DCIM/IMG_0001.HEIC", A),
        heic("2", "DCIM/IMG_0001.JPG", b"same bytes"),
    ]);
    claim(&e, &key_of(&src));
    put(&e.root, "DCIM/IMG_0001.HEIC", A);
    put(&e.root, "DCIM/IMG_0001.JPG", b"same bytes");
    let root = fs::canonicalize(&e.root).unwrap();
    let mut l = MadeList::default();
    l.insert(entry_for(
        &root,
        "DCIM/IMG_0001.JPG",
        "DCIM/IMG_0001.HEIC",
        A.len() as u64,
    ));
    l.save(&root).unwrap();
    let (plan, r) = round(&mut src, &e, DeletedFiles::Archive, false);
    assert!(plan.entries[entry_of(&plan, "DCIM/IMG_0001.JPG")].made_jpg);
    assert_eq!(r.copy.copied, 0);
    assert!(made_of(&e).is_empty());
    assert_eq!(
        fs::read(e.root.join("DCIM/IMG_0001.JPG")).unwrap(),
        b"same bytes"
    );
    assert_eq!(visible(&e.root).len(), 2);
}

/// Sets a backup up as a sync leaves it: a HEIC and its JPG, made by the app, `n` pairs.
fn backup_with_made(e: &Env, src: &MemSource, n: usize) {
    claim(e, &key_of(src));
    let root = fs::canonicalize(&e.root).unwrap();
    let mut l = MadeList::default();
    for i in 0..n {
        let heic = format!("DCIM/M{i}.HEIC");
        let jpg = format!("DCIM/M{i}.JPG");
        put(&root, &heic, A);
        put(&root, &jpg, format!("jpg {i}").as_bytes());
        l.insert(entry_for(&root, &jpg, &heic, A.len() as u64));
    }
    l.save(&root).unwrap();
}

/// A made JPG never counts as deleted from the device, and never counts in the cautions: with
/// thirty of them, none of the "large batch" or "few files on the device" reasons turn up.
#[test]
fn made_jpgs_never_count_as_deleted_or_in_the_cautions() {
    for mode in [
        DeletedFiles::Archive,
        DeletedFiles::LeaveInPlace,
        DeletedFiles::RecycleBin,
    ] {
        let _d = fake_decoder::working();
        let e = env();
        let mut objects: Vec<MemObject> = (0..30)
            .map(|i| heic(&format!("{i}"), &format!("DCIM/M{i}.HEIC"), A))
            .collect();
        objects.push(heic("x", "DCIM/KEEP.MOV", b"video"));
        let mut src = phone(objects);
        backup_with_made(&e, &src, 30);
        let (plan, _) = do_scan_with(&mut src, &e, &jopts(mode));
        assert!(
            plan.removed.is_empty(),
            "{mode:?}: {:?}",
            removed_paths(&plan)
        );
        assert!(plan.cautions.is_empty(), "{mode:?}: {:?}", plan.cautions);
        assert_eq!(plan.outside.len(), 30, "{mode:?}");
        assert!(plan.outside.iter().all(|p| p.ends_with(".JPG")));
        // Not even when their HEICs are gone from the backup: still not "deleted".
        for i in 0..30 {
            fs::remove_file(e.root.join(format!("DCIM/M{i}.HEIC"))).unwrap();
        }
        let (plan, _) = do_scan_with(&mut src, &e, &jopts(mode));
        assert!(plan.removed.is_empty(), "{mode:?}");
        assert!(
            !plan
                .cautions
                .iter()
                .any(|c| matches!(c, Caution::LargeBatch { .. })),
            "{mode:?}"
        );
    }
}

/// A made JPG the user edited (so the list no longer vouches for it), or whose made list is gone,
/// is never called deleted while its HEIC is still on the device, and a sync doesn't touch it.
#[test]
fn an_edited_or_unlisted_jpg_beside_a_device_heic_is_not_deleted() {
    for damage in ["edited", "list gone"] {
        let _d = fake_decoder::working();
        let e = env();
        let mut src = synced_pair(&e, DeletedFiles::Archive);
        let jpg = e.root.join("DCIM/100APPLE/IMG_0001.JPG");
        assert!(jpg.is_file());
        if damage == "edited" {
            fs::write(&jpg, b"edited by the user, longer than before").unwrap();
        } else {
            fs::remove_file(e.root.join(MADE_NAME)).unwrap();
        }
        let before = fs::read(&jpg).unwrap();
        let (plan, r) = round(&mut src, &e, DeletedFiles::Archive, true);
        assert!(
            plan.removed.is_empty(),
            "{damage}: {:?}",
            removed_paths(&plan)
        );
        assert!(
            plan.outside
                .iter()
                .any(|p| p == "DCIM/100APPLE/IMG_0001.JPG"),
            "{damage}"
        );
        assert!(r.archive.is_none(), "{damage}");
        assert_eq!(fs::read(&jpg).unwrap(), before, "{damage}: not replaced");
        assert_eq!(r.jpg.made, 0, "{damage}");
    }
}

/// Makes the backup of one HEIC and its JPG through a real sync, then returns the device.
fn synced_pair(e: &Env, mode: DeletedFiles) -> MemSource {
    let mut src = phone(vec![heic("1", "DCIM/100APPLE/IMG_0001.HEIC", A)]);
    src.objects.extend(filler(3));
    let (_, r) = round(&mut src, e, mode, false);
    assert_eq!((r.copy.copied, r.jpg.made), (4, 1), "{:?}", r.jpg_events);
    src
}

/// A HEIC deleted from the device takes its JPG with it the way the HEIC goes: into the same
/// dated archive folder (with the same sub-path), into the bin, or left where it is. The JPG is
/// never "deleted" on its own, and the list forgets it once it has gone.
#[test]
fn a_deleted_heic_takes_its_jpg_with_it_in_each_mode() {
    let _d = fake_decoder::working();
    // Archive.
    let e = env();
    let mut src = synced_pair(&e, DeletedFiles::Archive);
    let jpg_bytes = fs::read(e.root.join("DCIM/100APPLE/IMG_0001.JPG")).unwrap();
    src.objects.remove(0);
    let (plan, r) = round(&mut src, &e, DeletedFiles::Archive, false);
    assert_eq!(removed_paths(&plan), ["DCIM/100APPLE/IMG_0001.HEIC"]);
    assert!(plan.removed[0].jpg.is_some());
    let a = r.archive.unwrap().unwrap();
    assert_eq!((a.moved, a.failed, a.jpgs_moved), (1, 0, 1));
    let dir = format!("{ARCHIVE_DIR}/{STAMP}/DCIM/100APPLE");
    assert_eq!(
        fs::read(e.root.join(format!("{dir}/IMG_0001.JPG"))).unwrap(),
        jpg_bytes
    );
    assert_eq!(
        fs::read(e.root.join(format!("{dir}/IMG_0001.HEIC"))).unwrap(),
        A
    );
    assert!(!e.root.join("DCIM/100APPLE/IMG_0001.JPG").exists());
    assert!(made_of(&e).is_empty());

    // Recycle bin.
    let e = env();
    let bin = e.base.join("bin");
    let _g = fake_bin::set(Fake::Into(bin.clone()));
    let mut src = synced_pair(&e, DeletedFiles::RecycleBin);
    src.objects.remove(0);
    let (_, r) = round(&mut src, &e, DeletedFiles::RecycleBin, false);
    let a = r.archive.unwrap().unwrap();
    assert_eq!((a.moved, a.failed, a.jpgs_moved), (1, 0, 1));
    assert_eq!(files_under(&bin), set(&["IMG_0001.HEIC", "IMG_0001.JPG"]));
    assert!(
        !e.root.join("DCIM/100APPLE").exists()
            || visible(&e.root).iter().all(|p| !p.contains("IMG_0001"))
    );
    assert!(made_of(&e).is_empty());

    // Leave in place: both stay, and the JPG is still the app's (it is not "deleted" either).
    let e = env();
    let mut src = synced_pair(&e, DeletedFiles::LeaveInPlace);
    src.objects.remove(0);
    let (plan, r) = round(&mut src, &e, DeletedFiles::LeaveInPlace, false);
    assert_eq!(removed_paths(&plan), ["DCIM/100APPLE/IMG_0001.HEIC"]);
    let a = r.archive.unwrap().unwrap();
    assert_eq!((a.moved, a.jpgs_moved), (0, 0));
    assert!(e.root.join("DCIM/100APPLE/IMG_0001.HEIC").exists());
    assert!(e.root.join("DCIM/100APPLE/IMG_0001.JPG").exists());
    assert!(made_of(&e).made_by_us("DCIM/100APPLE/IMG_0001.JPG"));
}

/// A changed HEIC: the old JPG goes the way the old HEIC copy goes, and a new JPG is made from
/// the new HEIC. Nothing is overwritten.
#[test]
fn a_changed_heic_replaces_its_jpg_in_each_mode() {
    for mode in [
        DeletedFiles::Archive,
        DeletedFiles::LeaveInPlace,
        DeletedFiles::RecycleBin,
    ] {
        let _d = fake_decoder::working();
        let e = env();
        let bin = e.base.join("bin");
        let _g = fake_bin::set(Fake::Into(bin.clone()));
        let mut src = synced_pair(&e, mode);
        let old_jpg = fs::read(e.root.join("DCIM/100APPLE/IMG_0001.JPG")).unwrap();
        src.objects[0].data = B.to_vec();
        src.objects[0].reported_size = Some(B.len() as u64);
        let (plan, r) = round(&mut src, &e, mode, false);
        let entry = &plan.entries[entry_of(&plan, "DCIM/100APPLE/IMG_0001.HEIC")];
        assert!(entry.old_jpg.is_some(), "{mode:?}");
        assert!(plan.removed.is_empty(), "{mode:?}");
        assert_eq!(
            (r.copy.copied, r.copy.replaced, r.copy.failed),
            (1, 1, 0),
            "{mode:?}: {:?}",
            r.copy_events
        );
        assert_eq!(r.jpg.made, 1, "{mode:?}: {:?}", r.jpg_events);
        assert_eq!(
            fs::read(e.root.join("DCIM/100APPLE/IMG_0001.HEIC")).unwrap(),
            B
        );
        // The new JPG is in place, made from the new HEIC.
        let made = made_of(&e);
        let m = made.by_jpg("DCIM/100APPLE/IMG_0001.JPG").unwrap();
        assert_eq!(m.heic_size, B.len() as u64, "{mode:?}");
        assert_eq!(made.len(), 1, "{mode:?}");
        // The old one is where the old HEIC copy went.
        match mode {
            DeletedFiles::Archive => {
                let dir = format!("{ARCHIVE_DIR}/{STAMP}/DCIM/100APPLE");
                assert_eq!(
                    fs::read(e.root.join(format!("{dir}/IMG_0001.JPG"))).unwrap(),
                    old_jpg
                );
                assert_eq!(
                    fs::read(e.root.join(format!("{dir}/IMG_0001.HEIC"))).unwrap(),
                    A
                );
            }
            DeletedFiles::LeaveInPlace => {
                let dir = "DCIM/100APPLE";
                assert_eq!(
                    fs::read(
                        e.root
                            .join(format!("{dir}/IMG_0001 (old 2026-01-02 03-04).JPG"))
                    )
                    .unwrap(),
                    old_jpg
                );
                assert_eq!(
                    fs::read(
                        e.root
                            .join(format!("{dir}/IMG_0001 (old 2026-01-02 03-04).HEIC"))
                    )
                    .unwrap(),
                    A
                );
            }
            DeletedFiles::RecycleBin => {
                assert_eq!(files_under(&bin), set(&["IMG_0001.HEIC", "IMG_0001.JPG"]));
                assert_eq!(fs::read(bin.join("IMG_0001.JPG")).unwrap(), old_jpg);
            }
        }
        // A rescan finds nothing deleted and nothing to do; the aside copies are the app's.
        let (plan, _) = do_scan_with(&mut src, &e, &jopts(mode));
        assert!(
            plan.removed.is_empty(),
            "{mode:?}: {:?}",
            removed_paths(&plan)
        );
        assert_eq!(plan.jpg.to_make, 0, "{mode:?}");
    }
}

/// The cautions gate the JPG's follow-moves as they gate the HEIC's: without the OK, a deleted
/// HEIC's JPG and a changed HEIC's old JPG stay exactly where they are, in every mode.
#[test]
fn cautions_block_the_jpg_follow_moves() {
    for mode in [
        DeletedFiles::Archive,
        DeletedFiles::LeaveInPlace,
        DeletedFiles::RecycleBin,
    ] {
        let _d = fake_decoder::working();
        let e = env();
        let bin = e.base.join("bin");
        let _g = fake_bin::set(Fake::Into(bin.clone()));
        // A folder that already has files and no marker: the first-sync caution.
        put(&e.root, "DCIM/DEL.HEIC", A);
        put(&e.root, "DCIM/DEL.JPG", b"made");
        put(&e.root, "DCIM/CHG.HEIC", A);
        put(&e.root, "DCIM/CHG.JPG", b"made too");
        let mut l = MadeList::default();
        for n in ["DEL", "CHG"] {
            l.insert(entry_for(
                &e.root,
                &format!("DCIM/{n}.JPG"),
                &format!("DCIM/{n}.HEIC"),
                A.len() as u64,
            ));
        }
        l.save(&fs::canonicalize(&e.root).unwrap()).unwrap();
        let before = visible(&e.root);
        let mut src = phone(vec![heic("1", "DCIM/CHG.HEIC", B)]);
        let (plan, _) = do_scan_with(&mut src, &e, &jopts(mode));
        assert!(
            has_caution(&plan, |c| matches!(
                c,
                Caution::FirstSyncIntoFilledFolder { .. }
            )),
            "{mode:?}: {:?}",
            plan.cautions
        );
        assert_eq!(removed_paths(&plan), ["DCIM/DEL.HEIC"], "{mode:?}");
        let r = finish(&mut src, &plan, &e, false, &AtomicBool::new(false));
        assert_eq!(r.copy.replaced, 0, "{mode:?}");
        assert_eq!(r.copy.failed, 1, "{mode:?}: {:?}", r.copy_events);
        if mode != DeletedFiles::LeaveInPlace {
            assert!(
                matches!(r.archive, Some(Err(SyncError::NeedsConfirmation(_)))),
                "{mode:?}"
            );
        }
        assert_eq!(visible(&e.root), before, "{mode:?}: nothing moved");
        assert!(!bin.exists() || files_under(&bin).is_empty(), "{mode:?}");
        assert!(made_of(&e).made_by_us("DCIM/DEL.JPG") && made_of(&e).made_by_us("DCIM/CHG.JPG"));
    }
}

/// A HEIC that can't be converted is recorded with its reason and doesn't fail the sync. It is
/// not tried again while it is unchanged, but is when it changes.
#[test]
fn a_failed_conversion_is_recorded_and_not_retried_until_it_changes() {
    let calls = Rc::new(Cell::new(0));
    let c = calls.clone();
    let _d = fake_decoder::set(move |p| {
        c.set(c.get() + 1);
        if p.to_string_lossy().contains("BAD") {
            Err(DecodeError::Damaged("the decoder choked".into()))
        } else {
            Ok(fake_decoder::picture_for(p))
        }
    });
    let e = env();
    let mut src = phone(vec![
        heic("1", "DCIM/GOOD.HEIC", A),
        heic("2", "DCIM/BAD.HEIC", B),
    ]);
    let (_, r) = round(&mut src, &e, DeletedFiles::Archive, false);
    assert_eq!((r.copy.copied, r.copy.failed), (2, 0));
    assert_eq!((r.jpg.made, r.jpg.failed), (1, 1));
    assert!(
        r.jpg_events
            .iter()
            .any(|ev| matches!(ev, JpgEvent::Failed { heic, .. } if heic == "DCIM/BAD.HEIC"))
    );
    let made = made_of(&e);
    let f = made.failed_for("DCIM/BAD.HEIC").unwrap();
    assert!(
        f.reason.contains("can't be read") && f.reason.contains("choked"),
        "{}",
        f.reason
    );
    assert_eq!((f.heic_size, made.failed().len()), (B.len() as u64, 1));
    assert_eq!(visible(&e.root).len(), 3);
    assert_eq!(calls.get(), 2, "one call for each photo");
    let after_first = calls.get();
    // Nothing changed: not tried again.
    let (plan, r) = round(&mut src, &e, DeletedFiles::Archive, false);
    assert_eq!(plan.jpg.to_make, 0);
    assert_eq!((r.jpg.made, r.jpg.failed, r.jpg.already_failed), (0, 0, 1));
    assert_eq!(calls.get(), after_first);
    // The HEIC changes: it is tried again (and fails again, with its new size).
    src.objects[1].data = C.to_vec();
    src.objects[1].reported_size = Some(C.len() as u64);
    let (plan, r) = round(&mut src, &e, DeletedFiles::Archive, false);
    assert_eq!(plan.jpg.to_make, 1);
    assert_eq!((r.jpg.made, r.jpg.failed), (0, 1));
    assert_eq!(
        made_of(&e).failed_for("DCIM/BAD.HEIC").unwrap().heic_size,
        C.len() as u64
    );
}

/// "Try again" forgets the recorded failures (the chosen ones, or all), so the next JPG step
/// tries them again.
#[test]
fn try_again_makes_a_failed_photo_when_the_decoder_works_now() {
    let broken = Rc::new(Cell::new(true));
    let b = broken.clone();
    let _d = fake_decoder::set(move |p| {
        if b.get() {
            Err(DecodeError::Damaged("not today".into()))
        } else {
            Ok(fake_decoder::picture_for(p))
        }
    });
    let e = env();
    let mut src = phone(vec![
        heic("1", "DCIM/ONE.HEIC", A),
        heic("2", "DCIM/TWO.HEIC", B),
        heic("3", "DCIM/THREE.HEIC", C),
    ]);
    let key = key_of(&src);
    let (_, r) = round(&mut src, &e, DeletedFiles::Archive, false);
    assert_eq!((r.jpg.made, r.jpg.failed), (0, 3));
    broken.set(false);
    // Without asking, nothing is tried.
    let (_, r) = round(&mut src, &e, DeletedFiles::Archive, false);
    assert_eq!((r.jpg.made, r.jpg.already_failed), (0, 3));
    // One chosen photo.
    let n = forget_failures(
        &e.root,
        &key,
        Some(View::WindowsWpd),
        Some(&["DCIM/ONE.HEIC".to_string(), "DCIM/NONE.HEIC".to_string()]),
    )
    .unwrap();
    assert_eq!(n, 1);
    let (_, r) = round(&mut src, &e, DeletedFiles::Archive, false);
    assert_eq!((r.jpg.made, r.jpg.already_failed), (1, 2));
    assert!(e.root.join("DCIM/ONE.JPG").exists() && !e.root.join("DCIM/TWO.JPG").exists());
    // All of them.
    assert_eq!(
        forget_failures(&e.root, &key, Some(View::WindowsWpd), None).unwrap(),
        2
    );
    let (_, r) = round(&mut src, &e, DeletedFiles::Archive, false);
    assert_eq!((r.jpg.made, r.jpg.failed), (2, 0));
    assert!(made_of(&e).failed().is_empty());
    assert_eq!(made_of(&e).len(), 3);
    // Another device's folder is refused.
    let r = forget_failures(&e.root, "someone-else", Some(View::WindowsWpd), None);
    assert!(matches!(r, Err(SyncError::WrongFolder(_))), "{r:?}");
}

/// A folder with HEICs already in it: the one-off backfill makes the JPGs, skips taken names and
/// the device's own `.jpeg`, leaves the archive alone, and stops when asked; run again, it
/// carries on and makes only what is left.
#[test]
fn the_backfill_makes_jpgs_skips_taken_names_and_can_be_stopped_and_resumed() {
    let _d = fake_decoder::working();
    let e = env();
    let src = phone(vec![heic("1", "DCIM/x.MOV", b"v")]);
    let key = key_of(&src);
    claim(&e, &key);
    let root = fs::canonicalize(&e.root).unwrap();
    for i in 0..5 {
        put(&root, &format!("DCIM/IMG_{i}.HEIC"), A);
    }
    put(&root, "DCIM/TAKEN.HEIC", B);
    put(&root, "DCIM/TAKEN.JPG", b"not ours");
    put(&root, "DCIM/OWN.HEIC", C);
    put(&root, "DCIM/OWN.jpeg", b"device's");
    put(&root, &format!("{ARCHIVE_DIR}/{STAMP}/DCIM/OLD.HEIC"), D);
    let cancel = AtomicBool::new(false);
    let mut events = Vec::new();
    let first = backfill(
        &root,
        &key,
        Some(View::WindowsWpd),
        ARCHIVE_DIR,
        &DeviceSettings::default().ignore_names,
        &cancel,
        &mut |ev| {
            if matches!(ev, JpgEvent::Made { .. })
                && events
                    .iter()
                    .filter(|e| matches!(e, JpgEvent::Made { .. }))
                    .count()
                    == 1
            {
                // The second one has just been made: Stop.
                cancel.store(true, Ordering::Relaxed);
            }
            events.push(ev);
        },
    )
    .unwrap();
    assert!(first.cancelled);
    assert_eq!(first.made, 2, "{events:?}");
    assert_eq!(made_of(&e).len(), 2);
    // Everything made is listed, nothing is half-written.
    let jpgs: Vec<String> = visible(&root)
        .into_iter()
        .filter(|p| p.ends_with(".JPG"))
        .collect();
    assert_eq!(jpgs.len(), 3, "two made and the taken one: {jpgs:?}");
    assert!(visible(&root).iter().all(|p| !p.ends_with(".pnk-part")));
    // Run again: carries on.
    cancel.store(false, Ordering::Relaxed);
    let second = backfill(
        &root,
        &key,
        Some(View::WindowsWpd),
        ARCHIVE_DIR,
        &DeviceSettings::default().ignore_names,
        &cancel,
        &mut |_| {},
    )
    .unwrap();
    assert!(!second.cancelled);
    assert_eq!((second.made, second.up_to_date), (3, 2 + 2), "{second:?}");
    assert_eq!(made_of(&e).len(), 5);
    assert_eq!(fs::read(root.join("DCIM/TAKEN.JPG")).unwrap(), b"not ours");
    // A JPEG of that name that the app didn't make is the device's: no failure is recorded.
    assert!(made_of(&e).failed_for("DCIM/TAKEN.HEIC").is_none());
    assert!(!root.join("DCIM/OWN.JPG").exists());
    assert!(
        !root
            .join(format!("{ARCHIVE_DIR}/{STAMP}/DCIM/OLD.JPG"))
            .exists()
    );
    // And a third time there is nothing left to do.
    let third = backfill(
        &root,
        &key,
        Some(View::WindowsWpd),
        ARCHIVE_DIR,
        &DeviceSettings::default().ignore_names,
        &cancel,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!((third.made, third.failed, third.already_failed), (0, 0, 0));
    // Another device's folder is refused.
    let r = backfill(
        &root,
        "someone-else",
        Some(View::WindowsWpd),
        ARCHIVE_DIR,
        &[],
        &cancel,
        &mut |_| {},
    );
    assert!(matches!(r, Err(SyncError::WrongFolder(_))), "{r:?}");
}

/// With no decoder the step does nothing, makes no failures, and the scan says why.
#[test]
fn with_no_decoder_nothing_is_made_and_nothing_is_recorded() {
    let _d = fake_decoder::missing("install the thing");
    let e = env();
    let mut src = phone(vec![heic("1", "DCIM/IMG_0001.HEIC", A)]);
    let (plan, r) = round(&mut src, &e, DeletedFiles::Archive, false);
    assert!(!plan.jpg.enabled);
    assert_eq!(plan.jpg.unavailable.as_deref(), Some("install the thing"));
    assert_eq!(r.jpg, JpgReport::default());
    assert!(made_of(&e).is_empty());
    assert_eq!(visible(&e.root), set(&["DCIM/IMG_0001.HEIC"]));
}

/// The room check counts the JPGs: a sync whose HEICs fit, but not their JPGs as well, is
/// refused before anything is written.
#[test]
fn the_space_estimate_includes_the_jpgs() {
    const GIB: u64 = 1 << 30;
    let _d = fake_decoder::working();
    let e = env();
    let mut src = phone(vec![heic("1", "DCIM/BIG.HEIC", A)]);
    src.objects[0].reported_size = Some(GIB);
    let (plan, _) = do_scan_with(&mut src, &e, &jopts(DeletedFiles::Archive));
    assert_eq!(plan.jpg.estimate_heic_bytes, GIB);
    assert_eq!(
        crate::space::jpg_bytes(&plan),
        (GIB as f64 * crate::space::JPG_SIZE_FACTOR) as u64
    );
    // 10 GiB disk: the margin is 1 GiB. The HEIC alone needs 1 + 1 = 2 GiB.
    let _g = fake_space::set(Some(DiskSpace {
        total: 10 * GIB,
        free: 2 * GIB + GIB / 2,
    }));
    let (r, _) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    match r {
        Err(SyncError::NotEnoughSpace(s)) => {
            assert_eq!(
                s.needed,
                GIB + GIB * 3 / 2,
                "the HEIC and its JPG's estimate"
            );
        }
        other => panic!("{other:?}"),
    }
    assert!(visible(&e.root).is_empty());
    // With the setting off the same disk is room enough: the sync gets past the check (and
    // then fails on the made-up size, which is not what is being tested).
    let off = ScanOptions {
        settings: DeviceSettings {
            jpeg_copies: false,
            ..jopts(DeletedFiles::Archive).settings
        },
        weak_match: false,
    };
    let (plan, _) = do_scan_with(&mut src, &e, &off);
    assert_eq!(crate::space::jpg_bytes(&plan), 0);
    let (r, _) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert!(!matches!(r, Err(SyncError::NotEnoughSpace(_))), "{r:?}");
}

/// A crash or a Stop part-way through the JPG step leaves nothing lost or overwritten: every JPG
/// the app wrote is in the list, nothing half-written is left, and the next step finishes the
/// job. An untracked JPG (a crash between the write and the list) is treated as someone else's
/// file and never overwritten; a listed JPG that has gone is made again.
#[test]
fn a_crash_or_stop_mid_jpg_step_loses_nothing_and_overwrites_nothing() {
    let _d = fake_decoder::working();
    let e = env();
    let mut src = phone(
        (0..6)
            .map(|i| heic(&format!("{i}"), &format!("DCIM/IMG_{i}.HEIC"), A))
            .collect(),
    );
    let (plan, _) = do_scan_with(&mut src, &e, &jopts(DeletedFiles::Archive));
    // Stop after the third JPG.
    let cancel = AtomicBool::new(false);
    let (copy, _) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    copy.unwrap();
    let mut n = 0;
    let r = make_jpegs(&plan, &cancel, &mut |ev| {
        if matches!(ev, JpgEvent::Made { .. }) {
            n += 1;
            if n == 3 {
                cancel.store(true, Ordering::Relaxed);
            }
        }
    })
    .unwrap();
    assert!(r.cancelled);
    assert_eq!(r.made, 3);
    let made = made_of(&e);
    let jpgs: BTreeSet<String> = visible(&e.root)
        .into_iter()
        .filter(|p| p.ends_with(".JPG"))
        .collect();
    assert_eq!(jpgs.len(), 3);
    for j in &jpgs {
        assert!(made.made_by_us(j), "{j} is not in the list");
    }
    assert!(
        visible(&e.root)
            .iter()
            .all(|p| !crate::paths::is_temp_name(p))
    );

    // A crash between writing a JPG and saving the list: the file is there, the list doesn't
    // know it. It is never overwritten; it is recorded as a taken name.
    let untracked = (0..6)
        .map(|i| format!("DCIM/IMG_{i}.JPG"))
        .find(|j| !jpgs.contains(j))
        .unwrap();
    put(&e.root, &untracked, b"half a crash");
    // A crash between moving a JPG away and saving the list: listed, but gone.
    let gone = jpgs.iter().next().unwrap().clone();
    fs::remove_file(e.root.join(&gone)).unwrap();
    // A stale temporary file from the crash, of the JPG that is made again.
    let stale = format!("{gone}{JPG_TEMP_SUFFIX}");
    put(&e.root, &stale, b"stale");
    let before: Vec<(String, Vec<u8>)> = jpgs
        .iter()
        .skip(1)
        .map(|j| (j.clone(), fs::read(e.root.join(j)).unwrap()))
        .collect();
    let (plan, _) = do_scan_with(&mut src, &e, &jopts(DeletedFiles::Archive));
    let r = make_jpegs(&plan, &AtomicBool::new(false), &mut |_| {}).unwrap();
    assert_eq!(fs::read(e.root.join(&untracked)).unwrap(), b"half a crash");
    assert!(
        e.root.join(&gone).exists(),
        "the missing JPG was made again"
    );
    for (j, bytes) in before {
        assert_eq!(
            fs::read(e.root.join(&j)).unwrap(),
            bytes,
            "{j} was rewritten"
        );
    }
    assert!(
        made_of(&e)
            .failed_for(&untracked.replace(".JPG", ".HEIC"))
            .is_some()
    );
    assert_eq!((r.made, r.failed), (3, 1), "{r:?}");
    // The step's own leftover temp file is replaced, not trusted, and nothing is left behind.
    assert!(
        visible(&e.root)
            .iter()
            .all(|p| !crate::paths::is_temp_name(p))
    );
    assert_eq!(made_of(&e).len(), 5);
}

/// A JPG that can't be moved along with its HEIC leaves both where they are: nothing is lost,
/// and the HEIC isn't moved without it.
#[test]
fn a_jpg_that_cannot_follow_its_heic_keeps_both_in_place() {
    let _d = fake_decoder::working();
    let e = env();
    let bin = e.base.join("bin");
    let _g = fake_bin::set(Fake::Unavailable);
    let mut src = synced_pair(&e, DeletedFiles::RecycleBin);
    src.objects.remove(0);
    let (_, r) = round(&mut src, &e, DeletedFiles::RecycleBin, false);
    let a = r.archive.unwrap().unwrap();
    assert_eq!((a.moved, a.failed, a.jpgs_moved), (0, 1, 0));
    assert!(e.root.join("DCIM/100APPLE/IMG_0001.HEIC").exists());
    assert!(e.root.join("DCIM/100APPLE/IMG_0001.JPG").exists());
    assert!(made_of(&e).made_by_us("DCIM/100APPLE/IMG_0001.JPG"));
    assert!(!bin.exists());
}

/// "Try again" also makes the JPG straight away, from the backup folder alone, for the chosen
/// photos or all of them.
#[test]
fn try_again_retries_now_from_the_backup_folder() {
    let broken = Rc::new(Cell::new(true));
    let b = broken.clone();
    let _d = fake_decoder::set(move |p| {
        if b.get() {
            Err(DecodeError::Damaged("not today".into()))
        } else {
            Ok(fake_decoder::picture_for(p))
        }
    });
    let e = env();
    let mut src = phone(vec![
        heic("1", "DCIM/ONE.HEIC", A),
        heic("2", "DCIM/TWO.HEIC", B),
    ]);
    let key = key_of(&src);
    let (_, r) = round(&mut src, &e, DeletedFiles::Archive, false);
    assert_eq!(r.jpg.failed, 2);
    broken.set(false);
    let cancel = AtomicBool::new(false);
    let one = retry_failures(
        &e.root,
        &key,
        Some(View::WindowsWpd),
        Some(&["DCIM/ONE.HEIC".to_string()]),
        &cancel,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!((one.made, one.failed), (1, 0));
    assert!(e.root.join("DCIM/ONE.JPG").exists() && !e.root.join("DCIM/TWO.JPG").exists());
    let all = retry_failures(
        &e.root,
        &key,
        Some(View::WindowsWpd),
        None,
        &cancel,
        &mut |_| {},
    )
    .unwrap();
    assert_eq!((all.made, all.failed), (1, 0));
    assert!(made_of(&e).failed().is_empty());
    assert_eq!(made_of(&e).len(), 2);
}

// ---- The review's findings (H-M1 to H-M3, H-L1 to H-L8) ----

fn device_path(o: &MemObject) -> String {
    o.components.join("/")
}

/// A made-list entry is believed only while the file is what the app wrote. A forged entry for a
/// file that isn't a JPG, or that isn't the name its HEIC gives, hides and moves nothing; a JPG
/// the user edited is no longer the app's, so it is no longer hidden from "deleted" and doesn't
/// follow its HEIC.
#[test]
fn a_forged_or_stale_made_entry_hides_and_moves_nothing() {
    let _d = fake_decoder::working();
    let e = env();
    let mut src = synced_pair(&e, DeletedFiles::Archive);
    let root = fs::canonicalize(&e.root).unwrap();
    // Forged: videos listed as JPGs the app made, one of a real HEIC, one of none.
    let mut l = MadeList::load(&root);
    for (jpg, heic) in [
        ("DCIM/F0.MOV", "DCIM/100APPLE/IMG_0001.HEIC"),
        ("DCIM/F1.MOV", "DCIM/F1.HEIC"),
    ] {
        let (size, ms) = facts_of(&root.join(jpg)).unwrap();
        l.insert(MadeEntry {
            jpg: jpg.into(),
            heic: heic.into(),
            heic_size: 1,
            heic_modified_ms: 1,
            made: "x".into(),
            jpg_size: Some(size),
            jpg_modified_ms: Some(ms),
        });
    }
    l.save(&root).unwrap();
    // And stale: the user edits the made JPG.
    fs::write(
        root.join("DCIM/100APPLE/IMG_0001.JPG"),
        b"edited by the user, so longer",
    )
    .unwrap();
    // The videos and the HEIC leave the device.
    src.objects.retain(|o| {
        !["DCIM/F0.MOV", "DCIM/F1.MOV", "DCIM/100APPLE/IMG_0001.HEIC"]
            .contains(&device_path(o).as_str())
    });
    let (plan, _) = do_scan_with(&mut src, &e, &jopts(DeletedFiles::Archive));
    let mut gone = removed_paths(&plan);
    gone.sort();
    assert_eq!(
        gone,
        [
            "DCIM/100APPLE/IMG_0001.HEIC",
            "DCIM/100APPLE/IMG_0001.JPG",
            "DCIM/F0.MOV",
            "DCIM/F1.MOV"
        ]
    );
    assert!(
        plan.removed.iter().all(|r| r.jpg.is_none()),
        "no file moves with another's HEIC"
    );
    assert!(plan.outside.is_empty(), "{:?}", plan.outside);
    // They are counted as changed? No: nothing is hidden from the counts either.
    assert!(made_of(&e).is_empty());
}

/// A list written by an earlier version has no facts of the JPG. Its entries are believed while
/// the JPG has its HEIC's modified time, so the JPG still follows its HEIC; the next save writes
/// the facts.
#[test]
fn an_old_format_list_still_works_and_is_upgraded() {
    let _d = fake_decoder::working();
    let e = env();
    let mut src = synced_pair(&e, DeletedFiles::Archive);
    let root = fs::canonicalize(&e.root).unwrap();
    // Take the new fields out, as an earlier version wrote the file.
    let text = fs::read_to_string(root.join(MADE_NAME)).unwrap();
    let mut v: serde_json::Value = serde_json::from_str(&text).unwrap();
    for m in v["made"].as_array_mut().unwrap() {
        let o = m.as_object_mut().unwrap();
        assert!(o.remove("jpg_size").is_some());
        assert!(o.remove("jpg_modified_ms").is_some());
    }
    fs::write(root.join(MADE_NAME), serde_json::to_vec(&v).unwrap()).unwrap();
    let l = made_of(&e);
    assert_eq!(l.len(), 1, "an old entry is still believed");
    assert!(l.entries()[0].jpg_size.is_some(), "and upgraded");
    // It follows its HEIC.
    src.objects.remove(0);
    let (plan, r) = round(&mut src, &e, DeletedFiles::Archive, false);
    assert_eq!(removed_paths(&plan), ["DCIM/100APPLE/IMG_0001.HEIC"]);
    assert!(plan.removed[0].jpg.is_some());
    let a = r.archive.unwrap().unwrap();
    assert_eq!((a.moved, a.failed, a.jpgs_moved), (1, 0, 1));
    assert!(made_of(&e).is_empty());
}

/// A JPG of the device's own that is copied onto a name the list still has an entry for (an old
/// entry, in a form that trusts the modified time) ends that entry: the sync forgets it when the
/// copy lands.
#[test]
fn a_copy_that_lands_on_a_made_name_ends_its_entry() {
    let _d = fake_decoder::working();
    let e = env();
    let mut src = phone(vec![
        heic("1", "DCIM/IMG_1.HEIC", A),
        heic("2", "DCIM/IMG_1.JPG", b"the device's own"),
    ]);
    src.objects[0].modified = Some(at(10));
    src.objects[1].modified = Some(at(10));
    claim(&e, &key_of(&src));
    let root = fs::canonicalize(&e.root).unwrap();
    let mut l = MadeList::default();
    l.insert(MadeEntry {
        jpg: "DCIM/IMG_1.JPG".into(),
        heic: "DCIM/IMG_1.HEIC".into(),
        heic_size: A.len() as u64,
        heic_modified_ms: to_ms(at(10)),
        made: "x".into(),
        jpg_size: None,
        jpg_modified_ms: None,
    });
    l.save(&root).unwrap();
    let (_, r) = round(&mut src, &e, DeletedFiles::Archive, false);
    assert_eq!(
        (r.copy.copied, r.copy.failed),
        (2, 0),
        "{:?}",
        r.copy_events
    );
    assert_eq!(
        fs::read(root.join("DCIM/IMG_1.JPG")).unwrap(),
        b"the device's own"
    );
    assert!(
        !made_of(&e).made_by_us("DCIM/IMG_1.JPG"),
        "the list calls the device's own JPG the app's"
    );
}

/// A crash between writing a JPG and saving the list: the list already had a provisional entry,
/// so the JPG is still the app's, and when its HEIC is deleted from the device it follows the HEIC
/// instead of being archived as "deleted" on its own.
#[test]
fn a_crash_after_a_jpg_is_written_still_lets_it_follow_its_heic() {
    let e = env();
    let list = e.root.join(MADE_NAME);
    let snapshot: Rc<RefCell<Option<Vec<u8>>>> = Rc::new(RefCell::new(None));
    let s2 = snapshot.clone();
    // The decoder runs after the provisional entry is saved and before the JPG is written, so
    // the list as it is then is what a crash right after the write would leave.
    let _d = fake_decoder::set(move |p| {
        *s2.borrow_mut() = fs::read(&list).ok();
        Ok(fake_decoder::picture_for(p))
    });
    let mut src = synced_pair(&e, DeletedFiles::Archive);
    let root = fs::canonicalize(&e.root).unwrap();
    assert!(root.join("DCIM/100APPLE/IMG_0001.JPG").exists());
    // The crash: the JPG is there, and the list is what it was before the final save.
    match snapshot.borrow().as_ref() {
        Some(bytes) => fs::write(root.join(MADE_NAME), bytes).unwrap(),
        None => fs::remove_file(root.join(MADE_NAME)).unwrap(),
    }
    // Then the HEIC is deleted from the device.
    src.objects.remove(0);
    let (plan, r) = round(&mut src, &e, DeletedFiles::Archive, false);
    assert_eq!(
        removed_paths(&plan),
        ["DCIM/100APPLE/IMG_0001.HEIC"],
        "the JPG is not listed as deleted on its own"
    );
    assert!(plan.removed[0].jpg.is_some());
    let a = r.archive.unwrap().unwrap();
    assert_eq!((a.moved, a.failed, a.jpgs_moved), (1, 0, 1));
    let dir = format!("{ARCHIVE_DIR}/{STAMP}/DCIM/100APPLE");
    assert!(root.join(format!("{dir}/IMG_0001.JPG")).exists());
    assert!(!root.join("DCIM/100APPLE/IMG_0001.JPG").exists());
}

/// A JPG that fails leaves no provisional entry behind: the list holds only JPGs that exist and
/// failures.
#[test]
fn a_failed_jpg_leaves_no_provisional_entry() {
    let _d = fake_decoder::set(|_| Err(DecodeError::Damaged("not today".into())));
    let e = env();
    let mut src = phone(vec![heic("1", "DCIM/IMG_1.HEIC", A)]);
    let (_, r) = round(&mut src, &e, DeletedFiles::Archive, false);
    assert_eq!((r.jpg.made, r.jpg.failed), (0, 1));
    let raw = fs::read_to_string(e.root.join(MADE_NAME)).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(v["made"].as_array().unwrap().is_empty(), "{raw}");
    assert_eq!(v["failed"].as_array().unwrap().len(), 1);
}

/// The JPG step keeps the same room free as a sync does ([`crate::space::margin`]): with less than
/// the next JPG's estimate plus the margin free, it stops with a plain message and records
/// nothing against the photo; a disk that reports no figure doesn't stop it. The backfill and
/// "Try again" do the same.
#[test]
fn the_jpg_step_keeps_the_margin_free() {
    const GIB: u64 = 1 << 30;
    let _d = fake_decoder::working();
    let total = 100 * GIB;
    let margin = crate::space::margin(total);
    assert_eq!(margin, 2 * GIB);
    let est = (A.len() as f64 * crate::space::JPG_SIZE_FACTOR) as u64;
    let short = DiskSpace {
        total,
        free: margin + est - 1,
    };
    let enough = DiskSpace {
        total,
        free: margin + est,
    };

    // The sync's own JPG step.
    let e = env();
    let mut src = phone(vec![
        heic("1", "DCIM/ONE.HEIC", A),
        heic("2", "DCIM/TWO.HEIC", B),
    ]);
    let (plan, _) = do_scan_with(&mut src, &e, &jopts(DeletedFiles::Archive));
    let (copy, _) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    copy.unwrap();
    {
        let _g = fake_space::set(Some(short));
        let r = make_jpegs(&plan, &AtomicBool::new(false), &mut |_| {}).unwrap();
        let why = r.error.expect("it stopped for room");
        assert!(why.contains("must keep"), "{why}");
        assert_eq!((r.made, r.failed), (0, 0));
        assert!(made_of(&e).is_empty() && made_of(&e).failed().is_empty());
        assert!(visible(&e.root).iter().all(|p| !p.ends_with(".JPG")));
    }
    {
        // A disk with no figure (0 free on a disk that has a size) is unknown, not full.
        let _g = fake_space::set(Some(DiskSpace { total, free: 0 }));
        let r = make_jpegs(&plan, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert_eq!((r.made, r.error), (2, None));
    }
    // Exactly the margin and the estimate is enough (the other JPG is made at once).
    let e = env();
    let mut src = phone(vec![heic("1", "DCIM/ONE.HEIC", A)]);
    let (plan, _) = do_scan_with(&mut src, &e, &jopts(DeletedFiles::Archive));
    do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    )
    .0
    .unwrap();
    {
        let _g = fake_space::set(Some(enough));
        let r = make_jpegs(&plan, &AtomicBool::new(false), &mut |_| {}).unwrap();
        assert_eq!((r.made, r.error), (1, None));
    }

    // The backfill.
    let e = env();
    let src = phone(vec![heic("1", "DCIM/x.MOV", b"v")]);
    let key = key_of(&src);
    claim(&e, &key);
    let root = fs::canonicalize(&e.root).unwrap();
    put(&root, "DCIM/ONE.HEIC", A);
    let cancel = AtomicBool::new(false);
    let ignore = DeviceSettings::default().ignore_names;
    {
        let _g = fake_space::set(Some(short));
        let r = backfill(
            &root,
            &key,
            Some(View::WindowsWpd),
            ARCHIVE_DIR,
            &ignore,
            &cancel,
            &mut |_| {},
        )
        .unwrap();
        assert!(r.error.is_some_and(|w| w.contains("must keep")));
        assert_eq!(r.made, 0);
        assert!(!root.join("DCIM/ONE.JPG").exists());
        assert!(made_of(&e).is_empty() && made_of(&e).failed().is_empty());
    }
    {
        let _g = fake_space::set(Some(enough));
        let r = backfill(
            &root,
            &key,
            Some(View::WindowsWpd),
            ARCHIVE_DIR,
            &ignore,
            &cancel,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!((r.made, r.error), (1, None));
    }

    // "Try again".
    let broken = Rc::new(Cell::new(true));
    let b = broken.clone();
    let _d = fake_decoder::set(move |p| {
        if b.get() {
            Err(DecodeError::Damaged("not today".into()))
        } else {
            Ok(fake_decoder::picture_for(p))
        }
    });
    let e = env();
    let mut src = phone(vec![heic("1", "DCIM/ONE.HEIC", A)]);
    let key = key_of(&src);
    let (_, r) = round(&mut src, &e, DeletedFiles::Archive, false);
    assert_eq!(r.jpg.failed, 1);
    broken.set(false);
    {
        let _g = fake_space::set(Some(short));
        let r = retry_failures(
            &e.root,
            &key,
            Some(View::WindowsWpd),
            None,
            &cancel,
            &mut |_| {},
        )
        .unwrap();
        assert!(r.error.is_some_and(|w| w.contains("must keep")));
        assert_eq!(r.made, 0);
        assert!(!e.root.join("DCIM/ONE.JPG").exists());
    }
    {
        // Asked again, with room, the photo is made.
        let _g = fake_space::set(Some(enough));
        let again = backfill(
            &e.root,
            &key,
            Some(View::WindowsWpd),
            ARCHIVE_DIR,
            &ignore,
            &cancel,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!((again.made, again.error), (1, None));
    }
}

/// The backfill treats a JPEG of the same name that the app did not make as the device's own, at
/// the exact name too (DESIGN.md section 7.6): nothing is made and no failure is recorded.
#[test]
fn the_backfill_leaves_a_heic_with_a_jpeg_of_the_same_name_alone() {
    let _d = fake_decoder::working();
    let e = env();
    let src = phone(vec![heic("1", "DCIM/x.MOV", b"v")]);
    let key = key_of(&src);
    claim(&e, &key);
    let root = fs::canonicalize(&e.root).unwrap();
    for (heic_name, jpeg) in [
        ("A.HEIC", "A.JPG"),
        ("B.HEIC", "B.jpeg"),
        ("C.heic", "c.JPG"),
        ("D.HEIC", "D.Jpeg"),
    ] {
        put(&root, &format!("DCIM/{heic_name}"), A);
        put(&root, &format!("DCIM/{jpeg}"), b"the device's");
    }
    put(&root, "DCIM/E.HEIC", A);
    let before = visible(&root);
    let r = backfill(
        &root,
        &key,
        Some(View::WindowsWpd),
        ARCHIVE_DIR,
        &DeviceSettings::default().ignore_names,
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!((r.made, r.failed, r.up_to_date), (1, 0, 4), "{r:?}");
    assert!(made_of(&e).failed().is_empty());
    let mut after = visible(&root);
    assert!(after.remove("DCIM/E.JPG"));
    assert_eq!(after, before);
}

/// "Try again" asks the same question: a JPEG beside the HEIC that the app didn't make (`.jpeg`,
/// another case) is the device's, so nothing is made, and the failure is cleared.
#[test]
fn try_again_leaves_a_heic_with_a_jpeg_of_the_same_name_alone() {
    let broken = Rc::new(Cell::new(true));
    let b = broken.clone();
    let _d = fake_decoder::set(move |p| {
        if b.get() {
            Err(DecodeError::Damaged("not today".into()))
        } else {
            Ok(fake_decoder::picture_for(p))
        }
    });
    let e = env();
    let mut src = phone(vec![
        heic("1", "DCIM/ONE.HEIC", A),
        heic("2", "DCIM/TWO.HEIC", B),
        heic("3", "DCIM/THREE.HEIC", C),
    ]);
    let key = key_of(&src);
    let (_, r) = round(&mut src, &e, DeletedFiles::Archive, false);
    assert_eq!(r.jpg.failed, 3);
    broken.set(false);
    // The device's JPEGs turn up beside two of them.
    put(&e.root, "DCIM/ONE.jpeg", b"the device's");
    put(&e.root, "DCIM/TWO.Jpg", b"the device's");
    let r = retry_failures(
        &e.root,
        &key,
        Some(View::WindowsWpd),
        None,
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!((r.made, r.failed, r.up_to_date), (1, 0, 2), "{r:?}");
    assert!(made_of(&e).failed().is_empty());
    // (By the names the folder lists: on Windows `TWO.Jpg` and `TWO.JPG` are one file.)
    let names = visible(&e.root);
    assert!(names.contains("DCIM/THREE.JPG"));
    assert!(!names.contains("DCIM/ONE.JPG") && !names.contains("DCIM/TWO.JPG"));
    assert_eq!(names.len(), 3 + 2 + 1, "{names:?}");
}

/// A Stop while the backfill is still looking through the backup is a Stop, not an error.
#[test]
fn stopping_the_backfill_while_it_looks_is_a_stop_not_an_error() {
    let _d = fake_decoder::working();
    let e = env();
    let src = phone(vec![heic("1", "DCIM/x.MOV", b"v")]);
    let key = key_of(&src);
    claim(&e, &key);
    let root = fs::canonicalize(&e.root).unwrap();
    for i in 0..20 {
        put(&root, &format!("DCIM/IMG_{i}.HEIC"), A);
    }
    let r = backfill(
        &root,
        &key,
        Some(View::WindowsWpd),
        ARCHIVE_DIR,
        &DeviceSettings::default().ignore_names,
        &AtomicBool::new(true),
        &mut |_| {},
    )
    .expect("a Stop is not an error");
    assert!(r.cancelled);
    assert_eq!((r.made, r.failed, r.error), (0, 0, None));
    assert!(made_of(&e).is_empty());
}

/// The JPG's temporary file has a name the copy step can't produce: a device file called
/// `IMG_1.JPG.making` has the copy temp `IMG_1.JPG.making.pnk-part`, which the JPG step used to
/// use for `IMG_1.JPG`. A copy in progress is never removed by the JPG step, and a stale JPG temp
/// is.
#[test]
fn the_jpg_temp_name_never_clashes_with_a_copy_temp() {
    let _d = fake_decoder::working();
    let e = env();
    let mut src = phone(vec![
        heic("1", "DCIM/IMG_1.HEIC", A),
        heic("2", "DCIM/IMG_1.JPG.making", b"a device file"),
    ]);
    let (plan, _) = do_scan_with(&mut src, &e, &jopts(DeletedFiles::Archive));
    let (copy, _) = do_sync(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
    );
    copy.unwrap();
    // As if the copy of the device file were in progress, and an earlier JPG step had crashed.
    let copy_temp = format!("DCIM/IMG_1.JPG.making{TEMP_SUFFIX}");
    put(&e.root, &copy_temp, b"half a copy");
    put(
        &e.root,
        &format!("DCIM/IMG_1.JPG{JPG_TEMP_SUFFIX}"),
        b"stale",
    );
    let r = make_jpegs(&plan, &AtomicBool::new(false), &mut |_| {}).unwrap();
    assert_eq!((r.made, r.failed), (1, 0), "{r:?}");
    assert_eq!(fs::read(e.root.join(&copy_temp)).unwrap(), b"half a copy");
    assert!(
        !e.root
            .join(format!("DCIM/IMG_1.JPG{JPG_TEMP_SUFFIX}"))
            .exists()
    );
    assert!(e.root.join("DCIM/IMG_1.JPG").exists());
    // Both names are the app's own, never a device file's.
    assert!(crate::paths::is_temp_name(&copy_temp));
    assert!(crate::paths::is_temp_name(&format!(
        "IMG_1.JPG{JPG_TEMP_SUFFIX}"
    )));
    assert!(SafeRelPath::parse(&format!("DCIM/IMG_1.JPG{JPG_TEMP_SUFFIX}")).is_err());
}

/// `IMG.heic` on the device with `img.JPG` beside it: the device has its own JPG (the match is
/// without regard to case, the safe way round), so none is made.
#[test]
fn a_device_jpg_of_another_case_means_no_jpg_is_made() {
    let _d = fake_decoder::working();
    let e = env();
    let mut src = phone(vec![
        heic("1", "DCIM/IMG.heic", A),
        heic("2", "DCIM/img.JPG", b"the device's"),
    ]);
    let (plan, r) = round(&mut src, &e, DeletedFiles::Archive, false);
    assert_eq!((plan.jpg.device_has_jpg, plan.jpg.candidates.len()), (1, 0));
    assert_eq!(r.jpg.made, 0);
    assert!(made_of(&e).is_empty() && made_of(&e).failed().is_empty());
    assert_eq!(visible(&e.root), set(&["DCIM/IMG.heic", "DCIM/img.JPG"]));
}

/// Restores the fault seam of the archive step when a test ends.
struct HeicMoveFault;

impl HeicMoveFault {
    fn set() -> HeicMoveFault {
        crate::sync::HEIC_MOVE_FAULT.with(|c| c.set(true));
        HeicMoveFault
    }
}

impl Drop for HeicMoveFault {
    fn drop(&mut self) {
        crate::sync::HEIC_MOVE_FAULT.with(|c| c.set(false));
    }
}

/// A HEIC that can't be moved after its JPG has been: the JPG is put back where it was (nothing
/// is replaced), so the pair stays whole, and the next sync moves both. The other order, a JPG
/// that can't be moved, leaves both in place and is covered by
/// `a_jpg_that_cannot_follow_its_heic_keeps_both_in_place`.
#[test]
fn a_heic_that_cannot_move_gets_its_jpg_put_back() {
    let _d = fake_decoder::working();
    let e = env();
    let mut src = synced_pair(&e, DeletedFiles::Archive);
    let root = fs::canonicalize(&e.root).unwrap();
    let jpg_bytes = fs::read(root.join("DCIM/100APPLE/IMG_0001.JPG")).unwrap();
    src.objects.remove(0);
    let (plan, _) = do_scan_with(&mut src, &e, &jopts(DeletedFiles::Archive));
    {
        let _f = HeicMoveFault::set();
        let mut events = Vec::new();
        let a = archive_removed(&plan, STAMP, false, &mut |ev| events.push(ev)).unwrap();
        assert_eq!((a.moved, a.failed, a.jpgs_moved), (0, 1, 0), "{events:?}");
        let msg = format!("{events:?}");
        assert!(msg.contains("put back"), "{msg}");
    }
    assert!(root.join("DCIM/100APPLE/IMG_0001.HEIC").exists());
    assert_eq!(
        fs::read(root.join("DCIM/100APPLE/IMG_0001.JPG")).unwrap(),
        jpg_bytes
    );
    assert!(made_of(&e).made_by_us("DCIM/100APPLE/IMG_0001.JPG"));
    // Nothing of the pair is left in the archive.
    assert!(
        visible(&root)
            .iter()
            .all(|p| !(p.starts_with(ARCHIVE_DIR) && p.contains("IMG_0001")))
    );
    // Without the fault the next sync moves both.
    let (plan, _) = do_scan_with(&mut src, &e, &jopts(DeletedFiles::Archive));
    let a = archive_removed(&plan, STAMP, false, &mut |_| {}).unwrap();
    assert_eq!((a.moved, a.failed, a.jpgs_moved), (1, 0, 1));
}

/// In the bin the JPG can't be taken back out: the HEIC stays, the failure says where the JPG is,
/// and the list lets the JPG go (it is no longer in the backup folder).
#[test]
fn a_heic_that_cannot_move_after_its_jpg_went_to_the_bin_says_so() {
    let _d = fake_decoder::working();
    let e = env();
    let bin = e.base.join("bin");
    let _g = fake_bin::set(Fake::Into(bin.clone()));
    let mut src = synced_pair(&e, DeletedFiles::RecycleBin);
    src.objects.remove(0);
    let (plan, _) = do_scan_with(&mut src, &e, &jopts(DeletedFiles::RecycleBin));
    let _f = HeicMoveFault::set();
    let mut events = Vec::new();
    let a = archive_removed(&plan, STAMP, false, &mut |ev| events.push(ev)).unwrap();
    assert_eq!((a.moved, a.failed, a.jpgs_moved), (0, 1, 1), "{events:?}");
    let msg = format!("{events:?}");
    assert!(msg.contains("could not be put back"), "{msg}");
    assert!(e.root.join("DCIM/100APPLE/IMG_0001.HEIC").exists());
    assert_eq!(files_under(&bin), set(&["IMG_0001.JPG"]));
    assert!(made_of(&e).is_empty());
}
