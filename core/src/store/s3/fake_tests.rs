//! Tests of the S3 store against the pretend server ([`FakeS3`]): lost answers, refused
//! requests, the order of requests, the self-test and the checks on references and prefixes.
#![forbid(unsafe_code)]

use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::fake::FakeS3;
use super::tests::{offline_config, rel};
use super::{Limits, S3Store};
use crate::store::{BackupStore, is_conflict};

/// A store with tiny parts (4 bytes; anything over 8 bytes copies in parts) on a fake server.
fn store() -> (S3Store, FakeS3, tempfile::TempDir) {
    let limits = Limits {
        part_size: 4,
        copy_limit: 8,
        copy_part: 4,
        list_page: 1000,
    };
    let (s, fake) = S3Store::on_fake(limits);
    (s, fake, tempfile::tempdir().unwrap())
}

fn mtime(ms: u64) -> Option<SystemTime> {
    Some(UNIX_EPOCH + Duration::from_millis(ms))
}

// ---- an answer lost after the request went through ------------------------------------

#[test]
fn a_new_file_whose_answer_was_lost_is_still_ours() {
    let (s, fake, _d) = store();
    fake.lose_reply("put");
    let tag = s.write_new(&rel("a.txt"), b"mine").unwrap();
    assert_eq!(fake.count("put"), 2);
    assert_eq!(fake.get("p/a.txt").unwrap(), b"mine");
    assert_eq!(s.stat(&rel("a.txt")).unwrap().unwrap().tag, Some(tag));
    // Somebody else's file at the name is still refused, also after a retry.
    fake.insert("p/b.txt", b"theirs", &[]);
    let e = s.write_new(&rel("b.txt"), b"mine").unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
    fake.insert("p/c.txt", b"theirs", &[]);
    fake.refuse_once("put");
    let e = s.write_new(&rel("c.txt"), b"mine").unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::AlreadyExists, "{e}");
    assert_eq!(fake.get("p/c.txt").unwrap(), b"theirs");
    // The same bytes put by someone else are not ours either: only our token counts.
    fake.insert("p/d.txt", b"same", &[]);
    fake.refuse_once("put");
    assert_eq!(
        s.write_new(&rel("d.txt"), b"same").unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
}

#[test]
fn a_replace_whose_answer_was_lost_is_not_a_conflict() {
    let (s, fake, _d) = store();
    let t0 = s.write_new(&rel("s.json"), b"v0").unwrap();
    fake.lose_reply("put");
    let t1 = s.write_replace(&rel("s.json"), b"v1", Some(&t0)).unwrap();
    assert_ne!(t0, t1);
    assert_eq!(fake.get("p/s.json").unwrap(), b"v1");
    // Changed by someone else in between: still a conflict, after a retry too.
    fake.insert("p/s.json", b"other", &[]);
    fake.refuse_once("put");
    let e = s
        .write_replace(&rel("s.json"), b"v2", Some(&t1))
        .unwrap_err();
    assert!(is_conflict(&e), "{e}");
    assert_eq!(fake.get("p/s.json").unwrap(), b"other");
    // A missing bucket is not a conflict.
    fake.missing_bucket();
    let e = s
        .write_replace(&rel("s.json"), b"v3", Some(&t1))
        .unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::NotFound);
    assert!(!is_conflict(&e));
}

#[test]
fn a_small_upload_whose_answer_was_lost_is_finished() {
    let (s, fake, _d) = store();
    let mut up = s.begin_upload(&rel("x.bin"), 3, mtime(5)).unwrap();
    up.write(b"abc").unwrap();
    fake.lose_reply("put");
    up.finish_no_replace().unwrap();
    assert_eq!(fake.get("p/x.bin").unwrap(), b"abc");
    // A file that is somebody else's: refused.
    fake.insert("p/y.bin", b"abc", &[]);
    let mut up = s.begin_upload(&rel("y.bin"), 3, mtime(5)).unwrap();
    up.write(b"abc").unwrap();
    fake.refuse_once("put");
    let e = up.finish_no_replace().unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
}

