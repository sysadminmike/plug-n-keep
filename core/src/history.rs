//! The history of every sync, one file per device.
//!
//! A device's record only keeps the last sync's summary. The full history lives apart, so that
//! file stays small and a damaged history can never cost a device its settings. It is kept in the
//! device's backup folder, so it travels with the backup (`.plug-n-keep-history.jsonl`, see
//! [`crate::folder_record`]). A device with no backup folder yet keeps it in the settings folder:
//!
//! `<settings folder>/history/<device id>.jsonl`, one JSON object ([`SyncRecord`]) per line,
//! oldest first. A sync adds its line by reading the file and writing it back with the line added, only if it is unchanged. At most
//! [`KEEP`] records are kept: when a new line takes the file over that, it is rewritten with the
//! newest [`KEEP`] (in the same write). A line that can't be read
//! (a crash mid-write, an edit) is skipped and never stops the others loading.
//!
//! Forgetting a device leaves every history file alone, in its backup folder and here.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use log::warn;
use serde::{Deserialize, Serialize};

use crate::folder_record::HISTORY_FILE;
use crate::paths::SafeRelPath;
use crate::registry::DeletedFiles;
use crate::state::StateDir;
use crate::store::{BackupStore, LocalStore, is_conflict};

/// The most records kept for one device.
pub const KEEP: usize = 500;

const DIR: &str = "history";

/// What one sync did. A field a line lacks has its default, so records from an earlier or later
/// version still load.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncRecord {
    /// When the sync started, as UTC text such as `2026-11-18T09:30:12Z`.
    pub started: String,
    /// The same moment on this PC's clock, `2026-11-18 09-30-12`, for showing it.
    pub local: String,
    /// How long it took, in seconds.
    pub secs: f64,
    /// Files copied, and their bytes.
    pub copied: u64,
    pub copied_bytes: u64,
    /// Of those, changed files whose old copy was replaced (and moved out of the way).
    pub replaced: u64,
    pub already_backed_up: u64,
    pub skipped: u64,
    /// Files that failed to copy.
    pub failed: u64,
    /// What this device does with files deleted from it: the next three say what that did.
    pub mode: DeletedFiles,
    /// Moved out of place: into the archive folder, or into the Recycle Bin or Trash (by `mode`).
    pub moved: u64,
    /// Files deleted from the device that stayed in the backup: all of them with
    /// [`DeletedFiles::LeaveInPlace`], else the ones that could not be moved.
    pub left: u64,
    /// Moving deleted files was refused as a whole (it needed an OK, or the folder was not safe).
    pub move_refused: bool,
    /// JPG copies of HEIC photos made in this sync (DESIGN.md section 7.6), and the HEICs that
    /// didn't get one (they are listed in the file list, under "Couldn't make a JPG").
    pub jpgs_made: u64,
    pub jpgs_failed: u64,
    /// Stopped by the user.
    pub stopped: bool,
    /// Cut off: a transfer ended badly and the connection had to be reset.
    pub cut_off: bool,
    /// Stopped because the backup disk filled up.
    pub disk_full: bool,
    /// Anything else worth saying (an archiving refusal, for one).
    pub note: String,
}

impl Default for SyncRecord {
    fn default() -> Self {
        Self {
            started: String::new(),
            local: String::new(),
            secs: 0.0,
            copied: 0,
            copied_bytes: 0,
            replaced: 0,
            already_backed_up: 0,
            skipped: 0,
            failed: 0,
            mode: DeletedFiles::Archive,
            moved: 0,
            left: 0,
            move_refused: false,
            jpgs_made: 0,
            jpgs_failed: 0,
            stopped: false,
            cut_off: false,
            disk_full: false,
            note: String::new(),
        }
    }
}

impl SyncRecord {
    /// Whether the sync ended before it was done, for any reason.
    pub fn ended_early(&self) -> bool {
        self.stopped || self.cut_off || self.disk_full
    }
}

/// The histories kept in one settings folder.
#[derive(Debug, Clone)]
pub struct History {
    state: StateDir,
}

impl History {
    pub fn new(state: &StateDir) -> Self {
        Self {
            state: state.clone(),
        }
    }

    fn folder(&self) -> PathBuf {
        self.state.path().join(DIR)
    }

