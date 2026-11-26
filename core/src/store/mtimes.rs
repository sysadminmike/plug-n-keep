//! The modified-time cache of an object store (S3-PLAN section 4).
//!
//! An object store's listing has no device modified times, so a stick or card (compared by date)
//! would need one stat per file. `.plug-n-keep-mtimes.json` at the store root keeps them:
//! key -> {size, tag, mtime}. The listing stays the truth: an entry counts only when its size and
//! tag match the listing. One read at the start of a scan, one write at the end of a sync (and a
//! checkpoint every 15 minutes in a long one). Losing it is harmless: a missing entry costs one
//! stat. Folders on this PC never use it: they keep real file times.
#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::io;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use log::{info, warn};
use serde::{Deserialize, Serialize};

use super::{BackupStore, Tag, conflict, is_conflict};
use crate::paths::SafeRelPath;
use crate::plan::IndexedFile;

/// The cache object's name, at the store root.
pub const NAME: &str = ".plug-n-keep-mtimes.json";

const VERSION: u32 = 1;
/// A long sync saves the cache about this often.
const CHECKPOINT: Duration = Duration::from_secs(15 * 60);
/// Tries to save when another writer keeps getting there first.
const TRIES: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub size: u64,
    pub tag: String,
    /// Milliseconds since the epoch.
    pub mtime: i64,
}

impl Entry {
    pub fn new(size: u64, tag: &str, mtime: SystemTime) -> Self {
        Entry {
            size,
            tag: tag.to_string(),
            mtime: to_ms(mtime),
        }
    }
}

fn to_ms(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_millis()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_millis()).unwrap_or(i64::MAX),
    }
}

fn from_ms(ms: i64) -> SystemTime {
    if ms >= 0 {
        UNIX_EPOCH + Duration::from_millis(ms.unsigned_abs())
    } else {
        UNIX_EPOCH - Duration::from_millis(ms.unsigned_abs())
    }
}

#[derive(Serialize, Deserialize)]
struct Doc {
    version: u32,
    entries: BTreeMap<String, Entry>,
}

fn rel() -> SafeRelPath {
    SafeRelPath::parse(NAME).expect("the cache name is a safe path")
}

fn encode(entries: &BTreeMap<String, Entry>) -> Vec<u8> {
    serde_json::to_vec(&Doc {
        version: VERSION,
        entries: entries.clone(),
    })
    .unwrap_or_default()
}

/// The cache as it was read, and what a scan makes of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// The tag of the cache object when it was read; `None` if there was none.
    pub tag: Option<Tag>,
    pub entries: BTreeMap<String, Entry>,
    /// Differs from what is stored (a lookup was added or a stale entry dropped).
    pub changed: bool,
}

impl Snapshot {
    /// The cached modified time, if the entry for `key` matches the listing's size and tag.
    pub fn lookup(&self, key: &str, size: u64, tag: Option<&str>) -> Option<SystemTime> {
        let e = self.entries.get(key)?;
        (e.size == size && Some(e.tag.as_str()) == tag).then(|| from_ms(e.mtime))
    }

    /// What this scan leaves: the entries that still match the listing (`files`) plus the
    /// lookups it made. Entries of keys no longer listed are dropped.
    pub fn carried<'a>(
        self,
        files: impl Iterator<Item = &'a IndexedFile>,
        found: Vec<(String, Entry)>,
    ) -> Snapshot {
        let mut entries = BTreeMap::new();
        for f in files {
            if let Some(e) = self.entries.get(&f.path)
                && e.size == f.size
                && Some(e.tag.as_str()) == f.tag.as_deref()
            {
                entries.insert(f.path.clone(), e.clone());
            }
        }
        entries.extend(found);
        let changed = entries != self.entries;
        Snapshot {
            tag: self.tag,
            entries,
            changed,
        }
    }
}

fn parse(bytes: &[u8]) -> BTreeMap<String, Entry> {
    match serde_json::from_slice::<Doc>(bytes) {
        Ok(d) if d.version == VERSION => d.entries,
        _ => BTreeMap::new(),
    }
}

/// Reads the cache: one request. A missing, unreadable or unknown-version cache is an empty one
/// (the scan then looks files up itself); an unreachable store is left for the scan to report.
pub fn load(store: &dyn BackupStore) -> Snapshot {
    match store.read(&rel()) {
        Ok(Some((bytes, tag))) => {
            let entries = parse(&bytes);
            info!("Read the modified-time cache: {} entries", entries.len());
            Snapshot {
                tag: Some(tag),
                entries,
                changed: false,
            }
        }
        Ok(None) => Snapshot::default(),
        Err(e) => {
            warn!("Could not read the modified-time cache ({e}); files are looked up one by one");
            Snapshot::default()
        }
    }
}

