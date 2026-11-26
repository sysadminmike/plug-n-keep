//! A backup folder on this PC. Every operation calls the helpers the backup code used before
//! (no-replace renames, flushes, link and containment checks), so behaviour is the same.
#![forbid(unsafe_code)]

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use log::warn;
use walkdir::WalkDir;

use super::{BackupStore, Kind, ListIssue, Listed, Listing, Stat, Tag, Upload, conflict};
use crate::paths::{
    SafeRelPath, TEMP_SUFFIX, fit_name, is_link, prepare_parent, validate_component,
};
use crate::plan::MAX_NUMBERED;
use crate::platform::{self, DiskSpace};
use crate::state::{write_atomic_at, write_new_file};

#[derive(Debug, Clone)]
pub struct LocalStore {
    pub root: PathBuf,
}

/// A version tag from size and modified time (nanoseconds).
pub(crate) fn tag_of(meta: &fs::Metadata) -> Tag {
    let ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    format!("{}-{ns}", meta.len())
}

fn kind_of(meta: &fs::Metadata) -> Kind {
    if is_link(meta) {
        Kind::Link
    } else if meta.is_file() {
        Kind::File
    } else {
        Kind::Other
    }
}

fn shown(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .replace('\\', "/")
}

impl LocalStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The root with links resolved. The containment checks work from this.
    fn canon(&self) -> io::Result<PathBuf> {
        fs::canonicalize(&self.root)
    }

    /// The file's full path, with its folders made (refusing links and files in the way).
    fn prepared(&self, rel: &SafeRelPath) -> io::Result<(PathBuf, PathBuf)> {
        let root = self.canon()?;
        let dir = prepare_parent(&root, rel)?;
        let path = dir.join(rel.file_name());
        Ok((root, path))
    }

    /// A temp file name of its own, so two writers never share one.
    fn unique_temp(path: &Path) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        path.with_file_name(fit_name(
            &format!("{name}.{}-{n}", std::process::id()),
            TEMP_SUFFIX,
        ))
    }
}

