//! Which device a backup folder belongs to: a small file in the backup folder, written by the
//! first sync into it. Files deleted from a device are only archived in that device's own folder,
//! so pointing a second phone at the folder by mistake can't archive the first phone's photos.
#![forbid(unsafe_code)]

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use log::info;
use serde::{Deserialize, Serialize};

use crate::paths::is_link;
use crate::plan::ARCHIVE_DIR;
use crate::signals::{Signals, View};

/// The marker file, in the top of the backup folder.
pub const MARKER: &str = ".plug-n-keep.json";

/// What the marker holds (DESIGN.md section 6.2). It also lets "Find my backups" rebuild the list
/// of known devices if the app's own records are lost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderOwner {
    /// The app-made ID of the device ([`crate::registry::KnownDevice::id`]). Never changes.
    pub device_id: String,
    /// For messages, and for rebuilding the list: e.g. "My phone".
    pub device_name: String,
    /// What the device looked like when the marker was written.
    #[serde(default)]
    pub signals: Signals,
    /// How the device was read when this folder was made. A folder is only synced through the
    /// same view. `None` if the marker doesn't say.
    #[serde(default)]
    pub view: Option<View>,
    /// Every name the archive folder has had in this folder, the current one last. A folder
    /// under any of them is never treated as backup content.
    #[serde(default)]
    pub archive_names: Vec<String>,
    /// When the marker was first written, as UTC text such as `2026-11-17T18:00:00Z`.
    #[serde(default)]
    pub created: String,
    #[serde(default)]
    pub app_version: String,
    /// The folder already held files when this marker was written, and archiving them has not
    /// been confirmed yet. Until one archive is confirmed, files here that aren't on the device
    /// still ask first, so "Leave them" stays left.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub foreign_unconfirmed: bool,
}

impl FolderOwner {
    /// A marker for a folder whose archive is named [`ARCHIVE_DIR`].
    pub fn new(
        device_id: &str,
        device_name: &str,
        signals: Signals,
        view: View,
        created: &str,
    ) -> Self {
        Self {
            device_id: device_id.to_string(),
            device_name: device_name.to_string(),
            signals,
            view: Some(view),
            archive_names: vec![ARCHIVE_DIR.to_string()],
            created: created.to_string(),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            foreign_unconfirmed: false,
        }
    }

    /// The same marker for a folder whose archive is named `name` (a new folder has used no
    /// other name).
    pub fn with_archive_name(mut self, name: &str) -> Self {
        self.archive_names = vec![name.to_string()];
        self
    }

    /// Whether `name` is one of the archive names this folder has had (ignoring case).
    pub fn has_archive_name(&self, name: &str) -> bool {
        self.archive_names
            .iter()
            .any(|n| n.eq_ignore_ascii_case(name))
    }

    /// Make `name` the current (last) archive name, keeping the others. Returns whether the
    /// list changed.
    pub fn use_archive_name(&mut self, name: &str) -> bool {
        if self
            .archive_names
            .last()
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
        {
            return false;
        }
        self.archive_names.retain(|n| !n.eq_ignore_ascii_case(name));
        self.archive_names.push(name.to_string());
        true
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum OwnerState {
    /// No marker yet: a new folder, or one made before this version.
    #[default]
    Unclaimed,
    Owned(Box<FolderOwner>),
    /// There is a marker but it can't be read. Archiving stays off until it's sorted out.
    Unreadable(String),
}

/// Read the marker in `root`. A link or folder in its place counts as unreadable.
pub fn read(root: &Path) -> OwnerState {
    let path = root.join(MARKER);
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return OwnerState::Unclaimed,
        Err(e) => return OwnerState::Unreadable(e.to_string()),
        Ok(m) if is_link(&m) || !m.is_file() => {
            return OwnerState::Unreadable("it is not a plain file".into());
        }
        Ok(_) => {}
    }
    match fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(owner) => OwnerState::Owned(Box::new(owner)),
            Err(e) => OwnerState::Unreadable(e.to_string()),
        },
        Err(e) => OwnerState::Unreadable(e.to_string()),
    }
}

