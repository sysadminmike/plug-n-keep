//! Copying planned files from the device into the backup folder.
//!
//! Rules:
//! - sync takes a [`ScanPlan`] and refuses if the device or the backup folder changed;
//! - each file is re-checked on the device just before copying;
//! - nothing in the backup is ever deleted or overwritten: a changed file (or one the user asked
//!   to copy again, [`CopyMode::Replace`]) is copied to a temporary file first; the old copy then
//!   moves out of the way (into the archive, renamed beside the new one, or into the Recycle Bin
//!   or Trash, by the device's [`DeletedFiles`] choice) and the new one into its place, moves
//!   that never replace anything, with a crash record that lets the next start finish them;
//! - a changed file's old copy is moved, so it waits for the same confirmation as moving files
//!   deleted from the device when the plan asks for one ([`ScanPlan::cautions`]);
//! - data goes to a temporary file, is flushed to disk, then moved into place;
//! - if the move fails, the finished temporary file is kept and reported;
//! - a JPG copy the app made of a HEIC goes the way its HEIC goes (DESIGN.md section 7.6): when
//!   the HEIC is deleted from the device, or changes, its JPG is moved first, by the same
//!   no-replace moves, and then the HEIC. A made JPG is never "deleted from the device" itself;
//! - a user skip adds the file to the skip list for this device and folder only;
//! - a crash leaves a record that the next start uses to clean up, without skipping.
#![forbid(unsafe_code)]

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use log::{debug, error, info, warn};

use crate::counted;
use crate::device::{DeviceSource, ObjectInfo};
use crate::heic::made::{MadeEntry, MadeList};
use crate::owner::{self, FolderOwner, OwnerState};
use crate::paths::{SafeRelPath, TEMP_SUFFIX, is_link, prepare_parent};
use crate::plan::{MAX_NUMBERED, MadeJpg, PlanEntry, Removed, ScanPlan, Status};
use crate::platform;
use crate::registry::{ArchiveLayout, DeletedFiles};
use crate::space::{self, ShortOfSpace};
use crate::state::{InProgress, OldCopyGoes, ReplaceStep, Scope, StateDir};

