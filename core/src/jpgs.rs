//! JPG copies of HEIC photos in a sync (DESIGN.md section 7.6): which HEICs get one, the sync's
//! JPG step, and the one-off backfill of HEICs already in the backup.
//!
//! [`crate::heic`] converts one file and keeps the list of what was made; this module decides
//! when. The rules, in one place ([`decide`]):
//! - a HEIC the device also has a JPG of (same folder, same name, `.jpg` or `.jpeg`) gets none;
//! - a name already taken by a file the app did not make is never touched: nothing is made and
//!   the reason is recorded;
//! - a JPG the app made from the HEIC as it is now is up to date;
//! - a HEIC that failed is not tried again until it changes or the user says "Try again".
//!
//! A failure never fails a sync. Everything is saved as the step goes, so a crash loses at most
//! the entry being written. Nothing here overwrites or deletes anything: a JPG is written to a
//! temporary file and moved into place without replacing ([`crate::heic::make_jpeg`]).
//!
//! What happens to a made JPG when its HEIC is deleted or changed is in [`crate::sync`].
#![forbid(unsafe_code)]

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use log::{debug, error, info, warn};

use crate::heic::made::{FailedEntry, MadeEntry, MadeList, facts_of, from_ms, to_ms};
use crate::heic::{self, MakeError};
use crate::owner::{self, OwnerState};
use crate::paths::{SafeRelPath, is_link, prepare_parent};
use crate::plan::{self, BackupIndex, ScanPlan, same_modified};
use crate::platform;
use crate::signals::View;
use crate::sync::SyncError;

/// A HEIC on the device that is being backed up, and the JPG that would sit beside it in the
/// backup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JpgCandidate {
    pub heic: SafeRelPath,
    pub jpg: SafeRelPath,
}

/// The JPG work a scan found (DESIGN.md section 7.6). Empty unless the device's setting is on
/// and the system can decode HEIC.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JpgPlan {
    /// The setting is on and a decoder is there: a sync makes JPG copies.
    pub enabled: bool,
    /// The setting is on but the system can't decode HEIC; says what to install.
    pub unavailable: Option<String>,
    /// Every HEIC being backed up that the device has no JPG of. The sync step looks at each
    /// one in the backup as it is by then ([`decide`]).
    pub candidates: Vec<JpgCandidate>,
    /// HEICs the device has its own JPG of: nothing is made for them.
    pub device_has_jpg: usize,
    /// How many of the candidates would get a JPG, going by the scan.
    pub to_make: usize,
    /// The size of those HEICs, for the estimate of the room the JPGs need
    /// ([`crate::space::jpg_bytes`]).
    pub estimate_heic_bytes: u64,
    /// The HEICs no JPG could be made from, as the made list had them at scan time.
    pub failed: Vec<FailedEntry>,
}

/// What to do about one HEIC (see the module's rules).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Make a JPG.
    Make,
    /// The app made one from this HEIC as it is: nothing to do.
    UpToDate,
    /// It failed before and nothing has changed since: not tried again.
    KnownFailure,
    /// A file of the JPG's name is there and the app didn't make it. Nothing is made.
    Taken,
    /// The JPG's name is the app's own, but made from another photo (`IMG.HEIC` and `IMG.HEIF`
    /// side by side).
    TakenByOther,
    /// The JPG there was made from an earlier version of this HEIC and is still in place.
    Stale,
}

/// Decides what to do about the HEIC `heic` whose JPG would be `jpg` (both relative paths).
/// `size` and `modified` are the HEIC's now. `will_change` says the sync is about to write or
/// replace the HEIC, so its date isn't known yet and an old made JPG is about to move away.
#[allow(clippy::too_many_arguments)]
pub fn decide(
    made: &MadeList,
    heic: &str,
    jpg: &str,
    size: u64,
    modified: Option<SystemTime>,
    target_exists: bool,
    will_change: bool,
) -> Decision {
    let same = |ms: i64| modified.is_none_or(|m| same_modified(m, from_ms(ms)));
    if !will_change
        && let Some(f) = made.failed_for(heic)
        && !f.name_taken
        && f.heic_size == size
        && same(f.heic_modified_ms)
    {
        return Decision::KnownFailure;
    }
    match made.by_jpg(jpg) {
        Some(e) => {
            if e.heic != heic {
                Decision::TakenByOther
            } else if !target_exists || will_change {
                Decision::Make
            } else if e.heic_size == size && same(e.heic_modified_ms) {
                Decision::UpToDate
            } else {
                Decision::Stale
            }
        }
        None if target_exists => Decision::Taken,
        None => Decision::Make,
    }
}

