//! The sync, scan and recovery paths against object stores: an in-memory one ([`MemStore`]: no
//! folders, no rename, uploads that appear only when finished, no bin) and, when `PNK_S3_TEST=1`,
//! a real bucket ([`S3Store`] under its own `itest/<random>/` prefix, removed afterwards).
//!
//! The tests that need only the store's own rules run on every backend through [`Backend`]; the
//! ones that inject failures run on the memory store alone. The app's state files (the owner
//! marker, the skip list) live in the store, so a folder on disk only holds the app's settings.
//! The old-journal tests at the end are about folders on disk.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::*;
use crate::paths::SafeRelPath;
use crate::registry::DeletedFiles;
use crate::state::{InProgress, Interrupted, OldCopyGoes, ReplaceStep};
use crate::store::mem::MemStore;
use crate::store::s3::fake::FakeS3;
use crate::store::s3::{Limits, S3Config, S3Store};
use crate::store::{BackupStore, Destination, LocalStore, Upload};
use crate::sync::{archive_removed_in, run_sync_in};

const MIB: usize = 1024 * 1024;

/// One place to test against.
struct Backend {
    name: &'static str,
    mem: Option<MemStore>,
    s3: Option<S3Store>,
    /// Set when `s3` is on a pretend server, whose request log a test can read.
    fake: Option<FakeS3>,
}

impl Drop for Backend {
    fn drop(&mut self) {
        if let Some(s) = &self.s3 {
            s.purge_test_prefix();
        }
    }
}

impl Backend {
    /// A memory store that, like an S3 listing, gives no modified times.
    fn mem() -> Self {
        Backend {
            name: "memory",
            mem: Some(MemStore::new().without_list_mtimes()),
            s3: None,
            fake: None,
        }
    }

    /// An S3 store on a pretend server: no real bucket is needed and the requests can be read.
    fn fake_s3(limits: Limits) -> Self {
        let (s3, fake) = S3Store::on_fake(limits);
        Backend {
            name: "fake s3",
            mem: None,
            s3: Some(s3),
            fake: Some(fake),
        }
    }

    /// A bucket under a fresh prefix, if `PNK_S3_TEST=1`.
    fn s3_with(limits: Limits) -> Option<Self> {
        if std::env::var("PNK_S3_TEST").ok().as_deref() != Some("1") {
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
            "tests only touch the bucket pnk-test"
        );
        Some(Backend {
            name: "s3",
            mem: None,
            s3: Some(S3Store::with_limits(cfg, limits).unwrap().fast_retries()),
            fake: None,
        })
    }

    /// The memory store, and the bucket when it is switched on.
    fn all() -> Vec<Backend> {
        Self::all_with(Limits::default())
    }

    /// As [`all`](Self::all), with the bucket's part sizes lowered so a few MiB go multipart.
    fn all_small_parts() -> Vec<Backend> {
        Self::all_with(Limits {
            part_size: 5 * MIB,
            copy_limit: 5 * MIB as u64,
            copy_part: 5 * MIB as u64,
            ..Limits::default()
        })
    }

    fn all_with(limits: Limits) -> Vec<Backend> {
        let mut v = vec![Self::mem()];
        v.extend(Self::s3_with(limits));
        v
    }

    fn store(&self) -> &dyn BackupStore {
        match (&self.mem, &self.s3) {
            (Some(m), _) => m,
            (_, Some(s)) => s,
            _ => unreachable!("a backend has a store"),
        }
    }

    /// A second handle on the same store, as a crash-recovery start would open.
    fn handle(&self) -> Box<dyn BackupStore> {
        match (&self.mem, &self.s3) {
            (Some(m), _) => Box::new(m.clone()),
            (_, Some(s)) => Box::new(s.clone()),
            _ => unreachable!("a backend has a store"),
        }
    }

    /// The memory store, for the tests that inject failures.
    fn mem_store(&self) -> &MemStore {
        self.mem
            .as_ref()
            .expect("a test that needs the memory store")
    }

    /// The backup's name as a plan and a crash record carry it.
    fn root(&self, e: &Env) -> PathBuf {
        match self.store().destination() {
            Some(d) => d.label(),
            None => {
                fs::create_dir_all(&e.root).unwrap();
                e.root.clone()
            }
        }
    }

    /// The backup as recovery is told the app holds it.
    fn place(&self, e: &Env) -> Destination {
        self.store()
            .destination()
            .unwrap_or_else(|| Destination::Local(self.root(e)))
    }

    /// An object the way a sync leaves it: with a modified time (`pnk-mtime`) when given.
    fn put(&self, key: &str, data: &[u8], mtime: Option<SystemTime>) {
        let rel = SafeRelPath::parse(key).unwrap();
        let mut up = self
            .store()
            .begin_upload(&rel, data.len() as u64, mtime)
            .unwrap();
        up.write(data).unwrap();
        up.finish_no_replace().unwrap();
    }

    /// An object as another tool would put it: no modified time at all.
    fn put_plain(&self, key: &str, data: &[u8]) {
        let rel = SafeRelPath::parse(key).unwrap();
        self.store().write_new(&rel, data).unwrap();
    }

    fn get(&self, key: &str) -> Option<Vec<u8>> {
        let rel = SafeRelPath::parse(key).unwrap();
        self.store().read(&rel).unwrap().map(|(b, _)| b)
    }

    /// Every key, sorted, the app's own files (marker, skip list) left out.
    fn keys(&self) -> Vec<String> {
        self.all_keys()
            .into_iter()
            .filter(|k| k.contains('/') || !owner::is_marker_name(k))
            .collect()
    }

    fn all_keys(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .store()
            .list(&|_| false)
            .unwrap()
            .files
            .into_iter()
            .map(|f| f.rel)
            .collect();
        v.sort();
        v
    }

    fn open_uploads(&self) -> Vec<String> {
        match (&self.mem, &self.s3) {
            (Some(m), _) => m.open_uploads(),
            (_, Some(s)) => s.open_upload_refs().unwrap(),
            _ => unreachable!("a backend has a store"),
        }
    }

    /// Makes the backup the device's own, as a first sync does.
    fn claim(&self, key: &str) {
        let me = owner::FolderOwner::new(
            key,
            "phone",
            Signals::default(),
            View::WindowsWpd,
            "2026-01-01T00:00:00Z",
        );
        owner::write_in(self.store(), &me).unwrap();
    }
}

fn rel(s: &str) -> SafeRelPath {
    SafeRelPath::parse(s).unwrap()
}

fn scan_b(src: &mut dyn DeviceSource, e: &Env, b: &Backend, opts: &ScanOptions) -> ScanPlan {
    scan_b_result(src, e, b, opts).unwrap().0
}

fn scan_b_result(
    src: &mut dyn DeviceSource,
    e: &Env,
    b: &Backend,
    opts: &ScanOptions,
) -> Result<(ScanPlan, Summary), crate::device::DeviceError> {
    let store = b.store();
    let root = b.root(e);
    let scope = Scope::in_store(&src.identity().key, store, &root);
    let skip = e.state.skip_list_in(&scope, store).unwrap();
    crate::plan::scan_in(
        src,
        store,
        &root,
        &skip,
        opts,
        &AtomicBool::new(false),
        &mut |_| {},
    )
}

fn sync_b(
    src: &mut dyn DeviceSource,
    plan: &ScanPlan,
    e: &Env,
    b: &Backend,
    ctl: &SyncControl,
) -> (Result<SyncReport, SyncError>, Vec<SyncEvent>) {
    sync_in(src, plan, e, b.store(), &pending(plan), ctl)
}

fn sync_in(
    src: &mut dyn DeviceSource,
    plan: &ScanPlan,
    e: &Env,
    store: &dyn BackupStore,
    items: &[SyncItem],
    ctl: &SyncControl,
) -> (Result<SyncReport, SyncError>, Vec<SyncEvent>) {
    let mut events = Vec::new();
    let r = run_sync_in(
        store,
        src,
        plan,
        items,
        &e.state,
        ctl,
        STAMP,
        false,
        &mut |ev| events.push(ev),
    );
    (r, events)
}

fn archive_b(
    plan: &ScanPlan,
    b: &Backend,
) -> Result<(ArchiveReport, Vec<ArchiveEvent>), SyncError> {
    let mut events = Vec::new();
    archive_removed_in(b.store(), plan, STAMP, false, None, &mut |ev| {
        events.push(ev)
    })
    .map(|r| (r, events))
}

fn no_crash_record(e: &Env) -> bool {
    !e.state.path().join("in_progress.json").exists()
}

fn storage_opts() -> ScanOptions {
    opts_with(DeviceSettings {
        compare_by_date: true,
        ..open_settings()
    })
}

/// A crash record for `temp`, as a running copy writes it into the store at `b`.
fn write_record(e: &Env, b: &Backend, temp: &str, replace: Option<ReplaceStep>) {
    e.state
        .begin_copy(&InProgress {
            device: "serial:PHONE-A".into(),
            root: b.root(e),
            source: "DCIM/IMG.HEIC".into(),
            temp: temp.into(),
            replace,
            destination: b.store().destination(),
        })
        .unwrap();
}

/// The start-up recovery, with the backup at `b` the only one the app holds.
fn recover_b(e: &Env, b: &Backend) -> Interrupted {
    let place = b.place(e);
    e.state
        .recover_interrupted_for(&|_| vec![place.clone()], &|_| Ok(b.handle()))
        .unwrap()
        .unwrap()
}

/// A store that passes everything on, with two hooks: something that happens right after an
/// upload starts, and a note of the ages stale uploads were asked to be cleaned at.
struct Wrap {
    inner: Box<dyn BackupStore>,
    #[allow(clippy::type_complexity)]
    after_begin: Option<Box<dyn Fn(&dyn BackupStore, &SafeRelPath) + Send + Sync>>,
    /// Called with the two names just before a move.
    #[allow(clippy::type_complexity)]
    before_move: Option<Box<dyn Fn(&SafeRelPath, &SafeRelPath) + Send + Sync>>,
    stale: Arc<Mutex<Vec<Duration>>>,
}

impl Wrap {
    fn new(inner: Box<dyn BackupStore>) -> Self {
        Wrap {
            inner,
            after_begin: None,
            before_move: None,
            stale: Arc::default(),
        }
    }
}

impl BackupStore for Wrap {
    fn list(&self, p: &dyn Fn(&str) -> bool) -> io::Result<crate::store::Listing> {
        self.inner.list(p)
    }
    fn stat(&self, r: &SafeRelPath) -> io::Result<Option<crate::store::Stat>> {
        self.inner.stat(r)
    }
    fn read(&self, r: &SafeRelPath) -> io::Result<Option<(Vec<u8>, crate::store::Tag)>> {
        self.inner.read(r)
    }
    fn write_new(&self, r: &SafeRelPath, b: &[u8]) -> io::Result<crate::store::Tag> {
        self.inner.write_new(r, b)
    }
    fn write_replace(
        &self,
        r: &SafeRelPath,
        b: &[u8],
        x: Option<&crate::store::Tag>,
    ) -> io::Result<crate::store::Tag> {
        self.inner.write_replace(r, b, x)
    }
    fn begin_upload(
        &self,
        r: &SafeRelPath,
        s: u64,
        m: Option<SystemTime>,
    ) -> io::Result<Box<dyn Upload>> {
        let up = self.inner.begin_upload(r, s, m)?;
        if let Some(f) = &self.after_begin {
            f(self.inner.as_ref(), r);
        }
        Ok(up)
    }
    fn abort_by_ref(&self, j: &str) -> io::Result<()> {
        self.inner.abort_by_ref(j)
    }
    fn finish_by_ref(&self, j: &str, n: u64) -> io::Result<crate::store::Tag> {
        self.inner.finish_by_ref(j, n)
    }
    fn move_no_replace(&self, a: &SafeRelPath, b: &SafeRelPath) -> io::Result<()> {
        if let Some(f) = &self.before_move {
            f(a, b);
        }
        self.inner.move_no_replace(a, b)
    }
    fn remove_verified_original(&self, a: &SafeRelPath, b: &SafeRelPath) -> io::Result<bool> {
        self.inner.remove_verified_original(a, b)
    }
    fn send_to_bin(&self, r: &SafeRelPath) -> io::Result<()> {
        self.inner.send_to_bin(r)
    }
    fn supports_bin(&self) -> bool {
        false
    }
    fn space(&self) -> Option<crate::platform::DiskSpace> {
        None
    }
    fn root_exists(&self) -> io::Result<bool> {
        self.inner.root_exists()
    }
    fn ensure_root(&self) -> io::Result<()> {
        self.inner.ensure_root()
    }
    fn clean_stale_uploads(&self, older_than: Duration) -> io::Result<usize> {
        self.stale.lock().unwrap().push(older_than);
        Ok(0)
    }
    fn destination(&self) -> Option<Destination> {
        self.inner.destination()
    }
    fn describe(&self) -> String {
        "wrapped store".into()
    }
}

