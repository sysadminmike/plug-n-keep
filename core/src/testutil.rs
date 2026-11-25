//! Helpers shared by the tests.

use std::path::Path;

/// A folder link: a junction on Windows (no admin rights needed, unlike symlinks), a symlink
/// elsewhere. Both are what the code under test must refuse to follow.
#[cfg(windows)]
pub fn make_junction(link: &Path, target: &Path) {
    let out = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .expect("run mklink");
    assert!(
        out.status.success(),
        "mklink /J failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[cfg(unix)]
pub fn make_junction(link: &Path, target: &Path) {
    std::os::unix::fs::symlink(target, link).expect("make a symlink");
}

/// A folder made read-only for a test. Dropping it (also when the test panics) makes the folder
/// writable again, so the temporary folder can be cleaned up.
#[cfg(unix)]
pub struct ReadOnlyDir(std::path::PathBuf);

#[cfg(unix)]
impl ReadOnlyDir {
    pub fn new(dir: &Path) -> Self {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555))
            .expect("make the folder read-only");
        Self(dir.to_path_buf())
    }

    /// Takes charge of a folder that something else made read-only, to undo it on drop.
    pub fn adopt(dir: &Path) -> Self {
        Self(dir.to_path_buf())
    }

    /// Whether the folder really refuses new files. Not so for root, which ignores folder
    /// permissions.
    pub fn is_enforced(&self) -> bool {
        let probe = self.0.join(".pnk-probe");
        let writable = std::fs::write(&probe, b"x").is_ok();
        let _ = std::fs::remove_file(&probe);
        !writable
    }
}

#[cfg(unix)]
impl Drop for ReadOnlyDir {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
}