impl Decision {
    /// Why no JPG was made, in plain words, for the list of HEICs that didn't get one. `None`
    /// for the decisions that aren't a problem.
    pub fn reason(self, jpg_name: &str) -> Option<String> {
        match self {
            Decision::Taken => Some(format!(
                "a file named {jpg_name} is already there and this app didn't make it, so no JPG was made (nothing is overwritten)"
            )),
            Decision::TakenByOther => Some(format!(
                "{jpg_name} was made from another photo of the same name, so no JPG was made"
            )),
            Decision::Stale => Some(format!(
                "the {jpg_name} made from an earlier version of this photo is still there; it was not replaced"
            )),
            Decision::Make | Decision::UpToDate | Decision::KnownFailure => None,
        }
    }
}

/// The JPG's path for the HEIC at `heic`: beside it, named by [`heic::jpg_name_for`].
pub fn jpg_path_for(heic: &SafeRelPath) -> Option<SafeRelPath> {
    let name = heic::jpg_name_for(heic.file_name())?;
    let mut parts: Vec<&str> = heic.components().collect();
    parts.pop();
    parts.push(&name);
    SafeRelPath::from_components(&parts).ok()
}

/// A key for "this folder, this name without its extension", to tell whether the device has a
/// JPG of a HEIC. The folders are kept as they are (on Linux `DCIM/a.JPG` is not beside
/// `dcim/a.HEIC`); only the name's stem is lower-cased, by Unicode rules. Matching without regard
/// to case is only ever used to decide that a HEIC needs no JPG, the safe way round: nothing is
/// moved or hidden because of it (the made list is compared exactly).
pub fn stem_key(parts: &[impl AsRef<str>]) -> String {
    let Some((name, folder)) = parts.split_last() else {
        return String::new();
    };
    let name = name.as_ref();
    let stem = name.rsplit_once('.').map_or(name, |(s, _)| s);
    let mut key = String::new();
    for p in folder {
        key.push_str(p.as_ref());
        key.push('/');
    }
    key.push_str(&stem.to_lowercase());
    key
}

/// Works out the JPG part of a scan. `device_jpgs` holds [`stem_key`] of every JPEG the device
/// has, backed up or not. Candidates are the plan's HEICs: new, changed, already backed up, or
/// on the skip list with a copy in the backup.
pub(crate) fn plan_jpgs(
    enabled: Result<(), String>,
    entries: &[plan::PlanEntry],
    index: &BackupIndex,
    made: &MadeList,
    device_jpgs: &HashSet<String>,
) -> JpgPlan {
    let mut p = JpgPlan::default();
    match enabled {
        Ok(()) => p.enabled = true,
        Err(hint) => {
            p.unavailable = Some(hint);
            return p;
        }
    }
    p.failed = made.failed().to_vec();
    for e in entries {
        if !heic::is_heic_name(e.source.file_name()) {
            continue;
        }
        let will_change = matches!(e.status, plan::Status::New | plan::Status::Mismatch);
        let present = e.existing_size.is_some();
        match &e.status {
            plan::Status::Error(_) => continue,
            plan::Status::Skipped if !present => continue,
            _ => {}
        }
        let Some(jpg) = jpg_path_for(&e.target) else {
            continue;
        };
        let parts: Vec<&str> = e.target.components().collect();
        if device_jpgs.contains(&stem_key(&parts)) {
            p.device_has_jpg += 1;
            continue;
        }
        let have = index.get(&e.target);
        let size = if will_change {
            e.size.unwrap_or(0)
        } else {
            have.as_ref().map_or(0, |f| f.size)
        };
        let modified = if will_change {
            None
        } else {
            have.as_ref().and_then(|f| f.modified)
        };
        let target_exists = index.get(&jpg).is_some();
        let d = decide(
            made,
            e.target.as_str(),
            jpg.as_str(),
            size,
            modified,
            target_exists,
            will_change,
        );
        if d == Decision::Make {
            p.to_make += 1;
            p.estimate_heic_bytes = p.estimate_heic_bytes.saturating_add(size);
        }
        p.candidates.push(JpgCandidate {
            heic: e.target.clone(),
            jpg,
        });
    }
    p
}

