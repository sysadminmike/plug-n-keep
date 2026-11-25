//! Sticks, SD cards and drives on Linux: the mounted file systems whose block device is
//! removable or sits on USB, and what can be learned about each without opening any of them.
//!
//! - **Mounts:** `/proc/self/mountinfo`.
//! - **Block device:** sysfs, through `/sys/dev/block/<major:minor>` (or the device name).
//!   Walking up from it finds the USB device (vendor, product, serial, maker, product name),
//!   the `removable` flag and an SD card.
//! - **Format ID, partition ID, label:** the links in `/dev/disk/by-uuid`, `by-partuuid` and
//!   `by-label`.
//! - **Size:** `statvfs` on the mount point.
//!
//! The parsers are pure functions over text and folder trees, and are tested on samples. The
//! system's own file systems are never offered.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use log::{debug, info};

use crate::device::DeviceSource;
use crate::platform::{ListedDevice, statvfs};
use crate::signals::Signals;
use crate::volume::{VolumeDetails, open_listed};

/// Where the system keeps what this module reads, so tests can point it at a fake tree.
struct Sys {
    mountinfo: PathBuf,
    sys_dev_block: PathBuf,
    sys_class_block: PathBuf,
    dev_disk: PathBuf,
}

impl Sys {
    fn real() -> Self {
        Self {
            mountinfo: "/proc/self/mountinfo".into(),
            sys_dev_block: "/sys/dev/block".into(),
            sys_class_block: "/sys/class/block".into(),
            dev_disk: "/dev/disk".into(),
        }
    }
}

/// Mount points of the running system. Whatever is mounted here, and anything else on the same
/// disk, is never offered.
pub(super) const SYSTEM_MOUNTS: [&str; 14] = [
    "/",
    "/boot",
    "/boot/efi",
    "/boot/firmware",
    "/efi",
    "/usr",
    "/var",
    "/var/lib",
    "/home",
    "/root",
    "/etc",
    "/opt",
    "/srv",
    "/nix",
];

/// File systems a stick, card or drive is formatted with.
const FILE_SYSTEMS: [&str; 14] = [
    "vfat", "msdos", "exfat", "ntfs", "ntfs3", "fuseblk", "ext2", "ext3", "ext4", "btrfs", "xfs",
    "f2fs", "hfsplus", "udf",
];

/// The mounted volumes that could be a source.
pub fn list() -> Vec<ListedDevice> {
    volumes(&Sys::real())
        .iter()
        .map(VolumeDetails::listed)
        .collect()
}

/// Opens a listed volume as a read-only source. Only a volume that is listed now can be
/// opened, so an ID can't name some other folder.
pub fn open(device: &ListedDevice) -> Result<Box<dyn DeviceSource>, String> {
    let found = volumes(&Sys::real())
        .into_iter()
        .find(|v| v.root.to_string_lossy() == device.id);
    match found {
        Some(v) => open_listed(v),
        None => Err(format!("{} is no longer mounted", device.id)),
    }
}

/// The device node a mount point is mounted from (`/dev/sdb1`), from the mount table. Eject uses
/// it as a second way to find the drive.
pub(super) fn device_of_mount(mount_point: &str) -> Option<String> {
    let text = fs::read_to_string(Sys::real().mountinfo).ok()?;
    parse_mountinfo(&text)
        .into_iter()
        .find(|e| e.mount_point == mount_point)
        .map(|e| e.source)
}

fn volumes(sys: &Sys) -> Vec<VolumeDetails> {
    match fs::read_to_string(&sys.mountinfo) {
        Ok(text) => volumes_from(sys, &text),
        Err(e) => {
            debug!("Could not read {}: {e}", sys.mountinfo.display());
            Vec::new()
        }
    }
}

/// One line of `mountinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MountEntry {
    /// `major:minor` of the device.
    dev: String,
    /// The part of the file system that is mounted: `/` for a whole one.
    root: String,
    mount_point: String,
    fs_type: String,
    source: String,
}

