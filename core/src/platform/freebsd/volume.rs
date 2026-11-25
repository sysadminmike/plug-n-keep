//! Sticks, SD cards and drives on FreeBSD: the mounted `/dev/da*` volumes, from `getmntinfo`,
//! with their size from `statvfs`. The label and the USB serial are not read yet, so these
//! volumes are recognised by their size and by the ID file.
//!
//! Which mounts are offered (and which are never, because the system runs from their disk) is
//! decided in `platform::bsd_mounts`, which is tested on every system. This file is compiled and
//! checked on FreeBSD only; there is no device to try it on yet.

use std::ffi::CStr;
use std::path::PathBuf;

use log::{debug, info};

use crate::device::DeviceSource;
use crate::platform::bsd_mounts::{Mount, usb_volumes};
use crate::platform::{ListedDevice, statvfs};
use crate::signals::Signals;
use crate::volume::{VolumeDetails, open_listed};

fn mounts() -> Vec<Mount> {
    let mut list: *mut libc::statfs = std::ptr::null_mut();
    // SAFETY: getmntinfo points `list` at a buffer it owns (valid until the next call; never
    // freed by us) and returns how many entries it holds, or 0 on failure.
    let n = unsafe { libc::getmntinfo(&mut list, libc::MNT_NOWAIT) };
    if n <= 0 || list.is_null() {
        debug!("getmntinfo listed nothing");
        return Vec::new();
    }
    // SAFETY: `list` has `n` entries, per the call above.
    let entries = unsafe { std::slice::from_raw_parts(list, n as usize) };
    entries
        .iter()
        .map(|e| {
            let text = |p: *const libc::c_char| {
                // SAFETY: the names are NUL-terminated arrays inside the entry.
                unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
            };
            Mount {
                source: text(e.f_mntfromname.as_ptr()),
                mount_point: text(e.f_mntonname.as_ptr()),
                fs_type: text(e.f_fstypename.as_ptr()),
            }
        })
        .collect()
}

fn volumes() -> Vec<VolumeDetails> {
    usb_volumes(&mounts())
        .into_iter()
        .map(|m| {
            let root = PathBuf::from(&m.mount_point);
            info!(
                "Found the volume {} ({} on {})",
                m.mount_point, m.fs_type, m.source
            );
            VolumeDetails {
                signals: Signals {
                    capacity: statvfs::capacity(&root),
                    ..Signals::default()
                },
                file_system: Some(m.fs_type),
                root,
                ..VolumeDetails::default()
            }
        })
        .collect()
}

/// The mounted volumes that could be a source.
pub fn list() -> Vec<ListedDevice> {
    volumes().iter().map(VolumeDetails::listed).collect()
}

/// Opens a listed volume as a read-only source. Only a volume that is listed now can be
/// opened, so an ID can't name some other folder.
pub fn open(device: &ListedDevice) -> Result<Box<dyn DeviceSource>, String> {
    match volumes()
        .into_iter()
        .find(|v| v.root.to_string_lossy() == device.id)
    {
        Some(v) => open_listed(v),
        None => Err(format!("{} is no longer mounted", device.id)),
    }
}
