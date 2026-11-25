//! Sending a file to the Trash on Linux and FreeBSD, by the freedesktop.org Trash
//! specification, written out here on purpose (no crate): the one thing this module must never
//! do is delete a file, and a library that "falls back" to deleting could not promise that.
//!
//! What it does, in order, for a file:
//! 1. **Pick a trash on the file's own file system.** The home trash,
//!    `$XDG_DATA_HOME/Trash` (default `~/.local/share/Trash`), if that is on the same file
//!    system (`st_dev`) as the file. Otherwise the trash on the file's own mount:
//!    `$topdir/.Trash/$uid` if `$topdir/.Trash` is a real folder (not a link) with the sticky
//!    bit, else `$topdir/.Trash-$uid`, made with mode 0700 and checked to be ours and not a link.
//! 2. **Write the `.trashinfo` first**, with `create_new`, so a name is claimed before the file
//!    moves and an existing entry is never overwritten. Then move the file into `files/` with
//!    the no-replace rename. If the move fails, the `.trashinfo` is removed again.
//!
//! It never copies, and it never deletes the file: a rename within one file system either
//! happens or does not. If no trash on the file's file system works, the file stays where it
//! is and the caller is told why.
//!
//! The only `remove_file` here takes away the `.trashinfo` this very call just created.
//!
//! **The mount** is found from the system's own mount table (`/proc/self/mountinfo` on Linux,
//! `getmntinfo` on FreeBSD), taking the longest mount point that holds the file. Walking up while
//! `st_dev` stays the same gives the wrong answer for btrfs and ZFS sub-volumes and bind mounts,
//! which have their own `st_dev` without being mounted there. Only if the table can't be read is
//! that walk used. If `.Trash/$uid` can't be made or isn't safe, `.Trash-$uid` is used instead,
//! as the specification allows.
//!
//! **A limit, on purpose.** The folders are checked (real folder, not a link, ours) and then used
//! by path, not through open folder handles (`openat` with `O_NOFOLLOW`, `renameat2` relative to
//! them). Someone who can swap a folder between the check and the use could point the move at
//! another folder, on the same file system. The move is still the no-replace rename, so nothing
//! can be overwritten or deleted, and the file is only ever renamed within its own file system.
//! Closing that window would mean a second, per-system set of fd-relative calls (FreeBSD has no
//! `renameat2`); it is left out, for a desktop where only the user and root write these folders.

use std::ffi::{OsStr, OsString};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use log::{debug, warn};

use super::{local_stamp, rename_no_replace};

/// The longest file name, in bytes, on the systems this runs on (the `.trashinfo` note is named
/// like the file, plus `.trashinfo`).
const NAME_MAX: usize = 255;

/// How many numbered names to try in one trash before giving up.
const MAX_NAMES: u32 = 1000;

type DevFn = Box<dyn Fn(&Path) -> io::Result<u64>>;
type TopFn = Box<dyn Fn(&Path) -> io::Result<PathBuf>>;

/// What a send needs to know about the system. [`Env::system`] is the real one; tests make their
/// own, with made-up file systems.
pub(crate) struct Env {
    /// `$XDG_DATA_HOME` if it is set to an absolute path, else `~/.local/share`.
    pub data_home: Option<PathBuf>,
    pub uid: u32,
    /// `DeletionDate` text: local time, `YYYY-MM-DDThh:mm:ss`.
    pub now: String,
    /// The file system a path is on (`st_dev`); the path exists.
    pub dev_of: DevFn,
    /// The mount point of the file system `path` is on.
    pub topdir_of: TopFn,
}

impl Env {
    pub fn system() -> Self {
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| {
                std::env::var_os("HOME")
                    .filter(|h| !h.is_empty())
                    .map(|h| PathBuf::from(h).join(".local/share"))
            });
        Self {
            data_home,
            // SAFETY: geteuid has no arguments and cannot fail.
            uid: unsafe { libc::geteuid() },
            now: deletion_date(&local_stamp()),
            dev_of: Box::new(|p| Ok(fs::symlink_metadata(p)?.dev())),
            topdir_of: Box::new(real_topdir),
        }
    }
}

/// `2026-11-16 15-30-05` as `2026-11-16T15:30:05`.
fn deletion_date(stamp: &str) -> String {
    match stamp.split_once(' ') {
        Some((date, time)) => format!("{date}T{}", time.replace('-', ":")),
        None => stamp.to_string(),
    }
}

