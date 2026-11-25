//! The ID file kept at the top of a stick or drive (DESIGN.md section 5.4): reading it, and the
//! one function that writes it.
//!
//! The file is named [`FILE_NAME`] and holds `{"app", "device_id", "written"}`. Reading never
//! fails loudly: a missing, unreadable or damaged file is reported as such, never a panic.
#![forbid(unsafe_code)]

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;

use log::{info, warn};
use serde::{Deserialize, Serialize};

use crate::volume::open_read_only;

/// The ID file's name, at the top of the device.
pub const FILE_NAME: &str = ".plug-n-keep-id";
/// The `app` field's value.
const APP: &str = "plug-n-keep";
/// A real ID file is tiny; anything bigger is not ours.
const MAX_BYTES: u64 = 4096;
/// The longest `device_id` accepted.
const MAX_ID_CHARS: usize = 128;

#[derive(Serialize, Deserialize)]
struct Contents {
    app: String,
    device_id: String,
    written: String,
}

/// What reading the ID file found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdFile {
    /// There is no ID file.
    Missing,
    /// A good file, with this device ID.
    Found(String),
    /// Something is at the name but it is not a usable ID file (not JSON, another program's
    /// file, a link, a folder, too big, unreadable). Counts as no ID file, and is listed under
    /// Problems.
    Damaged(String),
}

impl IdFile {
    /// The device ID, if the file was good.
    pub fn id(&self) -> Option<&str> {
        match self {
            IdFile::Found(id) => Some(id),
            _ => None,
        }
    }
}

fn id_is_sane(id: &str) -> bool {
    !id.trim().is_empty()
        && id.chars().count() <= MAX_ID_CHARS
        && !id.chars().any(|c| c.is_control())
}

/// Reads the ID file at the top of `root`, through the read-only open. Never writes, never
/// panics.
pub fn read(root: &Path) -> IdFile {
    let path = root.join(FILE_NAME);
    let meta = match fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return IdFile::Missing,
        Err(e) => return IdFile::Damaged(format!("can't be looked at: {e}")),
    };
    if crate::platform::is_link(&meta) || !meta.is_file() {
        return IdFile::Damaged("is not a plain file".into());
    }
    if meta.len() > MAX_BYTES {
        return IdFile::Damaged("is too big to be an ID file".into());
    }
    let mut bytes = Vec::new();
    match open_read_only(&path) {
        Ok(f) => {
            if let Err(e) = f.take(MAX_BYTES + 1).read_to_end(&mut bytes) {
                return IdFile::Damaged(format!("can't be read: {e}"));
            }
        }
        Err(e) => return IdFile::Damaged(format!("can't be opened: {e}")),
    }
    match serde_json::from_slice::<Contents>(&bytes) {
        Ok(c) if c.app != APP => IdFile::Damaged("belongs to another program".into()),
        Ok(c) if id_is_sane(&c.device_id) => IdFile::Found(c.device_id),
        Ok(_) => IdFile::Damaged("holds no usable device ID".into()),
        Err(e) => IdFile::Damaged(format!("is not a valid ID file: {e}")),
    }
}

/// Writes the ID file at the top of `root`. **This is the only function in the app that writes
/// to a device.** Only the user's explicit Yes to the question "write an ID file to this
/// stick?" may call it: never recognition, scanning, syncing or any background step.
///
/// - It creates a new file only (`create_new`), so it never overwrites anything: if something
///   is already at the name it fails with `AlreadyExists` and changes nothing.
/// - The file is hidden on Windows, and flushed before this returns `Ok`.
/// - If anything fails after the file was created, the part-written file is removed again.
pub fn write(root: &Path, device_id: &str) -> io::Result<()> {
    write_checked(root, device_id, None)
}

