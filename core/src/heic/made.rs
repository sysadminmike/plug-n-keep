//! The list of JPG copies the app made, `.plug-n-keep-made.json` at the top of the backup folder
//! (DESIGN.md section 7.6). It is how the app tells its own JPGs from the device's.
//!
//! An entry is believed only while the JPG is what the app wrote: it must be a JPG name that is
//! the one the HEIC's name gives, and the file must still have the size and modified time stored
//! with the entry ([`MadeList::load`]). A JPG the user has edited, or one the device has put in
//! its place, is not the app's any more, so it is never moved or hidden along with a HEIC.
//!
//! A damaged file is set aside (as `.plug-n-keep-made.json.damaged`, `.damaged2`, ...), never
//! overwritten, and the app starts with an empty list. Saving is atomic.
#![forbid(unsafe_code)]

use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use log::{debug, warn};
use serde::{Deserialize, Serialize};

use crate::paths::SafeRelPath;
use crate::store::{BackupStore, Kind, LocalStore};

/// The file's name. It starts with a dot and is kept out of every scan, like the owner marker
/// ([`crate::owner::is_marker_name`]).
pub const MADE_NAME: &str = ".plug-n-keep-made.json";

fn made_rel() -> SafeRelPath {
    SafeRelPath::parse(MADE_NAME).expect("the made-list name is a safe name")
}

/// One JPG the app made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MadeEntry {
    /// The JPG's path relative to the backup folder, with `/` between the parts.
    pub jpg: String,
    /// Its HEIC's path, in the same form.
    pub heic: String,
    /// The HEIC's size in bytes when the JPG was made.
    pub heic_size: u64,
    /// The HEIC's modified time when the JPG was made, in milliseconds since 1970 (UTC).
    pub heic_modified_ms: i64,
    /// When the JPG was made, e.g. `2026-11-17T18:00:00Z`.
    pub made: String,
    /// The JPG's own size in bytes as the app wrote it. Missing in a list from an earlier
    /// version, and in the provisional entry saved just before a JPG is written; such an entry
    /// is believed only while the JPG's modified time is the HEIC's (the app gives it that), and
    /// then gets the facts filled in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jpg_size: Option<u64>,
    /// The JPG's own modified time as the app wrote it, in milliseconds since 1970 (UTC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jpg_modified_ms: Option<i64>,
}

/// A HEIC no JPG could be made from, and why. It is not tried again until the HEIC changes (its
/// size or modified time differ from these) or the user asks ("Try again").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailedEntry {
    /// The HEIC's path relative to the backup folder, with `/` between the parts.
    pub heic: String,
    /// Plain words for the file list.
    pub reason: String,
    /// The HEIC's size in bytes when it failed.
    pub heic_size: u64,
    /// The HEIC's modified time when it failed, in milliseconds since 1970 (UTC).
    pub heic_modified_ms: i64,
    /// When it failed, e.g. `2026-11-17T18:00:00Z`.
    #[serde(default)]
    pub when: String,
    /// The failure is only that the JPG's name is taken by another file. That is not sticky: it
    /// is looked at again at every scan, so once the other file has gone (archived as deleted,
    /// say) the JPG is made without "Try again". A decode failure stays until the HEIC changes.
    #[serde(default)]
    pub name_taken: bool,
}

/// A file's size and modified time as an entry stores them. `None` if it isn't there or isn't a
/// plain file.
pub fn facts_of(path: &Path) -> Option<(u64, i64)> {
    let m = fs::symlink_metadata(path).ok()?;
    if !m.is_file() || crate::paths::is_link(&m) {
        return None;
    }
    Some((m.len(), to_ms(m.modified().ok()?)))
}

/// [`facts_of`] for the file `rel` in a store.
pub fn facts_in(store: &dyn BackupStore, rel: &SafeRelPath) -> Option<(u64, i64)> {
    let s = store.stat(rel).ok()??;
    if s.kind != Kind::File {
        return None;
    }
    Some((s.size, to_ms(s.mtime?)))
}

