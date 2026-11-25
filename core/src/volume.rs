//! A mounted file system as a read-only [`DeviceSource`]: a stick, card or drive. The walk and
//! the reads are the same on Windows, Linux and FreeBSD; listing the volumes and finding out
//! what they are is per system (`platform/*/volume.rs`).
//!
//! - The walk is iterative, follows no links (a link is an issue), stops at depth
//!   [`MAX_DEPTH`], and turns every unreadable entry into an issue and an incomplete folder.
//! - Every read of the device goes through [`open_read_only`], the one function that opens a
//!   file here. Nothing in this module can write.
//! - The app's own ID file is left out. What to back up and what to leave out is decided
//!   elsewhere.
#![forbid(unsafe_code)]

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use log::{debug, info, warn};

use crate::device::{
    DeviceError, DeviceFile, DeviceIdentity, DeviceSource, Enumeration, ObjectInfo, ScanIssue,
    strip_direction_marks,
};
use crate::idfile::{self, IdFile};
use crate::platform::{ListedDevice, Transport};
use crate::signals::{DeviceKind, Signals, View};

/// The deepest folder nesting listed. Folders below it are reported and left unlisted.
pub const MAX_DEPTH: usize = 64;

/// Opens a file for reading, and for nothing else. Every read of a device's files goes through
/// here.
///
/// - Links are not followed: on Linux and FreeBSD with `O_NOFOLLOW`, on Windows by opening a
///   reparse point itself.
/// - On Linux it also asks for `O_NOATIME`, so reading doesn't change the file's access date. The
///   system refuses that for files the user doesn't own (`EPERM`); it then opens again without.
/// - What was opened must be a regular file. A file swapped for a pipe between the check and the
///   open would otherwise block the open for ever: `O_NONBLOCK` makes the open return, and the
///   opened handle is then checked.
pub(crate) fn open_read_only(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        #[cfg(target_os = "linux")]
        {
            opts.custom_flags(libc::O_NOFOLLOW | libc::O_NOATIME | libc::O_NONBLOCK);
            match opts.open(path) {
                Err(e) if e.raw_os_error() == Some(libc::EPERM) => {
                    opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
                    opts.open(path)
                }
                other => other,
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
            opts.open(path)
        }
    }?;
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        opts.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        opts.open(path)
    }?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "it is not a regular file",
        ));
    }
    Ok(file)
}

/// What a platform's listing found out about one mounted volume.
#[derive(Debug, Clone, Default)]
pub struct VolumeDetails {
    /// Where it is mounted: `E:\` on Windows, the mount point elsewhere.
    pub root: PathBuf,
    pub label: Option<String>,
    pub file_system: Option<String>,
    /// The USB device's maker string, if the system gives one.
    pub maker: Option<String>,
    /// The USB device's product string, or the disk model.
    pub model: Option<String>,
    /// Volume serial, partition ID, USB IDs, label and capacity. The ID file is added by
    /// [`VolumeSource::open`].
    pub signals: Signals,
}

impl VolumeDetails {
    /// How the device picker names it: `Label (E:\) 29.8 GB`.
    pub fn picker_label(&self) -> String {
        let name = self
            .label
            .as_deref()
            .or(self.model.as_deref())
            .filter(|n| !n.trim().is_empty())
            .unwrap_or("Drive");
        let mut s = format!("{name} ({})", self.root.display());
        if let Some(c) = self.signals.capacity {
            s.push(' ');
            s.push_str(&human_size(c));
        }
        s
    }

    /// The entry for the device list. Its ID is the mount point, which [`open_listed`] uses to
    /// open it.
    pub fn listed(&self) -> ListedDevice {
        ListedDevice {
            id: self.root.to_string_lossy().into_owned(),
            label: self.picker_label(),
            kind_hint: Some(DeviceKind::Storage),
            transport: Transport::Volume,
        }
    }
}

/// `29.8 GB`: 1024-based, with one decimal, the way the rest of the app writes sizes.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Opens the volume a platform listing named, as a read-only source.
pub fn open_listed(details: VolumeDetails) -> Result<Box<dyn DeviceSource>, String> {
    Ok(Box::new(VolumeSource::open(details)?))
}

/// A mounted file system, read through plain read-only file opens.
pub struct VolumeSource {
    root: PathBuf,
    identity: DeviceIdentity,
    id_file: IdFile,
    /// Which volume the root was when this source was opened ([`crate::platform::volume_token`]).
    /// Checked again before every listing and every read: a stick pulled out and another put in
    /// at the same place must not be read as this one.
    token: u64,
}

