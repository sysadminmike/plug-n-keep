//! S3Store tests. The ones that need a server are skipped unless `PNK_S3_TEST=1`; they use
//! `PNK_S3_ENDPOINT`, `PNK_S3_BUCKET` (default `pnk-test`), `PNK_S3_PROFILE` (default
//! `pnk-test`) and `AWS_SHARED_CREDENTIALS_FILE`. Each test works under its own random
//! `itest/<id>/` prefix and deletes only what is under it.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use rusty_s3::Method;

use super::*;
use crate::store::{is_conflict, is_offline, tests as shared};

pub(super) fn rel(s: &str) -> SafeRelPath {
    SafeRelPath::parse(s).unwrap()
}

const MIB: usize = 1024 * 1024;

/// Makes a new file (the scan for permanent writes only allows creating).
fn write_file(path: &std::path::Path, text: &str) {
    use std::io::Write;
    std::fs::File::create_new(path)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}

// ---- no server needed ------------------------------------------------------------------------

#[test]
fn prefixes_are_normalised() {
    assert_eq!(normalise_prefix(""), "");
    assert_eq!(normalise_prefix("/"), "");
    assert_eq!(normalise_prefix("phones"), "phones/");
    assert_eq!(normalise_prefix("/phones/iphone/"), "phones/iphone/");
    assert_eq!(normalise_prefix("a//"), "a/");
}

#[test]
fn etags_lose_their_quotes() {
    assert_eq!(normalise_etag("\"abc\""), "abc");
    assert_eq!(normalise_etag("W/\"abc\""), "abc");
    assert_eq!(normalise_etag("abc-3"), "abc-3");
}

#[test]
fn copy_sources_are_encoded() {
    assert_eq!(copy_source("b", "a/x y&z.jpg"), "/b/a/x%20y%26z.jpg");
    assert_eq!(copy_source("b", "p/é"), "/b/p/%C3%A9");
}

#[test]
fn parts_are_cut_to_size() {
    assert_eq!(ranges(0, 10), vec![]);
    assert_eq!(ranges(10, 10), vec![(0, 9)]);
    assert_eq!(ranges(25, 10), vec![(0, 9), (10, 19), (20, 24)]);
}

#[test]
fn pruning_asks_once_per_folder_and_stops_at_the_first_yes() {
    let asked = std::cell::RefCell::new(Vec::<String>::new());
    let prune = |d: &str| {
        asked.borrow_mut().push(d.to_string());
        d == "a/_Archived"
    };
    let mut verdicts = HashMap::new();
    assert!(!pruned("top.txt", &prune, &mut verdicts));
    assert!(!pruned("a/b.txt", &prune, &mut verdicts));
    assert!(pruned("a/_Archived/x/y.txt", &prune, &mut verdicts));
    assert!(pruned("a/_Archived/z.txt", &prune, &mut verdicts));
    assert!(!pruned("a/c/d.txt", &prune, &mut verdicts));
    assert_eq!(*asked.borrow(), vec!["a", "a/_Archived", "a/c"]);
}

pub(crate) fn offline_config() -> (tempfile::TempDir, S3Config) {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("credentials");
    write_file(
        &f,
        "[t]\naws_access_key_id = TESTKEYID\naws_secret_access_key = TESTSECRETVALUE\n",
    );
    let cfg = S3Config {
        provider: Default::default(),
        account_id: None,
        endpoint: "http://127.0.0.1:1".into(),
        region: "us-east-1".into(),
        bucket: "b".into(),
        prefix: "/p//".into(),
        profile: Some("t".into()),
        credentials_file: Some(f),
        path_style: true,
        ca_file: None,
    };
    (dir, cfg)
}

