//! Linux. Mounted file systems and the single-instance lock through POSIX, phones through
//! libmtp and AFC, and still cameras through libgphoto2.

pub mod camera;
mod dynlib;
pub mod eject;
pub mod fs;
pub mod heic;
pub mod ios;
pub mod mtp;
pub mod notify;
pub mod volume;

pub use super::statvfs::disk_space;
pub use super::unix::{
    Session, acquire_lock, flush_dir, flush_file, is_link, local_stamp, local_time_ms,
    rename_replace, send_to_bin, settings_dir, utc_stamp, volume_token,
};
pub use fs::rename_no_replace;
