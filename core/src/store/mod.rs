//! Where a backup lives: the storage behind a device's backup folder.
//!
//! [`BackupStore`] is the small set of operations the backup code needs, keyed by
//! [`SafeRelPath`] relative to the store's root. [`local::LocalStore`] is a folder on this PC and
//! calls the same helpers the code used before; `mem::MemStore` is an in-memory store with the
//! rules of an object store (no folders, no rename, conditional writes, uploads that appear only
//! when finished), for tests.
//!
//! Errors are `io::Error`:
//! - `ErrorKind::AlreadyExists`: a no-replace write or move was refused; nothing changed.
//! - a version mismatch (the file changed since it was read): see [`is_conflict`].
//! - `ErrorKind::Unsupported`: the store does not offer this (the bin on a memory store).
#![forbid(unsafe_code)]

use std::fmt;
use std::io;
use std::time::{Duration, SystemTime};

use crate::paths::SafeRelPath;
use crate::platform::DiskSpace;

pub mod creds;
pub mod dest;
pub mod find;
pub mod local;
#[cfg(any(test, feature = "fake-device"))]
pub mod mem;
pub mod mtimes;
pub mod watch;

pub mod s3;

pub use dest::Destination;
pub use local::LocalStore;
pub use s3::{S3Config, S3Store};
pub use watch::Watched;

/// A version tag: changes whenever the file's content is written. Opaque to callers; only
/// compare it for equality. (ETag on an object store; size and modified time locally.)
pub type Tag = String;

/// What a listed or stat'ed entry is. Folders are never listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    /// A symlink, junction or other reparse point. Reported, never followed.
    Link,
    Other,
}

/// One file found by [`BackupStore::list`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// `/`-separated, in the case the store holds it.
    pub rel: String,
    pub size: u64,
    pub mtime: Option<SystemTime>,
    pub tag: Option<Tag>,
    pub kind: Kind,
}

/// Something in the store the listing could not use (unreadable, or a name that is not valid
/// Unicode). `at` is a shown path, lossy for bad names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListIssue {
    pub at: String,
    pub why: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    pub files: Vec<Listed>,
    pub issues: Vec<ListIssue>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    pub size: u64,
    pub mtime: Option<SystemTime>,
    pub tag: Option<Tag>,
    pub kind: Kind,
}

/// The error inside an `io::Error` when a conditional replace found the file changed (or gone)
/// since its tag was read. Test with [`is_conflict`].
#[derive(Debug)]
pub struct VersionMismatch;

impl fmt::Display for VersionMismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the file changed since it was read")
    }
}

impl std::error::Error for VersionMismatch {}

pub(crate) fn conflict() -> io::Error {
    io::Error::other(VersionMismatch)
}

/// Whether `e` is a version mismatch from [`BackupStore::write_replace`].
pub fn is_conflict(e: &io::Error) -> bool {
    e.get_ref()
        .is_some_and(|inner| inner.downcast_ref::<VersionMismatch>().is_some())
}

/// The error inside an `io::Error` when the store could not be reached at all (no network,
/// server down, name not found). Test with [`is_offline`].
#[derive(Debug)]
pub struct Offline(pub String);

impl fmt::Display for Offline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Offline {}

/// Whether `e` means the store could not be reached.
pub fn is_offline(e: &io::Error) -> bool {
    e.get_ref()
        .is_some_and(|inner| inner.downcast_ref::<Offline>().is_some())
}

/// What to tell the user about a failed store call: "No network: can't reach ..." when the store
/// could not be reached, else the error itself (a refusal by the bucket says "no access to the
/// bucket").
pub fn explain(e: &io::Error) -> String {
    if is_offline(e) {
        format!("No network: {e}")
    } else {
        e.to_string()
    }
}

/// A file being sent in. Nothing is visible at the final name until
/// [`finish_no_replace`](Upload::finish_no_replace). Other store operations (moving the old copy
/// away, say) may happen between the [`seal`](Upload::seal) and the finish.
pub trait Upload: Send {
    fn write(&mut self, data: &[u8]) -> io::Result<()>;
    /// Makes the file whole and durable without making it visible: checks that exactly the size
    /// given at the start was written, gives it its modified time and gets it onto disk (an
    /// object store sends the last part but does not complete the upload). No more `write`s
    /// after it. Calling it again is fine. On failure the upload is still there for `abort`
    /// (a wrong size also removes it). A changed file's old copy only moves after this.
    fn seal(&mut self) -> io::Result<()>;
    /// [`seal`](Upload::seal), and also puts every byte where the store keeps it: after this an
    /// object store holds the whole file as an unfinished upload, so a crash can still finish
    /// it ([`BackupStore::finish_by_ref`]). Costs an object store more requests for a small
    /// file than a plain seal (which may keep a small file in memory until the finish), so it
    /// is for a changed file whose old copy is about to move away; a new file only needs the
    /// seal. The reference ([`journal_ref`](Upload::journal_ref)) may differ afterwards, so
    /// read it again. A folder store's seal already does all of this.
    fn stage(&mut self) -> io::Result<()> {
        self.seal()
    }
    /// Makes the file appear, never replacing one: `AlreadyExists` if the name is taken, and the
    /// upload is then gone (nothing left behind). Seals first if that has not been done. Other
    /// failures may leave the upload in place for [`BackupStore::abort_by_ref`].
    fn finish_no_replace(self: Box<Self>) -> io::Result<Tag>;
    /// Throws the upload away. Leaves nothing.
    fn abort(self: Box<Self>) -> io::Result<()>;
    /// What the crash record keeps to clean up after this upload: for a local store the temp
    /// file's path relative to the root (`DCIM/IMG_1.HEIC.pnk-part`), for an object store the
    /// upload id.
    fn journal_ref(&self) -> String;
}