#[test]
fn a_closed_port_is_offline_and_the_message_is_clean() {
    let (_d, cfg) = offline_config();
    let s = S3Store::new(cfg).unwrap().fast_retries();
    assert_eq!(s.prefix(), "p/");
    assert_eq!(s.describe(), "s3://b/p/ on http://127.0.0.1:1");
    for e in [
        s.root_exists().unwrap_err(),
        s.ensure_root().unwrap_err(),
        s.stat(&rel("a")).unwrap_err(),
        s.read(&rel("a")).unwrap_err(),
        s.write_new(&rel("a"), b"x").unwrap_err(),
        s.list(&|_| false).unwrap_err(),
        s.clean_stale_uploads(Duration::ZERO).unwrap_err(),
    ] {
        assert!(is_offline(&e), "{e}");
        let text = e.to_string();
        assert!(text.contains("127.0.0.1:1"), "{text}");
        assert!(
            !text.contains("TESTKEYID") && !text.contains("TESTSECRET"),
            "{text}"
        );
    }
}

#[test]
fn an_endpoint_with_userinfo_a_query_or_a_fragment_is_refused_without_echoing_it() {
    for bad in [
        "http://user:SECRETPW@host:9000",
        "https://user@host",
        "http://host:9000/path?x=1",
        "http://host:9000#frag",
        "http://host@evil:9000/x",
    ] {
        let (_d, mut cfg) = offline_config();
        cfg.endpoint = bad.into();
        let e = S3Store::new(cfg).err().expect(bad);
        assert!(e.contains("credentials file"), "{e}");
        assert!(!e.contains("SECRETPW") && !e.contains("evil"), "{e}");
    }
    // An `@` further along the path is not userinfo.
    let (_d, mut cfg) = offline_config();
    cfg.endpoint = "http://host:9000/a@b".into();
    assert!(S3Store::new(cfg).is_ok());
}

#[test]
fn journal_refs_are_checked_without_a_server() {
    let (_d, cfg) = offline_config();
    let s = S3Store::new(cfg).unwrap().fast_retries();
    // A single-PUT upload has nothing to clean.
    let up = s.begin_upload(&rel("a.txt"), 3, None).unwrap();
    let r = up.journal_ref();
    assert_eq!(r, "s3put:p/a.txt");
    s.abort_by_ref(&r).unwrap();
    // Refs for other places or of other kinds are refused before any request.
    for bad in [
        "s3mpu:id:other/key",
        "s3mpu::p/key",
        "s3mpu:nokey",
        "s3put:other/key",
        "upload-1",
        "",
    ] {
        let e = s.abort_by_ref(bad).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "{bad}");
    }
    assert_eq!(s.planned_ref(&rel("a")), None);
    assert!(!s.supports_bin());
    assert!(s.space().is_none());
    assert_eq!(
        s.send_to_bin(&rel("a")).unwrap_err().kind(),
        io::ErrorKind::Unsupported
    );
}