impl VolumeSource {
    /// Opens the volume: checks the root is a folder, and reads the ID file (the strongest
    /// signal) into the identity's signals. Nothing is kept open afterwards.
    pub fn open(details: VolumeDetails) -> Result<Self, String> {
        let root = details.root.clone();
        let meta =
            fs::metadata(&root).map_err(|e| format!("Could not open {}: {e}", root.display()))?;
        if !meta.is_dir() {
            return Err(format!("{} is not a folder", root.display()));
        }
        let token = crate::platform::volume_token(&root)
            .map_err(|e| format!("Could not identify the volume at {}: {e}", root.display()))?;
        let id_file = idfile::read(&root);
        match &id_file {
            IdFile::Found(id) => debug!("{} has the ID file {id}", root.display()),
            IdFile::Damaged(why) => warn!("The ID file on {} {why}", root.display()),
            IdFile::Missing => debug!("{} has no ID file", root.display()),
        }
        let mut signals = details.signals;
        signals.id_file_id = id_file.id().map(str::to_string);
        if signals.label.is_none() {
            signals.label = details.label.clone().filter(|l| !l.trim().is_empty());
        }
        let key = if let Some(s) = &signals.volume_serial {
            format!("volume:{}", s.trim().to_lowercase())
        } else if let Some(s) = &signals.usb_serial {
            format!("usb:{}", s.trim().to_lowercase())
        } else {
            format!("path:{}", root.display())
        };
        let friendly_name = details
            .label
            .clone()
            .filter(|l| !l.trim().is_empty())
            .unwrap_or_else(|| root.display().to_string());
        let identity = DeviceIdentity {
            key,
            signals,
            view: View::Files,
            friendly_name,
            manufacturer: details.maker.unwrap_or_default(),
            model: details.model.unwrap_or_default(),
            serial: None,
            kind_hint: Some(DeviceKind::Storage),
        };
        info!(
            "Opened the volume {} ({})",
            root.display(),
            identity.label()
        );
        Ok(Self {
            root,
            identity,
            id_file,
            token,
        })
    }

    /// Fails unless the root is still on the volume that was opened.
    fn check_same_volume(&self) -> Result<(), DeviceError> {
        match crate::platform::volume_token(&self.root) {
            Ok(now) if now == self.token => Ok(()),
            Ok(_) => Err(DeviceError::Failed(format!(
                "{} is no longer the volume that was opened (it was removed, or another one \
                 took its place); open the device again",
                self.root.display()
            ))),
            Err(e) => Err(DeviceError::Failed(format!(
                "could not check the volume at {}: {e}",
                self.root.display()
            ))),
        }
    }

    /// What the ID file at the top of the volume said when the volume was opened.
    pub fn id_file(&self) -> &IdFile {
        &self.id_file
    }

    /// The file for an object ID, with every part checked: no `.`, `..`, separators, drive
    /// prefixes or links on the way. Returns its metadata too (not following links).
    fn resolve(&self, object_id: &str) -> Result<(PathBuf, fs::Metadata), DeviceError> {
        let bad = |why: &str| DeviceError::Failed(format!("{object_id}: {why}"));
        let mut path = self.root.clone();
        let parts: Vec<&str> = object_id.split('/').collect();
        let mut last_meta = None;
        for (i, part) in parts.iter().enumerate() {
            let mut comps = Path::new(part).components();
            let plain = matches!(
                (comps.next(), comps.next()),
                (Some(Component::Normal(n)), None) if n == *part
            );
            if !plain {
                return Err(bad("not a plain file name"));
            }
            path.push(part);
            let meta = fs::symlink_metadata(&path)
                .map_err(|e| DeviceError::Failed(format!("{object_id}: {e}")))?;
            if crate::platform::is_link(&meta) {
                return Err(bad("is a link"));
            }
            if i + 1 < parts.len() && !meta.is_dir() {
                return Err(bad("is not inside a folder"));
            }
            last_meta = Some(meta);
        }
        match last_meta {
            Some(m) if m.is_file() => Ok((path, m)),
            _ => Err(bad("is not a file")),
        }
    }
}