#[test]
fn a_multipart_upload_whose_complete_was_lost_is_finished() {
    let (s, fake, _d) = store();
    let data = b"0123456789";
    let mut up = s.begin_upload(&rel("m.bin"), 10, mtime(7)).unwrap();
    up.write(data).unwrap();
    fake.lose_reply("complete");
    up.finish_no_replace().unwrap();
    assert_eq!(fake.count("complete"), 2);
    assert_eq!(fake.get("p/m.bin").unwrap(), data);
    assert!(fake.open_uploads().is_empty());
    // Somebody else's object at the name: refused, and the upload is dropped.
    fake.insert("p/n.bin", b"0123456789", &[]);
    let mut up = s.begin_upload(&rel("n.bin"), 10, mtime(7)).unwrap();
    up.write(data).unwrap();
    fake.refuse_once("complete");
    let e = up.finish_no_replace().unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::AlreadyExists, "{e}");
    assert!(fake.open_uploads().is_empty());
}

#[test]
fn a_copy_whose_answer_was_lost_is_done() {
    let (s, fake, _d) = store();
    // Small copy.
    fake.insert("p/a", b"1234", &[("pnk-mtime", "9")]);
    fake.lose_reply("copy");
    s.move_no_replace(&rel("a"), &rel("_Archived/a")).unwrap();
    assert_eq!(fake.get("p/_Archived/a").unwrap(), b"1234");
    assert!(fake.get("p/a").is_none());
    // Copy in parts.
    fake.insert("p/b", b"0123456789", &[("pnk-mtime", "9")]);
    fake.lose_reply("complete");
    s.move_no_replace(&rel("b"), &rel("_Archived/b")).unwrap();
    assert_eq!(fake.get("p/_Archived/b").unwrap(), b"0123456789");
    assert!(fake.get("p/b").is_none());
    // A different file at the destination is not taken for the copy.
    fake.insert("p/c", b"1234", &[("pnk-mtime", "9")]);
    fake.insert("p/_Archived/c", b"abcd", &[("pnk-mtime", "10")]);
    fake.refuse_once("copy");
    let e = s
        .move_no_replace(&rel("c"), &rel("_Archived/c"))
        .unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::AlreadyExists, "{e}");
    assert_eq!(fake.get("p/c").unwrap(), b"1234");
    // A source with nothing to tell it by never counts.
    fake.insert("p/d", b"1234", &[]);
    fake.insert("p/_Archived/d", b"1234", &[]);
    fake.refuse_once("copy");
    let e = s
        .move_no_replace(&rel("d"), &rel("_Archived/d"))
        .unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::AlreadyExists, "{e}");
    assert!(fake.get("p/d").is_some());
}

// ---- staging ----------------------------------------------------------------------------

#[test]
fn staging_puts_the_bytes_on_the_server_before_the_finish() {
    let (s, fake, _d) = store();
    let mut up = s.begin_upload(&rel("r.bin"), 3, mtime(1)).unwrap();
    up.write(b"abc").unwrap();
    up.seal().unwrap();
    // A plain seal keeps a small file in memory.
    assert!(fake.log().is_empty());
    assert_eq!(up.journal_ref(), "s3put:p/r.bin");
    up.stage().unwrap();
    up.stage().unwrap();
    assert_eq!(fake.count("create"), 1);
    assert_eq!(fake.count("part"), 1);
    assert_eq!(fake.count("put"), 0);
    assert!(up.journal_ref().starts_with("s3mpu:"));
    assert!(fake.get("p/r.bin").is_none());
    up.finish_no_replace().unwrap();
    assert_eq!(fake.get("p/r.bin").unwrap(), b"abc");
    // Only the completing request came after the staging.
    assert_eq!(fake.log().last().unwrap(), "complete p/r.bin");
    assert_eq!(fake.count("put"), 0);
    // An empty file stages too.
    let mut up = s.begin_upload(&rel("e.bin"), 0, None).unwrap();
    up.stage().unwrap();
    up.finish_no_replace().unwrap();
    assert_eq!(fake.get("p/e.bin").unwrap(), b"");
    // A big one is staged by its seal already.
    let mut up = s.begin_upload(&rel("big.bin"), 9, None).unwrap();
    up.write(b"012345678").unwrap();
    up.stage().unwrap();
    let parts = fake
        .log()
        .iter()
        .filter(|l| l.starts_with("part p/big.bin"))
        .count();
    assert_eq!(parts, 3);
    up.abort().unwrap();
    assert!(fake.open_uploads().is_empty());
}