const BUFFER: usize = 256 * 1024;
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMode {
    /// Never touch an existing file: a new file is written to a free name; a changed file's old
    /// copy moves into the archive first.
    NoReplace,
    /// The user asked to copy a backed-up file again. The existing copy is not overwritten: the
    /// new copy is made first, then the old one moves into the archive and the new one into its
    /// place, the same as for a changed file.
    Replace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncItem {
    /// Index into [`ScanPlan::entries`].
    pub entry: usize,
    pub mode: CopyMode,
}

/// Flags the UI sets while a sync runs.
#[derive(Debug, Clone)]
pub struct SyncControl {
    pub cancel: Arc<AtomicBool>,
    /// Skip the file being copied, if it is entry `skip_entry`. The window may be a file or two
    /// behind the copy, so a bare "skip the current file" could hit the wrong one.
    pub skip_current: Arc<AtomicBool>,
    pub skip_entry: Arc<AtomicUsize>,
}

impl Default for SyncControl {
    fn default() -> Self {
        Self {
            cancel: Arc::default(),
            skip_current: Arc::default(),
            skip_entry: Arc::new(AtomicUsize::new(usize::MAX)),
        }
    }
}

impl SyncControl {
    /// Ask to skip plan entry `entry`, if it is still the one being copied.
    pub fn request_skip(&self, entry: usize) {
        self.skip_entry.store(entry, Ordering::SeqCst);
        self.skip_current.store(true, Ordering::SeqCst);
    }

    fn skip_requested_for(&self, entry: usize) -> bool {
        self.skip_current.load(Ordering::SeqCst) && self.skip_entry.load(Ordering::SeqCst) == entry
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SyncEvent {
    FileStarted {
        entry: usize,
        ordinal: usize,
        total: usize,
    },
    Progress {
        entry: usize,
        bytes: u64,
        total: Option<u64>,
        bytes_per_sec: f64,
    },
    Copied {
        entry: usize,
        target: SafeRelPath,
        bytes: u64,
        /// For a changed file: where its old copy went.
        old_copy: Option<OldCopy>,
    },
    /// An identical-size file turned up at the target since the scan; nothing was copied.
    AlreadyBackedUp {
        entry: usize,
        target: SafeRelPath,
    },
    /// Skipped by the user; now on the skip list.
    Skipped {
        entry: usize,
    },
    Failed {
        entry: usize,
        error: String,
    },
}

/// Where the old copy of a changed file went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OldCopy {
    /// Into the archive, at this path in the backup folder.
    Archived(SafeRelPath),
    /// Renamed beside the new copy, at this path in the backup folder.
    RenamedAside(SafeRelPath),
    /// Into the Recycle Bin or Trash.
    Binned,
    /// The bin reported a failure, but the old copy is gone from its place anyway. It may or
    /// may not be in the bin.
    Missing,
}

impl OldCopy {
    /// "moved into the archive as ...", for the log and messages.
    pub fn describe(&self) -> String {
        match self {
            OldCopy::Archived(to) => format!("moved into the archive as {to}"),
            OldCopy::RenamedAside(to) => format!("kept beside it as {to}"),
            OldCopy::Binned => format!("sent to the {}", platform::BIN_NAME),
            OldCopy::Missing => format!(
                "gone from its place, and may not be in the {}",
                platform::BIN_NAME
            ),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub copied: usize,
    /// Of those, changed files whose old copy was moved out of the way (archived, renamed
    /// aside or sent to the bin).
    pub replaced: usize,
    /// Bytes copied.
    pub copied_bytes: u64,
    pub already_backed_up: usize,
    pub skipped: usize,
    pub failed: usize,
    pub cancelled: bool,
    /// A transfer was abandoned part-way or delivered the wrong number of bytes, so the sync
    /// stopped there. Close the device and open it again, then scan again, before copying
    /// more: after Stop part-way through a file, an iPhone was seen answering every later
    /// request with the file requested before it.
    pub must_reconnect: bool,
    /// The backup disk filled up. The file being copied was removed (it is on the device) and the
    /// sync stopped there; nothing already copied was touched.
    pub disk_full: bool,
    /// A file failed because the connection to the device was lost (the error that says it is
    /// kept here), so the sync stopped there: nothing more can be read from this session.
    pub connection_lost: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncError {
    DeviceChanged {
        scanned: String,
        connected: String,
    },
    RootChanged {
        scanned: PathBuf,
        now: PathBuf,
    },
    /// The backup folder belongs to another device (or its owner marker can't be read), so
    /// nothing is copied into it or archived in it. The message says why.
    WrongFolder(String),
    /// Moving files out of place needs an explicit OK, for these reasons; nothing was moved.
    NeedsConfirmation(Vec<String>),
    /// The files to copy won't fit on the backup disk with the margin ([`crate::space`]); nothing
    /// was copied.
    NotEnoughSpace(ShortOfSpace),
    Io(String),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::DeviceChanged { scanned, connected } => write!(
                f,
                "this is not the device that was scanned (scanned {scanned}, connected {connected}); scan again"
            ),
            SyncError::RootChanged { scanned, now } => write!(
                f,
                "the backup folder now resolves to {} instead of {}; scan again",
                now.display(),
                scanned.display()
            ),
            SyncError::WrongFolder(why) => {
                write!(f, "{why}; nothing was copied or archived")
            }
            SyncError::NeedsConfirmation(why) => write!(
                f,
                "moving files needs an explicit OK first ({}); nothing was moved",
                why.join("; ")
            ),
            SyncError::NotEnoughSpace(short) => write!(f, "{short} Nothing was copied."),
            SyncError::Io(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for SyncError {}

/// Copy `items` from `source` according to `plan`. `stamp` names this sync's dated archive
/// folder ([`archive_stamp`]); the old copies of changed files go into it, and so should the
/// files [`archive_removed`] moves, so a sync has one dated folder.
///
/// `confirmed` is the user's OK to the reasons archiving asks first ([`ScanPlan::cautions`]).
/// Without it, when the plan has any such reason, a changed file fails with "needs
/// confirmation" and nothing is moved; new files are still copied.
#[allow(clippy::too_many_arguments)]
pub fn run_sync(
    source: &mut dyn DeviceSource,
    plan: &ScanPlan,
    items: &[SyncItem],
    state: &StateDir,
    ctl: &SyncControl,
    stamp: &str,
    confirmed: bool,
    on_event: &mut dyn FnMut(SyncEvent),
) -> Result<SyncReport, SyncError> {
    let connected = source.identity().key.clone();
    if connected != plan.device_key {
        warn!(
            "Sync refused: the connected device ({connected}) is not the one that was scanned ({})",
            plan.device_key
        );
        return Err(SyncError::DeviceChanged {
            scanned: plan.device_key.clone(),
            connected,
        });
    }
    info!(
        "Sync started: {} from {} into {}",
        counted(items.len(), "file", "files"),
        plan.device_label,
        plan.root.display()
    );
    fs::create_dir_all(&plan.root).map_err(|e| {
        error!(
            "Could not create the backup folder {}: {e}",
            plan.root.display()
        );
        SyncError::Io(format!(
            "could not create the backup folder {}: {e}",
            plan.root.display()
        ))
    })?;
    let root_canon = checked_root(plan)?;
    // A backup folder belongs to one device: never copy one device's files into another's.
    check_owner(plan, &root_canon)?;
    // Will it fit? Refused before anything is written, with the margin kept free.
    check_space(plan, items, &root_canon)?;
    // The first sync into a folder records which device it belongs to (see `owner`), and every
    // sync records the archive name it uses. If that can't be written, new files are still
    // copied, but nothing is moved into an archive: the marker must list the name first, or a
    // later scan could take the archive for backup files.
    let archive_ready = match record_archive_name(plan, &root_canon, confirmed) {
        Ok(()) => Ok(()),
        Err(e) => {
            warn!("{e}; copying new files anyway, but no file is moved into the archive");
            Err(e)
        }
    };
    let needs_ok = if confirmed {
        Vec::new()
    } else {
        blocking_reasons(plan)
    };
    // The device has a JPG of its own where the app made one, and the backup already holds that
    // very file: it is the device's now, so the list stops calling it the app's.
    forget_made(
        &root_canon,
        plan.entries
            .iter()
            .filter(|e| e.made_jpg && e.status == Status::Verified)
            .map(|e| e.target.as_str()),
    );
    let scope = Scope::new(&plan.device_key, &plan.root);
    let mut report = SyncReport::default();
    let started = Instant::now();
    let mut copied_bytes: u64 = 0;
    let mut landed_on_jpeg: Vec<String> = Vec::new();

    for (ordinal, item) in items.iter().enumerate() {
        if ctl.cancel.load(Ordering::Relaxed) {
            info!(
                "Sync stopped (Stop sync) before file {} of {}",
                ordinal + 1,
                items.len()
            );
            report.cancelled = true;
            break;
        }
        ctl.skip_current.store(false, Ordering::Relaxed);
        let Some(entry) = plan.entries.get(item.entry) else {
            error!("Failed: entry {} is not in the scan", item.entry);
            report.failed += 1;
            on_event(SyncEvent::Failed {
                entry: item.entry,
                error: "no such entry in the scan".into(),
            });
            continue;
        };
        debug!(
            "Copying file {} of {}: {} ({}){}",
            ordinal + 1,
            items.len(),
            entry.source,
            entry.size.map_or_else(|| "size unknown".into(), human),
            if item.mode == CopyMode::Replace {
                ", over its backup copy"
            } else {
                ""
            }
        );
        on_event(SyncEvent::FileStarted {
            entry: item.entry,
            ordinal: ordinal + 1,
            total: items.len(),
        });
        let file_started = Instant::now();
        let ctx = FileCtx {
            root: &plan.root,
            root_canon: &root_canon,
            device: &plan.device_key,
            index: item.entry,
            entry,
            mode: item.mode,
            plan,
            stamp,
            archive_ready: &archive_ready,
            needs_ok: &needs_ok,
        };
        let mut unfinished = false;
        match copy_one(source, &ctx, state, ctl, on_event, &mut unfinished) {
            Outcome::Copied(target, bytes, old_copy) => {
                let secs = file_started.elapsed().as_secs_f64();
                copied_bytes += bytes;
                let as_name = if target == entry.source {
                    String::new()
                } else {
                    format!(" as {target}")
                };
                info!(
                    "Copied {}{as_name}: {} in {secs:.1} s ({})",
                    entry.source,
                    human(bytes),
                    rate(bytes, secs)
                );
                if let Some(old) = &old_copy {
                    info!("The old copy of {} was {}", entry.source, old.describe());
                    report.replaced += 1;
                }
                // A copy that lands on a JPEG's name is the device's own, whatever the made list
                // says of that name (a replacement of a made JPG, or a new file at a name an
                // entry was left for): the list stops calling it the app's, below.
                if crate::heic::is_jpeg_name(target.file_name()) {
                    landed_on_jpeg.push(target.as_str().to_string());
                }
                report.copied += 1;
                on_event(SyncEvent::Copied {
                    entry: item.entry,
                    target,
                    bytes,
                    old_copy,
                });
            }
            Outcome::Already(target) => {
                info!("Already backed up: {} (as {target})", entry.source);
                report.already_backed_up += 1;
                on_event(SyncEvent::AlreadyBackedUp {
                    entry: item.entry,
                    target,
                });
            }
            Outcome::Skipped => match state.skip_add(&scope, &entry.source) {
                Ok(()) => {
                    warn!("Skipped {} part-way (Skip current)", entry.source);
                    report.skipped += 1;
                    on_event(SyncEvent::Skipped { entry: item.entry });
                }
                Err(e) => {
                    let error = format!("skipped, but the skip list could not be saved: {e}");
                    error!("Failed: {}: {error}", entry.source);
                    report.failed += 1;
                    on_event(SyncEvent::Failed {
                        entry: item.entry,
                        error,
                    });
                }
            },
            Outcome::Cancelled => {
                info!(
                    "Sync stopped (Stop sync) part-way through {}; its unfinished copy was removed",
                    entry.source
                );
                report.cancelled = true;
            }
            Outcome::Failed(error) => {
                error!("Failed: {}: {error}", entry.source);
                report.failed += 1;
                if crate::device::is_connection_lost(&error) {
                    warn!("The connection to the device was lost, so the sync stops here");
                    report.connection_lost = Some(error.clone());
                }
                on_event(SyncEvent::Failed {
                    entry: item.entry,
                    error,
                });
            }
            Outcome::DiskFull(error) => {
                error!("Failed: {}: {error}", entry.source);
                report.failed += 1;
                report.disk_full = true;
                on_event(SyncEvent::Failed {
                    entry: item.entry,
                    error,
                });
            }
        }
        // Nothing more over this connection once a transfer didn't finish cleanly: the
        // device may now be one file behind, and a wrong file of the right size would pass.
        // `spoil` makes a `Guarded` connection refuse further reads even if a caller carries on.
        if unfinished {
            warn!(
                "The transfer of {} didn't finish cleanly, so the sync stops here: the connection to the device must be reset before anything else is copied",
                entry.source
            );
            source.spoil();
            report.must_reconnect = true;
        }
        if unfinished || report.cancelled || report.disk_full || report.connection_lost.is_some() {
            break;
        }
    }
    report.copied_bytes = copied_bytes;
    forget_made(&root_canon, landed_on_jpeg.iter().map(String::as_str));
    let r = &report;
    info!(
        "Copying finished in {:.1} s: {} copied ({}), {} already backed up, {} skipped, {} failed{}{}{}",
        started.elapsed().as_secs_f64(),
        r.copied,
        human(copied_bytes),
        r.already_backed_up,
        r.skipped,
        r.failed,
        if r.cancelled { ", stopped" } else { "" },
        if r.disk_full {
            ", the backup disk is full"
        } else {
            ""
        },
        if r.must_reconnect {
            ", the connection must be reset"
        } else {
            ""
        }
    );
    Ok(report)
}

/// Refuse a sync whose new files, new copies of changed files and JPG copies of HEIC photos
/// won't fit on the backup disk ([`crate::space`]). Moves into the archive, beside the new copy or into the bin stay on the
/// same disk and need nothing extra. If the disk can't be asked, the sync goes ahead: the
/// disk-full stop is the backstop.
fn check_space(plan: &ScanPlan, items: &[SyncItem], root_canon: &Path) -> Result<(), SyncError> {
    let needed = space::bytes_to_write(plan, items).saturating_add(space::jpg_bytes(plan));
    let disk = platform::disk_space(root_canon);
    match &disk {
        Some(d) => info!(
            "This sync writes {}; the backup disk has {} free of {}",
            human(needed),
            human(d.free),
            human(d.total)
        ),
        None => warn!(
            "Could not find how much room the backup disk has; this sync writes {}",
            human(needed)
        ),
    }
    space::check(needed, disk).map_err(|short| {
        error!("Sync refused: {short}");
        SyncError::NotEnoughSpace(short)
    })
}

/// Whether a write failed because the disk (or the user's quota on it) is full:
/// `ERROR_DISK_FULL` or `ENOSPC`.
fn is_disk_full(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded
    ) || is_disk_full_code(e.raw_os_error())
}

/// The raw codes, by system: 39 is `ENOTEMPTY` on Linux and 28 is not a disk error on Windows.
#[cfg(windows)]
fn is_disk_full_code(code: Option<i32>) -> bool {
    // ERROR_DISK_FULL, ERROR_HANDLE_DISK_FULL
    matches!(code, Some(112 | 39))
}

#[cfg(not(windows))]
fn is_disk_full_code(code: Option<i32>) -> bool {
    matches!(code, Some(28)) // ENOSPC
}

#[cfg(test)]
pub(crate) fn is_disk_full_for_tests(e: &io::Error) -> bool {
    is_disk_full(e)
}

/// Write one chunk of a copy. Tests can make the disk "fill up" after so many bytes
/// (`fail_writes_after`), by writing what fits and then failing as a full disk does.
fn write_chunk(out: &mut File, chunk: &[u8], written_before: u64) -> io::Result<()> {
    #[cfg(test)]
    if let Some(limit) = FAIL_WRITES_AFTER.with(std::cell::Cell::get)
        && written_before + chunk.len() as u64 > limit
    {
        let room = limit.saturating_sub(written_before) as usize;
        out.write_all(&chunk[..room])?;
        return Err(io::Error::from_raw_os_error(DISK_FULL_CODE));
    }
    let _ = written_before;
    out.write_all(chunk)
}

#[cfg(test)]
const DISK_FULL_CODE: i32 = if cfg!(windows) { 112 } else { 28 };

#[cfg(test)]
thread_local! {
    static FAIL_WRITES_AFTER: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}

/// Test seam: on this thread, a copy's writes fail as a full disk after `limit` bytes of a file,
/// until the guard is dropped.
#[cfg(test)]
pub(crate) fn fail_writes_after(limit: u64) -> impl Drop {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            FAIL_WRITES_AFTER.with(|c| c.set(None));
        }
    }
    FAIL_WRITES_AFTER.with(|c| c.set(Some(limit)));
    Reset
}

/// The reasons archiving asks first that hold everything back, as text.
fn blocking_reasons(plan: &ScanPlan) -> Vec<String> {
    plan.cautions
        .iter()
        .filter(|c| c.blocks_all())
        .map(|c| c.describe())
        .collect()
}

/// A size for log lines, 1024-based like the window's, e.g. "3.4 MB".
fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}

/// Bytes per second for log lines, e.g. "2.8 MB/s".
fn rate(bytes: u64, secs: f64) -> String {
    if secs > 0.0 {
        format!("{}/s", human((bytes as f64 / secs) as u64))
    } else {
        "too quick to time".into()
    }
}

/// The backup folder, resolved, provided it still resolves to where it did at scan time.
pub(crate) fn checked_root(plan: &ScanPlan) -> Result<PathBuf, SyncError> {
    let root_canon = fs::canonicalize(&plan.root).map_err(|e| {
        error!(
            "Could not resolve the backup folder {}: {e}",
            plan.root.display()
        );
        SyncError::Io(format!(
            "could not resolve the backup folder {}: {e}",
            plan.root.display()
        ))
    })?;
    if let Some(scanned) = &plan.root_canonical
        && *scanned != root_canon
    {
        warn!(
            "Refused: the backup folder now resolves to {} instead of {}, as it did when scanned",
            root_canon.display(),
            scanned.display()
        );
        return Err(SyncError::RootChanged {
            scanned: scanned.clone(),
            now: root_canon,
        });
    }
    Ok(root_canon)
}

/// Refuse if the folder belonged to another device at scan time, or does now (another device
/// may have claimed it since), or if its owner marker can't be read.
pub(crate) fn check_owner(plan: &ScanPlan, root_canon: &Path) -> Result<(), SyncError> {
    let why = match plan.folder_blocked() {
        Some(why) => why,
        None => match owner::read(root_canon) {
            OwnerState::Owned(o) => match owner::refusal(&o, &plan.device_key, plan.view) {
                Some(why) => why,
                None => return Ok(()),
            },
            OwnerState::Unreadable(e) => {
                format!("{} in the backup folder can't be read ({e})", owner::MARKER)
            }
            _ => return Ok(()),
        },
    };
    warn!("Refused: {why}; nothing is copied into it or archived in it");
    Err(SyncError::WrongFolder(why))
}

/// What happened to one file of [`ScanPlan::removed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveEvent {
    /// Moved to `to`, a path in the backup folder under the archive folder.
    Moved { removed: usize, to: SafeRelPath },
    /// Sent to the Recycle Bin or Trash.
    Binned { removed: usize },
    /// Left where it was.
    Failed { removed: usize, error: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArchiveReport {
    /// What was done with the files that left the device: the device's choice.
    pub mode: DeletedFiles,
    /// The dated folder, if anything was moved into it ([`DeletedFiles::Archive`] only).
    pub folder: Option<SafeRelPath>,
    /// Moved out of place: into the archive, or into the bin.
    pub moved: usize,
    pub failed: usize,
    /// Of the failed ones, files held back because their device folder lists as empty.
    pub held: usize,
    /// JPG copies moved along with their HEICs, to the same place. Not counted in `moved`.
    pub jpgs_moved: usize,
}

/// A name for this sync's archive folder: the local date and time, e.g. `2026-11-16 15-30-05`.
pub fn archive_stamp() -> String {
    platform::local_stamp()
}

/// Make sure the backup folder's marker exists and lists the archive name this sync uses, so
/// that a scan never takes the archive for backup files. The first sync into a folder writes the
/// marker; a later one with a new name adds it to the list and keeps the old names.
///
/// A first marker is written without replacing anything. If the folder already held files, the
/// marker records that they have not been confirmed yet (unless `confirmed`), so a sync that
/// leaves them alone doesn't make the next scan forget to ask.
fn record_archive_name(plan: &ScanPlan, root_canon: &Path, confirmed: bool) -> Result<(), String> {
    owner::validate_archive_name(&plan.archive_name)?;
    let write_failed =
        |e: io::Error| format!("could not record the backup folder's owner and archive name ({e})");
    match owner::read(root_canon) {
        OwnerState::Unclaimed => {
            let mut marker = FolderOwner::new(
                &plan.device_key,
                &plan.device_label,
                plan.signals.clone(),
                plan.view,
                &crate::platform::utc_stamp(),
            )
            .with_archive_name(&plan.archive_name);
            marker.foreign_unconfirmed = plan.backup_files > 0 && !confirmed;
            owner::write_new(root_canon, &marker).map_err(write_failed)
        }
        OwnerState::Owned(mut o) => {
            if o.has_archive_name(&plan.archive_name) {
                return Ok(());
            }
            info!(
                "Adding the archive name {:?} to the marker (it keeps {:?})",
                plan.archive_name, o.archive_names
            );
            o.use_archive_name(&plan.archive_name);
            owner::write(root_canon, &o).map_err(write_failed)
        }
        OwnerState::Unreadable(e) => Err(format!(
            "{} in the backup folder can't be read ({e})",
            owner::MARKER
        )),
    }
}

/// Deal with the files that are no longer on the device ([`ScanPlan::removed`]) the way the
/// plan's [`DeletedFiles`] says:
/// - **Archive:** move them into the archive folder, in `<archive name>/<stamp>/`. Nothing is
///   deleted or overwritten, and the dated folder is only created when the first file moves. The
///   layout inside it is the plan's ([`ArchiveLayout`]): the device's folders kept, or every file
///   directly in it, with clashing names numbered.
/// - **Leave in place:** do nothing at all; the files stay in the backup folder.
/// - **Recycle bin:** send each to the Recycle Bin or Trash ([`platform::send_to_bin`]), which
///   never deletes: a file the bin can't take stays where it is.
///
/// Refuses if the backup folder belongs to another device. When the plan lists reasons that
/// archiving needs an explicit OK ([`ScanPlan::cautions`]) and `confirmed` is false, it
/// archives nothing and says why; files held back (under a folder the device lists as
/// empty) are only archived when `confirmed`. Each file is checked first: it must still be
/// the plain file of the size seen at scan time, in a folder that still resolves inside the
/// backup folder.
pub fn archive_removed(
    plan: &ScanPlan,
    stamp: &str,
    confirmed: bool,
    on_event: &mut dyn FnMut(ArchiveEvent),
) -> Result<ArchiveReport, SyncError> {
    let mode = plan.deleted_files;
    let mut report = ArchiveReport {
        mode,
        ..ArchiveReport::default()
    };
    if plan.removed.is_empty() {
        return Ok(report);
    }
    if mode == DeletedFiles::LeaveInPlace {
        info!(
            "{} no longer on the device stay in the backup folder",
            counted(plan.removed.len(), "file", "files")
        );
        return Ok(report);
    }
    let root_canon = checked_root(plan)?;
    check_owner(plan, &root_canon)?;
    if !confirmed {
        let why = blocking_reasons(plan);
        if !why.is_empty() {
            warn!("Moving files refused until confirmed: {}", why.join("; "));
            return Err(SyncError::NeedsConfirmation(why));
        }
    }
    record_archive_name(plan, &root_canon, confirmed).map_err(|e| {
        error!("Archiving refused: {e}");
        SyncError::Io(format!("{e}; nothing was archived"))
    })?;
    if confirmed {
        confirm_foreign_files(&root_canon);
    }
    let base = archive_base(&plan.archive_name, stamp).map_err(|e| {
        error!("Bad archive folder name {stamp:?}: {e}");
        SyncError::Io(format!("bad archive folder name {stamp:?}: {e}"))
    })?;
    let to_where = match mode {
        DeletedFiles::RecycleBin => platform::BIN_NAME.to_string(),
        _ => base.to_string(),
    };
    info!(
        "Moving {} no longer on the device into {to_where}",
        counted(plan.removed.len(), "file", "files")
    );
    // JPG copies the app made follow their HEICs. The list is saved as they go, now and then.
    let mut made = MadeList::load(&root_canon);
    let mut unsaved = 0usize;
    for (i, r) in plan.removed.iter().enumerate() {
        if r.held && !confirmed {
            warn!(
                "Held back: {} sits under a folder the device lists as empty",
                r.path
            );
            report.failed += 1;
            report.held += 1;
            on_event(ArchiveEvent::Failed {
                removed: i,
                error: "held back: it is under a folder the device lists as empty; archive it after confirming".into(),
            });
            continue;
        }
        // The HEIC must still be what the scan saw before its JPG moves, and the JPG goes first:
        // a crash between the two leaves a HEIC without its JPG, which is not a loss. If the
        // HEIC then can't be moved, the JPG is put back (below), so a pair is never split.
        let mut jpg_went: Option<(OldCopy, Option<MadeEntry>)> = None;
        if let Some(jpg) = &r.jpg {
            let went = check_backup_file(&root_canon, &r.path, r.size).and_then(|_| {
                move_made_jpg(&root_canon, plan, stamp, &Ok(()), jpg).map_err(|e| {
                    format!("its JPG copy could not be moved ({e}); both left in place")
                })
            });
            match went {
                Ok(went) => {
                    info!(
                        "The JPG copy {} of {} was {}",
                        jpg.path,
                        r.path,
                        went.describe()
                    );
                    let entry = made.remove_jpg(jpg.path.as_str());
                    report.jpgs_moved += 1;
                    unsaved += 1;
                    if unsaved >= 20 {
                        unsaved = 0;
                        save_made(&made, &root_canon);
                    }
                    if report.folder.is_none() && matches!(went, OldCopy::Archived(_)) {
                        report.folder = Some(base.clone());
                    }
                    jpg_went = Some((went, entry));
                }
                Err(error) => {
                    error!("Could not move {} with its JPG copy: {error}", r.path);
                    report.failed += 1;
                    on_event(ArchiveEvent::Failed { removed: i, error });
                    continue;
                }
            }
        }
        if mode == DeletedFiles::RecycleBin {
            match heic_move_fault().map_or_else(|| bin_one(&root_canon, &r.path, r.size), Err) {
                Ok(()) => {
                    info!("Sent {} to the {}", r.path, platform::BIN_NAME);
                    report.moved += 1;
                    on_event(ArchiveEvent::Binned { removed: i });
                }
                Err(error) => {
                    error!("Could not send {} to the bin: {error}", r.path);
                    let (error, back) =
                        put_jpg_back(&root_canon, &mut made, r, jpg_went.take(), error);
                    if back {
                        report.jpgs_moved = report.jpgs_moved.saturating_sub(1);
                        save_made(&made, &root_canon);
                    }
                    report.failed += 1;
                    on_event(ArchiveEvent::Failed { removed: i, error });
                }
            }
            continue;
        }
        let moved = heic_move_fault().map_or_else(
            || {
                archive_one(
                    &root_canon,
                    &plan.archive_name,
                    plan.archive_layout,
                    stamp,
                    &r.path,
                    r.size,
                )
            },
            Err,
        );
        match moved {
            Ok(to) => {
                info!("Archived {} to {to}", r.path);
                report.moved += 1;
                report.folder.get_or_insert_with(|| base.clone());
                on_event(ArchiveEvent::Moved { removed: i, to });
            }
            Err(error) => {
                error!("Could not archive {}: {error}", r.path);
                let (error, back) = put_jpg_back(&root_canon, &mut made, r, jpg_went.take(), error);
                if back {
                    report.jpgs_moved = report.jpgs_moved.saturating_sub(1);
                    save_made(&made, &root_canon);
                }
                report.failed += 1;
                on_event(ArchiveEvent::Failed { removed: i, error });
            }
        }
    }
    if unsaved > 0 {
        save_made(&made, &root_canon);
    }
    info!(
        "Moving finished: {} moved into {to_where}, {} left in place",
        report.moved, report.failed
    );
    Ok(report)
}

/// The HEIC `r` could not be moved after its JPG copy had been (`went`, with the entry the made
/// list had for it). Puts the JPG back where it was, without replacing anything, and gives its
/// entry back to the list, so the pair stays whole and both are looked at again next time. Returns
/// the message for the HEIC's failure, and whether the JPG is back. If it can't be put back (it
/// went to the bin, or the move fails), the message says so: the HEIC stays, its JPG copy is
/// where it went.
fn put_jpg_back(
    root_canon: &Path,
    made: &mut MadeList,
    r: &Removed,
    went: Option<(OldCopy, Option<MadeEntry>)>,
    error: String,
) -> (String, bool) {
    let (Some(jpg), Some((went, entry))) = (&r.jpg, went) else {
        return (error, false);
    };
    let back = match &went {
        OldCopy::Archived(to) | OldCopy::RenamedAside(to) => {
            check_backup_file(root_canon, to, jpg.size).and_then(|from| {
                platform::rename_no_replace(&from, &jpg.path.under(root_canon))
                    .map_err(|e| format!("could not move it back: {e}"))
            })
        }
        OldCopy::Binned | OldCopy::Missing => {
            Err(format!("it is in the {} now", platform::BIN_NAME))
        }
    };
    match back {
        Ok(()) => {
            info!(
                "The JPG copy {} was put back: {} could not be moved",
                jpg.path, r.path
            );
            if let Some(entry) = entry {
                made.insert(entry);
            }
            (
                format!("{error}; its JPG copy was put back, both left in place"),
                true,
            )
        }
        Err(why) => {
            error!(
                "The JPG copy {} could not be put back after {} failed to move: {why}",
                jpg.path, r.path
            );
            (
                format!(
                    "{error}; its JPG copy had been moved already ({}) and could not be put back ({why})",
                    went.describe()
                ),
                false,
            )
        }
    }
}

// A fault for a test to put into the HEIC's own move, after its JPG has moved. Never set outside
// tests, where `heic_move_fault` is always `None`.
#[cfg(test)]
thread_local! {
    pub(crate) static HEIC_MOVE_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn heic_move_fault() -> Option<String> {
    #[cfg(test)]
    if HEIC_MOVE_FAULT.with(std::cell::Cell::get) {
        return Some("could not move it: a fault put in by the test".into());
    }
    None
}

/// Saves the made list; a list that can't be saved only means some entries stay behind for
/// files that have moved, which the next JPG step notices (the file is gone).
fn save_made(made: &MadeList, root_canon: &Path) {
    if let Err(e) = made.save(root_canon) {
        warn!("Could not save the list of JPG copies: {e}");
    }
}

/// Forgets these JPGs (paths relative to the backup folder) in the made list, if they are in
/// it. Not fatal if the list can't be saved: an entry for a file that is gone is noticed later.
fn forget_made<'a>(root_canon: &Path, paths: impl IntoIterator<Item = &'a str>) {
    let mut paths = paths.into_iter().peekable();
    if paths.peek().is_none() {
        return;
    }
    let mut made = MadeList::load(root_canon);
    let mut any = false;
    for p in paths {
        any |= made.remove_jpg(p).is_some();
    }
    if any {
        save_made(&made, root_canon);
    }
}

/// Moves a JPG the app made out of its place the way the device's choice for deleted files and
/// changed files' old copies says ([`DeletedFiles`]): into the archive (the same dated folder
/// and sub-path as its HEIC), renamed aside, or into the bin. Never replaces anything. The
/// JPG must still be the plain file of the size the scan saw.
fn move_made_jpg(
    root_canon: &Path,
    plan: &ScanPlan,
    stamp: &str,
    archive_ready: &Result<(), String>,
    jpg: &MadeJpg,
) -> Result<OldCopy, String> {
    match plan.deleted_files {
        DeletedFiles::Archive => {
            if let Err(e) = archive_ready {
                return Err(format!("the archive can't be used: {e}"));
            }
            archive_one(
                root_canon,
                &plan.archive_name,
                plan.archive_layout,
                stamp,
                &jpg.path,
                jpg.size,
            )
            .map(OldCopy::Archived)
        }
        DeletedFiles::LeaveInPlace => {
            let src = check_backup_file(root_canon, &jpg.path, jpg.size)?;
            let (rel, full) = first_free_aside_path(&src, &jpg.path, stamp)?;
            platform::rename_no_replace(&src, &full)
                .map_err(|e| format!("could not move it aside: {e}"))?;
            Ok(OldCopy::RenamedAside(rel))
        }
        DeletedFiles::RecycleBin => {
            bin_one(root_canon, &jpg.path, jpg.size)?;
            Ok(OldCopy::Binned)
        }
    }
}

/// An archive was confirmed: the files that were in the folder before it was claimed no longer
/// ask first. Not fatal if the marker can't be rewritten: they just ask again.
fn confirm_foreign_files(root_canon: &Path) {
    if let OwnerState::Owned(mut o) = owner::read(root_canon)
        && o.foreign_unconfirmed
    {
        o.foreign_unconfirmed = false;
        if let Err(e) = owner::write(root_canon, &o) {
            warn!("Could not record that the files already in the folder were confirmed: {e}");
        }
    }
}

/// `<archive name>/<stamp>`, validated.
fn archive_base(archive_name: &str, stamp: &str) -> Result<SafeRelPath, String> {
    owner::validate_archive_name(archive_name)?;
    SafeRelPath::from_components(&[archive_name, stamp]).map_err(|e| e.to_string())
}

/// Where file `path` goes in the archive for a sync stamped `stamp`: the `n`th candidate name
/// (the first is the plain one; the later ones are numbered, `IMG (2).HEIC`).
fn archive_candidate(
    archive_name: &str,
    layout: ArchiveLayout,
    stamp: &str,
    path: &SafeRelPath,
    n: u32,
) -> Result<SafeRelPath, String> {
    let name = path.file_name();
    let name = if n == 1 {
        name.to_string()
    } else {
        numbered(name, n)
    };
    let mut parts: Vec<&str> = vec![archive_name, stamp];
    if layout == ArchiveLayout::KeepFolders {
        let all: Vec<&str> = path.components().collect();
        parts.extend(&all[..all.len() - 1]);
    }
    parts.push(&name);
    SafeRelPath::from_components(&parts).map_err(|e| e.to_string())
}

/// The first archive name for `path` that isn't taken, with its folder made. Nothing is moved.
fn first_free_archive_path(
    root_canon: &Path,
    archive_name: &str,
    layout: ArchiveLayout,
    stamp: &str,
    path: &SafeRelPath,
) -> Result<(SafeRelPath, PathBuf), String> {
    for n in 1..=MAX_NUMBERED {
        let to = archive_candidate(archive_name, layout, stamp, path, n)?;
        let folder = prepare_parent(root_canon, &to).map_err(|e| e.to_string())?;
        let full = folder.join(to.file_name());
        match fs::symlink_metadata(&full) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((to, full)),
            Err(e) => return Err(format!("could not check {to}: {e}")),
            Ok(_) => continue,
        }
    }
    Err("too many files with this name in the archive folder; left in place".into())
}

/// Check that a backup file is still the plain file of `size` it was at scan time, in a folder
/// that resolves inside the backup folder.
fn check_backup_file(root_canon: &Path, path: &SafeRelPath, size: u64) -> Result<PathBuf, String> {
    let src = path.under(root_canon);
    // Its folder must still resolve inside the backup folder: no junction swapped in since.
    let folder = src.parent().ok_or("it has no folder")?;
    let folder_canon =
        fs::canonicalize(folder).map_err(|e| format!("could not check its folder: {e}"))?;
    if !folder_canon.starts_with(root_canon) {
        return Err("its folder now leads outside the backup folder; left in place".into());
    }
    match fs::symlink_metadata(&src) {
        Ok(m) if m.is_file() && !is_link(&m) && m.len() == size => Ok(src),
        Ok(_) => Err("it changed since the scan; left in place".into()),
        Err(e) => Err(format!("could not check it: {e}")),
    }
}

fn archive_one(
    root_canon: &Path,
    archive_name: &str,
    layout: ArchiveLayout,
    stamp: &str,
    path: &SafeRelPath,
    size: u64,
) -> Result<SafeRelPath, String> {
    let src = check_backup_file(root_canon, path, size)?;
    for _ in 0..MAX_NUMBERED {
        let (to, full) = first_free_archive_path(root_canon, archive_name, layout, stamp, path)?;
        match platform::rename_no_replace(&src, &full) {
            Ok(()) => return Ok(to),
            // Taken between the check and the move: pick the next name.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("could not move it: {e}")),
        }
    }
    Err("too many files with this name in the archive folder; left in place".into())
}

/// Send a backup file that has left the device to the Recycle Bin or Trash, after the same
/// checks as archiving it. Never deletes: if the bin can't take it, it stays.
fn bin_one(root_canon: &Path, path: &SafeRelPath, size: u64) -> Result<(), String> {
    let src = check_backup_file(root_canon, path, size)?;
    platform::send_to_bin(&src).map_err(|e| {
        if fs::symlink_metadata(&src).is_err() {
            // The bin said no, yet the file is no longer there. Do not claim it was left.
            error!(
                "{}: the {} reported a failure ({e}) but the file is gone from the backup folder; it may not be in the {}",
                src.display(),
                platform::BIN_NAME,
                platform::BIN_NAME
            );
            return format!(
                "the {} reported a failure ({e}), but the file is gone from the backup folder and may not be in the {}",
                platform::BIN_NAME,
                platform::BIN_NAME
            );
        }
        bin_refused(&e)
    })
}

/// The message when a file could not be sent to the bin and so was left where it is.
fn bin_refused(why: &str) -> String {
    format!(
        "couldn't send it to the {}, so it was left in place ({why})",
        platform::BIN_NAME
    )
}

/// `IMG_0001.HEIC`, 2 -> `IMG_0001 (2).HEIC`.
fn numbered(name: &str, n: u32) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 => crate::paths::fit_name(&name[..i], &format!(" ({n}){}", &name[i..])),
        _ => crate::paths::fit_name(name, &format!(" ({n})")),
    }
}

/// What [`rename_archive`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveRenamed {
    /// The archive folder was moved to the new name. `false`: there was no archive folder under
    /// the old name (or no marker yet), so only the name changes.
    pub moved: bool,
}

