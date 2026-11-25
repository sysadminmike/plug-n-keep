//! FreeBSD. Plain POSIX: mounted file systems and the single-instance lock; no portable devices
//! yet.

pub mod fs;
pub mod heic;
pub mod notify;
pub mod volume;

pub use super::statvfs::disk_space;
pub use super::unix::{
    Session, acquire_lock, flush_dir, flush_file, is_link, local_stamp, local_time_ms,
    rename_replace, send_to_bin, settings_dir, utc_stamp, volume_token,
};
pub use fs::rename_no_replace;
