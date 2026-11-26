//! An in-memory store with the rules of an object store, for tests: no folders, no rename (a move
//! is a copy and then a delete), writes that can be conditional, version tags that change on every
//! write, uploads that are invisible until finished, user modified times kept per object, keys
//! that differ by case, no free-space figure and no bin.
//!
//! Failures can be injected ([`MemStore::fail_nth`]) and a move can be stopped between its copy
//! and its delete ([`MemStore::stop_next_move_after_copy`]), for recovery tests.
#![forbid(unsafe_code)]

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;

use super::{BackupStore, Kind, Listed, Listing, Stat, Tag, Upload, conflict};
use crate::paths::SafeRelPath;
use crate::platform::DiskSpace;

#[derive(Debug, Clone)]
struct Object {
    data: Vec<u8>,
    mtime: Option<SystemTime>,
    tag: Tag,
}

struct Pending {
    key: String,
    size: u64,
    mtime: Option<SystemTime>,
    data: Vec<u8>,
    sealed: bool,
}

#[derive(Default)]
struct Inner {
    objects: BTreeMap<String, Object>,
    uploads: HashMap<u64, Pending>,
    next_upload: u64,
    version: u64,
    calls: HashMap<&'static str, usize>,
    /// `read`, `write_new` and `write_replace` calls by key.
    key_calls: HashMap<(&'static str, String), usize>,
    failures: Vec<(&'static str, usize, io::ErrorKind, bool)>,
    stop_move: bool,
    /// The listing carries no modified times (as an S3 listing).
    hide_list_mtimes: bool,
    /// Every counted call, in order.
    log: Vec<&'static str>,
}

impl Inner {
    fn next_tag(&mut self) -> Tag {
        self.version += 1;
        format!("etag-{}", self.version)
    }

    /// Counts the call and applies an injected failure that is due.
    fn hit(&mut self, op: &'static str) -> io::Result<()> {
        self.log.push(op);
        let n = self.calls.entry(op).or_insert(0);
        *n += 1;
        let n = *n;
        if let Some(i) = self
            .failures
            .iter()
            .position(|(o, at, _, _)| *o == op && *at == n)
        {
            let (_, _, kind, offline) = self.failures.remove(i);
            let why = format!("injected failure in {op} #{n}");
            return Err(if offline {
                io::Error::new(
                    kind,
                    super::Offline(format!("can't reach the memory store: {why}")),
                )
            } else {
                io::Error::new(kind, why)
            });
        }
        Ok(())
    }

    fn count_key(&mut self, op: &'static str, key: &str) {
        *self.key_calls.entry((op, key.to_string())).or_insert(0) += 1;
    }

    fn put(&mut self, key: &str, data: Vec<u8>, mtime: Option<SystemTime>) -> Tag {
        let tag = self.next_tag();
        self.objects.insert(
            key.to_string(),
            Object {
                data,
                mtime,
                tag: tag.clone(),
            },
        );
        tag
    }
}

#[derive(Clone, Default)]
pub struct MemStore {
    inner: Arc<Mutex<Inner>>,
}

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// A store whose listing gives no modified times, as an S3 listing: only `stat` has them.
    pub fn without_list_mtimes(self) -> Self {
        self.lock().hide_list_mtimes = true;
        self
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Makes the `n`th call (counting from 1, from now) of `op` fail with `kind`. Operations:
    /// `list stat read write_new write_replace begin_upload upload_write seal finish abort
    /// abort_by_ref finish_by_ref move remove_verified`.
    pub fn fail_nth(&self, op: &'static str, n: usize, kind: io::ErrorKind) {
        let mut g = self.lock();
        let seen = g.calls.get(op).copied().unwrap_or(0);
        g.failures.push((op, seen + n, kind, false));
    }

    /// Like [`fail_nth`](Self::fail_nth), but the failure is "could not reach the store"
    /// (`store::is_offline`).
    pub fn fail_nth_offline(&self, op: &'static str, n: usize) {
        let mut g = self.lock();
        let seen = g.calls.get(op).copied().unwrap_or(0);
        g.failures
            .push((op, seen + n, io::ErrorKind::ConnectionRefused, true));
    }

    /// The next `move_no_replace` copies, then stops with an `Interrupted` error without deleting
    /// the original.
    pub fn stop_next_move_after_copy(&self) {
        self.lock().stop_move = true;
    }

    /// Every counted call so far, in order.
    pub fn log(&self) -> Vec<&'static str> {
        self.lock().log.clone()
    }

    /// How many times `op` (`read`, `write_new` or `write_replace`) was called for `key`.
    pub fn calls_for(&self, op: &'static str, key: &str) -> usize {
        let g = self.lock();
        g.key_calls
            .get(&(op, key.to_string()))
            .copied()
            .unwrap_or(0)
    }

    /// How many times `op` has been called.
    pub fn calls(&self, op: &'static str) -> usize {
        self.lock().calls.get(op).copied().unwrap_or(0)
    }

    /// Puts an object in directly (a test's starting state), with a user modified time.
    pub fn insert(&self, key: &str, data: &[u8], mtime: Option<SystemTime>) -> Tag {
        self.lock().put(key, data.to_vec(), mtime)
    }

    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.lock().objects.get(key).map(|o| o.data.clone())
    }