pub trait BackupStore: Send + Sync {
    /// All files under the root, recursively, with folders for which `prune` (given the
    /// `/`-separated path of the folder) answers true left out whole.
    fn list(&self, prune: &dyn Fn(&str) -> bool) -> io::Result<Listing>;

    /// The names of the folders directly under `under` (a `/`-separated folder; `""` is the
    /// root), sorted. An object store lists them with a delimiter, a folder store reads one
    /// level. The default walks the whole listing.
    fn list_dirs(&self, under: &str) -> io::Result<Vec<String>> {
        let prefix = if under.is_empty() {
            String::new()
        } else {
            format!("{under}/")
        };
        let names: std::collections::BTreeSet<String> = self
            .list(&|_| false)?
            .files
            .iter()
            .filter_map(|f| f.rel.strip_prefix(&prefix)?.split_once('/'))
            .map(|(dir, _)| dir.to_string())
            .collect();
        Ok(names.into_iter().collect())
    }

    /// `None` when absent. Links are reported as such, not followed.
    fn stat(&self, rel: &SafeRelPath) -> io::Result<Option<Stat>>;

    fn read(&self, rel: &SafeRelPath) -> io::Result<Option<(Vec<u8>, Tag)>>;

    /// Creates the file; `AlreadyExists` if there is one.
    fn write_new(&self, rel: &SafeRelPath, bytes: &[u8]) -> io::Result<Tag>;

    /// Atomically replaces the file (or creates it). With `expect`, only if its tag is still
    /// that; otherwise (changed, or gone) the error satisfies [`is_conflict`]. Locally the
    /// check and the replace are two steps, so a change in between is not caught.
    fn write_replace(
        &self,
        rel: &SafeRelPath,
        bytes: &[u8],
        expect: Option<&Tag>,
    ) -> io::Result<Tag>;

    /// Starts sending a file of `size` bytes, which gets `mtime` (where known) when finished.
    fn begin_upload(
        &self,
        rel: &SafeRelPath,
        size: u64,
        mtime: Option<SystemTime>,
    ) -> io::Result<Box<dyn Upload>>;

    /// The [`Upload::journal_ref`] that [`begin_upload`](BackupStore::begin_upload) of `rel` will
    /// give, when it is known before the upload starts (a folder store), so the crash record can
    /// be written first. `None` for a store that only learns it when the upload starts.
    fn planned_ref(&self, _rel: &SafeRelPath) -> Option<String> {
        None
    }

    /// Throws away an unfinished upload from its [`Upload::journal_ref`], after a crash. Already
    /// gone is fine. Refuses anything that is not one of this app's uploads.
    fn abort_by_ref(&self, journal_ref: &str) -> io::Result<()>;

    /// Finishes an upload that was staged ([`Upload::stage`]) before a crash, from its
    /// [`Upload::journal_ref`], making the file appear (never replacing one: `AlreadyExists`,
    /// and the upload is then gone). Only if the store holds the whole file, `expected_size`
    /// bytes of it; otherwise an error and the upload is left for
    /// [`abort_by_ref`](BackupStore::abort_by_ref). `Unsupported` for a store that cannot (a
    /// folder store's temp file is moved into place instead).
    fn finish_by_ref(&self, _journal_ref: &str, _expected_size: u64) -> io::Result<Tag> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this store can't finish an upload later",
        ))
    }

    /// Moves a file, never replacing: `AlreadyExists` if `to` exists. On an object store this is
    /// a copy and then a delete, and a stop between the two leaves both.
    fn move_no_replace(&self, from: &SafeRelPath, to: &SafeRelPath) -> io::Result<()>;

    /// After a move was cut between its copy and its delete (an object store): deletes `from`
    /// only if `copy` is there with the same size and the same modified time (not the same tag:
    /// a copy has a tag of its own), and `from` has not changed since it was looked at. Returns
    /// whether it was deleted. `Ok(false)` is "not sure": both stay. A folder store never has a
    /// half-done move, so it never deletes here.
    fn remove_verified_original(
        &self,
        _from: &SafeRelPath,
        _copy: &SafeRelPath,
    ) -> io::Result<bool> {
        Ok(false)
    }

    /// Sends a plain file to the Recycle Bin or Trash. Never a permanent delete.
    fn send_to_bin(&self, rel: &SafeRelPath) -> io::Result<()>;
    fn supports_bin(&self) -> bool;

    /// Size and free space, or `None` if the store has no such limit to check.
    fn space(&self) -> Option<DiskSpace>;

    /// Whether the root exists (a missing backup folder means every file is new).
    fn root_exists(&self) -> io::Result<bool>;
    /// Makes the root if it is not there yet.
    fn ensure_root(&self) -> io::Result<()>;

    /// Aborts unfinished uploads older than `older_than` that this store holds (an object store
    /// keeps them until told otherwise). Returns how many. A folder store has none.
    fn clean_stale_uploads(&self, _older_than: Duration) -> io::Result<usize> {
        Ok(0)
    }

    /// Where this store is, when it is not a folder (a crash record then names it, to find the
    /// backup again). `None` for a folder: the record's root says it.
    fn destination(&self) -> Option<Destination> {
        None
    }

    /// For messages: the folder path, or the bucket address.
    fn describe(&self) -> String;

    /// Something the server's self-test found that works but is not perfect (a server that
    /// doesn't check upload checksums). `None` for a folder, or when there is nothing to say.
    fn server_note(&self) -> Option<String> {
        None
    }

    /// The folder store behind this one, for the few things only a folder on this PC can do
    /// (renaming the archive folder, checks that a folder still resolves inside the root).
    fn as_local(&self) -> Option<&LocalStore> {
        None
    }
}

#[cfg(test)]
mod tests;
