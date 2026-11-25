//! Devices that aren't devices: for tests and for the GUI's debug-only fake mode.
//!
//! - [`MemSource`]: files held in memory, with any names (including hostile ones a real
//!   folder can't hold), injectable read failures and a hook to trip the skip flag mid-file.
//! - [`FolderSource`]: serves the files of a folder on the PC, read-only.
//! - [`Lagging`]: wraps either, to fall one file behind after an abandoned transfer, as an
//!   iPhone did.
#![forbid(unsafe_code)]

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use log::debug;
use walkdir::WalkDir;

use crate::device::{
    DeviceError, DeviceFile, DeviceIdentity, DeviceSource, Enumeration, ObjectInfo, ScanIssue,
};
use crate::paths::SafeRelPath;

#[derive(Debug, Clone)]
pub struct MemObject {
    pub id: String,
    pub components: Vec<String>,
    pub data: Vec<u8>,
    /// What the device claims the size is (normally `data.len()`).
    pub reported_size: Option<u64>,
    pub persistent_id: Option<String>,
    /// The last-modified time the device reports.
    pub modified: Option<SystemTime>,
    /// Once the object has been opened for reading, [`DeviceSource::object_info`] reports this
    /// modified time instead: the file changed while it was being read.
    pub modified_after_read: Option<SystemTime>,
    /// The device reports this storage name for the file.
    pub storage: Option<String>,
    /// Reading fails once this many bytes have been read.
    pub fail_after: Option<usize>,
    /// Once this many bytes have been read, set the flag (e.g. `SyncControl::skip_current`).
    pub trip: Option<(usize, Arc<AtomicBool>)>,
    /// When reading starts, write this file on the PC: something turning up at the copy's
    /// target while it is being copied.
    pub on_first_read: Option<(PathBuf, Vec<u8>)>,
}

impl MemObject {
    /// `path` is split on `/`; use [`MemObject::with_components`] for names containing `/`.
    pub fn new(id: &str, path: &str, data: &[u8]) -> Self {
        Self::with_components(id, path.split('/').map(String::from).collect(), data)
    }

    pub fn with_components(id: &str, components: Vec<String>, data: &[u8]) -> Self {
        Self {
            id: id.to_string(),
            components,
            data: data.to_vec(),
            reported_size: Some(data.len() as u64),
            persistent_id: Some(format!("pid-{id}")),
            modified: None,
            modified_after_read: None,
            storage: None,
            fail_after: None,
            trip: None,
            on_first_read: None,
        }
    }
}

pub struct MemSource {
    pub identity: DeviceIdentity,
    pub objects: Vec<MemObject>,
    pub issues: Vec<ScanIssue>,
    pub incomplete: Vec<String>,
    /// Folders the device lists as empty.
    pub empty_folders: Vec<String>,
    /// Objects that have been opened for reading (see [`MemObject::modified_after_read`]).
    opened: std::collections::HashSet<String>,
}

impl MemSource {
    pub fn new(serial: &str, objects: Vec<MemObject>) -> Self {
        Self {
            identity: DeviceIdentity::new(
                "mem",
                "Test iPhone".into(),
                "Apple Inc.".into(),
                "iPhone".into(),
                Some(serial.into()),
            ),
            objects,
            issues: Vec::new(),
            incomplete: Vec::new(),
            empty_folders: Vec::new(),
            opened: std::collections::HashSet::new(),
        }
    }

    pub fn object_mut(&mut self, id: &str) -> &mut MemObject {
        self.objects
            .iter_mut()
            .find(|o| o.id == id)
            .expect("no such object")
    }

    fn find(&self, id: &str) -> Result<&MemObject, DeviceError> {
        self.objects
            .iter()
            .find(|o| o.id == id)
            .ok_or_else(|| DeviceError::Failed(format!("no object {id}")))
    }
}

impl DeviceSource for MemSource {
    fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    fn enumerate(
        &mut self,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(usize, &str),
    ) -> Result<Enumeration, DeviceError> {
        if cancel.load(Ordering::Relaxed) {
            return Err(DeviceError::Cancelled);
        }
        let files: Vec<DeviceFile> = self
            .objects
            .iter()
            .map(|o| DeviceFile {
                object_id: o.id.clone(),
                persistent_id: o.persistent_id.clone(),
                components: o.components.clone(),
                size: o.reported_size,
                modified: o.modified,
                storage: o.storage.clone(),
            })
            .collect();
        progress(files.len(), "");
        Ok(Enumeration {
            files,
            issues: self.issues.clone(),
            incomplete: self.incomplete.clone(),
            empty_folders: self.empty_folders.clone(),
        })
    }