// ---- sync on every backend ----------------------------------------------------------------

/// A new file is written into the store, whole, and nothing is left open. The owner marker is
/// written into the store too.
#[test]
fn a_fresh_copy_lands_in_the_store() {
    for b in Backend::all() {
        eprintln!("backend: {}", b.name);
        let e = env();
        let mut src = phone(vec![
            MemObject::new("1", "DCIM/A.HEIC", b"aaaa"),
            MemObject::new("2", "DCIM/B.MOV", b"bbbbbb"),
        ]);
        let plan = scan_b(&mut src, &e, &b, &open_opts());
        let (r, ev) = sync_b(&mut src, &plan, &e, &b, &SyncControl::default());
        let r = r.unwrap();
        assert_eq!((r.copied, r.failed), (2, 0), "{ev:?}");
        assert_eq!(b.keys(), vec!["DCIM/A.HEIC", "DCIM/B.MOV"]);
        assert_eq!(b.get("DCIM/B.MOV").unwrap(), b"bbbbbb");
        assert!(b.open_uploads().is_empty());
        assert!(no_crash_record(&e));
        assert!(matches!(owner::read_in(b.store()), OwnerState::Owned(_)));
        assert!(!e.root.join(owner::MARKER).exists(), "no marker on disk");
        // A second scan sees both as backed up.
        let again = scan_b(&mut src, &e, &b, &open_opts());
        assert!(again.entries.iter().all(|x| x.status == Status::Verified));
    }
}

/// The whole life of a backup on an object store: new files, a file deleted from the device
/// (archived), a changed file (old copy archived, new one in place), and a quiet rescan.
#[test]
fn scan_sync_delete_archive_and_change_cycle() {
    for b in Backend::all() {
        eprintln!("backend: {}", b.name);
        let e = env();
        let mut src = phone(vec![
            MemObject::new("1", "DCIM/A.HEIC", b"aaaa"),
            MemObject::new("2", "DCIM/B.MOV", b"bbbbbb"),
        ]);
        let key = key_of(&src);
        let plan = scan_b(&mut src, &e, &b, &open_opts());
        let (r, ev) = sync_b(&mut src, &plan, &e, &b, &SyncControl::default());
        assert_eq!(r.unwrap().copied, 2, "{ev:?}");

        // B is deleted from the device: it moves into the archive.
        src.objects.retain(|o| o.id != "2");
        let plan = scan_b(&mut src, &e, &b, &open_opts());
        assert_eq!(plan.removed.len(), 1);
        let (report, events) = archive_b(&plan, &b).unwrap();
        assert_eq!((report.moved, report.failed), (1, 0), "{events:?}");
        assert_eq!(b.get("DCIM/B.MOV"), None);
        assert_eq!(
            b.get(&format!("{ARCHIVE_DIR}/{STAMP}/B.MOV")).unwrap(),
            b"bbbbbb"
        );

        // A changes: its old copy goes into the archive, the new one takes its place.
        src.object_mut("1").data = b"the new A".to_vec();
        src.object_mut("1").reported_size = Some(9);
        let plan = scan_b(&mut src, &e, &b, &open_opts());
        assert_eq!(plan.entries[0].status, Status::Mismatch);
        let (r, ev) = sync_b(&mut src, &plan, &e, &b, &SyncControl::default());
        let r = r.unwrap();
        assert_eq!((r.copied, r.replaced), (1, 1), "{ev:?}");
        assert_eq!(b.get("DCIM/A.HEIC").unwrap(), b"the new A");
        assert_eq!(
            b.get(&format!("{ARCHIVE_DIR}/{STAMP}/A.HEIC")).unwrap(),
            b"aaaa"
        );
        assert!(b.open_uploads().is_empty());
        assert!(no_crash_record(&e));

        // Quiet now: everything backed up, nothing to archive, the archive not taken for files.
        let plan = scan_b(&mut src, &e, &b, &open_opts());
        assert!(plan.entries.iter().all(|x| x.status == Status::Verified));
        assert!(plan.removed.is_empty());
        assert_eq!(plan.backup_files, 1);
        let OwnerState::Owned(o) = owner::read_in(b.store()) else {
            panic!("the marker is in the store");
        };
        assert_eq!(o.device_id, key);
    }
}

/// DESIGN.md 2: Stop leaves no half file. An upload that is stopped is thrown away, so nothing
/// shows in the store and nothing is left open.
#[test]
fn stop_part_way_leaves_nothing_in_the_store() {
    for b in Backend::all() {
        eprintln!("backend: {}", b.name);
        let e = env();
        let ctl = SyncControl::default();
        let mut obj = MemObject::new("1", "DCIM/BIG.MOV", &[3u8; 400_000]);
        obj.trip = Some((10_000, ctl.cancel.clone()));
        let mut src = phone(vec![obj]);
        b.claim(&key_of(&src));
        let plan = scan_b(&mut src, &e, &b, &open_opts());
        let (r, _) = sync_b(&mut src, &plan, &e, &b, &ctl);
        assert!(r.unwrap().cancelled);
        assert!(b.keys().is_empty(), "{:?}", b.keys());
        assert!(b.open_uploads().is_empty());
        assert!(no_crash_record(&e));
    }
}

/// Skip current is the same: nothing in the store, and the file goes on the skip list that is
/// kept in the store.
#[test]
fn skip_part_way_leaves_nothing_in_the_store() {
    for b in Backend::all() {
        eprintln!("backend: {}", b.name);
        let e = env();
        let ctl = SyncControl::default();
        let mut obj = MemObject::new("1", "DCIM/BIG.MOV", &[3u8; 400_000]);
        obj.trip = Some((10_000, ctl.skip_current.clone()));
        let mut src = phone(vec![obj]);
        b.claim(&key_of(&src));
        let plan = scan_b(&mut src, &e, &b, &open_opts());
        ctl.skip_entry
            .store(entry_of(&plan, "DCIM/BIG.MOV"), Ordering::SeqCst);
        let (r, _) = sync_b(&mut src, &plan, &e, &b, &ctl);
        assert_eq!(r.unwrap().skipped, 1);
        assert!(b.keys().is_empty());
        assert!(b.open_uploads().is_empty());
        let list = b
            .get(crate::state::SKIP_NAME)
            .expect("the list is in the store");
        assert!(String::from_utf8_lossy(&list).contains("DCIM/BIG.MOV"));
        // The next scan leaves it be.
        let plan = scan_b(&mut src, &e, &b, &open_opts());
        assert_eq!(plan.entries[0].status, Status::Skipped);
    }
}

/// DESIGN.md 7.3: a changed file's old copy moves into the archive and the new one takes its
/// place; the old copy is moved only after the whole new copy was uploaded.
#[test]
fn a_changed_file_is_archived_then_replaced() {
    for b in Backend::all() {
        eprintln!("backend: {}", b.name);
        let e = env();
        b.put("DCIM/IMG.HEIC", b"old", None);
        let mut src = phone(vec![MemObject::new("1", "DCIM/IMG.HEIC", b"the new one")]);
        b.claim(&key_of(&src));
        let plan = scan_b(&mut src, &e, &b, &open_opts());
        assert_eq!(plan.entries[0].status, Status::Mismatch);
        let (r, ev) = sync_b(&mut src, &plan, &e, &b, &SyncControl::default());
        let r = r.unwrap();
        assert_eq!((r.copied, r.replaced), (1, 1), "{ev:?}");
        assert_eq!(b.get("DCIM/IMG.HEIC").unwrap(), b"the new one");
        assert_eq!(
            b.get(&format!("{ARCHIVE_DIR}/{STAMP}/IMG.HEIC")).unwrap(),
            b"old"
        );
        assert!(b.open_uploads().is_empty());
        assert!(no_crash_record(&e));
    }
}

/// DESIGN.md 6.3: a file deleted from the device is archived; a name already taken in the
/// archive is never overwritten, the next number is used.
#[test]
fn a_deleted_file_is_archived_under_a_free_name() {
    for b in Backend::all() {
        eprintln!("backend: {}", b.name);
        let e = env();
        b.put("DCIM/KEEP.HEIC", b"keep", None);
        b.put("DCIM/GONE.HEIC", b"gone", None);
        b.put(
            &format!("{ARCHIVE_DIR}/{STAMP}/GONE.HEIC"),
            b"earlier",
            None,
        );
        let mut src = phone(vec![MemObject::new("1", "DCIM/KEEP.HEIC", b"keep")]);
        b.claim(&key_of(&src));
        let plan = scan_b(&mut src, &e, &b, &open_opts());
        assert_eq!(plan.removed.len(), 1, "the archive is not backup content");
        let (report, events) = archive_b(&plan, &b).unwrap();
        assert_eq!(report.moved, 1, "{events:?}");
        assert_eq!(
            b.get(&format!("{ARCHIVE_DIR}/{STAMP}/GONE.HEIC")).unwrap(),
            b"earlier"
        );
        assert_eq!(
            b.get(&format!("{ARCHIVE_DIR}/{STAMP}/GONE (2).HEIC"))
                .unwrap(),
            b"gone"
        );
        assert!(b.get("DCIM/GONE.HEIC").is_none());
        assert_eq!(b.get("DCIM/KEEP.HEIC").unwrap(), b"keep");
    }
}

/// A name that turns up while the upload runs is never overwritten: the finish is refused and
/// nothing is left open.
#[test]
fn a_file_that_turns_up_during_the_upload_is_not_overwritten() {
    for b in Backend::all() {
        eprintln!("backend: {}", b.name);
        let e = env();
        let mut src = phone(vec![MemObject::new("1", "DCIM/A.HEIC", b"aaaa")]);
        b.claim(&key_of(&src));
        let plan = scan_b(&mut src, &e, &b, &open_opts());
        // Turns up after the check, before the finish.
        let mut store = Wrap::new(b.handle());
        store.after_begin = Some(Box::new(|s, r| {
            s.write_new(r, b"someone else's").unwrap();
        }));
        let (r, events) = sync_in(
            &mut src,
            &plan,
            &e,
            &store,
            &pending(&plan),
            &SyncControl::default(),
        );
        let r = r.unwrap();
        assert_eq!((r.copied, r.failed), (0, 1), "{events:?}");
        assert_eq!(b.get("DCIM/A.HEIC").unwrap(), b"someone else's");
        assert!(b.open_uploads().is_empty());
    }
}