#[test]
fn config_round_trips_without_secrets() {
    let (_d, cfg) = offline_config();
    let json = serde_json::to_string(&cfg).unwrap();
    assert!(!json.contains("SECRET") && !json.contains("TESTKEYID"));
    let back: S3Config = serde_json::from_str(&json).unwrap();
    assert_eq!(back, cfg);
    let min: S3Config =
        serde_json::from_str(r#"{"endpoint":"http://h","region":"r","bucket":"b"}"#).unwrap();
    assert_eq!(min.prefix, "");
    assert!(!min.path_style);
}

// ---- needs the test server ------------------------------------------------------------------------

/// A store under a fresh prefix. Dropping it deletes everything under that prefix.
pub(crate) struct Fixture {
    store: S3Store,
}

impl std::ops::Deref for Fixture {
    type Target = S3Store;
    fn deref(&self) -> &S3Store {
        &self.store
    }
}

fn fixture_with(limits: Limits) -> Option<Fixture> {
    if std::env::var("PNK_S3_TEST").ok().as_deref() != Some("1") {
        eprintln!("skipped: PNK_S3_TEST is not 1");
        return None;
    }
    let var = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    let id = uuid::Uuid::new_v4().simple().to_string();
    let cfg = S3Config {
        provider: Default::default(),
        account_id: None,
        endpoint: std::env::var("PNK_S3_ENDPOINT").expect("PNK_S3_ENDPOINT"),
        region: "us-east-1".into(),
        bucket: var("PNK_S3_BUCKET", "pnk-test"),
        prefix: format!("itest/{id}/"),
        profile: Some(var("PNK_S3_PROFILE", "pnk-test")),
        credentials_file: None,
        path_style: true,
        ca_file: None,
    };
    assert_eq!(
        cfg.bucket, "pnk-test",
        "tests only ever touch bucket pnk-test"
    );
    Some(Fixture {
        store: S3Store::with_limits(cfg, limits).unwrap().fast_retries(),
    })
}

pub(crate) fn fixture() -> Option<Fixture> {
    fixture_with(Limits::default())
}

/// Smaller parts so a few MiB go multipart (S3 still needs 5 MiB for all but the last).
fn small_parts() -> Limits {
    Limits {
        part_size: 5 * MIB,
        copy_limit: 5 * MIB as u64,
        copy_part: 5 * MIB as u64,
        ..Limits::default()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.store.purge_test_prefix();
    }
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

fn open_uploads(f: &Fixture) -> Vec<(String, String)> {
    let u = f.inner.list_uploads(None).unwrap();
    u.items
        .into_iter()
        .filter(|(k, _, _)| k.starts_with(&f.inner.prefix))
        .map(|(k, i, _)| (k, i))
        .collect()
}

#[test]
fn shared_store_rules_hold() {
    if fixture().is_none() {
        return;
    }
    shared::run_all(|| {
        let f = fixture().unwrap();
        (Box::new(f.store.clone()), Box::new(f))
    });
}

#[test]
fn shared_store_rules_hold_with_multipart_uploads() {
    // The same rules, but the uploads of the shared checks are tiny, so also run a few of the
    // same shapes over parts.
    let Some(f) = fixture_with(small_parts()) else {
        return;
    };
    let data = pattern(6 * MIB, 1);
    let when = UNIX_EPOCH + Duration::from_millis(1_650_000_321_123);
    let mut up = f
        .begin_upload(&rel("d/big.bin"), data.len() as u64, Some(when))
        .unwrap();
    assert!(up.journal_ref().starts_with("s3mpu:"));
    up.write(&data).unwrap();
    // Invisible until finished.
    assert!(f.stat(&rel("d/big.bin")).unwrap().is_none());
    assert!(f.list(&|_| false).unwrap().files.is_empty());
    up.seal().unwrap();
    up.seal().unwrap();
    assert!(up.write(b"x").is_err());
    assert!(f.stat(&rel("d/big.bin")).unwrap().is_none());
    let tag = up.finish_no_replace().unwrap();
    let st = f.stat(&rel("d/big.bin")).unwrap().unwrap();
    assert_eq!(
        (st.size, st.mtime, st.tag),
        (data.len() as u64, Some(when), Some(tag))
    );
    assert_eq!(f.read(&rel("d/big.bin")).unwrap().unwrap().0, data);
    assert!(open_uploads(&f).is_empty());

    // Wrong size: the upload is removed.
    let mut up = f.begin_upload(&rel("w.bin"), 6 * MIB as u64, None).unwrap();
    up.write(&data[..5 * MIB + 10]).unwrap();
    assert_eq!(up.seal().unwrap_err().kind(), io::ErrorKind::InvalidData);
    assert!(open_uploads(&f).is_empty());
    // Too much.
    let mut up = f
        .begin_upload(&rel("w2.bin"), 6 * MIB as u64, None)
        .unwrap();
    assert!(up.write(&pattern(6 * MIB + 1, 0)).is_err());
    up.abort().unwrap();
    assert!(open_uploads(&f).is_empty());
}

#[test]
fn a_forty_mib_file_goes_up_in_real_sized_parts() {
    let Some(f) = fixture() else { return };
    let data = pattern(40 * MIB, 7);
    let mut up = f
        .begin_upload(&rel("v/video.mov"), data.len() as u64, None)
        .unwrap();
    let r = up.journal_ref();
    for chunk in data.chunks(MIB) {
        up.write(chunk).unwrap();
    }
    assert_eq!(open_uploads(&f).len(), 1);
    assert!(r.starts_with("s3mpu:"));
    up.finish_no_replace().unwrap();
    assert!(open_uploads(&f).is_empty());
    let got = f.read(&rel("v/video.mov")).unwrap().unwrap().0;
    assert!(got == data, "content differs");
}

#[test]
fn abort_leaves_no_object_and_no_pending_upload() {
    let Some(f) = fixture_with(small_parts()) else {
        return;
    };
    let mut up = f
        .begin_upload(&rel("x.bin"), 12 * MIB as u64, None)
        .unwrap();
    up.write(&pattern(11 * MIB, 3)).unwrap();
    assert_eq!(open_uploads(&f).len(), 1);
    up.abort().unwrap();
    assert!(open_uploads(&f).is_empty());
    assert!(f.stat(&rel("x.bin")).unwrap().is_none());
    // From the crash record, twice (already gone is fine).
    let up = f
        .begin_upload(&rel("y.bin"), 12 * MIB as u64, None)
        .unwrap();
    let r = up.journal_ref();
    drop(up);
    assert_eq!(open_uploads(&f).len(), 1);
    f.abort_by_ref(&r).unwrap();
    f.abort_by_ref(&r).unwrap();
    assert!(open_uploads(&f).is_empty());
    // A ref for a key outside the prefix is refused.
    let e = f.abort_by_ref("s3mpu:someid:other/place.bin").unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
}

#[test]
fn finish_is_refused_when_the_key_exists() {
    let Some(f) = fixture_with(small_parts()) else {
        return;
    };
    let mut up = f.begin_upload(&rel("x.bin"), 6 * MIB as u64, None).unwrap();
    up.write(&pattern(6 * MIB, 1)).unwrap();
    up.seal().unwrap();
    f.write_new(&rel("x.bin"), b"theirs").unwrap();
    let e = up.finish_no_replace().unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::AlreadyExists, "{e}");
    assert_eq!(f.read(&rel("x.bin")).unwrap().unwrap().0, b"theirs");
    assert!(open_uploads(&f).is_empty());
}

#[test]
fn a_move_copies_verifies_then_deletes_the_source() {
    let Some(f) = fixture() else { return };
    let when = UNIX_EPOCH + Duration::from_millis(1_600_000_000_456);
    let mut up = f.begin_upload(&rel("a/p q.jpg"), 4, Some(when)).unwrap();
    up.write(b"jpeg").unwrap();
    let t1 = up.finish_no_replace().unwrap();
    f.move_no_replace(&rel("a/p q.jpg"), &rel("_Archived/s/a/p q.jpg"))
        .unwrap();
    assert!(f.stat(&rel("a/p q.jpg")).unwrap().is_none());
    let st = f.stat(&rel("_Archived/s/a/p q.jpg")).unwrap().unwrap();
    assert_eq!((st.size, st.mtime), (4, Some(when)));
    let _ = t1;
    // Missing source, taken destination.
    let e = f.move_no_replace(&rel("none"), &rel("n2")).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::NotFound);
    f.write_new(&rel("x"), b"x").unwrap();
    f.write_new(&rel("y"), b"y").unwrap();
    let e = f.move_no_replace(&rel("x"), &rel("y")).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(f.read(&rel("x")).unwrap().unwrap().0, b"x");
    assert_eq!(f.read(&rel("y")).unwrap().unwrap().0, b"y");
}

