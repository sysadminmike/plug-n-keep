//! Comparing what's on the device with what's in the backup folder, producing a [`ScanPlan`].
//!
//! The plan carries its own backup folder and device key, and sync takes only a plan, so a
//! sync can't be pointed at a different folder or device than the one scanned.
//!
//! The device's own settings ([`DeviceSettings`]) are applied here: what to back up, the
//! archive folder's name and layout, how storages are laid out, and which comparison to use.
#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use log::{debug, info, warn};
use walkdir::WalkDir;

use crate::counted;
use crate::device::{
    DeviceError, DeviceFile, DeviceIdentity, DeviceSource, Enumeration, ScanIssue,
};
use crate::heic::made::MadeList;
use crate::jpgs::{self, JpgPlan};
use crate::owner::{self, OwnerState};
use crate::paths::SafeRelPath;
use crate::registry::{ArchiveLayout, DeletedFiles, DeviceSettings, FileTypes, StorageLayout};
use crate::signals::{Signals, View};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanProgress {
    Device { files: usize, folder: String },
    Backup { files: usize },
    Comparing { files: usize },
}

/// What a scan needs to know about the device beyond what it reads from it.
#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    /// The device's settings from the registry.
    pub settings: DeviceSettings,
    /// The device was recognised only by weak signals (name, model, size): archiving then asks
    /// first, because it may be another device.
    pub weak_match: bool,
}

/// Enumerate the device, index the backup folder and classify: the whole scan.
pub fn scan(
    source: &mut dyn DeviceSource,
    root: &Path,
    skip: &HashSet<String>,
    opts: &ScanOptions,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(ScanProgress),
) -> Result<(ScanPlan, Summary), DeviceError> {
    let started = Instant::now();
    owner::validate_archive_name(&opts.settings.archive_name).map_err(DeviceError::Failed)?;
    refuse_dot_dot(root)?;
    refuse_overlap(source.local_root(), root)?;
    info!(
        "Scan started: {} against the backup folder {} ({} on the skip list)",
        source.identity().label(),
        root.display(),
        skip.len()
    );
    let enumeration = source.enumerate(cancel, &mut |files, folder| {
        progress(ScanProgress::Device {
            files,
            folder: folder.to_string(),
        })
    })?;
    info!(
        "The device lists {} ({}, {} not read completely) after {:.1} s",
        counted(enumeration.files.len(), "file", "files"),
        counted(enumeration.issues.len(), "problem", "problems"),
        counted(enumeration.incomplete.len(), "folder", "folders"),
        started.elapsed().as_secs_f64()
    );
    let index = index_backup(
        root,
        &opts.settings.archive_name,
        &opts.settings.ignore_names,
        cancel,
        &mut |files| progress(ScanProgress::Backup { files }),
    )
    .map_err(|Cancelled| DeviceError::Cancelled)?;
    info!(
        "Indexed the backup folder: {}, {}",
        counted(index.len(), "file", "files"),
        counted(index.issues.len(), "problem", "problems")
    );
    progress(ScanProgress::Comparing {
        files: enumeration.files.len(),
    });
    let made = MadeList::load_readonly(root);
    let plan = build_plan(
        source.identity(),
        root,
        enumeration,
        &index,
        &made,
        skip,
        opts,
    );
    let summary = plan.summary(index.len());
    let s = &summary;
    info!(
        "Scan finished in {:.1} s: {} on the device, {} new, {} changed, {} already backed up, {} skipped, {} can't be compared, {} no longer on the device, {} not in what's backed up now, {}",
        started.elapsed().as_secs_f64(),
        counted(s.device_files, "file", "files"),
        s.new,
        s.mismatch,
        s.verified,
        s.skipped,
        s.errors,
        s.removed,
        s.outside,
        counted(s.issues, "problem", "problems")
    );
    Ok((plan, summary))
}

/// The path as the file system names it. A folder that doesn't exist yet is resolved through its
/// nearest existing parent, so a backup folder not yet made is still compared properly. On
/// Windows the letter case is dropped, since names there differ only by case at most.
fn comparable(path: &Path) -> PathBuf {
    let mut rest = Vec::new();
    let mut at = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut resolved = loop {
        match fs::canonicalize(&at) {
            Ok(real) => break real,
            Err(_) => match (at.parent().map(Path::to_path_buf), at.file_name()) {
                (Some(parent), Some(name)) => {
                    rest.push(name.to_os_string());
                    at = parent;
                }
                _ => break at,
            },
        }
    };
    resolved.extend(rest.iter().rev());
    if cfg!(windows) {
        PathBuf::from(resolved.to_string_lossy().to_lowercase())
    } else {
        resolved
    }
}

/// Refuse a backup folder written with `..` in it. On Unix such a path can lead into the
/// device's own folder while looking elsewhere, which the overlap check can't see.
fn refuse_dot_dot(backup: &Path) -> Result<(), DeviceError> {
    // (A verbatim Windows path, `\?\...`, reports `..` as an ordinary name.)
    let dot_dot = |c: std::path::Component<'_>| match c {
        std::path::Component::ParentDir => true,
        std::path::Component::Normal(n) => n == "..",
        _ => false,
    };
    if backup.components().any(dot_dot) {
        warn!(
            "Scan refused: the backup folder {} contains ..",
            backup.display()
        );
        return Err(DeviceError::Failed(format!(
            "The backup folder {} contains \"..\". Choose the folder by its full path, without \"..\" in it.",
            backup.display()
        )));
    }
    Ok(())
}

/// Refuse a scan when the backup folder and the folder the source reads (a mounted stick or
/// drive) contain one another: the backup would otherwise be read as part of the device, or the
/// device's files would be taken for backup content.
fn refuse_overlap(source_root: Option<&Path>, backup: &Path) -> Result<(), DeviceError> {
    let Some(source_root) = source_root else {
        return Ok(());
    };
    let (device, backup_folder) = (comparable(source_root), comparable(backup));
    let why = if backup_folder.starts_with(&device) {
        "is inside the folder the device is read from"
    } else if device.starts_with(&backup_folder) {
        "contains the folder the device is read from"
    } else {
        return Ok(());
    };
    warn!(
        "Scan refused: the backup folder {} {why} ({})",
        backup.display(),
        source_root.display()
    );
    Err(DeviceError::Failed(format!(
        "The backup folder {} {why} ({}). Choose a backup folder that is separate from the device.",
        backup.display(),
        source_root.display()
    )))
}

/// How many numbered names (`IMG (2)`, `IMG (3)`, …) to try in the archive before giving up.
pub const MAX_NUMBERED: u32 = 99;

/// The default name of the folder in the backup where files that left the device are moved,
/// each sync's in a dated subfolder. The name is set per device ([`DeviceSettings::archive_name`]).
pub const ARCHIVE_DIR: &str = "_Archive";