/// The mount point of the file system `path` is on: the longest mount point in the system's
/// mount table that holds it (the folder's links resolved first). If the table can't be read, the
/// highest folder above it with the same device.
fn real_topdir(path: &Path) -> io::Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::other("it has no name"))?;
    let folder = path.parent().unwrap_or(Path::new("/"));
    let real = fs::canonicalize(folder)?.join(name);
    if let Some(points) = system_mount_points()
        && let Some(top) = longest_mount(&real, &points)
    {
        return Ok(top.to_path_buf());
    }
    debug!("The mount table could not be used; finding the mount point by device");
    walk_topdir(path)
}

/// The longest of `mounts` that is `path` or a folder above it.
fn longest_mount<'a>(path: &Path, mounts: &'a [PathBuf]) -> Option<&'a Path> {
    mounts
        .iter()
        .filter(|m| path.starts_with(m))
        .max_by_key(|m| m.components().count())
        .map(PathBuf::as_path)
}

/// The mount points in `/proc/self/mountinfo` text: the fifth field, with `\040`-style escapes.
#[cfg(any(target_os = "linux", test))]
fn mountinfo_points(text: &str) -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    text.lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(|p| PathBuf::from(OsString::from_vec(unescape_octal(p))))
        .collect()
}

/// `\040` and the like: the kernel writes a space, tab, newline or backslash as three octal digits.
#[cfg(any(target_os = "linux", test))]
fn unescape_octal(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..=i + 3].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let v = u32::from(b[i + 1] - b'0') * 64
                + u32::from(b[i + 2] - b'0') * 8
                + u32::from(b[i + 3] - b'0');
            if let Ok(byte) = u8::try_from(v) {
                out.push(byte);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

#[cfg(target_os = "linux")]
fn system_mount_points() -> Option<Vec<PathBuf>> {
    let text = fs::read_to_string("/proc/self/mountinfo").ok()?;
    let points = mountinfo_points(&text);
    (!points.is_empty()).then_some(points)
}

#[cfg(target_os = "freebsd")]
fn system_mount_points() -> Option<Vec<PathBuf>> {
    let mut list: *mut libc::statfs = std::ptr::null_mut();
    // SAFETY: getmntinfo points `list` at a buffer it owns (valid until the next call; never
    // freed by us) and returns how many entries it holds, or 0 on failure.
    let n = unsafe { libc::getmntinfo(&mut list, libc::MNT_NOWAIT) };
    if n <= 0 || list.is_null() {
        return None;
    }
    // SAFETY: `list` has `n` entries, per the call above.
    let entries = unsafe { std::slice::from_raw_parts(list, n as usize) };
    Some(
        entries
            .iter()
            .map(|e| {
                // SAFETY: the name is a NUL-terminated array inside the entry.
                let name = unsafe { std::ffi::CStr::from_ptr(e.f_mntonname.as_ptr()) };
                PathBuf::from(OsStr::from_bytes(name.to_bytes()))
            })
            .collect(),
    )
}

/// The fall-back: the highest folder above `path` with the same device.
fn walk_topdir(path: &Path) -> io::Result<PathBuf> {
    let dev = fs::symlink_metadata(path)?.dev();
    let mut top = path.parent().unwrap_or(Path::new("/")).to_path_buf();
    while let Some(up) = top.parent() {
        match fs::symlink_metadata(up) {
            Ok(m) if m.dev() == dev => top = up.to_path_buf(),
            _ => break,
        }
    }
    Ok(top)
}

/// Move `file` into the Trash of its own file system. `file` must be an absolute path.
pub fn send(file: &Path) -> Result<(), String> {
    send_with(file, &Env::system())
}

pub(crate) fn send_with(file: &Path, env: &Env) -> Result<(), String> {
    if !file.is_absolute() {
        return Err("the path is not absolute".into());
    }
    let meta = fs::symlink_metadata(file).map_err(|e| format!("could not look at it: {e}"))?;
    if !meta.is_file() {
        return Err("it is not a plain file".into());
    }
    let name = file.file_name().ok_or("it has no name")?.to_os_string();
    let dev = (env.dev_of)(file)
        .map_err(|e| format!("could not tell which file system it is on: {e}"))?;
    // Each trash that could be used, in order, with why one was not.
    let mut why_not: Vec<String> = Vec::new();
    match home_trash(env, dev) {
        Ok(Some(trash)) => {
            // An absolute path is allowed in the home trash.
            match put(&trash, &name, file, file.as_os_str(), &env.now) {
                Ok(()) => return Ok(()),
                Err(e) => why_not.push(format!("the home Trash: {e}")),
            }
        }
        Ok(None) => debug!(
            "The home Trash is on another file system than {}",
            file.display()
        ),
        Err(e) => why_not.push(format!("the home Trash: {e}")),
    }
    // Not in the home trash (another file system, or it failed): the trash on the file's own mount.
    match topdir_trash(env, file) {
        Ok((trash, rel)) => match put(&trash, &name, file, rel.as_os_str(), &env.now) {
            Ok(()) => return Ok(()),
            Err(e) => why_not.push(format!("the Trash on its own drive: {e}")),
        },
        Err(e) => why_not.push(format!("the Trash on its own drive: {e}")),
    }
    Err(format!(
        "no Trash on the file's own file system could be used ({})",
        why_not.join("; ")
    ))
}

/// The home trash folder, made if need be, if it is on file system `dev`. `Ok(None)` if it is
/// on another one. Nothing is made on a file system the file isn't on.
fn home_trash(env: &Env, dev: u64) -> Result<Option<PathBuf>, String> {
    let Some(data) = &env.data_home else {
        return Err("no home folder is known".into());
    };
    let trash = data.join("Trash");
    // The nearest folder that exists decides which file system the trash would be on.
    let mut probe = trash.as_path();
    let probe_dev = loop {
        match (env.dev_of)(probe) {
            Ok(d) => break d,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                probe = probe.parent().ok_or("no folder above it exists")?;
            }
            Err(e) => return Err(format!("could not check {}: {e}", probe.display())),
        }
    };
    if probe_dev != dev {
        return Ok(None);
    }
    make_dir_all(&trash)?;
    ensure_trash_dirs(&trash, env.uid, false)?;
    Ok(Some(trash))
}