#[test]
fn a_staged_upload_can_be_finished_from_its_reference() {
    let (s, fake, _d) = store();
    let stage = |name: &str, data: &[u8]| {
        let mut up = s
            .begin_upload(&rel(name), data.len() as u64, mtime(3))
            .unwrap();
        up.write(data).unwrap();
        up.stage().unwrap();
        let r = up.journal_ref();
        std::mem::forget(up);
        r
    };
    let r = stage("f.bin", b"0123456789");
    assert!(fake.get("p/f.bin").is_none());
    s.finish_by_ref(&r, 10).unwrap();
    assert_eq!(fake.get("p/f.bin").unwrap(), b"0123456789");
    assert!(fake.open_uploads().is_empty());
    // The wrong size: nothing is finished and the upload is left for abort_by_ref.
    let r = stage("g.bin", b"abc");
    let e = s.finish_by_ref(&r, 4).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidData);
    assert!(fake.get("p/g.bin").is_none());
    assert_eq!(fake.open_uploads().len(), 1);
    s.abort_by_ref(&r).unwrap();
    assert!(fake.open_uploads().is_empty());
    // A name that has been taken since: refused, never replaced, the upload is gone.
    let r = stage("h.bin", b"abc");
    fake.insert("p/h.bin", b"theirs", &[]);
    let e = s.finish_by_ref(&r, 3).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(fake.get("p/h.bin").unwrap(), b"theirs");
    assert!(fake.open_uploads().is_empty());
    // An upload that is not there.
    let e = s.finish_by_ref(&r, 3).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::NotFound);
    // A single request reference has nothing to finish.
    let e = s.finish_by_ref("s3put:p/h.bin", 3).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::Unsupported);
}

// ---- prefixes and references ---------------------------------------------------------------

#[test]
fn references_outside_the_prefix_or_not_plain_paths_send_nothing() {
    let (s, fake, _d) = store();
    for bad in [
        "s3mpu:id:p/../x",
        "s3mpu:id:p/./x",
        "s3mpu:id:p//x",
        "s3mpu:id:p/",
        "s3mpu:id:p/a\\b",
        "s3mpu:id:q/x",
        "s3put:p/../x",
        "s3put:p/x.pnk-part",
    ] {
        assert_eq!(
            s.abort_by_ref(bad).unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
            "{bad}"
        );
        assert_eq!(
            s.finish_by_ref(bad, 1).unwrap_err().kind(),
            io::ErrorKind::InvalidInput,
            "{bad}"
        );
    }
    assert!(fake.log().is_empty(), "{:?}", fake.log());
}

#[test]
fn cleaning_stale_uploads_skips_keys_that_are_not_plain_paths() {
    let (s, fake, _d) = store();
    fake.insert_upload("p/a/b.bin", "good");
    fake.insert_upload("p/../evil", "dots");
    fake.insert_upload("p//evil", "empty");
    fake.insert_upload("p/x\\y", "slash");
    assert_eq!(s.clean_stale_uploads(Duration::ZERO).unwrap(), 1);
    assert_eq!(fake.count("abort"), 1);
    assert_eq!(fake.open_uploads().len(), 3);
}

#[test]
fn prefixes_with_dots_empty_parts_or_backslashes_are_refused() {
    for bad in ["a/../b", "..", "a/./b", "a//b", "a\\b", "a/b\\", "x/\u{7}y"] {
        let (_d, mut cfg) = offline_config();
        cfg.prefix = bad.into();
        assert!(S3Store::new(cfg).is_err(), "{bad:?} was accepted");
    }
    for good in ["", "/", "a", "/a/b/", "a b/c d", "p//", "//p"] {
        let (_d, mut cfg) = offline_config();
        cfg.prefix = good.into();
        assert!(S3Store::new(cfg).is_ok(), "{good:?}");
    }
}

