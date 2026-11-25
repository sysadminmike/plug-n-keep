//! The POSIX pieces Linux and FreeBSD share: link detection, flushing, the single-instance
//! lock (`flock`), the settings folder, the local clock, and the two ways of moving a file
//! into place without replacing anything when the file system has no atomic way to do it.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use log::{debug, warn};

use crate::device::DeviceSource;
use crate::platform::ListedDevice;

/// Whether metadata (from `symlink_metadata`) describes a symlink.
pub fn is_link(meta: &fs::Metadata) -> bool {
    meta.file_type().is_symlink()
}

/// The device number of the file system `root` is on.
pub fn volume_token(root: &Path) -> io::Result<u64> {
    Ok(fs::metadata(root)?.dev())
}

/// Forces a file's data to disk.
pub fn flush_file(file: &File) -> io::Result<()> {
    file.sync_all()
}

/// Forces a folder's entries (a new name, a removed name) to disk.
pub fn flush_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// The folder that holds `path` (`.` for a bare name).
fn parent_dir(path: &Path) -> &Path {
    match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    }
}

/// Whether a failed folder flush just means this file system can't do it (`EINVAL`, `ENOTSUP`).
fn flush_refused(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EINVAL | libc::ENOTSUP))
}

/// Flushes a folder where the work is already done: a failure is logged, not returned. A file
/// system that can't flush folders at all is only noted at debug level; any other failure is a
/// warning, because the move may not survive a power cut.
fn flush_dir_logged(dir: &Path) {
    if let Err(e) = flush_dir(dir) {
        if flush_refused(&e) {
            debug!(
                "This file system can't flush the folder {}: {e}",
                dir.display()
            );
        } else {
            warn!(
                "Could not flush the folder {}: {e}. The move is done but may not survive a power cut",
                dir.display()
            );
        }
    }
}

/// After a move: both folders involved are flushed. The move has already happened, so a folder
/// that can't be flushed is logged, not reported as a failed move.
pub(super) fn flush_after_move(from: &Path, to: &Path) {
    for path in [from, to] {
        flush_dir_logged(parent_dir(path));
    }
}

/// Atomically replaces `to` if it exists: `rename(2)`.
pub fn rename_replace(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)?;
    flush_after_move(from, to);
    Ok(())
}

/// Whether `a` and `b` are the same file (same device and inode).
fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.dev() == b.dev() && a.ino() == b.ino()
}

/// Moves by making a second name for the file, then removing the first. A name that is already
/// taken makes the link fail, so nothing is ever replaced.
///
/// - The new name's folder is flushed before the old name goes, so a power cut can't leave the
///   file with no name when it moves between folders.
/// - If the old name is already gone (someone else removed it), the move counts as done: `to`
///   is then the only name for the data and is kept.
/// - If the old name can't be removed for another reason, the new name is taken away again so
///   the caller sees a clean failure, but only if it is still the same file as the old one was.
pub(super) fn move_by_link(from: &Path, to: &Path) -> io::Result<()> {
    move_by_link_with(from, to, |p| fs::remove_file(p))
}

/// [`move_by_link`] with the unlink step passed in, so a test can make it fail or race.
fn move_by_link_with(
    from: &Path,
    to: &Path,
    unlink: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let before = fs::symlink_metadata(from)?;
    fs::hard_link(from, to)?;
    flush_dir_logged(parent_dir(to));
    match unlink(from) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => {
            if fs::symlink_metadata(to).is_ok_and(|now| same_file(&before, &now)) {
                let _ = fs::remove_file(to);
            }
            Err(e)
        }
    }
}