/// Rename the existing archive folder to `new_name` in the backup folder, as one move, after
/// the usual checks: the folder belongs to this device, the new name is a safe single name that
/// the device doesn't use at the top of the backup and that nothing in the backup folder has,
/// and the old folder is a plain folder in the backup folder. The marker keeps the old name in
/// `archive_names` and lists the new one, so neither is ever taken for backup content. The
/// caller then stores the new name in the device's settings and scans again.
pub fn rename_archive(plan: &ScanPlan, new_name: &str) -> Result<ArchiveRenamed, SyncError> {
    let old = plan.archive_name.as_str();
    owner::validate_archive_name(new_name).map_err(SyncError::Io)?;
    if new_name.eq_ignore_ascii_case(old) {
        return Err(SyncError::Io(format!(
            "the archive folder is already named {old:?}"
        )));
    }
    if plan
        .top_names
        .iter()
        .any(|t| t.eq_ignore_ascii_case(new_name))
    {
        return Err(SyncError::Io(format!(
            "the device already has a folder named {new_name:?} at the top, so it can't be the archive folder's name"
        )));
    }
    let root_canon = checked_root(plan)?;
    check_owner(plan, &root_canon)?;
    // Whether or not there is an archive to move, the new name must not be one of the backup's
    // own top-level names (in any letter case), or its content would be taken for archive.
    let names = fs::read_dir(&root_canon)
        .map_err(|e| SyncError::Io(format!("could not list the backup folder: {e}")))?;
    for entry in names {
        let entry =
            entry.map_err(|e| SyncError::Io(format!("could not list the backup folder: {e}")))?;
        if entry
            .file_name()
            .to_string_lossy()
            .eq_ignore_ascii_case(new_name)
        {
            return Err(SyncError::Io(format!(
                "{new_name:?} already exists in the backup folder; not renaming over it"
            )));
        }
    }
    let OwnerState::Owned(mut marker) = owner::read(&root_canon) else {
        // No marker, so no archive of ours here: the name is recorded by the next sync.
        return Ok(ArchiveRenamed { moved: false });
    };
    let from = root_canon.join(old);
    let to = root_canon.join(new_name);
    let exists = match fs::symlink_metadata(&from) {
        Ok(m) if is_link(&m) || !m.is_dir() => {
            return Err(SyncError::Io(format!(
                "{old:?} in the backup folder is not a plain folder; not renaming it"
            )));
        }
        Ok(_) => true,
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => return Err(SyncError::Io(format!("could not check {old:?}: {e}"))),
    };
    // The marker lists the new name before the folder takes it, and keeps the old one.
    marker.use_archive_name(new_name);
    owner::write(&root_canon, &marker)
        .map_err(|e| SyncError::Io(format!("could not record the new archive name: {e}")))?;
    if !exists {
        return Ok(ArchiveRenamed { moved: false });
    }
    platform::rename_no_replace(&from, &to)
        .map_err(|e| SyncError::Io(format!("could not rename the archive folder: {e}")))?;
    info!("Renamed the archive folder {old:?} to {new_name:?}");
    Ok(ArchiveRenamed { moved: true })
}