    fn object_info(&mut self, object_id: &str) -> Result<ObjectInfo, DeviceError> {
        let opened = self.opened.contains(object_id);
        let o = self.find(object_id)?;
        Ok(ObjectInfo {
            name: o.components.last().cloned().unwrap_or_default(),
            size: o.reported_size,
            persistent_id: o.persistent_id.clone(),
            modified: match (opened, o.modified_after_read) {
                (true, Some(changed)) => Some(changed),
                _ => o.modified,
            },
            storage: o.storage.clone(),
        })
    }

    fn open_read(&mut self, object_id: &str) -> Result<Box<dyn Read + '_>, DeviceError> {
        self.find(object_id)?;
        self.opened.insert(object_id.to_string());
        let o = self.find(object_id)?;
        Ok(Box::new(MemReader {
            data: o.data.clone(),
            pos: 0,
            fail_after: o.fail_after,
            trip: o.trip.clone(),
            on_first_read: o.on_first_read.clone(),
        }))
    }
}

struct MemReader {
    data: Vec<u8>,
    pos: usize,
    fail_after: Option<usize>,
    trip: Option<(usize, Arc<AtomicBool>)>,
    on_first_read: Option<(PathBuf, Vec<u8>)>,
}

impl Read for MemReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some((path, data)) = self.on_first_read.take() {
            std::fs::write(path, data)?;
        }
        if let Some(limit) = self.fail_after
            && self.pos >= limit
        {
            return Err(io::Error::other("simulated device read failure"));
        }
        if let Some((at, flag)) = &self.trip
            && self.pos >= *at
        {
            flag.store(true, Ordering::Relaxed);
        }
        // Small reads, so tests can interrupt a file part-way.
        let n = buf.len().min(4096).min(self.data.len() - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Serves the files under a folder as if they were on a device. Object IDs are the files'
/// relative paths.
pub struct FolderSource {
    root: PathBuf,
    identity: DeviceIdentity,
}

impl FolderSource {
    pub fn new(root: &Path) -> io::Result<Self> {
        let root = std::fs::canonicalize(root)?;
        let id = format!("fake:{}", root.display());
        let identity = DeviceIdentity::new(
            &id,
            "Fake device".into(),
            "Fake".into(),
            format!("Folder {}", root.display()),
            None,
        );
        Ok(Self { root, identity })
    }

    /// Like [`FolderSource::new`], but named after the folder and with a serial number, so the
    /// device shows a plain name and is recognised the next time it is plugged in.
    pub fn named(root: &Path) -> io::Result<Self> {
        let mut src = Self::new(root)?;
        let name = src
            .root
            .file_name()
            .map_or_else(|| "Device".into(), |n| n.to_string_lossy().into_owned());
        let id = format!("fake:{}", src.root.display());
        src.identity =
            DeviceIdentity::new(&id, name.clone(), String::new(), name.clone(), Some(name));
        Ok(src)
    }

    fn path_of(&self, object_id: &str) -> Result<PathBuf, DeviceError> {
        SafeRelPath::parse(object_id)
            .map(|p| p.under(&self.root))
            .map_err(|e| DeviceError::Failed(e.to_string()))
    }
}

impl DeviceSource for FolderSource {
    fn identity(&self) -> &DeviceIdentity {
        &self.identity
    }

    fn local_root(&self) -> Option<&Path> {
        Some(&self.root)
    }

    fn enumerate(
        &mut self,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(usize, &str),
    ) -> Result<Enumeration, DeviceError> {
        let mut out = Enumeration::default();
        for entry in WalkDir::new(&self.root)
            .follow_links(false)
            .sort_by_file_name()
        {
            if cancel.load(Ordering::Relaxed) {
                return Err(DeviceError::Cancelled);
            }
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    out.issues.push(ScanIssue::new(
                        e.path()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default(),
                        e.to_string(),
                    ));
                    // Whatever is under the unreadable entry is unknown.
                    let folder = e
                        .path()
                        .and_then(|p| p.strip_prefix(&self.root).ok())
                        .map(|rel| rel.to_string_lossy().replace('\\', "/"))
                        .unwrap_or_default();
                    out.incomplete.push(folder);
                    continue;
                }
            };
            if entry.file_type().is_dir() {
                let rel = entry
                    .path()
                    .strip_prefix(&self.root)
                    .unwrap_or(entry.path());
                debug!(
                    "Listing {} on the fake device",
                    if rel.as_os_str().is_empty() {
                        "(top)".to_string()
                    } else {
                        rel.display().to_string()
                    }
                );
                let childless =
                    std::fs::read_dir(entry.path()).is_ok_and(|mut d| d.next().is_none());
                if childless && !rel.as_os_str().is_empty() {
                    out.empty_folders.push(
                        rel.iter()
                            .map(|c| c.to_string_lossy())
                            .collect::<Vec<_>>()
                            .join("/"),
                    );
                }
            }
            if !entry.file_type().is_file() {
                continue;
            }
            let Ok(rel) = entry.path().strip_prefix(&self.root) else {
                continue;
            };
            let components: Vec<String> = rel
                .iter()
                .map(|c| c.to_string_lossy().into_owned())
                .collect();
            let meta = entry.metadata().ok();
            let size = meta.as_ref().map(|m| m.len());
            let modified = meta.as_ref().and_then(|m| m.modified().ok());
            out.files.push(DeviceFile {
                object_id: components.join("/"),
                persistent_id: None,
                components,
                size,
                modified,
                storage: None,
            });
            if out.files.len() % 50 == 0 {
                progress(out.files.len(), &rel.display().to_string());
            }
        }
        progress(out.files.len(), "");
        Ok(out)
    }

    fn object_info(&mut self, object_id: &str) -> Result<ObjectInfo, DeviceError> {
        let p = self.path_of(object_id)?;
        let meta = std::fs::metadata(&p).map_err(|e| DeviceError::Failed(e.to_string()))?;
        Ok(ObjectInfo {
            name: p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            size: Some(meta.len()),
            persistent_id: None,
            modified: meta.modified().ok(),
            storage: None,
        })
    }

    fn open_read(&mut self, object_id: &str) -> Result<Box<dyn Read + '_>, DeviceError> {
        let p = self.path_of(object_id)?;
        Ok(Box::new(
            File::open(p).map_err(|e| DeviceError::Failed(e.to_string()))?,
        ))
    }
}