/// For file systems with no hard links (FAT, exFAT): first claim the final name by creating an
/// empty file there, which fails if the name is taken, then rename the finished file over our
/// own empty one. The name is never given to anyone else's data on purpose.
///
/// - **Crash leftover.** A crash between the two steps leaves an empty file under the final
///   name. A later sync's `free_target` sees the wrong size and does not treat it as a
///   finished copy.
/// - **Small race.** `rename` replaces whatever is under the name at that moment. If someone
///   else puts a file there between the claim and the rename, it is replaced. The window is a
///   few system calls wide.
/// - **Order.** The placeholder's folder is flushed before the rename.
/// - **Failure.** If the rename fails, the placeholder is removed again, but only if it is
///   still empty and still the same file this call created.
pub(super) fn move_by_placeholder(from: &Path, to: &Path) -> io::Result<()> {
    move_by_placeholder_with(from, to, |f, t| fs::rename(f, t))
}

/// [`move_by_placeholder`] with the rename passed in, so a test can make it fail.
fn move_by_placeholder_with(
    from: &Path,
    to: &Path,
    rename: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let claimed = OpenOptions::new().write(true).create_new(true).open(to)?;
    let claimed_meta = claimed.metadata()?;
    drop(claimed);
    flush_dir_logged(parent_dir(to));
    match rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) => {
            let ours = fs::symlink_metadata(to)
                .is_ok_and(|m| m.is_file() && m.len() == 0 && same_file(&claimed_meta, &m));
            if ours {
                let _ = fs::remove_file(to);
            }
            Err(e)
        }
    }
}

/// Whether an error from `link` means "this file system has no hard links" rather than a real
/// problem such as the name being taken.
fn links_unsupported(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::EPERM | libc::ENOTSUP | libc::ENOSYS | libc::EINVAL)
    )
}

/// Link then unlink, and the placeholder method where there are no links.
pub(super) fn move_without_replacing(from: &Path, to: &Path) -> io::Result<()> {
    move_without_replacing_with(from, to, move_by_link)
}

/// [`move_without_replacing`] with the first attempt passed in, so a test can make it report
/// that links are unsupported.
pub(super) fn move_without_replacing_with(
    from: &Path,
    to: &Path,
    first: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    match first(from, to) {
        Err(e) if links_unsupported(&e) => {
            debug!("No hard links here ({e}); claiming the name with a placeholder instead");
            move_by_placeholder(from, to)
        }
        other => other,
    }
}

/// Sends a file to the Trash ([`super::unix_trash`]).
pub fn send_to_bin(path: &Path) -> Result<(), String> {
    super::unix_trash::send(path)
}

/// Takes an exclusive `flock` on `path` (creating it), held until the returned file is
/// dropped. `Ok(None)` if someone else already holds it.
pub fn acquire_lock(path: &Path) -> io::Result<Option<File>> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    // SAFETY: the descriptor belongs to `f`, which is alive for the call.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(f));
    }
    let e = io::Error::last_os_error();
    if e.kind() == io::ErrorKind::WouldBlock {
        Ok(None)
    } else {
        Err(e)
    }
}

/// `$XDG_CONFIG_HOME/<app_dir>`, else `~/.config/<app_dir>`.
pub fn settings_dir(app_dir: &str) -> Option<PathBuf> {
    settings_dir_from(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
        app_dir,
    )
}

/// The rule behind [`settings_dir`], on plain values. A relative `XDG_CONFIG_HOME` is ignored, as
/// the XDG rules say.
pub fn settings_dir_from(
    xdg_config_home: Option<OsString>,
    home: Option<OsString>,
    app_dir: &str,
) -> Option<PathBuf> {
    if let Some(x) = xdg_config_home.map(PathBuf::from)
        && x.is_absolute()
    {
        return Some(x.join(app_dir));
    }
    home.filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".config").join(app_dir))
}

fn now() -> (libc::time_t, u32) {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (d.as_secs() as libc::time_t, d.subsec_millis())
}

fn broken_down(secs: libc::time_t, local: bool) -> libc::tm {
    // SAFETY: an all-zero `tm` is valid (a null `tm_zone` pointer is allowed), and the *_r
    // functions only write into it.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are to live locals.
    let ok = local && unsafe { !libc::localtime_r(&secs, &mut tm).is_null() };
    if !ok {
        // SAFETY: as above.
        unsafe { libc::gmtime_r(&secs, &mut tm) };
    }
    tm
}

