//! Turning device-supplied names into paths on the PC, safely: a hostile name must never write outside the backup folder.
//!
//! A [`SafeRelPath`] can only be made by validating every component, so code that holds one
//! knows it cannot escape the backup folder, name a device (`CON`, `NUL`, …), open an alternate
//! data stream, or collapse onto a different file through Windows' trailing-dot/space rules.
//! [`prepare_parent`] then refuses to write through links or junctions inside the backup folder.
#![forbid(unsafe_code)]

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Suffix of this app's temporary files. Device files may not use it, so a temporary file can
/// never be mistaken for, or collide with, a real one.
pub const TEMP_SUFFIX: &str = ".pnk-part";

/// Suffix of the temporary file a JPG copy of a HEIC photo is written to. It is not
/// [`TEMP_SUFFIX`] on purpose: the copy step's temp for a device file is that file's name plus
/// [`TEMP_SUFFIX`], so no name of a device file can make the two clash. Device files may not use
/// this one either.
pub const JPG_TEMP_SUFFIX: &str = ".pnk-jpg-part";

/// Whether `name` is one of this app's temporary file names (either suffix, in any case).
pub fn is_temp_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(TEMP_SUFFIX) || lower.ends_with(JPG_TEMP_SUFFIX)
}

/// Windows' limit for one path component, in UTF-16 code units.
const MAX_COMPONENT_UTF16: usize = 255;
/// More folders deep than this is not a real photo library.
pub const MAX_COMPONENTS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathError {
    Empty,
    TooDeep,
    BadComponent {
        component: String,
        reason: &'static str,
    },
}

impl fmt::Display for PathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PathError::Empty => write!(f, "empty path"),
            PathError::TooDeep => write!(f, "more than {MAX_COMPONENTS} folders deep"),
            PathError::BadComponent { component, reason } => {
                write!(f, "unsafe name {component:?}: {reason}")
            }
        }
    }
}

impl std::error::Error for PathError {}

/// A relative path whose every component is safe to create on Windows. Stored `/`-separated.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SafeRelPath(String);

impl SafeRelPath {
    /// Validate folder names plus a file name.
    pub fn from_components<S: AsRef<str>>(parts: &[S]) -> Result<Self, PathError> {
        if parts.is_empty() {
            return Err(PathError::Empty);
        }
        if parts.len() > MAX_COMPONENTS {
            return Err(PathError::TooDeep);
        }
        for p in parts {
            let p = p.as_ref();
            validate_component(p).map_err(|reason| PathError::BadComponent {
                component: p.to_string(),
                reason,
            })?;
        }
        Ok(SafeRelPath(
            parts
                .iter()
                .map(|p| p.as_ref())
                .collect::<Vec<_>>()
                .join("/"),
        ))
    }

    /// Parse a stored `/`-separated path (skip lists, records) and validate it again.
    pub fn parse(s: &str) -> Result<Self, PathError> {
        let parts: Vec<&str> = s.split('/').collect();
        Self::from_components(&parts)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }

    pub fn file_name(&self) -> &str {
        self.0.rsplit('/').next().unwrap_or(&self.0)
    }

    /// Key for comparing paths the way NTFS does (case-insensitively).
    pub fn fold_key(&self) -> String {
        self.0.to_lowercase()
    }

    /// The path under `root`. Components are pushed one by one; because each was validated
    /// (no separators, no drive, no `..`), the result is always inside `root`.
    pub fn under(&self, root: &Path) -> PathBuf {
        let mut p = root.to_path_buf();
        for c in self.components() {
            p.push(c);
        }
        p
    }
}