/// `\040` in mountinfo fields stands for a space (and `\011`, `\012`, `\134` for tab, newline,
/// backslash): the kernel writes those characters as three octal digits.
fn unescape_octal(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1..=i + 3]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b))
        {
            let v = u32::from(bytes[i + 1] - b'0') * 64
                + u32::from(bytes[i + 2] - b'0') * 8
                + u32::from(bytes[i + 3] - b'0');
            if let Ok(b) = u8::try_from(v) {
                out.push(b);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Reads `/proc/self/mountinfo`:
/// `id parent major:minor root mount-point options [tags] - fstype source super-options`.
fn parse_mountinfo(text: &str) -> Vec<MountEntry> {
    text.lines()
        .filter_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let l: Vec<&str> = left.split_whitespace().collect();
            let r: Vec<&str> = right.split_whitespace().collect();
            if l.len() < 6 || r.len() < 2 {
                return None;
            }
            Some(MountEntry {
                dev: l[2].to_string(),
                root: unescape_octal(l[3]),
                mount_point: unescape_octal(l[4]),
                fs_type: r[0].to_string(),
                source: unescape_octal(r[1]),
            })
        })
        .collect()
}

/// udev writes odd characters in link names as `\xNN` (a space is `\x20`).
fn unescape_udev(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1] == b'x'
            && let Some(hex) = s.get(i + 2..i + 4)
            && let Ok(v) = u8::from_str_radix(hex, 16)
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Takes out of a volume label what could mislead or break a display: direction marks, control
/// characters and `/`.
fn clean_label(s: &str) -> String {
    crate::device::strip_direction_marks(s)
        .chars()
        .filter(|c| !c.is_control() && *c != '/')
        .collect()
}

/// What sysfs says about a block device.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct BlockInfo {
    /// The partition's name, e.g. `sdb1`.
    name: String,
    /// The whole disk's name, e.g. `sdb`.
    disk: String,
    removable: bool,
    sd_card: bool,
    usb_vendor: Option<String>,
    usb_product: Option<String>,
    usb_serial: Option<String>,
    usb_maker: Option<String>,
    usb_product_name: Option<String>,
    model: Option<String>,
}

fn read_trimmed(path: &Path) -> Option<String> {
    let s = fs::read_to_string(path).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// The sysfs facts about the block device at `sysfs_path` (a `/sys/...` link or folder). Walks
/// up from it to the USB device, and notes the `removable` flag of the disk.
fn inspect_block(sysfs_path: &Path) -> Option<BlockInfo> {
    let real = fs::canonicalize(sysfs_path).ok()?;
    let name = real.file_name()?.to_string_lossy().into_owned();
    let is_partition = real.join("partition").exists();
    let disk_dir = if is_partition {
        real.parent()?.to_path_buf()
    } else {
        real.clone()
    };
    let mut info = BlockInfo {
        disk: disk_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        name,
        removable: read_trimmed(&disk_dir.join("removable")).as_deref() == Some("1"),
        model: read_trimmed(&disk_dir.join("device/model")),
        ..BlockInfo::default()
    };
    for dir in real.ancestors() {
        if read_trimmed(&dir.join("type")).as_deref() == Some("SD") {
            info.sd_card = true;
        }
        if dir.join("idVendor").is_file() {
            info.usb_vendor = read_trimmed(&dir.join("idVendor")).map(|s| s.to_lowercase());
            info.usb_product = read_trimmed(&dir.join("idProduct")).map(|s| s.to_lowercase());
            info.usb_serial = read_trimmed(&dir.join("serial"));
            info.usb_maker = read_trimmed(&dir.join("manufacturer"));
            info.usb_product_name = read_trimmed(&dir.join("product"));
            break;
        }
    }
    Some(info)
}

impl BlockInfo {
    fn is_removable_or_usb(&self) -> bool {
        self.removable || self.sd_card || self.usb_vendor.is_some()
    }
}

/// The sysfs location of a mount's device: by `major:minor`, else by the `/dev/<name>` source.
fn sysfs_path_of(sys: &Sys, entry: &MountEntry) -> Option<PathBuf> {
    let by_number = sys.sys_dev_block.join(&entry.dev);
    if by_number.exists() {
        return Some(by_number);
    }
    let name = entry.source.strip_prefix("/dev/")?;
    if name.is_empty() || name.contains('/') {
        return None;
    }
    let by_name = sys.sys_class_block.join(name);
    by_name.exists().then_some(by_name)
}

/// The name of the link in `dir` (one of `/dev/disk/by-*`) that points at device `dev_name`.
fn link_name_for(dir: &Path, dev_name: &str) -> Option<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            fs::read_link(e.path())
                .ok()
                .and_then(|t| t.file_name().map(|n| n.to_string_lossy() == dev_name))
                .unwrap_or(false)
        })
        .map(|e| unescape_udev(&e.file_name().to_string_lossy()))
        .collect();
    names.sort();
    names.into_iter().next()
}