struct FileCtx<'a> {
    root: &'a Path,
    root_canon: &'a Path,
    device: &'a str,
    index: usize,
    entry: &'a PlanEntry,
    mode: CopyMode,
    plan: &'a ScanPlan,
    stamp: &'a str,
    /// Whether the marker lists the archive name (a changed file's old copy can then move into
    /// the archive).
    archive_ready: &'a Result<(), String>,
    /// Why moving files needs an OK that hasn't been given; empty if nothing is held back. A
    /// changed file's old copy is moved too (archived, renamed aside or sent to the bin), so it
    /// waits for the same OK.
    needs_ok: &'a [String],
}

enum Outcome {
    /// Copied to the target; the last is where a changed file's old copy went.
    Copied(SafeRelPath, u64, Option<OldCopy>),
    Already(SafeRelPath),
    Skipped,
    Cancelled,
    Failed(String),
    /// The backup disk is full: this file's unfinished copy is gone, and the sync stops.
    DiskFull(String),
}

/// What copying one file does to the backup.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Nothing is at the target: write the file there.
    Fresh,
    /// A different file is at the target: the old copy moves out of the way first (into the
    /// archive, beside it, or into the bin).
    Changed,
    /// The user asked to copy a backed-up file again: handled like a changed file, so the old
    /// copy moves out of the way and nothing is overwritten.
    Replace,
}