impl fmt::Display for SafeRelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `stem` followed by `tail`, with the stem cut short (and a few letters of a hash of the whole
/// stem put on its end, so two long names stay two names) when the result would be longer than a
/// name may be. For the names the app makes up itself (temporary copies, old copies kept aside,
/// numbered archive copies): a device file's name may already be at the limit.
pub fn fit_name(stem: &str, tail: &str) -> String {
    let room = MAX_COMPONENT_UTF16.saturating_sub(tail.encode_utf16().count());
    if stem.encode_utf16().count() <= room {
        return format!("{stem}{tail}");
    }
    let mut h: u32 = 0x811c_9dc5;
    for b in stem.bytes() {
        h = (h ^ u32::from(b)).wrapping_mul(0x0100_0193);
    }
    let mark = format!("~{h:08x}");
    let keep = room.saturating_sub(mark.len());
    let mut cut = String::new();
    let mut used = 0;
    for ch in stem.chars() {
        used += ch.len_utf16();
        if used > keep {
            break;
        }
        cut.push(ch);
    }
    format!("{cut}{mark}{tail}")
}

/// Check one file or folder name. Returns the reason it is unsafe.
pub fn validate_component(c: &str) -> Result<(), &'static str> {
    if c.is_empty() {
        return Err("empty name");
    }
    if c == "." || c == ".." {
        return Err("refers to a folder outside its own");
    }
    if c.encode_utf16().count() > MAX_COMPONENT_UTF16 {
        return Err("longer than Windows allows");
    }
    for ch in c.chars() {
        if (ch as u32) < 0x20 || ch == '\u{7f}' {
            return Err("contains a control character");
        }
        if matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') {
            return Err("contains a character Windows does not allow in names (<>:\"/\\|?*)");
        }
    }
    if c.ends_with('.') || c.ends_with(' ') {
        return Err("ends with a dot or space, which Windows silently removes");
    }
    if is_reserved_device_name(c) {
        return Err("is a reserved Windows device name");
    }
    if is_temp_name(c) {
        return Err("uses this app's temporary-file suffix");
    }
    Ok(())
}

/// CON, PRN, AUX, NUL, CONIN$, CONOUT$, COM0-9, LPT0-9 and COM/LPT with superscript 1-3, with
/// or without an extension, in any case.
fn is_reserved_device_name(c: &str) -> bool {
    let stem = c.split('.').next().unwrap_or(c).trim_end_matches(' ');
    let up = stem.to_uppercase();
    if matches!(
        up.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) {
        return true;
    }
    if up.starts_with("COM") || up.starts_with("LPT") {
        let mut rest = up[3..].chars();
        if let (Some(d), None) = (rest.next(), rest.next()) {
            return matches!(d, '0'..='9' | '¹' | '²' | '³');
        }
    }
    false
}

/// Whether metadata (from `symlink_metadata`) describes a link: a symlink, or on Windows also a
/// junction or other reparse point. Nothing here follows or writes through one.
pub fn is_link(meta: &fs::Metadata) -> bool {
    crate::platform::is_link(meta)
}

/// Create (as needed) the folders for `rel` under `root_canon`, which must be canonical
/// (`fs::canonicalize`). Refuses to pass through any link or junction, or through a file, and
/// checks that the resulting folder still resolves inside `root_canon`. Returns the folder.
pub fn prepare_parent(root_canon: &Path, rel: &SafeRelPath) -> io::Result<PathBuf> {
    let parts: Vec<&str> = rel.components().collect();
    let mut dir = root_canon.to_path_buf();
    for part in &parts[..parts.len() - 1] {
        dir.push(part);
        match fs::symlink_metadata(&dir) {
            Ok(meta) => check_plain_dir(&dir, &meta)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => match fs::create_dir(&dir) {
                // Flush the folder that holds the new one, so the new folder's name survives
                // a power cut. A failure is not an error: the folder exists.
                Ok(()) => {
                    if let Some(parent) = dir.parent()
                        && let Err(e) = crate::platform::flush_dir(parent)
                    {
                        log::warn!("Could not flush the folder {}: {e}", parent.display());
                    }
                }
                // Someone else created it in the meantime: check what it is.
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    check_plain_dir(&dir, &fs::symlink_metadata(&dir)?)?
                }
                Err(e) => return Err(e),
            },
            Err(e) => return Err(e),
        }
    }
    let resolved = fs::canonicalize(&dir)?;
    if !resolved.starts_with(root_canon) {
        return Err(io::Error::other(format!(
            "{} resolves outside the backup folder; refusing to write there",
            dir.display()
        )));
    }
    Ok(dir)
}

fn check_plain_dir(dir: &Path, meta: &fs::Metadata) -> io::Result<()> {
    if is_link(meta) {
        return Err(io::Error::other(format!(
            "{} is a link or junction; refusing to write through it",
            dir.display()
        )));
    }
    if !meta.is_dir() {
        return Err(io::Error::other(format!(
            "{} exists and is not a folder",
            dir.display()
        )));
    }
    Ok(())
}