/// Replaces this app's own ID file at the top of `root` with one holding `device_id`: for a
/// stick that is a copy of another (it carries the same ID), after the user's explicit Yes. It
/// only ever replaces a good ID file written by this app (anything else, including a damaged
/// file, is left alone and refused). The new file is written beside it and moved over the
/// old one in one step, so a failure leaves the old file as it was.
pub fn replace(root: &Path, device_id: &str) -> io::Result<()> {
    if !id_is_sane(device_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the device ID is empty, too long or has control characters",
        ));
    }
    if !matches!(read(root), IdFile::Found(_)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "there is no ID file of this app to replace",
        ));
    }
    let text = serde_json::to_string(&Contents {
        app: APP.into(),
        device_id: device_id.into(),
        written: crate::utc_stamp(),
    })
    .map_err(io::Error::other)?;
    let tmp = root.join(format!("{FILE_NAME}.tmp"));
    let path = root.join(FILE_NAME);
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opts.attributes(0x2);
    }
    let result = (|| {
        let mut file = opts.open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.write_all(
            b"
",
        )?;
        crate::platform::flush_file(&file)?;
        drop(file);
        crate::platform::rename_replace(&tmp, &path)
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = crate::platform::flush_dir(root) {
        warn!("Could not flush {}: {e}", root.display());
    }
    info!("Replaced the ID file {}", path.display());
    Ok(())
}

/// [`write`], with a failure that tests can make happen after the file exists.
fn write_checked(
    root: &Path,
    device_id: &str,
    fail_after_create: Option<io::ErrorKind>,
) -> io::Result<()> {
    if !id_is_sane(device_id) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the device ID is empty, too long or has control characters",
        ));
    }
    let text = serde_json::to_string(&Contents {
        app: APP.into(),
        device_id: device_id.into(),
        written: crate::utc_stamp(),
    })
    .map_err(io::Error::other)?;
    let path = root.join(FILE_NAME);
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
        opts.attributes(FILE_ATTRIBUTE_HIDDEN);
    }
    // Nothing exists yet, so a failure here (AlreadyExists included) has nothing to clean up.
    let mut file = opts.open(&path)?;
    let result = (|| {
        if let Some(kind) = fail_after_create {
            return Err(io::Error::new(kind, "simulated failure"));
        }
        file.write_all(text.as_bytes())?;
        file.write_all(b"\n")?;
        crate::platform::flush_file(&file)
    })();
    match result {
        Ok(()) => {
            drop(file);
            // The new name's folder entry; best effort, the file itself is already flushed.
            if let Err(e) = crate::platform::flush_dir(root) {
                warn!("Could not flush {}: {e}", root.display());
            }
            info!("Wrote the ID file {}", path.display());
            Ok(())
        }
        Err(e) => {
            drop(file);
            if let Err(rm) = fs::remove_file(&path) {
                warn!(
                    "The ID file {} could not be written ({e}) and its remains could not be removed: {rm}",
                    path.display()
                );
            }
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn a_written_file_reads_back() {
        let d = dir();
        assert_eq!(read(d.path()), IdFile::Missing);
        write(d.path(), "5f1c-aa").unwrap();
        assert_eq!(read(d.path()), IdFile::Found("5f1c-aa".into()));
        let text = fs::read_to_string(d.path().join(FILE_NAME)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["app"], "plug-n-keep");
        assert_eq!(v["device_id"], "5f1c-aa");
        assert!(v["written"].as_str().unwrap().ends_with('Z'));
    }

    #[test]
    fn a_damaged_file_is_reported_not_a_panic() {
        let cases: [(&[u8], &str); 6] = [
            (b"", "empty"),
            (b"not json", "garbage"),
            (b"{\"app\":\"plug-n-keep\"}", "no id"),
            (
                b"{\"app\":\"other\",\"device_id\":\"x\",\"written\":\"t\"}",
                "other app",
            ),
            (
                b"{\"app\":\"plug-n-keep\",\"device_id\":\"  \",\"written\":\"t\"}",
                "blank id",
            ),
            (&[0xff, 0xfe, 0x00], "binary"),
        ];
        for (bytes, what) in cases {
            let d = dir();
            fs::write(d.path().join(FILE_NAME), bytes).unwrap();
            assert!(
                matches!(read(d.path()), IdFile::Damaged(_)),
                "{what}: {:?}",
                read(d.path())
            );
        }
        let d = dir();
        fs::write(d.path().join(FILE_NAME), vec![b' '; 10_000]).unwrap();
        assert!(matches!(read(d.path()), IdFile::Damaged(_)));
        let d = dir();
        fs::create_dir(d.path().join(FILE_NAME)).unwrap();
        assert!(matches!(read(d.path()), IdFile::Damaged(_)));
    }

    #[test]
    fn an_existing_file_is_never_overwritten() {
        let d = dir();
        let path = d.path().join(FILE_NAME);
        // Someone else's file, and a good ID file: both stay exactly as they were.
        fs::write(&path, b"precious").unwrap();
        let e = write(d.path(), "new-id").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path).unwrap(), b"precious");
        fs::remove_file(&path).unwrap();
        write(d.path(), "first").unwrap();
        let before = fs::read(&path).unwrap();
        assert!(write(d.path(), "second").is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(read(d.path()).id(), Some("first"));
    }

    #[test]
    fn a_file_that_fails_after_it_was_created_is_removed() {
        let d = dir();
        let e = write_checked(d.path(), "x1", Some(io::ErrorKind::StorageFull)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::StorageFull);
        assert!(
            fs::read_dir(d.path()).unwrap().next().is_none(),
            "the partial file was left behind"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_failed_create_on_a_read_only_folder_leaves_nothing() {
        let d = dir();
        let ro = crate::testutil::ReadOnlyDir::new(d.path());
        if ro.is_enforced() {
            assert!(write(d.path(), "x1").is_err());
            assert!(fs::read_dir(d.path()).unwrap().next().is_none());
        }
    }

    #[test]
    fn a_bad_id_is_refused_before_anything_is_created() {
        let d = dir();
        for bad in ["", "  ", "a\nb", &"x".repeat(200)] {
            assert_eq!(
                write(d.path(), bad).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
        assert!(fs::read_dir(d.path()).unwrap().next().is_none());
    }

    #[test]
    #[cfg(unix)]
    fn a_link_at_the_name_is_damaged_and_never_followed_or_written_through() {
        let d = dir();
        let victim = d.path().join("victim");
        fs::write(&victim, b"precious").unwrap();
        std::os::unix::fs::symlink(&victim, d.path().join(FILE_NAME)).unwrap();
        assert!(matches!(read(d.path()), IdFile::Damaged(_)));
        assert!(write(d.path(), "x1").is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"precious");
    }

    #[cfg(windows)]
    #[test]
    fn the_file_is_hidden_on_windows() {
        use std::os::windows::fs::MetadataExt;
        let d = dir();
        write(d.path(), "x1").unwrap();
        let m = fs::metadata(d.path().join(FILE_NAME)).unwrap();
        assert_ne!(m.file_attributes() & 0x2, 0);
    }

    /// Only the user's Yes may write to a device: no code in the core (outside its test modules)
    /// calls `write`.
    #[test]
    fn nothing_in_the_core_calls_write_except_this_file() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let needle = ["idfile", "::", "write("].concat();
        let mut stack = vec![src];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap() {
                let p = entry.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|e| e == "rs")
                    && p.file_name().is_some_and(|n| n != "idfile.rs")
                {
                    // Test modules (at the end of a file) may write to temp folders.
                    let text = fs::read_to_string(&p).unwrap().replace("\r\n", "\n");
                    let code = text.split("#[cfg(test)]\nmod ").next().unwrap_or("");
                    assert!(!code.contains(&needle), "{} calls the writer", p.display());
                }
            }
        }
    }

    #[test]
    fn replace_swaps_our_own_file_and_refuses_anything_else() {
        let d = dir();
        assert!(replace(d.path(), "new").is_err(), "nothing to replace");
        write(d.path(), "old-id").unwrap();
        replace(d.path(), "new-id").unwrap();
        assert_eq!(read(d.path()), IdFile::Found("new-id".into()));
        assert!(!d.path().join(format!("{FILE_NAME}.tmp")).exists());
        fs::write(d.path().join(FILE_NAME), b"not ours").unwrap();
        assert!(replace(d.path(), "x").is_err());
        assert_eq!(fs::read(d.path().join(FILE_NAME)).unwrap(), b"not ours");
    }

    /// DESIGN.md 10.3: the ID file is the one thing the app writes to a device, and only the
    /// window's handlers for the user's Yes may do it. Nothing else in either crate calls
    /// [`write`] or [`replace`] (code in test modules aside).
    #[test]
    fn only_the_windows_confirmed_handlers_write_an_id_file() {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut found = Vec::new();
        for src_dir in [manifest.join("src"), manifest.join("../app/src")] {
            for entry in walkdir::WalkDir::new(&src_dir) {
                let entry = entry.unwrap();
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                let text = fs::read_to_string(path).unwrap();
                // Production code only: up to the first test module.
                let production = &text[..text.find("#[cfg(test)]").unwrap_or(text.len())];
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                for call in ["idfile::write(", "idfile::replace("] {
                    for (i, _) in production.match_indices(call) {
                        let func = production[..i]
                            .rfind("fn ")
                            .map(|f| {
                                production[f + 3..]
                                    .chars()
                                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                                    .collect::<String>()
                            })
                            .unwrap_or_default();
                        found.push((name.clone(), func));
                    }
                }
            }
        }
        found.sort();
        assert_eq!(
            found,
            [
                ("worker.rs".to_string(), "replace_id_file".to_string()),
                ("worker.rs".to_string(), "write_id_file".to_string()),
            ],
            "{found:?}"
        );
    }
}