/// `unfinished` is set if a transfer from the device was started and didn't end cleanly.
fn copy_one(
    source: &mut dyn DeviceSource,
    ctx: &FileCtx<'_>,
    state: &StateDir,
    ctl: &SyncControl,
    on_event: &mut dyn FnMut(SyncEvent),
    unfinished: &mut bool,
) -> Outcome {
    let entry = ctx.entry;
    // Without a size there is no way to tell a whole copy from a short or wrong one.
    let Some(size) = entry.size else {
        return Outcome::Failed(
            "the device didn't report this file's size, so a copy couldn't be checked; not copied"
                .into(),
        );
    };
    let kind = match (ctx.mode, &entry.status) {
        (CopyMode::Replace, _) => Kind::Replace,
        (_, Status::Error(why)) => {
            return Outcome::Failed(format!("it can't be copied: {why}"));
        }
        (_, Status::Mismatch) => Kind::Changed,
        (_, Status::Skipped) if entry.existing_size.is_some() => Kind::Changed,
        _ => Kind::Fresh,
    };
    if kind == Kind::Changed && !ctx.needs_ok.is_empty() {
        return Outcome::Failed(format!(
            "needs confirmation: replacing it would move its old copy out of place, and that asks first ({}); nothing was moved",
            ctx.needs_ok.join("; ")
        ));
    }
    // A file that shrank to nothing never replaces a copy that has content.
    if kind == Kind::Changed && size == 0 && entry.existing_size.unwrap_or(0) > 0 {
        return Outcome::Failed(
            "the file is 0 bytes on the device but its backup copy has content: the backup copy is kept; check the file on the device"
                .into(),
        );
    }
    // Is this object still the file we planned to copy?
    let before = match source.object_info(&entry.object_id) {
        Ok(info) => {
            if let Some(why) = changed_since_scan(entry, &info) {
                return Outcome::Failed(format!(
                    "the file on the device changed since the scan ({why}); scan again"
                ));
            }
            info
        }
        Err(e) => {
            // A connection that has been spoiled answers nothing: the sync stops here too.
            if source.is_spoiled() {
                *unfinished = true;
            }
            return Outcome::Failed(format!("could not re-check the file on the device: {e}"));
        }
    };

    let target = entry.target.clone();
    let dir = match prepare_parent(ctx.root_canon, &target) {
        Ok(d) => d,
        Err(e) => return Outcome::Failed(e.to_string()),
    };
    let final_path = dir.join(target.file_name());
    match kind {
        Kind::Fresh => match fs::symlink_metadata(&final_path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            // The same file is already there: nothing to copy.
            Ok(m) if m.is_file() && !is_link(&m) && m.len() == size => {
                return Outcome::Already(target);
            }
            Ok(_) => {
                return Outcome::Failed(format!(
                    "a different file is at {target} now, so it was not overwritten; scan again"
                ));
            }
            Err(e) => return Outcome::Failed(format!("could not check {target}: {e}")),
        },
        // A changed file's old copy must still be exactly what the scan saw. A file copied again
        // only ever replaces a backup that is still exactly what the scan called backed up.
        Kind::Changed | Kind::Replace => {
            if kind == Kind::Replace && entry.status != Status::Verified {
                return Outcome::Failed(
                    "only a file that was backed up when you scanned can be copied over its backup"
                        .into(),
                );
            }
            let want = if kind == Kind::Replace {
                Some(size)
            } else {
                entry.existing_size
            };
            match fs::symlink_metadata(&final_path) {
                Ok(meta) if is_link(&meta) || !meta.is_file() => {
                    return Outcome::Failed(
                        "the existing backup is not a plain file; not touching it".into(),
                    );
                }
                Ok(meta) if Some(meta.len()) == want => {}
                Ok(_) => {
                    return Outcome::Failed(
                        "the backup file changed since the scan; not touching it. Scan again"
                            .into(),
                    );
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    return Outcome::Failed(
                        "the backup file is no longer there; scan again".into(),
                    );
                }
                Err(e) => {
                    return Outcome::Failed(format!("could not check the backup file: {e}"));
                }
            }
            if ctx.plan.deleted_files == DeletedFiles::Archive
                && let Err(e) = ctx.archive_ready
            {
                return Outcome::Failed(format!(
                    "the old copy can't be moved into the archive: {e}; not replaced"
                ));
            }
        }
    }
    let temp = dir.join(crate::paths::fit_name(target.file_name(), TEMP_SUFFIX));
    if let Err(e) = clear_stale_temp(&temp) {
        return Outcome::Failed(e);
    }
    let record = InProgress {
        device: ctx.device.to_string(),
        root: ctx.root.to_path_buf(),
        source: entry.source.as_str().to_string(),
        temp: temp.clone(),
        replace: None,
    };
    if let Err(e) = state.begin_copy(&record) {
        return Outcome::Failed(format!("could not record the copy in progress: {e}"));
    }

    // Whether the temp is ours to remove: only once this call created it. A file that was
    // already there when `create_new` was refused is someone else's, whole or not.
    let mut created = false;
    // Set when an unfinished temp could not be removed: its crash record is then kept, so the
    // next start removes it (otherwise a later sync would keep it forever as a partial copy).
    let mut keep_record = false;
    let outcome = (|| {
        let wrote = write_temp(
            source,
            ctx,
            &before,
            &temp,
            ctl,
            on_event,
            unfinished,
            &mut created,
        );
        let bytes = match wrote {
            Ok(b) => b,
            Err(stop) => {
                if created {
                    discard_temp(&temp, state, &record, &mut keep_record);
                }
                return match stop {
                    Stop::Skipped => Outcome::Skipped,
                    Stop::Cancelled => Outcome::Cancelled,
                    Stop::Failed(m) => Outcome::Failed(m),
                    Stop::DiskFull(m) => Outcome::DiskFull(m),
                };
            }
        };
        match kind {
            Kind::Fresh => match platform::rename_no_replace(&temp, &final_path) {
                Ok(()) => Outcome::Copied(target.clone(), bytes, None),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    // Something turned up at the target during the copy: never overwrite it.
                    discard_temp(&temp, state, &record, &mut keep_record);
                    match fs::symlink_metadata(&final_path) {
                        Ok(m) if m.is_file() && !is_link(&m) && m.len() == bytes => {
                            Outcome::Already(target.clone())
                        }
                        _ => Outcome::Failed(format!(
                            "a different file turned up at {target} during the copy, so it was not overwritten; scan again"
                        )),
                    }
                }
                Err(e) => Outcome::Failed(format!(
                    "copied, but could not move it into place ({e}); the copy is kept as {} (a later sync moves it aside, never deletes it)",
                    temp.display()
                )),
            },
            Kind::Changed | Kind::Replace => replace_changed(
                ctx,
                state,
                &record,
                &temp,
                &final_path,
                &target,
                bytes,
                &mut keep_record,
            ),
        }
    })();
    if keep_record {
        warn!(
            "The unfinished copy {} could not be removed; the next start will try again",
            temp.display()
        );
    } else if let Err(e) = state.end_copy() {
        warn!("Could not clear the record of the copy in progress: {e}");
    }
    outcome
}

