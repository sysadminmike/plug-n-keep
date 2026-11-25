//! Windows file-system pieces: moving a file into place *without* replacing anything already
//! there (and an explicit, atomic replace), link detection, flushing, the single-instance lock
//! and the local clock. Both moves are write-through, so the move is on disk before they return.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use ::windows::Win32::Storage::FileSystem::{
    MOVE_FILE_FLAGS, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use ::windows::core::PCWSTR;

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const ERROR_SHARING_VIOLATION: i32 = 32;

/// Fails with `ErrorKind::AlreadyExists` if `to` exists.
pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    move_file(from, to, MOVEFILE_WRITE_THROUGH)
}

/// Atomically replaces `to` if it exists. There is never a moment with neither file.
pub fn rename_replace(from: &Path, to: &Path) -> io::Result<()> {
    move_file(
        from,
        to,
        MOVE_FILE_FLAGS(MOVEFILE_REPLACE_EXISTING.0 | MOVEFILE_WRITE_THROUGH.0),
    )
}

fn move_file(from: &Path, to: &Path, flags: MOVE_FILE_FLAGS) -> io::Result<()> {
    let f: Vec<u16> = from
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let t: Vec<u16> = to
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: both buffers are NUL-terminated and outlive the call.
    unsafe { MoveFileExW(PCWSTR(f.as_ptr()), PCWSTR(t.as_ptr()), flags) }.map_err(|e| {
        let code = e.code().0 as u32;
        if code & 0xFFFF_0000 == 0x8007_0000 {
            io::Error::from_raw_os_error((code & 0xFFFF) as i32)
        } else {
            io::Error::other(e)
        }
    })
}

/// Whether metadata (from `symlink_metadata`) describes a symlink, junction or other reparse point.
pub fn is_link(meta: &fs::Metadata) -> bool {
    meta.file_type().is_symlink() || meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

/// A number that tells the volume `path` is on from any other: its serial number (high half)
/// together with a hash of its volume GUID path (low half). Two cloned sticks share a serial,
/// and some report 0, but not a GUID path. It is only ever compared within one run of the app
/// (see [`crate::platform::volume_token`]), so its form can change freely.
pub fn volume_token(path: &Path) -> io::Result<u64> {
    use ::windows::Win32::Storage::FileSystem::{
        GetVolumeInformationW, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
    };
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut volume = [0u16; 1024];
    // SAFETY: `wide` is NUL-terminated and outlives the call; `volume` is a buffer of its length.
    unsafe { GetVolumePathNameW(PCWSTR(wide.as_ptr()), &mut volume) }.map_err(io::Error::other)?;
    let mut serial = 0u32;
    // SAFETY: `volume` was filled in above and is NUL-terminated; `serial` is valid to write.
    unsafe {
        GetVolumeInformationW(
            PCWSTR(volume.as_ptr()),
            None,
            Some(&mut serial),
            None,
            None,
            None,
        )
    }
    .map_err(io::Error::other)?;
    // The GUID path, `\?\Volume{...}\`. If it can't be had, the serial alone is the token.
    let mut guid = [0u16; 64];
    // SAFETY: `volume` is NUL-terminated; `guid` is a buffer of its length.
    let hashed =
        match unsafe { GetVolumeNameForVolumeMountPointW(PCWSTR(volume.as_ptr()), &mut guid) } {
            Ok(()) => {
                let len = guid.iter().position(|&c| c == 0).unwrap_or(guid.len());
                let mut h: u32 = 0x811c_9dc5;
                for &c in &guid[..len] {
                    h = (h ^ u32::from(c)).wrapping_mul(0x0100_0193);
                }
                h
            }
            Err(_) => 0,
        };
    Ok((u64::from(serial) << 32) | u64::from(hashed))
}

/// Forces a file's data to disk.
pub fn flush_file(file: &File) -> io::Result<()> {
    file.sync_all()
}

/// Windows has no folder flush; the moves above are write-through instead.
pub fn flush_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

/// Opens `path` (creating it) so that nobody else can open it until the returned file is
/// dropped. `Ok(None)` if someone else already holds it.
pub fn acquire_lock(path: &Path) -> io::Result<Option<File>> {
    match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(path)
    {
        Ok(f) => Ok(Some(f)),
        Err(e) if e.raw_os_error() == Some(ERROR_SHARING_VIOLATION) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The local date and time as a folder name, e.g. `2026-11-16 15-30-05` (no `:`, which Windows
/// doesn't allow in names).
pub fn local_stamp() -> String {
    // SAFETY: GetLocalTime only fills in the returned struct.
    let t = unsafe { ::windows::Win32::System::SystemInformation::GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}-{:02}-{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

/// The current UTC time as text, e.g. `2026-11-17T18:00:00Z`, for the marker and the device list.
pub fn utc_stamp() -> String {
    // SAFETY: GetSystemTime only fills in the returned struct.
    let t = unsafe { ::windows::Win32::System::SystemInformation::GetSystemTime() };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

/// The local time of day in milliseconds since midnight, for time-stamping log lines.
pub fn local_time_ms() -> u32 {
    // SAFETY: GetLocalTime only fills in the returned struct.
    let t = unsafe { ::windows::Win32::System::SystemInformation::GetLocalTime() };
    ((u32::from(t.wHour) * 60 + u32::from(t.wMinute)) * 60 + u32::from(t.wSecond)) * 1000
        + u32::from(t.wMilliseconds)
}

/// The size and free space of the disk `path` is on: `GetDiskFreeSpaceExW`. "Free" is what this
/// user can use (quotas count), which is what a copy is limited by. Only reads.
pub fn disk_space(path: &Path) -> Option<super::super::DiskSpace> {
    use ::windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    // The call wants a folder; a file's own folder will do.
    let dir = if path.is_dir() { path } else { path.parent()? };
    let w: Vec<u16> = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let (mut free, mut total) = (0u64, 0u64);
    // SAFETY: `w` is NUL-terminated and outlives the call; the two outputs are plain integers
    // that live through it; the third (total free) is not wanted.
    unsafe { GetDiskFreeSpaceExW(PCWSTR(w.as_ptr()), Some(&mut free), Some(&mut total), None) }
        .ok()?;
    (total > 0).then_some(super::super::DiskSpace { total, free })
}