/// Write the marker into `root_canon`, the canonical backup folder, replacing any marker
/// in one step.
pub fn write(root_canon: &Path, owner: &FolderOwner) -> io::Result<()> {
    write_marker(root_canon, owner, true)
}

/// Write a first marker into `root_canon`: if one has turned up since it was last read, it is
/// not replaced (`AlreadyExists`).
pub fn write_new(root_canon: &Path, owner: &FolderOwner) -> io::Result<()> {
    write_marker(root_canon, owner, false)
}

fn write_marker(root_canon: &Path, owner: &FolderOwner, replace: bool) -> io::Result<()> {
    let tmp = root_canon.join(format!("{MARKER}.tmp"));
    let _ = fs::remove_file(&tmp); // a stale one from a crash
    {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(owner).map_err(io::Error::other)?)?;
        crate::platform::flush_file(&f)?;
    }
    let to = root_canon.join(MARKER);
    if replace {
        crate::platform::rename_replace(&tmp, &to)?;
    } else if let Err(e) = crate::platform::rename_no_replace(&tmp, &to) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    info!(
        "Recorded {} as the owner of the backup folder {} ({MARKER})",
        owner.device_name,
        root_canon.display()
    );
    Ok(())
}

/// Why a folder owned by `owner` can't be used by the device `device_id` read through `view`, or
/// `None` if it can. Another device's folder is refused, and so is the same device through a
/// different view: its files look different there, so files would be archived as "deleted".
pub fn refusal(owner: &FolderOwner, device_id: &str, view: View) -> Option<String> {
    if owner.device_id != device_id {
        return Some(format!(
            "this backup folder belongs to another device ({})",
            owner.device_name
        ));
    }
    match owner.view {
        Some(made) if made != view => Some(format!(
            "this backup folder was made reading the device as \"{}\", and it is now read as \"{}\"; \
             the files look different that way, so use a separate backup folder for this connection",
            made.name(),
            view.name()
        )),
        _ => None,
    }
}

/// Why a folder whose marker records a view can't be handed to a device read through `view`: the
/// files look different through another view, so a hand-over must not change the marker's
/// view. `None` if the marker has no view, or the same one.
pub fn view_conflict(owner: &FolderOwner, view: View) -> Option<String> {
    match owner.view {
        Some(made) if made != view => Some(format!(
            "this backup folder was made reading the device as \"{}\", and it is now read as \"{}\"; \
             the files look different that way, so it can't be handed over. Use a separate backup \
             folder for this connection",
            made.name(),
            view.name()
        )),
        _ => None,
    }
}

/// "Use this folder for this device": make `device_id` the owner of `root`, replacing the
/// marker in one step. The marker is read again first, so it only happens if the folder is
/// still another device's (or its marker is unreadable) at this moment. It refuses when the
/// marker's view differs ([`view_conflict`]). The old marker's `archive_names` and `created`
/// are kept, with the current archive name last, so an archive renamed earlier is never taken
/// for backup content. Nothing is moved.
pub fn hand_over(
    root: &Path,
    device_id: &str,
    device_name: &str,
    signals: Signals,
    view: View,
    archive_name: &str,
    now: &str,
) -> Result<(), String> {
    let root = fs::canonicalize(root)
        .map_err(|e| format!("Could not find the backup folder {}: {e}", root.display()))?;
    let mut marker = FolderOwner::new(device_id, device_name, signals, view, now)
        .with_archive_name(archive_name);
    match read(&root) {
        OwnerState::Owned(old) => {
            if let Some(why) = view_conflict(&old, view) {
                return Err(format!("Not handed over: {why}."));
            }
            if refusal(&old, device_id, view).is_none() {
                return Err("This folder already belongs to this device.".into());
            }
            for name in &old.archive_names {
                marker.use_archive_name(name);
            }
            marker.use_archive_name(archive_name);
            if !old.created.is_empty() {
                marker.created = old.created.clone();
            }
        }
        OwnerState::Unclaimed => {
            return write_new(&root, &marker)
                .map_err(|e| format!("Could not record the folder's owner: {e}"));
        }
        OwnerState::Unreadable(_) => {}
    }
    write(&root, &marker).map_err(|e| format!("Could not record the folder's owner: {e}"))
}