impl DeviceSource for VolumeSource {
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
        self.check_same_volume()?;
        let expect = fs::metadata(&self.root).ok().and_then(|m| dev_of(&m));
        walk(&self.root, expect, cancel, progress)
    }

    fn object_info(&mut self, object_id: &str) -> Result<ObjectInfo, DeviceError> {
        let (path, meta) = self.resolve(object_id)?;
        Ok(ObjectInfo {
            name: path
                .file_name()
                .map(|n| strip_direction_marks(&n.to_string_lossy()))
                .unwrap_or_default(),
            size: Some(meta.len()),
            persistent_id: None,
            modified: meta.modified().ok(),
            storage: None,
        })
    }

    fn open_read(&mut self, object_id: &str) -> Result<Box<dyn Read + '_>, DeviceError> {
        self.check_same_volume()?;
        let (path, _) = self.resolve(object_id)?;
        let file =
            open_read_only(&path).map_err(|e| DeviceError::Failed(format!("{object_id}: {e}")))?;
        Ok(Box::new(file))
    }
}

/// Folders at the top of a volume that belong to the system, not to the user: never listed,
/// counted or reported (so they never make the scan look incomplete), whatever their
/// permissions. Compared ignoring case.
fn is_system_folder(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower == "system volume information"
        || lower == "$recycle.bin"
        || lower == "lost+found"
        || lower.starts_with(".trash-")
}

/// Directories already listed, by (file system, inode), so a directory reachable twice (a bind
/// mount, a hard-linked directory) is listed once and the walk can never loop. Windows has no
/// such pairs and follows no links, so it records nothing.
#[derive(Default)]
struct SeenDirs {
    #[cfg(unix)]
    seen: std::collections::HashSet<(u64, u64)>,
}

impl SeenDirs {
    /// `false` if this directory was seen before.
    fn first_visit(&mut self, meta: &fs::Metadata) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            self.seen.insert((meta.dev(), meta.ino()))
        }
        #[cfg(not(unix))]
        {
            let _ = meta;
            true
        }
    }
}