/// Whether the entry is still true of the file in the store; fills in the facts of one that has
/// none yet ([`MadeList::vetted`]).
fn vet_entry(store: &dyn BackupStore, e: &mut MadeEntry) -> bool {
    use crate::plan::same_modified;
    let named_right = SafeRelPath::parse(&e.heic)
        .ok()
        .and_then(|h| crate::jpgs::jpg_path_for(&h))
        .is_some_and(|j| j.as_str() == e.jpg && crate::heic::is_jpeg_name(j.file_name()));
    if !named_right {
        warn!(
            "{MADE_NAME}: {} is not the JPG copy name of {}; not trusted",
            e.jpg, e.heic
        );
        return false;
    }
    let Ok(jpg) = SafeRelPath::parse(&e.jpg) else {
        return false;
    };
    let Some((size, modified)) = facts_in(store, &jpg) else {
        return false; // gone, or not a plain file
    };
    match (e.jpg_size, e.jpg_modified_ms) {
        (Some(s), Some(m)) => s == size && same_modified(from_ms(m), from_ms(modified)),
        _ => {
            // No facts: the app gives its JPG the HEIC's modified time, so that is the test.
            let ok = same_modified(from_ms(e.heic_modified_ms), from_ms(modified));
            if ok {
                e.jpg_size = Some(size);
                e.jpg_modified_ms = Some(modified);
            }
            ok
        }
    }
}

/// Whether the HEIC at `heic` may still be in the store: it is, or it can't be told.
fn heic_may_exist(store: &dyn BackupStore, heic: &str) -> bool {
    let Ok(p) = SafeRelPath::parse(heic) else {
        return false;
    };
    match store.stat(&p) {
        Err(_) => true,
        Ok(found) => found.is_some(),
    }
}

/// A time as the record stores it.
pub fn to_ms(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
        Err(e) => i64::try_from(e.duration().as_millis()).map_or(i64::MIN, |m| -m),
    }
}

/// The time a record's milliseconds stand for.
pub fn from_ms(ms: i64) -> SystemTime {
    let d = Duration::from_millis(ms.unsigned_abs());
    if ms >= 0 {
        UNIX_EPOCH + d
    } else {
        UNIX_EPOCH - d
    }
}

#[derive(Serialize, Deserialize)]
struct FileForm {
    version: u32,
    #[serde(default)]
    made: Vec<MadeEntry>,
    /// HEICs that couldn't be converted. A file without this (from an earlier version) loads
    /// with none.
    #[serde(default)]
    failed: Vec<FailedEntry>,
}

/// The made-JPG list of one backup folder.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MadeList {
    entries: Vec<MadeEntry>,
    failed: Vec<FailedEntry>,
    /// Set when the file existed but couldn't be read and couldn't be moved aside: the list
    /// is then not saved, so the file is not lost.
    pub save_blocked: Option<String>,
}

impl MadeList {
    /// Reads the list in `root`. A missing file is an empty list. A damaged or unreadable one
    /// is moved aside and the list starts empty.
    pub fn load(root: &Path) -> MadeList {
        Self::load_in(&LocalStore::new(root))
    }

    /// [`MadeList::load`] on a store.
    pub fn load_in(store: &dyn BackupStore) -> MadeList {
        let bytes = match store.read(&made_rel()) {
            Ok(Some((b, _))) => b,
            Ok(None) => return MadeList::default(),
            Err(e) => return Self::set_aside(store, &format!("it can't be read ({e})")),
        };
        match serde_json::from_slice::<FileForm>(&bytes) {
            Ok(f) => MadeList {
                entries: f.made,
                failed: f.failed,
                save_blocked: None,
            }
            .vetted(store),
            Err(e) => Self::set_aside(store, &format!("it is damaged ({e})")),
        }
    }

    /// Reads the list for a scan, which changes nothing: a damaged or unreadable file gives an
    /// empty list and is left where it is (a sync moves it aside).
    pub fn load_readonly(root: &Path) -> MadeList {
        Self::load_readonly_in(&LocalStore::new(root))
    }