/// What the JPG step did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JpgReport {
    /// JPG copies made.
    pub made: usize,
    /// HEICs that didn't get one this time, and are now on the failed list.
    pub failed: usize,
    /// HEICs that already had an up-to-date JPG.
    pub up_to_date: usize,
    /// HEICs that failed in an earlier run and haven't changed: not tried again.
    pub already_failed: usize,
    /// How many JPGs this run set out to make.
    pub total: usize,
    pub cancelled: bool,
    /// The system can't decode HEIC; says what to install. Nothing was tried.
    pub unavailable: Option<String>,
    /// The step stopped because of a problem that isn't one photo's (the disk is full, the list
    /// can't be saved). What was made so far is kept.
    pub error: Option<String>,
}

/// What happens to the JPG step as it goes, for the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JpgEvent {
    /// About to make the JPG for `heic`: `done` are finished of `total`.
    Started {
        done: usize,
        total: usize,
        heic: String,
    },
    Made {
        heic: String,
        jpg: String,
    },
    Failed {
        heic: String,
        reason: String,
    },
}

/// One JPG to make.
struct Work {
    heic: SafeRelPath,
    jpg: SafeRelPath,
}

fn plain_file(path: &Path) -> Option<fs::Metadata> {
    fs::symlink_metadata(path)
        .ok()
        .filter(|m| m.is_file() && !is_link(m))
}

/// Whether anything is at `path` (a link, a folder, anything). A path that can't be checked
/// counts as taken: nothing is written over what can't be seen.
fn exists(path: &Path) -> bool {
    match fs::symlink_metadata(path) {
        Err(e) => e.kind() != io::ErrorKind::NotFound,
        Ok(_) => true,
    }
}

fn failed_entry(
    heic: &str,
    reason: String,
    size: u64,
    modified: Option<SystemTime>,
    name_taken: bool,
) -> FailedEntry {
    FailedEntry {
        heic: heic.to_string(),
        reason,
        heic_size: size,
        heic_modified_ms: modified.map_or(0, to_ms),
        when: crate::platform::utc_stamp(),
        name_taken,
    }
}

/// Saves the list after a change. A list that can't be saved stops the step: a JPG the list
/// doesn't know about would look like a file of the device's.
fn save(made: &MadeList, root: &Path, report: &mut JpgReport) -> bool {
    match made.save(root) {
        Ok(()) => true,
        Err(e) => {
            let why =
                format!("the list of JPG copies could not be saved ({e}), so no more were made");
            error!("{why}");
            report.error = Some(why);
            false
        }
    }
}

/// Looks at every candidate in the backup as it is now, and sorts it: what to make, and what
/// to count or record. Records the HEICs whose name is taken.
fn sort_out(
    root_canon: &Path,
    made: &mut MadeList,
    candidates: Vec<(SafeRelPath, SafeRelPath)>,
    report: &mut JpgReport,
    on_event: &mut dyn FnMut(JpgEvent),
) -> Vec<Work> {
    let mut work = Vec::new();
    let mut recorded = false;
    for (heic, jpg) in candidates {
        let Some(meta) = plain_file(&heic.under(root_canon)) else {
            debug!("No JPG for {heic}: it isn't a plain file in the backup (yet)");
            continue;
        };
        let size = meta.len();
        let modified = meta.modified().ok();
        let target_exists = exists(&jpg.under(root_canon));
        match decide(
            made,
            heic.as_str(),
            jpg.as_str(),
            size,
            modified,
            target_exists,
            false,
        ) {
            Decision::Make => work.push(Work { heic, jpg }),
            Decision::UpToDate => report.up_to_date += 1,
            Decision::KnownFailure => report.already_failed += 1,
            d => {
                // A name that was already known to be taken and still is: the same news again.
                if made
                    .failed_for(heic.as_str())
                    .is_some_and(|f| f.name_taken && f.heic_size == size)
                {
                    report.already_failed += 1;
                    continue;
                }
                let reason = d.reason(jpg.file_name()).unwrap_or_default();
                warn!("No JPG for {heic}: {reason}");
                made.set_failed(failed_entry(
                    heic.as_str(),
                    reason.clone(),
                    size,
                    modified,
                    true,
                ));
                report.failed += 1;
                recorded = true;
                on_event(JpgEvent::Failed {
                    heic: heic.as_str().to_string(),
                    reason,
                });
            }
        }
    }
    if recorded {
        let _ = save(made, root_canon, report);
    }
    work
}

