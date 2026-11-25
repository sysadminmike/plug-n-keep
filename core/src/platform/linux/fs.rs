//! Moving a file into place without replacing anything, on Linux.

use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use log::debug;

use crate::platform::unix;

/// Fails with `ErrorKind::AlreadyExists` if `to` exists.
///
/// `renameat2` with `RENAME_NOREPLACE` is atomic: the name is either taken for the new file or
/// refused. A file system or kernel that doesn't know the flag gets the link-then-unlink method,
/// and one with no hard links (FAT, exFAT) the placeholder method.
pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    rename_no_replace_with(from, to, renameat2_no_replace)
}

/// [`rename_no_replace`] with the first attempt passed in, so a test can make it report that
/// the flag isn't supported.
fn rename_no_replace_with(
    from: &Path,
    to: &Path,
    first: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    match first(from, to) {
        Ok(()) => {}
        Err(e) if flag_unsupported(&e) => {
            debug!("renameat2 is not available here ({e}); using a link instead");
            unix::move_without_replacing(from, to)?;
        }
        Err(e) => return Err(e),
    }
    unix::flush_after_move(from, to);
    Ok(())
}

fn flag_unsupported(e: &io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(libc::ENOSYS | libc::EINVAL | libc::ENOTSUP)
    )
}

fn renameat2_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    let cstr = |p: &Path| {
        CString::new(p.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path contains a NUL"))
    };
    let (f, t) = (cstr(from)?, cstr(to)?);
    // SAFETY: both strings are NUL-terminated and outlive the call; the flag is a plain int.
    let r = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            f.as_ptr(),
            libc::AT_FDCWD,
            t.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn renameat2_refuses_a_taken_name_and_keeps_both_files() {
        let t = tempfile::tempdir().unwrap();
        let (from, to) = (t.path().join("new"), t.path().join("old"));
        fs::write(&from, b"new").unwrap();
        fs::write(&to, b"old").unwrap();
        let err = renameat2_no_replace(&from, &to).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
        assert_eq!(fs::read(&from).unwrap(), b"new");
        assert_eq!(fs::read(&to).unwrap(), b"old");
    }

    /// A kernel or file system that doesn't know the flag must lead to the link method, which
    /// also never replaces.
    #[test]
    fn an_unsupported_flag_falls_back_to_a_link_and_never_replaces() {
        let unsupported = |_: &Path, _: &Path| Err(io::Error::from_raw_os_error(libc::ENOSYS));
        let t = tempfile::tempdir().unwrap();
        let (from, to) = (t.path().join("new"), t.path().join("old"));
        fs::write(&from, b"new").unwrap();
        fs::write(&to, b"old").unwrap();
        let err = rename_no_replace_with(&from, &to, unsupported).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{err}");
        assert_eq!(fs::read(&to).unwrap(), b"old");
        assert_eq!(fs::read(&from).unwrap(), b"new");
        let free = t.path().join("free");
        rename_no_replace_with(&from, &free, unsupported).unwrap();
        assert!(!from.exists());
        assert_eq!(fs::read(&free).unwrap(), b"new");
    }

    /// Any other failure of the first attempt is reported, not retried another way.
    #[test]
    fn another_failure_does_not_fall_back() {
        let denied = |_: &Path, _: &Path| Err(io::Error::from_raw_os_error(libc::EACCES));
        let t = tempfile::tempdir().unwrap();
        let (from, to) = (t.path().join("new"), t.path().join("free"));
        fs::write(&from, b"new").unwrap();
        assert!(rename_no_replace_with(&from, &to, denied).is_err());
        assert!(!to.exists());
        assert_eq!(fs::read(&from).unwrap(), b"new");
    }
}