/// DESIGN.md 6.3: the bin is never faked. A store without one refuses to be asked: nothing is
/// moved, and a changed file's old copy is not replaced.
#[test]
fn the_bin_is_refused_by_a_store_without_one() {
    for b in Backend::all() {
        eprintln!("backend: {}", b.name);
        let e = env();
        b.put("DCIM/KEEP.HEIC", b"keep", None);
        b.put("DCIM/GONE.HEIC", b"gone", None);
        b.put("DCIM/CHG.HEIC", b"old", None);
        let mut src = phone(vec![
            MemObject::new("1", "DCIM/KEEP.HEIC", b"keep"),
            MemObject::new("2", "DCIM/CHG.HEIC", b"changed!"),
        ]);
        b.claim(&key_of(&src));
        let opts = opts_with(DeviceSettings {
            deleted_files: DeletedFiles::RecycleBin,
            ..open_settings()
        });
        let plan = scan_b(&mut src, &e, &b, &opts);
        let err = archive_b(&plan, &b).unwrap_err();
        assert!(err.to_string().contains("not available"), "{err}");
        let (r, ev) = sync_b(&mut src, &plan, &e, &b, &SyncControl::default());
        let r = r.unwrap();
        assert_eq!((r.copied, r.failed), (0, 1), "{ev:?}");
        assert!(failures(&ev)[0].contains("not available"), "{ev:?}");
        assert_eq!(b.get("DCIM/GONE.HEIC").unwrap(), b"gone");
        assert_eq!(b.get("DCIM/CHG.HEIC").unwrap(), b"old");
        assert!(b.open_uploads().is_empty());
        assert_eq!(b.keys().len(), 3);
    }
}

/// A store that is not a folder on this PC can't rename the archive folder.
#[test]
fn renaming_the_archive_is_not_offered_for_a_store_without_folders() {
    for b in Backend::all() {
        eprintln!("backend: {}", b.name);
        let e = env();
        let mut src = phone(vec![MemObject::new("1", "DCIM/A.HEIC", b"aaaa")]);
        b.claim(&key_of(&src));
        let plan = scan_b(&mut src, &e, &b, &open_opts());
        let err = crate::sync::rename_archive_in(b.store(), &plan, "_Old").unwrap_err();
        assert!(err.to_string().contains("not available"), "{err}");
    }
    // A folder store does it.
    let e = env();
    let mem = MemStore::new();
    let mut src = phone(vec![MemObject::new("1", "DCIM/A.HEIC", b"aaaa")]);
    let plan = crate::plan::scan_in(
        &mut src,
        &mem,
        &e.root,
        &Default::default(),
        &open_opts(),
        &AtomicBool::new(false),
        &mut |_| {},
    )
    .unwrap()
    .0;
    fs::create_dir_all(&e.root).unwrap();
    let local = LocalStore::new(&e.root);
    assert!(crate::sync::rename_archive_in(&local, &plan, "_Old").is_ok());
}

// ---- modified times on an object store ------------------------------------------------------

/// A storage device compares dates. The listing has none, so a file that matches in name and
/// size is looked at once; the date the sync gave it (`pnk-mtime`) comes back, nothing is sent
/// again, and a changed date is still noticed. A phone compares no dates and looks at nothing.
#[test]
fn a_storage_device_with_dates_round_trips_its_dates_and_is_not_sent_twice() {
    for b in Backend::all() {
        eprintln!("backend: {}", b.name);
        let e = env();
        let mut one = MemObject::new("1", "a.txt", b"abc");
        one.modified = Some(at(100));
        let mut two = MemObject::new("2", "dir/b.bin", b"bbbbb");
        two.modified = Some(at(200));
        let mut src = phone(vec![one, two]);
        let plan = scan_b(&mut src, &e, &b, &storage_opts());
        assert_eq!(plan.mtime_lookups, 0, "nothing in the backup to look at");
        let (r, ev) = sync_b(&mut src, &plan, &e, &b, &SyncControl::default());
        assert_eq!(r.unwrap().copied, 2, "{ev:?}");
        // The date went in with the file and comes back out.
        let st = b.store().stat(&rel("a.txt")).unwrap().unwrap();
        assert!(
            crate::plan::same_modified(st.mtime.unwrap(), at(100)),
            "{st:?}"
        );

        // Scanned again: both are the same files, their dates come from the cache the sync
        // saved, so there are no lookups, and none is re-sent.
        let plan = scan_b(&mut src, &e, &b, &storage_opts());
        assert!(
            plan.entries.iter().all(|x| x.status == Status::Verified),
            "{plan:?}"
        );
        assert_eq!(plan.mtime_lookups, 0);
        assert!(pending(&plan).is_empty());
        eprintln!(
            "{}: a rescan of 2 files did {} lookups",
            b.name, plan.mtime_lookups
        );

        // The same size with another date is a changed file.
        src.object_mut("1").modified = Some(at(9999));
        let plan = scan_b(&mut src, &e, &b, &storage_opts());
        assert_eq!(
            plan.entries[entry_of(&plan, "a.txt")].status,
            Status::Mismatch
        );
        assert_eq!(
            plan.entries[entry_of(&plan, "dir/b.bin")].status,
            Status::Verified
        );
        assert_eq!(plan.mtime_lookups, 0, "the dates are in the cache");

        // A phone compares by name and size only: no lookups at all.
        let plan = scan_b(&mut src, &e, &b, &open_opts());
        assert!(plan.entries.iter().all(|x| x.status == Status::Verified));
        assert_eq!(plan.mtime_lookups, 0);
    }
}

/// A bucket someone filled with another tool has objects with no `pnk-mtime`. They are matched by
/// name and size, nothing is sent again, and nothing is changed in them.
#[test]
fn objects_without_a_modified_time_are_matched_by_name_and_size() {
    for b in Backend::all() {
        eprintln!("backend: {}", b.name);
        let e = env();
        b.put_plain("a.txt", b"abc");
        b.put_plain("dir/b.bin", b"bbbbb");
        b.put_plain("c.txt", b"short");
        let mut objs = vec![
            MemObject::new("1", "a.txt", b"abc"),
            MemObject::new("2", "dir/b.bin", b"bbbbb"),
            // Not the same size as the object of that name.
            MemObject::new("3", "c.txt", b"much longer"),
        ];
        for (i, o) in objs.iter_mut().enumerate() {
            o.modified = Some(at(500 + i as u64));
        }
        let mut src = phone(objs);
        b.claim(&key_of(&src));
        let plan = scan_b(&mut src, &e, &b, &storage_opts());
        assert_eq!(
            plan.entries[entry_of(&plan, "a.txt")].status,
            Status::Verified
        );
        assert_eq!(
            plan.entries[entry_of(&plan, "dir/b.bin")].status,
            Status::Verified
        );
        assert_eq!(
            plan.entries[entry_of(&plan, "c.txt")].status,
            Status::Mismatch
        );
        assert_eq!(
            plan.mtime_lookups, 2,
            "only the two that match in size are looked at"
        );
        // Syncing only the one that differs leaves the others as they were.
        let items: Vec<SyncItem> = pending(&plan);
        assert_eq!(items.len(), 1);
        let (r, ev) = sync_in(
            &mut src,
            &plan,
            &e,
            b.store(),
            &items,
            &SyncControl::default(),
        );
        assert_eq!(r.unwrap().copied, 1, "{ev:?}");
        assert!(
            b.store()
                .stat(&rel("a.txt"))
                .unwrap()
                .unwrap()
                .mtime
                .is_none()
        );
        assert_eq!(b.get("a.txt").unwrap(), b"abc");
        assert_eq!(b.get("c.txt").unwrap(), b"much longer");
        // Rescanned: all three are the device's files.
        let plan = scan_b(&mut src, &e, &b, &storage_opts());
        assert!(
            plan.entries.iter().all(|x| x.status == Status::Verified),
            "{plan:?}"
        );
    }
}

// ---- no network, a full server ---------------------------------------------------------------

/// A store on a port nobody listens on, with throw-away keys.
fn unreachable_store(dir: &Path) -> S3Store {
    use std::io::Write;
    let creds = dir.join("credentials");
    fs::File::create_new(&creds)
        .unwrap()
        .write_all(b"[t]\naws_access_key_id = TESTKEYID\naws_secret_access_key = TESTSECRETVALUE\n")
        .unwrap();
    S3Store::new(S3Config {
        provider: Default::default(),
        account_id: None,
        endpoint: "http://127.0.0.1:1".into(),
        region: "us-east-1".into(),
        bucket: "b".into(),
        prefix: "p/".into(),
        profile: Some("t".into()),
        credentials_file: Some(creds),
        path_style: true,
        ca_file: None,
    })
    .unwrap()
    .fast_retries()
}

/// With no network a scan stops with a plain message; it does not show every file as new.
/// Syncing and archiving into an unreachable bucket do nothing and say the same.
#[test]
fn no_network_stops_the_scan_the_sync_and_the_archive_with_one_message() {
    let e = env();
    let store = unreachable_store(&e.base);
    let root = Path::new("s3://b/p/");
    let mut src = phone(vec![MemObject::new("1", "DCIM/A.HEIC", b"aaaa")]);
    let scanned = crate::plan::scan_in(
        &mut src,
        &store,
        root,
        &Default::default(),
        &open_opts(),
        &AtomicBool::new(false),
        &mut |_| {},
    );
    match scanned {
        Err(crate::device::DeviceError::Failed(m)) => {
            assert!(
                m.starts_with("No network: can't reach http://127.0.0.1:1"),
                "{m}"
            );
            assert!(!m.contains("TESTKEY") && !m.contains("TESTSECRET"), "{m}");
        }
        other => panic!("{other:?}"),
    }
    // A plan made against some other store, run into the unreachable one.
    let mem = Backend::mem();
    let plan = scan_b(&mut src, &e, &mem, &open_opts());
    let (r, _) = sync_in(
        &mut src,
        &plan,
        &e,
        &store,
        &pending(&plan),
        &SyncControl::default(),
    );
    match r {
        Err(SyncError::Offline(m)) => assert!(m.starts_with("No network:"), "{m}"),
        other => panic!("{other:?}"),
    }
    assert!(no_crash_record(&e));
    let mut gone = phone(vec![MemObject::new("2", "DCIM/B.HEIC", b"b")]);
    mem.put("DCIM/OLD.HEIC", b"old", None);
    mem.claim(&key_of(&gone));
    let plan = scan_b(&mut gone, &e, &mem, &open_opts());
    assert_eq!(plan.removed.len(), 1);
    let r = archive_removed_in(&store, &plan, STAMP, false, None, &mut |_| {});
    assert!(matches!(r, Err(SyncError::Offline(_))), "{r:?}");
}

/// The network goes away part-way through: the sync stops at that file instead of failing every
/// file after it, and the files already copied stay.
#[test]
fn losing_the_network_mid_sync_stops_there() {
    let b = Backend::mem();
    let e = env();
    let mut src = phone(vec![
        MemObject::new("1", "DCIM/A.HEIC", b"aaaa"),
        MemObject::new("2", "DCIM/B.HEIC", b"bbbb"),
        MemObject::new("3", "DCIM/C.HEIC", b"cccc"),
    ]);
    b.claim(&key_of(&src));
    let plan = scan_b(&mut src, &e, &b, &open_opts());
    b.mem_store().fail_nth_offline("upload_write", 2);
    let (r, ev) = sync_b(&mut src, &plan, &e, &b, &SyncControl::default());
    let r = r.unwrap();
    assert_eq!((r.copied, r.failed), (1, 1), "{ev:?}");
    let why = r.offline.expect("reported as offline");
    assert!(why.starts_with("No network:"), "{why}");
    let started = ev
        .iter()
        .filter(|x| matches!(x, SyncEvent::FileStarted { .. }))
        .count();
    assert_eq!(started, 2, "the third file was not tried");
    assert_eq!(b.keys(), vec!["DCIM/A.HEIC"]);
    assert!(b.open_uploads().is_empty());
    assert!(no_crash_record(&e));
}

/// Archiving stops at the first file the network failed on too.
#[test]
fn losing_the_network_mid_archive_stops_there() {
    let b = Backend::mem();
    let e = env();
    let mut on_device = Vec::new();
    for n in ["K1", "K2", "K3"] {
        let name = format!("DCIM/{n}.HEIC");
        b.put(&name, n.as_bytes(), None);
        on_device.push(MemObject::new(n, &name, n.as_bytes()));
    }
    for n in ["X", "Y", "Z"] {
        b.put(&format!("DCIM/{n}.HEIC"), n.as_bytes(), None);
    }
    let mut src = phone(on_device);
    b.claim(&key_of(&src));
    let plan = scan_b(&mut src, &e, &b, &open_opts());
    assert_eq!(plan.removed.len(), 3);
    b.mem_store().fail_nth_offline("move", 2);
    let (report, events) = archive_b(&plan, &b).unwrap();
    assert_eq!((report.moved, report.failed), (1, 1), "{events:?}");
    assert!(report.offline.unwrap().starts_with("No network:"));
    assert_eq!(b.mem_store().calls("move"), 2, "the third was not tried");
}