    pub fn keys(&self) -> Vec<String> {
        self.lock().objects.keys().cloned().collect()
    }

    /// Ids of uploads started and neither finished nor aborted.
    pub fn open_uploads(&self) -> Vec<String> {
        let mut v: Vec<String> = self.lock().uploads.keys().map(|i| upload_ref(*i)).collect();
        v.sort();
        v
    }
}

fn upload_ref(id: u64) -> String {
    format!("upload-{id}")
}

impl BackupStore for MemStore {
    fn list(&self, prune: &dyn Fn(&str) -> bool) -> io::Result<Listing> {
        let mut g = self.lock();
        g.hit("list")?;
        let mut out = Listing::default();
        let hide = g.hide_list_mtimes;
        'keys: for (key, o) in &g.objects {
            let mut at = 0;
            while let Some(i) = key[at..].find('/') {
                at += i;
                if prune(&key[..at]) {
                    continue 'keys;
                }
                at += 1;
            }
            out.files.push(Listed {
                rel: key.clone(),
                size: o.data.len() as u64,
                mtime: if hide { None } else { o.mtime },
                tag: Some(o.tag.clone()),
                kind: Kind::File,
            });
        }
        Ok(out)
    }

    fn stat(&self, rel: &SafeRelPath) -> io::Result<Option<Stat>> {
        let mut g = self.lock();
        g.hit("stat")?;
        Ok(g.objects.get(rel.as_str()).map(|o| Stat {
            size: o.data.len() as u64,
            mtime: o.mtime,
            tag: Some(o.tag.clone()),
            kind: Kind::File,
        }))
    }

    fn read(&self, rel: &SafeRelPath) -> io::Result<Option<(Vec<u8>, Tag)>> {
        let mut g = self.lock();
        g.hit("read")?;
        g.count_key("read", rel.as_str());
        Ok(g.objects
            .get(rel.as_str())
            .map(|o| (o.data.clone(), o.tag.clone())))
    }

    fn write_new(&self, rel: &SafeRelPath, bytes: &[u8]) -> io::Result<Tag> {
        let mut g = self.lock();
        g.hit("write_new")?;
        g.count_key("write_new", rel.as_str());
        if g.objects.contains_key(rel.as_str()) {
            return Err(already_exists(rel.as_str()));
        }
        Ok(g.put(rel.as_str(), bytes.to_vec(), None))
    }

    fn write_replace(
        &self,
        rel: &SafeRelPath,
        bytes: &[u8],
        expect: Option<&Tag>,
    ) -> io::Result<Tag> {
        let mut g = self.lock();
        g.hit("write_replace")?;
        g.count_key("write_replace", rel.as_str());
        if let Some(expect) = expect
            && g.objects.get(rel.as_str()).map(|o| &o.tag) != Some(expect)
        {
            return Err(conflict());
        }
        Ok(g.put(rel.as_str(), bytes.to_vec(), None))
    }

    fn begin_upload(
        &self,
        rel: &SafeRelPath,
        size: u64,
        mtime: Option<SystemTime>,
    ) -> io::Result<Box<dyn Upload>> {
        let mut g = self.lock();
        g.hit("begin_upload")?;
        g.next_upload += 1;
        let id = g.next_upload;
        g.uploads.insert(
            id,
            Pending {
                key: rel.as_str().to_string(),
                size,
                mtime,
                data: Vec::new(),
                sealed: false,
            },
        );
        Ok(Box::new(MemUpload {
            store: self.clone(),
            id,
        }))
    }

    fn abort_by_ref(&self, journal_ref: &str) -> io::Result<()> {
        let mut g = self.lock();
        g.hit("abort_by_ref")?;
        let id = journal_ref
            .strip_prefix("upload-")
            .and_then(|n| n.parse::<u64>().ok())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{journal_ref:?} is not an upload of this app"),
                )
            })?;
        g.uploads.remove(&id);
        Ok(())
    }

    fn finish_by_ref(&self, journal_ref: &str, expected_size: u64) -> io::Result<Tag> {
        let mut g = self.lock();
        g.hit("finish_by_ref")?;
        let id = journal_ref
            .strip_prefix("upload-")
            .and_then(|n| n.parse::<u64>().ok())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{journal_ref:?} is not an upload of this app"),
                )
            })?;
        // Only a sealed upload holds the whole file.
        let whole = g
            .uploads
            .get(&id)
            .is_some_and(|p| p.sealed && p.data.len() as u64 == expected_size);
        if !whole {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the unfinished upload is not whole",
            ));
        }
        let p = g.uploads.remove(&id).expect("just found");
        if g.objects.contains_key(&p.key) {
            return Err(already_exists(&p.key));
        }
        Ok(g.put(&p.key, p.data, p.mtime))
    }

    fn move_no_replace(&self, from: &SafeRelPath, to: &SafeRelPath) -> io::Result<()> {
        let mut g = self.lock();
        g.hit("move")?;
        let Some(src) = g.objects.get(from.as_str()).cloned() else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{from} is not there"),
            ));
        };
        if g.objects.contains_key(to.as_str()) {
            return Err(already_exists(to.as_str()));
        }
        // Copy: the copy has the same data and user time, and a tag of its own.
        g.put(to.as_str(), src.data, src.mtime);
        if std::mem::take(&mut g.stop_move) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "stopped between the copy and the delete",
            ));
        }
        g.objects.remove(from.as_str());
        Ok(())
    }

    fn remove_verified_original(&self, from: &SafeRelPath, copy: &SafeRelPath) -> io::Result<bool> {
        let mut g = self.lock();
        g.hit("remove_verified")?;
        let (Some(orig), Some(dup)) = (g.objects.get(from.as_str()), g.objects.get(copy.as_str()))
        else {
            return Ok(false);
        };
        if orig.data.len() != dup.data.len() || orig.mtime.is_none() || orig.mtime != dup.mtime {
            return Ok(false);
        }
        g.objects.remove(from.as_str());
        Ok(true)
    }

    fn send_to_bin(&self, _rel: &SafeRelPath) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "there is no bin for this store",
        ))
    }

    fn supports_bin(&self) -> bool {
        false
    }

    fn space(&self) -> Option<DiskSpace> {
        None
    }

    fn root_exists(&self) -> io::Result<bool> {
        Ok(true)
    }

    fn ensure_root(&self) -> io::Result<()> {
        Ok(())
    }

    fn describe(&self) -> String {
        "memory store".into()
    }
}