    /// The file for a device. The ID is a UUID, but the name is made so that no other ID can
    /// give the same one (a lossy `_` for every odd character would let `a/b` and `a:b` share a
    /// history): letters, digits and `-` stay, every other byte becomes `_` and two hex digits,
    /// so `_` itself is `_5F`. It can never name another folder, and a name that Windows keeps
    /// for devices (`CON`, `NUL`, `COM1`, ...) gets `_r` in front, which no encoded name has.
    fn file_of(&self, device_id: &str) -> PathBuf {
        let mut safe = String::new();
        for b in device_id.bytes() {
            if b.is_ascii_alphanumeric() || b == b'-' {
                safe.push(b as char);
            } else {
                safe.push_str(&format!("_{b:02X}"));
            }
        }
        if safe.is_empty() {
            safe.push_str("_00");
        }
        if is_reserved_name(&safe) {
            safe.insert_str(0, "_r");
        }
        self.folder().join(format!("{safe}.jsonl"))
    }

    /// Add one record at the end of the device's history in the settings folder: one write,
    /// flushed to disk. Trims the file to the newest [`KEEP`] when it is over.
    pub fn append(&self, device_id: &str, record: &SyncRecord) -> io::Result<()> {
        fs::create_dir_all(self.folder())?;
        append_at(&self.file_of(device_id), record)
    }

    /// The device's history in the settings folder, newest first. A line that can't be read is
    /// skipped (and logged); no file is no history.
    pub fn load(&self, device_id: &str) -> Vec<SyncRecord> {
        load_at(&self.file_of(device_id))
    }

    /// Where the device's history is kept in the settings folder (the file may not exist).
    pub fn profile_file(&self, device_id: &str) -> PathBuf {
        self.file_of(device_id)
    }
}

/// A file on this PC as a store and a name in it.
fn local_of(path: &Path) -> io::Result<(LocalStore, SafeRelPath)> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a usable file name"))?;
    let rel = SafeRelPath::parse(name)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;
    Ok((LocalStore::new(dir), rel))
}

fn history_rel() -> SafeRelPath {
    SafeRelPath::parse(HISTORY_FILE).expect("the history name is a safe name")
}

/// How many times an append starts again when the file changed under it.
const TRIES: usize = 5;

/// Add one record at the end of the history file `path`, trimmed to the newest [`KEEP`] when it
/// is over.
pub fn append_at(path: &Path, record: &SyncRecord) -> io::Result<()> {
    let (store, rel) = local_of(path)?;
    append_in(&store, &rel, record)
}

/// Add one record to the history file `rel` in `store`: read the file, add the line, and write it
/// back only if it is still as it was read (starting again a few times if not). A file that is
/// not there is created, never replacing one that turned up. The newest [`KEEP`] readable lines
/// are kept when the file is over that.
pub fn append_in(
    store: &dyn BackupStore,
    rel: &SafeRelPath,
    record: &SyncRecord,
) -> io::Result<()> {
    let mut line = serde_json::to_string(record).map_err(io::Error::other)?;
    line.push('\n');
    let mut last_err = None;
    for _ in 0..TRIES {
        let (mut bytes, tag) = match store.read(rel)? {
            Some((b, t)) => (b, Some(t)),
            None => (Vec::new(), None),
        };
        // A crash may have left the last line without its end: start on a fresh line, or this
        // record would be glued to the damaged one and lost with it.
        if bytes.last().is_some_and(|&b| b != b'\n') {
            bytes.push(b'\n');
        }
        bytes.extend_from_slice(line.as_bytes());
        let bytes = trimmed(bytes);
        let done = match &tag {
            Some(t) => store.write_replace(rel, &bytes, Some(t)),
            None => store.write_new(rel, &bytes),
        };
        match done {
            Ok(_) => return Ok(()),
            Err(e) if is_conflict(&e) || e.kind() == io::ErrorKind::AlreadyExists => {
                last_err = Some(e);
            }
            Err(e) => return Err(e),
        }
    }
    Err(last_err.unwrap_or_else(|| io::Error::other("could not add to the history")))
}