impl BackupStore for LocalStore {
    fn list(&self, prune: &dyn Fn(&str) -> bool) -> io::Result<Listing> {
        let mut out = Listing::default();
        if !self.root.exists() {
            return Ok(out);
        }
        let root = &self.root;
        let mut walk = WalkDir::new(root)
            .follow_links(false)
            .min_depth(1)
            .into_iter()
            .filter_entry(|e| !(e.file_type().is_dir() && prune(&shown(root, e.path()))));
        while let Some(entry) = walk.next() {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    out.issues.push(ListIssue {
                        at: e.path().map(|p| shown(root, p)).unwrap_or_default(),
                        why: format!("could not read this in the backup folder: {e}"),
                    });
                    continue;
                }
            };
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(e) => {
                    out.issues.push(ListIssue {
                        at: shown(root, entry.path()),
                        why: format!("could not read this in the backup folder: {e}"),
                    });
                    continue;
                }
            };
            let link = is_link(&meta);
            if meta.is_dir() && !link {
                continue;
            }
            if meta.is_dir() {
                walk.skip_current_dir();
            }
            let Ok(rel_path) = entry.path().strip_prefix(root) else {
                continue;
            };
            let Some(rel) = rel_path.to_str() else {
                out.issues.push(ListIssue {
                    at: shown(root, entry.path()),
                    why: "name in the backup folder is not valid Unicode".into(),
                });
                continue;
            };
            out.files.push(Listed {
                rel: rel.replace('\\', "/"),
                size: meta.len(),
                mtime: meta.modified().ok(),
                tag: Some(tag_of(&meta)),
                kind: if link { Kind::Link } else { kind_of(&meta) },
            });
        }
        Ok(out)
    }

    fn list_dirs(&self, under: &str) -> io::Result<Vec<String>> {
        let dir = if under.is_empty() {
            self.root.clone()
        } else {
            self.root.join(under)
        };
        let mut names = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            // Links are not followed: a link could lead anywhere.
            let plain_dir = entry
                .metadata()
                .is_ok_and(|m| m.is_dir() && !crate::paths::is_link(&m));
            if plain_dir && let Some(name) = entry.file_name().to_str() {
                names.push(name.to_string());
            }
        }
        names.sort();
        Ok(names)
    }

    fn stat(&self, rel: &SafeRelPath) -> io::Result<Option<Stat>> {
        match fs::symlink_metadata(rel.under(&self.root)) {
            Ok(m) => Ok(Some(Stat {
                size: m.len(),
                mtime: m.modified().ok(),
                tag: Some(tag_of(&m)),
                kind: kind_of(&m),
            })),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn read(&self, rel: &SafeRelPath) -> io::Result<Option<(Vec<u8>, Tag)>> {
        let path = rel.under(&self.root);
        let meta = match fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if is_link(&meta) {
            return Err(io::Error::other(format!(
                "{} is a link or junction; refusing to read through it",
                path.display()
            )));
        }
        let bytes = fs::read(&path)?;
        Ok(Some((bytes, tag_of(&meta))))
    }

    fn write_new(&self, rel: &SafeRelPath, bytes: &[u8]) -> io::Result<Tag> {
        let (_, path) = self.prepared(rel)?;
        let temp = Self::unique_temp(&path);
        let done = (|| {
            write_new_file(&temp, bytes)?;
            platform::rename_no_replace(&temp, &path)
        })();
        if let Err(e) = done {
            let _ = fs::remove_file(&temp);
            return Err(e);
        }
        Ok(tag_of(&fs::symlink_metadata(&path)?))
    }

    fn write_replace(
        &self,
        rel: &SafeRelPath,
        bytes: &[u8],
        expect: Option<&Tag>,
    ) -> io::Result<Tag> {
        let (_, path) = self.prepared(rel)?;
        if let Some(expect) = expect {
            match fs::symlink_metadata(&path) {
                Ok(m) if !is_link(&m) && m.is_file() && &tag_of(&m) == expect => {}
                Ok(m) if is_link(&m) || !m.is_file() => {
                    return Err(io::Error::other(format!(
                        "{} is not a plain file; refusing to replace it",
                        path.display()
                    )));
                }
                Ok(_) => return Err(conflict()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(conflict()),
                Err(e) => return Err(e),
            }
        }
        write_atomic_at(&path, bytes)?;
        Ok(tag_of(&fs::symlink_metadata(&path)?))
    }

    fn begin_upload(
        &self,
        rel: &SafeRelPath,
        size: u64,
        mtime: Option<SystemTime>,
    ) -> io::Result<Box<dyn Upload>> {
        let (_, final_path) = self.prepared(rel)?;
        let temp_name = fit_name(rel.file_name(), TEMP_SUFFIX);
        let temp = final_path.with_file_name(&temp_name);
        clear_stale_temp(&temp)?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        let journal = journal_of(rel);
        Ok(Box::new(LocalUpload {
            file: Some(file),
            temp,
            final_path,
            mtime,
            size,
            written: 0,
            journal,
            sealed: false,
        }))
    }

    fn planned_ref(&self, rel: &SafeRelPath) -> Option<String> {
        Some(journal_of(rel))
    }

    fn abort_by_ref(&self, journal_ref: &str) -> io::Result<()> {
        let bad = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{journal_ref:?} is not one of this app's temporary files"),
            )
        };
        let parts: Vec<&str> = journal_ref.split('/').collect();
        let (last, dirs) = parts.split_last().ok_or_else(bad)?;
        // Only the upload temp (`.pnk-part`, any case); the JPG step's own temp is not an upload.
        if !last.to_ascii_lowercase().ends_with(TEMP_SUFFIX) {
            return Err(bad());
        }
        // The folders must be valid names; the temp name is its own file name plus the suffix.
        let stem = &last[..last.len() - TEMP_SUFFIX.len()];
        if stem.is_empty() || validate_component(&format!("{stem}_")).is_err() {
            return Err(bad());
        }
        if dirs.iter().any(|d| validate_component(d).is_err()) {
            return Err(bad());
        }
        // No link or junction in any folder between the root and the temp.
        let mut path = self.root.clone();
        for d in dirs {
            path.push(d);
            match fs::symlink_metadata(&path) {
                Ok(m) if m.is_dir() && !is_link(&m) => {}
                Ok(_) => return Err(bad()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e),
            }
        }
        path.push(last);
        match fs::symlink_metadata(&path) {
            Ok(m) if m.is_file() && !is_link(&m) => fs::remove_file(&path),
            Ok(_) => Err(bad()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn move_no_replace(&self, from: &SafeRelPath, to: &SafeRelPath) -> io::Result<()> {
        let root = self.canon()?;
        let dir = prepare_parent(&root, to)?;
        platform::rename_no_replace(&from.under(&root), &dir.join(to.file_name()))
    }

    fn send_to_bin(&self, rel: &SafeRelPath) -> io::Result<()> {
        let root = self.canon()?;
        let path = rel.under(&root);
        let meta = fs::symlink_metadata(&path)?;
        if is_link(&meta) || !meta.is_file() {
            return Err(io::Error::other(format!(
                "{} is not a plain file; not sent to the {}",
                path.display(),
                platform::BIN_NAME
            )));
        }
        platform::send_to_bin(&path).map_err(io::Error::other)
    }

    fn supports_bin(&self) -> bool {
        true
    }

    fn space(&self) -> Option<DiskSpace> {
        platform::disk_space(&self.root)
    }

    fn root_exists(&self) -> io::Result<bool> {
        Ok(self.root.exists())
    }

    fn ensure_root(&self) -> io::Result<()> {
        fs::create_dir_all(&self.root)
    }

    fn describe(&self) -> String {
        self.root.display().to_string()
    }

    fn as_local(&self) -> Option<&LocalStore> {
        Some(self)
    }
}

/// An unfinished temp already at the name is moved aside (to ` (kept n)`), never overwritten or
/// deleted.
fn clear_stale_temp(temp: &Path) -> io::Result<()> {
    match fs::symlink_metadata(temp) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(m) if m.is_file() && !is_link(&m) => {
            let stem = temp
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(TEMP_SUFFIX))
                .unwrap_or("copy")
                .to_string();
            for n in 1..=MAX_NUMBERED {
                let aside =
                    temp.with_file_name(fit_name(&stem, &format!(" (kept {n}){TEMP_SUFFIX}")));
                match platform::rename_no_replace(temp, &aside) {
                    Ok(()) => {
                        warn!(
                            "An unfinished copy was already at {}; it was moved aside to {}",
                            temp.display(),
                            aside.display()
                        );
                        return Ok(());
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => return Err(e),
                }
            }
            Err(io::Error::other(format!(
                "too many unfinished copies of {stem} are already kept next to it"
            )))
        }
        Ok(_) => Err(io::Error::other(format!(
            "{} is in the way and is not a plain file",
            temp.display()
        ))),
    }
}

/// The temp's path relative to the root, as the crash record keeps it.
fn journal_of(rel: &SafeRelPath) -> String {
    let temp_name = fit_name(rel.file_name(), TEMP_SUFFIX);
    match rel.as_str().rsplit_once('/') {
        Some((dir, _)) => format!("{dir}/{temp_name}"),
        None => temp_name,
    }
}

struct LocalUpload {
    file: Option<File>,
    temp: PathBuf,
    final_path: PathBuf,
    mtime: Option<SystemTime>,
    size: u64,
    written: u64,
    journal: String,
    sealed: bool,
}

impl LocalUpload {
    fn remove_temp(&mut self) {
        self.file = None;
        let _ = fs::remove_file(&self.temp);
    }
}

impl Upload for LocalUpload {
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("the upload is finished"))?;
        file.write_all(data)?;
        self.written += data.len() as u64;
        Ok(())
    }

    fn seal(&mut self) -> io::Result<()> {
        if self.sealed {
            return Ok(());
        }
        if self.written != self.size {
            let msg = format!(
                "{} bytes were written for a {}-byte file",
                self.written, self.size
            );
            self.remove_temp();
            return Err(io::Error::new(io::ErrorKind::InvalidData, msg));
        }
        let Some(file) = self.file.take() else {
            return Err(io::Error::other("the upload is finished"));
        };
        // Not fatal if it can't be set: the data is whole.
        if let Some(when) = self.mtime
            && let Err(e) = platform::set_modified(&file, when)
        {
            warn!(
                "Could not set the modified date of {}: {e}",
                self.final_path.display()
            );
        }
        // On disk before it can get its real name; closed too (Windows won't rename an open file).
        let flushed = platform::flush_file(&file);
        drop(file);
        if let Err(e) = flushed {
            self.remove_temp();
            return Err(e);
        }
        self.sealed = true;
        Ok(())
    }

    fn finish_no_replace(mut self: Box<Self>) -> io::Result<Tag> {
        self.seal()?;
        match platform::rename_no_replace(&self.temp, &self.final_path) {
            Ok(()) => Ok(tag_of(&fs::symlink_metadata(&self.final_path)?)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                self.remove_temp();
                Err(e)
            }
            // Anything else keeps the finished temp, as the sync code does.
            Err(e) => Err(e),
        }
    }

    fn abort(mut self: Box<Self>) -> io::Result<()> {
        self.file = None;
        match fs::remove_file(&self.temp) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    fn journal_ref(&self) -> String {
        self.journal.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{is_conflict, tests};
    use std::time::Duration;

    fn at_ms(ms: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(ms)
    }

    fn rel(s: &str) -> SafeRelPath {
        SafeRelPath::parse(s).unwrap()
    }

    fn store() -> (tempfile::TempDir, LocalStore) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("backup");
        fs::create_dir(&root).unwrap();
        (tmp, LocalStore::new(root))
    }

    #[test]
    fn shared_rules_hold() {
        tests::run_all(|| {
            let (tmp, s) = store();
            (Box::new(s), Box::new(tmp))
        });
    }

    #[test]
    fn upload_leaves_a_temp_named_as_the_sync_does() {
        let (_t, s) = store();
        let mut up = s
            .begin_upload(&rel("DCIM/100APPLE/IMG_1.HEIC"), 2, None)
            .unwrap();
        assert_eq!(up.journal_ref(), "DCIM/100APPLE/IMG_1.HEIC.pnk-part");
        up.write(b"ab").unwrap();
        assert!(
            s.root
                .join("DCIM")
                .join("100APPLE")
                .join("IMG_1.HEIC.pnk-part")
                .is_file()
        );
        up.finish_no_replace().unwrap();
        assert!(!s.root.join("DCIM/100APPLE/IMG_1.HEIC.pnk-part").exists());
    }

    #[test]
    fn long_names_are_fitted_for_the_temp() {
        let (_t, s) = store();
        let name = format!("{}.HEIC", "a".repeat(250));
        let up = s.begin_upload(&rel(&name), 0, None).unwrap();
        let j = up.journal_ref();
        assert_eq!(j, fit_name(&name, TEMP_SUFFIX));
        assert!(s.root.join(&j).is_file());
        up.abort().unwrap();
        assert!(!s.root.join(&j).exists());
    }

    #[test]
    fn a_stale_temp_is_moved_aside_not_overwritten() {
        let (_t, s) = store();
        fs::write(s.root.join("a.jpg.pnk-part"), b"old").unwrap();
        let up = s.begin_upload(&rel("a.jpg"), 0, None).unwrap();
        assert_eq!(
            fs::read(s.root.join("a.jpg (kept 1).pnk-part")).unwrap(),
            b"old"
        );
        up.abort().unwrap();
    }

    #[test]
    fn abort_by_ref_removes_only_our_temp() {
        let (_t, s) = store();
        fs::write(s.root.join("x.jpg.pnk-part"), b"half").unwrap();
        fs::write(s.root.join("keep.jpg"), b"data").unwrap();
        s.abort_by_ref("x.jpg.pnk-part").unwrap();
        assert!(!s.root.join("x.jpg.pnk-part").exists());
        s.abort_by_ref("x.jpg.pnk-part").unwrap();
        assert!(s.abort_by_ref("keep.jpg").is_err());
        assert!(s.abort_by_ref("../x.pnk-part").is_err());
        assert!(s.abort_by_ref("a/../../x.pnk-part").is_err());
        assert!(s.root.join("keep.jpg").exists());
        // The JPG step's own temp is not an upload.
        fs::write(s.root.join("y.jpg.pnk-jpg-part"), b"j").unwrap();
        assert!(s.abort_by_ref("y.jpg.pnk-jpg-part").is_err());
        assert!(s.root.join("y.jpg.pnk-jpg-part").exists());
    }

    #[test]
    fn abort_by_ref_refuses_a_link_in_a_parent_folder() {
        let (tmp, s) = store();
        let outside = tmp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("x.jpg.pnk-part"), b"not ours").unwrap();
        crate::testutil::make_junction(&s.root.join("linked"), &outside);
        assert!(s.abort_by_ref("linked/x.jpg.pnk-part").is_err());
        assert!(outside.join("x.jpg.pnk-part").exists());
        // A folder that isn't there has nothing to abort.
        s.abort_by_ref("nope/x.jpg.pnk-part").unwrap();
    }

    #[test]
    fn planned_ref_is_the_ref_of_the_upload() {
        let (_t, s) = store();
        fs::create_dir(s.root.join("d")).unwrap();
        for name in ["a.jpg", "d/b.heic"] {
            let planned = s.planned_ref(&rel(name)).unwrap();
            // A stale temp moved aside first doesn't change it.
            fs::write(s.root.join(&planned), b"stale").unwrap();
            let up = s.begin_upload(&rel(name), 0, None).unwrap();
            assert_eq!(up.journal_ref(), planned);
            up.abort().unwrap();
        }
    }

    #[test]
    fn a_sealed_upload_is_whole_and_still_invisible() {
        let (_t, s) = store();
        let mut up = s
            .begin_upload(&rel("a.txt"), 2, Some(at_ms(5_000)))
            .unwrap();
        up.write(b"x").unwrap();
        assert_eq!(up.seal().unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert!(!s.root.join(up.journal_ref()).exists());
        let mut up = s
            .begin_upload(&rel("a.txt"), 2, Some(at_ms(5_000)))
            .unwrap();
        up.write(b"xy").unwrap();
        up.seal().unwrap();
        up.seal().unwrap();
        assert!(s.stat(&rel("a.txt")).unwrap().is_none());
        assert!(up.write(b"z").is_err());
        let j = up.journal_ref();
        up.finish_no_replace().unwrap();
        assert!(!s.root.join(j).exists());
        assert_eq!(s.read(&rel("a.txt")).unwrap().unwrap().0, b"xy");
    }

    #[test]
    fn mtime_round_trips_through_an_upload() {
        let (_t, s) = store();
        let when = at_ms(1_700_000_123_000);
        let mut up = s.begin_upload(&rel("a.txt"), 1, Some(when)).unwrap();
        up.write(b"x").unwrap();
        up.finish_no_replace().unwrap();
        let st = s.stat(&rel("a.txt")).unwrap().unwrap();
        assert_eq!(st.mtime, Some(when));
    }

    #[test]
    fn tag_follows_content_changes() {
        let (_t, s) = store();
        let t1 = s.write_new(&rel("s.json"), b"one").unwrap();
        let t2 = s
            .write_replace(&rel("s.json"), b"three", Some(&t1))
            .unwrap();
        assert_ne!(t1, t2);
        let err = s
            .write_replace(&rel("s.json"), b"x", Some(&t1))
            .unwrap_err();
        assert!(is_conflict(&err));
    }

    #[test]
    fn links_are_reported_and_not_followed() {
        let (tmp, s) = store();
        let outside = tmp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret.txt"), b"s").unwrap();
        crate::testutil::make_junction(&s.root.join("linked"), &outside);
        fs::write(s.root.join("plain.txt"), b"p").unwrap();
        let l = s.list(&|_| false).unwrap();
        let mut got: Vec<(String, Kind)> =
            l.files.iter().map(|f| (f.rel.clone(), f.kind)).collect();
        got.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            got,
            vec![
                ("linked".to_string(), Kind::Link),
                ("plain.txt".to_string(), Kind::File)
            ]
        );
        assert_eq!(s.stat(&rel("linked")).unwrap().unwrap().kind, Kind::Link);
        assert!(s.read(&rel("linked")).is_err());
        assert!(s.write_new(&rel("linked/new.txt"), b"x").is_err());
        assert!(!outside.join("new.txt").exists());
    }

    #[test]
    fn move_makes_folders_and_never_replaces() {
        let (_t, s) = store();
        s.write_new(&rel("a/x.jpg"), b"1").unwrap();
        s.write_new(&rel("b.jpg"), b"2").unwrap();
        s.move_no_replace(&rel("a/x.jpg"), &rel("_Archived/s/a/x.jpg"))
            .unwrap();
        assert!(!s.root.join("a/x.jpg").exists());
        let e = s.move_no_replace(&rel("b.jpg"), &rel("_Archived/s/a/x.jpg"));
        assert_eq!(e.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert!(s.root.join("b.jpg").exists());
    }

    #[test]
    fn bin_goes_through_the_platform_bin() {
        let (tmp, s) = store();
        let _g = platform::fake_bin::set(platform::fake_bin::Fake::Into(tmp.path().join("bin")));
        s.write_new(&rel("a.jpg"), b"1").unwrap();
        s.send_to_bin(&rel("a.jpg")).unwrap();
        assert!(!s.root.join("a.jpg").exists());
        assert!(tmp.path().join("bin/a.jpg").exists());
        assert!(s.supports_bin());
    }

    #[test]
    fn a_missing_root_lists_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let s = LocalStore::new(tmp.path().join("nope"));
        assert!(!s.root_exists().unwrap());
        assert!(s.list(&|_| false).unwrap().files.is_empty());
        s.ensure_root().unwrap();
        assert!(s.root_exists().unwrap());
    }
}