    /// [`MadeList::load_readonly`] on a store.
    pub fn load_readonly_in(store: &dyn BackupStore) -> MadeList {
        match store
            .read(&made_rel())
            .map(|r| r.map(|(b, _)| serde_json::from_slice::<FileForm>(&b)))
        {
            Ok(Some(Ok(f))) => MadeList {
                entries: f.made,
                failed: f.failed,
                save_blocked: None,
            }
            .vetted(store),
            Ok(Some(Err(e))) => {
                warn!("{MADE_NAME} is damaged ({e}); a sync will set it aside");
                MadeList::default()
            }
            Ok(None) => MadeList::default(),
            Err(e) => {
                warn!("{MADE_NAME} can't be read ({e})");
                MadeList::default()
            }
        }
    }

    /// Keeps only what is still true of the files in `root` (see the module's notes):
    /// - an entry whose JPG name isn't a JPEG's, or isn't the name its HEIC gives, is dropped;
    /// - so is one whose JPG is gone, isn't a plain file, or no longer has the size and modified
    ///   time stored with the entry. An entry without those facts (an old list, or one saved
    ///   just before the JPG was written) stays only if the JPG's modified time is its HEIC's,
    ///   and then has the facts filled in, so the next save upgrades it;
    /// - a failure whose HEIC is gone (archived, say) is dropped.
    fn vetted(mut self, store: &dyn BackupStore) -> MadeList {
        let before = (self.entries.len(), self.failed.len());
        self.entries.retain_mut(|e| vet_entry(store, e));
        self.failed.retain(|f| heic_may_exist(store, &f.heic));
        let dropped = (before.0 - self.entries.len(), before.1 - self.failed.len());
        if dropped != (0, 0) {
            debug!(
                "{MADE_NAME}: {} and {} no longer true, left out",
                crate::counted(dropped.0, "JPG entry", "JPG entries"),
                crate::counted(dropped.1, "failure", "failures"),
            );
        }
        self
    }

    fn set_aside(store: &dyn BackupStore, why: &str) -> MadeList {
        let path = made_rel();
        let mut n = 1;
        let moved = loop {
            let name = if n == 1 {
                format!("{MADE_NAME}.damaged")
            } else {
                format!("{MADE_NAME}.damaged{n}")
            };
            let aside = SafeRelPath::parse(&name).expect("a made-list name is a safe name");
            match store.move_no_replace(&path, &aside) {
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => n += 1,
                other => break other.map(|()| name),
            }
        };
        match moved {
            Ok(name) => {
                warn!(
                    "{MADE_NAME} was not used: {why}. It is kept as {name} in {}; starting with an empty list",
                    store.describe()
                );
                MadeList::default()
            }
            Err(e) => {
                let why = format!(
                    "{MADE_NAME} was not used ({why}) and could not be moved aside ({e});                      it is left as it is and the list is not saved"
                );
                warn!("{why}");
                MadeList {
                    save_blocked: Some(why),
                    ..MadeList::default()
                }
            }
        }
    }

    /// Writes the list to `root` in one step, so a crash never leaves half of it.
    pub fn save(&self, root: &Path) -> io::Result<()> {
        self.save_in(&LocalStore::new(root))
    }

    /// [`MadeList::save`] on a store.
    pub fn save_in(&self, store: &dyn BackupStore) -> io::Result<()> {
        if let Some(why) = &self.save_blocked {
            return Err(io::Error::other(why.clone()));
        }
        let form = FileForm {
            version: 1,
            made: self.entries.clone(),
            failed: self.failed.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&form).map_err(io::Error::other)?;
        store.write_replace(&made_rel(), &bytes, None).map(|_| ())
    }

    pub fn entries(&self) -> &[MadeEntry] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The entry for the JPG at `jpg` (relative path), compared exactly: a JPG is moved or hidden
    /// from the "deleted" list on the strength of this, and on Linux `a.jpg` and `A.JPG` are two
    /// files. (A path that differs only in case is at worst taken for the device's, which is the
    /// safe way round.)
    pub fn by_jpg(&self, jpg: &str) -> Option<&MadeEntry> {
        self.entries.iter().find(|e| e.jpg == jpg)
    }