/// The trash on the file's own mount, and the file's path relative to the mount (what the
/// `.trashinfo` records for it).
fn topdir_trash(env: &Env, file: &Path) -> Result<(PathBuf, PathBuf), String> {
    let top = (env.topdir_of)(file).map_err(|e| format!("could not find its mount point: {e}"))?;
    // The mount point is a real path (links resolved), so the file's must be too: a file reached
    // through a linked folder is not under it by its own spelling.
    let real = file
        .parent()
        .and_then(|p| fs::canonicalize(p).ok())
        .zip(file.file_name())
        .map(|(p, n)| p.join(n));
    let rel = real
        .as_deref()
        .and_then(|r| r.strip_prefix(&top).ok())
        .or_else(|| file.strip_prefix(&top).ok())
        .ok_or_else(|| "its mount point is not above it".to_string())?
        .to_path_buf();
    let shared = top.join(".Trash");
    let uid = env.uid;
    // The shared one, only if it is a real folder with the sticky bit, and only if our own
    // folder in it can be made and checked; otherwise the per-user one beside it.
    let shared_usable = match fs::symlink_metadata(&shared) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() && m.mode() & 0o1000 != 0 => true,
        Ok(_) => {
            warn!(
                "{} exists but is not a real folder with the sticky bit; using .Trash-{uid} instead",
                shared.display()
            );
            false
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => {
            warn!(
                "could not check {} ({e}); using .Trash-{uid} instead",
                shared.display()
            );
            false
        }
    };
    if shared_usable {
        let mine = shared.join(uid.to_string());
        match make_private_dir(&mine).and_then(|()| ensure_trash_dirs(&mine, uid, true)) {
            Ok(()) => return Ok((mine, rel)),
            Err(e) => warn!("{e}; using .Trash-{uid} instead"),
        }
    }
    let own = top.join(format!(".Trash-{uid}"));
    make_private_dir(&own)?;
    ensure_trash_dirs(&own, uid, true)?;
    Ok((own, rel))
}

/// A folder that is ours: made with mode 0700 if missing, then checked to be a real folder
/// (not a link) owned by this user.
fn make_private_dir(dir: &Path) -> Result<(), String> {
    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("could not make {}: {e}", dir.display())),
    }
    Ok(())
}

fn make_dir_all(dir: &Path) -> Result<(), String> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("could not make {}: {e}", dir.display()))
}