/// A full server (a quota, insufficient storage) is the full-disk stop: the file is thrown away
/// and the sync ends there.
#[test]
fn a_full_server_is_the_disk_full_stop() {
    let b = Backend::mem();
    let e = env();
    let mut src = phone(vec![
        MemObject::new("1", "DCIM/A.HEIC", b"aaaa"),
        MemObject::new("2", "DCIM/B.HEIC", b"bbbb"),
    ]);
    b.claim(&key_of(&src));
    let plan = scan_b(&mut src, &e, &b, &open_opts());
    b.mem_store()
        .fail_nth("upload_write", 1, io::ErrorKind::StorageFull);
    let (r, ev) = sync_b(&mut src, &plan, &e, &b, &SyncControl::default());
    let r = r.unwrap();
    assert!(r.disk_full, "{ev:?}");
    assert_eq!(r.failed, 1);
    assert!(b.keys().is_empty());
    assert!(b.open_uploads().is_empty());
}

// ---- failures that only the memory store can be made to have -------------------------------

/// The old copy moves out before the new one is finished, so a failed finish puts the old copy
/// back: both are never lost.
#[test]
fn a_changed_file_whose_upload_cannot_finish_gets_its_old_copy_back() {
    let b = Backend::mem();
    let (e, mem) = (env(), b.mem_store());
    b.put("DCIM/IMG.HEIC", b"old", None);
    let mut src = phone(vec![MemObject::new("1", "DCIM/IMG.HEIC", b"the new one")]);
    b.claim(&key_of(&src));
    let plan = scan_b(&mut src, &e, &b, &open_opts());
    mem.fail_nth("finish", 1, io::ErrorKind::TimedOut);
    let (r, ev) = sync_b(&mut src, &plan, &e, &b, &SyncControl::default());
    assert_eq!(r.unwrap().failed, 1, "{ev:?}");
    assert_eq!(mem.calls("move"), 2, "moved away, then moved back");
    assert_eq!(b.get("DCIM/IMG.HEIC").unwrap(), b"old");
    assert_eq!(b.keys(), vec!["DCIM/IMG.HEIC"]);
}

/// DESIGN.md 5/6: the new copy is whole and durable before the old copy moves.
#[test]
fn the_new_copy_is_sealed_before_the_old_copy_moves() {
    let b = Backend::mem();
    let (e, mem) = (env(), b.mem_store());
    b.put("DCIM/IMG.HEIC", b"old", None);
    let mut src = phone(vec![MemObject::new("1", "DCIM/IMG.HEIC", b"the new one")]);
    b.claim(&key_of(&src));
    let plan = scan_b(&mut src, &e, &b, &open_opts());
    let (r, ev) = sync_b(&mut src, &plan, &e, &b, &SyncControl::default());
    assert_eq!(r.unwrap().copied, 1, "{ev:?}");
    let log = mem.log();
    let at = |op: &str| log.iter().rposition(|o| *o == op).unwrap();
    assert!(at("upload_write") < at("seal"), "{log:?}");
    assert!(at("seal") < at("move"), "{log:?}");
    assert!(at("move") < at("finish"), "{log:?}");
    // One for the old file put in; for the new copy the seal when its stream ended and the
    // stage before the move.
    assert_eq!(mem.calls("seal"), 3, "{log:?}");
}

/// A new copy that can't be made durable counts as a failed write: the old copy is not touched.
#[test]
fn a_failed_seal_leaves_the_old_copy_in_place() {
    let b = Backend::mem();
    let (e, mem) = (env(), b.mem_store());
    b.put("DCIM/IMG.HEIC", b"old", None);
    let mut src = phone(vec![MemObject::new("1", "DCIM/IMG.HEIC", b"the new one")]);
    b.claim(&key_of(&src));
    let plan = scan_b(&mut src, &e, &b, &open_opts());
    mem.fail_nth("seal", 1, io::ErrorKind::TimedOut);
    let (r, ev) = sync_b(&mut src, &plan, &e, &b, &SyncControl::default());
    assert_eq!((r.unwrap().copied, 1), (0, 1), "{ev:?}");
    assert!(
        failures(&ev)[0].contains("writing to the backup failed"),
        "{ev:?}"
    );
    assert_eq!(mem.calls("move"), 0);
    assert_eq!(b.get("DCIM/IMG.HEIC").unwrap(), b"old");
    assert_eq!(b.keys(), vec!["DCIM/IMG.HEIC"]);
    assert!(b.open_uploads().is_empty());
    assert!(no_crash_record(&e));
}

/// A move on an object store is a copy and a delete. Cut between the two, both copies exist and
/// the archive step reports the failure; nothing is lost and nothing is overwritten.
#[test]
fn an_archive_move_cut_after_its_copy_keeps_both_copies() {
    let b = Backend::mem();
    let (e, mem) = (env(), b.mem_store());
    b.put("DCIM/KEEP.HEIC", b"keep", None);
    b.put("DCIM/GONE.HEIC", b"gone", None);
    let mut src = phone(vec![MemObject::new("1", "DCIM/KEEP.HEIC", b"keep")]);
    b.claim(&key_of(&src));
    let plan = scan_b(&mut src, &e, &b, &open_opts());
    mem.stop_next_move_after_copy();
    let (report, events) = archive_b(&plan, &b).unwrap();
    assert_eq!((report.moved, report.failed), (0, 1), "{events:?}");
    assert_eq!(b.get("DCIM/GONE.HEIC").unwrap(), b"gone");
    assert_eq!(
        b.get(&format!("{ARCHIVE_DIR}/{STAMP}/GONE.HEIC")).unwrap(),
        b"gone"
    );
}

/// A failing write aborts the upload: nothing is in the store, and nothing is left open.
#[test]
fn a_failed_upload_write_leaves_nothing() {
    let b = Backend::mem();
    let (e, mem) = (env(), b.mem_store());
    let mut src = phone(vec![MemObject::new("1", "DCIM/A.HEIC", b"aaaa")]);
    b.claim(&key_of(&src));
    let plan = scan_b(&mut src, &e, &b, &open_opts());
    mem.fail_nth("upload_write", 1, io::ErrorKind::ConnectionReset);
    let (r, ev) = sync_b(&mut src, &plan, &e, &b, &SyncControl::default());
    let r = r.unwrap();
    assert_eq!((r.copied, r.failed), (0, 1), "{ev:?}");
    assert!(
        failures(&ev)[0].contains("writing to the backup failed"),
        "{ev:?}"
    );
    assert!(b.keys().is_empty());
    assert!(b.open_uploads().is_empty());
    assert!(no_crash_record(&e));
}

/// An upload that can't even start changes nothing.
#[test]
fn a_failed_begin_changes_nothing() {
    let b = Backend::mem();
    let (e, mem) = (env(), b.mem_store());
    let mut src = phone(vec![MemObject::new("1", "DCIM/A.HEIC", b"aaaa")]);
    b.claim(&key_of(&src));
    let plan = scan_b(&mut src, &e, &b, &open_opts());
    mem.fail_nth("begin_upload", 1, io::ErrorKind::PermissionDenied);
    let (r, ev) = sync_b(&mut src, &plan, &e, &b, &SyncControl::default());
    assert_eq!(r.unwrap().failed, 1, "{ev:?}");
    assert!(b.keys().is_empty());
    assert!(b.open_uploads().is_empty());
    assert!(no_crash_record(&e));
}

/// An object store only learns an upload's name when it starts, so the crash record is written
/// after. If that record can't be written the upload is thrown away rather than left that
/// nobody can clean up.
#[test]
fn an_upload_is_aborted_when_its_crash_record_cannot_be_written() {
    let b = Backend::mem();
    let (e, mem) = (env(), b.mem_store());
    let mut src = phone(vec![MemObject::new("1", "DCIM/A.HEIC", b"aaaa")]);
    b.claim(&key_of(&src));
    let plan = scan_b(&mut src, &e, &b, &open_opts());
    // A folder where the record's file goes: writing it fails.
    let state_file = e.state.path().join("in_progress.json");
    fs::create_dir(&state_file).unwrap();
    let (r, ev) = sync_b(&mut src, &plan, &e, &b, &SyncControl::default());
    assert_eq!(r.unwrap().failed, 1, "{ev:?}");
    assert!(failures(&ev)[0].contains("could not record"), "{ev:?}");
    assert_eq!(mem.calls("abort"), 1);
    assert!(b.keys().is_empty());
    assert!(b.open_uploads().is_empty());
}

// ---- recovery ------------------------------------------------------------------------------

fn replace_step(archived: &str, goes: OldCopyGoes) -> ReplaceStep {
    ReplaceStep {
        original: "DCIM/IMG.HEIC".into(),
        archived: archived.into(),
        goes,
        staged_size: None,
    }
}

/// An upload that was open when the app died: started, parts sent, never finished.
fn open_upload(b: &Backend, key: &str, bytes: usize) -> String {
    let mut up = b
        .store()
        .begin_upload(&rel(key), bytes as u64, Some(at(1)))
        .unwrap();
    up.write(&vec![7u8; bytes]).unwrap();
    let r = up.journal_ref();
    // (The crash: the upload is never finished or aborted.)
    std::mem::forget(up);
    r
}

/// A crash part-way through an upload: the record names it, and recovery drops it. Nothing
/// appears under the file's name, and nothing is left open (for a bucket, no multipart upload).
#[test]
fn recovery_drops_an_interrupted_multipart_upload() {
    for b in Backend::all_small_parts() {
        eprintln!("backend: {}", b.name);
        let e = env();
        b.claim("serial:PHONE-A");
        let temp = open_upload(&b, "DCIM/BIG.MOV", 6 * MIB);
        assert_eq!(b.open_uploads().len(), 1);
        write_record(&e, &b, &temp, None);
        let got = recover_b(&e, &b);
        assert!(got.temp_removed && !got.retry_later, "{}", got.note);
        assert!(b.open_uploads().is_empty());
        assert!(b.keys().is_empty());
        assert!(no_crash_record(&e));
    }
}

/// A crash between the old copy's copy to the archive and its delete: both exist and an upload
/// is open. Recovery drops the upload and, because the archive copy has the same size and date,
/// removes the original. Nothing is lost: the file is in the archive, and the device still has
/// the new version for the next sync.
#[test]
fn recovery_removes_the_original_once_the_archive_copy_is_verified() {
    for (goes, moved) in [
        (
            OldCopyGoes::Archive,
            format!("{ARCHIVE_DIR}/{STAMP}/IMG.HEIC"),
        ),
        (
            OldCopyGoes::Aside,
            "DCIM/IMG (old 2026-01-02 03-04).HEIC".to_string(),
        ),
    ] {
        for b in Backend::all_small_parts() {
            eprintln!("backend: {} {goes:?}", b.name);
            let e = env();
            b.claim("serial:PHONE-A");
            b.put("DCIM/IMG.HEIC", b"old", Some(at(5)));
            b.put(&moved, b"old", Some(at(5)));
            let temp = open_upload(&b, "DCIM/IMG.HEIC", 6 * MIB);
            write_record(&e, &b, &temp, Some(replace_step(&moved, goes)));
            let got = recover_b(&e, &b);
            assert!(got.temp_removed && !got.completed, "{}", got.note);
            assert!(got.note.contains("original was removed"), "{}", got.note);
            assert!(b.open_uploads().is_empty());
            assert_eq!(b.get("DCIM/IMG.HEIC"), None);
            assert_eq!(b.get(&moved).unwrap(), b"old");
            assert!(no_crash_record(&e));
        }
    }
}

