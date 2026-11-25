//! Checking what the user types into the settings, and the warnings the new-device card gives
//! about the chosen folder. No window in here.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use pnk_core::owner::{self, OwnerState};
use pnk_core::paths::validate_component;
use pnk_core::registry::{DeviceSettings, FileTypes, KnownDevice};

/// Split a typed list at line breaks and commas: trimmed, empty ones dropped, repeats (in any
/// case) dropped.
pub fn parse_names(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    text.split(['\n', '\r', ','])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter(|s| seen.insert(s.to_lowercase()))
        .map(str::to_string)
        .collect()
}

/// A list as it is shown for editing: one name per line.
pub fn names_text(names: &[String]) -> String {
    names.join("\n")
}

/// An archive folder's name (DESIGN.md section 6.3): one safe name, not the marker's, and not a
/// name the device uses at the top of the backup (`top_names`, lower case, from the last scan).
pub fn check_archive_name(name: &str, top_names: &[String]) -> Result<(), String> {
    owner::validate_archive_name(name)?;
    if top_names.iter().any(|t| t.eq_ignore_ascii_case(name)) {
        return Err(format!(
            "The device already has a folder named \"{name}\" at the top, so the archive can't use that name."
        ));
    }
    Ok(())
}

/// One top-level folder of the device, or one name to leave out: a single plain name.
pub fn check_name(name: &str) -> Result<(), String> {
    validate_component(name).map_err(|why| format!("\"{name}\" can't be used: {why}."))
}

/// File name extensions typed as `jpg, .PNG, *.mov`: lower case, no dot, letters and digits.
pub fn parse_extensions(text: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for item in parse_names(text) {
        let ext = item
            .trim_start_matches('*')
            .trim_start_matches('.')
            .to_lowercase();
        if ext.is_empty() || ext.len() > 10 || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(format!(
                "\"{item}\" is not a file type: use letters and digits only, such as jpg."
            ));
        }
        if !out.contains(&ext) {
            out.push(ext);
        }
    }
    Ok(out)
}

/// Every problem with these settings, as `(which setting, what is wrong)`. Empty means they can
/// be saved.
pub fn check_settings(s: &DeviceSettings, top_names: &[String]) -> Vec<(&'static str, String)> {
    let mut problems = Vec::new();
    if let Err(e) = check_archive_name(&s.archive_name, top_names) {
        problems.push(("archive name", e));
    }
    for f in &s.folders {
        if let Err(e) = check_name(f) {
            problems.push(("folders", e));
        }
    }
    for n in &s.always_left_out {
        if let Err(e) = check_name(n) {
            problems.push(("left-out names", e));
        }
    }
    for n in &s.ignore_names {
        if let Err(e) = check_name(n) {
            problems.push(("ignore list", e));
        }
    }
    if s.file_types == FileTypes::PhotosAndVideos && s.media_extensions.is_empty() {
        problems.push((
            "file types",
            "Photos and videos needs at least one file type.".into(),
        ));
    }
    problems
}

/// A folder typed or chosen for a backup: it must be a full path.
pub fn check_backup_folder(text: &str) -> Result<PathBuf, String> {
    let t = text.trim();
    if t.is_empty() {
        return Err("Choose a backup folder.".into());
    }
    let p = PathBuf::from(t);
    if !p.is_absolute() {
        return Err(format!(
            "Enter the backup folder's full path, such as {}.",
            crate::format::example_folder()
        ));
    }
    Ok(p)
}

/// What the new-device card says about the chosen folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderNote {
    /// Nothing to say.
    Fine,
    /// The folder's marker says it belongs to another device.
    OtherDevice(String),
    /// Another known device backs up to this folder.
    KnownElsewhere(String),
    /// It is not empty and has no marker: its files count as already backed up where they match.
    HoldsFiles(usize),
}

impl FolderNote {
    pub fn text(&self) -> Option<String> {
        match self {
            FolderNote::Fine => None,
            FolderNote::OtherDevice(name) => Some(format!(
                "This folder belongs to another device (\"{name}\"). Nothing will be backed up into it. Choose another folder."
            )),
            FolderNote::KnownElsewhere(name) => Some(format!(
                "\"{name}\" already backs up to this folder. Choose another folder."
            )),
            FolderNote::HoldsFiles(1) => Some(
                "This folder already holds 1 item. It is kept as it is; if a file on the device matches it, that file counts as backed up."
                    .to_string(),
            ),
            FolderNote::HoldsFiles(n) => Some(format!(
                "This folder already holds {}. They are kept as they are; a file on the device that matches one counts as backed up.",
                if *n >= 1000 {
                    "1,000 or more items".to_string()
                } else {
                    format!("{} items", crate::format::count(*n))
                }
            )),
        }
    }