/// `files` and `info` inside `trash`, made if missing. With `check_owner`, the trash and both
/// must be real folders owned by `uid` (a top-dir trash sits on a shared drive).
fn ensure_trash_dirs(trash: &Path, uid: u32, check_owner: bool) -> Result<(), String> {
    for dir in [trash.to_path_buf(), trash.join("files"), trash.join("info")] {
        if dir != trash {
            make_private_dir(&dir)?;
        }
        let m = fs::symlink_metadata(&dir)
            .map_err(|e| format!("could not check {}: {e}", dir.display()))?;
        let m = if dir == trash && m.file_type().is_symlink() && !check_owner {
            // The user's own `~/.local/share/Trash` may be a link (a trash kept on another
            // disk): fine when it leads to a folder this user owns. The `files` and `info`
            // folders in it must still be real ones.
            let real = fs::metadata(&dir)
                .map_err(|e| format!("could not check {}: {e}", dir.display()))?;
            if !real.is_dir() || real.uid() != uid {
                return Err(format!(
                    "{} is a link to something that is not a folder owned by this user",
                    dir.display()
                ));
            }
            continue;
        } else {
            m
        };
        if m.file_type().is_symlink() || !m.is_dir() {
            return Err(format!("{} is not a real folder", dir.display()));
        }
        if check_owner && m.uid() != uid {
            return Err(format!("{} is not owned by this user", dir.display()));
        }
    }
    Ok(())
}

/// Write the `.trashinfo`, then move `file` into `files/`. `recorded` is the path written in the
/// `.trashinfo`.
fn put(trash: &Path, name: &OsStr, file: &Path, recorded: &OsStr, now: &str) -> Result<(), String> {
    let (files, info) = (trash.join("files"), trash.join("info"));
    let body = format!(
        "[Trash Info]\nPath={}\nDeletionDate={now}\n",
        percent_encode(recorded)
    );
    let mut last = String::from("no free name");
    for n in 1..=MAX_NAMES {
        let candidate = numbered_name(name, n);
        let target = files.join(&candidate);
        // A name is free only if neither the file nor its info exists.
        match fs::symlink_metadata(&target) {
            Ok(_) => continue,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("could not check {}: {e}", target.display())),
        }
        let mut info_name = candidate.clone();
        info_name.push(".trashinfo");
        if info_name.len() > NAME_MAX {
            return Err(format!(
                "its name is too long for the Trash: the note kept beside it needs {} more characters than a file name can have",
                info_name.len() - NAME_MAX
            ));
        }
        let info_path = info.join(info_name);
        let mut f = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&info_path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("could not write {}: {e}", info_path.display())),
        };
        if let Err(e) = f.write_all(body.as_bytes()).and_then(|()| f.sync_all()) {
            // Our own half-written note, which nothing points at yet.
            let _ = fs::remove_file(&info_path);
            return Err(format!("could not write {}: {e}", info_path.display()));
        }
        drop(f);
        match rename_no_replace(file, &target) {
            Ok(()) => return Ok(()),
            Err(e) => {
                // The move did not happen; the note this call made must not outlive it.
                let _ = fs::remove_file(&info_path);
                if e.kind() == io::ErrorKind::AlreadyExists {
                    last = format!("{} appeared", target.display());
                    continue;
                }
                return Err(format!("could not move it into {}: {e}", files.display()));
            }
        }
    }
    Err(last)
}

/// `name`, or `name (n)` before the extension for `n > 1`.
fn numbered_name(name: &OsStr, n: u32) -> OsString {
    if n == 1 {
        return name.to_os_string();
    }
    let bytes = name.as_bytes();
    let dot = bytes.iter().rposition(|&b| b == b'.').filter(|&i| i > 0);
    let mut out = Vec::new();
    match dot {
        Some(i) => {
            out.extend_from_slice(&bytes[..i]);
            out.extend_from_slice(format!(" ({n})").as_bytes());
            out.extend_from_slice(&bytes[i..]);
        }
        None => {
            out.extend_from_slice(bytes);
            out.extend_from_slice(format!(" ({n})").as_bytes());
        }
    }
    OsString::from(OsStr::from_bytes(&out))
}