/// Makes the JPGs in `work`, one at a time, saving the list after each.
fn make_all(
    root_canon: &Path,
    made: &mut MadeList,
    work: Vec<Work>,
    cancel: &AtomicBool,
    report: &mut JpgReport,
    on_event: &mut dyn FnMut(JpgEvent),
) {
    report.total = work.len();
    info!(
        "Making {}",
        crate::counted(work.len(), "JPG copy", "JPG copies")
    );
    for (done, w) in work.iter().enumerate() {
        if report.error.is_some() {
            break;
        }
        if cancel.load(Ordering::Relaxed) {
            info!(
                "Making JPG copies stopped (Stop) after {done} of {}",
                work.len()
            );
            report.cancelled = true;
            break;
        }
        let heic_name = w.heic.as_str().to_string();
        on_event(JpgEvent::Started {
            done,
            total: work.len(),
            heic: heic_name.clone(),
        });
        let (heic_full, meta) = {
            let p = w.heic.under(root_canon);
            let m = plain_file(&p);
            (p, m)
        };
        let Some(meta) = meta else {
            continue;
        };
        // Room for this JPG, with the margin a sync keeps. Not one photo's fault, so nothing is
        // recorded against it; what was made so far stays.
        if let Err(short) = crate::space::check_jpg(meta.len(), platform::disk_space(root_canon)) {
            let why = format!("making JPG copies stopped: {short}");
            warn!("{why}");
            report.error = Some(why);
            break;
        }
        let jpg_full = match prepare_parent(root_canon, &w.jpg) {
            Ok(dir) => dir.join(w.jpg.file_name()),
            Err(e) => {
                fail(
                    made,
                    root_canon,
                    report,
                    on_event,
                    &w.heic,
                    format!("its folder can't be used ({e})"),
                    &meta,
                );
                continue;
            }
        };
        // The entry goes into the list before the JPG is written: a crash between the write and
        // the save would otherwise leave a JPG the list doesn't know, which would later be taken
        // for the device's own and archived as "deleted" on its own. The entry has no facts of the
        // JPG yet; the JPG gets its HEIC's modified time, and that is what it is then known by
        // ([`MadeList::load`]). Not if something is at the name already: that file isn't ours.
        if exists(&jpg_full) {
            // Taken since the sort: looked at again at the next scan, like any taken name.
            let reason = Decision::Taken
                .reason(w.jpg.file_name())
                .unwrap_or_default();
            warn!("No JPG for {}: {reason}", w.heic);
            made.set_failed(failed_entry(
                &heic_name,
                reason.clone(),
                meta.len(),
                meta.modified().ok(),
                true,
            ));
            report.failed += 1;
            on_event(JpgEvent::Failed {
                heic: heic_name,
                reason,
            });
            let _ = save(made, root_canon, report);
            continue;
        }
        made.insert(MadeEntry {
            jpg: w.jpg.as_str().to_string(),
            heic: heic_name.clone(),
            heic_size: meta.len(),
            heic_modified_ms: meta.modified().map_or(0, to_ms),
            made: crate::platform::utc_stamp(),
            jpg_size: None,
            jpg_modified_ms: None,
        });
        if !save(made, root_canon, report) {
            made.remove_jpg(w.jpg.as_str());
            break;
        }
        let result = heic::make_jpeg(&heic_full, &jpg_full, heic::QUALITY);
        if result.is_err() {
            // Nothing was written: the provisional entry goes again.
            made.remove_jpg(w.jpg.as_str());
        }
        match result {
            Ok(info) => {
                let (jpg_size, jpg_modified_ms) = match facts_of(&jpg_full) {
                    Some((s, m)) => (Some(s), Some(m)),
                    None => (None, None),
                };
                made.insert(MadeEntry {
                    jpg: w.jpg.as_str().to_string(),
                    heic: heic_name.clone(),
                    heic_size: info.heic_size,
                    heic_modified_ms: to_ms(info.heic_modified),
                    made: crate::platform::utc_stamp(),
                    jpg_size,
                    jpg_modified_ms,
                });
                made.clear_failed(&heic_name);
                report.made += 1;
                debug!("Made the JPG copy {}", w.jpg);
                on_event(JpgEvent::Made {
                    heic: heic_name,
                    jpg: w.jpg.as_str().to_string(),
                });
                if !save(made, root_canon, report) {
                    break;
                }
            }
            Err(MakeError::DecoderMissing(hint)) => {
                warn!("Making JPG copies stopped: {hint}");
                report.unavailable = Some(hint);
                let _ = made.save(root_canon); // without the provisional entry
                break;
            }
            Err(MakeError::Io(why)) => {
                let _ = made.save(root_canon);
                // Not this photo's fault (a full disk, a read-only folder): not recorded
                // against it, and the rest would fail the same way.
                let why = format!("a JPG could not be written ({why}); making JPG copies stopped");
                error!("{why}");
                report.error = Some(why);
                break;
            }
            Err(e) => {
                let reason = e.to_string();
                fail(made, root_canon, report, on_event, &w.heic, reason, &meta);
            }
        }
    }
    info!(
        "Making JPG copies finished: {} made, {} not made, {} already there{}",
        report.made,
        report.failed,
        report.up_to_date,
        if report.cancelled { " (stopped)" } else { "" }
    );
}