/// The name of the original file, if `name` is a copy that was renamed aside beside the file it
/// replaced: `IMG_0001 (old 2026-11-18 09-30).jpg`, or numbered if that was taken,
/// `IMG_0001 (old 2026-11-18 09-30) (2).jpg`. The aside name is made by `sync` and recognised
/// here (by the scan, which must not treat these as files deleted from the device, and by crash
/// recovery), so the format lives in one place.
pub fn aside_original_name(name: &str) -> Option<String> {
    // The marker is `(old YYYY-MM-DD HH-MM)`; the last one is the app's own.
    let mut from = 0;
    let mut found = None;
    while let Some(i) = name[from..].find(" (old ") {
        let at = from + i;
        let rest = &name[at + " (old ".len()..];
        if let Some(after) = rest.get(16..)
            && is_stamp_to_the_minute(&rest[..16])
            && let Some(tail) = after.strip_prefix(')')
        {
            found = Some((at, tail));
        }
        from = at + 1;
    }
    let (at, tail) = found?;
    // Optionally ` (n)`, then nothing or the extension.
    let tail = match tail.strip_prefix(" (") {
        Some(t) => {
            let close = t.find(')')?;
            if close == 0 || !t[..close].bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            &t[close + 1..]
        }
        None => tail,
    };
    if !(tail.is_empty() || tail.starts_with('.')) || at == 0 {
        return None;
    }
    Some(format!("{}{}", &name[..at], tail))
}