/// Never delete if unsure: a copy of another size, another date or no date at all is not shown
/// to be the original, so both stay and recovery says so.
#[test]
fn recovery_keeps_both_when_the_copy_cannot_be_shown_to_match() {
    let archived = format!("{ARCHIVE_DIR}/{STAMP}/IMG.HEIC");
    #[allow(clippy::type_complexity)]
    let cases: [(&[u8], Option<SystemTime>, &[u8], Option<SystemTime>); 4] = [
        (b"old", Some(at(5)), b"old!", Some(at(5))),
        (b"old", Some(at(5)), b"old", Some(at(6))),
        (b"old", Some(at(5)), b"old", None),
        // Neither has a date: nothing to tell it from another file of that size.
        (b"old", None, b"old", None),
    ];
    for (orig, om, copy, cm) in cases {
        for b in Backend::all() {
            eprintln!("backend: {}", b.name);
            let e = env();
            b.claim("serial:PHONE-A");
            b.put("DCIM/IMG.HEIC", orig, om);
            b.put(&archived, copy, cm);
            let temp = open_upload(&b, "DCIM/IMG.HEIC", 10);
            write_record(
                &e,
                &b,
                &temp,
                Some(replace_step(&archived, OldCopyGoes::Archive)),
            );
            let got = recover_b(&e, &b);
            assert!(got.note.contains("both"), "{}", got.note);
            assert!(!got.note.contains("original was removed"), "{}", got.note);
            assert_eq!(b.get("DCIM/IMG.HEIC").unwrap(), orig);
            assert_eq!(b.get(&archived).unwrap(), copy);
            assert!(b.open_uploads().is_empty());
        }
    }
}

/// A move cut before its copy leaves the original alone; one cut after the delete only needs the
/// upload dropped.
#[test]
fn recovery_of_the_other_cuts_of_a_move() {
    for b in Backend::all() {
        eprintln!("backend: {}", b.name);
        let archived = format!("{ARCHIVE_DIR}/{STAMP}/IMG.HEIC");
        // Before the copy.
        let e = env();
        b.claim("serial:PHONE-A");
        b.put("DCIM/IMG.HEIC", b"old", Some(at(5)));
        let temp = open_upload(&b, "DCIM/IMG.HEIC", 10);
        write_record(
            &e,
            &b,
            &temp,
            Some(replace_step(&archived, OldCopyGoes::Archive)),
        );
        let got = recover_b(&e, &b);
        assert!(got.note.contains("had not been moved"), "{}", got.note);
        assert_eq!(b.get("DCIM/IMG.HEIC").unwrap(), b"old");
        assert_eq!(b.get(&archived), None);
        // After the delete.
        let e = env();
        b.store()
            .move_no_replace(&rel("DCIM/IMG.HEIC"), &rel(&archived))
            .unwrap();
        let temp = open_upload(&b, "DCIM/IMG.HEIC", 10);
        write_record(
            &e,
            &b,
            &temp,
            Some(replace_step(&archived, OldCopyGoes::Archive)),
        );
        let got = recover_b(&e, &b);
        assert!(
            got.note.contains("next sync copies the file again"),
            "{}",
            got.note
        );
        assert_eq!(b.get("DCIM/IMG.HEIC"), None);
        assert_eq!(b.get(&archived).unwrap(), b"old");
        assert!(b.open_uploads().is_empty());
    }
}

/// A record whose backup the app does not hold is left alone, bucket or not.
#[test]
fn recovery_ignores_a_record_for_a_backup_the_app_does_not_hold() {
    let b = Backend::mem();
    let e = env();
    b.claim("serial:PHONE-A");
    let temp = open_upload(&b, "DCIM/A.HEIC", 5);
    write_record(&e, &b, &temp, None);
    let elsewhere = e.base.join("some other folder");
    fs::create_dir_all(&elsewhere).unwrap();
    let got = e
        .state
        .recover_interrupted_for(&|_| vec![Destination::Local(elsewhere.clone())], &|_| {
            Ok(b.handle())
        })
        .unwrap()
        .unwrap();
    assert!(!got.temp_removed, "{}", got.note);
    assert!(
        got.note.contains("not a backup this app holds"),
        "{}",
        got.note
    );
    assert_eq!(b.open_uploads().len(), 1);
}

/// With no network at start-up the record is kept, so the next start tries again; then it is
/// dealt with.
#[test]
fn recovery_keeps_its_record_while_the_backup_cannot_be_reached() {
    let b = Backend::mem();
    let e = env();
    b.claim("serial:PHONE-A");
    let temp = open_upload(&b, "DCIM/A.HEIC", 5);
    write_record(&e, &b, &temp, None);
    b.mem_store().fail_nth_offline("abort_by_ref", 1);
    let got = recover_b(&e, &b);
    assert!(got.retry_later && !got.temp_removed, "{}", got.note);
    assert!(got.note.starts_with("No network:"), "{}", got.note);
    assert!(!no_crash_record(&e));
    assert_eq!(b.open_uploads().len(), 1);
    let got = recover_b(&e, &b);
    assert!(got.temp_removed && !got.retry_later, "{}", got.note);
    assert!(no_crash_record(&e));
    assert!(b.open_uploads().is_empty());
}

/// Recovery also asks the store to clean up its own unfinished uploads older than a day, and the
/// start-up form does the same for each bucket the app holds (never for a folder).
#[test]
fn recovery_cleans_up_day_old_unfinished_uploads() {
    let day = Duration::from_secs(24 * 60 * 60);
    let b = Backend::mem();
    let e = env();
    b.claim("serial:PHONE-A");
    let temp = open_upload(&b, "DCIM/A.HEIC", 5);
    write_record(&e, &b, &temp, None);
    let asked: Arc<Mutex<Vec<Duration>>> = Arc::default();
    let place = b.place(&e);
    let got = e
        .state
        .recover_interrupted_for(&|_| vec![place.clone()], &|_| {
            let mut w = Wrap::new(b.handle());
            w.stale = asked.clone();
            Ok(Box::new(w))
        })
        .unwrap()
        .unwrap();
    assert!(got.temp_removed, "{}", got.note);
    assert_eq!(*asked.lock().unwrap(), vec![day]);
    // The start-up form, without a record.
    let store = unreachable_store(&e.base);
    let bucket = store.destination().unwrap();
    let n =
        crate::state::clean_stale_uploads(&[bucket, Destination::Local(e.root.clone())], &|d| {
            match d {
                Destination::Local(_) => panic!("a folder has nothing to clean"),
                _ => {
                    let mut w = Wrap::new(b.handle());
                    w.stale = asked.clone();
                    Ok(Box::new(w))
                }
            }
        });
    assert_eq!(n, 0);
    assert_eq!(*asked.lock().unwrap(), vec![day, day]);
}

/// The crash record a copy into a bucket writes names the bucket, and old records (a folder's,
/// with no such field) still read.
#[test]
fn the_crash_record_names_the_bucket_and_old_records_still_read() {
    let e = env();
    let store = unreachable_store(&e.base);
    let dest = store.destination().unwrap();
    let rec = InProgress {
        device: "d".into(),
        root: dest.label(),
        source: "a".into(),
        temp: "s3mpu:id:p/a".into(),
        replace: None,
        destination: Some(dest.clone()),
    };
    let json = serde_json::to_string(&rec).unwrap();
    assert!(!json.contains("TESTSECRET") && !json.contains("TESTKEYID"));
    assert_eq!(serde_json::from_str::<InProgress>(&json).unwrap(), rec);
    assert!(rec.place().same_place(&dest));
    let old = r#"{"device":"d","root":"C:\\x","source":"a","temp":"a.pnk-part"}"#;
    let got: InProgress = serde_json::from_str(old).unwrap();
    assert_eq!(got.destination, None);
    assert_eq!(got.place(), Destination::Local(PathBuf::from("C:\\x")));
    // A folder's record is written without the field.
    let plain = InProgress {
        destination: None,
        ..rec
    };
    assert!(
        !serde_json::to_string(&plain)
            .unwrap()
            .contains("destination")
    );
}

// ---- folders on disk: the journal formats --------------------------------------------------

/// A record from the current release holds absolute paths. It is still read: cut down to the
/// part under the backup folder, and acted on the same way.
#[test]
fn an_old_journal_with_absolute_paths_is_recovered() {
    let e = env();
    let (o, a, t) = crash_state(&e, None, Some(b"old"), Some(b"new"), true);
    // Written as the release before this one wrote it.
    let json = serde_json::json!({
        "device": "serial:PHONE-A",
        "root": e.root,
        "source": "DCIM/IMG.HEIC",
        "temp": t,
        "replace": { "original": o, "archived": a, "goes": "archive" },
    });
    fs::write(
        e.state.path().join("in_progress.json"),
        serde_json::to_vec(&json).unwrap(),
    )
    .unwrap();
    let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
    assert!(got.completed, "{}", got.note);
    assert_eq!(fs::read(&o).unwrap(), b"new");
    assert_eq!(fs::read(&a).unwrap(), b"old");
    assert!(!t.exists());
}

/// What this release writes holds paths relative to the backup folder, and recovery finishes it
/// the same way.
#[test]
fn a_journal_with_store_relative_paths_is_recovered_and_is_what_a_copy_writes() {
    let e = env();
    let (o, a, t) = crash_state(&e, Some(b"old"), None, Some(b"new"), false);
    e.state
        .begin_copy(&InProgress {
            device: "serial:PHONE-A".into(),
            root: e.root.clone(),
            source: "DCIM/IMG.HEIC".into(),
            temp: format!("DCIM/IMG.HEIC{TEMP_SUFFIX}").into(),
            replace: Some(ReplaceStep {
                original: "DCIM/IMG.HEIC".into(),
                archived: format!("{ARCHIVE_DIR}/{STAMP}/IMG.HEIC").into(),
                goes: OldCopyGoes::Archive,
                staged_size: None,
            }),
            destination: None,
        })
        .unwrap();
    let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
    assert!(got.completed, "{}", got.note);
    assert_eq!(fs::read(&o).unwrap(), b"new");
    assert_eq!(fs::read(&a).unwrap(), b"old");
    assert!(!t.exists());

    // A relative path that climbs out of the folder is never acted on.
    let e = env();
    let (o, _, t) = crash_state(&e, Some(b"old"), None, Some(b"new"), false);
    e.state
        .begin_copy(&InProgress {
            device: "serial:PHONE-A".into(),
            root: e.root.clone(),
            source: "DCIM/IMG.HEIC".into(),
            temp: format!("../DCIM/IMG.HEIC{TEMP_SUFFIX}").into(),
            replace: None,
            destination: None,
        })
        .unwrap();
    let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
    assert!(!got.temp_removed, "{}", got.note);
    assert_eq!(fs::read(&o).unwrap(), b"old");
    assert_eq!(fs::read(&t).unwrap(), b"new");
}

/// The record a running copy writes is store-relative.
#[test]
fn a_running_copy_writes_a_relative_journal() {
    let e = env();
    let ctl = SyncControl::default();
    let mut obj = MemObject::new("1", "DCIM/BIG.MOV", &[1u8; 100_000]);
    // Stop part-way, with the temp's removal failing, so the record stays to be read.
    obj.trip = Some((10_000, ctl.skip_current.clone()));
    let mut src = phone(vec![obj]);
    let (plan, _) = do_scan(&mut src, &e);
    ctl.skip_entry
        .store(entry_of(&plan, "DCIM/BIG.MOV"), Ordering::SeqCst);
    {
        let _fail = crate::sync::fail_temp_removal();
        let (r, _) = do_sync(&mut src, &plan, &e, &pending(&plan), &ctl);
        assert_eq!(r.unwrap().skipped, 1);
    }
    let text = fs::read_to_string(e.state.path().join("in_progress.json")).unwrap();
    let rec: InProgress = serde_json::from_str(&text).unwrap();
    assert_eq!(
        rec.temp.to_string_lossy(),
        format!("DCIM/BIG.MOV{TEMP_SUFFIX}")
    );
    let got = e.state.recover_interrupted(&known(&e)).unwrap().unwrap();
    assert!(got.temp_removed, "{}", got.note);
}