// ---- integrity --------------------------------------------------------------------------------

#[test]
fn every_write_carries_a_checksum_the_server_can_refuse() {
    let (s, fake, _d) = store();
    fake.corrupt_next_body();
    let e = s.write_new(&rel("a"), b"hello").unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{e}");
    assert!(fake.get("p/a").is_none());
    // A part too.
    let mut up = s.begin_upload(&rel("b"), 9, None).unwrap();
    fake.corrupt_next_body();
    assert!(up.write(b"012345678").is_err());
    up.abort().unwrap();
    // And a good one goes through.
    s.write_new(&rel("a"), b"hello").unwrap();
}

// ---- the self-test -------------------------------------------------------------------------------

#[test]
fn a_server_that_honours_conditions_passes_the_self_test_once() {
    let (s, fake, _d) = store();
    assert!(s.root_exists().unwrap());
    assert!(fake.keys().is_empty(), "the probe object is deleted");
    let puts = fake.count("put");
    // The probe object, its second and stale writes, a taken key, and a wrong checksum.
    assert_eq!(puts, 5);
    assert_eq!(fake.count("copy"), 2);
    assert_eq!(s.server_note(), None);
    assert!(
        fake.log()
            .iter()
            .any(|l| l.starts_with("put p/.plug-n-keep-probe-"))
    );
    assert!(s.root_exists().unwrap());
    assert!(s.ensure_root().is_ok());
    assert_eq!(fake.count("put"), puts, "asked once");
}

#[test]
fn a_server_that_ignores_conditions_is_refused() {
    let (s, fake, _d) = store();
    fake.ignore_conditions();
    for _ in 0..2 {
        let e = s.root_exists().unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Unsupported);
        assert!(
            e.to_string()
                .contains("does not support conditional writes"),
            "{e}"
        );
    }
    assert!(s.ensure_root().is_err());
    assert!(fake.keys().is_empty(), "the probe object is deleted");
    // Asked once, then remembered.
    assert_eq!(fake.count("put"), 2);
}

#[test]
fn a_server_that_ignores_copy_conditions_is_refused() {
    let (s, fake, _d) = store();
    fake.ignore_copy_conditions();
    let e = s.root_exists().unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::Unsupported);
    assert!(fake.keys().is_empty(), "the probe objects are deleted");
}

#[test]
fn a_server_without_checksums_is_used_without_them_and_says_so() {
    let (s, fake, _d) = store();
    fake.reject_checksums();
    assert!(s.root_exists().unwrap());
    assert!(
        s.server_note()
            .unwrap()
            .contains("doesn't check upload checksums")
    );
    assert!(fake.keys().is_empty());
    // Uploads work, without the header.
    s.write_new(&rel("a"), b"hello").unwrap();
    assert_eq!(fake.get("p/a").unwrap(), b"hello");
}

#[test]
fn a_server_that_does_not_verify_checksums_is_used_and_says_so() {
    let (s, fake, _d) = store();
    fake.skip_checksum_check();
    assert!(s.root_exists().unwrap());
    assert!(s.server_note().unwrap().contains("doesn't verify"));
    assert!(fake.keys().is_empty(), "the probe objects are deleted");
}

#[test]
fn the_probe_object_is_an_app_file_not_backup_content() {
    assert!(crate::owner::is_marker_name(".plug-n-keep-probe-abc123"));
    assert!(crate::owner::is_marker_name(".Plug-N-Keep-Probe-ABC"));
    assert!(!crate::owner::is_marker_name("probe.txt"));
}

// ---- small things ---------------------------------------------------------------------------------

#[test]
fn a_missing_size_is_an_error_not_zero() {
    let (s, fake, _d) = store();
    fake.insert("p/a", b"", &[]);
    fake.head_without_length();
    assert!(s.stat(&rel("a")).is_err());
    assert!(s.move_no_replace(&rel("a"), &rel("b2")).is_err());
    assert!(fake.get("p/a").is_some(), "nothing was moved");
}
