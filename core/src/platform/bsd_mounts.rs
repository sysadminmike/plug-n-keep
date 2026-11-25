//! Choosing the USB volumes out of FreeBSD's list of mounts. Pure functions over text-like
//! values, built on every system for the tests, and used by `freebsd/volume.rs`.
//!
//! A USB disk shows up as `/dev/da<N>`, with partitions such as `da0p2` or `da0s1`. A volume is
//! offered only if it is a `da` partition with a file system a stick is formatted with, and its
//! disk is not one the running system lives on.

/// A mounted file system, as `getmntinfo` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub source: String,
    pub mount_point: String,
    pub fs_type: String,
}

/// File systems a stick, card or drive is formatted with. `fusefs` is how exFAT and NTFS
/// volumes mounted through FUSE report themselves.
const FILE_SYSTEMS: [&str; 6] = ["msdosfs", "exfat", "ntfs", "ufs", "ext2fs", "fusefs"];

/// Mount points of the running system. Whatever is mounted here, and anything else on the same
/// disk, is never offered.
const SYSTEM_MOUNTS: [&str; 7] = ["/", "/boot", "/usr", "/usr/local", "/var", "/home", "/tmp"];

/// The `da` disk a device name belongs to: `/dev/da0`, `/dev/da0p2` and `/dev/da0s1a` all give
/// `da0`. Anything else (`/dev/daemon`, `/dev/ada0p2`, `devfs`) gives `None`.
pub fn da_disk(source: &str) -> Option<String> {
    let rest = source.strip_prefix("/dev/da")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let tail = &rest[digits.len()..];
    let slice_or_partition = tail.is_empty()
        || ((tail.starts_with('s') || tail.starts_with('p'))
            && tail[1..].chars().next().is_some_and(|c| c.is_ascii_digit()));
    slice_or_partition.then(|| format!("da{digits}"))
}

/// The mounts that are USB volumes worth offering: not the system's own, and not any other
/// partition of a disk the system runs from.
pub fn usb_volumes(mounts: &[Mount]) -> Vec<Mount> {
    let system_disks: Vec<String> = mounts
        .iter()
        .filter(|m| SYSTEM_MOUNTS.contains(&m.mount_point.as_str()))
        .filter_map(|m| da_disk(&m.source))
        .collect();
    mounts
        .iter()
        .filter(|m| {
            let Some(disk) = da_disk(&m.source) else {
                return false;
            };
            FILE_SYSTEMS.contains(&m.fs_type.as_str())
                && !SYSTEM_MOUNTS.contains(&m.mount_point.as_str())
                && !system_disks.contains(&disk)
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(source: &str, mount_point: &str, fs_type: &str) -> Mount {
        Mount {
            source: source.into(),
            mount_point: mount_point.into(),
            fs_type: fs_type.into(),
        }
    }

    fn points(v: &[Mount]) -> Vec<&str> {
        v.iter().map(|m| m.mount_point.as_str()).collect()
    }

    #[test]
    fn disks_are_named_from_their_partitions() {
        assert_eq!(da_disk("/dev/da0").as_deref(), Some("da0"));
        assert_eq!(da_disk("/dev/da0s1").as_deref(), Some("da0"));
        assert_eq!(da_disk("/dev/da12p2").as_deref(), Some("da12"));
        assert_eq!(da_disk("/dev/da1s1a").as_deref(), Some("da1"));
        for no in [
            "/dev/daemon",
            "/dev/ada0p2",
            "devfs",
            "/dev/da",
            "/dev/da0x",
        ] {
            assert_eq!(da_disk(no), None, "{no}");
        }
    }

    #[test]
    fn only_usb_disk_volumes_are_offered() {
        let all = [
            m("/dev/da0s1", "/media/STICK", "msdosfs"),
            m("/dev/da1p2", "/mnt/x", "ufs"),
            m("/dev/ada0p2", "/", "ufs"),
            m("/dev/daemon", "/mnt/y", "ufs"),
            m("devfs", "/dev", "devfs"),
            // exFAT and NTFS volumes mounted through FUSE.
            m("/dev/da2s1", "/mnt/exfat", "fusefs"),
            m("/dev/da3s1", "/mnt/other", "zfs"),
        ];
        assert_eq!(
            points(&usb_volumes(&all)),
            ["/media/STICK", "/mnt/x", "/mnt/exfat"]
        );
    }

    /// A system installed on a USB disk: its other partitions are the system's too.
    #[test]
    fn no_partition_of_a_disk_the_system_runs_from_is_offered() {
        for system_mount in ["/", "/usr", "/var", "/home"] {
            let all = [
                m("/dev/da0p2", system_mount, "ufs"),
                m("/dev/da0p3", "/mnt/data", "ufs"),
                m("/dev/da0p4", "/mnt/win", "fusefs"),
                m("/dev/da1s1", "/media/STICK", "msdosfs"),
            ];
            assert_eq!(
                points(&usb_volumes(&all)),
                ["/media/STICK"],
                "system on {system_mount}"
            );
        }
    }
}
