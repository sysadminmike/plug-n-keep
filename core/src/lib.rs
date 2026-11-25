//! Device and backup logic for Plug-n-Keep. No GUI code lives here.
//!
//! - [`device`]: the read-only `DeviceSource` interface everything else uses.
//! - [`platform`]: everything that talks to the operating system or to a phone: moving and
//!   flushing files, link detection, the single-instance lock, the settings folder, the clock,
//!   and listing and opening devices (WPD on Windows).
//! - [`paths`]: validated paths; the only way device names become PC paths.
//! - [`plan`]: comparing device and backup folder into a `ScanPlan`.
//! - [`sync`]: copying a plan's files, never overwriting.
//! - [`space`]: whether a sync fits on the backup disk, worked out before it starts.
//! - [`history`]: the log of every sync, per device.
//! - [`state`]: skip lists, settings, crash record, single-instance lock.
//! - [`owner`]: which device a backup folder belongs to (guards archiving deleted files).
//! - [`signals`]: what tells devices apart, and how strong each signal is.
//! - [`recognise`]: matching a plugged-in device against the known ones (pure logic).
//! - [`heic`]: JPG copies of HEIC photos, and the list of the ones the app made.
//! - [`jpgs`]: when to make them: the sync's JPG step, "Try again" and the backfill.
//! - [`folder_record`]: a device's own record and history, kept in its backup folder.
//! - [`registry`]: the list of known devices (their records in the backup folders, pointers in
//!   `devices.json`), and "Find my backups".
//!
//! Every module reports what it does through the `log` facade (plain sentences; local paths and
//! file names, never file contents). The app installs the logger; [`local_time_ms`] is its clock.
//!
//! `unsafe` is confined to `platform`; every other module forbids it.

pub mod device;
pub mod folder_record;
pub mod heic;
pub mod history;
pub mod idfile;
pub mod jpgs;
pub mod owner;
pub mod paths;
pub mod plan;
pub mod platform;
pub mod recognise;
pub mod registry;
pub mod signals;
pub mod space;
pub mod state;
pub mod sync;
pub mod volume;

#[cfg(any(test, feature = "fake-device"))]
pub mod fake;

pub use platform::{local_time_ms, utc_stamp};

/// "1 file", "2 files": a count with its noun, for log lines.
pub(crate) fn counted(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

#[cfg(test)]
mod e2e_tests;
#[cfg(test)]
mod testutil;