/// The file system `meta` is on, where the system says (Unix), for [`walk`]'s check that no
/// other file system is mounted inside the volume.
fn dev_of(meta: &fs::Metadata) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(meta.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

/// The read-only walk of `root`. `expect_fs`, where the system gives one, is the file system
/// the root is on: a folder on another one is something mounted inside the stick, and is not
/// entered.
fn walk(
    root: &Path,
    expect_fs: Option<u64>,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(usize, &str),
) -> Result<Enumeration, DeviceError> {
    let mut out = Enumeration::default();
    let mut seen = SeenDirs::default();
    if let Ok(meta) = fs::metadata(root) {
        seen.first_visit(&meta);
    }
    // Folders still to list: the real names (to open them) and the cleaned ones (to report).
    let mut stack: Vec<(PathBuf, Vec<String>, Vec<String>)> =
        vec![(root.to_path_buf(), Vec::new(), Vec::new())];
    while let Some((dir, components, raw)) = stack.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Err(DeviceError::Cancelled);
        }
        let shown = components.join("/");
        progress(out.files.len(), &shown);
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                out.issues.push(ScanIssue::new(
                    shown.clone(),
                    format!("can't be listed: {e}"),
                ));
                out.incomplete.push(shown);
                continue;
            }
        };
        let mut names = Vec::new();
        let mut incomplete_here = false;
        for entry in entries {
            match entry {
                Ok(e) => names.push(e.file_name()),
                Err(e) => {
                    out.issues
                        .push(ScanIssue::new(shown.clone(), format!("can't be read: {e}")));
                    incomplete_here = true;
                }
            }
        }
        names.sort();
        let listed_nothing = names.is_empty();
        let mut subfolders = Vec::new();
        for name in names {
            if cancel.load(Ordering::Relaxed) {
                return Err(DeviceError::Cancelled);
            }
            let Some(text) = name.to_str() else {
                out.issues.push(ScanIssue::new(
                    join(&shown, &name.to_string_lossy()),
                    "the name is not valid text, so it is not listed",
                ));
                incomplete_here = true;
                continue;
            };
            let clean = strip_direction_marks(text);
            let here = join(&shown, &clean);
            let path = dir.join(&name);
            let meta = match fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(e) => {
                    out.issues
                        .push(ScanIssue::new(here, format!("can't be read: {e}")));
                    incomplete_here = true;
                    continue;
                }
            };
            if crate::platform::is_link(&meta) {
                out.issues
                    .push(ScanIssue::new(here, "is a link, so it is not followed"));
                incomplete_here = true;
                continue;
            }
            if meta.is_dir() {
                if components.is_empty() && is_system_folder(&clean) {
                    debug!("Leaving out the system folder {clean}");
                    continue;
                }
                if expect_fs.is_some() && dev_of(&meta) != expect_fs {
                    out.issues.push(ScanIssue::new(
                        here.clone(),
                        "is another file system mounted inside the device, so it is not listed",
                    ));
                    out.incomplete.push(here);
                } else if !seen.first_visit(&meta) {
                    out.issues.push(ScanIssue::new(
                        here.clone(),
                        "is the same folder as one already listed, so it is not listed again",
                    ));
                    out.incomplete.push(here);
                } else if components.len() + 1 > MAX_DEPTH {
                    out.issues.push(ScanIssue::new(
                        here.clone(),
                        format!(
                            "is nested more than {MAX_DEPTH} folders deep, so it is not listed"
                        ),
                    ));
                    out.incomplete.push(here);
                } else {
                    let mut c = components.clone();
                    c.push(clean);
                    let mut r = raw.clone();
                    r.push(text.to_string());
                    subfolders.push((path, c, r));
                }
            } else if meta.is_file() {
                if components.is_empty() && clean.eq_ignore_ascii_case(idfile::FILE_NAME) {
                    continue;
                }
                let mut comps = components.clone();
                comps.push(clean);
                let mut object = raw.clone();
                object.push(text.to_string());
                out.files.push(DeviceFile {
                    object_id: object.join("/"),
                    persistent_id: None,
                    components: comps,
                    size: Some(meta.len()),
                    modified: meta.modified().ok(),
                    storage: None,
                });
            } else {
                out.issues.push(ScanIssue::new(
                    here,
                    "is not a file or folder, so it is skipped",
                ));
            }
        }
        if incomplete_here {
            out.incomplete.push(shown.clone());
        } else if listed_nothing && !components.is_empty() {
            // Listed, and nothing there: backup files under it wait for a confirmation.
            out.empty_folders.push(shown);
        }
        // Reverse, so the stack lists them in name order.
        stack.extend(subfolders.into_iter().rev());
    }
    progress(out.files.len(), "");
    Ok(out)
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{ScanOptions, Status, scan};
    use crate::registry::{ArchiveLayout, DeviceSettings, FileTypes};
    use crate::state::{Scope, StateDir};
    use crate::sync::{CopyMode, SyncControl, SyncItem, run_sync};
    use std::collections::BTreeMap;
    use std::time::{Duration, SystemTime};

    fn details(root: &Path) -> VolumeDetails {
        VolumeDetails {
            root: root.to_path_buf(),
            label: Some("STICK".into()),
            signals: Signals {
                volume_serial: Some("ABCD-1234".into()),
                ..Signals::default()
            },
            ..VolumeDetails::default()
        }
    }

    fn enumerate(root: &Path) -> Enumeration {
        let mut s = VolumeSource::open(details(root)).unwrap();
        s.enumerate(&AtomicBool::new(false), &mut |_, _| {})
            .unwrap()
    }

    fn paths(e: &Enumeration) -> Vec<String> {
        let mut v: Vec<String> = e.files.iter().map(|f| f.display_path()).collect();
        v.sort();
        v
    }

    fn put(root: &Path, rel: &str, data: &[u8]) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, data).unwrap();
    }

    #[test]
    fn lists_files_with_size_and_modified_and_is_a_files_view_of_storage() {
        let t = tempfile::tempdir().unwrap();
        put(t.path(), "DCIM/100/IMG_1.JPG", b"abc");
        put(t.path(), "top.txt", b"hello");
        let mut s = VolumeSource::open(details(t.path())).unwrap();
        assert_eq!(s.identity().view, View::Files);
        assert_eq!(s.identity().key, "volume:abcd-1234");
        assert_eq!(s.local_root(), Some(t.path()));
        let e = s
            .enumerate(&AtomicBool::new(false), &mut |_, _| {})
            .unwrap();
        assert_eq!(paths(&e), ["DCIM/100/IMG_1.JPG", "top.txt"]);
        assert!(e.issues.is_empty() && e.incomplete.is_empty());
        let f = e.files.iter().find(|f| f.object_id == "top.txt").unwrap();
        assert_eq!(f.size, Some(5));
        assert!(f.modified.is_some());
        let info = s.object_info("DCIM/100/IMG_1.JPG").unwrap();
        assert_eq!((info.name.as_str(), info.size), ("IMG_1.JPG", Some(3)));
        let mut data = Vec::new();
        s.open_read("top.txt")
            .unwrap()
            .read_to_end(&mut data)
            .unwrap();
        assert_eq!(data, b"hello");
    }

    #[test]
    fn the_id_file_is_left_out_at_the_top_only_and_fills_the_signals() {
        let t = tempfile::tempdir().unwrap();
        put(t.path(), "a.txt", b"a");
        idfile::write(t.path(), "id-1").unwrap();
        put(t.path(), "sub/.plug-n-keep-id", b"mine");
        let mut s = VolumeSource::open(details(t.path())).unwrap();
        assert_eq!(s.identity().signals.id_file_id.as_deref(), Some("id-1"));
        let e = s
            .enumerate(&AtomicBool::new(false), &mut |_, _| {})
            .unwrap();
        assert_eq!(paths(&e), ["a.txt", "sub/.plug-n-keep-id"]);
    }

    #[test]
    fn a_damaged_id_file_gives_no_id_and_is_left_out_too() {
        let t = tempfile::tempdir().unwrap();
        put(t.path(), ".plug-n-keep-id", b"junk");
        let s = VolumeSource::open(details(t.path())).unwrap();
        assert_eq!(s.identity().signals.id_file_id, None);
        assert!(matches!(s.id_file(), IdFile::Damaged(_)));
        assert!(enumerate(t.path()).files.is_empty());
    }

    #[test]
    fn a_clone_with_the_same_id_reports_the_same_identity() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        idfile::write(a.path(), "same-id").unwrap();
        fs::copy(
            a.path().join(idfile::FILE_NAME),
            b.path().join(idfile::FILE_NAME),
        )
        .unwrap();
        let mut db = details(b.path());
        db.signals.volume_serial = Some("FFFF-0000".into());
        let sa = VolumeSource::open(details(a.path())).unwrap();
        let sb = VolumeSource::open(db).unwrap();
        // The same ID file signal, but a clashing format ID: what recognition keys on.
        assert_eq!(
            sa.identity().signals.id_file_id,
            sb.identity().signals.id_file_id
        );
        assert_ne!(
            sa.identity().signals.volume_serial,
            sb.identity().signals.volume_serial
        );
    }

    #[test]
    fn links_are_not_followed_and_are_reported() {
        let t = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        put(outside.path(), "secret.txt", b"secret");
        put(t.path(), "real/ok.txt", b"ok");
        crate::testutil::make_junction(&t.path().join("link"), outside.path());
        let e = enumerate(t.path());
        assert_eq!(paths(&e), ["real/ok.txt"]);
        assert!(
            e.issues.iter().any(|i| i.location == "link"),
            "{:?}",
            e.issues
        );
        // Whatever the link hid is unknown, so nothing under it may count as deleted.
        assert!(
            e.incomplete.iter().any(|f| f.is_empty()),
            "{:?}",
            e.incomplete
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_file_link_is_an_issue_and_cannot_be_read_through() {
        let t = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        put(outside.path(), "secret.txt", b"secret");
        std::os::unix::fs::symlink(outside.path().join("secret.txt"), t.path().join("l.txt"))
            .unwrap();
        let mut s = VolumeSource::open(details(t.path())).unwrap();
        let e = s
            .enumerate(&AtomicBool::new(false), &mut |_, _| {})
            .unwrap();
        assert!(e.files.is_empty());
        assert_eq!(e.issues.len(), 1);
        assert!(s.open_read("l.txt").is_err());
        assert!(s.object_info("l.txt").is_err());
        assert!(open_read_only(&t.path().join("l.txt")).is_err());
    }

    #[test]
    fn nothing_outside_the_volume_can_be_named() {
        let t = tempfile::tempdir().unwrap();
        let vol = t.path().join("vol");
        fs::create_dir(&vol).unwrap();
        put(t.path(), "outside.txt", b"x");
        let mut s = VolumeSource::open(details(&vol)).unwrap();
        for bad in [
            "../outside.txt",
            "",
            ".",
            "a//b",
            "/etc/passwd",
            "a/../../outside.txt",
        ] {
            assert!(s.open_read(bad).is_err(), "{bad}");
            assert!(s.object_info(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_depth_limit_stops_the_walk_and_marks_the_folder_incomplete() {
        let t = tempfile::tempdir().unwrap();
        let mut rel = PathBuf::new();
        for _ in 0..MAX_DEPTH {
            rel.push("d");
        }
        let deepest_ok = t.path().join(&rel);
        fs::create_dir_all(&deepest_ok).unwrap();
        fs::write(deepest_ok.join("at-limit.txt"), b"1").unwrap();
        fs::create_dir(deepest_ok.join("too-deep")).unwrap();
        fs::write(deepest_ok.join("too-deep").join("lost.txt"), b"2").unwrap();
        let e = enumerate(t.path());
        assert_eq!(e.files.len(), 1, "{:?}", paths(&e));
        assert!(e.files[0].components.last().unwrap() == "at-limit.txt");
        assert_eq!(e.issues.len(), 1);
        assert!(e.incomplete.iter().any(|f| f.ends_with("d/too-deep")));
    }

    #[test]
    #[cfg(unix)]
    fn an_unreadable_folder_is_an_issue_and_incomplete_not_a_failure() {
        use crate::testutil::ReadOnlyDir;
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        put(t.path(), "ok/a.txt", b"a");
        put(t.path(), "locked/b.txt", b"b");
        let locked = t.path().join("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let _restore = ReadOnlyDir::adopt(&locked);
        if fs::read_dir(&locked).is_ok() {
            return; // root ignores permissions
        }
        let e = enumerate(t.path());
        assert_eq!(paths(&e), ["ok/a.txt"]);
        assert_eq!(e.issues.len(), 1);
        assert_eq!(e.issues[0].location, "locked");
        assert_eq!(e.incomplete, ["locked"]);
    }

    #[test]
    fn names_are_cleaned_of_direction_marks_but_still_open() {
        let t = tempfile::tempdir().unwrap();
        // U+200E is legal in names on every system.
        put(t.path(), "pic\u{200E}.jpg", b"x");
        let mut s = VolumeSource::open(details(t.path())).unwrap();
        let e = s
            .enumerate(&AtomicBool::new(false), &mut |_, _| {})
            .unwrap();
        assert_eq!(e.files[0].components, ["pic.jpg"]);
        let mut data = Vec::new();
        s.open_read(&e.files[0].object_id)
            .unwrap()
            .read_to_end(&mut data)
            .unwrap();
        assert_eq!(data, b"x");
    }

    #[test]
    fn a_cancelled_walk_returns_cancelled() {
        let t = tempfile::tempdir().unwrap();
        put(t.path(), "a.txt", b"a");
        let mut s = VolumeSource::open(details(t.path())).unwrap();
        let r = s.enumerate(&AtomicBool::new(true), &mut |_, _| {});
        assert_eq!(r.unwrap_err(), DeviceError::Cancelled);
    }

    #[test]
    fn a_missing_root_is_refused() {
        let t = tempfile::tempdir().unwrap();
        assert!(VolumeSource::open(details(&t.path().join("gone"))).is_err());
        put(t.path(), "f", b"x");
        assert!(VolumeSource::open(details(&t.path().join("f"))).is_err());
    }

    #[test]
    fn object_info_sees_a_change_since_the_scan() {
        let t = tempfile::tempdir().unwrap();
        put(t.path(), "a.txt", b"one");
        let mut s = VolumeSource::open(details(t.path())).unwrap();
        let before = s.object_info("a.txt").unwrap();
        let f = fs::OpenOptions::new()
            .write(true)
            .open(t.path().join("a.txt"))
            .unwrap();
        f.set_len(1).unwrap();
        f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000))
            .unwrap();
        drop(f);
        let after = s.object_info("a.txt").unwrap();
        assert_eq!(after.size, Some(1));
        assert_ne!(before.modified, after.modified);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_file_the_user_does_not_own_still_opens_when_noatime_is_refused() {
        // Owned by root and world-readable on most systems: O_NOATIME gives EPERM for
        // anyone but root, and the open must retry without it.
        let p = Path::new("/etc/hostname");
        if p.exists() {
            let mut data = Vec::new();
            open_read_only(p).unwrap().read_to_end(&mut data).unwrap();
        }
    }

    #[test]
    fn picker_label_and_sizes() {
        let mut d = details(Path::new("E:\\"));
        d.signals.capacity = Some(32_000_000_000);
        assert_eq!(d.picker_label(), "STICK (E:\\) 29.8 GB");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1_572_864), "1.5 MB");
        let l = d.listed();
        assert_eq!(l.transport, Transport::Volume);
        assert_eq!(l.kind_hint, Some(DeviceKind::Storage));
    }

    // ---------------------------------------------------------------------------------------
    // A full scan and sync never changes the volume.

    /// Bytes (files only), length, modification time and read-only flag.
    type Stat = (Option<Vec<u8>>, u64, SystemTime, bool);

    /// Every file and folder: bytes (files), length, mtime, read-only flag.
    fn snapshot(root: &Path) -> BTreeMap<String, Stat> {
        let mut out = BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for e in fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                let m = fs::symlink_metadata(&p).unwrap();
                let rel = p
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                let bytes = m.is_file().then(|| fs::read(&p).unwrap());
                out.insert(
                    rel,
                    (
                        bytes,
                        m.len(),
                        m.modified().unwrap(),
                        m.permissions().readonly(),
                    ),
                );
                if m.is_dir() {
                    stack.push(p);
                }
            }
        }
        out
    }

    #[test]
    fn a_full_scan_and_sync_never_changes_the_volume() {
        let t = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(t.path()).unwrap();
        let vol = base.join("stick");
        put(&vol, "DCIM/100/IMG_1.JPG", &vec![7u8; 100_000]);
        put(&vol, "DCIM/100/IMG_2.JPG", b"two");
        put(&vol, "docs/readme.txt", b"hello");
        put(&vol, "empty.bin", b"");
        idfile::write(&vol, "id-e2e").unwrap();
        // Old modification times, so a touched file would show.
        let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_300_000_000);
        for rel in ["DCIM/100/IMG_1.JPG", "docs/readme.txt"] {
            let f = fs::OpenOptions::new()
                .write(true)
                .open(vol.join(rel))
                .unwrap();
            f.set_modified(old).unwrap();
        }
        let before = snapshot(&vol);

        let backup = base.join("backup");
        let state = StateDir::open(base.join("state")).unwrap();
        let mut src = VolumeSource::open(details(&vol)).unwrap();
        let skip = state
            .skip_list(&Scope::new(&src.identity().key, &backup))
            .unwrap();
        let opts = ScanOptions {
            settings: DeviceSettings {
                folders: Vec::new(),
                file_types: FileTypes::All,
                always_left_out: Vec::new(),
                archive_layout: ArchiveLayout::Flat,
                ..DeviceSettings::default()
            },
            weak_match: false,
        };
        let (plan, summary) = scan(
            &mut src,
            &backup,
            &skip,
            &opts,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(summary.new, 4);
        let items: Vec<SyncItem> = plan
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e.status, Status::New | Status::Mismatch))
            .map(|(i, _)| SyncItem {
                entry: i,
                mode: CopyMode::NoReplace,
            })
            .collect();
        let mut events = Vec::new();
        let report = run_sync(
            &mut src,
            &plan,
            &items,
            &state,
            &SyncControl::default(),
            "2026-01-02 03-04-05",
            false,
            &mut |e| events.push(e),
        )
        .unwrap();
        assert_eq!(report.copied, 4);
        // A second scan finds everything backed up, and reads the volume again.
        let (_, again) = scan(
            &mut src,
            &backup,
            &skip,
            &opts,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(again.new, 0);

        assert_eq!(fs::read(backup.join("DCIM/100/IMG_2.JPG")).unwrap(), b"two");
        assert!(!backup.join(idfile::FILE_NAME).exists(), "ID file copied");
        assert_eq!(snapshot(&vol), before, "the volume changed");
    }

    /// A folder the volume lists as empty is reported, so backup files under it wait for a
    /// confirmation. The top of the volume is never one, and a folder that holds only a folder
    /// is not empty.
    #[test]
    fn empty_folders_are_reported_but_never_the_root() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir_all(t.path().join("DCIM/100")).unwrap();
        fs::create_dir_all(t.path().join("Music")).unwrap();
        put(t.path(), "Docs/a.txt", b"a");
        let mut e = enumerate(t.path());
        e.empty_folders.sort();
        assert_eq!(e.empty_folders, ["DCIM/100", "Music"]);
        assert!(e.incomplete.is_empty() && e.issues.is_empty());
        // An empty volume has no empty folder: its root is the device itself.
        let empty = tempfile::tempdir().unwrap();
        assert!(enumerate(empty.path()).empty_folders.is_empty());
    }

    /// A folder that couldn't be listed is incomplete, not empty.
    #[test]
    #[cfg(unix)]
    fn a_folder_that_cannot_be_listed_is_not_called_empty() {
        use crate::testutil::ReadOnlyDir;
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        put(t.path(), "locked/b.txt", b"b");
        let locked = t.path().join("locked");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let _restore = ReadOnlyDir::adopt(&locked);
        if fs::read_dir(&locked).is_ok() {
            return; // root ignores permissions
        }
        let e = enumerate(t.path());
        assert!(e.empty_folders.is_empty(), "{:?}", e.empty_folders);
    }

    /// The system's own folders at the top are left out without a word: not indexed, not an
    /// issue, not a reason to call the scan incomplete. The same names lower down are ordinary.
    #[test]
    fn system_folders_at_the_top_are_skipped_silently() {
        let t = tempfile::tempdir().unwrap();
        put(t.path(), "System Volume Information/x.dat", b"x");
        put(t.path(), "$RECYCLE.BIN/S-1/y", b"y");
        put(t.path(), "lost+found/z", b"z");
        put(t.path(), ".Trash-1000/files/w", b"w");
        put(t.path(), ".trash-0/v", b"v");
        put(t.path(), "DCIM/lost+found/mine.txt", b"m");
        put(t.path(), "keep.txt", b"k");
        let e = enumerate(t.path());
        assert_eq!(paths(&e), ["DCIM/lost+found/mine.txt", "keep.txt"]);
        assert!(e.issues.is_empty(), "{:?}", e.issues);
        assert!(e.incomplete.is_empty(), "{:?}", e.incomplete);
        assert!(e.empty_folders.is_empty());
    }

    /// The root must still be the volume that was opened, at every listing and every read.
    #[test]
    fn a_different_volume_at_the_same_place_is_not_read() {
        let t = tempfile::tempdir().unwrap();
        put(t.path(), "a.txt", b"a");
        let mut s = VolumeSource::open(details(t.path())).unwrap();
        assert!(s.enumerate(&AtomicBool::new(false), &mut |_, _| {}).is_ok());
        assert!(s.open_read("a.txt").is_ok());
        // Another stick takes the place: its token differs from the one recorded at open.
        s.token ^= 1;
        let e = s.enumerate(&AtomicBool::new(false), &mut |_, _| {});
        assert!(matches!(e, Err(DeviceError::Failed(_))), "{e:?}");
        assert!(s.open_read("a.txt").is_err());
    }

    #[test]
    fn the_volume_token_is_steady_for_one_volume() {
        let t = tempfile::tempdir().unwrap();
        let a = crate::platform::volume_token(t.path()).unwrap();
        put(t.path(), "sub/x", b"x");
        assert_eq!(
            a,
            crate::platform::volume_token(&t.path().join("sub")).unwrap()
        );
        #[cfg(unix)]
        assert!(crate::platform::volume_token(&t.path().join("missing")).is_err());
    }

    /// Another file system mounted inside the stick isn't entered. (A test can't mount one, so
    /// the walk is told the root is on a different file system from the folders under it.)
    #[test]
    #[cfg(unix)]
    fn a_file_system_mounted_inside_the_volume_is_not_entered() {
        use std::os::unix::fs::MetadataExt;
        let t = tempfile::tempdir().unwrap();
        put(t.path(), "top.txt", b"t");
        put(t.path(), "inner/deep.txt", b"d");
        let dev = fs::metadata(t.path()).unwrap().dev();
        let walked =
            |expect| walk(t.path(), expect, &AtomicBool::new(false), &mut |_, _| {}).unwrap();
        let same = walked(Some(dev));
        assert_eq!(paths(&same), ["inner/deep.txt", "top.txt"]);
        let other = walked(Some(dev ^ 1));
        assert_eq!(paths(&other), ["top.txt"]);
        assert!(other.issues.iter().any(|i| i.location == "inner"));
        assert_eq!(other.incomplete, ["inner"]);
    }

    /// A directory reached a second time (a bind mount, a hard-linked directory) is listed once.
    #[test]
    #[cfg(unix)]
    fn a_directory_is_listed_once_even_if_reached_twice() {
        let t = tempfile::tempdir().unwrap();
        fs::create_dir(t.path().join("a")).unwrap();
        let meta = fs::metadata(t.path().join("a")).unwrap();
        let mut seen = SeenDirs::default();
        assert!(seen.first_visit(&meta));
        assert!(!seen.first_visit(&meta));
        assert!(seen.first_visit(&fs::metadata(t.path()).unwrap()));
    }

    /// A pipe where a file was (swapped in after the walk) is refused, and never waited on.
    #[test]
    #[cfg(unix)]
    fn a_pipe_is_not_opened_as_a_file() {
        let t = tempfile::tempdir().unwrap();
        let fifo = t.path().join("pipe");
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        if !matches!(made, Ok(s) if s.success()) {
            return; // no mkfifo here
        }
        let r = open_read_only(&fifo);
        assert!(r.is_err(), "a pipe was opened");
        put(t.path(), "plain.txt", b"p");
        assert!(open_read_only(&t.path().join("plain.txt")).is_ok());
    }
}