/// Archiving asks first when at least this many files would be archived and more than 1 in 20
/// of the backup's files.
pub const ARCHIVE_ASK_MIN: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedFile {
    pub size: u64,
    /// Path relative to the backup folder, with `/`, in its real letter case.
    pub path: String,
    /// The file's modified time on this computer, if it reports one.
    pub modified: Option<SystemTime>,
}

/// The files already in the backup folder, keyed case-insensitively (as NTFS compares names).
#[derive(Debug, Default)]
pub struct BackupIndex {
    files: HashMap<String, IndexedFile>,
    pub issues: Vec<ScanIssue>,
    /// Which device the folder belongs to ([`owner::MARKER`]).
    pub owner: OwnerState,
}

impl BackupIndex {
    pub fn get(&self, p: &SafeRelPath) -> Option<IndexedFile> {
        self.files.get(&p.fold_key()).cloned()
    }

    pub fn files(&self) -> impl Iterator<Item = &IndexedFile> {
        self.files.values()
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cancelled;

/// Whether `name` is on a list of names, ignoring case.
fn listed(list: &[String], name: &str) -> bool {
    list.iter().any(|n| n.eq_ignore_ascii_case(name))
}

/// Index the backup folder. Unreadable entries, links and leftover temporary files are
/// reported as issues rather than silently ignored. Links are not followed. The
/// archive folders (the marker's names and `archive_name`), the owner marker and PC clutter
/// (`ignore`, by file name) are left out.
pub fn index_backup(
    root: &Path,
    archive_name: &str,
    ignore: &[String],
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(usize),
) -> Result<BackupIndex, Cancelled> {
    let mut index = BackupIndex::default();
    if !root.exists() {
        info!(
            "The backup folder {} doesn't exist yet: every file counts as new",
            root.display()
        );
        return Ok(index);
    }
    index.owner = owner::read(root);
    match &index.owner {
        OwnerState::Unclaimed => info!("The backup folder has no owner yet ({})", owner::MARKER),
        OwnerState::Owned(o) => info!("The backup folder belongs to {}", o.device_name),
        OwnerState::Unreadable(e) => warn!(
            "The backup folder's owner marker {} can't be read ({e})",
            owner::MARKER
        ),
    }
    // The app's own entries are left out: the owner marker at the top, and archive folders at
    // any depth (an archive is never "backup files deleted from the device").
    let archives = owner::archive_names(&index.owner, archive_name);
    let mut walk = WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            let top_level_app_entry = e.depth() == 1 && is_app_entry(&name, &archives);
            let archive = e.depth() >= 1
                && e.file_type().is_dir()
                && archives.iter().any(|a| name.eq_ignore_ascii_case(a));
            !(top_level_app_entry || archive)
        });
    while let Some(entry) = walk.next() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled);
        }
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                let at = e.path().map(|p| shown(root, p)).unwrap_or_default();
                report(
                    &mut index.issues,
                    at,
                    format!("could not read this in the backup folder: {e}"),
                );
                continue;
            }
        };
        let ft = entry.file_type();
        // A folder with its own owner marker is another backup folder, perhaps another
        // phone's: none of it is this backup's, so it is never compared, copied over or archived.
        if ft.is_dir() && entry.depth() >= 1 && entry.path().join(owner::MARKER).exists() {
            index.issues.push(ScanIssue::new(
                shown(root, entry.path()),
                "another backup folder (it has its own owner marker); left alone",
            ));
            walk.skip_current_dir();
            continue;
        }
        if ft.is_symlink() {
            report(
                &mut index.issues,
                shown(root, entry.path()),
                "link or junction in the backup folder; not followed",
            );
            continue;
        }
        if !ft.is_file() {
            continue;
        }
        // PC clutter is never indexed, so it is never compared or archived.
        if listed(ignore, &entry.file_name().to_string_lossy()) {
            debug!("Ignored (PC clutter): {}", shown(root, entry.path()));
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let Some(rel) = rel.to_str() else {
            report(
                &mut index.issues,
                shown(root, entry.path()),
                "name in the backup folder is not valid Unicode; ignored",
            );
            continue;
        };
        let rel = rel.replace('\\', "/");
        if crate::paths::is_temp_name(&rel) {
            report(
                &mut index.issues,
                &rel,
                "unfinished copy left by an earlier run (safe to delete)",
            );
            continue;
        }
        match entry.metadata() {
            Ok(m) => {
                let file = IndexedFile {
                    size: m.len(),
                    path: rel.clone(),
                    modified: m.modified().ok(),
                };
                index.files.insert(rel.to_lowercase(), file);
            }
            Err(e) => report(
                &mut index.issues,
                &rel,
                format!("could not read this backup file's size: {e}"),
            ),
        }
        if index.files.len() % 200 == 0 {
            progress(index.files.len());
        }
    }
    progress(index.files.len());
    Ok(index)
}

/// Record a problem found in the backup folder, and log it.
fn report(issues: &mut Vec<ScanIssue>, at: impl Into<String>, problem: impl Into<String>) {
    let issue = ScanIssue::new(at, problem);
    warn!("Backup folder: {}: {}", issue.location, issue.problem);
    issues.push(issue);
}

/// Two modified times count as the same when they differ by up to 2 seconds (FAT keeps dates
/// to 2 seconds) or by one hour give or take 2 seconds, either way round (the daylight-saving
/// shift FAT drives show, combined with the 2-second rounding).
pub fn same_modified(a: SystemTime, b: SystemTime) -> bool {
    let diff = match a.duration_since(b) {
        Ok(d) => d,
        Err(e) => e.duration(),
    };
    let secs = Duration::from_secs;
    diff <= secs(2) || (secs(3598)..=secs(3602)).contains(&diff)
}