/// Saves `entries` with a conditional write against `tag`. If another writer changed the cache
/// meanwhile, reads theirs, lays `fresh` over it and tries again. `tag` ends as the new tag.
fn save(
    store: &dyn BackupStore,
    tag: &mut Option<Tag>,
    entries: &mut BTreeMap<String, Entry>,
    fresh: &BTreeMap<String, Entry>,
) -> io::Result<()> {
    let rel = rel();
    for _ in 0..TRIES {
        let bytes = encode(entries);
        let done = match tag.as_ref() {
            Some(t) => store.write_replace(&rel, &bytes, Some(t)),
            None => store.write_new(&rel, &bytes),
        };
        match done {
            Ok(new) => {
                *tag = Some(new);
                return Ok(());
            }
            Err(e) if is_conflict(&e) || e.kind() == io::ErrorKind::AlreadyExists => {
                // Theirs is the base now; what this run found goes on top.
                let (theirs, their_tag) = match store.read(&rel)? {
                    Some((b, t)) => (parse(&b), Some(t)),
                    None => (BTreeMap::new(), None),
                };
                *entries = theirs;
                entries.extend(fresh.iter().map(|(k, v)| (k.clone(), v.clone())));
                *tag = their_tag;
            }
            Err(e) => return Err(e),
        }
    }
    Err(conflict())
}

/// Makes the next scan rebuild the cache: replaces it with an empty one (nothing is deleted).
/// Nothing happens on a folder, which has no cache. The next scan looks every file up and the
/// next sync writes a full cache.
pub fn forget_cache(store: &dyn BackupStore) -> io::Result<()> {
    if store.as_local().is_some() {
        return Ok(());
    }
    let tag = match store.read(&rel())? {
        Some((_, t)) => t,
        None => return Ok(()),
    };
    reset(store, tag)?;
    info!("The modified-time cache was reset");
    Ok(())
}

/// Writes an empty cache over the one with `tag`. If another writer got in first, their tag is
/// read and the empty cache is written again: a reset never merges theirs back in.
fn reset(store: &dyn BackupStore, mut tag: Tag) -> io::Result<()> {
    let rel = rel();
    let bytes = encode(&BTreeMap::new());
    for _ in 0..TRIES {
        match store.write_replace(&rel, &bytes, Some(&tag)) {
            Ok(_) => return Ok(()),
            Err(e) if is_conflict(&e) || e.kind() == io::ErrorKind::AlreadyExists => {
                match store.read(&rel)? {
                    Some((_, t)) => tag = t,
                    // Gone meanwhile: nothing left to reset.
                    None => return Ok(()),
                }
            }
            Err(e) => return Err(e),
        }
    }
    Err(conflict())
}

/// What an upload left behind: its tag and the modified time it was given.
#[derive(Debug, Clone)]
pub struct Uploaded {
    pub tag: Tag,
    pub mtime: Option<SystemTime>,
}

/// The cache during a sync: starts from the scan's snapshot, takes the uploads, and saves.
pub struct Session {
    on: bool,
    tag: Option<Tag>,
    entries: BTreeMap<String, Entry>,
    fresh: BTreeMap<String, Entry>,
    dirty: bool,
    last: Instant,
    every: Duration,
}

impl Session {
    /// For an object store with a scan that read the cache; otherwise it does nothing.
    pub fn start(store: &dyn BackupStore, snapshot: Option<&Snapshot>) -> Session {
        let on = store.as_local().is_none() && snapshot.is_some();
        let s = snapshot.cloned().unwrap_or_default();
        Session {
            on,
            tag: s.tag,
            entries: s.entries,
            fresh: BTreeMap::new(),
            dirty: s.changed,
            last: Instant::now(),
            every: CHECKPOINT,
        }
    }

    #[cfg(test)]
    pub(crate) fn every(mut self, every: Duration) -> Self {
        self.every = every;
        self
    }

    /// A file this sync uploaded: its size, tag and modified time as they are in the store now.
    pub fn uploaded(&mut self, key: &str, size: u64, up: Uploaded) {
        let Some(mtime) = up.mtime else {
            return;
        };
        if !self.on {
            return;
        }
        let e = Entry::new(size, &up.tag, mtime);
        self.entries.insert(key.to_string(), e.clone());
        self.fresh.insert(key.to_string(), e);
        self.dirty = true;
    }

    /// Saves now if there is something new and the last save was long ago.
    pub fn checkpoint(&mut self, store: &dyn BackupStore) {
        if self.on && self.dirty && self.last.elapsed() >= self.every {
            self.write(store);
        }
    }

    /// The save at the end of a sync, if anything changed.
    pub fn finish(&mut self, store: &dyn BackupStore) {
        if self.on && self.dirty {
            self.write(store);
        }
    }