/// The `Path=` value: every byte but the unreserved ones and `/` as `%XX`.
fn percent_encode(path: &OsStr) -> String {
    let mut out = String::new();
    for &b in path.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;

    const NOW: &str = "2026-11-18T09:30:12";

    /// An `Env` whose file systems are made up: a path under `pretend_other` is on device 2,
    /// anything else on device 1. `top` is the mount point reported for files under it.
    fn env(data_home: &Path, pretend_other: Option<&Path>, top: Option<&Path>) -> Env {
        let other = pretend_other.map(Path::to_path_buf);
        let top = top.map(Path::to_path_buf);
        Env {
            data_home: Some(data_home.to_path_buf()),
            uid: unsafe { libc::geteuid() },
            now: NOW.into(),
            dev_of: Box::new(move |p| {
                fs::symlink_metadata(p).map(|_| match &other {
                    Some(o) if p.starts_with(o) => 2,
                    _ => 1,
                })
            }),
            topdir_of: Box::new(move |p| match &top {
                Some(t) if p.starts_with(t) => Ok(t.clone()),
                _ => Err(io::Error::other("no mount point in this test")),
            }),
        }
    }

    fn read_info(trash: &Path, name: &str) -> HashMap<String, String> {
        let text =
            fs::read_to_string(trash.join("info").join(format!("{name}.trashinfo"))).unwrap();
        assert!(text.starts_with("[Trash Info]\n"), "{text}");
        text.lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn a_file_on_the_home_file_system_goes_to_the_home_trash_with_its_note() {
        let t = tempfile::tempdir().unwrap();
        let data = t.path().join("share");
        let file = t.path().join("a b.txt");
        fs::write(&file, b"x").unwrap();
        send_with(&file, &env(&data, None, None)).unwrap();
        assert!(!file.exists());
        let trash = data.join("Trash");
        assert_eq!(fs::read(trash.join("files/a b.txt")).unwrap(), b"x");
        let info = read_info(&trash, "a b.txt");
        assert_eq!(
            info["Path"],
            format!("{}/a%20b.txt", t.path().display()).replace("//", "/")
        );
        assert_eq!(info["DeletionDate"], NOW);
    }

    #[test]
    fn a_name_taken_in_files_is_skipped_and_gets_no_note() {
        let t = tempfile::tempdir().unwrap();
        let data = t.path().join("share");
        let trash = data.join("Trash");
        let file = t.path().join("f");
        fs::write(&file, b"x").unwrap();
        fs::create_dir_all(trash.join("files")).unwrap();
        fs::create_dir_all(trash.join("info")).unwrap();
        let blocker = trash.join("files").join("f");
        fs::create_dir(&blocker).unwrap();
        fs::write(blocker.join("inner"), b"keep").unwrap();
        send_with(&file, &env(&data, None, None)).unwrap();
        assert!(trash.join("files/f (2)").exists());
        assert!(trash.join("info/f (2).trashinfo").exists());
        assert!(
            !trash.join("info/f.trashinfo").exists(),
            "no note for a taken name"
        );
        assert_eq!(fs::read(blocker.join("inner")).unwrap(), b"keep");
    }

    #[test]
    fn the_note_is_there_before_the_move_and_goes_when_the_move_fails() {
        let t = tempfile::tempdir().unwrap();
        let data = t.path().join("share");
        let trash = data.join("Trash");
        let file = t.path().join("f");
        fs::write(&file, b"x").unwrap();
        // The file vanishes after the note could be written but before it moves: the move
        // fails, and the note must not be left behind.
        let victim = file.clone();
        let mut e = env(&data, None, None);
        let inner = e.dev_of;
        e.dev_of = Box::new(move |p| {
            let r = inner(p);
            if p.ends_with("share") || p.ends_with("Trash") {
                let _ = fs::remove_file(&victim);
            }
            r
        });
        let info = trash.join("info");
        let result = send_with(&file, &e);
        assert!(result.is_err(), "{result:?}");
        assert_eq!(
            fs::read_dir(&info).map(|d| d.count()).unwrap_or(0),
            0,
            "no note outlives a move that did not happen"
        );
    }

    #[test]
    fn a_name_that_is_taken_gets_a_number_and_nothing_is_overwritten() {
        let t = tempfile::tempdir().unwrap();
        let data = t.path().join("share");
        let e = env(&data, None, None);
        for body in [b"one".as_slice(), b"two", b"three"] {
            let file = t.path().join("IMG.jpg");
            fs::write(&file, body).unwrap();
            send_with(&file, &e).unwrap();
        }
        let files = data.join("Trash/files");
        assert_eq!(fs::read(files.join("IMG.jpg")).unwrap(), b"one");
        assert_eq!(fs::read(files.join("IMG (2).jpg")).unwrap(), b"two");
        assert_eq!(fs::read(files.join("IMG (3).jpg")).unwrap(), b"three");
        for n in ["IMG.jpg", "IMG (2).jpg", "IMG (3).jpg"] {
            read_info(&data.join("Trash"), n);
        }
    }

    #[test]
    fn an_existing_note_is_never_overwritten() {
        let t = tempfile::tempdir().unwrap();
        let data = t.path().join("share");
        let trash = data.join("Trash");
        fs::create_dir_all(trash.join("info")).unwrap();
        fs::create_dir_all(trash.join("files")).unwrap();
        fs::write(trash.join("info/f.trashinfo"), b"someone else's").unwrap();
        let file = t.path().join("f");
        fs::write(&file, b"x").unwrap();
        send_with(&file, &env(&data, None, None)).unwrap();
        assert_eq!(
            fs::read(trash.join("info/f.trashinfo")).unwrap(),
            b"someone else's"
        );
        assert!(trash.join("files/f (2)").exists());
    }

    #[test]
    fn xdg_data_home_is_used_when_it_is_set() {
        // `Env::system` reads it; check the rule on the values.
        let before = std::env::var_os("XDG_DATA_HOME");
        let t = tempfile::tempdir().unwrap();
        // SAFETY: tests in this module that read the variable run in this one test.
        unsafe { std::env::set_var("XDG_DATA_HOME", t.path()) };
        let e = Env::system();
        assert_eq!(e.data_home.as_deref(), Some(t.path()));
        unsafe { std::env::set_var("XDG_DATA_HOME", "relative/dir") };
        let home = std::env::var_os("HOME");
        let e = Env::system();
        // A relative value is ignored, as the XDG rules say.
        assert_eq!(
            e.data_home,
            home.map(|h| PathBuf::from(h).join(".local/share"))
        );
        match before {
            Some(v) => unsafe { std::env::set_var("XDG_DATA_HOME", v) },
            None => unsafe { std::env::remove_var("XDG_DATA_HOME") },
        }
    }

    #[test]
    fn a_file_on_another_file_system_uses_the_trash_on_its_own_mount() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home/share");
        fs::create_dir_all(&home).unwrap();
        let mount = t.path().join("usb");
        fs::create_dir_all(mount.join("DCIM")).unwrap();
        let file = mount.join("DCIM/a.jpg");
        fs::write(&file, b"x").unwrap();
        let e = env(&home, Some(&mount), Some(&mount));
        send_with(&file, &e).unwrap();
        assert!(!file.exists());
        let trash = mount.join(format!(".Trash-{}", e.uid));
        assert_eq!(fs::read(trash.join("files/a.jpg")).unwrap(), b"x");
        let info = read_info(&trash, "a.jpg");
        assert_eq!(info["Path"], "DCIM/a.jpg", "relative to the mount");
        assert_eq!(
            fs::metadata(&trash).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(
            !home.join("Trash").exists(),
            "nothing is made on the home file system"
        );
    }

    #[test]
    fn the_shared_trash_folder_is_used_only_if_real_and_sticky() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home/share");
        fs::create_dir_all(&home).unwrap();
        let mount = t.path().join("usb");
        fs::create_dir_all(&mount).unwrap();
        let shared = mount.join(".Trash");
        fs::create_dir(&shared).unwrap();
        let file = mount.join("a.txt");
        fs::write(&file, b"x").unwrap();
        let e = env(&home, Some(&mount), Some(&mount));
        // Not sticky: refused, and the per-user folder is used.
        send_with(&file, &e).unwrap();
        assert!(mount.join(format!(".Trash-{}/files/a.txt", e.uid)).exists());
        assert!(!shared.join(e.uid.to_string()).exists());
        // Sticky: used.
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o1777)).unwrap();
        let file2 = mount.join("b.txt");
        fs::write(&file2, b"y").unwrap();
        send_with(&file2, &e).unwrap();
        assert_eq!(
            fs::read(shared.join(format!("{}/files/b.txt", e.uid))).unwrap(),
            b"y"
        );
    }

    #[test]
    fn a_shared_trash_that_is_a_link_is_not_followed() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home/share");
        fs::create_dir_all(&home).unwrap();
        let mount = t.path().join("usb");
        let elsewhere = t.path().join("elsewhere");
        fs::create_dir_all(&mount).unwrap();
        fs::create_dir(&elsewhere).unwrap();
        fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o1777)).unwrap();
        std::os::unix::fs::symlink(&elsewhere, mount.join(".Trash")).unwrap();
        let file = mount.join("a.txt");
        fs::write(&file, b"x").unwrap();
        let e = env(&home, Some(&mount), Some(&mount));
        send_with(&file, &e).unwrap();
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
        assert!(mount.join(format!(".Trash-{}/files/a.txt", e.uid)).exists());
    }

    #[test]
    fn a_per_user_trash_that_is_a_link_is_refused_and_the_file_stays() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home/share");
        fs::create_dir_all(&home).unwrap();
        let mount = t.path().join("usb");
        let elsewhere = t.path().join("elsewhere");
        fs::create_dir_all(&mount).unwrap();
        fs::create_dir(&elsewhere).unwrap();
        let e = env(&home, Some(&mount), Some(&mount));
        std::os::unix::fs::symlink(&elsewhere, mount.join(format!(".Trash-{}", e.uid))).unwrap();
        let file = mount.join("a.txt");
        fs::write(&file, b"x").unwrap();
        let err = send_with(&file, &e).unwrap_err();
        assert!(err.contains("no Trash"), "{err}");
        assert_eq!(fs::read(&file).unwrap(), b"x");
        assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
    }

    #[test]
    fn a_file_reached_through_a_linked_folder_is_recorded_relative_to_the_real_mount() {
        let t = tempfile::tempdir().unwrap();
        let t_real = fs::canonicalize(t.path()).unwrap();
        let home = t_real.join("home/share");
        fs::create_dir_all(&home).unwrap();
        let mount = t_real.join("usb");
        fs::create_dir_all(mount.join("DCIM")).unwrap();
        fs::write(mount.join("DCIM/a.jpg"), b"x").unwrap();
        let link = t_real.join("link");
        std::os::unix::fs::symlink(mount.join("DCIM"), &link).unwrap();
        let mut e = env(&home, Some(&mount), Some(&mount));
        let top = mount.clone();
        e.topdir_of = Box::new(move |_| Ok(top.clone()));
        // Everything but the home folder is on the pretend other file system, links or not.
        let home_dir = home.clone();
        e.dev_of = Box::new(move |p| {
            fs::symlink_metadata(p).map(|_| if p.starts_with(&home_dir) { 1 } else { 2 })
        });
        send_with(&link.join("a.jpg"), &e).unwrap();
        assert!(!mount.join("DCIM/a.jpg").exists());
        let trash = mount.join(format!(".Trash-{}", e.uid));
        assert_eq!(read_info(&trash, "a.jpg")["Path"], "DCIM/a.jpg");
    }

    #[test]
    fn a_home_trash_that_is_a_link_to_our_own_folder_is_used() {
        let t = tempfile::tempdir().unwrap();
        let data = t.path().join("share");
        let elsewhere = t.path().join("elsewhere");
        fs::create_dir_all(&data).unwrap();
        fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, data.join("Trash")).unwrap();
        let file = t.path().join("a.txt");
        fs::write(&file, b"x").unwrap();
        send_with(&file, &env(&data, None, None)).unwrap();
        assert_eq!(fs::read(elsewhere.join("files/a.txt")).unwrap(), b"x");
        // Its `files` folder may not be a link.
        let file = t.path().join("b.txt");
        fs::write(&file, b"y").unwrap();
        fs::remove_dir_all(elsewhere.join("files")).unwrap();
        let other = t.path().join("other");
        fs::create_dir(&other).unwrap();
        std::os::unix::fs::symlink(&other, elsewhere.join("files")).unwrap();
        assert!(send_with(&file, &env(&data, None, None)).is_err());
        assert_eq!(fs::read(&file).unwrap(), b"y");
    }

    #[test]
    fn with_no_usable_trash_on_the_files_file_system_it_stays_and_is_never_copied() {
        // The file is on another (pretend) file system, and its mount can't hold a trash (the
        // mount is read-only to us). The home trash is on the wrong file system, so it must
        // not be used even though it would work.
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home/share");
        fs::create_dir_all(&home).unwrap();
        let mount = t.path().join("usb");
        fs::create_dir_all(&mount).unwrap();
        let file = mount.join("a.txt");
        fs::write(&file, b"x").unwrap();
        fs::set_permissions(&mount, fs::Permissions::from_mode(0o500)).unwrap();
        let e = env(&home, Some(&mount), Some(&mount));
        let blocked = unsafe { libc::geteuid() } != 0;
        let result = send_with(&file, &e);
        fs::set_permissions(&mount, fs::Permissions::from_mode(0o700)).unwrap();
        if blocked {
            let err = result.unwrap_err();
            assert!(err.contains("no Trash"), "{err}");
            assert_eq!(fs::read(&file).unwrap(), b"x", "left in place");
            assert!(
                !home.join("Trash").exists(),
                "the home trash was not made or used"
            );
        }
    }

    #[test]
    fn a_real_cross_file_system_rename_is_refused_not_copied() {
        // Where /dev/shm is a different file system from the temp folder, a file moved there by
        // the no-replace rename must fail with the move refused and the file untouched.
        let shm = Path::new("/dev/shm");
        let t = tempfile::tempdir().unwrap();
        let file = t.path().join("f");
        fs::write(&file, b"x").unwrap();
        let same =
            fs::metadata(shm).map(|m| m.dev()).ok() == fs::metadata(t.path()).map(|m| m.dev()).ok();
        if !shm.is_dir() || same {
            return;
        }
        let target = shm.join(format!("pnk-test-{}", std::process::id()));
        assert!(rename_no_replace(&file, &target).is_err());
        assert_eq!(fs::read(&file).unwrap(), b"x");
        assert!(!target.exists());
    }

    #[test]
    fn the_path_is_percent_encoded_and_the_date_is_local_iso() {
        assert_eq!(
            percent_encode(OsStr::new("/a b/é#.txt")),
            "/a%20b/%C3%A9%23.txt"
        );
        assert_eq!(deletion_date("2026-11-16 15-30-05"), "2026-11-16T15:30:05");
    }

    #[test]
    fn numbering_goes_before_the_extension() {
        assert_eq!(
            numbered_name(OsStr::new("a.jpg"), 2),
            OsString::from("a (2).jpg")
        );
        assert_eq!(numbered_name(OsStr::new("a"), 3), OsString::from("a (3)"));
        assert_eq!(
            numbered_name(OsStr::new(".bashrc"), 2),
            OsString::from(".bashrc (2)")
        );
    }

    #[test]
    fn the_longest_mount_point_that_holds_the_file_wins() {
        let mounts: Vec<PathBuf> = ["/", "/home", "/home/me/data", "/mnt/usb", "/mnt"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let top = |p: &str| longest_mount(Path::new(p), &mounts).map(Path::to_path_buf);
        assert_eq!(top("/etc/x"), Some(PathBuf::from("/")));
        assert_eq!(top("/home/me/a.txt"), Some(PathBuf::from("/home")));
        // A bind mount or sub-volume mounted deeper is found; the same st_dev walk would not.
        assert_eq!(
            top("/home/me/data/a.txt"),
            Some(PathBuf::from("/home/me/data"))
        );
        assert_eq!(top("/mnt/usb/DCIM/a.jpg"), Some(PathBuf::from("/mnt/usb")));
        // "/mnt/usb2" is not under "/mnt/usb".
        assert_eq!(top("/mnt/usb2/a.jpg"), Some(PathBuf::from("/mnt")));
    }

    #[test]
    fn mount_points_are_read_from_mountinfo_with_escapes_undone() {
        let text = "22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw\n\
                    40 22 8:17 / /media/me/My\\040Stick rw,nosuid - vfat /dev/sdb1 rw\n\
                    41 22 0:40 /sub /srv/bind rw - btrfs /dev/sdc rw,subvol=/sub\n\
                    junk\n";
        let points = mountinfo_points(text);
        assert!(points.contains(&PathBuf::from("/")));
        assert!(points.contains(&PathBuf::from("/media/me/My Stick")));
        assert!(points.contains(&PathBuf::from("/srv/bind")));
    }

    #[test]
    fn the_real_mount_table_gives_a_mount_that_holds_the_file() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("a");
        fs::write(&f, b"x").unwrap();
        let top = real_topdir(&f).unwrap();
        assert!(fs::canonicalize(&f).unwrap().starts_with(&top), "{top:?}");
    }

    #[test]
    fn a_shared_trash_that_cannot_be_used_falls_back_to_the_per_user_one() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home/share");
        fs::create_dir_all(&home).unwrap();
        let mount = t.path().join("usb");
        fs::create_dir_all(&mount).unwrap();
        let shared = mount.join(".Trash");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o1777)).unwrap();
        let e = env(&home, Some(&mount), Some(&mount));
        // `.Trash/$uid` is a plain file: it can't be our folder.
        fs::write(shared.join(e.uid.to_string()), b"in the way").unwrap();
        let file = mount.join("a.txt");
        fs::write(&file, b"x").unwrap();
        send_with(&file, &e).unwrap();
        assert!(mount.join(format!(".Trash-{}/files/a.txt", e.uid)).exists());
        assert!(!file.exists());
    }

    #[test]
    fn a_name_too_long_for_its_note_is_refused_clearly_and_the_file_stays() {
        let t = tempfile::tempdir().unwrap();
        let data = t.path().join("share");
        let file = t.path().join("x".repeat(250));
        fs::write(&file, b"x").unwrap();
        let err = send_with(&file, &env(&data, None, None)).unwrap_err();
        assert!(err.contains("too long for the Trash"), "{err}");
        assert!(file.exists());
        let info = data.join("Trash/info");
        assert_eq!(fs::read_dir(info).map(|d| d.count()).unwrap_or(0), 0);
    }

    #[test]
    fn a_folder_or_a_relative_path_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let e = env(&t.path().join("share"), None, None);
        assert!(send_with(t.path(), &e).is_err());
        assert!(send_with(Path::new("relative"), &e).is_err());
        assert!(t.path().exists());
    }
}