// ---- a changed file is staged before its old copy moves ---------------------------------------

/// On an object store a small changed file used to be sent only at the finish, after the old
/// copy had moved. It is now sent as the one part of an upload first, and the crash record names
/// that upload and says it was staged.
#[test]
fn a_changed_small_file_is_on_the_server_before_its_old_copy_moves() {
    let b = Backend::fake_s3(Limits::default());
    let (e, fake) = (env(), b.fake.clone().unwrap());
    b.put("DCIM/IMG.HEIC", b"old", None);
    let mut src = phone(vec![
        MemObject::new("1", "DCIM/IMG.HEIC", b"the new one"),
        MemObject::new("2", "DCIM/NEW.HEIC", b"fresh"),
    ]);
    b.claim(&key_of(&src));
    let plan = scan_b(&mut src, &e, &b, &open_opts());
    let seen: Arc<Mutex<Option<InProgress>>> = Arc::default();
    let mut wrap = Wrap::new(b.handle());
    {
        let (path, seen) = (e.state.path().join("in_progress.json"), seen.clone());
        wrap.before_move = Some(Box::new(move |_, _| {
            if let Ok(text) = std::fs::read_to_string(&path) {
                *seen.lock().unwrap() = serde_json::from_str(&text).ok();
            }
        }));
    }
    let (r, ev) = sync_in(
        &mut src,
        &plan,
        &e,
        &wrap,
        &pending(&plan),
        &SyncControl::default(),
    );
    assert_eq!(r.unwrap().copied, 2, "{ev:?}");
    let log = fake.log();
    let at = |prefix: &str| {
        log.iter()
            .position(|l| l.starts_with(prefix))
            .unwrap_or_else(|| panic!("{prefix} not in {log:?}"))
    };
    // The part is up before the copy that archives the old file, and the upload is completed
    // after it. The new file went up in one request, the changed file's new copy in none.
    assert!(
        at("part p/DCIM/IMG.HEIC") < at("copy p/DCIM/IMG.HEIC"),
        "{log:?}"
    );
    assert!(
        at("copy p/DCIM/IMG.HEIC") < at("complete p/DCIM/IMG.HEIC"),
        "{log:?}"
    );
    let sent = |name: &str| log.iter().filter(|l| l.starts_with(name)).count();
    assert_eq!(
        sent("put p/DCIM/IMG.HEIC"),
        1,
        "only the old file's own: {log:?}"
    );
    assert_eq!(sent("put p/DCIM/NEW.HEIC"), 1, "{log:?}");
    assert!(
        !log.iter().any(|l| l.starts_with("create p/DCIM/NEW.HEIC")),
        "{log:?}"
    );
    assert_eq!(b.get("DCIM/IMG.HEIC").unwrap(), b"the new one");
    assert_eq!(
        b.get(&format!("{ARCHIVE_DIR}/{STAMP}/IMG.HEIC")).unwrap(),
        b"old"
    );
    // At the move the record named the staged upload.
    let rec = seen.lock().unwrap().clone().expect("a record at the move");
    assert!(rec.temp.to_string_lossy().starts_with("s3mpu:"), "{rec:?}");
    assert_eq!(
        rec.replace.as_ref().and_then(|r| r.staged_size),
        Some(b"the new one".len() as u64)
    );
    assert!(b.open_uploads().is_empty());
    assert!(no_crash_record(&e));
}

// ---- recovery rolls a staged upload forward ------------------------------------------------

/// The new copy of `DCIM/IMG.HEIC` staged in the store at `b` and then left, as a crash after
/// the stage does. Its reference.
fn staged_upload(b: &Backend, new: &[u8]) -> String {
    let mut up = b
        .store()
        .begin_upload(&rel("DCIM/IMG.HEIC"), new.len() as u64, Some(at(9)))
        .unwrap();
    up.write(new).unwrap();
    up.stage().unwrap();
    let r = up.journal_ref();
    std::mem::forget(up);
    r
}

fn staged_step(archived: &str, goes: OldCopyGoes, size: usize) -> ReplaceStep {
    ReplaceStep {
        staged_size: Some(size as u64),
        ..replace_step(archived, goes)
    }
}

/// A crash after the old copy moved aside: the new copy is whole in the store, so recovery
/// finishes it. The old copy stays where it went. (The same as the roll forward of a folder.)
#[test]
fn recovery_finishes_a_staged_upload_after_the_old_copy_moved() {
    for (goes, moved) in [
        (
            OldCopyGoes::Archive,
            format!("{ARCHIVE_DIR}/{STAMP}/IMG.HEIC"),
        ),
        (
            OldCopyGoes::Aside,
            "DCIM/IMG (old 2026-01-02 03-04).HEIC".to_string(),
        ),
    ] {
        for b in Backend::all_small_parts() {
            eprintln!("backend: {} {goes:?}", b.name);
            let e = env();
            b.claim("serial:PHONE-A");
            let new = vec![3u8; 6 * MIB];
            b.put("DCIM/IMG.HEIC", b"old", Some(at(5)));
            let temp = staged_upload(&b, &new);
            b.store()
                .move_no_replace(&rel("DCIM/IMG.HEIC"), &rel(&moved))
                .unwrap();
            write_record(&e, &b, &temp, Some(staged_step(&moved, goes, new.len())));
            let got = recover_b(&e, &b);
            assert!(got.completed && !got.retry_later, "{}", got.note);
            assert!(got.note.contains("in place"), "{}", got.note);
            assert!(b.get("DCIM/IMG.HEIC").unwrap() == new);
            assert_eq!(b.get(&moved).unwrap(), b"old");
            assert!(b.open_uploads().is_empty());
            assert!(no_crash_record(&e));
        }
    }
}

/// A small file's staged upload is a one-part upload, and rolls forward the same way.
#[test]
fn recovery_finishes_a_staged_small_upload() {
    for b in Backend::all() {
        eprintln!("backend: {}", b.name);
        let e = env();
        b.claim("serial:PHONE-A");
        let moved = format!("{ARCHIVE_DIR}/{STAMP}/IMG.HEIC");
        b.put("DCIM/IMG.HEIC", b"old", Some(at(5)));
        let temp = staged_upload(&b, b"the new copy");
        assert!(
            temp.starts_with("upload-") || temp.starts_with("s3mpu:"),
            "{temp}"
        );
        b.store()
            .move_no_replace(&rel("DCIM/IMG.HEIC"), &rel(&moved))
            .unwrap();
        write_record(
            &e,
            &b,
            &temp,
            Some(staged_step(&moved, OldCopyGoes::Archive, 12)),
        );
        let got = recover_b(&e, &b);
        assert!(got.completed, "{}", got.note);
        assert_eq!(b.get("DCIM/IMG.HEIC").unwrap(), b"the new copy");
        assert_eq!(b.get(&moved).unwrap(), b"old");
        assert!(b.open_uploads().is_empty());
    }
}

/// A move cut between its copy and its delete, with the new copy staged: the original goes
/// (the copy is shown to match), then the new copy is finished.
#[test]
fn recovery_finishes_a_staged_upload_after_a_cut_move() {
    for b in Backend::all() {
        eprintln!("backend: {}", b.name);
        let e = env();
        b.claim("serial:PHONE-A");
        let moved = format!("{ARCHIVE_DIR}/{STAMP}/IMG.HEIC");
        b.put("DCIM/IMG.HEIC", b"old", Some(at(5)));
        b.put(&moved, b"old", Some(at(5)));
        let temp = staged_upload(&b, b"the new copy");
        write_record(
            &e,
            &b,
            &temp,
            Some(staged_step(&moved, OldCopyGoes::Archive, 12)),
        );
        let got = recover_b(&e, &b);
        assert!(got.completed, "{}", got.note);
        assert!(got.note.contains("cut short"), "{}", got.note);
        assert_eq!(b.get("DCIM/IMG.HEIC").unwrap(), b"the new copy");
        assert_eq!(b.get(&moved).unwrap(), b"old");
        assert!(b.open_uploads().is_empty());
    }
}

/// Anything that does not check out: the upload is dropped and the old copy stays where it is.
#[test]
fn recovery_drops_a_staged_upload_that_does_not_check_out() {
    let moved = format!("{ARCHIVE_DIR}/{STAMP}/IMG.HEIC");
    for b in Backend::all() {
        eprintln!("backend: {}", b.name);
        // Not whole by the record's size.
        let e = env();
        b.claim("serial:PHONE-A");
        b.put("DCIM/IMG.HEIC", b"old", Some(at(5)));
        let temp = staged_upload(&b, b"the new copy");
        b.store()
            .move_no_replace(&rel("DCIM/IMG.HEIC"), &rel(&moved))
            .unwrap();
        write_record(
            &e,
            &b,
            &temp,
            Some(staged_step(&moved, OldCopyGoes::Archive, 99)),
        );
        let got = recover_b(&e, &b);
        assert!(!got.completed && got.temp_removed, "{}", got.note);
        assert!(got.note.contains("could not be finished"), "{}", got.note);
        assert_eq!(b.get("DCIM/IMG.HEIC"), None);
        assert_eq!(b.get(&moved).unwrap(), b"old");
        assert!(b.open_uploads().is_empty());
        assert!(no_crash_record(&e));
        // Before the old copy moved: nothing to roll forward, the old copy stays.
        let e = env();
        let _ = b
            .store()
            .move_no_replace(&rel(&moved), &rel("DCIM/IMG.HEIC"));
        let temp = staged_upload(&b, b"the new copy");
        write_record(
            &e,
            &b,
            &temp,
            Some(staged_step(&moved, OldCopyGoes::Archive, 12)),
        );
        let got = recover_b(&e, &b);
        assert!(!got.completed, "{}", got.note);
        assert!(got.note.contains("had not been moved"), "{}", got.note);
        assert_eq!(b.get("DCIM/IMG.HEIC").unwrap(), b"old");
        assert!(b.open_uploads().is_empty());
        // A different file turned up at the name since: never replaced.
        let e = env();
        b.store()
            .move_no_replace(&rel("DCIM/IMG.HEIC"), &rel(&moved))
            .unwrap();
        let temp = staged_upload(&b, b"the new copy");
        b.put("DCIM/IMG.HEIC", b"theirs", Some(at(7)));
        write_record(
            &e,
            &b,
            &temp,
            Some(staged_step(&moved, OldCopyGoes::Archive, 12)),
        );
        let got = recover_b(&e, &b);
        assert!(!got.completed, "{}", got.note);
        assert_eq!(b.get("DCIM/IMG.HEIC").unwrap(), b"theirs");
        assert_eq!(b.get(&moved).unwrap(), b"old");
        assert!(b.open_uploads().is_empty());
    }
}

/// A record from before staging (no `staged_size`) is handled as before: the upload is dropped.
#[test]
fn a_record_without_a_staged_size_still_drops_the_upload() {
    let b = Backend::mem();
    let e = env();
    b.claim("serial:PHONE-A");
    let moved = format!("{ARCHIVE_DIR}/{STAMP}/IMG.HEIC");
    b.put("DCIM/IMG.HEIC", b"old", Some(at(5)));
    let temp = staged_upload(&b, b"the new copy");
    b.store()
        .move_no_replace(&rel("DCIM/IMG.HEIC"), &rel(&moved))
        .unwrap();
    write_record(
        &e,
        &b,
        &temp,
        Some(replace_step(&moved, OldCopyGoes::Archive)),
    );
    let got = recover_b(&e, &b);
    assert!(!got.completed && got.temp_removed, "{}", got.note);
    assert_eq!(b.get("DCIM/IMG.HEIC"), None);
    assert!(b.open_uploads().is_empty());
}

// ---- a record for a lone archive move --------------------------------------------------------