/// The physical disks under a block device. An encrypted or logical volume (`dm-0`, `md0`) lists
/// what it is built on in `slaves/`; follow those down to the partitions and their disks. A
/// plain partition or disk gives its own disk.
fn underlying_disks(sys: &Sys, block_name: &str, depth: usize) -> Vec<String> {
    let dir = sys.sys_class_block.join(block_name);
    let mut slaves: Vec<String> = fs::read_dir(dir.join("slaves"))
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    slaves.sort();
    if slaves.is_empty() || depth == 0 {
        return inspect_block(&dir).map(|b| b.disk).into_iter().collect();
    }
    slaves
        .iter()
        .flat_map(|s| underlying_disks(sys, s, depth - 1))
        .collect()
}

fn volumes_from(sys: &Sys, mountinfo: &str) -> Vec<VolumeDetails> {
    let entries = parse_mountinfo(mountinfo);
    // The disks the running system lives on, through any encryption or volume manager.
    let system_disks: HashSet<String> = entries
        .iter()
        .filter(|e| SYSTEM_MOUNTS.contains(&e.mount_point.as_str()))
        .filter_map(|e| sysfs_path_of(sys, e))
        .filter_map(|p| inspect_block(&p))
        .flat_map(|b| {
            let mut disks = underlying_disks(sys, &b.name, 8);
            disks.push(b.disk);
            disks
        })
        .collect();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for e in &entries {
        if e.root != "/"
            || SYSTEM_MOUNTS.contains(&e.mount_point.as_str())
            || !FILE_SYSTEMS.contains(&e.fs_type.as_str())
            || !e.source.starts_with("/dev/")
        {
            continue;
        }
        let Some(block) = sysfs_path_of(sys, e).and_then(|p| inspect_block(&p)) else {
            continue;
        };
        if !block.is_removable_or_usb() || system_disks.contains(&block.disk) {
            continue;
        }
        if !seen.insert(e.dev.clone()) {
            continue; // mounted in two places: offer it once
        }
        let uuid = link_name_for(&sys.dev_disk.join("by-uuid"), &block.name);
        let partuuid = link_name_for(&sys.dev_disk.join("by-partuuid"), &block.name);
        let label = link_name_for(&sys.dev_disk.join("by-label"), &block.name)
            .map(|l| clean_label(&l))
            .filter(|l| !l.trim().is_empty());
        let root = PathBuf::from(&e.mount_point);
        let signals = Signals {
            volume_serial: uuid,
            gpt_partition_id: partuuid,
            usb_serial: block.usb_serial.clone(),
            usb_vendor: block.usb_vendor.clone(),
            usb_product: block.usb_product.clone(),
            label: label.clone(),
            capacity: statvfs::capacity(&root),
            ..Signals::default()
        };
        info!(
            "Found the volume {} ({} on {})",
            e.mount_point, e.fs_type, e.source
        );
        out.push(VolumeDetails {
            root,
            label,
            file_system: Some(e.fs_type.clone()),
            maker: block.usb_maker.clone(),
            model: block.usb_product_name.clone().or(block.model.clone()),
            signals,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
22 1 259:2 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p2 rw,errors=remount-ro
23 22 259:1 / /boot/efi rw,relatime shared:2 - vfat /dev/nvme0n1p1 rw,fmask=0077
40 22 0:35 / /sys rw,nosuid shared:3 - sysfs sysfs rw
88 22 8:17 / /media/user/MY\\040STICK rw,nosuid,nodev,relatime shared:9 - vfat /dev/sdb1 rw,uid=1000
89 22 8:17 / /mnt/second rw,relatime shared:10 - vfat /dev/sdb1 rw
90 22 8:33 /sub /mnt/bind rw - ext4 /dev/sdc1 rw
91 22 8:49 / /media/user/internal-data rw - ext4 /dev/sdd1 rw
";

    #[test]
    fn mountinfo_lines_are_parsed_with_escapes_undone() {
        let e = parse_mountinfo(SAMPLE);
        assert_eq!(e.len(), 7);
        assert_eq!(
            e[3],
            MountEntry {
                dev: "8:17".into(),
                root: "/".into(),
                mount_point: "/media/user/MY STICK".into(),
                fs_type: "vfat".into(),
                source: "/dev/sdb1".into(),
            }
        );
        assert_eq!(e[5].root, "/sub");
        assert!(parse_mountinfo("garbage\n\n12 3 - -").is_empty());
    }

    #[test]
    fn escapes_in_mount_points_and_udev_names() {
        assert_eq!(unescape_octal(r"a\040b\134c\011"), "a b\\c\t");
        assert_eq!(unescape_octal(r"trailing\04"), r"trailing\04");
        assert_eq!(unescape_octal(r"\999"), r"\999");
        assert_eq!(unescape_udev(r"My\x20Stick\x2d1"), "My Stick-1");
        assert_eq!(unescape_udev(r"bad\xZZ"), r"bad\xZZ");
        assert_eq!(unescape_udev(r"end\x2"), r"end\x2");
    }

    /// A fake sysfs and /dev/disk: a USB stick `sdb` (partition `sdb1`), a fixed disk `sdd`
    /// with no USB above it, an internal disk `nvme0n1`, an SD card in a built-in reader.
    struct Fake {
        _t: tempfile::TempDir,
        sys: Sys,
    }

    fn put(p: &Path, text: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    fn link(from: &Path, to: &str) {
        fs::create_dir_all(from.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(to, from).unwrap();
    }

    fn fake() -> Fake {
        let t = tempfile::tempdir().unwrap();
        let r = fs::canonicalize(t.path()).unwrap();
        let dev = r.join("sys/devices/pci0000:00/0000:00:14.0/usb1/1-2");
        put(&dev.join("idVendor"), "0951\n");
        put(&dev.join("idProduct"), "1666\n");
        put(&dev.join("serial"), "001CC0EC3412B0A1\n");
        put(&dev.join("manufacturer"), "Kingston \n");
        put(&dev.join("product"), "DataTraveler 3.0\n");
        let sdb = dev.join("1-2:1.0/host6/target6:0:0:0/6:0:0:0/block/sdb");
        put(&sdb.join("removable"), "1\n");
        put(&sdb.join("sdb1/partition"), "1\n");
        // A non-removable disk on a plain SATA path, no USB above it.
        let sdd =
            r.join("sys/devices/pci0000:00/0000:00:1f.2/ata1/host0/target0:0:0/0:0:0:0/block/sdd");
        put(&sdd.join("removable"), "0\n");
        put(&sdd.join("sdd1/partition"), "1\n");
        // The system disk.
        let nvme = r.join("sys/devices/pci0000:00/0000:00:1d.0/nvme/nvme0/nvme0n1");
        put(&nvme.join("removable"), "0\n");
        put(&nvme.join("nvme0n1p2/partition"), "2\n");
        put(&nvme.join("nvme0n1p1/partition"), "1\n");
        // An SD card in the laptop's own reader.
        let mmc = r.join("sys/devices/pci0000:00/0000:00:1c.0/mmc_host/mmc0/mmc0:aaaa");
        put(&mmc.join("type"), "SD\n");
        put(&mmc.join("block/mmcblk0/removable"), "0\n");
        put(&mmc.join("block/mmcblk0/mmcblk0p1/partition"), "1\n");
        // A second USB disk that the running system itself lives on (a live stick).
        let live = r.join("sys/devices/pci0000:00/0000:00:14.0/usb1/1-3");
        put(&live.join("idVendor"), "0781\n");
        put(&live.join("idProduct"), "5581\n");
        let sde = live.join("1-3:1.0/host7/target7:0:0:0/7:0:0:0/block/sde");
        put(&sde.join("removable"), "1\n");
        put(&sde.join("sde1/partition"), "1\n");
        put(&sde.join("sde2/partition"), "2\n");
        let cls = r.join("sys/class/block");
        link(&cls.join("sde1"), sde.join("sde1").to_str().unwrap());
        link(&cls.join("sde2"), sde.join("sde2").to_str().unwrap());
        link(&cls.join("sdb1"), sdb.join("sdb1").to_str().unwrap());
        link(&cls.join("sdd1"), sdd.join("sdd1").to_str().unwrap());
        link(
            &cls.join("nvme0n1p2"),
            nvme.join("nvme0n1p2").to_str().unwrap(),
        );
        link(
            &cls.join("nvme0n1p1"),
            nvme.join("nvme0n1p1").to_str().unwrap(),
        );
        link(
            &cls.join("mmcblk0p1"),
            mmc.join("block/mmcblk0/mmcblk0p1").to_str().unwrap(),
        );
        let by = r.join("dev/disk");
        link(&by.join("by-uuid/ABCD-1234"), "../../sdb1");
        link(&by.join("by-uuid/9f1c0000-aaaa"), "../../sdd1");
        link(&by.join("by-partuuid/0a1b2c3d-01"), "../../sdb1");
        link(&by.join("by-label/MY\\x20STICK"), "../../sdb1");
        link(&by.join("by-uuid/5555-6666"), "../../mmcblk0p1");
        let sys = Sys {
            mountinfo: r.join("mountinfo"),
            sys_dev_block: r.join("sys/dev/block"),
            sys_class_block: cls,
            dev_disk: by,
        };
        Fake { _t: t, sys }
    }

    #[test]
    fn a_usb_stick_is_found_with_its_whole_identity_and_nothing_else_is() {
        let f = fake();
        let mut text = SAMPLE.to_string();
        text.push_str("92 22 179:1 / /media/user/CARD rw - vfat /dev/mmcblk0p1 rw\n");
        let v = volumes_from(&f.sys, &text);
        let names: Vec<_> = v
            .iter()
            .map(|d| d.root.to_string_lossy().into_owned())
            .collect();
        // Not /, /boot/efi, sysfs, the second mount of one device, the plain SATA disk, the
        // bind mount; the stick once, and the built-in card reader's SD card.
        assert_eq!(
            names,
            ["/media/user/MY STICK", "/media/user/CARD"],
            "{names:?}"
        );
        let s = &v[0];
        assert_eq!(s.label.as_deref(), Some("MY STICK"));
        assert_eq!(s.file_system.as_deref(), Some("vfat"));
        assert_eq!(s.maker.as_deref(), Some("Kingston"));
        assert_eq!(s.model.as_deref(), Some("DataTraveler 3.0"));
        let g = &s.signals;
        assert_eq!(g.volume_serial.as_deref(), Some("ABCD-1234"));
        assert_eq!(g.gpt_partition_id.as_deref(), Some("0a1b2c3d-01"));
        assert_eq!(g.usb_vendor.as_deref(), Some("0951"));
        assert_eq!(g.usb_product.as_deref(), Some("1666"));
        assert_eq!(g.usb_serial.as_deref(), Some("001CC0EC3412B0A1"));
        assert_eq!(v[1].signals.volume_serial.as_deref(), Some("5555-6666"));
        assert_eq!(v[1].signals.usb_vendor, None);
    }

    #[test]
    fn a_partition_on_the_system_disk_is_never_offered() {
        let f = fake();
        let text = "\
22 1 259:2 / / rw - ext4 /dev/nvme0n1p2 rw
23 22 259:1 / /mnt/windows rw - ext4 /dev/nvme0n1p1 rw
";
        assert!(volumes_from(&f.sys, text).is_empty());
    }

    #[test]
    fn another_partition_of_a_usb_disk_the_system_runs_from_is_never_offered() {
        let f = fake();
        let text = "\
22 1 8:65 / / rw - ext4 /dev/sde1 rw
23 22 8:66 / /mnt/data rw - ext4 /dev/sde2 rw
88 22 8:17 / /mnt/stick rw - vfat /dev/sdb1 rw
";
        let v = volumes_from(&f.sys, text);
        let names: Vec<_> = v
            .iter()
            .map(|d| d.root.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["/mnt/stick"]);
    }

    /// Encrypted or logical volumes: `/` on `dm-0`, built on a partition of a USB disk. The disk's
    /// other partitions belong to the system, whatever the mapper device is called.
    #[test]
    fn partitions_under_an_encrypted_or_logical_system_volume_are_never_offered() {
        let f = fake();
        let root = f
            .sys
            .sys_class_block
            .ancestors()
            .nth(3)
            .unwrap()
            .to_path_buf();
        let virt = root.join("sys/devices/virtual/block");
        // dm-1 (LVM) is built on dm-0 (dm-crypt), which is built on sde2 of the live USB disk.
        put(&virt.join("dm-0/slaves/sde2"), "");
        put(&virt.join("dm-1/slaves/dm-0"), "");
        for dm in ["dm-0", "dm-1"] {
            link(
                &f.sys.sys_class_block.join(dm),
                virt.join(dm).to_str().unwrap(),
            );
        }
        link(
            &f.sys.sys_dev_block.join("253:1"),
            f.sys.sys_class_block.join("dm-1").to_str().unwrap(),
        );
        let text = "\
22 1 253:1 / / rw - ext4 /dev/mapper/vg-root rw
23 22 8:65 / /mnt/data rw - ext4 /dev/sde1 rw
88 22 8:17 / /mnt/stick rw - vfat /dev/sdb1 rw
";
        let names: Vec<_> = volumes_from(&f.sys, text)
            .iter()
            .map(|d| d.root.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["/mnt/stick"]);
    }

    #[test]
    fn more_of_the_systems_own_mounts_are_known() {
        for m in ["/boot/firmware", "/nix", "/root", "/var/lib"] {
            assert!(SYSTEM_MOUNTS.contains(&m), "{m}");
        }
    }

    #[test]
    fn a_label_has_no_direction_marks_controls_or_slashes() {
        assert_eq!(clean_label("MY\u{202E}STICK"), "MYSTICK");
        assert_eq!(clean_label("a/b\u{1}c\nd"), "abcd");
        assert_eq!(clean_label("Photos 2024"), "Photos 2024");
    }

    #[test]
    fn the_device_is_found_by_number_when_the_name_is_no_help() {
        let f = fake();
        let by_num = f.sys.sys_dev_block.join("8:17");
        link(
            &by_num,
            f.sys.sys_class_block.join("sdb1").to_str().unwrap(),
        );
        let text = "88 22 8:17 / /mnt/x rw - fuseblk /dev/mapper/odd rw\n";
        let v = volumes_from(&f.sys, text);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].signals.volume_serial.as_deref(), Some("ABCD-1234"));
    }

    #[test]
    fn a_device_with_no_sysfs_entry_is_skipped() {
        let f = fake();
        let text = "88 22 8:99 / /mnt/x rw - vfat /dev/sdz9 rw\n";
        assert!(volumes_from(&f.sys, text).is_empty());
    }

    #[test]
    fn a_missing_mountinfo_lists_nothing() {
        let f = fake();
        assert!(volumes(&f.sys).is_empty());
    }
}