/// The local date and time as a folder name, e.g. `2026-11-16 15-30-05`.
pub fn local_stamp() -> String {
    let t = broken_down(now().0, true);
    format!(
        "{:04}-{:02}-{:02} {:02}-{:02}-{:02}",
        t.tm_year + 1900,
        t.tm_mon + 1,
        t.tm_mday,
        t.tm_hour,
        t.tm_min,
        t.tm_sec
    )
}

/// The current UTC time as text, e.g. `2026-11-17T18:00:00Z`.
pub fn utc_stamp() -> String {
    let t = broken_down(now().0, false);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        t.tm_year + 1900,
        t.tm_mon + 1,
        t.tm_mday,
        t.tm_hour,
        t.tm_min,
        t.tm_sec
    )
}

/// The local time of day in milliseconds since midnight, for time-stamping log lines.
pub fn local_time_ms() -> u32 {
    let (secs, ms) = now();
    let t = broken_down(secs, true);
    ((t.tm_hour as u32 * 60 + t.tm_min as u32) * 60 + t.tm_sec as u32) * 1000 + ms
}

/// What the device thread needs from the system for as long as it runs. There are no portable
/// devices here yet (phones and cameras come later), so the list is always empty.
pub struct Session;

impl Session {
    pub fn init() -> Result<Self, String> {
        Ok(Self)
    }

    pub fn list_portable(&self) -> Result<Vec<ListedDevice>, String> {
        Ok(Vec::new())
    }