/// The newest [`KEEP`] readable lines if there are more lines than that (damaged lines go),
/// else the text as it is.
fn trimmed(bytes: Vec<u8>) -> Vec<u8> {
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() <= KEEP {
        return bytes;
    }
    let good: Vec<&str> = lines
        .into_iter()
        .filter(|l| serde_json::from_str::<SyncRecord>(l).is_ok())
        .collect();
    let keep = &good[good.len().saturating_sub(KEEP)..];
    let mut out = keep.join("\n");
    out.push('\n');
    out.into_bytes()
}

/// The history in the file `path`, newest first. A line that can't be read is skipped (and
/// logged); no file is no history.
pub fn load_at(path: &Path) -> Vec<SyncRecord> {
    match local_of(path) {
        Ok((store, rel)) => load_in(&store, &rel),
        Err(e) => {
            warn!("Could not read {}: {e}", path.display());
            Vec::new()
        }
    }
}

/// The history in the file `rel` in `store`, newest first. A line that can't be read is skipped
/// (and logged); no file is no history.
pub fn load_in(store: &dyn BackupStore, rel: &SafeRelPath) -> Vec<SyncRecord> {
    let bytes = match store.read(rel) {
        Ok(Some((b, _))) => b,
        Ok(None) => return Vec::new(),
        Err(e) => {
            warn!("Could not read {rel} in {}: {e}", store.describe());
            return Vec::new();
        }
    };
    let text = String::from_utf8_lossy(&bytes);
    let mut skipped = 0;
    let mut records: Vec<SyncRecord> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| match serde_json::from_str(l) {
            Ok(r) => Some(r),
            Err(_) => {
                skipped += 1;
                None
            }
        })
        .collect();
    if skipped > 0 {
        warn!(
            "{skipped} damaged line(s) in {rel} in {} were skipped",
            store.describe()
        );
    }
    if records.len() > KEEP {
        records.drain(..records.len() - KEEP);
    }
    records.reverse();
    records
}

/// Put every readable record of the history file `from` into the history file `to`, which may
/// not exist yet: both files' lines, none twice, oldest first (by start time), the newest
/// [`KEEP`]. `to` is replaced in one step and `from` is not touched, so doing it again changes
/// nothing. No `from` is nothing to do. Returns whether `to` was written.
pub fn merge_into(from: &Path, to: &Path) -> io::Result<bool> {
    let (store, rel) = local_of(to)?;
    merge_file_into(from, &store, &rel)
}

/// [`merge_into`] with the history file of a backup folder held in `store` as the target.
pub fn merge_from_file(from: &Path, store: &dyn BackupStore) -> io::Result<bool> {
    merge_file_into(from, store, &history_rel())
}

fn merge_file_into(from: &Path, store: &dyn BackupStore, rel: &SafeRelPath) -> io::Result<bool> {
    let from_text = match fs::read(from) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    let (to_text, tag) = match store.read(rel)? {
        Some((b, t)) => (Some(String::from_utf8_lossy(&b).into_owned()), Some(t)),
        None => (None, None),
    };
    let mut seen = std::collections::HashSet::new();
    let mut lines: Vec<(String, String)> = Vec::new();
    for line in to_text
        .as_deref()
        .unwrap_or("")
        .lines()
        .chain(from_text.lines())
    {
        let line = line.trim();
        if line.is_empty() || !seen.insert(line.to_string()) {
            continue;
        }
        if let Ok(r) = serde_json::from_str::<SyncRecord>(line) {
            lines.push((r.started, line.to_string()));
        }
    }
    // Stable: lines with the same start keep the order they came in.
    lines.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = String::new();
    for (_, l) in &lines[lines.len().saturating_sub(KEEP)..] {
        out.push_str(l);
        out.push('\n');
    }
    if to_text.as_deref() == Some(out.as_str()) {
        return Ok(false);
    }
    match tag {
        Some(t) => store.write_replace(rel, out.as_bytes(), Some(&t))?,
        None => store.write_new(rel, out.as_bytes())?,
    };
    Ok(true)
}