/// Whether a backup file is the device file: the same size and, when `by_date` is on and both
/// sides report a modified time, the same modified time ([`same_modified`]). The path has
/// already matched.
pub fn is_backed_up(
    device_size: u64,
    device_modified: Option<SystemTime>,
    have: &IndexedFile,
    by_date: bool,
) -> bool {
    if device_size != have.size {
        return false;
    }
    match (by_date, device_modified, have.modified) {
        (true, Some(d), Some(b)) => same_modified(d, b),
        _ => true,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Not in the backup: will be copied to `target`.
    New,
    /// A file at the same path differs (another size, or on storage another date): the new
    /// version is copied first, the old copy moves into the archive and the new one takes its
    /// place.
    Mismatch,
    /// Already backed up at `target` (same name and size).
    Verified,
    /// On the skip list for this device and folder.
    Skipped,
    /// Can't be classified; the message says why.
    Error(String),
}

/// Record each folder in `parts` (a file's path on the device, folders then the name) under its
/// lower-case key, with the exact spelling it has on the device.
fn note_folder_spellings(seen: &mut HashMap<String, HashSet<String>>, parts: &[String]) {
    let folders = parts.len().saturating_sub(1);
    for i in 1..=folders {
        let exact = parts[..i].join("/");
        seen.entry(exact.to_lowercase()).or_default().insert(exact);
    }
}

/// The index is case-insensitive but a Linux disk is not: a new file under a device folder
/// that the backup already has in other capitals (device `dcim/New.jpg`, backup `DCIM/`) would
/// make a second folder. A device folder that existing files were matched across case is the
/// backup's folder, so new files under it go into the backup's spelling.
///
/// A folder is only taken over when the device lists it in one spelling: two device folders that
/// differ only in case stay two names. Needs no file system, so it behaves the same everywhere.
fn adopt_backup_folder_spelling(
    entries: &mut [PlanEntry],
    device_spellings: &HashMap<String, HashSet<String>>,
) {
    // Exact device spelling of a folder -> the backup's spelling of it.
    let mut adopted: HashMap<String, String> = HashMap::new();
    for e in entries.iter().filter(|e| e.existing_size.is_some()) {
        let src: Vec<&str> = e.source.components().collect();
        let tgt: Vec<&str> = e.target.components().collect();
        if src.len() != tgt.len() {
            continue;
        }
        for i in 1..src.len() {
            let from = src[..i].join("/");
            let to = tgt[..i].join("/");
            if from != to && device_spellings.get(&from.to_lowercase()).map(HashSet::len) == Some(1)
            {
                adopted.entry(from).or_insert(to);
            }
        }
    }
    if adopted.is_empty() {
        return;
    }
    for e in entries.iter_mut().filter(|e| e.existing_size.is_none()) {
        let src: Vec<&str> = e.source.components().collect();
        // The deepest folder that has a spelling in the backup wins.
        for i in (1..src.len()).rev() {
            let Some(to) = adopted.get(&src[..i].join("/")) else {
                continue;
            };
            let mut parts: Vec<&str> = to.split('/').collect();
            parts.extend(&src[i..]);
            if let Ok(t) = SafeRelPath::from_components(&parts) {
                e.target = t;
            }
            break;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEntry {
    pub object_id: String,
    pub persistent_id: Option<String>,
    /// Path on the device (validated), with the storage's folder first when the device's
    /// storages are laid out one folder each. Also the skip-list key.
    pub source: SafeRelPath,
    pub size: Option<u64>,
    /// The device's modified time for the file: given to the copy.
    pub modified: Option<SystemTime>,
    pub status: Status,
    /// Where the file is, or would be written, under the backup folder.
    pub target: SafeRelPath,
    /// Size of the file already at `source`'s path in the backup, if any.
    pub existing_size: Option<u64>,
    /// The file already at this path in the backup is a JPG the app made, and the device has
    /// a JPG of its own here: the device's wins. The made copy goes the way a changed file's
    /// old copy goes, and the device's is copied in. It never counts in the cautions
    /// (DESIGN.md section 7.6).
    pub made_jpg: bool,
    /// For a HEIC whose backup copy has a JPG the app made: that JPG. If the HEIC changes
    /// ([`Status::Mismatch`]), the JPG moves the way the old HEIC copy does.
    pub old_jpg: Option<MadeJpg>,
}

/// A JPG the app made, in the backup folder: its path and size at scan time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MadeJpg {
    pub path: SafeRelPath,
    pub size: u64,
}

impl PlanEntry {
    /// Whether copying this file moves an existing backup copy into the archive: it changed
    /// (another size, or on storage another date), or it was skipped with a copy at its path.
    pub fn replaces_backup(&self) -> bool {
        match self.status {
            Status::Mismatch => true,
            Status::Skipped => self.existing_size.is_some(),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    /// Backup files that are no longer on the device (see [`ScanPlan::removed`]).
    pub removed: usize,
    /// Of those, files held back until confirmed.
    pub held: usize,
    /// Backup files outside what is backed up now: never counted as deleted.
    pub outside: usize,
    /// Of those, the old copies kept beside their files ([`ScanPlan::asides`]).
    pub asides: usize,
    pub device_files: usize,
    pub backup_files: usize,
    pub new: usize,
    pub mismatch: usize,
    pub verified: usize,
    pub skipped: usize,
    pub errors: usize,
    pub issues: usize,
    /// JPG copies the sync would make, going by the scan.
    pub jpg_to_make: usize,
}

/// A reason archiving asks for an explicit OK first (DESIGN.md sections 5.2 and 7.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caution {
    /// First sync into a folder that already has files and no marker.
    FirstSyncIntoFilledFolder { files: usize },
    /// At least [`ARCHIVE_ASK_MIN`] files, and more than 1 in 20 of the backup's.
    LargeBatch { count: usize, backup: usize },
    /// Fewer than half of the backup's files are on the device.
    FewOnDevice { on_device: usize, backup: usize },
    /// The device was recognised only by weak signals.
    WeakMatch,
    /// Files under a folder the device lists as empty: held back until confirmed.
    HeldBack { count: usize },
}

impl Caution {
    /// What the confirmation says.
    pub fn describe(&self) -> String {
        match self {
            Caution::FirstSyncIntoFilledFolder { files } => format!(
                "{} already here aren't on the device, and this folder has not been used for this device before",
                files_text(*files)
            ),
            Caution::LargeBatch { count, backup } => format!(
                "{} would be moved out of place, of {} in the backup",
                files_text(*count),
                files_text(*backup)
            ),
            Caution::FewOnDevice { on_device, backup } => format!(
                "only {on_device} of the backup's {} are on the device",
                files_text(*backup)
            ),
            Caution::WeakMatch => "the device was recognised only by its name, model and size, so it may be a different device".to_string(),
            Caution::HeldBack { count } => format!(
                "{} sit under a folder the device now lists as empty",
                files_text(*count)
            ),
        }
    }

    /// Whether, without the OK, no file is archived at all. Held-back files alone only hold
    /// back their own files.
    pub fn blocks_all(&self) -> bool {
        !matches!(self, Caution::HeldBack { .. })
    }
}

fn files_text(n: usize) -> String {
    counted(n, "file", "files")
}

/// How the device's storages are laid out in the backup, chosen at setup and stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageChoice {
    pub layout: StorageLayout,
    /// For [`StorageLayout::TopLevel`]: the storage whose files are at the top.
    pub top_level_storage: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ScanPlan {
    /// The device's ID ([`DeviceIdentity::key`]): the app-made one once the device is recognised
    /// or registered. Skip lists, the owner check and the sync check all use it.
    pub device_key: String,
    /// What the device looked like, for the backup folder's marker.
    pub signals: Signals,
    /// How the device was read.
    pub view: View,
    pub device_label: String,
    /// Backup folder, absolute.
    pub root: PathBuf,
    /// Its canonical form if it existed at scan time; sync refuses if this changes.
    pub root_canonical: Option<PathBuf>,
    pub entries: Vec<PlanEntry>,
    /// Everything that was found but can't be backed up, plus backup-folder problems.
    pub issues: Vec<ScanIssue>,
    /// Files in the backup folder that are no longer on the device, sorted by path. Sync can
    /// move them into the archive folder.
    pub removed: Vec<Removed>,
    /// Backup files outside what is backed up now (another folder, file type or storage): never
    /// counted as deleted. Sorted. "Not in what's backed up now."
    pub outside: Vec<String>,
    /// How many of `outside` are named like an old copy kept beside its file (`IMG (old
    /// 2026-11-18 09-30).jpg`): whoever made them, they are never treated as deleted from the
    /// device, so the scan says how many there are.
    pub asides: usize,
    /// Which device the backup folder belongs to, as of the scan.
    pub owner: OwnerState,
    /// The archive folder's name for this sync, from the device's settings.
    pub archive_name: String,
    pub archive_layout: ArchiveLayout,
    /// Where files deleted from the device go, and the old copy of a changed one.
    pub deleted_files: DeletedFiles,
    /// How many files the backup folder held at scan time.
    pub backup_files: usize,
    /// How many of those are JPG copies the app made (the made list). They are not device files:
    /// the summary leaves them out of "in the backup folder" and out of "not in what's backed up".
    pub made_jpgs: usize,
    /// How many of `outside` are JPG copies the app made.
    pub outside_made: usize,
    /// The JPG copies the app made that are in the backup (the made list at scan time), each
    /// with its HEIC.
    pub made: Vec<crate::heic::made::MadeEntry>,
    /// Why archiving needs an explicit OK ([`ScanPlan::archive_ok_needed`]).
    pub cautions: Vec<Caution>,
    /// The device uses the archive folder's name at the top of the backup: those files are
    /// not copied. Say so, so another name can be chosen.
    pub archive_clash: Option<String>,
    /// The top-level names the device's files use in the backup (lower case).
    pub top_names: Vec<String>,
    /// The storage layout, if this scan chose it (a first scan of a device); store it.
    pub storage_choice: Option<StorageChoice>,
    /// The connected storages that have a folder of their own in the backup: store them, so a
    /// storage taken out later never makes its backup look deleted.
    pub storage_folders: Vec<String>,
    /// The JPG copies of HEIC photos this sync would make ([`JpgPlan`]).
    pub jpg: JpgPlan,
}

/// A backup file whose device file is gone: no device file has its path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    /// Path in the backup folder.
    pub path: SafeRelPath,
    pub size: u64,
    /// Held back: it sits under a folder the device lists as empty, so it is archived only
    /// when archiving is confirmed.
    pub held: bool,
    /// If this is a HEIC the app made a JPG from: that JPG, which goes the same way.
    pub jpg: Option<MadeJpg>,
}

impl ScanPlan {
    /// Why this backup folder can't be used for this device (nothing copied or archived), if it
    /// can't: it belongs to another device, or its owner marker can't be read.
    pub fn folder_blocked(&self) -> Option<String> {
        match &self.owner {
            OwnerState::Unclaimed => None,
            OwnerState::Owned(o) => owner::refusal(o, &self.device_key, self.view),
            OwnerState::Unreadable(e) => Some(format!(
                "{} in the backup folder can't be read ({e})",
                owner::MARKER
            )),
        }
    }

    /// The reasons archiving needs an explicit OK before it moves anything; empty if it
    /// doesn't.
    pub fn archive_ok_needed(&self) -> Vec<String> {
        self.cautions.iter().map(Caution::describe).collect()
    }

    pub fn summary(&self, backup_files: usize) -> Summary {
        let mut s = Summary {
            removed: self.removed.len(),
            held: self.removed.iter().filter(|r| r.held).count(),
            outside: self.outside.len().saturating_sub(self.outside_made),
            asides: self.asides,
            device_files: self.entries.len(),
            backup_files: backup_files.saturating_sub(self.made_jpgs),
            issues: self.issues.len(),
            jpg_to_make: self.jpg.to_make,
            ..Default::default()
        };
        for e in &self.entries {
            match e.status {
                Status::New => s.new += 1,
                Status::Mismatch => s.mismatch += 1,
                Status::Verified => s.verified += 1,
                Status::Skipped => s.skipped += 1,
                Status::Error(_) => s.errors += 1,
            }
        }
        s
    }
}

/// A backup-folder path as shown in the Problems list: relative to the backup folder, with `/`.
fn shown(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(rel) if !rel.as_os_str().is_empty() => rel.to_string_lossy().replace('\\', "/"),
        _ => path.display().to_string(),
    }
}

/// What to back up (DESIGN.md section 7.1), from a device's settings and what it lists.
struct Selection {
    /// Lower-case top-level device folders to back up; `None` means every folder.
    folders: Option<Vec<String>>,
    /// Lower-case extensions to back up; `None` means every file.
    extensions: Option<HashSet<String>>,
    left_out: Vec<String>,
    ignore: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pick {
    Take,
    /// PC clutter: never backed up, never indexed.
    Clutter,
    /// A name on the left-out list.
    LeftOut,
    /// Not in the chosen folders or file types.
    Outside,
}

impl Selection {
    /// `present_top`: the lower-case top-level folders the device has (with files or listed
    /// empty). Chosen folders that exist narrow the backup to them; if none exists, the
    /// whole device is backed up.
    fn new(settings: &DeviceSettings, present_top: &HashSet<String>) -> Self {
        let configured: Vec<String> = settings.folders.iter().map(|f| f.to_lowercase()).collect();
        let folders = configured
            .iter()
            .any(|f| present_top.contains(f))
            .then_some(configured);
        let extensions = (settings.file_types == FileTypes::PhotosAndVideos).then(|| {
            settings
                .media_extensions
                .iter()
                .map(|e| e.trim_start_matches('.').to_lowercase())
                .collect()
        });
        Self {
            folders,
            extensions,
            left_out: settings.always_left_out.clone(),
            ignore: settings.ignore_names.clone(),
        }
    }

    /// Judge a path as the device has it (without a storage folder).
    fn test(&self, components: &[String]) -> Pick {
        let Some(name) = components.last() else {
            return Pick::Outside;
        };
        if listed(&self.ignore, name) {
            return Pick::Clutter;
        }
        if components.iter().any(|c| listed(&self.left_out, c)) {
            return Pick::LeftOut;
        }
        if let Some(folders) = &self.folders
            && !(components.len() >= 2 && folders.contains(&components[0].to_lowercase()))
        {
            return Pick::Outside;
        }
        if let Some(exts) = &self.extensions {
            let ext = name
                .rsplit_once('.')
                .filter(|(stem, _)| !stem.is_empty())
                .map(|(_, e)| e.to_lowercase());
            if !ext.is_some_and(|e| exts.contains(&e)) {
                return Pick::Outside;
            }
        }
        Pick::Take
    }
}

/// How the device's storages map into the backup folder (DESIGN.md section 6.1).
struct Storages {
    layout: StorageLayout,
    /// For `TopLevel`: the storage whose files sit at the top.
    flat: Option<String>,
    /// The storages the device lists now.
    present: Vec<String>,
    /// Storages that have had a folder of their own in the backup, connected or not.
    known: Vec<String>,
}

impl Storages {
    /// Work out the layout from the settings, or choose it on a device's first scan: a single
    /// storage stays at the top, several get a folder each. Returns the choice if this scan
    /// made one.
    fn resolve(settings: &DeviceSettings, files: &[DeviceFile]) -> (Self, Option<StorageChoice>) {
        let mut present: Vec<String> = Vec::new();
        for f in files {
            if let Some(s) = &f.storage
                && !listed(&present, s)
            {
                present.push(s.clone());
            }
        }
        match settings.storage_layout {
            StorageLayout::Unchosen => {
                if files.is_empty() {
                    return (
                        Self {
                            layout: StorageLayout::TopLevel,
                            flat: None,
                            known: settings.storage_folders.clone(),
                            present,
                        },
                        None,
                    );
                }
                let (layout, flat) = if present.len() <= 1 {
                    (StorageLayout::TopLevel, present.first().cloned())
                } else {
                    (StorageLayout::PerStorage, None)
                };
                let choice = StorageChoice {
                    layout,
                    top_level_storage: flat.clone(),
                };
                (
                    Self {
                        layout,
                        flat,
                        known: settings.storage_folders.clone(),
                        present,
                    },
                    Some(choice),
                )
            }
            layout => (
                Self {
                    layout,
                    flat: settings.top_level_storage.clone(),
                    known: settings.storage_folders.clone(),
                    present,
                },
                None,
            ),
        }
    }

    /// Whether this storage's files go at the top of the backup folder.
    fn is_flat(&self, storage: &str) -> bool {
        self.layout != StorageLayout::PerStorage
            && self
                .flat
                .as_deref()
                .is_some_and(|f| f.eq_ignore_ascii_case(storage))
    }

    /// The path parts a device file has in the backup folder: a storage that isn't the
    /// top-level one gets its own folder.
    fn backup_components(&self, file: &DeviceFile) -> Vec<String> {
        match &file.storage {
            Some(s) if !self.is_flat(s) => {
                let mut v = vec![s.clone()];
                v.extend(file.components.iter().cloned());
                v
            }
            _ => file.components.clone(),
        }
    }

    /// The device's own path for a path in the backup folder, if the device's listing can be
    /// compared with it. `None`: it belongs to a storage that isn't listed now, so nothing can
    /// be said about it.
    fn device_components(&self, backup: &[String]) -> Option<Vec<String>> {
        let first = backup.first()?;
        // A storage's folder: stripped if the storage is connected, not judged if it isn't.
        let own_folder = self
            .present
            .iter()
            .chain(self.known.iter())
            .find(|s| s.eq_ignore_ascii_case(first) && !self.is_flat(s));
        if own_folder.is_some() && backup.len() > 1 {
            return self
                .present
                .iter()
                .any(|s| s.eq_ignore_ascii_case(first))
                .then(|| backup[1..].to_vec());
        }
        match self.layout {
            StorageLayout::PerStorage => None,
            _ => match &self.flat {
                // The storage kept at the top isn't here now: its files can't be judged.
                Some(f) if !self.present.iter().any(|s| s.eq_ignore_ascii_case(f)) => None,
                _ => Some(backup.to_vec()),
            },
        }
    }
}

/// Classify every device file against the backup index.
pub fn build_plan(
    identity: &DeviceIdentity,
    root: &Path,
    enumeration: Enumeration,
    index: &BackupIndex,
    made: &MadeList,
    skip: &HashSet<String>,
    opts: &ScanOptions,
) -> ScanPlan {
    let settings = &opts.settings;
    let root_abs = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    let Enumeration {
        files,
        issues: device_issues,
        incomplete,
        empty_folders,
    } = enumeration;
    let archives = owner::archive_names(&index.owner, &settings.archive_name);
    let (storages, storage_choice) = Storages::resolve(settings, &files);

    // The top-level folders the device has, for "folders that exist".
    let with_files: HashSet<String> = files
        .iter()
        .filter(|f| f.components.len() >= 2)
        .map(|f| f.components[0].to_lowercase())
        .collect();
    let mut present_top = with_files.clone();
    present_top.extend(
        empty_folders
            .iter()
            .filter_map(|f| f.split('/').next())
            .map(str::to_lowercase),
    );
    let selection = Selection::new(settings, &present_top);

    // Issues under a left-out path are dropped.
    let mut issues: Vec<ScanIssue> = device_issues
        .into_iter()
        .filter(|i| {
            !i.location
                .split(['/', '\\'])
                .any(|c| listed(&settings.always_left_out, c))
        })
        .collect();
    issues.extend(index.issues.iter().cloned());
    let device_files = files.len();
    let mut entries = Vec::with_capacity(files.len());
    let mut seen: HashMap<String, String> = HashMap::new();
    // Every path on the device, lower-case, as it is in the backup: backup files not in here
    // and inside what's backed up were deleted from it.
    let mut on_device: HashSet<String> = HashSet::new();
    let mut top_names: HashSet<String> = HashSet::new();
    // The JPEGs the device has, backed up or not: a HEIC beside one gets no JPG copy.
    let mut device_jpgs: HashSet<String> = HashSet::new();
    // Every spelling of each folder the device lists, for `adopt_backup_folder_spelling`.
    let mut folder_spellings: HashMap<String, HashSet<String>> = HashMap::new();

    for file in files {
        let shown = file.display_path();
        let backup_parts = storages.backup_components(&file);
        note_folder_spellings(&mut folder_spellings, &backup_parts);
        if file
            .components
            .last()
            .is_some_and(|n| crate::heic::is_jpeg_name(n))
        {
            device_jpgs.insert(jpgs::stem_key(&backup_parts));
        }
        match selection.test(&file.components) {
            Pick::Take => {}
            pick => {
                // Still on the device, so never "deleted", but not backed up.
                on_device.insert(backup_parts.join("/").to_lowercase());
                debug!("Not backed up ({pick:?}): {shown}");
                continue;
            }
        }
        let source = match SafeRelPath::from_components(&backup_parts) {
            Ok(p) => p,
            Err(e) => {
                warn!("Not copied: {shown}: {e}");
                issues.push(ScanIssue::new(&shown, format!("not copied: {e}")));
                // Still on the device. An older version may have saved it under the name Windows
                // makes of it (trailing dots and spaces dropped), so don't call that deleted.
                let lenient: Vec<String> = backup_parts
                    .iter()
                    .map(|c| c.trim_end_matches(['.', ' ']).to_lowercase())
                    .collect();
                on_device.insert(lenient.join("/"));
                continue;
            }
        };
        on_device.insert(source.fold_key());
        if let Some(top) = source.components().next() {
            top_names.insert(top.to_lowercase());
        }
        // At any depth: a copied owner marker would make its folder look like another phone's
        // backup, and an archive folder is never indexed as backup files.
        if source.components().any(|c| is_app_entry(c, &archives)) {
            warn!("Not copied: {shown}: the app's own files in the backup folder use this name");
            issues.push(ScanIssue::new(
                &shown,
                "not copied: this name is used by the app's own files in the backup folder",
            ));
            continue;
        }
        if let Some(first) = seen.insert(source.fold_key(), shown.clone()) {
            warn!("Not copied: {shown}: same name on Windows as {first} (Windows ignores case)");
            issues.push(ScanIssue::new(
                &shown,
                format!("not copied: same name on Windows as {first} (Windows ignores case)"),
            ));
            continue;
        }
        let in_skip = skip.contains(source.as_str());
        let existing = index.get(&source);

        let status = match (&existing, file.size) {
            // Without a size a copy can't be checked, so the file is neither copied nor compared.
            (_, None) => Status::Error(
                "the device did not report this file's size, so it can't be copied or compared"
                    .into(),
            ),
            (None, Some(_)) => Status::New,
            (Some(have), Some(size)) => {
                if is_backed_up(size, file.modified, have, settings.compare_by_date) {
                    Status::Verified
                } else if size == 0 && have.size > 0 {
                    // A file that shrank to nothing is more likely a placeholder than a real
                    // change: the backup copy stays.
                    let why = format!(
                        "the file is now 0 bytes on the device but its backup copy has {} bytes: the backup copy is kept; check the file on the device",
                        have.size
                    );
                    warn!("Not replaced: {shown}: {why}");
                    issues.push(ScanIssue::new(&shown, format!("not replaced: {why}")));
                    Status::Error(why)
                } else {
                    Status::Mismatch
                }
            }
        };
        let status = match status {
            Status::New | Status::Mismatch if in_skip => Status::Skipped,
            s => s,
        };
        let existing_size = existing.as_ref().map(|f| f.size);
        log_classification(&source, file.size, existing_size, &status);
        // A made JPG the device now has its own of, or a made JPG that follows its HEIC.
        let made_jpg = existing_size.is_some() && made.made_by_us(source.as_str());
        let old_jpg = if existing_size.is_some() && crate::heic::is_heic_name(source.file_name()) {
            made_jpg_of(made, index, source.as_str())
        } else {
            None
        };
        // The index is case-insensitive but a Linux disk is not: a file that is already in the
        // backup is opened by its real name there, not by the case the device spells it in.
        let target = existing
            .as_ref()
            .and_then(|f| SafeRelPath::parse(&f.path).ok())
            .unwrap_or_else(|| source.clone());
        entries.push(PlanEntry {
            object_id: file.object_id,
            persistent_id: file.persistent_id,
            target,
            source,
            size: file.size,
            modified: file.modified,
            status,
            existing_size,
            made_jpg,
            old_jpg,
        });
    }

    adopt_backup_folder_spelling(&mut entries, &folder_spellings);

    let archive_clash = top_names
        .iter()
        .any(|t| t.eq_ignore_ascii_case(&settings.archive_name))
        .then(|| {
            let why = format!(
                "the device has a folder named {:?} at the top, which is also the archive folder's name, so its files are not backed up; choose another archive name",
                settings.archive_name
            );
            warn!("{why}");
            issues.push(ScanIssue::new("(archive folder)", why.clone()));
            why
        });

    let mut outside = Vec::new();
    let mut asides = 0;
    let removed = if device_files == 0 {
        // A locked iPhone shows no files. Never read that as "everything was deleted".
        if !index.is_empty() {
            warn!(
                "The device shows no files (is it locked?), so nothing in the backup folder counts as deleted from it"
            );
            issues.push(ScanIssue::new(
                "(device)",
                "the device shows no files (is it locked?), so nothing in the backup counts as deleted",
            ));
        }
        Vec::new()
    } else {
        // Chosen folders the device lists no files under: a glitch looks the same as a
        // deleted folder, so their files wait for a confirmation.
        let mut empty: Vec<String> = empty_folders.iter().map(|f| f.to_lowercase()).collect();
        if let Some(chosen) = &selection.folders {
            empty.extend(chosen.iter().filter(|f| !with_files.contains(*f)).cloned());
        }
        let ctx = RemovalContext {
            storages: &storages,
            selection: &selection,
            on_device: &on_device,
            incomplete: &incomplete,
            empty: &empty,
        };
        let (removed, mut more, out, kept_beside) = find_removed(index, made, &ctx);
        issues.append(&mut more);
        outside = out;
        asides = kept_beside;
        removed
    };
    outside.sort();

    // A made JPG that the device's own replaces is not a changed file of the device's.
    let changed = entries
        .iter()
        .filter(|e| e.status == Status::Mismatch && !e.made_jpg)
        .count();
    let cautions = work_out_cautions(opts, index, &removed, &outside, changed);
    let mut top_names: Vec<String> = top_names.into_iter().collect();
    top_names.sort();
    let jpg = if settings.jpeg_copies {
        jpgs::plan_jpgs(
            crate::heic::available(),
            &entries,
            index,
            made,
            &device_jpgs,
        )
    } else {
        JpgPlan::default()
    };
    let storage_folders: Vec<String> = storages
        .present
        .iter()
        .filter(|s| !storages.is_flat(s))
        .cloned()
        .collect();

    let outside_made = outside.iter().filter(|p| made.made_by_us(p)).count();
    ScanPlan {
        device_key: identity.key.clone(),
        signals: identity.signals.clone(),
        view: identity.view,
        device_label: identity.label(),
        root_canonical: fs::canonicalize(&root_abs).ok(),
        root: root_abs,
        entries,
        issues,
        removed,
        outside,
        asides,
        owner: index.owner.clone(),
        archive_name: settings.archive_name.clone(),
        archive_layout: settings.archive_layout,
        deleted_files: settings.deleted_files,
        backup_files: index.len(),
        made_jpgs: index.files().filter(|f| made.made_by_us(&f.path)).count(),
        made: made.entries().to_vec(),
        outside_made,
        cautions,
        archive_clash,
        top_names,
        storage_choice,
        storage_folders,
        jpg,
    }
}

/// The JPG the app made from the HEIC at `heic` in the backup, if it made one and it is still
/// there.
fn made_jpg_of(made: &MadeList, index: &BackupIndex, heic: &str) -> Option<MadeJpg> {
    let e = made.by_heic(heic)?;
    let path = SafeRelPath::parse(&e.jpg).ok()?;
    let have = index.get(&path)?;
    Some(MadeJpg {
        path,
        size: have.size,
    })
}

/// The reasons archiving needs an explicit OK (DESIGN.md sections 5.2 and 7.5).
fn work_out_cautions(
    opts: &ScanOptions,
    index: &BackupIndex,
    removed: &[Removed],
    outside: &[String],
    changed: usize,
) -> Vec<Caution> {
    let mut cautions = Vec::new();
    // Files that would be moved: those no longer on the device (unless they are left where they
    // are, which moves nothing), and the old copies of changed files.
    let moving_deleted = opts.settings.deleted_files.moves_deleted();
    let count = if moving_deleted { removed.len() } else { 0 } + changed;
    if count == 0 {
        return cautions;
    }
    let backup = index.len();
    let first_use = match &index.owner {
        OwnerState::Unclaimed => !index.is_empty(),
        OwnerState::Owned(o) => o.foreign_unconfirmed,
        OwnerState::Unreadable(_) => false,
    };
    if first_use {
        cautions.push(Caution::FirstSyncIntoFilledFolder { files: count });
    }
    if count >= ARCHIVE_ASK_MIN && count * 20 > backup {
        cautions.push(Caution::LargeBatch { count, backup });
    }
    // Of the backup files inside what's backed up now, how many are on the device?
    let considered = backup.saturating_sub(outside.len());
    let on_device = considered.saturating_sub(removed.len());
    if on_device * 2 < considered {
        cautions.push(Caution::FewOnDevice {
            on_device,
            backup: considered,
        });
    }
    if opts.weak_match {
        cautions.push(Caution::WeakMatch);
    }
    let held = removed.iter().filter(|r| r.held).count();
    if held > 0 && moving_deleted {
        cautions.push(Caution::HeldBack { count: held });
    }
    cautions
}

/// One debug line per device file: what the scan made of it.
fn log_classification(
    source: &SafeRelPath,
    size: Option<u64>,
    existing: Option<u64>,
    status: &Status,
) {
    let size = size.map_or_else(|| "size unknown".to_string(), |n| format!("{n} bytes"));
    match status {
        Status::New => debug!("New: {source} ({size})"),
        Status::Mismatch => debug!(
            "Changed: {source} is {size} on the device and {} bytes in the backup",
            existing.unwrap_or(0)
        ),
        Status::Verified => debug!("Backed up: {source} ({size})"),
        Status::Skipped => debug!("Skipped (on the skip list): {source} ({size})"),
        Status::Error(why) => warn!("Can't compare {source}: {why}"),
    }
}

/// Top-level names in the backup folder that belong to the app, not to the device.
fn is_app_entry(name: &str, archives: &[String]) -> bool {
    archives.iter().any(|a| name.eq_ignore_ascii_case(a)) || owner::is_marker_name(name)
}

struct RemovalContext<'a> {
    storages: &'a Storages,
    selection: &'a Selection,
    on_device: &'a HashSet<String>,
    incomplete: &'a [String],
    /// Lower-case device folders that list as empty (see [`Removed::held`]).
    empty: &'a [String],
}

/// Whether `path` (in the backup) has the name [`jpgs::jpg_path_for`] gives to a HEIC that is
/// in `on_device` (lower-case paths). Compared without regard to case: the safe way round, as
/// it only ever keeps a file from being called deleted.
fn jpg_of_device_heic(path: &str, on_device: &HashSet<String>) -> bool {
    let Some((stem, ext)) = path.rsplit_once('.') else {
        return false;
    };
    if !ext.eq_ignore_ascii_case("jpg") || stem.is_empty() || stem.ends_with('/') {
        return false;
    }
    let stem = stem.to_lowercase();
    ["heic", "heif"]
        .iter()
        .any(|e| on_device.contains(&format!("{stem}.{e}")))
}

/// Backup files that are no longer on the device, and the ones outside what's backed up now.
/// Files under a device folder that couldn't be listed completely are never included in the
/// first list: their device files may still be there. Files outside the chosen folders, file
/// types or storages are never included either: narrowing the selection archives nothing.
fn find_removed(
    index: &BackupIndex,
    made: &MadeList,
    ctx: &RemovalContext<'_>,
) -> (Vec<Removed>, Vec<ScanIssue>, Vec<String>, usize) {
    let unknown: Vec<String> = ctx.incomplete.iter().map(|f| f.to_lowercase()).collect();
    let mut seen_folders: Vec<&String> = ctx.incomplete.iter().collect();
    seen_folders.sort();
    seen_folders.dedup();
    for folder in seen_folders {
        let what = if folder.is_empty() {
            "the device".to_string()
        } else {
            folder.to_string()
        };
        warn!(
            "The scan couldn't read {what} completely: nothing under it counts as deleted from the device"
        );
    }
    let mut removed = Vec::new();
    let mut issues = Vec::new();
    let mut outside = Vec::new();
    let mut asides = 0;
    let mut unsure = 0;
    for f in index.files() {
        let key = f.path.to_lowercase();
        if ctx.on_device.contains(&key) {
            continue;
        }
        let backup_parts: Vec<String> = f.path.split('/').map(String::from).collect();
        // An old copy that Leave in place renamed beside its changed file is the backup's own
        // doing, never something deleted from the device. It goes in `outside` (not skipped
        // outright) so the "few files on the device" arithmetic still sees it as a backup file.
        if backup_parts
            .last()
            .is_some_and(|n| crate::paths::aside_original_name(n).is_some())
        {
            debug!("An old copy kept beside its file: {}", f.path);
            asides += 1;
            outside.push(f.path.clone());
            continue;
        }
        // A JPG the app made is never "deleted from the device": the device never had it. It
        // goes with its HEIC (see `Removed::jpg`). Like an old copy kept aside, it stays in
        // `outside`, so the arithmetic of the cautions still counts it as a backup file.
        if made.made_by_us(&f.path) {
            debug!("A JPG copy made by the app: {}", f.path);
            outside.push(f.path.clone());
            continue;
        }
        // The JPG of a HEIC that is on the device is never "deleted from the device" either,
        // even if the made list no longer vouches for it (the user edited or rotated it, or the
        // list is damaged): the device never had it, and it is the user's file now.
        if jpg_of_device_heic(&f.path, ctx.on_device) {
            debug!(
                "A JPG beside a HEIC that is still on the device: {}",
                f.path
            );
            outside.push(f.path.clone());
            continue;
        }
        // Inside what's backed up now? A path of a storage that isn't listed, or outside the
        // chosen folders or types, is not the scan's to judge.
        let device_parts = ctx.storages.device_components(&backup_parts);
        let inside = device_parts
            .as_ref()
            .is_some_and(|p| ctx.selection.test(p) == Pick::Take);
        if !inside {
            debug!("Not in what's backed up now: {}", f.path);
            outside.push(f.path.clone());
            continue;
        }
        let device_key = device_parts.unwrap_or_default().join("/").to_lowercase();
        if unknown
            .iter()
            .any(|u| u.is_empty() || device_key.starts_with(&format!("{u}/")))
        {
            unsure += 1;
            continue;
        }
        let held = ctx
            .empty
            .iter()
            .any(|e| device_key.starts_with(&format!("{e}/")));
        match SafeRelPath::parse(&f.path) {
            Ok(path) => {
                let jpg = if crate::heic::is_heic_name(path.file_name()) {
                    made_jpg_of(made, index, path.as_str())
                } else {
                    None
                };
                removed.push(Removed {
                    path,
                    size: f.size,
                    held,
                    jpg,
                });
            }
            Err(e) => {
                warn!(
                    "{} is no longer on the device, but can't be archived: {e}",
                    f.path
                );
                issues.push(ScanIssue::new(
                    &f.path,
                    format!("no longer on the device, but can't be archived: {e}"),
                ));
            }
        }
    }
    if unsure > 0 {
        warn!(
            "{} may have been deleted from the device, but the scan couldn't read its device folder completely, so they stay where they are",
            counted(unsure, "backup file", "backup files")
        );
        issues.push(ScanIssue::new(
            "(backup folder)",
            format!(
                "{} may have been deleted from the device, but the scan couldn't read their device folder completely, so they stay where they are",
                counted(unsure, "file", "files")
            ),
        ));
    }
    removed.sort_by(|a, b| a.path.as_str().cmp(b.path.as_str()));
    match removed.len() {
        0 => {}
        1 => info!("1 file in the backup folder is no longer on the device"),
        n => info!("{n} files in the backup folder are no longer on the device"),
    }
    if !outside.is_empty() {
        info!(
            "{} not in what's backed up now: left alone, never counted as deleted",
            counted(outside.len(), "backup file is", "backup files are")
        );
    }
    for r in &removed {
        debug!(
            "No longer on the device: {} ({} bytes){}",
            r.path,
            r.size,
            if r.held { ", held back" } else { "" }
        );
    }
    (removed, issues, outside, asides)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000 + secs)
    }

    #[test]
    fn modified_times_within_two_seconds_are_the_same() {
        assert!(same_modified(at(0), at(0)));
        assert!(same_modified(at(0), at(2)));
        assert!(same_modified(at(2), at(0)));
        assert!(!same_modified(at(0), at(3)));
        assert!(!same_modified(at(3), at(0)));
        // Just over 2 seconds is not the same.
        assert!(!same_modified(at(0), at(2) + Duration::from_millis(1)));
    }

    #[test]
    fn modified_times_an_hour_apart_give_or_take_two_seconds_are_the_same_either_way() {
        // The daylight-saving shift, alone and with FAT's 2-second rounding.
        for secs in [3598, 3599, 3600, 3601, 3602] {
            assert!(same_modified(at(0), at(secs)), "{secs}");
            assert!(same_modified(at(secs), at(0)), "{secs}");
        }
        assert!(!same_modified(at(0), at(3597)));
        assert!(!same_modified(at(0), at(3603)));
        assert!(!same_modified(at(0), at(7200)));
        assert!(!same_modified(at(0), at(3602) + Duration::from_millis(1)));
    }

    fn entry(path: &str, target: &str, existing: Option<u64>) -> PlanEntry {
        PlanEntry {
            object_id: path.into(),
            persistent_id: None,
            source: SafeRelPath::parse(path).unwrap(),
            size: Some(1),
            modified: None,
            status: if existing.is_some() {
                Status::Verified
            } else {
                Status::New
            },
            target: SafeRelPath::parse(target).unwrap(),
            existing_size: existing,
            made_jpg: false,
            old_jpg: None,
        }
    }

    fn spellings(paths: &[&str]) -> HashMap<String, HashSet<String>> {
        let mut m = HashMap::new();
        for p in paths {
            let parts: Vec<String> = p.split('/').map(String::from).collect();
            note_folder_spellings(&mut m, &parts);
        }
        m
    }

    #[test]
    fn a_new_file_goes_into_the_backups_spelling_of_a_matched_folder() {
        let mut v = vec![
            entry("dcim/a.jpg", "DCIM/a.jpg", Some(1)),
            entry("dcim/New.jpg", "dcim/New.jpg", None),
            entry("dcim/Cam/n.jpg", "dcim/Cam/n.jpg", None),
            entry("other/x.jpg", "other/x.jpg", None),
        ];
        let seen = spellings(&[
            "dcim/a.jpg",
            "dcim/New.jpg",
            "dcim/Cam/n.jpg",
            "other/x.jpg",
        ]);
        adopt_backup_folder_spelling(&mut v, &seen);
        assert_eq!(v[1].target.as_str(), "DCIM/New.jpg");
        assert_eq!(v[2].target.as_str(), "DCIM/Cam/n.jpg");
        assert_eq!(v[3].target.as_str(), "other/x.jpg");
        // The file already in the backup is untouched, and the source never changes.
        assert_eq!(v[0].target.as_str(), "DCIM/a.jpg");
        assert_eq!(v[1].source.as_str(), "dcim/New.jpg");
    }

    #[test]
    fn a_deeper_folder_is_matched_on_its_own() {
        let mut v = vec![
            entry("DCIM/camera/a.jpg", "DCIM/Camera/a.jpg", Some(1)),
            entry("DCIM/camera/b.jpg", "DCIM/camera/b.jpg", None),
        ];
        let seen = spellings(&["DCIM/camera/a.jpg", "DCIM/camera/b.jpg"]);
        adopt_backup_folder_spelling(&mut v, &seen);
        assert_eq!(v[1].target.as_str(), "DCIM/Camera/b.jpg");
    }

    #[test]
    fn two_device_folders_that_differ_only_in_case_stay_separate() {
        let mut v = vec![
            entry("dcim/a.jpg", "DCIM/a.jpg", Some(1)),
            entry("DCIM/b.jpg", "DCIM/b.jpg", None),
            entry("dcim/c.jpg", "dcim/c.jpg", None),
        ];
        let seen = spellings(&["dcim/a.jpg", "DCIM/b.jpg", "dcim/c.jpg"]);
        let before = v.clone();
        adopt_backup_folder_spelling(&mut v, &seen);
        assert_eq!(v, before);
    }

    #[test]
    fn a_folder_with_no_match_across_case_is_left_as_it_is() {
        let mut v = vec![entry("dcim/New.jpg", "dcim/New.jpg", None)];
        let seen = spellings(&["dcim/New.jpg"]);
        adopt_backup_folder_spelling(&mut v, &seen);
        assert_eq!(v[0].target.as_str(), "dcim/New.jpg");
    }

    fn have(size: u64, modified: Option<SystemTime>) -> IndexedFile {
        IndexedFile {
            size,
            path: "a".into(),
            modified,
        }
    }

    #[test]
    fn a_phone_file_is_backed_up_by_path_and_size_alone() {
        assert!(is_backed_up(
            10,
            Some(at(0)),
            &have(10, Some(at(500))),
            false
        ));
        assert!(!is_backed_up(
            10,
            Some(at(0)),
            &have(11, Some(at(0))),
            false
        ));
    }

    #[test]
    fn a_storage_file_must_also_match_in_date() {
        assert!(is_backed_up(10, Some(at(0)), &have(10, Some(at(1))), true));
        assert!(is_backed_up(
            10,
            Some(at(0)),
            &have(10, Some(at(3600))),
            true
        ));
        assert!(!is_backed_up(
            10,
            Some(at(0)),
            &have(10, Some(at(60))),
            true
        ));
        assert!(!is_backed_up(10, Some(at(0)), &have(11, Some(at(0))), true));
        // Without a date on one side there is nothing to compare: size decides.
        assert!(is_backed_up(10, None, &have(10, Some(at(60))), true));
        assert!(is_backed_up(10, Some(at(0)), &have(10, None), true));
    }
}
