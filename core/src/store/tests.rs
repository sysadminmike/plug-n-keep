//! Rules every store must keep, run against each of them.

use std::any::Any;
use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::*;

fn rel(s: &str) -> SafeRelPath {
    SafeRelPath::parse(s).unwrap()
}

type Make<'a> = &'a dyn Fn() -> (Box<dyn BackupStore>, Box<dyn Any>);

pub(super) fn run_all(make: impl Fn() -> (Box<dyn BackupStore>, Box<dyn Any>)) {
    let make: Make = &make;
    let checks: [fn(&dyn BackupStore); 8] = [
        write_new_refuses_to_replace,
        conditional_replace_conflicts,
        upload_is_invisible_until_finish,
        abort_leaves_nothing,
        finish_refuses_a_taken_name,
        wrong_size_is_refused,
        mtime_round_trips,
        move_never_replaces,
    ];
    for check in checks {
        let (store, _keep) = make();
        check(&*store);
    }
    let (store, _keep) = make();
    pruning_skips_folders(&*store);
}

fn names(s: &dyn BackupStore, prune: &dyn Fn(&str) -> bool) -> Vec<String> {
    let mut v: Vec<String> = s
        .list(prune)
        .unwrap()
        .files
        .into_iter()
        .map(|f| f.rel)
        .collect();
    v.sort();
    v
}

fn put(s: &dyn BackupStore, name: &str, data: &[u8]) {
    s.write_new(&rel(name), data).unwrap();
}

fn upload(
    s: &dyn BackupStore,
    name: &str,
    data: &[u8],
    mtime: Option<SystemTime>,
) -> Box<dyn Upload> {
    let mut up = s
        .begin_upload(&rel(name), data.len() as u64, mtime)
        .unwrap();
    up.write(data).unwrap();
    up
}

fn write_new_refuses_to_replace(s: &dyn BackupStore) {
    s.ensure_root().unwrap();
    put(s, "a/b.txt", b"one");
    let e = s.write_new(&rel("a/b.txt"), b"two").unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(s.read(&rel("a/b.txt")).unwrap().unwrap().0, b"one");
    assert!(s.read(&rel("a/none.txt")).unwrap().is_none());
    assert!(s.stat(&rel("a/none.txt")).unwrap().is_none());
}

fn conditional_replace_conflicts(s: &dyn BackupStore) {
    s.ensure_root().unwrap();
    let t1 = s.write_new(&rel("state.json"), b"v1").unwrap();
    let (bytes, tag) = s.read(&rel("state.json")).unwrap().unwrap();
    assert_eq!(bytes, b"v1");
    assert_eq!(tag, t1);
    let t2 = s
        .write_replace(&rel("state.json"), b"version 2", Some(&t1))
        .unwrap();
    assert_ne!(t1, t2);
    let e = s
        .write_replace(&rel("state.json"), b"lost", Some(&t1))
        .unwrap_err();
    assert!(is_conflict(&e), "{e}");
    assert_eq!(s.read(&rel("state.json")).unwrap().unwrap().0, b"version 2");
    // Gone counts as changed.
    let e = s
        .write_replace(&rel("other.json"), b"x", Some(&t1))
        .unwrap_err();
    assert!(is_conflict(&e));
    // No expectation: replace, or create.
    s.write_replace(&rel("state.json"), b"v3", None).unwrap();
    s.write_replace(&rel("new.json"), b"n", None).unwrap();
    assert_eq!(s.read(&rel("state.json")).unwrap().unwrap().0, b"v3");
    assert!(!is_conflict(&io::Error::other("x")));
}

fn upload_is_invisible_until_finish(s: &dyn BackupStore) {
    s.ensure_root().unwrap();
    let up = upload(s, "d/x.bin", b"hello", None);
    assert!(!up.journal_ref().is_empty());
    assert!(s.stat(&rel("d/x.bin")).unwrap().is_none());
    assert!(!names(s, &|_| false).contains(&"d/x.bin".to_string()));
    // Other work between the last write and the finish is allowed.
    put(s, "d/y.bin", b"other");
    s.move_no_replace(&rel("d/y.bin"), &rel("moved/y.bin"))
        .unwrap();
    let tag = up.finish_no_replace().unwrap();
    let st = s.stat(&rel("d/x.bin")).unwrap().unwrap();
    assert_eq!(st.size, 5);
    assert_eq!(st.tag, Some(tag));
    assert_eq!(s.read(&rel("d/x.bin")).unwrap().unwrap().0, b"hello");
}