/// The moves of a changed file, with the new copy whole in `temp` and the old one at
/// `final_path`: the old copy out of the way, then the new one into its place, neither move
/// replacing anything. Where the old copy goes is the device's [`DeletedFiles`] choice: into
/// the archive, renamed beside the new one, or into the Recycle Bin or Trash. The crash record
/// is rewritten first so a crash between the moves can be finished.
#[allow(clippy::too_many_arguments)]
fn replace_changed(
    ctx: &FileCtx<'_>,
    state: &StateDir,
    record: &InProgress,
    temp: &Path,
    final_path: &Path,
    target: &SafeRelPath,
    bytes: u64,
    keep_record: &mut bool,
) -> Outcome {
    let mode = ctx.plan.deleted_files;
    // The JPG the app made from the old HEIC goes first, the way the old copy will, so a crash
    // between the two leaves a HEIC without its JPG (the next sync makes it again) and never a
    // JPG that lost its way. Nothing here has been moved yet if it fails.
    if let Some(jpg) = &ctx.entry.old_jpg {
        match move_made_jpg(ctx.root_canon, ctx.plan, ctx.stamp, ctx.archive_ready, jpg) {
            Ok(went) => {
                info!(
                    "The JPG copy {} of the old {} was {}",
                    jpg.path,
                    target,
                    went.describe()
                );
                forget_made(ctx.root_canon, [jpg.path.as_str()]);
            }
            Err(e) => {
                discard_temp(temp, state, record, keep_record);
                return Outcome::Failed(format!(
                    "could not move the JPG copy of the old photo out of the way ({e}); nothing was replaced"
                ));
            }
        }
    }
    // Where the old copy goes (nothing is moved yet).
    let place = match mode {
        DeletedFiles::Archive => first_free_archive_path(
            ctx.root_canon,
            &ctx.plan.archive_name,
            ctx.plan.archive_layout,
            ctx.stamp,
            target,
        )
        .map(|(rel, full)| (OldCopyGoes::Archive, full, OldCopy::Archived(rel))),
        DeletedFiles::LeaveInPlace => first_free_aside_path(final_path, target, ctx.stamp)
            .map(|(rel, full)| (OldCopyGoes::Aside, full, OldCopy::RenamedAside(rel))),
        DeletedFiles::RecycleBin => Ok((OldCopyGoes::Bin, PathBuf::new(), OldCopy::Binned)),
    };
    let (goes, moved_to, mut old_copy) = match place {
        Ok(p) => p,
        Err(e) => {
            discard_temp(temp, state, record, keep_record);
            return Outcome::Failed(format!("could not prepare where the old copy goes: {e}"));
        }
    };
    let ready = InProgress {
        replace: Some(ReplaceStep {
            original: final_path.to_path_buf(),
            archived: moved_to.clone(),
            goes,
        }),
        ..record.clone()
    };
    if let Err(e) = state.begin_copy(&ready) {
        discard_temp(temp, state, record, keep_record);
        return Outcome::Failed(format!("could not record the replacement in progress: {e}"));
    }
    // Step 2: the old copy out of the way.
    let moved = match goes {
        OldCopyGoes::Bin => platform::send_to_bin(final_path).map_err(|e| bin_refused(&e)),
        _ => platform::rename_no_replace(final_path, &moved_to).map_err(|e| {
            format!(
                "could not move the old copy {} ({e}); it is left in place",
                match goes {
                    OldCopyGoes::Archive => "into the archive",
                    _ => "aside",
                }
            )
        }),
    };
    if let Err(why) = moved {
        // Normally nothing has moved: undo by dropping the unfinished new copy (it is on the
        // device). If the old copy is gone anyway, the bin took it after all, so carry on.
        let gone = matches!(
            fs::symlink_metadata(final_path),
            Err(e) if e.kind() == io::ErrorKind::NotFound
        );
        if !(goes == OldCopyGoes::Bin && gone) {
            discard_temp(temp, state, record, keep_record);
            return Outcome::Failed(why);
        }
        error!(
            "{why}, but the old copy is gone from {}: the {} may not have it",
            final_path.display(),
            platform::BIN_NAME
        );
        old_copy = OldCopy::Missing;
    }
    // Step 3: the new copy into place.
    match platform::rename_no_replace(temp, final_path) {
        Ok(()) => Outcome::Copied(target.clone(), bytes, Some(old_copy)),
        Err(e) => {
            // Put the old copy back if it can be and its place is still free; either way both
            // copies exist (one maybe in the bin).
            let restored = goes != OldCopyGoes::Bin
                && platform::rename_no_replace(&moved_to, final_path).is_ok();
            let old = match (&old_copy, restored) {
                (_, true) => "the old copy was put back".to_string(),
                (OldCopy::Binned, _) => {
                    format!("the old copy is in the {}", platform::BIN_NAME)
                }
                (OldCopy::Missing, _) => format!(
                    "the old copy is gone and may not be in the {}",
                    platform::BIN_NAME
                ),
                (other, _) => format!("the old copy was {}", other.describe()),
            };
            Outcome::Failed(format!(
                "could not move the new copy into place ({e}); {old}; the new copy is kept as {}",
                temp.display()
            ))
        }
    }
}