/// On an object store a move is a copy and then a delete, so the archive step writes a crash
/// record around each one. A stop between the two, then a restart, removes the original once
/// the archive copy is shown to match.
#[test]
fn an_archive_move_on_an_object_store_is_recorded_and_a_cut_one_is_finished_at_start() {
    let b = Backend::mem();
    let (e, mem) = (env(), b.mem_store());
    b.put("DCIM/KEEP.HEIC", b"keep", Some(at(5)));
    b.put("DCIM/GONE.HEIC", b"gone", Some(at(5)));
    let mut src = phone(vec![MemObject::new("1", "DCIM/KEEP.HEIC", b"keep")]);
    b.claim(&key_of(&src));
    let mut opts = open_opts();
    opts.settings.compare_by_date = false;
    let plan = scan_b(&mut src, &e, &b, &opts);
    assert_eq!(plan.removed.len(), 1);
    let seen: Arc<Mutex<Option<InProgress>>> = Arc::default();
    let mut wrap = Wrap::new(b.handle());
    {
        let (path, seen) = (e.state.path().join("in_progress.json"), seen.clone());
        wrap.before_move = Some(Box::new(move |_, _| {
            let text = std::fs::read_to_string(&path).expect("a record before the move");
            *seen.lock().unwrap() = serde_json::from_str(&text).ok();
        }));
    }
    mem.stop_next_move_after_copy();
    let r = archive_removed_in(&wrap, &plan, STAMP, false, Some(&e.state), &mut |_| {}).unwrap();
    assert_eq!((r.moved, r.failed), (0, 1));
    let rec = seen
        .lock()
        .unwrap()
        .clone()
        .expect("the record was written first");
    assert!(rec.is_move_only(), "{rec:?}");
    assert_eq!(rec.source, "DCIM/GONE.HEIC");
    let step = rec.replace.clone().unwrap();
    assert_eq!(step.original.to_str(), Some("DCIM/GONE.HEIC"));
    assert_eq!(
        step.archived.to_str(),
        Some(format!("{ARCHIVE_DIR}/{STAMP}/GONE.HEIC").as_str())
    );
    // The call returned, so the record was cleared; a crash would have left it. Put it back.
    assert!(no_crash_record(&e));
    assert!(b.get("DCIM/GONE.HEIC").is_some() && b.get(&step.archived.to_string_lossy()).is_some());
    e.state.begin_copy(&rec).unwrap();
    let got = recover_b(&e, &b);
    assert!(got.note.contains("original was removed"), "{}", got.note);
    assert!(!got.completed);
    assert_eq!(b.get("DCIM/GONE.HEIC"), None);
    assert_eq!(b.get(&step.archived.to_string_lossy()).unwrap(), b"gone");
    assert!(b.get("DCIM/KEEP.HEIC").is_some());
    assert!(no_crash_record(&e));
    // A move that had not copied yet, and one that was done, leave things alone.
    let rec2 = InProgress {
        replace: Some(ReplaceStep {
            original: "DCIM/KEEP.HEIC".into(),
            archived: format!("{ARCHIVE_DIR}/{STAMP}/KEEP.HEIC").into(),
            goes: OldCopyGoes::Archive,
            staged_size: None,
        }),
        source: "DCIM/KEEP.HEIC".into(),
        ..rec.clone()
    };
    e.state.begin_copy(&rec2).unwrap();
    let got = recover_b(&e, &b);
    assert!(got.note.contains("had not been moved"), "{}", got.note);
    assert!(b.get("DCIM/KEEP.HEIC").is_some());
    e.state.begin_copy(&rec).unwrap();
    let got = recover_b(&e, &b);
    assert!(got.note.contains("is in the archive"), "{}", got.note);
}

/// Without a state folder nothing is recorded (the folder store never needs it).
#[test]
fn archiving_without_a_record_still_works_and_a_folder_store_writes_none() {
    let b = Backend::mem();
    let e = env();
    b.put("DCIM/KEEP.HEIC", b"keep", Some(at(5)));
    b.put("DCIM/GONE.HEIC", b"gone", Some(at(5)));
    let mut src = phone(vec![MemObject::new("1", "DCIM/KEEP.HEIC", b"keep")]);
    b.claim(&key_of(&src));
    let plan = scan_b(&mut src, &e, &b, &open_opts());
    let wrap = Wrap::new(b.handle());
    let r = archive_removed_in(&wrap, &plan, STAMP, false, None, &mut |_| {}).unwrap();
    assert_eq!(r.moved, 1);
    assert!(no_crash_record(&e));
}

// ---- the self-test's probe object is not backup content ----------------------------------------

#[test]
fn a_probe_object_in_the_store_is_not_scanned_as_backup_content() {
    let b = Backend::mem();
    let e = env();
    b.put("DCIM/IMG.HEIC", b"x", Some(at(5)));
    b.put_plain(".plug-n-keep-probe-0123", b"probe");
    let mut src = phone(vec![MemObject::new("1", "DCIM/IMG.HEIC", b"x")]);
    b.claim(&key_of(&src));
    let plan = scan_b(&mut src, &e, &b, &open_opts());
    assert!(plan.removed.is_empty(), "{:?}", plan.removed);
    assert!(
        plan.entries
            .iter()
            .all(|x| !x.source.as_str().contains("probe")),
        "{:?}",
        plan.entries
    );
}

// ---- the modified-time cache --------------------------------------------------------------

/// The memory store, the pretend server and (when switched on) the real bucket.
fn cache_backends() -> Vec<Backend> {
    let mut v = Backend::all();
    v.push(Backend::fake_s3(Limits::default()));
    v
}

/// How many times the cache object was read and written so far, where the backend can tell (the
/// real bucket cannot).
fn cache_io(b: &Backend) -> Option<(usize, usize)> {
    let name = crate::store::mtimes::NAME;
    if let Some(m) = &b.mem {
        let w = m.calls_for("write_new", name) + m.calls_for("write_replace", name);
        return Some((m.calls_for("read", name), w));
    }
    let log = b.fake.as_ref()?.log();
    let n = |op: &str| {
        log.iter()
            .filter(|l| **l == format!("{op} p/{name}"))
            .count()
    };
    Some((n("get"), n("put")))
}

fn assert_io(b: &Backend, want: (usize, usize), what: &str) {
    if let Some(got) = cache_io(b) {
        assert_eq!(got, want, "{}: cache reads and writes {what}", b.name);
    }
}

/// A scan reads the cache once and a sync writes it once; a rescan then looks nothing up.
#[test]
fn the_cache_costs_one_read_per_scan_and_one_write_per_sync() {
    for b in cache_backends() {
        eprintln!("backend: {}", b.name);
        let e = env();
        let mut one = MemObject::new("1", "a.txt", b"abc");
        one.modified = Some(at(100));
        let mut two = MemObject::new("2", "dir/b.bin", b"bbbbb");
        two.modified = Some(at(200));
        let mut src = phone(vec![one, two]);
        let plan = scan_b(&mut src, &e, &b, &storage_opts());
        assert_eq!(plan.mtime_lookups, 0);
        assert_io(&b, (1, 0), "after the first scan");
        let (r, ev) = sync_b(&mut src, &plan, &e, &b, &SyncControl::default());
        assert_eq!(r.unwrap().copied, 2, "{ev:?}");
        assert_io(&b, (1, 1), "after the first sync");
        // The cache is an app file: never part of the backup's content.
        assert!(owner::is_marker_name(crate::store::mtimes::NAME));

        let plan = scan_b(&mut src, &e, &b, &storage_opts());
        assert_eq!(
            plan.mtime_lookups, 0,
            "{}: every date is in the cache",
            b.name
        );
        assert!(plan.entries.iter().all(|x| x.status == Status::Verified));
        assert!(
            plan.removed.is_empty() && plan.outside.is_empty(),
            "{plan:?}"
        );
        assert_eq!(plan.backup_files, 2, "the cache is not counted as a file");
        assert_io(&b, (2, 1), "after the rescan");
        // Nothing to copy and nothing new to remember: no write.
        let (r, _) = sync_in(&mut src, &plan, &e, b.store(), &[], &SyncControl::default());
        r.unwrap();
        assert_io(&b, (2, 1), "after a sync that changed nothing");
    }
}

/// The bucket's settings object ([`crate::bucket_settings`]) is an app file: when it sits in a
/// place that is scanned (a device kept at the base folder by an earlier version), the scan does
/// not count it as a backup file, and a sync never touches it.
#[test]
fn the_buckets_settings_object_is_never_backup_content() {
    for b in cache_backends() {
        eprintln!("backend: {}", b.name);
        let e = env();
        let mut one = MemObject::new("1", "a.txt", b"abc");
        one.modified = Some(at(100));
        let mut src = phone(vec![one]);
        let settings = crate::bucket_settings::BucketSettings {
            forgotten: vec!["x".into()],
            ..Default::default()
        };
        crate::bucket_settings::write(b.store(), &settings, None).unwrap();
        let plan = scan_b(&mut src, &e, &b, &storage_opts());
        assert_eq!(plan.backup_files, 0, "{plan:?}");
        assert!(
            plan.removed.is_empty() && plan.outside.is_empty(),
            "{plan:?}"
        );
        let (r, _) = sync_b(&mut src, &plan, &e, &b, &SyncControl::default());
        assert_eq!(r.unwrap().copied, 1);
        let again = scan_b(&mut src, &e, &b, &storage_opts());
        assert_eq!(again.backup_files, 1, "only the file itself");
        assert!(again.removed.is_empty() && again.outside.is_empty());
        let loaded = crate::bucket_settings::read(b.store()).unwrap().unwrap();
        assert_eq!(loaded.settings.forgotten, ["x"], "left as it was");
    }
}

/// An object whose tag differs from the cache's is looked at; the others are not. An object
/// with no recorded time is adopted at the device's time.
#[test]
fn a_changed_object_is_looked_up_and_the_rest_come_from_the_cache() {
    for b in cache_backends() {
        eprintln!("backend: {}", b.name);
        let e = env();
        let mut one = MemObject::new("1", "a.txt", b"abc");
        one.modified = Some(at(100));
        let mut two = MemObject::new("2", "dir/b.bin", b"bbbbb");
        two.modified = Some(at(200));
        let mut src = phone(vec![one, two]);
        let plan = scan_b(&mut src, &e, &b, &storage_opts());
        sync_b(&mut src, &plan, &e, &b, &SyncControl::default())
            .0
            .unwrap();
        // Another tool replaces a.txt with the same number of bytes: a new tag, no `pnk-mtime`.
        b.store()
            .write_replace(&rel("a.txt"), b"xyz", None)
            .unwrap();
        let plan = scan_b(&mut src, &e, &b, &storage_opts());
        assert_eq!(plan.mtime_lookups, 1, "{}: only a.txt is looked at", b.name);
        assert!(plan.entries.iter().all(|x| x.status == Status::Verified));
        // The sync remembers what the scan found, even with nothing to copy.
        sync_in(&mut src, &plan, &e, b.store(), &[], &SyncControl::default())
            .0
            .unwrap();
        let plan = scan_b(&mut src, &e, &b, &storage_opts());
        assert_eq!(plan.mtime_lookups, 0, "{}: now in the cache", b.name);
        // The device's date was recorded for it, so a later change of the date is noticed.
        src.object_mut("1").modified = Some(at(9999));
        let plan = scan_b(&mut src, &e, &b, &storage_opts());
        assert_eq!(
            plan.entries[entry_of(&plan, "a.txt")].status,
            Status::Mismatch
        );
        assert_eq!(plan.mtime_lookups, 0);
    }
}