fn fail(
    made: &mut MadeList,
    root: &Path,
    report: &mut JpgReport,
    on_event: &mut dyn FnMut(JpgEvent),
    heic: &SafeRelPath,
    reason: String,
    meta: &fs::Metadata,
) {
    warn!("No JPG for {heic}: {reason}");
    made.set_failed(failed_entry(
        heic.as_str(),
        reason.clone(),
        meta.len(),
        meta.modified().ok(),
        false,
    ));
    report.failed += 1;
    on_event(JpgEvent::Failed {
        heic: heic.as_str().to_string(),
        reason,
    });
    let _ = save(made, root, report);
}

/// The sync's JPG step: makes a JPG beside each HEIC of the plan that needs one, after the
/// copying and archiving. A failure of any photo is recorded and never fails the sync; Stop
/// (`cancel`) ends it between two photos. Refuses, like the rest of the sync, if the backup
/// folder isn't this device's.
pub fn make_jpegs(
    plan: &ScanPlan,
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(JpgEvent),
) -> Result<JpgReport, SyncError> {
    let mut report = JpgReport::default();
    if !plan.jpg.enabled {
        return Ok(report);
    }
    if let Err(hint) = heic::available() {
        report.unavailable = Some(hint);
        return Ok(report);
    }
    let root_canon = crate::sync::checked_root(plan)?;
    crate::sync::check_owner(plan, &root_canon)?;
    let mut made = MadeList::load(&root_canon);
    if let Some(why) = &made.save_blocked {
        return Err(SyncError::Io(format!("no JPG copies were made: {why}")));
    }
    let candidates = plan
        .jpg
        .candidates
        .iter()
        .map(|c| (c.heic.clone(), c.jpg.clone()))
        .collect();
    let work = sort_out(&root_canon, &mut made, candidates, &mut report, on_event);
    make_all(&root_canon, &mut made, work, cancel, &mut report, on_event);
    Ok(report)
}

/// "Try again": forgets the recorded failure of each of `heics` (relative paths), or of every
/// HEIC if `heics` is `None`, so the next JPG step (or backfill) tries them again. Returns how
/// many were forgotten. Refuses if the backup folder isn't this device's.
pub fn forget_failures(
    root: &Path,
    device_key: &str,
    view: Option<View>,
    heics: Option<&[String]>,
) -> Result<usize, SyncError> {
    let root_canon = owned_root(root, device_key, view)?;
    let mut made = MadeList::load(&root_canon);
    if let Some(why) = &made.save_blocked {
        return Err(SyncError::Io(why.clone()));
    }
    let n = match heics {
        None => made.clear_all_failed(),
        Some(list) => list.iter().filter(|h| made.clear_failed(h)).count(),
    };
    if n > 0 {
        made.save(&root_canon)
            .map_err(|e| SyncError::Io(format!("could not save the list of JPG copies: {e}")))?;
    }
    info!(
        "Try again: {} will be tried again",
        crate::counted(n, "HEIC photo", "HEIC photos")
    );
    Ok(n)
}