#[test]
fn a_big_move_uses_part_copies() {
    let Some(f) = fixture_with(small_parts()) else {
        return;
    };
    let data = pattern(12 * MIB, 9);
    let when = UNIX_EPOCH + Duration::from_millis(1_500_000_000_001);
    let mut up = f
        .begin_upload(&rel("big.bin"), data.len() as u64, Some(when))
        .unwrap();
    up.write(&data).unwrap();
    up.finish_no_replace().unwrap();
    f.move_no_replace(&rel("big.bin"), &rel("_Archived/big.bin"))
        .unwrap();
    assert!(f.stat(&rel("big.bin")).unwrap().is_none());
    let st = f.stat(&rel("_Archived/big.bin")).unwrap().unwrap();
    assert_eq!((st.size, st.mtime), (data.len() as u64, Some(when)));
    assert!(f.read(&rel("_Archived/big.bin")).unwrap().unwrap().0 == data);
    assert!(open_uploads(&f).is_empty());
    // A taken destination: refused, no upload left.
    let mut up = f
        .begin_upload(&rel("d.bin"), data.len() as u64, None)
        .unwrap();
    up.write(&data).unwrap();
    up.finish_no_replace().unwrap();
    let e = f
        .move_no_replace(&rel("d.bin"), &rel("_Archived/big.bin"))
        .unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::AlreadyExists, "{e}");
    assert!(f.stat(&rel("d.bin")).unwrap().is_some());
    assert!(open_uploads(&f).is_empty());
}

