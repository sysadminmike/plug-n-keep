//! A store that passes everything on and remembers if the network went away, so a sync can stop
//! with one clear answer instead of failing every remaining file.
#![forbid(unsafe_code)]

use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use super::{
    BackupStore, Destination, Listing, LocalStore, Stat, Tag, Upload, explain, is_offline,
};
use crate::paths::SafeRelPath;
use crate::platform::DiskSpace;

type Seen = Arc<Mutex<Option<String>>>;

fn note<T>(seen: &Seen, r: io::Result<T>) -> io::Result<T> {
    if let Err(e) = &r
        && is_offline(e)
    {
        seen.lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_or_insert_with(|| explain(e));
    }
    r
}

pub struct Watched<'a> {
    inner: &'a dyn BackupStore,
    seen: Seen,
}

impl<'a> Watched<'a> {
    pub fn new(inner: &'a dyn BackupStore) -> Self {
        Self {
            inner,
            seen: Seen::default(),
        }
    }

    /// The "No network: ..." message of the first call that could not reach the store, if any.
    pub fn offline(&self) -> Option<String> {
        self.seen.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn see<T>(&self, r: io::Result<T>) -> io::Result<T> {
        note(&self.seen, r)
    }
}

struct WatchedUpload {
    inner: Box<dyn Upload>,
    seen: Seen,
}

impl Upload for WatchedUpload {
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        note(&self.seen, self.inner.write(data))
    }
    fn seal(&mut self) -> io::Result<()> {
        note(&self.seen, self.inner.seal())
    }
    fn stage(&mut self) -> io::Result<()> {
        note(&self.seen, self.inner.stage())
    }
    fn finish_no_replace(self: Box<Self>) -> io::Result<Tag> {
        let seen = self.seen.clone();
        note(&seen, self.inner.finish_no_replace())
    }
    fn abort(self: Box<Self>) -> io::Result<()> {
        let seen = self.seen.clone();
        note(&seen, self.inner.abort())
    }
    fn journal_ref(&self) -> String {
        self.inner.journal_ref()
    }
}

impl BackupStore for Watched<'_> {
    fn list(&self, prune: &dyn Fn(&str) -> bool) -> io::Result<Listing> {
        self.see(self.inner.list(prune))
    }
    fn list_dirs(&self, under: &str) -> io::Result<Vec<String>> {
        self.see(self.inner.list_dirs(under))
    }
    fn stat(&self, rel: &SafeRelPath) -> io::Result<Option<Stat>> {
        self.see(self.inner.stat(rel))
    }
    fn read(&self, rel: &SafeRelPath) -> io::Result<Option<(Vec<u8>, Tag)>> {
        self.see(self.inner.read(rel))
    }
    fn write_new(&self, rel: &SafeRelPath, bytes: &[u8]) -> io::Result<Tag> {
        self.see(self.inner.write_new(rel, bytes))
    }
    fn write_replace(
        &self,
        rel: &SafeRelPath,
        bytes: &[u8],
        expect: Option<&Tag>,
    ) -> io::Result<Tag> {
        self.see(self.inner.write_replace(rel, bytes, expect))
    }
    fn begin_upload(
        &self,
        rel: &SafeRelPath,
        size: u64,
        mtime: Option<SystemTime>,
    ) -> io::Result<Box<dyn Upload>> {
        let up = self.see(self.inner.begin_upload(rel, size, mtime))?;
        Ok(Box::new(WatchedUpload {
            inner: up,
            seen: self.seen.clone(),
        }))
    }
    fn planned_ref(&self, rel: &SafeRelPath) -> Option<String> {
        self.inner.planned_ref(rel)
    }
    fn abort_by_ref(&self, journal_ref: &str) -> io::Result<()> {
        self.see(self.inner.abort_by_ref(journal_ref))
    }
    fn finish_by_ref(&self, journal_ref: &str, expected_size: u64) -> io::Result<Tag> {
        self.see(self.inner.finish_by_ref(journal_ref, expected_size))
    }
    fn move_no_replace(&self, from: &SafeRelPath, to: &SafeRelPath) -> io::Result<()> {
        self.see(self.inner.move_no_replace(from, to))
    }
    fn remove_verified_original(&self, from: &SafeRelPath, copy: &SafeRelPath) -> io::Result<bool> {
        self.see(self.inner.remove_verified_original(from, copy))
    }
    fn send_to_bin(&self, rel: &SafeRelPath) -> io::Result<()> {
        self.see(self.inner.send_to_bin(rel))
    }
    fn supports_bin(&self) -> bool {
        self.inner.supports_bin()
    }
    fn space(&self) -> Option<DiskSpace> {
        self.inner.space()
    }
    fn root_exists(&self) -> io::Result<bool> {
        self.see(self.inner.root_exists())
    }
    fn ensure_root(&self) -> io::Result<()> {
        self.see(self.inner.ensure_root())
    }
    fn clean_stale_uploads(&self, older_than: Duration) -> io::Result<usize> {
        self.see(self.inner.clean_stale_uploads(older_than))
    }
    fn destination(&self) -> Option<Destination> {
        self.inner.destination()
    }
    fn describe(&self) -> String {
        self.inner.describe()
    }
    fn as_local(&self) -> Option<&LocalStore> {
        self.inner.as_local()
    }
}