/// Whether a file name (without its extension) is one Windows keeps for devices.
fn is_reserved_name(stem: &str) -> bool {
    let up = stem.to_ascii_uppercase();
    matches!(up.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|p| {
            up.strip_prefix(p)
                .is_some_and(|n| n.len() == 1 && n != "0" && n.as_bytes()[0].is_ascii_digit())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::mem::MemStore;
    use std::fs::OpenOptions;
    use std::io::Write;

    fn history() -> (tempfile::TempDir, History) {
        let t = tempfile::tempdir().unwrap();
        let state = StateDir::open(t.path()).unwrap();
        (t, History::new(&state))
    }

    fn rec(n: u64) -> SyncRecord {
        SyncRecord {
            started: format!("2026-11-18T09:{:02}:00Z", n % 60),
            copied: n,
            ..SyncRecord::default()
        }
    }

    #[test]
    fn a_device_with_no_history_has_none() {
        let (_t, h) = history();
        assert!(h.load("dev").is_empty());
    }

    #[test]
    fn a_record_is_added_as_one_line_and_read_back() {
        let (t, h) = history();
        let mut r = rec(3);
        r.mode = DeletedFiles::RecycleBin;
        r.disk_full = true;
        r.note = "a note".into();
        h.append("dev-1", &r).unwrap();
        let text = fs::read_to_string(t.path().join("history/dev-1.jsonl")).unwrap();
        assert_eq!(text.lines().count(), 1);
        assert!(text.ends_with('\n'));
        assert_eq!(h.load("dev-1"), vec![r]);
        // Another device's history is its own.
        assert!(h.load("dev-2").is_empty());
    }

    #[test]
    fn the_newest_record_comes_first() {
        let (_t, h) = history();
        for n in 1..=4 {
            h.append("d", &rec(n)).unwrap();
        }
        let copied: Vec<u64> = h.load("d").iter().map(|r| r.copied).collect();
        assert_eq!(copied, [4, 3, 2, 1]);
    }

    #[test]
    fn only_the_newest_500_are_kept_and_the_file_is_trimmed() {
        let (t, h) = history();
        for n in 1..=(KEEP as u64 + 7) {
            h.append("d", &rec(n)).unwrap();
        }
        let loaded = h.load("d");
        assert_eq!(loaded.len(), KEEP);
        assert_eq!(loaded[0].copied, KEEP as u64 + 7, "the newest first");
        assert_eq!(loaded[KEEP - 1].copied, 8, "the oldest 7 are gone");
        let text = fs::read_to_string(t.path().join("history/d.jsonl")).unwrap();
        assert_eq!(text.lines().count(), KEEP, "the file itself was trimmed");
        assert!(!t.path().join("history/d.jsonl.tmp").exists());
    }

    #[test]
    fn a_damaged_line_is_skipped_and_the_others_load() {
        let (t, h) = history();
        h.append("d", &rec(1)).unwrap();
        let path = t.path().join("history/d.jsonl");
        // A crash mid-write: half a line, no end.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"started\":\"2026-11-1").unwrap();
        drop(f);
        assert_eq!(h.load("d").len(), 1);
        // The next record starts on its own line, so it is not lost with the damaged one.
        h.append("d", &rec(2)).unwrap();
        // And some other junk in the middle.
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str("not json at all\n\n");
        fs::write(&path, text).unwrap();
        h.append("d", &rec(3)).unwrap();
        let copied: Vec<u64> = h.load("d").iter().map(|r| r.copied).collect();
        assert_eq!(copied, [3, 2, 1]);
    }

    #[test]
    fn damaged_lines_do_not_count_toward_the_cap() {
        let (t, h) = history();
        let path = t.path().join("history/d.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "junk\n".repeat(20)).unwrap();
        for n in 1..=KEEP as u64 {
            h.append("d", &rec(n)).unwrap();
        }
        assert_eq!(h.load("d").len(), KEEP);
        assert_eq!(h.load("d")[0].copied, KEEP as u64);
    }

    #[test]
    fn a_line_with_fewer_or_more_fields_still_loads() {
        let (t, h) = history();
        let path = t.path().join("history/d.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "{\"started\":\"2026-01-01T00:00:00Z\",\"copied\":5,\"from_the_future\":true}\n",
        )
        .unwrap();
        let r = &h.load("d")[0];
        assert_eq!((r.copied, r.failed, r.mode), (5, 0, DeletedFiles::Archive));
    }

    #[test]
    fn an_odd_id_names_no_other_folder() {
        let (t, h) = history();
        h.append("a", &rec(1)).unwrap();
        h.append("b", &rec(2)).unwrap();
        h.append("../../evil", &rec(3)).unwrap();
        assert!(t.path().join("history").read_dir().unwrap().count() >= 2);
        assert!(!t.path().join("evil.jsonl").exists());
        assert_eq!(h.load("../../evil").len(), 1);
    }

    #[test]
    fn jpg_counts_are_kept_and_an_older_line_loads_with_none() {
        let t = tempfile::tempdir().unwrap();
        let h = History::new(&StateDir::open(t.path().join("s")).unwrap());
        let r = SyncRecord {
            jpgs_made: 12,
            jpgs_failed: 2,
            ..SyncRecord::default()
        };
        h.append("dev", &r).unwrap();
        let back = h.load("dev");
        assert_eq!((back[0].jpgs_made, back[0].jpgs_failed), (12, 2));
        let old: SyncRecord = serde_json::from_str(r#"{"copied": 3}"#).unwrap();
        assert_eq!((old.copied, old.jpgs_made, old.jpgs_failed), (3, 0, 0));
    }

    #[test]
    fn different_ids_never_share_a_file_and_reserved_names_are_avoided() {
        let (_t, h) = history();
        let ids = [
            "a/b", "a:b", "a_b", "a_2Fb", "", "_", "A", "con", "CON", "nul", "COM1", "com9",
            "COM0", "\u{e9}", "\u{e8}",
        ];
        let mut seen = std::collections::HashSet::new();
        for id in ids {
            let f = h.file_of(id);
            assert_eq!(f.parent().unwrap(), h.folder(), "{id}");
            let name = f
                .file_name()
                .unwrap()
                .to_string_lossy()
                .to_ascii_lowercase();
            // On Windows names are not case-sensitive: "A" and "a" would clash, so the test
            // folds case; IDs are lower-case UUIDs.
            if !seen.insert(name.clone()) {
                assert!(matches!(id, "CON" | "A"), "{id} shares {name}");
            }
            let stem = name.trim_end_matches(".jsonl");
            assert!(!is_reserved_name(stem), "{id} gives {stem}");
        }
        // A real ID is unchanged.
        let uuid = "123e4567-e89b-12d3-a456-426614174000";
        assert_eq!(
            h.file_of(uuid).file_name().unwrap().to_string_lossy(),
            format!("{uuid}.jsonl")
        );
    }

    fn hrel() -> SafeRelPath {
        SafeRelPath::parse(HISTORY_FILE).unwrap()
    }

    fn line(n: u64) -> String {
        format!("{}\n", serde_json::to_string(&rec(n)).unwrap())
    }

    #[test]
    fn on_a_store_a_record_is_added_and_the_newest_come_first() {
        let store = MemStore::new();
        for n in 1..=3 {
            append_in(&store, &hrel(), &rec(n)).unwrap();
        }
        let copied: Vec<u64> = load_in(&store, &hrel()).iter().map(|r| r.copied).collect();
        assert_eq!(copied, [3, 2, 1]);
        assert_eq!(store.keys(), [HISTORY_FILE]);
    }

    /// A store that lets another writer add a line right after the append's first read.
    struct Racing<'a> {
        inner: &'a MemStore,
        done: std::sync::atomic::AtomicBool,
    }

    // Only the calls the history makes matter; the rest pass straight through.
    impl BackupStore for Racing<'_> {
        fn list(&self, p: &dyn Fn(&str) -> bool) -> io::Result<crate::store::Listing> {
            self.inner.list(p)
        }
        fn stat(&self, r: &SafeRelPath) -> io::Result<Option<crate::store::Stat>> {
            self.inner.stat(r)
        }
        fn read(&self, r: &SafeRelPath) -> io::Result<Option<(Vec<u8>, crate::store::Tag)>> {
            let got = self.inner.read(r);
            if !self.done.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let (mut b, _) = self.inner.read(r).unwrap().unwrap();
                b.extend_from_slice(line(2).as_bytes());
                self.inner.insert(HISTORY_FILE, &b, None);
            }
            got
        }
        fn write_new(&self, r: &SafeRelPath, b: &[u8]) -> io::Result<crate::store::Tag> {
            self.inner.write_new(r, b)
        }
        fn write_replace(
            &self,
            r: &SafeRelPath,
            b: &[u8],
            e: Option<&crate::store::Tag>,
        ) -> io::Result<crate::store::Tag> {
            self.inner.write_replace(r, b, e)
        }
        fn begin_upload(
            &self,
            r: &SafeRelPath,
            s: u64,
            m: Option<std::time::SystemTime>,
        ) -> io::Result<Box<dyn crate::store::Upload>> {
            self.inner.begin_upload(r, s, m)
        }
        fn abort_by_ref(&self, j: &str) -> io::Result<()> {
            self.inner.abort_by_ref(j)
        }
        fn move_no_replace(&self, f: &SafeRelPath, t: &SafeRelPath) -> io::Result<()> {
            self.inner.move_no_replace(f, t)
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
            Ok(true)
        }
        fn ensure_root(&self) -> io::Result<()> {
            Ok(())
        }
        fn describe(&self) -> String {
            "racing".into()
        }
    }

    #[test]
    fn an_append_that_loses_a_race_reads_again_and_keeps_both_lines() {
        let store = MemStore::new();
        append_in(&store, &hrel(), &rec(1)).unwrap();
        let racing = Racing {
            inner: &store,
            done: std::sync::atomic::AtomicBool::new(false),
        };
        append_in(&racing, &hrel(), &rec(3)).unwrap();
        let copied: Vec<u64> = load_in(&store, &hrel()).iter().map(|r| r.copied).collect();
        assert_eq!(copied, [3, 2, 1], "nothing lost");
        // One write that conflicted, then the one that went through.
        assert_eq!(store.calls("write_replace"), 2);
    }

    #[test]
    fn an_append_that_fails_for_another_reason_is_not_retried() {
        let store = MemStore::new();
        append_in(&store, &hrel(), &rec(1)).unwrap();
        store.fail_nth("write_replace", 1, io::ErrorKind::PermissionDenied);
        assert!(append_in(&store, &hrel(), &rec(2)).is_err());
        assert_eq!(store.calls("write_replace"), 1, "tried once, not again");
    }

    #[test]
    fn a_first_append_that_finds_a_file_has_turned_up_adds_to_it() {
        let store = MemStore::new();
        // The create is refused as if the file had just appeared; the append reads again.
        store.fail_nth("write_new", 1, io::ErrorKind::AlreadyExists);
        store.insert(HISTORY_FILE, line(1).as_bytes(), None);
        append_in(&store, &hrel(), &rec(2)).unwrap();
        assert_eq!(load_in(&store, &hrel()).len(), 2);
        // And with no file the create is refused once, then the retry creates it.
        let fresh = MemStore::new();
        fresh.fail_nth("write_new", 1, io::ErrorKind::AlreadyExists);
        append_in(&fresh, &hrel(), &rec(1)).unwrap();
        assert_eq!(load_in(&fresh, &hrel()).len(), 1);
    }

    #[test]
    fn on_a_store_a_damaged_line_is_skipped_and_the_cap_holds() {
        let store = MemStore::new();
        let cut = "{\"started\":\"2026-01-01T00:00:00Z\",\"copied\":9}";
        store.insert(HISTORY_FILE, format!("junk\n{cut}").as_bytes(), None);
        append_in(&store, &hrel(), &rec(1)).unwrap();
        let copied: Vec<u64> = load_in(&store, &hrel()).iter().map(|r| r.copied).collect();
        assert_eq!(copied, [1, 9], "the new line got its own line");
        for n in 2..=(KEEP as u64 + 3) {
            append_in(&store, &hrel(), &rec(n)).unwrap();
        }
        let text = String::from_utf8(store.get(HISTORY_FILE).unwrap()).unwrap();
        assert_eq!(text.lines().count(), KEEP);
        assert!(!text.contains("junk"));
    }

    #[test]
    fn a_merge_into_a_store_is_written_once() {
        let t = tempfile::tempdir().unwrap();
        let from = t.path().join("from.jsonl");
        fs::write(&from, (1..=3).map(line).collect::<String>()).unwrap();
        let store = MemStore::new();
        append_in(&store, &hrel(), &rec(2)).unwrap();
        assert!(merge_from_file(&from, &store).unwrap());
        assert!(!merge_from_file(&from, &store).unwrap());
        let copied: Vec<u64> = load_in(&store, &hrel()).iter().map(|r| r.copied).collect();
        assert_eq!(copied, [3, 2, 1]);
    }
}