/// "Rebuild the cache" replaces the object with an empty one (it is never deleted); the next scan
/// looks every file up and the next sync writes a full cache again.
#[test]
fn forgetting_the_cache_makes_the_next_scan_rebuild_it() {
    for b in cache_backends() {
        eprintln!("backend: {}", b.name);
        let e = env();
        let mut one = MemObject::new("1", "a.txt", b"abc");
        one.modified = Some(at(100));
        let mut two = MemObject::new("2", "dir/b.bin", b"bbbbb");
        two.modified = Some(at(200));
        let mut src = phone(vec![one, two]);
        let plan = scan_b(&mut src, &e, &b, &storage_opts());
        sync_b(&mut src, &plan, &e, &b, &SyncControl::default())
            .0
            .unwrap();
        crate::store::mtimes::forget_cache(b.store()).unwrap();
        let object = b.get(crate::store::mtimes::NAME).expect("not deleted");
        let doc: serde_json::Value = serde_json::from_slice(&object).unwrap();
        assert!(doc["entries"].as_object().unwrap().is_empty(), "{doc}");
        // The device's own dates survived in the objects: both are found again by lookups.
        let plan = scan_b(&mut src, &e, &b, &storage_opts());
        assert_eq!(plan.mtime_lookups, 2, "{}", b.name);
        assert!(plan.entries.iter().all(|x| x.status == Status::Verified));
        sync_in(&mut src, &plan, &e, b.store(), &[], &SyncControl::default())
            .0
            .unwrap();
        let doc: serde_json::Value =
            serde_json::from_slice(&b.get(crate::store::mtimes::NAME).unwrap()).unwrap();
        assert_eq!(doc["entries"].as_object().unwrap().len(), 2, "{doc}");
        let plan = scan_b(&mut src, &e, &b, &storage_opts());
        assert_eq!(plan.mtime_lookups, 0, "{}", b.name);
    }
}

/// Another computer saved the cache between our scan and our save: its entries are kept, ours are
/// added, and nothing is lost.
#[test]
fn a_cache_saved_by_someone_else_meanwhile_is_merged() {
    for b in cache_backends() {
        eprintln!("backend: {}", b.name);
        let e = env();
        let mut one = MemObject::new("1", "a.txt", b"abc");
        one.modified = Some(at(100));
        let mut src = phone(vec![one]);
        let plan = scan_b(&mut src, &e, &b, &storage_opts());
        sync_b(&mut src, &plan, &e, &b, &SyncControl::default())
            .0
            .unwrap();
        let mut two = MemObject::new("2", "c.txt", b"cc");
        two.modified = Some(at(300));
        let mut src = phone(vec![
            {
                let mut o = MemObject::new("1", "a.txt", b"abc");
                o.modified = Some(at(100));
                o
            },
            two,
        ]);
        let plan = scan_b(&mut src, &e, &b, &storage_opts());
        // The other writer's save, after our read.
        let name = rel(crate::store::mtimes::NAME);
        let (bytes, _) = b.store().read(&name).unwrap().unwrap();
        let mut doc: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        doc["entries"]["theirs.bin"] = serde_json::json!({"size": 1, "tag": "t", "mtime": 5});
        b.store()
            .write_replace(&name, doc.to_string().as_bytes(), None)
            .unwrap();
        let before = cache_io(&b);
        let (r, ev) = sync_b(&mut src, &plan, &e, &b, &SyncControl::default());
        assert_eq!(r.unwrap().copied, 1, "{ev:?}");
        let doc: serde_json::Value =
            serde_json::from_slice(&b.get(name.as_str()).unwrap()).unwrap();
        let mut keys: Vec<&str> = doc["entries"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        assert_eq!(keys, ["a.txt", "c.txt", "theirs.bin"], "{}", b.name);
        // This sync: the refused save, one read of the other writer's cache, the merged save.
        // (The read in the check above is ours.)
        if let (Some((r0, w0)), Some((r1, w1))) = (before, cache_io(&b)) {
            assert_eq!((r1 - r0, w1 - w0), (2, 2), "{}", b.name);
        }
    }
}

/// A folder on this PC keeps its real file times: no cache object, no lookups.
#[test]
fn a_folder_on_this_pc_has_no_cache() {
    let e = env();
    let mut one = MemObject::new("1", "a.txt", b"abc");
    one.modified = Some(at(100));
    let mut src = phone(vec![one]);
    let (plan, _) = do_scan_with(&mut src, &e, &storage_opts());
    let (r, _) = do_sync_as(
        &mut src,
        &plan,
        &e,
        &pending(&plan),
        &SyncControl::default(),
        false,
    );
    assert_eq!(r.unwrap().copied, 1);
    assert!(
        !files_under_all(&e.root)
            .iter()
            .any(|f| f.contains("mtimes"))
    );
    let (plan, _) = do_scan_with(&mut src, &e, &storage_opts());
    assert!(plan.mtime_cache.is_none());
    assert_eq!(plan.mtime_lookups, 0);
    assert!(plan.entries.iter().all(|x| x.status == Status::Verified));
    // Forgetting is nothing to a folder.
    crate::store::mtimes::forget_cache(&LocalStore::new(&e.root)).unwrap();
}

// ---- JPG copies in an object store ----------------------------------------------------------

const HEIC_A: &[u8] = include_bytes!("../heic/fixtures/plain.heic");

fn jpeg_opts() -> ScanOptions {
    opts_with(DeviceSettings {
        jpeg_copies: true,
        ..open_settings()
    })
}

/// A HEIC in an object store gets a JPG beside it with the HEIC's date, recorded in the made list.
/// The HEIC goes through a temporary file of ours, which is gone afterwards; a second run finds
/// the JPG up to date.
#[test]
fn a_heic_in_an_object_store_gets_a_jpg_and_leaves_no_temp_file() {
    for b in cache_backends() {
        eprintln!("backend: {}", b.name);
        let seen: std::rc::Rc<std::cell::RefCell<Vec<PathBuf>>> = Default::default();
        let s2 = seen.clone();
        let _d = crate::heic::fake_decoder::set(move |p| {
            s2.borrow_mut().push(p.to_path_buf());
            Ok(crate::heic::fake_decoder::picture_for(p))
        });
        let e = env();
        let mut photo = MemObject::new("1", "DCIM/100APPLE/IMG_0001.HEIC", HEIC_A);
        photo.modified = Some(at(100));
        let mut objs = vec![photo];
        objs.push(MemObject::new("2", "DCIM/F0.MOV", b"video"));
        let mut src = phone(objs);
        let plan = scan_b(&mut src, &e, &b, &jpeg_opts());
        assert!(plan.jpg.enabled, "{:?}", plan.jpg);
        let (r, ev) = sync_b(&mut src, &plan, &e, &b, &SyncControl::default());
        assert_eq!(r.unwrap().copied, 2, "{ev:?}");
        let mut events = Vec::new();
        let report =
            crate::jpgs::make_jpegs_in(b.store(), &plan, &AtomicBool::new(false), &mut |ev| {
                events.push(ev)
            })
            .unwrap();
        assert_eq!(report.made, 1, "{}: {report:?} {events:?}", b.name);
        assert!(report.error.is_none() && report.failed == 0, "{report:?}");
        let jpg = b.get("DCIM/100APPLE/IMG_0001.JPG").expect("the JPG");
        assert_eq!(&jpg[..2], &[0xFF, 0xD8], "a JPEG");
        let st = b
            .store()
            .stat(&rel("DCIM/100APPLE/IMG_0001.JPG"))
            .unwrap()
            .unwrap();
        assert!(
            crate::plan::same_modified(st.mtime.unwrap(), at(100)),
            "{st:?}"
        );
        let made = crate::heic::made::MadeList::load_in(b.store());
        let entry = made.by_jpg("DCIM/100APPLE/IMG_0001.JPG").expect("recorded");
        assert_eq!(entry.heic, "DCIM/100APPLE/IMG_0001.HEIC");
        assert_eq!(entry.jpg_size, Some(jpg.len() as u64));
        // The temporary file was ours, in the temp folder, and is gone.
        let paths = seen.borrow();
        assert_eq!(paths.len(), 1);
        assert!(paths[0].starts_with(std::env::temp_dir()), "{:?}", paths[0]);
        let name = paths[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_lowercase();
        assert!(
            name.starts_with("plug-n-keep-") && name.ends_with(".heic"),
            "{name}"
        );
        assert!(!paths[0].exists());
        assert!(b.open_uploads().is_empty());
        // Again: up to date, nothing made, and it is not taken for a deleted device file.
        let again =
            crate::jpgs::make_jpegs_in(b.store(), &plan, &AtomicBool::new(false), &mut |_| {})
                .unwrap();
        assert_eq!((again.made, again.up_to_date), (0, 1), "{again:?}");
        let plan = scan_b(&mut src, &e, &b, &jpeg_opts());
        assert!(plan.removed.is_empty(), "{:?}", plan.removed);
        assert_eq!(plan.made_jpgs, 1);
    }
}

/// The backfill, "Try again" and the owner check work on an object store, and a JPEG of the
/// device's own beside a HEIC keeps its name.
#[test]
fn the_backfill_and_try_again_work_in_an_object_store() {
    use crate::jpgs::{backfill_in, forget_failures_in, retry_failures_in};
    use std::cell::Cell;
    for b in cache_backends() {
        eprintln!("backend: {}", b.name);
        let calls = std::rc::Rc::new(Cell::new(0));
        let c2 = calls.clone();
        let d = crate::heic::fake_decoder::set(move |p| {
            c2.set(c2.get() + 1);
            if c2.get() == 1 {
                return Err(crate::heic::DecodeError::Damaged("not a picture".into()));
            }
            Ok(crate::heic::fake_decoder::picture_for(p))
        });
        for (n, t) in [("A", 5), ("B", 6), ("C", 7)] {
            b.put(&format!("DCIM/{n}.HEIC"), HEIC_A, Some(at(t)));
        }
        // The device's own JPEG beside C.
        b.put("DCIM/C.JPG", b"theirs", Some(at(7)));
        b.claim("dev-key");
        let cancel = AtomicBool::new(false);
        let view = Some(View::WindowsWpd);
        let stranger = backfill_in(
            b.store(),
            "other",
            view,
            "_Archived",
            &[],
            &cancel,
            &mut |_| {},
        );
        assert!(
            matches!(stranger, Err(SyncError::WrongFolder(_))),
            "{stranger:?}"
        );
        let r = backfill_in(
            b.store(),
            "dev-key",
            view,
            "_Archived",
            &[],
            &cancel,
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(
            (r.made, r.failed, r.up_to_date),
            (1, 1, 1),
            "{}: {r:?}",
            b.name
        );
        assert!(b.get("DCIM/A.JPG").is_none(), "A failed to decode");
        assert!(b.get("DCIM/B.JPG").is_some());
        assert_eq!(b.get("DCIM/C.JPG").unwrap(), b"theirs");
        // Try again: the failure is forgotten and the JPG made.
        drop(d);
        let _d = crate::heic::fake_decoder::working();
        let r = retry_failures_in(b.store(), "dev-key", view, None, &cancel, &mut |_| {}).unwrap();
        assert_eq!((r.made, r.failed), (1, 0), "{r:?}");
        assert!(b.get("DCIM/A.JPG").is_some());
        let n = forget_failures_in(b.store(), "dev-key", view, None).unwrap();
        assert_eq!(n, 0, "nothing is on the failed list now");
        assert!(b.open_uploads().is_empty());
    }
}

// ---- sizes of an object store ---------------------------------------------------------------

/// Archive and content are told apart by the first part of the key; the rest is counted as content.
#[test]
fn the_sizes_of_an_object_store_split_archive_from_content() {
    for b in cache_backends() {
        eprintln!("backend: {}", b.name);
        b.put("a.bin", &[0; 10], None);
        b.put("DCIM/x.bin", &[0; 5], None);
        b.put("_Archived/2026/y.bin", &[0; 7], None);
        b.put("_Archived/z", &[0; 3], None);
        // Only a first part counts: a deeper folder of that name is content.
        b.put("DCIM/_Archived/w.bin", &[0; 4], None);
        let s = crate::space::folder_sizes_in(b.store(), &["_Archived".to_string()]);
        assert_eq!(
            (s.total, s.archive, s.files),
            (29, 10, 5),
            "{}: {s:?}",
            b.name
        );
    }
}