#[test]
fn a_changed_source_is_not_copied() {
    for limits in [Limits::default(), small_parts()] {
        let Some(f) = fixture_with(limits) else {
            return;
        };
        let size = if limits.copy_limit < 10 * MIB as u64 {
            6 * MIB
        } else {
            10
        };
        f.write_new(&rel("src"), &pattern(size, 1)).unwrap();
        let key = f.inner.key(&rel("src"));
        let stale = f.inner.head(&key).unwrap().unwrap();
        f.write_replace(&rel("src"), &pattern(size, 2), None)
            .unwrap();
        let to = f.inner.key(&rel("dst"));
        let e = f.inner.copy(&key, &to, &stale).unwrap_err();
        assert_ne!(e.kind(), io::ErrorKind::AlreadyExists, "{e}");
        assert!(f.stat(&rel("dst")).unwrap().is_none());
        assert!(open_uploads(&f).is_empty());
        // Still there and unchanged.
        assert_eq!(f.read(&rel("src")).unwrap().unwrap().0, pattern(size, 2));
    }
}

#[test]
fn stale_uploads_are_cleaned_up_but_fresh_ones_are_kept() {
    let Some(f) = fixture_with(small_parts()) else {
        return;
    };
    let mut up = f
        .begin_upload(&rel("old.bin"), 6 * MIB as u64, None)
        .unwrap();
    up.write(&pattern(6 * MIB, 4)).unwrap();
    drop(up);
    assert_eq!(f.clean_stale_uploads(Duration::from_secs(3600)).unwrap(), 0);
    assert_eq!(open_uploads(&f).len(), 1);
    std::thread::sleep(Duration::from_millis(2200));
    assert_eq!(f.clean_stale_uploads(Duration::from_secs(1)).unwrap(), 1);
    assert!(open_uploads(&f).is_empty());
}

#[test]
fn listing_pages_skips_placeholders_and_reports_bad_names() {
    let Some(mut f) = fixture() else { return };
    // Two keys per page, so the listing has to continue.
    Arc::get_mut(&mut f.store.inner).unwrap().limits.list_page = 2;
    for n in [
        "a/1.txt",
        "a/2.txt",
        "a/3.txt",
        "b/c/4.txt",
        "_Archived/x/5.txt",
    ] {
        f.write_new(&rel(n), b"1").unwrap();
    }
    // A folder placeholder, and a name the app can't use.
    f.inner
        .put(&format!("{}a/", f.inner.prefix), b"", None, "t")
        .unwrap();
    f.inner
        .put(&format!("{}bad:name.txt", f.inner.prefix), b"1", None, "t")
        .unwrap();
    let l = f.list(&|_| false).unwrap();
    let mut names: Vec<_> = l.files.iter().map(|x| x.rel.clone()).collect();
    names.sort();
    assert_eq!(
        names,
        [
            "_Archived/x/5.txt",
            "a/1.txt",
            "a/2.txt",
            "a/3.txt",
            "b/c/4.txt"
        ]
    );
    assert_eq!(l.issues.len(), 1, "{:?}", l.issues);
    assert!(l.issues[0].at.ends_with("bad:name.txt"));
    assert!(
        l.files
            .iter()
            .all(|x| x.kind == Kind::File && x.tag.is_some() && x.size == 1)
    );
    let l = f.list(&|d| d.ends_with("_Archived")).unwrap();
    assert_eq!(l.files.len(), 4);
}