fn abort_leaves_nothing(s: &dyn BackupStore) {
    s.ensure_root().unwrap();
    let up = upload(s, "x.bin", b"partial", None);
    up.abort().unwrap();
    assert!(names(s, &|_| false).is_empty());
    assert!(s.stat(&rel("x.bin")).unwrap().is_none());
    // And from the crash record.
    let up = upload(s, "y.bin", b"partial", None);
    let r = up.journal_ref();
    drop(up);
    s.abort_by_ref(&r).unwrap();
    assert!(names(s, &|_| false).is_empty());
    s.abort_by_ref(&r).unwrap();
}

fn finish_refuses_a_taken_name(s: &dyn BackupStore) {
    s.ensure_root().unwrap();
    let up = upload(s, "x.bin", b"new", None);
    put(s, "x.bin", b"theirs");
    let e = up.finish_no_replace().unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(s.read(&rel("x.bin")).unwrap().unwrap().0, b"theirs");
    // The refused upload is gone.
    assert_eq!(names(s, &|_| false), vec!["x.bin".to_string()]);
}

fn wrong_size_is_refused(s: &dyn BackupStore) {
    s.ensure_root().unwrap();
    let mut up = s.begin_upload(&rel("x.bin"), 10, None).unwrap();
    up.write(b"short").unwrap();
    assert!(up.finish_no_replace().is_err());
    assert!(s.stat(&rel("x.bin")).unwrap().is_none());
    assert!(names(s, &|_| false).is_empty());
}

fn mtime_round_trips(s: &dyn BackupStore) {
    s.ensure_root().unwrap();
    let when = UNIX_EPOCH + Duration::from_millis(1_650_000_321_000);
    upload(s, "p/img.jpg", b"jpeg", Some(when))
        .finish_no_replace()
        .unwrap();
    assert_eq!(
        s.stat(&rel("p/img.jpg")).unwrap().unwrap().mtime,
        Some(when)
    );
    let l = s.list(&|_| false).unwrap();
    assert_eq!(l.files.len(), 1);
    // An object store's listing may not carry modified times (stat does); never a wrong one.
    assert!(l.files[0].mtime.is_none() || l.files[0].mtime == Some(when));
    assert_eq!(l.files[0].size, 4);
    assert_eq!(l.files[0].kind, Kind::File);
}

fn move_never_replaces(s: &dyn BackupStore) {
    s.ensure_root().unwrap();
    put(s, "a.txt", b"a");
    put(s, "b.txt", b"b");
    let e = s.move_no_replace(&rel("a.txt"), &rel("b.txt")).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(s.read(&rel("a.txt")).unwrap().unwrap().0, b"a");
    assert_eq!(s.read(&rel("b.txt")).unwrap().unwrap().0, b"b");
    s.move_no_replace(&rel("a.txt"), &rel("sub/c.txt")).unwrap();
    assert!(s.stat(&rel("a.txt")).unwrap().is_none());
    assert_eq!(s.read(&rel("sub/c.txt")).unwrap().unwrap().0, b"a");
}

fn pruning_skips_folders(s: &dyn BackupStore) {
    s.ensure_root().unwrap();
    put(s, "top.txt", b"1");
    put(s, "DCIM/a.jpg", b"1");
    put(s, "DCIM/sub/b.jpg", b"1");
    put(s, "_Archived/2026/c.jpg", b"1");
    put(s, "x/_Archived/d.jpg", b"1");
    assert_eq!(names(s, &|_| false).len(), 5);
    let skip = |d: &str| d.rsplit('/').next() == Some("_Archived");
    assert_eq!(
        names(s, &skip),
        vec![
            "DCIM/a.jpg".to_string(),
            "DCIM/sub/b.jpg".to_string(),
            "top.txt".to_string()
        ]
    );
    assert_eq!(
        names(s, &|d| d == "DCIM"),
        vec![
            "_Archived/2026/c.jpg".to_string(),
            "top.txt".to_string(),
            "x/_Archived/d.jpg".to_string()
        ]
    );
}