/// `YYYY-MM-DD HH-MM`.
fn is_stamp_to_the_minute(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 16
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 | 13 => *c == b'-',
            10 => *c == b' ',
            _ => c.is_ascii_digit(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn made_up_names_are_cut_to_fit() {
        assert_eq!(fit_name("IMG_1.HEIC", ".pnk-part"), "IMG_1.HEIC.pnk-part");
        let long = format!("{}.HEIC", "a".repeat(250));
        let t = fit_name(&long, TEMP_SUFFIX);
        assert!(t.encode_utf16().count() <= MAX_COMPONENT_UTF16);
        assert!(t.ends_with(TEMP_SUFFIX));
        let other = format!("{}b.HEIC", "a".repeat(249));
        assert_ne!(t, fit_name(&other, TEMP_SUFFIX));
        let wide = "\u{1F600}".repeat(200);
        let n = fit_name(&wide, " (old 2026-11-18 09-30) (2).jpg");
        assert!(n.encode_utf16().count() <= MAX_COMPONENT_UTF16);
        assert!(n.ends_with(" (2).jpg"));
    }

    fn ok(parts: &[&str]) -> SafeRelPath {
        SafeRelPath::from_components(parts).unwrap_or_else(|e| panic!("{parts:?} rejected: {e}"))
    }

    fn rejected(parts: &[&str]) {
        assert!(
            SafeRelPath::from_components(parts).is_err(),
            "{parts:?} was accepted"
        );
    }

    #[test]
    fn normal_iphone_names_are_accepted() {
        assert_eq!(
            ok(&["DCIM", "100APPLE", "IMG_0001.HEIC"]).as_str(),
            "DCIM/100APPLE/IMG_0001.HEIC"
        );
        ok(&["DCIM", "202311__", "IMG_E0001.JPG"]);
        ok(&["DCIM", "Café", "photo 1.jpg"]);
        ok(&[".hidden"]);
        ok(&["noext"]);
        ok(&["COMPUTER.jpg"]); // starts with COM but is not a device name
        ok(&["CONSOLE.txt"]);
        ok(&["LPT10.txt"]);
    }

    // Every hostile case (traversal, drive letters, streams, reserved names) must be rejected.
    #[test]
    fn traversal_is_rejected() {
        rejected(&["DCIM", "..", "..", "Users", "victim", "report.docx"]);
        rejected(&[
            r"..\..\Users\victim\AppData\Roaming\Microsoft\Windows\Start Menu\Programs\Startup\x.bat",
        ]);
        rejected(&["DCIM", "."]);
        rejected(&["a/b"]);
        rejected(&[r"a\b"]);
    }

    #[test]
    fn absolute_drive_and_unc_are_rejected() {
        rejected(&["C:", "Windows", "Temp", "x.jpg"]);
        rejected(&["C:foo.jpg"]);
        rejected(&[r"D:\data\file.jpg"]);
        rejected(&[r"\\server\share\x.jpg"]);
        rejected(&["", "x.jpg"]);
    }

    #[test]
    fn streams_devices_and_trailing_dots_are_rejected() {
        rejected(&["DCIM", "IMG_0001.HEIC:evil"]);
        rejected(&["DCIM", "IMG_0001.HEIC::$DATA"]);
        for dev in [
            "CON", "con", "NUL.jpg", "aux.txt", "PRN", "COM1", "com9.jpg", "LPT1", "COM0", "LPT²",
            "CONIN$", "CONOUT$", "NUL .txt",
        ] {
            rejected(&["DCIM", dev]);
        }
        rejected(&["DCIM", "IMG_0002.HEIC."]);
        rejected(&["DCIM", "IMG_0002.HEIC "]);
        rejected(&["DCIM. ", "x.jpg"]);
    }

    #[test]
    fn control_characters_long_names_and_our_temp_suffix_are_rejected() {
        rejected(&["DCIM", "a\u{0}b.jpg"]);
        rejected(&["DCIM", "a\nb.jpg"]);
        rejected(&["DCIM", &"x".repeat(256)]);
        ok(&["DCIM", &"x".repeat(255)]);
        rejected(&["DCIM", "IMG_0001.HEIC.pnk-part"]);
        rejected(&["DCIM", "IMG_0001.JPG.pnk-jpg-part"]);
        rejected(&["DCIM", "IMG_0001.JPG.PNK-JPG-PART"]);
        ok(&["DCIM", "IMG_0001.JPG.making"]);
        rejected(&["DCIM", "IMG_0001.HEIC.PNK-PART"]);
        let deep: Vec<&str> = std::iter::repeat_n("d", MAX_COMPONENTS + 1).collect();
        assert_eq!(SafeRelPath::from_components(&deep), Err(PathError::TooDeep));
    }

    #[test]
    fn joining_never_leaves_the_root() {
        let root = if cfg!(windows) {
            Path::new(r"C:\Plug-n-Keep")
        } else {
            Path::new("/plug-n-keep")
        };
        let p = ok(&["DCIM", "100APPLE", "IMG_0001.HEIC"]).under(root);
        assert_eq!(p, root.join("DCIM").join("100APPLE").join("IMG_0001.HEIC"));
        assert!(std::path::absolute(&p).unwrap().starts_with(root));
    }

    #[test]
    fn parse_round_trips_and_revalidates() {
        assert_eq!(
            SafeRelPath::parse("DCIM/IMG_1.JPG").unwrap().as_str(),
            "DCIM/IMG_1.JPG"
        );
        assert!(SafeRelPath::parse("DCIM/../x").is_err());
        assert!(SafeRelPath::parse("/x").is_err());
    }

    #[test]
    fn prepare_parent_creates_folders_inside_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let dir = prepare_parent(&root, &ok(&["DCIM", "100APPLE", "x.jpg"])).unwrap();
        assert!(dir.is_dir());
        assert!(fs::canonicalize(&dir).unwrap().starts_with(&root));
    }

    #[test]
    fn prepare_parent_refuses_a_junction_inside_root() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("backup")).unwrap();
        let root = fs::canonicalize(tmp.path().join("backup")).unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir(&outside).unwrap();
        crate::testutil::make_junction(&root.join("DCIM"), &outside);
        let err = prepare_parent(&root, &ok(&["DCIM", "sub", "x.jpg"])).unwrap_err();
        assert!(err.to_string().contains("link or junction"), "{err}");
        assert!(
            !outside.join("sub").exists(),
            "a folder was created through the junction"
        );
    }

    #[test]
    fn an_aside_name_gives_back_the_original() {
        let o = aside_original_name;
        assert_eq!(
            o("IMG_1 (old 2026-11-18 09-30).jpg").as_deref(),
            Some("IMG_1.jpg")
        );
        assert_eq!(
            o("IMG_1 (old 2026-11-18 09-30) (2).jpg").as_deref(),
            Some("IMG_1.jpg")
        );
        assert_eq!(o("notes (old 2026-11-18 09-30)").as_deref(), Some("notes"));
        assert_eq!(
            o("v1.2 notes (old 2026-11-18 09-30) (12)").as_deref(),
            Some("v1.2 notes")
        );
        assert_eq!(
            o("a.tar (old 2026-11-18 09-30).gz").as_deref(),
            Some("a.tar.gz")
        );
        // Not ours.
        assert_eq!(o("IMG_1.jpg"), None);
        assert_eq!(o("IMG_1 (old).jpg"), None);
        assert_eq!(o("IMG_1 (old 2026-11-18).jpg"), None);
        assert_eq!(o("IMG_1 (old 2026-11-18 09-30) x.jpg"), None);
        assert_eq!(o("IMG_1 (old 2026-11-18 09-30) (x).jpg"), None);
        assert_eq!(o(" (old 2026-11-18 09-30).jpg"), None);
    }

    #[test]
    fn prepare_parent_refuses_a_file_in_the_way() {
        let tmp = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        fs::write(root.join("DCIM"), b"not a folder").unwrap();
        assert!(prepare_parent(&root, &ok(&["DCIM", "x.jpg"])).is_err());
    }
}