/// Where a changed file's old copy goes when it is left beside the new one: the first free
/// `IMG_0001 (old 2026-11-18 09-30).jpg`, numbered if taken (`... (old ...) (2).jpg`). The date
/// and time are this sync's, to the minute. Nothing is moved.
fn first_free_aside_path(
    final_path: &Path,
    target: &SafeRelPath,
    stamp: &str,
) -> Result<(SafeRelPath, PathBuf), String> {
    let folder = final_path.parent().ok_or("it has no folder")?;
    let all: Vec<&str> = target.components().collect();
    for n in 1..=MAX_NUMBERED {
        let name = aside_name(target.file_name(), stamp, n);
        let full = folder.join(&name);
        match fs::symlink_metadata(&full) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let mut parts: Vec<&str> = all[..all.len() - 1].to_vec();
                parts.push(&name);
                let rel = SafeRelPath::from_components(&parts).map_err(|e| e.to_string())?;
                return Ok((rel, full));
            }
            Err(e) => return Err(format!("could not check {name}: {e}")),
            Ok(_) => continue,
        }
    }
    Err("too many old copies of this file are kept beside it already; left in place".into())
}

/// `IMG_0001.jpg` -> `IMG_0001 (old 2026-11-18 09-30).jpg` for the sync stamped
/// `2026-11-18 09-30-12`; for `n > 1` the number comes last, before the extension.
fn aside_name(name: &str, stamp: &str, n: u32) -> String {
    // The stamp is `YYYY-MM-DD HH-MM-SS`: keep it to the minute.
    let when = stamp.get(..16).unwrap_or(stamp);
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    if n == 1 {
        crate::paths::fit_name(stem, &format!(" (old {when}){ext}"))
    } else {
        crate::paths::fit_name(stem, &format!(" (old {when}) ({n}){ext}"))
    }
}

fn changed_since_scan(entry: &PlanEntry, now: &ObjectInfo) -> Option<String> {
    if now.name != entry.source.file_name() {
        return Some(format!("its name is now {:?}", now.name));
    }
    if let (Some(a), Some(b)) = (entry.size, now.size)
        && a != b
    {
        return Some(format!("its size is now {b} bytes, was {a}"));
    }
    if let (Some(a), Some(b)) = (&entry.persistent_id, &now.persistent_id)
        && a != b
    {
        return Some("it is a different object".into());
    }
    if let (Some(a), Some(b)) = (entry.modified, now.modified)
        && a != b
    {
        return Some("its modified date is different".into());
    }
    None
}