    /// A warning that should stop the folder being used as it is.
    pub fn blocks(&self) -> bool {
        matches!(
            self,
            FolderNote::OtherDevice(_) | FolderNote::KnownElsewhere(_)
        )
    }
}

/// Whether two folder paths are the same folder by their text: without regard to case on Windows
/// only (on Linux `/data/Photos` and `/data/photos` are two folders).
fn same_folder(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        let t = p.to_string_lossy();
        let t = t.trim_end_matches(['\\', '/']);
        if cfg!(windows) {
            t.to_lowercase()
        } else {
            t.to_string()
        }
    };
    norm(a) == norm(b)
}

/// Look at `folder` for the card: whose marker it holds, whether another known device uses it,
/// whether it already has files. `own_id` is the device being set up, if it has one already.
pub fn folder_note(folder: &Path, own_id: Option<&str>, known: &[KnownDevice]) -> FolderNote {
    if let Some(d) = known.iter().find(|d| {
        Some(d.id.as_str()) != own_id
            && d.backup_dir
                .as_deref()
                .is_some_and(|p| same_folder(p, folder))
    }) {
        return FolderNote::KnownElsewhere(d.name.clone());
    }
    if let OwnerState::Owned(m) = owner::read(folder) {
        if Some(m.device_id.as_str()) != own_id {
            return FolderNote::OtherDevice(m.device_name);
        }
        return FolderNote::Fine;
    }
    match std::fs::read_dir(folder) {
        Ok(entries) => {
            // The app's own files (the marker, the device record, the history) aren't "files".
            let n = entries
                .flatten()
                .filter(|e| !owner::is_marker_name(&e.file_name().to_string_lossy()))
                .take(1000)
                .count();
            if n > 0 {
                FolderNote::HoldsFiles(n)
            } else {
                FolderNote::Fine
            }
        }
        Err(_) => FolderNote::Fine,
    }
}

/// A folder name from a device name: characters Windows doesn't allow become `_`, and a name
/// that would end in a dot or space, or is empty, is fixed.
pub fn folder_name_for(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim_end_matches(['.', ' ']).to_string();
    if cleaned.is_empty() || validate_component(&cleaned).is_err() {
        "Device".to_string()
    } else {
        cleaned
    }
}