/// A device that behaves like a real iPhone did. Once a transfer is abandoned
/// (its reader dropped before the end of the data), every later transfer over the same
/// connection delivers the data of the file requested before it. A new `Lagging`, or
/// [`Lagging::reconnect`], is a new connection.
pub struct Lagging<S> {
    pub inner: S,
    lagging: Arc<AtomicBool>,
    last_requested: Option<String>,
}

impl<S> Lagging<S> {
    pub fn new(inner: S) -> Self {
        Self {
            inner,
            lagging: Arc::new(AtomicBool::new(false)),
            last_requested: None,
        }
    }

    /// A new connection: transfers deliver the right data again.
    pub fn reconnect(&mut self) {
        self.lagging.store(false, Ordering::Relaxed);
        self.last_requested = None;
    }
}

impl<S: DeviceSource> DeviceSource for Lagging<S> {
    fn identity(&self) -> &DeviceIdentity {
        self.inner.identity()
    }

    fn enumerate(
        &mut self,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(usize, &str),
    ) -> Result<Enumeration, DeviceError> {
        self.inner.enumerate(cancel, progress)
    }

    fn object_info(&mut self, object_id: &str) -> Result<ObjectInfo, DeviceError> {
        self.inner.object_info(object_id)
    }

    fn local_root(&self) -> Option<&Path> {
        self.inner.local_root()
    }

    fn open_read(&mut self, object_id: &str) -> Result<Box<dyn Read + '_>, DeviceError> {
        let served = match &self.last_requested {
            Some(previous) if self.lagging.load(Ordering::Relaxed) => previous.clone(),
            _ => object_id.to_string(),
        };
        self.last_requested = Some(object_id.to_string());
        let reader = self.inner.open_read(&served)?;
        Ok(Box::new(LaggingReader {
            inner: reader,
            at_end: false,
            lagging: self.lagging.clone(),
        }))
    }
}

struct LaggingReader<R> {
    inner: R,
    at_end: bool,
    lagging: Arc<AtomicBool>,
}

impl<R: Read> Read for LaggingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let r = self.inner.read(buf);
        if !buf.is_empty() && matches!(r, Ok(0)) {
            self.at_end = true;
        }
        r
    }
}

impl<R> Drop for LaggingReader<R> {
    fn drop(&mut self) {
        if !self.at_end {
            self.lagging.store(true, Ordering::Relaxed);
        }
    }
}