/// "Try again", and then try: forgets the recorded failure of each of `heics` (relative paths),
/// or of every HEIC if `heics` is `None`, and makes their JPGs now, from the backup folder alone.
/// A name that is still taken is recorded again, and nothing is overwritten.
pub fn retry_failures(
    root: &Path,
    device_key: &str,
    view: Option<View>,
    heics: Option<&[String]>,
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(JpgEvent),
) -> Result<JpgReport, SyncError> {
    let mut report = JpgReport::default();
    if let Err(hint) = heic::available() {
        report.unavailable = Some(hint);
        return Ok(report);
    }
    let root_canon = owned_root(root, device_key, view)?;
    let mut made = MadeList::load(&root_canon);
    if let Some(why) = &made.save_blocked {
        return Err(SyncError::Io(format!("no JPG copies were made: {why}")));
    }
    let wanted: Vec<String> = match heics {
        None => made.failed().iter().map(|f| f.heic.clone()).collect(),
        Some(list) => list.to_vec(),
    };
    let mut candidates = Vec::new();
    let mut forgotten = false;
    for h in &wanted {
        if made.clear_failed(h) {
            forgotten = true;
            if let Ok(heic) = SafeRelPath::parse(h)
                && let Some(jpg) = jpg_path_for(&heic)
            {
                // The same test as the backfill's: a JPEG beside it that the app didn't make
                // (`.jpeg`, another case, or the exact name) is the device's, so none is made.
                if own_jpeg_beside(&root_canon, &made, &heic) {
                    debug!("No JPG for {heic}: a JPEG of the device's is beside it");
                    report.up_to_date += 1;
                    continue;
                }
                candidates.push((heic, jpg));
            }
        }
    }
    if !forgotten {
        return Ok(report);
    }
    made.save(&root_canon)
        .map_err(|e| SyncError::Io(format!("could not save the list of JPG copies: {e}")))?;
    if candidates.is_empty() {
        return Ok(report);
    }
    let work = sort_out(&root_canon, &mut made, candidates, &mut report, on_event);
    make_all(&root_canon, &mut made, work, cancel, &mut report, on_event);
    Ok(report)
}

/// Whether a JPEG the app did not make is beside the HEIC `heic` in the backup, with the same name
/// but for the extension and its case ([`stem_key`]): the device's own, so no JPG is made. Looks
/// only at the HEIC's folder.
fn own_jpeg_beside(root_canon: &Path, made: &MadeList, heic: &SafeRelPath) -> bool {
    let parts: Vec<&str> = heic.components().collect();
    let key = stem_key(&parts);
    let Some(folder) = heic.under(root_canon).parent().map(Path::to_path_buf) else {
        return false;
    };
    let Ok(dir) = fs::read_dir(folder) else {
        return false;
    };
    let folder_parts = &parts[..parts.len() - 1];
    dir.flatten().any(|e| {
        let name = e.file_name();
        let Some(name) = name.to_str() else {
            return false;
        };
        if !heic::is_jpeg_name(name) || !e.file_type().is_ok_and(|t| t.is_file()) {
            return false;
        }
        let mut p = folder_parts.to_vec();
        p.push(name);
        stem_key(&p) == key && !made.made_by_us(&p.join("/"))
    })
}

/// The backup folder, resolved, if it holds a marker saying it is this device's.
fn owned_root(root: &Path, device_key: &str, view: Option<View>) -> Result<PathBuf, SyncError> {
    let root_canon = fs::canonicalize(root).map_err(|e| {
        SyncError::Io(format!(
            "could not resolve the backup folder {}: {e}",
            root.display()
        ))
    })?;
    match owner::read(&root_canon) {
        OwnerState::Owned(o) => {
            let why = match view {
                Some(v) => owner::refusal(&o, device_key, v),
                None => (o.device_id != device_key).then(|| {
                    format!(
                        "this backup folder belongs to another device ({})",
                        o.device_name
                    )
                }),
            };
            match why {
                Some(why) => Err(SyncError::WrongFolder(why)),
                None => Ok(root_canon),
            }
        }
        OwnerState::Unclaimed => Err(SyncError::WrongFolder(
            "this backup folder has not been used for this device yet".into(),
        )),
        OwnerState::Unreadable(e) => Err(SyncError::WrongFolder(format!(
            "{} in the backup folder can't be read ({e})",
            owner::MARKER
        ))),
    }
}