    pub fn open_portable(&self, pnp_id: &str) -> Result<Box<dyn DeviceSource>, String> {
        Err(format!("No device {pnp_id} is available on this system"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    #[test]
    fn the_settings_folder_follows_xdg_then_home() {
        let d = |x, h| settings_dir_from(x, h, "plug-n-keep");
        assert_eq!(
            d(os("/x/cfg"), os("/home/a")),
            Some(PathBuf::from("/x/cfg/plug-n-keep"))
        );
        assert_eq!(
            d(None, os("/home/a")),
            Some(PathBuf::from("/home/a/.config/plug-n-keep"))
        );
        // Relative or empty XDG values are ignored.
        assert_eq!(
            d(os("rel"), os("/home/a")),
            Some(PathBuf::from("/home/a/.config/plug-n-keep"))
        );
        assert_eq!(
            d(os(""), os("/home/a")),
            Some(PathBuf::from("/home/a/.config/plug-n-keep"))
        );
        assert_eq!(d(None, None), None);
        assert_eq!(d(None, os("")), None);
    }

    #[test]
    fn a_link_is_detected_and_a_file_is_not() {
        let t = tempfile::tempdir().unwrap();
        let (file, link) = (t.path().join("f"), t.path().join("l"));
        fs::write(&file, b"x").unwrap();
        std::os::unix::fs::symlink(&file, &link).unwrap();
        assert!(is_link(&fs::symlink_metadata(&link).unwrap()));
        assert!(!is_link(&fs::symlink_metadata(&file).unwrap()));
        assert!(!is_link(&fs::symlink_metadata(t.path()).unwrap()));
    }

    #[test]
    fn the_link_method_never_replaces_and_moves_onto_a_free_name() {
        let t = tempfile::tempdir().unwrap();
        let (from, to) = (t.path().join("new"), t.path().join("old"));
        fs::write(&from, b"new").unwrap();
        fs::write(&to, b"old").unwrap();
        let err = move_by_link(&from, &to).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
        assert_eq!(fs::read(&from).unwrap(), b"new");
        assert_eq!(fs::read(&to).unwrap(), b"old");
        let free = t.path().join("free");
        move_by_link(&from, &free).unwrap();
        assert!(!from.exists());
        assert_eq!(fs::read(&free).unwrap(), b"new");
    }

    #[test]
    fn the_placeholder_method_never_replaces_and_moves_onto_a_free_name() {
        let t = tempfile::tempdir().unwrap();
        let (from, to) = (t.path().join("new"), t.path().join("old"));
        fs::write(&from, b"new").unwrap();
        fs::write(&to, b"old").unwrap();
        let err = move_by_placeholder(&from, &to).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
        assert_eq!(fs::read(&from).unwrap(), b"new");
        assert_eq!(fs::read(&to).unwrap(), b"old");
        let free = t.path().join("free");
        move_by_placeholder(&from, &free).unwrap();
        assert!(!from.exists());
        assert_eq!(fs::read(&free).unwrap(), b"new");
    }

    #[test]
    fn the_placeholder_method_leaves_nothing_behind_when_the_move_fails() {
        let t = tempfile::tempdir().unwrap();
        let (missing, to) = (t.path().join("missing"), t.path().join("target"));
        assert!(move_by_placeholder(&missing, &to).is_err());
        assert!(!to.exists(), "an empty placeholder was left behind");
    }

    #[test]
    fn the_placeholder_method_does_not_follow_a_link_in_the_way() {
        let t = tempfile::tempdir().unwrap();
        let (from, to, victim) = (
            t.path().join("new"),
            t.path().join("target"),
            t.path().join("victim"),
        );
        fs::write(&from, b"new").unwrap();
        fs::write(&victim, b"precious").unwrap();
        std::os::unix::fs::symlink(&victim, &to).unwrap();
        assert!(move_by_placeholder(&from, &to).is_err());
        assert_eq!(fs::read(&victim).unwrap(), b"precious");
        assert_eq!(fs::read(&from).unwrap(), b"new");
    }

    /// Puts a different file under `path`. The new file is made before the old one goes, so it
    /// can't be given the old one's inode.
    fn swap_in(path: &Path, data: &[u8]) {
        let side = path.with_extension("swap");
        fs::write(&side, data).unwrap();
        fs::rename(&side, path).unwrap();
    }

    #[test]
    fn the_fallback_chain_never_replaces() {
        let t = tempfile::tempdir().unwrap();
        let (from, to) = (t.path().join("new"), t.path().join("old"));
        fs::write(&from, b"new").unwrap();
        fs::write(&to, b"old").unwrap();
        let err = move_without_replacing(&from, &to).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
        assert_eq!(fs::read(&to).unwrap(), b"old");
    }

    /// Links reported as unsupported must lead to the placeholder method, which also never
    /// replaces.
    #[test]
    fn when_links_are_unsupported_the_placeholder_method_runs_and_never_replaces() {
        let unsupported = |_: &Path, _: &Path| Err(io::Error::from_raw_os_error(libc::EPERM));
        let t = tempfile::tempdir().unwrap();
        let (from, to) = (t.path().join("new"), t.path().join("old"));
        fs::write(&from, b"new").unwrap();
        fs::write(&to, b"old").unwrap();
        let err = move_without_replacing_with(&from, &to, unsupported).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
        assert_eq!(fs::read(&to).unwrap(), b"old");
        assert_eq!(fs::read(&from).unwrap(), b"new");
        // A free name: moved by the placeholder method.
        let free = t.path().join("free");
        move_without_replacing_with(&from, &free, unsupported).unwrap();
        assert!(!from.exists());
        assert_eq!(fs::read(&free).unwrap(), b"new");
    }

    /// A first attempt that fails for a real reason is reported, not retried another way.
    #[test]
    fn a_real_failure_does_not_fall_back() {
        let t = tempfile::tempdir().unwrap();
        let (from, to) = (t.path().join("new"), t.path().join("free"));
        fs::write(&from, b"new").unwrap();
        let denied = |_: &Path, _: &Path| Err(io::Error::from_raw_os_error(libc::EACCES));
        assert!(move_without_replacing_with(&from, &to, denied).is_err());
        assert!(!to.exists());
        assert_eq!(fs::read(&from).unwrap(), b"new");
    }

    #[test]
    fn a_link_move_keeps_the_new_name_when_the_old_one_was_already_removed() {
        let t = tempfile::tempdir().unwrap();
        let (from, to) = (t.path().join("a"), t.path().join("b"));
        fs::write(&from, b"data").unwrap();
        // Someone else removes the old name between our link and our unlink.
        move_by_link_with(&from, &to, |p| {
            fs::remove_file(p).unwrap();
            fs::remove_file(p)
        })
        .unwrap();
        assert!(!from.exists());
        assert_eq!(fs::read(&to).unwrap(), b"data");
    }

    #[test]
    fn a_link_move_that_cannot_unlink_undoes_its_own_link_only() {
        let t = tempfile::tempdir().unwrap();
        let (from, to) = (t.path().join("a"), t.path().join("b"));
        fs::write(&from, b"data").unwrap();
        let denied = || io::Error::from_raw_os_error(libc::EACCES);
        assert!(move_by_link_with(&from, &to, |_| Err(denied())).is_err());
        assert_eq!(fs::read(&from).unwrap(), b"data");
        assert!(!to.exists());
        // If the new name no longer is our file, it is left alone.
        let (from, to) = (t.path().join("c"), t.path().join("d"));
        fs::write(&from, b"data").unwrap();
        let r = move_by_link_with(&from, &to, |_| {
            swap_in(&to, b"other");
            Err(denied())
        });
        assert!(r.is_err());
        assert_eq!(fs::read(&to).unwrap(), b"other");
        assert_eq!(fs::read(&from).unwrap(), b"data");
    }

    #[test]
    fn a_failed_placeholder_rename_removes_only_our_own_empty_placeholder() {
        let t = tempfile::tempdir().unwrap();
        let (from, to) = (t.path().join("a"), t.path().join("b"));
        fs::write(&from, b"data").unwrap();
        let fail = |_: &Path, _: &Path| Err(io::Error::from_raw_os_error(libc::EIO));
        assert!(move_by_placeholder_with(&from, &to, fail).is_err());
        assert!(!to.exists(), "our placeholder is removed");
        assert_eq!(fs::read(&from).unwrap(), b"data");
        // Someone else's file replaced the placeholder: it is not ours, so it stays.
        let other = t.path().join("c");
        let r = move_by_placeholder_with(&from, &other, |_, t| {
            swap_in(t, b"other");
            Err(io::Error::from_raw_os_error(libc::EIO))
        });
        assert!(r.is_err());
        assert_eq!(fs::read(&other).unwrap(), b"other");
        // Someone else's empty file (a different inode) stays too.
        let empty = t.path().join("e");
        let r = move_by_placeholder_with(&from, &empty, |_, t| {
            swap_in(t, b"");
            Err(io::Error::from_raw_os_error(libc::EIO))
        });
        assert!(r.is_err());
        assert!(empty.exists());
        // The placeholder got data written into it (same file, no longer empty): it stays.
        let filled = t.path().join("f");
        let r = move_by_placeholder_with(&from, &filled, |_, t| {
            fs::write(t, b"data").unwrap();
            Err(io::Error::from_raw_os_error(libc::EIO))
        });
        assert!(r.is_err());
        assert_eq!(fs::read(&filled).unwrap(), b"data");
    }

    #[test]
    fn a_second_lock_on_the_same_file_is_refused_until_the_first_is_dropped() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("instance.lock");
        let first = acquire_lock(&p).unwrap();
        assert!(first.is_some());
        assert!(acquire_lock(&p).unwrap().is_none());
        drop(first);
        assert!(acquire_lock(&p).unwrap().is_some());
    }

    #[test]
    fn the_clock_gives_well_formed_text() {
        let s = local_stamp();
        assert_eq!(s.len(), "2026-11-16 15-30-05".len(), "{s}");
        assert!(local_time_ms() < 86_400_000);
        assert!(utc_stamp().ends_with('Z'));
    }
}