/// A temporary file already at the name: a crash's is gone by now (recovery removes it), so this
/// is one kept after a failed final move, and it may be a whole copy. It is moved aside under
/// a numbered name, never deleted. Anything else in the way: refuse.
fn clear_stale_temp(temp: &Path) -> Result<(), String> {
    match fs::symlink_metadata(temp) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("could not check {}: {e}", temp.display())),
        Ok(m) if m.is_file() && !is_link(&m) => {
            let stem = temp
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(TEMP_SUFFIX))
                .unwrap_or("copy")
                .to_string();
            for n in 1..=MAX_NUMBERED {
                let aside = temp.with_file_name(crate::paths::fit_name(
                    &stem,
                    &format!(" (kept {n}){TEMP_SUFFIX}"),
                ));
                match platform::rename_no_replace(temp, &aside) {
                    Ok(()) => {
                        warn!(
                            "An unfinished copy was already at {}; it was moved aside to {}",
                            temp.display(),
                            aside.display()
                        );
                        return Ok(());
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(e) => {
                        return Err(format!(
                            "could not move the old unfinished copy {} aside: {e}",
                            temp.display()
                        ));
                    }
                }
            }
            Err(format!(
                "too many unfinished copies of {stem} are already kept next to it"
            ))
        }
        Ok(_) => Err(format!(
            "{} is in the way and is not a plain file",
            temp.display()
        )),
    }
}

/// Removes an unfinished temp copy this call created. If it can't be removed (an antivirus
/// lock, permissions), the crash record is put back to a plain one and `keep_record` is set, so
/// the record stays and the next start removes the temp.
fn discard_temp(temp: &Path, state: &StateDir, record: &InProgress, keep_record: &mut bool) {
    if !remove_unfinished(temp) {
        *keep_record = true;
        // A record of a replacement would finish it at the next start; this is only a leftover.
        if let Err(e) = state.begin_copy(record) {
            warn!("Could not rewrite the record of the copy in progress: {e}");
        }
    }
}

/// Removes `temp`; whether it is gone. A failure is logged.
fn remove_unfinished(temp: &Path) -> bool {
    #[cfg(test)]
    if FAIL_TEMP_REMOVAL.with(std::cell::Cell::get) {
        warn!("Test: not removing {}", temp.display());
        return false;
    }
    match fs::remove_file(temp) {
        Ok(()) => true,
        Err(e) if e.kind() == io::ErrorKind::NotFound => true,
        Err(e) => {
            warn!(
                "Could not remove the unfinished copy {}: {e}",
                temp.display()
            );
            false
        }
    }
}

#[cfg(test)]
type TempHook = Box<dyn Fn(&Path)>;

#[cfg(test)]
thread_local! {
    static FAIL_TEMP_REMOVAL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static BEFORE_TEMP_CREATE: std::cell::RefCell<Option<TempHook>> =
        const { std::cell::RefCell::new(None) };
}

/// Test seam: on this thread, removing an unfinished temp copy fails until the guard is dropped.
#[cfg(test)]
pub(crate) fn fail_temp_removal() -> impl Drop {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            FAIL_TEMP_REMOVAL.with(|c| c.set(false));
        }
    }
    FAIL_TEMP_REMOVAL.with(|c| c.set(true));
    Reset
}

/// Test seam: on this thread, `f` runs with the temp's path just before a copy creates it,
/// until the guard is dropped.
#[cfg(test)]
pub(crate) fn before_temp_create(f: impl Fn(&Path) + 'static) -> impl Drop {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            BEFORE_TEMP_CREATE.with(|c| *c.borrow_mut() = None);
        }
    }
    BEFORE_TEMP_CREATE.with(|c| *c.borrow_mut() = Some(Box::new(f)));
    Reset
}

enum Stop {
    Skipped,
    Cancelled,
    Failed(String),
    /// A write to the backup failed because the disk is full.
    DiskFull(String),
}

/// What a full disk is called to the user.
const DISK_FULL_TEXT: &str =
    "the backup disk is full, so the sync stopped here; the unfinished copy was removed";

#[allow(clippy::too_many_arguments)]
fn write_temp(
    source: &mut dyn DeviceSource,
    ctx: &FileCtx<'_>,
    before: &ObjectInfo,
    temp: &Path,
    ctl: &SyncControl,
    on_event: &mut dyn FnMut(SyncEvent),
    unfinished: &mut bool,
    created: &mut bool,
) -> Result<u64, Stop> {
    #[cfg(test)]
    BEFORE_TEMP_CREATE.with(|c| {
        if let Some(f) = c.borrow().as_ref() {
            f(temp);
        }
    });
    // The local file first, so a problem here never leaves a device transfer half-read.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp)
        .map_err(|e| {
            if is_disk_full(&e) {
                Stop::DiskFull(DISK_FULL_TEXT.into())
            } else {
                Stop::Failed(format!("could not create {}: {e}", temp.display()))
            }
        })?;
    *created = true;
    // From here until the whole file has arrived, at the size the device reported, the
    // connection is in doubt.
    *unfinished = true;
    let mut reader = source
        .open_read(&ctx.entry.object_id)
        .map_err(|e| Stop::Failed(e.to_string()))?;
    let bytes = copy_stream(
        &mut *reader,
        &mut file,
        ctl,
        ctx.index,
        &mut |bytes, rate| {
            on_event(SyncEvent::Progress {
                entry: ctx.index,
                bytes,
                total: ctx.entry.size,
                bytes_per_sec: rate,
            });
        },
    )?;
    drop(reader);
    // The whole file, or nothing.
    match ctx.entry.size {
        Some(expected) if bytes == expected => {}
        Some(expected) => {
            return Err(Stop::Failed(format!(
                "the device sent {bytes} bytes for this {expected}-byte file, so it was not saved"
            )));
        }
        None => {
            return Err(Stop::Failed(
                "the device didn't report this file's size, so the copy can't be checked".into(),
            ));
        }
    }
    *unfinished = false;
    // Where the device reports a modified time, the file must not have changed while it was
    // read: a copy of a file that was being written to is not a copy of any version of it.
    if before.modified.is_some() {
        match source.object_info(&ctx.entry.object_id) {
            Ok(after) if after.size == before.size && after.modified == before.modified => {}
            Ok(_) => {
                return Err(Stop::Failed(
                    "the file on the device changed while copying, so it was not saved; scan again"
                        .into(),
                ));
            }
            Err(e) => {
                return Err(Stop::Failed(format!(
                    "could not check the file on the device after copying it, so it was not saved: {e}"
                )));
            }
        }
    }
    // The copy gets the device's modified time. Not fatal if it can't be set: the data is whole.
    if let Some(when) = before.modified.or(ctx.entry.modified)
        && let Err(e) = platform::set_modified(&file, when)
    {
        warn!(
            "Could not set the modified date of the copy of {}: {e}",
            ctx.entry.source
        );
    }
    // On disk before it gets its real name.
    platform::flush_file(&file).map_err(|e| {
        if is_disk_full(&e) {
            Stop::DiskFull(DISK_FULL_TEXT.into())
        } else {
            Stop::Failed(format!("could not flush the copy to disk: {e}"))
        }
    })?;
    Ok(bytes)
}

/// `entry` is the plan entry being copied: a skip request only applies to that one.
fn copy_stream(
    reader: &mut dyn Read,
    out: &mut File,
    ctl: &SyncControl,
    entry: usize,
    progress: &mut dyn FnMut(u64, f64),
) -> Result<u64, Stop> {
    let mut buf = vec![0u8; BUFFER];
    let start = Instant::now();
    let mut last = start;
    let mut bytes: u64 = 0;
    let rate = |bytes: u64| {
        let s = start.elapsed().as_secs_f64();
        if s > 0.0 { bytes as f64 / s } else { 0.0 }
    };
    loop {
        if ctl.cancel.load(Ordering::Relaxed) {
            return Err(Stop::Cancelled);
        }
        if ctl.skip_requested_for(entry) {
            return Err(Stop::Skipped);
        }
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Stop::Failed(format!("reading from the device failed: {e}"))),
        };
        write_chunk(out, &buf[..n], bytes).map_err(|e| {
            if is_disk_full(&e) {
                Stop::DiskFull(DISK_FULL_TEXT.into())
            } else {
                Stop::Failed(format!("writing to the backup failed: {e}"))
            }
        })?;
        bytes += n as u64;
        if last.elapsed() >= PROGRESS_EVERY {
            last = Instant::now();
            progress(bytes, rate(bytes));
        }
    }
    progress(bytes, rate(bytes));
    Ok(bytes)
}
