//! Will a sync fit on the backup disk? Worked out before anything is copied, so a sync that
//! can't finish is refused up front instead of filling the disk half-way.
//!
//! What a sync writes is the size of every file it copies: a new file, or a changed file's new
//! copy. Each is written to a temporary file that is then moved into place, so the temporary
//! file is the new copy and counts once. Moving an old copy into the archive, beside the new
//! one, or into the Recycle Bin or Trash stays on the same disk and needs no more room. The JPG
//! copies of HEIC photos ([`jpg_bytes`]) are added as an estimate.
//!
//! **The margin.** The disk must keep free, after the sync, 2 % of its size, but never less
//! than 1 GiB and never more than 10 GiB (on a 4 TB disk 2 % would be 80 GB, which would refuse
//! a sync that plainly fits). A disk filled to the last byte makes the machine itself misbehave
//! (the page file, other programs' saves), and the free space moves while a sync runs. The
//! margin is not counted as "needed": the message says what the sync needs and what is free.
//!
//! **Odd numbers.** A file system that reports 0 free on a disk with a size (some network and
//! FUSE shares do) or more free than its size is taken as "unknown", not as full: the sync is
//! allowed and the disk-full stop is the backstop.

use crate::plan::{ScanPlan, Status};
use crate::platform::DiskSpace;
use crate::sync::{CopyMode, SyncItem};

/// How big a JPG copy of a HEIC photo is expected to be, as a multiple of the HEIC's size. A
/// JPG at quality 90 of an iPhone photo runs about this (HEIC is the smaller format); it is an
/// estimate for the room check, not a promise.
pub const JPG_SIZE_FACTOR: f64 = 1.5;

/// The least free space left after a sync: 1 GiB ...
pub const MIN_FREE: u64 = 1 << 30;
/// ... or this share of the disk's size, whichever is more, but at most [`MAX_MARGIN`].
pub const MIN_FREE_PERCENT: u64 = 2;
/// The most the margin ever asks for, however big the disk.
pub const MAX_MARGIN: u64 = 10 << 30;

/// The free space a sync must leave on a disk of `total` bytes:
/// `max(1 GiB, min(2 % of the disk, 10 GiB))`.
pub fn margin(total: u64) -> u64 {
    MIN_FREE.max((total / 100 * MIN_FREE_PERCENT).min(MAX_MARGIN))
}

/// The bytes the sync of `items` will write into the backup folder: every file that will be
/// copied, at its size. A file already backed up (nothing to copy), one that can't be classified
/// and one with no known size add nothing.
pub fn bytes_to_write(plan: &ScanPlan, items: &[SyncItem]) -> u64 {
    items
        .iter()
        .filter_map(|item| plan.entries.get(item.entry).map(|e| (item, e)))
        .filter(|(item, e)| match &e.status {
            Status::Error(_) => false,
            // Copying a backed-up file again writes a whole new copy.
            Status::Verified => item.mode == CopyMode::Replace,
            _ => true,
        })
        .filter_map(|(_, e)| e.size)
        .fold(0u64, u64::saturating_add)
}

/// The room the JPG copies this plan's sync would make are expected to take: [`JPG_SIZE_FACTOR`]
/// times the size of their HEICs ([`crate::jpgs::JpgPlan::estimate_heic_bytes`]). Nothing when
/// the plan makes none.
pub fn jpg_bytes(plan: &ScanPlan) -> u64 {
    (plan.jpg.estimate_heic_bytes as f64 * JPG_SIZE_FACTOR) as u64
}

/// A sync that does not fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortOfSpace {
    pub needed: u64,
    pub free: u64,
    /// What must stay free as well ([`margin`]).
    pub margin: u64,
}

impl std::fmt::Display for ShortOfSpace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.needed > self.free {
            write!(
                f,
                "This sync needs {} but the backup disk has {} free.",
                size_text(self.needed),
                size_text(self.free)
            )
        } else {
            write!(
                f,
                "This sync needs {} and the backup disk must keep {} free, but it has {} free.",
                size_text(self.needed),
                size_text(self.margin),
                size_text(self.free)
            )
        }
    }
}

/// Whether `needed` bytes fit on `space` with the margin kept. With no figure for the disk the
/// answer is yes: the disk-full stop mid-sync is the backstop.
pub fn check(needed: u64, space: Option<DiskSpace>) -> Result<(), ShortOfSpace> {
    // A sync that writes nothing (it only moves files) needs no room.
    let Some(space) = space.filter(|_| needed > 0) else {
        return Ok(());
    };
    // Numbers that can't be right are no reason to refuse.
    if (space.free == 0 && space.total > 0) || space.free > space.total {
        return Ok(());
    }
    let margin = margin(space.total);
    if needed.saturating_add(margin) > space.free {
        Err(ShortOfSpace {
            needed,
            free: space.free,
            margin,
        })
    } else {
        Ok(())
    }
}

/// Whether the JPG copy of a HEIC of `heic_size` bytes ([`JPG_SIZE_FACTOR`] times that) fits on
/// `space` with the same margin kept as for a sync ([`check`], so [`margin`]). The JPG step asks
/// before each JPG, since the free space moves while it runs. The error is a plain sentence for
/// the step's report.
pub fn check_jpg(heic_size: u64, space: Option<DiskSpace>) -> Result<(), String> {
    let needed = (heic_size as f64 * JPG_SIZE_FACTOR) as u64;
    check(needed, space).map_err(|s| {
        format!(
            "the backup disk has {} free and must keep {} free, and the next JPG copy needs about {}",
            size_text(s.free),
            size_text(s.margin),
            size_text(s.needed)
        )
    })
}