#[test]
fn tags_agree_between_write_stat_list_and_read() {
    let Some(f) = fixture() else { return };
    let t = f.write_new(&rel("s.json"), b"{}").unwrap();
    assert_eq!(
        f.stat(&rel("s.json")).unwrap().unwrap().tag,
        Some(t.clone())
    );
    assert_eq!(f.read(&rel("s.json")).unwrap().unwrap().1, t);
    assert_eq!(f.list(&|_| false).unwrap().files[0].tag, Some(t));
}

#[test]
fn root_checks() {
    let Some(f) = fixture() else { return };
    assert!(f.root_exists().unwrap());
    f.ensure_root().unwrap();
    let mut cfg = offline_cfg_from(&f);
    cfg.bucket = "pnk-no-such-bucket-xyz".into();
    let s = S3Store::new(cfg).unwrap().fast_retries();
    match s.root_exists() {
        Ok(false) => {}
        other => panic!("expected a missing bucket, got {other:?}"),
    }
    assert_eq!(s.ensure_root().unwrap_err().kind(), io::ErrorKind::NotFound);
}

fn offline_cfg_from(f: &Fixture) -> S3Config {
    S3Config {
        provider: Default::default(),
        account_id: None,
        endpoint: f.inner.client.endpoint.clone(),
        region: "us-east-1".into(),
        bucket: f.inner.client.bucket.name().to_string(),
        prefix: "itest/none/".into(),
        profile: Some(std::env::var("PNK_S3_PROFILE").unwrap_or("pnk-test".into())),
        credentials_file: None,
        path_style: true,
        ca_file: None,
    }
}

#[test]
fn concurrent_conditional_writes_have_one_winner() {
    let Some(f) = fixture() else { return };
    let wins = AtomicUsize::new(0);
    let taken = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for i in 0..8u8 {
            let (f, wins, taken) = (&f, &wins, &taken);
            s.spawn(move || match f.write_new(&rel("race"), &[i]) {
                Ok(_) => wins.fetch_add(1, Ordering::SeqCst),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    taken.fetch_add(1, Ordering::SeqCst)
                }
                Err(e) => panic!("{e}"),
            });
        }
    });
    assert_eq!(
        (wins.load(Ordering::SeqCst), taken.load(Ordering::SeqCst)),
        (1, 7)
    );
    // And replaces that all expect the same version: at most... exactly one wins.
    let t = f.write_replace(&rel("ver"), b"0", None).unwrap();
    let wins = AtomicUsize::new(0);
    let lost = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for i in 0..8u8 {
            let (f, wins, lost, t) = (&f, &wins, &lost, &t);
            s.spawn(move || match f.write_replace(&rel("ver"), &[i], Some(t)) {
                Ok(_) => wins.fetch_add(1, Ordering::SeqCst),
                Err(e) if is_conflict(&e) => lost.fetch_add(1, Ordering::SeqCst),
                Err(e) => panic!("{e}"),
            });
        }
    });
    let w = wins.load(Ordering::SeqCst);
    assert_eq!(w + lost.load(Ordering::SeqCst), 8);
    assert_eq!(w, 1, "exactly one writer may win a conditional replace");
}