    /// The entry made from the HEIC at `heic` (relative path), compared exactly.
    pub fn by_heic(&self, heic: &str) -> Option<&MadeEntry> {
        self.entries.iter().find(|e| e.heic == heic)
    }

    /// Whether the app made the JPG at `jpg`.
    pub fn made_by_us(&self, jpg: &str) -> bool {
        self.by_jpg(jpg).is_some()
    }

    /// Adds an entry, replacing any for the same JPG.
    pub fn insert(&mut self, entry: MadeEntry) {
        self.remove_jpg(&entry.jpg);
        self.entries.push(entry);
    }

    /// The HEICs no JPG could be made from, in the order they failed.
    pub fn failed(&self) -> &[FailedEntry] {
        &self.failed
    }

    /// The failure recorded for the HEIC at `heic` (relative path), compared exactly.
    pub fn failed_for(&self, heic: &str) -> Option<&FailedEntry> {
        self.failed.iter().find(|e| e.heic == heic)
    }

    /// Records a failure, replacing any earlier one for the same HEIC.
    pub fn set_failed(&mut self, entry: FailedEntry) {
        self.clear_failed(&entry.heic);
        self.failed.push(entry);
    }

    /// Forgets the failure of the HEIC at `heic`, so it is tried again. Returns whether there was
    /// one.
    pub fn clear_failed(&mut self, heic: &str) -> bool {
        let before = self.failed.len();
        self.failed.retain(|e| e.heic != heic);
        self.failed.len() != before
    }

    /// Forgets every failure ("Try again" for all). Returns how many there were.
    pub fn clear_all_failed(&mut self) -> usize {
        std::mem::take(&mut self.failed).len()
    }