fn already_exists(key: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("{key} already exists"),
    )
}

struct MemUpload {
    store: MemStore,
    id: u64,
}

impl Upload for MemUpload {
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        let mut g = self.store.lock();
        g.hit("upload_write")?;
        match g.uploads.get_mut(&self.id) {
            Some(p) if p.sealed => Err(io::Error::other("the upload is sealed")),
            Some(p) => {
                p.data.extend_from_slice(data);
                Ok(())
            }
            None => Err(io::Error::other("the upload is gone")),
        }
    }

    fn seal(&mut self) -> io::Result<()> {
        let mut g = self.store.lock();
        g.hit("seal")?;
        let Some(p) = g.uploads.get_mut(&self.id) else {
            return Err(io::Error::other("the upload is gone"));
        };
        if p.data.len() as u64 != p.size {
            let msg = format!(
                "{} bytes were written for a {}-byte file",
                p.data.len(),
                p.size
            );
            g.uploads.remove(&self.id);
            return Err(io::Error::new(io::ErrorKind::InvalidData, msg));
        }
        p.sealed = true;
        Ok(())
    }

    fn finish_no_replace(mut self: Box<Self>) -> io::Result<Tag> {
        let sealed = self
            .store
            .lock()
            .uploads
            .get(&self.id)
            .is_some_and(|p| p.sealed);
        if !sealed {
            self.seal()?;
        }
        let mut g = self.store.lock();
        g.hit("finish")?;
        let Some(p) = g.uploads.remove(&self.id) else {
            return Err(io::Error::other("the upload is gone"));
        };
        if p.data.len() as u64 != p.size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} bytes were written for a {}-byte file",
                    p.data.len(),
                    p.size
                ),
            ));
        }
        if g.objects.contains_key(&p.key) {
            return Err(already_exists(&p.key));
        }
        Ok(g.put(&p.key, p.data, p.mtime))
    }

    fn abort(self: Box<Self>) -> io::Result<()> {
        let mut g = self.store.lock();
        g.hit("abort")?;
        g.uploads.remove(&self.id);
        Ok(())
    }

    fn journal_ref(&self) -> String {
        upload_ref(self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{is_conflict, tests};

    fn rel(s: &str) -> SafeRelPath {
        SafeRelPath::parse(s).unwrap()
    }

    #[test]
    fn shared_rules_hold() {
        tests::run_all(|| (Box::new(MemStore::new()), Box::new(())));
    }

    #[test]
    fn a_move_stopped_between_copy_and_delete_leaves_both() {
        let s = MemStore::new();
        s.insert("a.jpg", b"data", None);
        s.stop_next_move_after_copy();
        let e = s
            .move_no_replace(&rel("a.jpg"), &rel("_Archived/s/a.jpg"))
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Interrupted);
        assert_eq!(s.keys(), vec!["_Archived/s/a.jpg", "a.jpg"]);
        assert_eq!(s.get("_Archived/s/a.jpg").unwrap(), b"data");
        // The hook is one-shot: the next move completes.
        s.move_no_replace(&rel("a.jpg"), &rel("_Archived/s/b.jpg"))
            .unwrap();
        assert_eq!(s.keys(), vec!["_Archived/s/a.jpg", "_Archived/s/b.jpg"]);
    }

    #[test]
    fn tags_change_on_every_write_and_a_copy_has_its_own() {
        let s = MemStore::new();
        let t1 = s.write_new(&rel("a"), b"same").unwrap();
        let t2 = s.write_replace(&rel("a"), b"same", None).unwrap();
        assert_ne!(t1, t2);
        s.move_no_replace(&rel("a"), &rel("b")).unwrap();
        assert_ne!(s.stat(&rel("b")).unwrap().unwrap().tag, Some(t2));
    }

    #[test]
    fn keys_are_case_sensitive_and_there_are_no_folders() {
        let s = MemStore::new();
        s.write_new(&rel("DCIM/IMG.JPG"), b"1").unwrap();
        s.write_new(&rel("dcim/img.jpg"), b"2").unwrap();
        s.write_new(&rel("DCIM"), b"3").unwrap();
        assert_eq!(s.keys().len(), 3);
        assert!(s.stat(&rel("Dcim")).unwrap().is_none());
    }

    #[test]
    fn nothing_to_check_and_no_bin() {
        let s = MemStore::new();
        assert!(s.space().is_none());
        assert!(!s.supports_bin());
        s.insert("a", b"x", None);
        let e = s.send_to_bin(&rel("a")).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Unsupported);
        assert!(s.get("a").is_some());
    }

    #[test]
    fn failures_can_be_injected_by_call_number() {
        let s = MemStore::new();
        s.fail_nth("write_new", 2, io::ErrorKind::TimedOut);
        s.write_new(&rel("a"), b"1").unwrap();
        let e = s.write_new(&rel("b"), b"2").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        s.write_new(&rel("b"), b"2").unwrap();
        assert_eq!(s.calls("write_new"), 3);
        assert_eq!(s.keys(), vec!["a", "b"]);
        // An upload that fails at its finish stays open for abort_by_ref.
        s.fail_nth("finish", 1, io::ErrorKind::ConnectionReset);
        let mut up = s.begin_upload(&rel("c"), 1, None).unwrap();
        up.write(b"x").unwrap();
        let r = up.journal_ref();
        assert!(up.finish_no_replace().is_err());
        assert!(s.get("c").is_none());
        s.abort_by_ref(&r).unwrap();
        assert!(s.open_uploads().is_empty());
        assert!(!is_conflict(&io::Error::other("x")));
    }

    #[test]
    fn open_uploads_are_tracked() {
        let s = MemStore::new();
        let up = s.begin_upload(&rel("a"), 0, None).unwrap();
        assert_eq!(s.open_uploads(), vec![up.journal_ref()]);
        up.abort().unwrap();
        assert!(s.open_uploads().is_empty());
        assert!(s.abort_by_ref("not-ours").is_err());
    }
}