/// `<backups root>/<name>`, or with " (2)", " (3)"... if that is another device's folder or
/// already holds something that isn't ours.
pub fn suggested_folder(root: &Path, name: &str, known: &[KnownDevice]) -> PathBuf {
    let base = folder_name_for(name);
    for n in 1..100 {
        let leaf = if n == 1 {
            base.clone()
        } else {
            format!("{base} ({n})")
        };
        let p = root.join(&leaf);
        if folder_note(&p, None, known) == FolderNote::Fine {
            return p;
        }
    }
    root.join(base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pnk_core::signals::{DeviceKind, Signals};

    #[test]
    fn folders_compare_without_case_on_windows_only() {
        assert!(same_folder(
            Path::new("/data/Photos/"),
            Path::new("/data/Photos")
        ));
        assert_eq!(
            same_folder(Path::new("/data/Photos"), Path::new("/data/photos")),
            cfg!(windows)
        );
    }

    #[test]
    fn lists_are_split_trimmed_and_deduplicated() {
        assert_eq!(
            parse_names(" DCIM, Pictures\n\ndcim ,Movies "),
            ["DCIM", "Pictures", "Movies"]
        );
        assert!(parse_names(" , \n").is_empty());
        assert_eq!(names_text(&["a".into(), "b".into()]), "a\nb");
    }

    #[test]
    fn archive_names_follow_the_design_rules() {
        assert!(check_archive_name("_Archive", &[]).is_ok());
        for bad in ["", ".", "a/b", "NUL", "end.", "x:y", ".plug-n-keep.json"] {
            assert!(check_archive_name(bad, &[]).is_err(), "{bad:?}");
        }
        let top = vec!["dcim".to_string(), "_archive".to_string()];
        let e = check_archive_name("DCIM", &top).unwrap_err();
        assert!(e.contains("already has a folder"), "{e}");
        assert!(check_archive_name("_Archive", &top).is_err());
        assert!(check_archive_name("Old stuff", &top).is_ok());
    }

    #[test]
    fn extensions_are_cleaned_and_odd_ones_refused() {
        assert_eq!(
            parse_extensions("jpg, .PNG, *.mov, JPG").unwrap(),
            ["jpg", "png", "mov"]
        );
        assert!(parse_extensions("j/pg").is_err());
        assert!(parse_extensions("waytoolongextension").is_err());
        assert!(parse_extensions("").unwrap().is_empty());
    }

    #[test]
    fn settings_problems_name_the_setting() {
        let mut s = DeviceSettings::defaults_for(DeviceKind::Ptp);
        assert!(check_settings(&s, &[]).is_empty());
        s.archive_name = "a/b".into();
        s.folders.push("x:y".into());
        s.ignore_names.push("".into());
        s.media_extensions.clear();
        let which: Vec<&str> = check_settings(&s, &[]).iter().map(|p| p.0).collect();
        assert_eq!(
            which,
            ["archive name", "folders", "ignore list", "file types"]
        );
    }

    #[test]
    fn a_backup_folder_must_be_a_full_path() {
        assert!(check_backup_folder("").is_err());
        assert!(check_backup_folder("relative/path").is_err());
        let full = std::env::temp_dir();
        assert_eq!(
            check_backup_folder(&format!(" {} ", full.display())).unwrap(),
            full
        );
    }

    #[test]
    fn folder_names_are_made_safe() {
        assert_eq!(folder_name_for("Test iPhone"), "Test iPhone");
        assert_eq!(folder_name_for("a/b:c*"), "a_b_c_");
        assert_eq!(folder_name_for("  dots.. "), "dots");
        assert_eq!(folder_name_for(".."), "Device");
        assert_eq!(folder_name_for("CON"), "Device");
    }

    fn marker_for(dir: &Path, id: &str, name: &str) {
        let m = owner::FolderOwner::new(
            id,
            name,
            Signals::default(),
            pnk_core::signals::View::Files,
            "2026-01-01T00:00:00Z",
        );
        owner::write(&std::fs::canonicalize(dir).unwrap(), &m).unwrap();
    }

    #[test]
    fn the_card_warns_about_another_devices_folder_and_about_files() {
        let t = tempfile::tempdir().unwrap();
        let (mine, other, filled, empty) = (
            t.path().join("mine"),
            t.path().join("other"),
            t.path().join("filled"),
            t.path().join("empty"),
        );
        for d in [&mine, &other, &filled, &empty] {
            std::fs::create_dir(d).unwrap();
        }
        marker_for(&mine, "me", "Me");
        marker_for(&other, "them", "Their stick");
        std::fs::write(filled.join("a.txt"), b"x").unwrap();

        assert_eq!(folder_note(&mine, Some("me"), &[]), FolderNote::Fine);
        assert_eq!(
            folder_note(&other, Some("me"), &[]),
            FolderNote::OtherDevice("Their stick".into())
        );
        assert_eq!(folder_note(&filled, None, &[]), FolderNote::HoldsFiles(1));
        assert_eq!(folder_note(&empty, None, &[]), FolderNote::Fine);
        assert_eq!(
            folder_note(&t.path().join("missing"), None, &[]),
            FolderNote::Fine
        );
        assert!(FolderNote::OtherDevice("x".into()).blocks());
        assert!(!FolderNote::HoldsFiles(3).blocks());
        assert!(FolderNote::Fine.text().is_none());
        let one = FolderNote::HoldsFiles(1).text().unwrap();
        assert!(one.contains("1 item. It is kept"), "{one}");
        let many = FolderNote::HoldsFiles(3).text().unwrap();
        assert!(many.contains("3 items. They are kept"), "{many}");

        let mut d = KnownDevice::new(DeviceKind::Storage, "Known", &Signals::default(), "now");
        d.backup_dir = Some(empty.clone());
        assert_eq!(
            folder_note(&empty, None, std::slice::from_ref(&d)),
            FolderNote::KnownElsewhere("Known".into())
        );
        assert_eq!(
            folder_note(&empty, Some(&d.id), std::slice::from_ref(&d)),
            FolderNote::Fine
        );
    }

    #[test]
    fn a_suggested_folder_skips_taken_ones() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(
            suggested_folder(t.path(), "My phone", &[]),
            t.path().join("My phone")
        );
        let taken = t.path().join("My phone");
        std::fs::create_dir(&taken).unwrap();
        marker_for(&taken, "x", "Other");
        assert_eq!(
            suggested_folder(t.path(), "My phone", &[]),
            t.path().join("My phone (2)")
        );
    }
}