    /// Forgets the JPG at `jpg` (its file is not touched). Returns the entry if there was one.
    pub fn remove_jpg(&mut self, jpg: &str) -> Option<MadeEntry> {
        let i = self.entries.iter().position(|e| e.jpg == jpg)?;
        Some(self.entries.remove(i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;

    /// A modified time well after 1970, in milliseconds.
    const T: i64 = 1_700_000_000_000;

    /// An entry as an earlier version wrote it: no facts of the JPG. The JPG's own modified time
    /// is taken to be the HEIC's.
    fn entry(jpg: &str, heic: &str) -> MadeEntry {
        MadeEntry {
            jpg: jpg.into(),
            heic: heic.into(),
            heic_size: 1234,
            heic_modified_ms: T,
            made: "2026-11-17T18:00:00Z".into(),
            jpg_size: None,
            jpg_modified_ms: None,
        }
    }

    /// Writes `bytes` to `rel` under `root` with the modified time `ms`.
    fn put(root: &Path, rel: &str, bytes: &[u8], ms: i64) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, bytes).unwrap();
        let f = OpenOptions::new().write(true).open(&p).unwrap();
        crate::platform::set_modified(&f, from_ms(ms)).unwrap();
    }

    /// The entry the app writes for the JPG at `rel` as it is now.
    fn with_facts(root: &Path, rel: &str, heic: &str) -> MadeEntry {
        let (size, ms) = facts_of(&root.join(rel)).unwrap();
        MadeEntry {
            jpg_size: Some(size),
            jpg_modified_ms: Some(ms),
            ..entry(rel, heic)
        }
    }

    fn saved(root: &Path, entries: &[MadeEntry]) {
        let mut l = MadeList::default();
        for e in entries {
            l.insert(e.clone());
        }
        l.save(root).unwrap();
    }

    #[test]
    fn a_missing_file_is_an_empty_list() {
        let t = tempfile::tempdir().unwrap();
        let l = MadeList::load(t.path());
        assert!(l.is_empty() && l.save_blocked.is_none());
    }

    #[test]
    fn saves_and_loads_back() {
        let t = tempfile::tempdir().unwrap();
        put(t.path(), "DCIM/100APPLE/IMG_1.JPG", b"one", T + 5);
        put(t.path(), "DCIM/IMG_2.jpg", b"two!", T + 9);
        let mut l = MadeList::default();
        l.insert(with_facts(
            t.path(),
            "DCIM/100APPLE/IMG_1.JPG",
            "DCIM/100APPLE/IMG_1.HEIC",
        ));
        l.insert(with_facts(t.path(), "DCIM/IMG_2.jpg", "DCIM/IMG_2.heic"));
        l.save(t.path()).unwrap();
        assert!(!t.path().join(format!("{MADE_NAME}.tmp")).exists());
        let back = MadeList::load(t.path());
        assert_eq!(back, l);
        assert_eq!(back.len(), 2);
        assert_eq!(back.by_jpg("DCIM/IMG_2.jpg").unwrap().jpg_size, Some(4));
    }

    #[test]
    fn lookups_are_exact_and_inserting_replaces() {
        let mut l = MadeList::default();
        l.insert(entry("A/IMG_1.JPG", "A/IMG_1.HEIC"));
        assert!(l.made_by_us("A/IMG_1.JPG"));
        assert!(l.by_heic("A/IMG_1.HEIC").is_some());
        // On Linux these are other files: they are not the app's.
        assert!(!l.made_by_us("a/img_1.jpg"));
        assert!(!l.made_by_us("A/IMG_1.jpg"));
        assert!(l.by_heic("A/img_1.heic").is_none());
        assert!(!l.made_by_us("A/IMG_2.JPG"));
        let mut e = entry("A/IMG_1.JPG", "A/IMG_1.HEIC");
        e.heic_size = 99;
        l.insert(e);
        assert_eq!(l.len(), 1);
        assert_eq!(l.by_jpg("A/IMG_1.JPG").unwrap().heic_size, 99);
        assert!(l.remove_jpg("a/img_1.jpg").is_none());
        assert!(l.remove_jpg("A/IMG_1.JPG").is_some());
        assert!(l.is_empty());
    }

    #[test]
    fn a_damaged_file_is_set_aside_not_fatal() {
        let t = tempfile::tempdir().unwrap();
        fs::write(t.path().join(MADE_NAME), b"{ not json").unwrap();
        let l = MadeList::load(t.path());
        assert!(l.is_empty() && l.save_blocked.is_none());
        assert!(!t.path().join(MADE_NAME).exists());
        assert_eq!(
            fs::read(t.path().join(format!("{MADE_NAME}.damaged"))).unwrap(),
            b"{ not json"
        );
        // A second damaged file is set aside under another name, not over the first.
        fs::write(t.path().join(MADE_NAME), b"also bad").unwrap();
        let _ = MadeList::load(t.path());
        assert!(t.path().join(format!("{MADE_NAME}.damaged2")).exists());
        // The list can be saved again afterwards.
        put(t.path(), "a.JPG", b"x", T);
        let mut l = MadeList::default();
        l.insert(entry("a.JPG", "a.HEIC"));
        l.save(t.path()).unwrap();
        assert_eq!(MadeList::load(t.path()).len(), 1);
    }

    #[test]
    fn a_blocked_list_is_never_saved() {
        let t = tempfile::tempdir().unwrap();
        let l = MadeList {
            save_blocked: Some("because".into()),
            ..MadeList::default()
        };
        assert!(l.save(t.path()).is_err());
        assert!(!t.path().join(MADE_NAME).exists());
    }

    #[test]
    fn failures_are_kept_replaced_cleared_and_saved() {
        let t = tempfile::tempdir().unwrap();
        for h in ["A/IMG_1.HEIC", "A/IMG_2.HEIC", "A/img_1.heic"] {
            put(t.path(), h, b"h", T);
        }
        let fail = |heic: &str, why: &str| FailedEntry {
            heic: heic.into(),
            reason: why.into(),
            heic_size: 5,
            heic_modified_ms: 7,
            when: "2026-11-17T18:00:00Z".into(),
            name_taken: false,
        };
        let mut l = MadeList::default();
        l.set_failed(fail("A/IMG_1.HEIC", "broken"));
        l.set_failed(fail("A/IMG_2.HEIC", "also broken"));
        l.set_failed(fail("A/IMG_1.HEIC", "broken again"));
        // Another case is another file (on Linux): its failure is its own.
        l.set_failed(fail("A/img_1.heic", "the other one"));
        assert_eq!(l.failed().len(), 3);
        assert_eq!(l.failed_for("A/IMG_1.HEIC").unwrap().reason, "broken again");
        l.save(t.path()).unwrap();
        let mut back = MadeList::load(t.path());
        assert_eq!(back, l);
        assert!(!back.clear_failed("A/img_2.heic"));
        assert!(back.clear_failed("A/IMG_2.HEIC"));
        assert!(!back.clear_failed("A/IMG_2.HEIC"));
        assert_eq!(back.clear_all_failed(), 2);
        assert!(back.failed().is_empty());
    }

    #[test]
    fn a_failure_of_a_heic_that_has_gone_is_dropped_when_the_list_loads() {
        let t = tempfile::tempdir().unwrap();
        put(t.path(), "A/HERE.HEIC", b"h", T);
        let mut l = MadeList::default();
        for h in ["A/HERE.HEIC", "A/GONE.HEIC"] {
            l.set_failed(FailedEntry {
                heic: h.into(),
                reason: "broken".into(),
                heic_size: 1,
                heic_modified_ms: T,
                when: String::new(),
                name_taken: false,
            });
        }
        l.save(t.path()).unwrap();
        let back = MadeList::load(t.path());
        assert_eq!(back.failed().len(), 1);
        assert!(back.failed_for("A/HERE.HEIC").is_some());
        assert_eq!(MadeList::load_readonly(t.path()), back);
    }

    #[test]
    fn a_list_from_before_failures_were_kept_still_loads() {
        let t = tempfile::tempdir().unwrap();
        put(t.path(), "a.JPG", b"x", T);
        fs::write(
            t.path().join(MADE_NAME),
            format!(
                r#"{{"version":1,"made":[{{"jpg":"a.JPG","heic":"a.HEIC","heic_size":1,"heic_modified_ms":{T},"made":"x"}}]}}"#
            ),
        )
        .unwrap();
        let l = MadeList::load(t.path());
        assert_eq!(l.len(), 1);
        assert!(l.failed().is_empty() && l.save_blocked.is_none());
    }

    /// The old format has no facts of the JPG. Such an entry is believed if the JPG has its HEIC's
    /// modified time (the app sets that), and is then upgraded: the facts are filled in and are
    /// what the next save writes.
    #[test]
    fn an_old_entry_is_trusted_by_its_heics_time_and_then_upgraded() {
        let t = tempfile::tempdir().unwrap();
        put(t.path(), "DCIM/IMG_1.JPG", b"made by us", T);
        put(t.path(), "DCIM/IMG_2.JPG", b"not ours", T + 60_000);
        let old = format!(
            r#"{{"version":1,"made":[
              {{"jpg":"DCIM/IMG_1.JPG","heic":"DCIM/IMG_1.HEIC","heic_size":1,"heic_modified_ms":{T},"made":"x"}},
              {{"jpg":"DCIM/IMG_2.JPG","heic":"DCIM/IMG_2.HEIC","heic_size":1,"heic_modified_ms":{T},"made":"x"}}
            ],"failed":[]}}"#
        );
        fs::write(t.path().join(MADE_NAME), old).unwrap();
        let l = MadeList::load(t.path());
        assert_eq!(
            l.len(),
            1,
            "the one whose time is not its HEIC's is dropped"
        );
        let e = l.by_jpg("DCIM/IMG_1.JPG").unwrap();
        assert_eq!(e.jpg_size, Some(10));
        assert_eq!(e.jpg_modified_ms, Some(T));
        // Saved, it is in the new form, and loads the same.
        l.save(t.path()).unwrap();
        let text = fs::read_to_string(t.path().join(MADE_NAME)).unwrap();
        assert!(text.contains("jpg_size"), "{text}");
        assert_eq!(MadeList::load(t.path()), l);
        // From now on it is the facts that count, not the HEIC's time: an edited JPG goes.
        put(t.path(), "DCIM/IMG_1.JPG", b"edited by hand", T);
        assert!(MadeList::load(t.path()).is_empty());
    }

    #[test]
    fn an_entry_whose_file_is_not_a_jpeg_name_is_not_trusted() {
        let t = tempfile::tempdir().unwrap();
        // A forged entry that would make any file count as "made".
        put(t.path(), "DCIM/IMG_5.MOV", b"a video", T);
        saved(t.path(), &[entry("DCIM/IMG_5.MOV", "DCIM/IMG_1.HEIC")]);
        let l = MadeList::load(t.path());
        assert!(l.is_empty() && !l.made_by_us("DCIM/IMG_5.MOV"));
        // Even with the right facts.
        let e = with_facts(t.path(), "DCIM/IMG_5.MOV", "DCIM/IMG_5.HEIC");
        saved(t.path(), &[e]);
        assert!(MadeList::load(t.path()).is_empty());
        assert!(MadeList::load_readonly(t.path()).is_empty());
    }

    #[test]
    fn an_entry_whose_jpg_is_not_the_name_of_its_heic_is_not_trusted() {
        let t = tempfile::tempdir().unwrap();
        put(t.path(), "DCIM/IMG_9.JPG", b"a photo", T);
        put(t.path(), "OTHER/IMG_1.JPG", b"a photo", T);
        put(t.path(), "DCIM/IMG_1.jpg", b"a photo", T);
        saved(
            t.path(),
            &[
                entry("DCIM/IMG_9.JPG", "DCIM/IMG_1.HEIC"),
                entry("OTHER/IMG_1.JPG", "DCIM/IMG_1.HEIC"),
                // The case of the extension follows the HEIC's: `.HEIC` gives `.JPG`.
                entry("DCIM/IMG_1.jpg", "DCIM/IMG_1.HEIC"),
                // Not a HEIC at all.
                entry("DCIM/IMG_9.JPG", "DCIM/IMG_9.MOV"),
            ],
        );
        assert!(MadeList::load(t.path()).is_empty());
    }

    #[test]
    fn a_jpg_the_user_edited_or_the_device_replaced_is_no_longer_the_apps() {
        let t = tempfile::tempdir().unwrap();
        for n in 1..=3 {
            put(t.path(), &format!("DCIM/IMG_{n}.JPG"), b"as made", T);
        }
        let all: Vec<MadeEntry> = (1..=3)
            .map(|n| {
                with_facts(
                    t.path(),
                    &format!("DCIM/IMG_{n}.JPG"),
                    &format!("DCIM/IMG_{n}.HEIC"),
                )
            })
            .collect();
        saved(t.path(), &all);
        assert_eq!(MadeList::load(t.path()).len(), 3);
        // Edited: another size.
        put(t.path(), "DCIM/IMG_1.JPG", b"as edited, longer", T);
        // Edited without a change of size: another time.
        put(t.path(), "DCIM/IMG_2.JPG", b"as made", T + 120_000);
        // Gone, as when the device's own is moved aside.
        fs::remove_file(t.path().join("DCIM/IMG_3.JPG")).unwrap();
        let l = MadeList::load(t.path());
        assert!(l.is_empty(), "{:?}", l.entries());
        // A load doesn't rewrite the file; the next save leaves them out.
        assert!(
            fs::read_to_string(t.path().join(MADE_NAME))
                .unwrap()
                .contains("IMG_1.JPG")
        );
    }

    #[test]
    fn times_round_trip_through_milliseconds() {
        let t = UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);
        assert_eq!(to_ms(t), 1_700_000_000_123);
        assert_eq!(from_ms(to_ms(t)), t);
        let before = UNIX_EPOCH - Duration::from_millis(5000);
        assert_eq!(from_ms(to_ms(before)), before);
    }

    #[test]
    fn the_made_list_is_kept_out_of_scans_and_never_an_archive_name() {
        assert!(crate::owner::is_marker_name(MADE_NAME));
        assert!(crate::owner::is_marker_name(&format!("{MADE_NAME}.tmp")));
        assert!(crate::owner::is_marker_name(&format!(
            "{MADE_NAME}.damaged2"
        )));
        assert!(crate::owner::validate_archive_name(MADE_NAME).is_err());
    }

    mod on_a_store {
        use super::*;
        use crate::store::mem::MemStore;

        fn mem_entry(jpg: &str, heic: &str, size: u64, ms: i64) -> MadeEntry {
            MadeEntry {
                jpg_size: Some(size),
                jpg_modified_ms: Some(ms),
                ..entry(jpg, heic)
            }
        }

        fn at(ms: i64) -> Option<SystemTime> {
            Some(from_ms(ms))
        }

        #[test]
        fn a_list_round_trips_and_only_true_entries_stay() {
            let store = MemStore::new();
            store.insert("DCIM/IMG_1.JPG", b"jpgdata", at(5_000));
            let good = mem_entry("DCIM/IMG_1.JPG", "DCIM/IMG_1.HEIC", 7, 5_000);
            let gone = mem_entry("DCIM/IMG_2.JPG", "DCIM/IMG_2.HEIC", 7, 5_000);
            let mut list = MadeList::default();
            list.insert(good.clone());
            list.insert(gone);
            list.save_in(&store).unwrap();
            assert_eq!(
                MadeList::load_in(&store).entries(),
                std::slice::from_ref(&good)
            );
            assert_eq!(MadeList::load_readonly_in(&store).entries(), [good]);
            // The JPG changes: the entry is no longer believed.
            store.insert("DCIM/IMG_1.JPG", b"edited!!", at(5_000));
            assert!(MadeList::load_in(&store).is_empty());
        }

        #[test]
        fn a_damaged_list_is_set_aside_and_the_next_one_gets_a_new_name() {
            let store = MemStore::new();
            store.insert(MADE_NAME, b"{ broken", None);
            let l = MadeList::load_readonly_in(&store);
            assert!(l.is_empty() && l.save_blocked.is_none());
            assert!(
                store.get(MADE_NAME).is_some(),
                "a read-only load moves nothing"
            );
            let l = MadeList::load_in(&store);
            assert!(l.is_empty() && l.save_blocked.is_none());
            let first = format!("{MADE_NAME}.damaged");
            assert_eq!(store.get(&first).unwrap(), b"{ broken");
            assert!(store.get(MADE_NAME).is_none());
            store.insert(MADE_NAME, b"second", None);
            MadeList::load_in(&store);
            assert_eq!(
                store.get(&format!("{MADE_NAME}.damaged2")).unwrap(),
                b"second"
            );
            assert_eq!(store.get(&first).unwrap(), b"{ broken");
        }

        #[test]
        fn a_list_that_cannot_be_set_aside_is_not_saved() {
            let store = MemStore::new();
            store.insert(MADE_NAME, b"{ broken", None);
            store.fail_nth("move", 1, io::ErrorKind::PermissionDenied);
            let l = MadeList::load_in(&store);
            assert!(l.save_blocked.is_some());
            assert!(l.save_in(&store).is_err());
            assert_eq!(store.get(MADE_NAME).unwrap(), b"{ broken");
        }

        #[test]
        fn failures_of_gone_heics_are_dropped_and_facts_come_from_the_store() {
            let store = MemStore::new();
            store.insert("a/IMG_9.HEIC", b"heic", at(1_000));
            let mut l = MadeList::default();
            for (heic, why) in [("a/IMG_9.HEIC", "kept"), ("a/IMG_8.HEIC", "dropped")] {
                l.set_failed(FailedEntry {
                    heic: heic.into(),
                    reason: why.into(),
                    heic_size: 4,
                    heic_modified_ms: 1_000,
                    when: String::new(),
                    name_taken: false,
                });
            }
            l.save_in(&store).unwrap();
            let back = MadeList::load_in(&store);
            assert_eq!(back.failed().len(), 1);
            assert_eq!(back.failed()[0].reason, "kept");
            let rel = SafeRelPath::parse("a/IMG_9.HEIC").unwrap();
            assert_eq!(facts_in(&store, &rel), Some((4, 1_000)));
            let none = SafeRelPath::parse("a/none.HEIC").unwrap();
            assert_eq!(facts_in(&store, &none), None);
        }
    }
}