    fn write(&mut self, store: &dyn BackupStore) {
        self.last = Instant::now();
        match save(store, &mut self.tag, &mut self.entries, &self.fresh) {
            Ok(()) => {
                self.dirty = false;
                self.fresh.clear();
                info!(
                    "Saved the modified-time cache: {} entries",
                    self.entries.len()
                );
            }
            // Losing the cache costs one stat per file next scan.
            Err(e) => warn!("Could not save the modified-time cache ({e}); it is rebuilt later"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::LocalStore;
    use crate::store::mem::MemStore;

    fn at(ms: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_millis(ms)
    }

    fn up(tag: &str, ms: u64) -> Uploaded {
        Uploaded {
            tag: tag.into(),
            mtime: Some(at(ms)),
        }
    }

    fn stored(store: &MemStore) -> BTreeMap<String, Entry> {
        parse(&store.get(NAME).expect("the cache is there"))
    }

    fn writes(store: &MemStore) -> usize {
        store.calls("write_new") + store.calls("write_replace")
    }

    #[test]
    fn the_name_is_an_app_file_not_backup_content() {
        assert!(crate::owner::is_marker_name(NAME));
        assert!(NAME.starts_with('.'));
        assert!(SafeRelPath::parse(NAME).is_ok());
    }

    #[test]
    fn an_entry_counts_only_when_size_and_tag_match() {
        let mut s = Snapshot::default();
        s.entries.insert("a".into(), Entry::new(5, "t1", at(1000)));
        assert_eq!(s.lookup("a", 5, Some("t1")), Some(at(1000)));
        assert_eq!(s.lookup("a", 6, Some("t1")), None);
        assert_eq!(s.lookup("a", 5, Some("t2")), None);
        assert_eq!(s.lookup("a", 5, None), None);
        assert_eq!(s.lookup("b", 5, Some("t1")), None);
        // Times before 1970 round trip as well.
        let e = Entry::new(1, "t", UNIX_EPOCH - Duration::from_millis(1500));
        assert_eq!(e.mtime, -1500);
        assert_eq!(from_ms(e.mtime), UNIX_EPOCH - Duration::from_millis(1500));
    }

    #[test]
    fn a_scan_keeps_what_still_matches_and_drops_what_is_gone() {
        let mut s = Snapshot::default();
        s.entries.insert("keep".into(), Entry::new(1, "t", at(1)));
        s.entries
            .insert("changed".into(), Entry::new(1, "old", at(2)));
        s.entries.insert("gone".into(), Entry::new(1, "t", at(3)));
        let file = |p: &str, tag: &str| IndexedFile {
            size: 1,
            path: p.into(),
            modified: None,
            tag: Some(tag.into()),
        };
        let files = [
            file("keep", "t"),
            file("changed", "new"),
            file("fresh", "f"),
        ];
        let found = vec![("fresh".to_string(), Entry::new(1, "f", at(4)))];
        let out = s.carried(files.iter(), found);
        let keys: Vec<&str> = out.entries.keys().map(String::as_str).collect();
        assert_eq!(keys, ["fresh", "keep"]);
        assert!(out.changed);
        // Nothing dropped and nothing found: unchanged, so a sync has nothing to save.
        let again = [file("keep", "t"), file("fresh", "f")];
        let same = Snapshot {
            changed: false,
            ..out
        };
        assert!(!same.carried(again.iter(), vec![]).changed);
    }

    #[test]
    fn a_damaged_or_unknown_cache_reads_as_empty() {
        let store = MemStore::new();
        store.insert(NAME, b"not json", None);
        let s = load(&store);
        assert!(s.entries.is_empty() && s.tag.is_some());
        let unknown = br#"{"version":99,"entries":{"a":{"size":1,"tag":"t","mtime":1}}}"#;
        store.insert(NAME, unknown, None);
        assert!(load(&store).entries.is_empty());
        // And it is replaced, not left, by the next save.
        let mut session = Session::start(&store, Some(&load(&store)));
        session.uploaded("x", 3, up("t", 5));
        session.finish(&store);
        assert_eq!(stored(&store).keys().collect::<Vec<_>>(), ["x"]);
    }

    #[test]
    fn a_sync_saves_once_and_a_checkpoint_only_when_due() {
        let store = MemStore::new();
        let mut s = Session::start(&store, Some(&Snapshot::default()));
        s.uploaded("a", 1, up("t1", 10));
        // Not due: the first checkpoint is 15 minutes away.
        s.checkpoint(&store);
        assert_eq!(writes(&store), 0);
        s.finish(&store);
        assert_eq!(store.calls("write_new"), 1);
        assert_eq!(store.calls("write_replace"), 0);
        // Nothing new: nothing written.
        s.finish(&store);
        assert_eq!(writes(&store), 1);

        let mut s = Session::start(&store, Some(&load(&store))).every(Duration::ZERO);
        s.uploaded("b", 2, up("t2", 20));
        s.checkpoint(&store);
        assert_eq!(store.calls("write_replace"), 1, "the checkpoint");
        s.uploaded("c", 3, up("t3", 30));
        s.finish(&store);
        assert_eq!(store.calls("write_replace"), 2);
        let keys: Vec<String> = stored(&store).into_keys().collect();
        assert_eq!(keys, ["a", "b", "c"]);
    }

    #[test]
    fn an_upload_without_a_time_or_on_a_folder_is_not_cached() {
        let store = MemStore::new();
        let mut s = Session::start(&store, Some(&Snapshot::default()));
        let no_time = Uploaded {
            tag: "t".into(),
            mtime: None,
        };
        s.uploaded("a", 1, no_time);
        s.finish(&store);
        assert!(store.get(NAME).is_none());
        // A folder store never has a cache, whatever the scan carried.
        let dir = tempfile::tempdir().unwrap();
        let local = LocalStore::new(dir.path());
        let mut s = Session::start(&local, Some(&Snapshot::default()));
        s.uploaded("a", 1, up("t", 1));
        s.finish(&local);
        assert!(!dir.path().join(NAME).exists());
        forget_cache(&local).unwrap();
        assert!(!dir.path().join(NAME).exists());
    }

    #[test]
    fn a_writer_that_got_there_first_is_merged_not_overwritten() {
        let store = MemStore::new();
        let mut first = Session::start(&store, Some(&Snapshot::default()));
        first.uploaded("a", 1, up("t1", 10));
        first.finish(&store);
        // Both read the same cache; the other one saves first.
        let snap = load(&store);
        let mut mine = Session::start(&store, Some(&snap));
        let mut theirs = Session::start(&store, Some(&snap));
        theirs.uploaded("theirs", 2, up("t2", 20));
        theirs.finish(&store);
        mine.uploaded("mine", 3, up("t3", 30));
        mine.finish(&store);
        let keys: Vec<String> = stored(&store).into_keys().collect();
        assert_eq!(keys, ["a", "mine", "theirs"]);
        // Theirs, then ours refused and ours again: two writes after the first.
        assert_eq!(store.calls("write_replace"), 3);
        // The same when the cache did not exist yet for either.
        let fresh = MemStore::new();
        let mut a = Session::start(&fresh, Some(&Snapshot::default()));
        let mut b = Session::start(&fresh, Some(&Snapshot::default()));
        a.uploaded("a", 1, up("t", 1));
        a.finish(&fresh);
        b.uploaded("b", 1, up("t", 1));
        b.finish(&fresh);
        assert_eq!(stored(&fresh).into_keys().collect::<Vec<_>>(), ["a", "b"]);
    }

    #[test]
    fn a_save_that_keeps_conflicting_gives_up_quietly() {
        let store = MemStore::new();
        let mut s = Session::start(&store, Some(&Snapshot::default()));
        s.uploaded("a", 1, up("t", 1));
        // Every attempt finds the name taken meanwhile; the retries run out and nothing panics.
        for n in 1..=TRIES {
            store.fail_nth("write_new", n, io::ErrorKind::AlreadyExists);
        }
        s.finish(&store);
        assert!(s.dirty, "still to be saved at the next chance");
        assert!(store.get(NAME).is_none());
    }

    #[test]
    fn forgetting_replaces_the_cache_with_an_empty_one_and_never_deletes() {
        let store = MemStore::new();
        // Nothing to forget: nothing is made.
        forget_cache(&store).unwrap();
        assert!(store.get(NAME).is_none());
        let mut s = Session::start(&store, Some(&Snapshot::default()));
        s.uploaded("a", 1, up("t", 1));
        s.finish(&store);
        forget_cache(&store).unwrap();
        assert!(store.get(NAME).is_some(), "the object stays");
        assert!(stored(&store).is_empty());
        assert!(load(&store).entries.is_empty());
    }

    #[test]
    fn a_reset_that_meets_another_writer_still_ends_empty() {
        let store = MemStore::new();
        let mut s = Session::start(&store, Some(&Snapshot::default()));
        s.uploaded("a", 1, up("t", 1));
        s.finish(&store);
        let stale = store.insert(NAME, &encode(&BTreeMap::new()), None);
        // The other writer saves entries after the tag was read.
        let mut other = Session::start(&store, Some(&load(&store)));
        other.uploaded("theirs", 2, up("t2", 2));
        other.finish(&store);
        assert!(!stored(&store).is_empty());
        reset(&store, stale).unwrap();
        assert!(
            stored(&store).is_empty(),
            "the other writer's entries are not written back"
        );
    }
}