/// How big a backup folder is, worked out by walking it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FolderSizes {
    /// Every file in the folder, the archive included.
    pub total: u64,
    /// The part of that inside the archive folders ([`FolderSizes::total`] holds them too).
    pub archive: u64,
    pub files: u64,
}

/// Walk `root` and add up its files. A top-level folder whose name is in `archive_names` (any
/// case, as the file system sees it) counts as the archive. Links are not followed and not
/// counted; what can't be read is left out. Only reads, and slow on a big folder: run it off the
/// window's thread.
pub fn folder_sizes(root: &std::path::Path, archive_names: &[String]) -> FolderSizes {
    folder_sizes_within(root, archive_names, WALK_LIMIT)
}

/// The longest a folder walk runs. A share that hangs would otherwise keep the walk (and its
/// thread) forever; what was counted by then is the answer, which is only an estimate anyway.
const WALK_LIMIT: std::time::Duration = std::time::Duration::from_secs(120);

/// [`folder_sizes`], giving up after `limit`. A file still being written (`.pnk-part`, `.pnk-jpg-part`) is not
/// counted.
pub fn folder_sizes_within(
    root: &std::path::Path,
    archive_names: &[String],
    limit: std::time::Duration,
) -> FolderSizes {
    let started = std::time::Instant::now();
    let mut sizes = FolderSizes::default();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .flatten()
    {
        if started.elapsed() > limit {
            log::warn!(
                "Counting the size of {} took too long; the figures are partial",
                root.display()
            );
            break;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        if entry
            .file_name()
            .to_str()
            .is_some_and(crate::paths::is_temp_name)
        {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let len = meta.len();
        sizes.total = sizes.total.saturating_add(len);
        sizes.files += 1;
        let first = entry
            .path()
            .strip_prefix(root)
            .ok()
            .and_then(|p| p.components().next())
            .and_then(|c| c.as_os_str().to_str());
        // A file directly in the folder has no "first folder": its own name is not an archive.
        let in_a_folder = entry.depth() > 1;
        if in_a_folder
            && first.is_some_and(|f| archive_names.iter().any(|a| a.eq_ignore_ascii_case(f)))
        {
            sizes.archive = sizes.archive.saturating_add(len);
        }
    }
    sizes
}

/// "14.2 GB", 1024-based like the window's sizes.
fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1 << 30;

    fn disk(total_gb: u64, free_gb: u64) -> Option<DiskSpace> {
        Some(DiskSpace {
            total: total_gb * GB,
            free: free_gb * GB,
        })
    }

    #[test]
    fn a_folders_size_counts_the_archive_apart_and_in_the_total() {
        let t = tempfile::tempdir().unwrap();
        let w = |rel: &str, n: usize| {
            let p = t.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, vec![0u8; n]).unwrap();
        };
        w("DCIM/a.jpg", 100);
        w("b.txt", 10);
        w("_Archive/2026-01-01 00-00-00/DCIM/old.jpg", 40);
        w("_archive/x.bin", 5);
        // A file named like the archive, not in a folder of that name, is not archive.
        w("Old/_Archive", 7);
        // A copy in progress is not part of the backup yet.
        w("DCIM/b.mov.pnk-part", 1000);
        let s = folder_sizes(t.path(), &["_Archive".into()]);
        assert_eq!((s.total, s.archive, s.files), (162, 45, 5));
        assert_eq!(
            folder_sizes(&t.path().join("none"), &[]),
            FolderSizes::default()
        );
    }

    #[test]
    fn the_margin_is_two_percent_between_one_and_ten_gigabytes() {
        assert_eq!(margin(10 * GB), GB);
        assert_eq!(margin(50 * GB), GB);
        assert_eq!(margin(100 * GB), 2 * GB);
        assert_eq!(margin(500 * GB), 10 * GB);
        assert_eq!(margin(10_000 * GB), 10 * GB);
        assert_eq!(margin(0), GB);
    }

    #[test]
    fn a_disk_reporting_nothing_free_or_more_than_its_size_is_not_a_refusal() {
        assert!(check(5 * GB, disk(100, 0)).is_ok());
        assert!(check(5 * GB, disk(100, 200)).is_ok());
        assert!(check(5 * GB, disk(100, 1)).is_err());
    }

    #[test]
    fn what_fits_with_the_margin_is_allowed_and_what_does_not_is_refused() {
        assert!(check(5 * GB, disk(50, 7)).is_ok());
        // Exactly the margin (1 GiB on a 50 GiB disk) is enough; one byte more is not.
        assert!(check(6 * GB, disk(50, 7)).is_ok());
        assert!(check(6 * GB + 1, disk(50, 7)).is_err());
        // 2 % of a 100 GiB disk is 2 GiB.
        assert!(check(5 * GB, disk(100, 7)).is_ok());
        assert!(check(5 * GB + 1, disk(100, 7)).is_err());
        // Nothing to write always fits (a sync that only moves files).
        assert!(check(0, disk(100, 0)).is_ok());
        // No figure for the disk: not refused.
        assert!(check(50 * GB, None).is_ok());
    }

    #[test]
    fn the_message_says_what_is_needed_and_what_is_free() {
        let short = check(14 * GB + GB / 5, disk(500, 9)).unwrap_err();
        assert_eq!(
            short.to_string(),
            "This sync needs 14.2 GB but the backup disk has 9.0 GB free."
        );
        let tight = check(8 * GB + GB / 2, disk(50, 9)).unwrap_err();
        assert!(
            tight.to_string().contains("must keep 1.0 GB free"),
            "{tight}"
        );
        assert!(tight.to_string().contains("has 9.0 GB free"), "{tight}");
    }
}
