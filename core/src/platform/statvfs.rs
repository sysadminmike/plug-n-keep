//! The size of a mounted file system, through `statvfs`. Shared by Linux and FreeBSD.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Total size in bytes of the file system mounted at `mount_point`, or `None` if it can't be
/// asked. Only reads.
pub fn capacity(mount_point: &Path) -> Option<u64> {
    let c = CString::new(mount_point.as_os_str().as_bytes()).ok()?;
    // SAFETY: `st` is plain data that statvfs fills in; `c` is NUL-terminated and outlives the call.
    let st = unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 {
            return None;
        }
        st
    };
    #[allow(clippy::unnecessary_cast)]
    let total = (st.f_blocks as u64).checked_mul(st.f_frsize as u64)?;
    (total > 0).then_some(total)
}

/// The size and the free space (what an ordinary user can still use) of the file system `path`
/// is on, through `statvfs`. Only reads.
pub fn disk_space(path: &Path) -> Option<super::DiskSpace> {
    let c = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `st` is plain data that statvfs fills in; `c` is NUL-terminated and outlives the call.
    let st = unsafe {
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c.as_ptr(), &mut st) != 0 {
            return None;
        }
        st
    };
    #[allow(clippy::unnecessary_cast)]
    let (blocks, avail, unit) = (st.f_blocks as u64, st.f_bavail as u64, st.f_frsize as u64);
    let total = blocks.checked_mul(unit)?;
    let free = avail.checked_mul(unit)?;
    (total > 0).then_some(super::DiskSpace { total, free })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_temp_folders_file_system_has_a_size_and_a_missing_one_has_none() {
        let t = tempfile::tempdir().unwrap();
        // tmpfs and friends may report 0; a real size must at least not be absurd.
        if let Some(c) = capacity(t.path()) {
            assert!(c >= 4096);
        }
        assert_eq!(capacity(&t.path().join("gone")), None);
    }

    #[test]
    fn a_temp_folders_disk_has_free_space_not_above_its_size() {
        let t = tempfile::tempdir().unwrap();
        if let Some(s) = disk_space(t.path()) {
            assert!(s.free <= s.total);
        }
        assert_eq!(disk_space(&t.path().join("gone")), None);
    }
}
