//! Moving a file into place without replacing anything, on FreeBSD.

use std::io;
use std::path::Path;

use crate::platform::unix;

/// Fails with `ErrorKind::AlreadyExists` if `to` exists. There is no atomic "rename unless the
/// name is taken" here, so the file gets a second name (a hard link, which fails if the name is
/// taken) and then loses the first. File systems with no hard links get the placeholder method.
pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    unix::move_without_replacing(from, to)?;
    unix::flush_after_move(from, to);
    Ok(())
}