/// The names that are the app's own in a backup folder, never backup content: every archive
/// name the marker lists, plus the device's current one, `current`. The marker itself is
/// covered by [`is_marker_name`].
pub(crate) fn archive_names(state: &OwnerState, current: &str) -> Vec<String> {
    let mut names = vec![current.to_string()];
    if let OwnerState::Owned(o) = state {
        for n in &o.archive_names {
            if !names.iter().any(|x| x.eq_ignore_ascii_case(n)) {
                names.push(n.clone());
            }
        }
    }
    names
}

/// Whether `name` can be an archive folder's name: one safe name by the rules of
/// [`crate::paths::SafeRelPath`] (which also refuse the app's temporary-file suffix), and not
/// the marker's name.
pub fn validate_archive_name(name: &str) -> Result<(), String> {
    crate::paths::validate_component(name)
        .map_err(|why| format!("the archive folder name {name:?} can't be used: {why}"))?;
    if is_marker_name(name) {
        return Err(format!(
            "the archive folder name {name:?} can't be used: it is the name of the app's marker file"
        ));
    }
    Ok(())
}

/// The marker, the device record, the sync history, the skip list, the list of made JPG copies and their
/// temporary or set-aside files: app files, not backup files.
pub fn is_marker_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [
        MARKER,
        crate::heic::made::MADE_NAME,
        crate::folder_record::DEVICE_FILE,
        crate::folder_record::HISTORY_FILE,
        crate::state::SKIP_NAME,
    ]
    .iter()
    .any(|n| lower.starts_with(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_archive_name_is_one_safe_name_that_is_not_the_markers() {
        for ok in [
            "_Archive",
            "_Deleted from phone",
            "Old stuff",
            ".hidden",
            "a.b",
        ] {
            assert!(validate_archive_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "a:b",
            "a*b",
            "a\u{1}b",
            "NUL",
            "com1",
            "end.",
            "end ",
            "x.pnk-part",
            "X.PNK-PART",
            ".plug-n-keep.json",
            ".PLUG-N-KEEP.JSON",
            ".plug-n-keep.json.tmp",
        ] {
            assert!(validate_archive_name(bad).is_err(), "{bad:?}");
        }
        assert!(validate_archive_name(&"n".repeat(255)).is_ok());
        assert!(validate_archive_name(&"n".repeat(256)).is_err());
    }

    #[test]
    fn the_current_archive_name_is_last_and_the_old_ones_are_kept() {
        let mut o = FolderOwner::new(
            "id",
            "phone",
            Signals::default(),
            View::WindowsWpd,
            "2026-01-01T00:00:00Z",
        );
        assert_eq!(o.archive_names, [ARCHIVE_DIR]);
        assert!(
            !o.use_archive_name("_archive"),
            "already current, in any case"
        );
        assert!(o.use_archive_name("Gone"));
        assert_eq!(o.archive_names, [ARCHIVE_DIR, "Gone"]);
        assert!(o.has_archive_name("gone") && o.has_archive_name("_Archive"));
        // Going back to an old name makes it current again without listing it twice.
        assert!(o.use_archive_name("_Archive"));
        assert_eq!(o.archive_names, ["Gone", ARCHIVE_DIR]);
        let o = o.with_archive_name("Fresh");
        assert_eq!(o.archive_names, ["Fresh"]);
    }

    #[test]
    fn every_listed_name_and_the_current_one_stay_out_of_the_scan() {
        let mut o = FolderOwner::new(
            "id",
            "phone",
            Signals::default(),
            View::WindowsWpd,
            "2026-01-01T00:00:00Z",
        );
        o.archive_names = vec!["Old".into(), "Older".into()];
        let state = OwnerState::Owned(Box::new(o));
        assert_eq!(archive_names(&state, "Now"), ["Now", "Old", "Older"]);
        assert_eq!(archive_names(&OwnerState::Unclaimed, "Now"), ["Now"]);
    }

    fn marker(id: &str, view: View, names: &[&str]) -> FolderOwner {
        let mut o = FolderOwner::new(
            id,
            "old phone",
            Signals::default(),
            view,
            "2025-05-05T00:00:00Z",
        );
        o.archive_names = names.iter().map(|n| n.to_string()).collect();
        o
    }

    #[test]
    fn a_hand_over_keeps_the_old_archive_names_and_the_creation_time() {
        let t = tempfile::tempdir().unwrap();
        write(
            t.path(),
            &marker("old", View::Files, &["Old name", "_Archive"]),
        )
        .unwrap();
        hand_over(
            t.path(),
            "new",
            "new stick",
            Signals::default(),
            View::Files,
            "Fresh",
            "2026-01-01T00:00:00Z",
        )
        .unwrap();
        let OwnerState::Owned(m) = read(t.path()) else {
            panic!("no marker")
        };
        assert_eq!(m.device_id, "new");
        assert_eq!(m.archive_names, ["Old name", "_Archive", "Fresh"]);
        assert_eq!(m.created, "2025-05-05T00:00:00Z");
    }

    #[test]
    fn a_hand_over_is_refused_when_the_view_differs_and_changes_nothing() {
        let t = tempfile::tempdir().unwrap();
        let old = marker("old", View::WindowsWpd, &["_Archive"]);
        write(t.path(), &old).unwrap();
        let err = hand_over(
            t.path(),
            "new",
            "n",
            Signals::default(),
            View::Afc,
            "_Archive",
            "now",
        )
        .unwrap_err();
        assert!(err.contains("windows-wpd") && err.contains("afc"), "{err}");
        assert_eq!(read(t.path()), OwnerState::Owned(Box::new(old)));
    }

    #[test]
    fn a_hand_over_checks_again_that_the_folder_is_still_blocked() {
        let t = tempfile::tempdir().unwrap();
        write(t.path(), &marker("me", View::Files, &["_Archive"])).unwrap();
        let err = hand_over(
            t.path(),
            "me",
            "n",
            Signals::default(),
            View::Files,
            "_Archive",
            "now",
        )
        .unwrap_err();
        assert!(err.contains("already belongs"), "{err}");
    }

    #[test]
    fn a_hand_over_into_a_folder_with_no_marker_writes_a_fresh_one() {
        let t = tempfile::tempdir().unwrap();
        hand_over(
            t.path(),
            "me",
            "n",
            Signals::default(),
            View::Files,
            "Arch",
            "now",
        )
        .unwrap();
        let OwnerState::Owned(m) = read(t.path()) else {
            panic!("no marker")
        };
        assert_eq!(
            (m.device_id.as_str(), m.archive_names.as_slice()),
            ("me", ["Arch".to_string()].as_slice())
        );
    }

    /// The first claim of a folder never replaces a marker that turned up since it was read.
    #[test]
    fn a_first_marker_never_replaces_one_that_is_already_there() {
        let t = tempfile::tempdir().unwrap();
        let mine = marker("first", View::Files, &["A"]);
        write_new(t.path(), &mine).unwrap();
        let err = write_new(t.path(), &marker("second", View::Files, &["B"])).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        let OwnerState::Owned(m) = read(t.path()) else {
            panic!("no marker")
        };
        assert_eq!(m.device_id, "first");
        assert!(!t.path().join(format!("{MARKER}.tmp")).exists());
        // A hand-over into a folder where a marker has turned up is a replacement, and is
        // checked on its own.
        write(t.path(), &marker("third", View::Files, &["C"])).unwrap();
    }
}