/// Prints what the server does for the calls the app leans on (run with `--nocapture`), and
/// asserts the ones the code depends on.
#[test]
fn server_behaviour_probe() {
    let Some(f) = fixture_with(small_parts()) else {
        return;
    };
    let inner = &f.inner;
    let c = &inner.client;
    let say = |what: &str, v: String| eprintln!("PROBE {what}: {v}");

    // DeleteObject with If-Match: stale tag.
    f.write_new(&rel("d1"), b"one").unwrap();
    let stale = "\"00000000000000000000000000000000\"";
    let key = inner.key(&rel("d1"));
    let r = c
        .exec(Method::Delete, &[("if-match", stale)], &[], |h| {
            Client::presign(c.bucket.delete_object(Some(&c.creds), &key), h)
        })
        .unwrap();
    let still = f.stat(&rel("d1")).unwrap().is_some();
    say(
        "DELETE with stale If-Match",
        format!("status {}, object still there: {still}", r.status),
    );
    // DeleteObject with If-Match: right tag.
    let tag = f.stat(&rel("d1")).unwrap().unwrap().tag.unwrap();
    let q = format!("\"{tag}\"");
    let r = c
        .exec(Method::Delete, &[("if-match", q.as_str())], &[], |h| {
            Client::presign(c.bucket.delete_object(Some(&c.creds), &key), h)
        })
        .unwrap();
    say(
        "DELETE with matching If-Match",
        format!(
            "status {}, gone: {}",
            r.status,
            f.stat(&rel("d1")).unwrap().is_none()
        ),
    );

    // Copy with x-amz-copy-source-if-match: stale tag.
    f.write_new(&rel("c1"), b"one").unwrap();
    let ckey = inner.key(&rel("c1"));
    let dkey = inner.key(&rel("c2"));
    let src = copy_source(c.bucket.name(), &ckey);
    let hdrs = [
        ("if-none-match", "*"),
        ("x-amz-copy-source", src.as_str()),
        ("x-amz-copy-source-if-match", stale),
    ];
    let r = c
        .exec(Method::Put, &hdrs, &[], |h| {
            Client::presign(c.bucket.put_object(Some(&c.creds), &dkey), h)
        })
        .unwrap();
    let made = f.stat(&rel("c2")).unwrap().is_some();
    say(
        "COPY with stale x-amz-copy-source-if-match",
        format!(
            "status {} {}, dest made: {made}",
            r.status,
            xml::error_code(&r.text()).unwrap_or_default()
        ),
    );
    assert!(!made, "the server must honour x-amz-copy-source-if-match");

    // Conditional write statuses.
    let r = inner
        .put(&ckey, b"x", Some(("if-none-match", "*")), "t")
        .unwrap()
        .0;
    say(
        "PUT If-None-Match: * on existing key",
        format!("status {}", r.status),
    );
    let r = inner
        .put(&ckey, b"x", Some(("if-match", stale)), "t")
        .unwrap()
        .0;
    say("PUT with stale If-Match", format!("status {}", r.status));
    let missing = inner.key(&rel("no-such"));
    let r = inner
        .put(&missing, b"x", Some(("if-match", stale)), "t")
        .unwrap()
        .0;
    say(
        "PUT If-Match on a missing key",
        format!(
            "status {} (made: {})",
            r.status,
            f.stat(&rel("no-such")).unwrap().is_some()
        ),
    );

    // Statuses seen when 8 threads race the same conditional PUT.
    let statuses = std::sync::Mutex::new(Vec::new());
    let k = inner.key(&rel("race"));
    std::thread::scope(|s| {
        for i in 0..8u8 {
            let (statuses, k) = (&statuses, &k);
            s.spawn(move || {
                let st = inner
                    .put(k, &[i], Some(("if-none-match", "*")), "t")
                    .unwrap()
                    .0
                    .status;
                statuses.lock().unwrap().push(st);
            });
        }
    });
    let mut v = statuses.into_inner().unwrap();
    v.sort();
    say("8 racing PUT If-None-Match: *", format!("{v:?}"));

    // ListMultipartUploads.
    let up = f
        .begin_upload(&rel("mp.bin"), 6 * MIB as u64, None)
        .unwrap();
    let listed = open_uploads(&f);
    say(
        "ListMultipartUploads",
        format!("{} upload(s) listed under the prefix", listed.len()),
    );
    let all = inner.list_uploads(None).unwrap();
    say(
        "ListMultipartUploads initiated time vs local clock",
        format!(
            "{:?}",
            all.items
                .iter()
                .find(|(k, _, _)| k.ends_with("mp.bin"))
                .map(|(_, _, t)| t
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0)
                    - std::time::SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs() as i64)
        ),
    );
    assert_eq!(listed.len(), 1);
    up.abort().unwrap();
}