/// The one-off backfill: makes JPG copies of the HEICs already in the backup folder, using only
/// the folder (the main folder, not the archive; no device needed). A name taken by
/// something that is not a JPEG file is skipped and recorded, and anything already made or failed
/// is left alone, so it carries on where it stopped if it is stopped (`cancel`) and run again.
///
/// A HEIC counts as having its own JPG when a JPEG of the same name (`.jpg` or `.jpeg`, any case)
/// that the app did not make is beside it ([`stem_key`]); the device isn't asked, and nothing is
/// made or recorded for it, as in the sync. Stopped while it looks, it reports the Stop, not an
/// error.
pub fn backfill(
    root: &Path,
    device_key: &str,
    view: Option<View>,
    archive_name: &str,
    ignore: &[String],
    cancel: &AtomicBool,
    on_event: &mut dyn FnMut(JpgEvent),
) -> Result<JpgReport, SyncError> {
    let mut report = JpgReport::default();
    if let Err(hint) = heic::available() {
        report.unavailable = Some(hint);
        return Ok(report);
    }
    let root_canon = owned_root(root, device_key, view)?;
    let mut made = MadeList::load(&root_canon);
    if let Some(why) = &made.save_blocked {
        return Err(SyncError::Io(format!("no JPG copies were made: {why}")));
    }
    let Ok(index) = plan::index_backup(&root_canon, archive_name, ignore, cancel, &mut |_| {})
    else {
        // Stopped while looking: nothing was made, which is a Stop and not an error.
        info!("Making JPG copies stopped (Stop) before any was made");
        report.cancelled = true;
        return Ok(report);
    };
    let mut heics: Vec<&plan::IndexedFile> = index
        .files()
        .filter(|f| heic::is_heic_name(f.path.rsplit('/').next().unwrap_or(&f.path)))
        .collect();
    heics.sort_by(|a, b| a.path.cmp(&b.path));
    // JPEGs in the backup that the app didn't make are the device's own.
    let own_jpegs: HashSet<String> = index
        .files()
        .filter(|f| {
            heic::is_jpeg_name(f.path.rsplit('/').next().unwrap_or(&f.path))
                && !made.made_by_us(&f.path)
        })
        .map(|f| stem_key(&f.path.split('/').collect::<Vec<_>>()))
        .collect();
    let mut candidates = Vec::new();
    for f in heics {
        let Ok(heic) = SafeRelPath::parse(&f.path) else {
            continue;
        };
        let Some(jpg) = jpg_path_for(&heic) else {
            continue;
        };
        // A JPEG beside it that the app didn't make (`.jpg` or `.jpeg`, any case) is the
        // device's own, as in the sync: no JPG is made and nothing is recorded.
        if own_jpegs.contains(&stem_key(&f.path.split('/').collect::<Vec<_>>())) {
            report.up_to_date += 1;
            continue;
        }
        candidates.push((heic, jpg));
    }
    let work = sort_out(&root_canon, &mut made, candidates, &mut report, on_event);
    make_all(&root_canon, &mut made, work, cancel, &mut report, on_event);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The key tells "this folder, this name" without regard to the case of the name and its
    /// extension, but the folders are compared exactly (on Linux `DCIM` and `dcim` are two
    /// folders).
    #[test]
    fn the_stem_key_folds_the_name_and_not_the_folders() {
        let key = |p: &str| stem_key(&p.split('/').collect::<Vec<_>>());
        assert_eq!(key("DCIM/IMG.heic"), key("DCIM/img.JPG"));
        assert_eq!(key("DCIM/100APPLE/a.b.HEIC"), key("DCIM/100APPLE/A.B.jpeg"));
        // Unicode letters fold too, the same way for both.
        assert_eq!(
            key("DCIM/\u{c9}t\u{e9}.HEIC"),
            key("DCIM/\u{e9}T\u{c9}.jpg")
        );
        // Another folder, or another name, is another key.
        assert_ne!(key("DCIM/a.JPG"), key("dcim/a.HEIC"));
        assert_ne!(key("DCIM/a.JPG"), key("DCIM/b.HEIC"));
        assert_ne!(key("DCIM/a.JPG"), key("DCIM/sub/a.HEIC"));
        assert_eq!(key("a.JPG"), key("A.heic"));
    }
}