/// The server refuses bytes whose SHA-256 is not the one sent, for a plain PUT and for a part,
/// and keeps the checksum of what it was given.
#[test]
fn the_server_checks_the_checksum_it_is_sent() {
    let Some(f) = fixture() else { return };
    let inner = &f.inner;
    let c = &inner.client;
    let key = inner.key(&rel("sum1"));
    let wrong = sum::sha256_b64(b"something else");
    let hdrs = [(H_SUM, wrong.as_str())];
    let r = c
        .exec(Method::Put, &hdrs, b"hello", |h| {
            Client::presign(c.bucket.put_object(Some(&c.creds), &key), h)
        })
        .unwrap();
    eprintln!(
        "PROBE PUT with a wrong checksum: status {} {}",
        r.status,
        xml::error_code(&r.text()).unwrap_or_default()
    );
    assert_eq!(r.status, 400, "the server must refuse a wrong checksum");
    assert!(f.stat(&rel("sum1")).unwrap().is_none());
    // A part.
    let id = inner.create_multipart(&key, None, None).unwrap();
    let r = c
        .exec(Method::Put, &hdrs, b"hello", |h| {
            Client::presign(c.bucket.upload_part(Some(&c.creds), &key, 1, &id), h)
        })
        .unwrap();
    eprintln!("PROBE part with a wrong checksum: status {}", r.status);
    assert_eq!(r.status, 400);
    inner.abort_multipart(&key, &id).unwrap();
    // The right one is kept and given back on a HEAD.
    f.write_new(&rel("sum2"), b"hello").unwrap();
    let h = inner.head(&inner.key(&rel("sum2"))).unwrap().unwrap();
    eprintln!("PROBE stored checksum on HEAD: {:?}", h.checksum);
    assert_eq!(
        h.checksum.as_deref(),
        Some(sum::sha256_b64(b"hello").as_str())
    );
    assert!(h.token.is_some());
}

/// The first `root_exists` runs the conditional-write self-test and leaves nothing behind.
#[test]
fn the_server_passes_the_self_test_and_it_leaves_nothing() {
    let Some(f) = fixture() else { return };
    assert!(f.root_exists().unwrap());
    assert!(f.root_exists().unwrap());
    assert!(f.list(&|_| false).unwrap().files.is_empty());
    assert!(open_uploads(&f).is_empty());
}

#[test]
fn read_and_stat_missing_keys_are_none() {
    let Some(f) = fixture() else { return };
    assert!(f.read(&rel("none/x")).unwrap().is_none());
    assert!(f.stat(&rel("none/x")).unwrap().is_none());
}

#[test]
fn bad_credentials_are_denied_without_leaking_them() {
    let Some(f) = fixture() else { return };
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("c");
    write_file(
        &file,
        "[x]\naws_access_key_id = WRONGKEYID123\naws_secret_access_key = WRONGSECRET456\n",
    );
    let mut cfg = offline_cfg_from(&f);
    cfg.profile = Some("x".into());
    cfg.credentials_file = Some(file);
    let s = S3Store::new(cfg).unwrap().fast_retries();
    let e = s.read(&rel("a")).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e}");
    assert!(!e.to_string().contains("WRONG"));
}
