//! The window. Device work happens on the worker thread (worker.rs); the files under `app/`
//! draw the state and turn clicks into commands:
//! - `device_list`: the devices on the left and each one's menu;
//! - `device_page`: the top bar and the chosen device's page;
//! - `setup`: the recognition questions and the new-device card;
//! - `settings`: the device's Settings tab and the global settings;
//! - `dialogs`: confirmations and questions that need an answer;
//! - `sync_ops`, `progress`, `files_view`, `lists_view`: scans, syncs and their lists.

mod device_list;
mod device_page;
mod dialogs;
mod files_view;
mod history_view;
mod lists_view;
mod progress;
mod settings;
mod setup;
mod sync_ops;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, Sender};

use eframe::egui::{self, Color32, Key, RichText};
use egui_extras::{Column, TableBuilder};
use log::{debug, error, info, warn};
use pnk_core::heic::made::FailedEntry;
use pnk_core::history::SyncRecord;
use pnk_core::idfile::IdFile;
use pnk_core::plan::{ARCHIVE_DIR, PlanEntry, ScanPlan, ScanProgress, Status};
use pnk_core::platform::ListedDevice;
use pnk_core::platform::notify::Watcher;
use pnk_core::registry::{DeletedFiles, KnownDevice};
use pnk_core::signals::DeviceKind;
use pnk_core::state::{Config, LogLevel, NewDeviceDefaults, StateDir};
use pnk_core::sync::{ArchiveEvent, CopyMode, OldCopy, SyncEvent, SyncItem, SyncReport};

use crate::Options;
use crate::format;
use crate::log_view::{self, LogView};
use crate::logging::{self, Logger};
use crate::queue::{self, Gate, Queue};
use crate::theme;
use crate::worker::{
    self, Command, Event, Flags, FolderState, Found, How, Opened, ScanDone, Sizes, Stage,
    SyncOutcome,
};
use dialogs::Dialog;
use settings::{GlobalForm, SettingsForm};
use setup::SetupForm;

/// The backup folder offered until one is chosen: `Plug-n-Keep` in the user's home folder
/// (`%USERPROFILE%` on Windows, `$HOME` elsewhere), or the sample path when neither is set.
fn default_folder() -> String {
    let home = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(home)
        .filter(|p| !p.is_empty())
        .map(|p| PathBuf::from(p).join("Plug-n-Keep"))
        .unwrap_or_else(|| PathBuf::from(format::example_folder()))
        .display()
        .to_string()
}

// Light, with a little colour: blue for new, green for done, amber and red for attention.
const NORMAL: Color32 = theme::ACCENT;
const GOOD: Color32 = theme::GOOD;
const CAUTION: Color32 = theme::CAUTION;
const QUIET: Color32 = theme::WEAK;
const RED: Color32 = theme::DANGER;
const GREY: Color32 = theme::FAINT;

/// The pages of a device: what the tabs under its summary show.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PageTab {
    Files,
    Problems,
    SkipList,
    History,
    Settings,
}

/// The lists inside the Files tab.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Pending,
    New,
    Mismatch,
    Failed,
    /// HEICs no JPG copy could be made from.
    JpgFailed,
    /// The JPG copies the app made, with their HEICs.
    JpgMade,
    Deleted,
    All,
}

/// What is chosen in the device list.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Sel {
    /// A plugged-in device, by the ID it is listed with.
    Connected(String),
    /// A known device that is not plugged in, by its app-made ID.
    Known(String),
}

/// How far a plugged-in device is along, for the list.
#[derive(Clone, PartialEq, Eq)]
enum Link {
    /// Plugged in, waiting its turn: one device is looked at at a time.
    Waiting,
    Checking,
    Recognised(String),
    /// A question about it needs an answer.
    Asking,
    NewDevice,
    Failed(String),
}

#[allow(clippy::large_enum_variant)] // one value, replaced when a device is opened
enum DeviceState {
    None,
    Opening,
    Open(Box<Opened>),
    Failed(String),
}

/// What the window shows for one plan entry. Changes as the sync goes.
struct Row {
    status: Status,
    /// Failed during a sync in this session; Start sync tries it again.
    sync_failed: bool,
    target: String,
    /// Lower-case device path and target, for the filter box.
    search: String,
}

impl Row {
    fn new(e: &PlanEntry) -> Self {
        let mut r = Row {
            status: e.status.clone(),
            sync_failed: false,
            target: String::new(),
            search: String::new(),
        };
        r.set_target(e, e.target.as_str());
        r
    }

    fn set_target(&mut self, e: &PlanEntry, target: &str) {
        self.target = target.to_string();
        self.search = format!("{}\n{}", e.source.as_str(), target).to_lowercase();
    }

    fn is_pending(&self) -> bool {
        matches!(self.status, Status::New | Status::Mismatch) || self.sync_failed
    }
}

struct ScanView {
    plan_id: u64,
    plan: Arc<ScanPlan>,
    rows: Vec<Row>,
    /// One per `plan.removed`: what happened to each file deleted from the device.
    gone: Vec<Gone>,
    backup_files: usize,
    copied: usize,
    archived: usize,
    /// The HEICs no JPG copy could be made from, as of the scan and what has happened since.
    jpg_failed: Vec<FailedEntry>,
    /// The JPG copies the app made, as (JPG, HEIC) paths in the backup folder: the made list at
    /// the scan and what has been made since.
    jpg_made: Vec<(String, String)>,
    /// JPG copies the sync has dealt with since the scan (made, or tried and failed).
    jpg_done: usize,
}

impl ScanView {
    /// JPG copies still to be made: the scan's count less what the sync has dealt with.
    fn jpg_pending(&self) -> usize {
        jpg_pending(self.plan.jpg.to_make, self.jpg_done)
    }
}

enum Gone {
    Waiting,
    /// Moved to this path in the backup folder.
    Archived(String),
    /// Sent to the Recycle Bin or Trash.
    Binned,
    Failed(String),
}

#[derive(Default)]
struct SyncView {
    entry: Option<usize>,
    ordinal: usize,
    total: usize,
    bytes: u64,
    file_size: Option<u64>,
    speed: f64,
    /// Entries copied over their existing backup: no new file in the backup folder.
    replacing: HashSet<usize>,
    /// The batch's size as the device reported it, and how much of it is done (whatever
    /// the outcome) and actually copied.
    total_bytes: u64,
    finished_bytes: u64,
    copied_bytes: u64,
    started: Option<std::time::Instant>,
    /// "about 5 min left", worked out at most every [`ETA_EVERY`] so it can be read.
    eta: String,
    eta_updated: Option<std::time::Instant>,
    /// While the JPG step runs: the one being made, how many are done and how many there are.
    jpg: Option<JpgView>,
}

/// The JPG step's progress: "Making JPG copies 12 of 340".
#[derive(Clone, Default)]
struct JpgView {
    done: usize,
    total: usize,
    name: String,
}

/// How often the time left is worked out again.
const ETA_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

impl SyncView {
    /// The batch so far: files that have ended, however they ended, plus the current file.
    fn done_bytes(&self) -> u64 {
        self.finished_bytes + self.bytes
    }

    /// Average speed since the sync started, from the bytes actually copied.
    fn rate(&self) -> f64 {
        let elapsed = self.started.map_or(0.0, |t| t.elapsed().as_secs_f64());
        if elapsed > 0.0 {
            (self.copied_bytes + self.bytes) as f64 / elapsed
        } else {
            0.0
        }
    }

    /// Work out the time left again, unless that was done less than [`ETA_EVERY`] ago. Shown
    /// only after a few seconds, when the average speed means something.
    fn refresh_eta(&mut self) {
        let now = std::time::Instant::now();
        if self
            .eta_updated
            .is_some_and(|t| now.duration_since(t) < ETA_EVERY)
        {
            return;
        }
        let elapsed = self.started.map_or(0.0, |t| t.elapsed().as_secs_f64());
        let (done, rate) = (self.done_bytes(), self.rate());
        self.eta = if elapsed >= 3.0 && rate > 0.0 && self.total_bytes > done {
            format::time_left((self.total_bytes - done) as f64 / rate)
        } else {
            String::new()
        };
        self.eta_updated = Some(now);
    }
}

enum Busy {
    Idle,
    Scanning(Option<ScanProgress>),
    Syncing(SyncView),
    /// The backfill or "Try again": making JPG copies on their own.
    Jpgs(JpgView),
}

enum Confirm {
    ClearSkipList,
    Recopy(usize),
    /// Moving files out of place (to the archive or the bin) needs an explicit OK (the scan
    /// gives the reasons): sync and move them, or sync without moving.
    Archive {
        items: Vec<SyncItem>,
        /// Files deleted from the device that would be moved.
        count: usize,
        /// Changed files whose old copy would be moved.
        changed: usize,
    },
    ClaimFolder,
}

enum RowAction {
    SyncOne(usize),
    ToggleSkip(usize),
}

#[derive(Default)]
struct Counts {
    new: usize,
    mismatch: usize,
    verified: usize,
    skipped: usize,
    failed: usize,
    pending: usize,
    /// Files deleted from the device, not archived yet.
    deleted: usize,
    /// Sizes as the device reports them (unknown sizes count as 0).
    device_bytes: u64,
    verified_bytes: u64,
    pending_bytes: u64,
}

pub struct App {
    tx: Sender<Command>,
    rx: Receiver<Event>,
    flags: Flags,
    state: StateDir,
    /// The known devices, as the worker last said, and where each backup folder stands.
    known: Vec<KnownDevice>,
    folders: HashMap<String, FolderState>,
    devices: Vec<ListedDevice>,
    listing: bool,
    /// The listed device being ejected right now: the window waits for the system's answer.
    ejecting: Option<String>,
    /// The listed device that was ejected and has not been unplugged yet: its page says so.
    ejected: Option<String>,
    /// The "You can unplug it now" notice, so it can be taken down when the device goes.
    eject_notice: Option<String>,
    /// Why some devices can't be listed at all, from the last listing.
    device_hint: Option<String>,
    links: HashMap<String, Link>,
    queue: Queue,
    selected: Option<Sel>,
    /// The listed device the worker has open.
    current: Option<String>,
    device: DeviceState,
    /// The device opened by itself because it just arrived: if it turns out to be one we know
    /// and its "scan automatically" is on, it is scanned.
    auto_opened: Option<String>,
    /// Scan as soon as the device being set up is ready ("Save and scan").
    scan_when_ready: bool,
    /// The worker let go of the device's session because the phone stopped answering: the
    /// next Scan or Refresh opens the device again.
    session_dropped: bool,
    page: PageTab,
    /// How big each device's backup and disks are, as last worked out (after a scan or a sync).
    sizes: HashMap<String, Sizes>,
    /// Each device's sync history, newest first, as the worker last said.
    history: HashMap<String, Vec<SyncRecord>>,
    /// The history row whose full summary is shown.
    history_row: Option<usize>,
    scan: Option<ScanView>,
    /// The skip list for the scanned device and folder; unknown until a scan.
    skip_list: Option<Vec<String>>,
    busy: Busy,
    tab: Tab,
    search: String,
    filtered: Vec<usize>,
    filter_dirty: bool,
    /// Device paths whose copy ended a sync with a transfer problem in this session. They go
    /// last in the next sync, so one file that always fails can't block the rest.
    deferred: HashSet<String>,
    /// The device path of the last file that failed during the current sync.
    last_failed: Option<String>,
    notice: Option<String>,
    error: Option<String>,
    confirm: Option<Confirm>,
    dialog: Option<Dialog>,
    /// After the archive folder is renamed: the device's other settings to save.
    after_rename: Option<(String, Box<pnk_core::registry::DeviceSettings>)>,
    closing: bool,
    /// The worker's channel closed: the device thread is gone.
    worker_gone: bool,
    /// The saved settings (`config.json`): the backups root folder, whether to archive files
    /// deleted from a device, the log level, and what new devices start with.
    config: Config,
    /// The new-device card being filled in.
    setup: Option<SetupForm>,
    /// The Settings tab's working copy of the chosen device's settings.
    form: Option<SettingsForm>,
    /// The global settings page, while it is shown.
    global: Option<GlobalForm>,
    /// What "Find my backups" found.
    found: Option<Found>,
    /// Plug-in detection; stops when the window closes.
    #[allow(dead_code)]
    watcher: Option<Watcher>,
    /// Whether this computer can decode HEIC, found out when first needed and again on Refresh.
    heic_check: Option<Result<(), String>>,
    logger: &'static Logger,
    log_view: LogView,
}

impl App {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        state: StateDir,
        opts: Options,
        logger: &'static Logger,
    ) -> Self {
        theme::install(&cc.egui_ctx);
        // While the Log panel is open, new lines repaint the window (a few times a second).
        let ctx = cc.egui_ctx.clone();
        logger.set_waker(move || ctx.request_repaint_after(logging::REPAINT_EVERY));
        let config = state.config();
        let flags = Flags::default();
        let fake = opts.fake_device.clone().map(|folder| worker::Fake {
            folder,
            slow: opts.fake_slow,
            lag: opts.fake_lag,
            named: opts.fake_named,
        });
        let (tx, rx) = worker::spawn(cc.egui_ctx.clone(), state.clone(), fake, flags.clone());
        let _ = tx.send(Command::ListDevices);
        // A device plugged in or unplugged: the worker lists the devices again.
        let watcher = {
            let (tx, ctx) = (tx.clone(), cc.egui_ctx.clone());
            match pnk_core::platform::notify::watch(Box::new(move || {
                let _ = tx.send(Command::ListDevices);
                ctx.request_repaint();
            })) {
                Ok(w) => Some(w),
                Err(e) => {
                    warn!("Plug-in detection is not available ({e}); use Refresh");
                    None
                }
            }
        };
        let notice = opts.fake_device.as_ref().map(|d| {
            format!(
                "Fake-device mode: the files under {} stand in for a phone.",
                d.display()
            )
        });
        Self {
            tx,
            rx,
            flags,
            state,
            known: Vec::new(),
            folders: HashMap::new(),
            devices: Vec::new(),
            listing: true,
            ejecting: None,
            ejected: None,
            eject_notice: None,
            device_hint: None,
            links: HashMap::new(),
            queue: Queue::default(),
            selected: None,
            current: None,
            device: DeviceState::None,
            auto_opened: None,
            scan_when_ready: false,
            session_dropped: false,
            page: PageTab::Files,
            sizes: HashMap::new(),
            history: HashMap::new(),
            history_row: None,
            scan: None,
            skip_list: None,
            busy: Busy::Idle,
            heic_check: None,
            tab: Tab::Pending,
            search: String::new(),
            filtered: Vec::new(),
            filter_dirty: true,
            deferred: HashSet::new(),
            last_failed: None,
            notice,
            error: None,
            confirm: None,
            dialog: None,
            after_rename: None,
            closing: false,
            worker_gone: false,
            config,
            setup: None,
            form: None,
            global: None,
            found: None,
            watcher,
            logger,
            log_view: LogView::new(),
        }
    }

    fn send(&self, cmd: Command) {
        let _ = self.tx.send(cmd);
    }

    /// Whether HEIC photos can be decoded here, and if not what to install. Asked once.
    fn heic_ok(&mut self) -> Result<(), String> {
        self.heic_check
            .get_or_insert_with(pnk_core::heic::available)
            .clone()
    }

    fn idle(&self) -> bool {
        matches!(self.busy, Busy::Idle) && self.ejecting.is_none()
    }

    /// What happens to files deleted from the device, by the device's setting at the time of
    /// the scan the window holds.
    fn deleted_mode(&self) -> DeletedFiles {
        self.scan
            .as_ref()
            .map(|s| s.plan.deleted_files)
            .unwrap_or_default()
    }

    /// Whether files deleted from the device are moved out of the backup folder (to the archive
    /// or the bin), as opposed to left where they are.
    fn archive_on(&self) -> bool {
        self.deleted_mode().moves_deleted()
    }

    /// The backups root folder: where each device's folder is suggested.
    fn backups_root(&self) -> PathBuf {
        self.config
            .backup_dir
            .clone()
            .unwrap_or_else(|| PathBuf::from(default_folder()))
    }

    // -----------------------------------------------------------------------------------------
    // What is chosen

    fn known_by_id(&self, id: &str) -> Option<&KnownDevice> {
        self.known.iter().find(|d| d.id == id)
    }

    /// The opened device, if it is the one chosen and it has been told apart (recognised, or
    /// answered, or set up).
    fn active(&self) -> Option<(&Opened, &KnownDevice, How)> {
        let Some(Sel::Connected(listed)) = &self.selected else {
            return None;
        };
        if self.current.as_ref() != Some(listed) {
            return None;
        }
        let DeviceState::Open(o) = &self.device else {
            return None;
        };
        let Stage::Ready { device_id, how } = &o.stage else {
            return None;
        };
        Some((o, self.known_by_id(device_id)?, *how))
    }

    /// The known device whose page is shown, connected or not.
    fn shown_device(&self) -> Option<&KnownDevice> {
        match &self.selected {
            Some(Sel::Known(id)) => self.known_by_id(id),
            Some(Sel::Connected(_)) => self.active().map(|(_, d, _)| d),
            None => None,
        }
    }

    fn gate(&self) -> Gate {
        let needs_answer = matches!(
            &self.device,
            DeviceState::Open(o) if matches!(o.stage, Stage::Ask(_) | Stage::Setup(_))
        );
        Gate {
            idle: self.idle(),
            opening: matches!(self.device, DeviceState::Opening),
            needs_answer,
            scan_loaded: self.scan.is_some(),
            editing: self.confirm.is_some()
                || self.dialog.is_some()
                || self.after_rename.is_some()
                || self.form.as_ref().is_some_and(|f| f.dirty()),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Events from the worker

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Recovered(note) => self.notice = Some(note),
            Event::Devices(result) => {
                self.listing = false;
                match result {
                    Ok(list) => self.set_devices(list),
                    // A listing that failed says nothing about what is plugged in: keep the
                    // list (and the open device) as it was, and show the error.
                    Err(e) => self.error = Some(e),
                }
            }
            Event::DeviceHint(hint) => self.device_hint = hint,
            Event::Opened { id, result } => {
                if self.current.as_deref() != Some(id.as_str()) {
                    return; // an answer to a device that is no longer the open one
                }
                match result {
                    Ok(o) => self.on_opened(id, o),
                    Err(e) => {
                        self.links.insert(id, Link::Failed(e.clone()));
                        self.device = DeviceState::Failed(e);
                    }
                }
            }
            Event::Registry { devices, folders } => {
                self.known = devices;
                self.folders = folders.into_iter().collect();
                self.sync_forms();
                // A device that was forgotten leaves the list.
                if let Some(Sel::Known(id)) = &self.selected
                    && self.known_by_id(id).is_none()
                {
                    self.selected = None;
                }
            }
            Event::Ejected { id, label, result } => self.on_ejected(id, &label, result),
            Event::Notice(Ok(msg)) => self.notice = Some(msg),
            Event::Notice(Err(e)) => {
                // While the new-device card is up, a refusal is the worker saying no to
                // Save and scan: the scan it was waiting to start must not run later.
                if matches!(&self.device, DeviceState::Open(o) if matches!(o.stage, Stage::Setup(_)))
                {
                    self.scan_when_ready = false;
                }
                self.error = Some(e);
            }
            Event::IdFile(Ok(msg)) => {
                self.notice = Some(msg);
                self.error = None;
            }
            Event::IdFile(Err(e)) => self.error = Some(e),
            Event::ArchiveRenamed(result) => {
                self.clear_scan();
                let rest = self.after_rename.take();
                match result {
                    Ok(msg) => {
                        self.notice = Some(msg);
                        self.error = None;
                        if let Some((id, settings)) = rest {
                            self.send(Command::SetSettings { id, settings });
                        }
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            Event::Found(found) => self.found = Some(*found),
            Event::Sizes { id, sizes } => {
                self.sizes.insert(id, *sizes);
            }
            Event::History { id, records } => {
                // New records shift every row, so a selected row would point at another sync.
                self.history_row = None;
                self.history.insert(id, records);
            }
            Event::ScanProgress(p) => {
                if let Busy::Scanning(slot) = &mut self.busy {
                    *slot = Some(p);
                }
            }
            Event::ScanDone(done) => {
                self.busy = Busy::Idle;
                if self.is_current(&done.listed) {
                    self.load_scan(*done);
                } else {
                    info!("A scan of another device finished; it is ignored");
                    self.send(Command::DiscardPlan);
                }
            }
            Event::SessionDropped => self.session_dropped = true,
            Event::ScanFailed { listed, message } => {
                self.busy = Busy::Idle;
                if self.is_current(&listed) {
                    self.error = Some(message);
                }
            }
            Event::ScanCancelled => {
                self.busy = Busy::Idle;
                self.notice = Some("Scan cancelled.".into());
            }
            Event::Jpg(ev) => self.on_jpg_event(ev),
            Event::JpgFailed { id, failed } => {
                if let Some(scan) = self.scan.as_mut()
                    && scan.plan.device_key == id
                {
                    scan.jpg_failed = failed;
                    self.filter_dirty = true;
                }
            }
            Event::JpgDone(result) => {
                self.busy = Busy::Idle;
                // The JPGs made, moved or recorded since the scan make it stale (a sync with it
                // would act on a made list that has changed): it goes, like after a change of
                // settings, and the messages below are set after that.
                self.clear_scan();
                match result {
                    Ok(r) => {
                        let (mut notice, error) = sync_apply_jpg_summary(&r);
                        if r.failed == 0 {
                            notice += " Scan again to refresh the file list.";
                        }
                        self.notice = Some(notice);
                        self.error = error;
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            Event::Sync(ev) => self.on_sync_event(ev),
            Event::Archive(ev) => self.on_archive_event(ev),
            Event::SyncDone { listed, result } => {
                if !self.is_current(&listed) {
                    // The device is not the open one any more: what the sync did is in the
                    // history; this window has no scan or page for it.
                    self.busy = Busy::Idle;
                    info!("A sync of another device finished; its result is not shown");
                    return;
                }
                let copied_bytes = match &self.busy {
                    Busy::Syncing(v) => v.copied_bytes,
                    _ => 0,
                };
                self.busy = Busy::Idle;
                match result {
                    Ok(outcome) => {
                        let (notice, error) = outcome_summary(&outcome, copied_bytes);
                        // The worker is opening a new session, and the scan's object IDs
                        // don't carry over: clear it, but keep the error that says why.
                        let r = &outcome.copy;
                        if r.must_reconnect
                            && !r.cancelled
                            && r.skipped == 0
                            && let Some(path) = self.last_failed.take()
                        {
                            self.deferred.insert(path);
                        }
                        // The JPG step ran to its end: nothing is left to make, whatever the
                        // scan counted (a HEIC that changed or went since is not made).
                        if let (Some(scan), Some(Ok(j))) = (self.scan.as_mut(), &outcome.jpg)
                            && !j.cancelled
                            && j.error.is_none()
                            && j.unavailable.is_none()
                        {
                            scan.jpg_done = scan.plan.jpg.to_make;
                        }
                        let why = if outcome.copy.must_reconnect
                            || outcome.copy.connection_lost.is_some()
                        {
                            let why = self.error.take();
                            self.clear_scan();
                            why
                        } else {
                            None
                        };
                        self.notice = Some(notice);
                        self.error = error.or(why);
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            Event::Claimed(result) => match result {
                Ok(()) => {
                    self.start_scan();
                    self.notice =
                        Some("This backup folder is now this device's. Scanning again…".into());
                }
                Err(e) => self.error = Some(e),
            },
            Event::SkipList { plan_id, result } => {
                if self.scan.as_ref().is_some_and(|s| s.plan_id == plan_id) {
                    match result {
                        Ok(list) => self.apply_skip_list(list),
                        Err(e) => self.error = Some(format!("Could not change the skip list: {e}")),
                    }
                }
            }
        }
    }

    /// Whether `listed` (from a worker event) is the device the window has open. An empty ID
    /// (the worker had no device) is the window's own to show.
    fn is_current(&self, listed: &str) -> bool {
        listed.is_empty() || self.current.as_deref() == Some(listed)
    }

    /// The worker has opened a device and says what recognition made of it.
    fn on_opened(&mut self, listed: String, o: Box<Opened>) {
        let link = match &o.stage {
            Stage::Ready { device_id, .. } => Link::Recognised(device_id.clone()),
            Stage::Ask(_) => Link::Asking,
            Stage::Setup(_) => Link::NewDevice,
        };
        self.links.insert(listed.clone(), link);
        let stage = o.stage.clone();
        match &stage {
            Stage::Setup(info) => match &mut self.setup {
                Some(f) => f.refresh(info),
                None => {
                    self.setup = Some(SetupForm::new(
                        info,
                        o.kind,
                        &o.identity.signals,
                        &self.config.new_devices,
                        &self.backups_root(),
                        &self.known,
                    ));
                }
            },
            Stage::Ask(_) => self.setup = None,
            Stage::Ready { .. } => {
                self.setup = None;
                self.form = None;
            }
        }
        // Whether an ID file is worth offering for this stick (worked out before `o` is stored).
        let offer = match &stage {
            Stage::Ready { how, .. } => {
                o.is_volume
                    && o.kind == DeviceKind::Storage
                    && matches!(o.id_file, IdFile::Missing)
                    && match how {
                        How::Created => crate::questions::lacks_strong_id(
                            &o.identity.signals,
                            o.kind,
                            &self.known,
                        ),
                        How::Answered | How::Linked => true,
                        How::Recognised(_) | How::Reconnected => false,
                    }
            }
            _ => false,
        };
        self.device = DeviceState::Open(o);
        if let Stage::Ready { device_id, how } = stage {
            self.after_ready(&listed, device_id, how, offer);
        }
    }

    /// A device is known now: offer an ID file where that helps, and scan if asked to.
    fn after_ready(&mut self, listed: &str, device_id: String, how: How, offer: bool) {
        if offer {
            let name = self
                .known_by_id(&device_id)
                .map(|d| d.name.clone())
                .unwrap_or_default();
            self.dialog = Some(Dialog::IdFile {
                name,
                device: listed.to_string(),
                after_question: how != How::Created,
            });
        }
        let scan_now = std::mem::take(&mut self.scan_when_ready);
        let auto = self.auto_opened.as_deref() == Some(listed)
            && matches!(how, How::Recognised(_) | How::Answered | How::Linked)
            && self
                .known_by_id(&device_id)
                .is_some_and(|d| d.settings.scan_automatically);
        if self.auto_opened.as_deref() == Some(listed) {
            self.auto_opened = None;
        }
        if (scan_now || auto) && self.selected == Some(Sel::Connected(listed.to_string())) {
            if auto {
                info!("Scanning automatically: this device is set to scan when it is plugged in");
            }
            self.start_scan();
        }
    }

    fn set_devices(&mut self, list: Vec<ListedDevice>) {
        let before: Vec<String> = self.devices.iter().map(|d| d.id.clone()).collect();
        let now: Vec<String> = list.iter().map(|d| d.id.clone()).collect();
        let arrived = queue::arrivals(&before, &now);
        if !arrived.is_empty() {
            info!(
                "Plugged in: {}",
                list.iter()
                    .filter(|d| arrived.contains(&d.id))
                    .map(|d| format!("\"{}\"", d.label))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        self.devices = list;
        self.links.retain(|id, _| now.contains(id));
        self.queue.keep_only(&now);
        // An ejected device that has gone is just an unplugged one.
        // One that is plugged in again is a new arrival.
        if self
            .ejected
            .as_ref()
            .is_some_and(|id| !now.contains(id) || arrived.contains(id))
        {
            self.forget_ejected();
        }
        if let Some(cur) = self.current.clone()
            && !now.contains(&cur)
        {
            // The open device was unplugged.
            info!("The open device was unplugged");
            let device_id = match &self.device {
                DeviceState::Open(o) => match &o.stage {
                    Stage::Ready { device_id, .. } => Some(device_id.clone()),
                    _ => None,
                },
                _ => None,
            };
            self.current = None;
            self.device = DeviceState::None;
            self.setup = None;
            self.auto_opened = None;
            self.scan_when_ready = false;
            self.clear_scan();
            self.send(Command::Close);
            if self.selected == Some(Sel::Connected(cur)) {
                self.selected = device_id.map(Sel::Known);
            }
        }
        self.queue.add(arrived.clone(), self.current.as_deref());
        for id in arrived {
            self.links.entry(id).or_insert(Link::Waiting);
        }
        // Try again if opening failed before (e.g. the phone was locked).
        if matches!(self.device, DeviceState::Failed(_))
            && let Some(cur) = self.current.clone()
        {
            self.open_listed(&cur);
        }
        self.advance_queue();
    }

    /// Open the next device that is waiting, if nothing is in the way.
    fn advance_queue(&mut self) {
        if let Some(id) = self.queue.next(self.gate()) {
            info!("Looking at the next device that was waiting");
            self.auto_opened = Some(id.clone());
            self.selected = Some(Sel::Connected(id.clone()));
            self.open_listed(&id);
        }
    }

    /// Open a plugged-in device in the worker and match it.
    fn open_listed(&mut self, id: &str) {
        let Some(device) = self.devices.iter().find(|d| d.id == id).cloned() else {
            return;
        };
        self.clear_scan();
        self.session_dropped = false;
        self.setup = None;
        self.queue.remove(id);
        self.links.insert(id.to_string(), Link::Checking);
        self.current = Some(id.to_string());
        self.device = DeviceState::Opening;
        self.send(Command::Open(device));
    }

    /// The Eject button: the worker drops everything it holds on the stick or drive, then asks
    /// the system to eject it. The window's own scan goes too, and nothing is clickable until
    /// the answer comes.
    pub(super) fn eject_device(&mut self, id: &str) {
        let Some(device) = self.devices.iter().find(|d| d.id == id).cloned() else {
            return;
        };
        if !pnk_core::platform::can_eject(&device) || !self.idle() {
            return;
        }
        info!("Eject clicked for \"{}\"", device.label);
        self.clear_scan();
        self.error = None;
        self.notice = None;
        self.ejecting = Some(id.to_string());
        self.send(Command::Eject(device));
    }

    /// The ejected device has gone or come back: its page and its "you can unplug" notice no
    /// longer apply.
    fn forget_ejected(&mut self) {
        self.ejected = None;
        if let Some(text) = self.eject_notice.take()
            && self.notice.as_ref() == Some(&text)
        {
            self.notice = None;
        }
    }

    /// The system's answer to an eject. A refusal says why and opens the device again.
    fn on_ejected(&mut self, id: String, label: &str, result: Result<(), String>) {
        self.ejecting = None;
        match result {
            Ok(()) => {
                self.error = None;
                let text = eject_done_text(label);
                self.notice = Some(text.clone());
                self.eject_notice = Some(text);
                // The worker dropped its session with the device, so the page is closed on this
                // side too: "Open it again" opens it afresh.
                if self.current.as_deref() == Some(id.as_str()) {
                    self.current = None;
                    self.device = DeviceState::None;
                    self.setup = None;
                    self.scan_when_ready = false;
                    self.auto_opened = None;
                    self.session_dropped = false;
                }
                self.ejected = Some(id);
                // The device's row says "not plugged in" when the watcher sees it go; look now
                // as well, in case it already has.
                self.refresh();
            }
            Err(e) => {
                self.error = Some(e);
                self.notice = None;
                if self.selected == Some(Sel::Connected(id.clone())) {
                    self.open_listed(&id);
                }
            }
        }
    }

    /// The user chose a plugged-in device in the list.
    fn select_connected(&mut self, id: &str) {
        let again = self.selected == Some(Sel::Connected(id.to_string()));
        if !again {
            self.page = PageTab::Files;
            self.auto_opened = None;
            self.scan_when_ready = false;
            self.form = None;
        }
        self.selected = Some(Sel::Connected(id.to_string()));
        if self.current.as_deref() != Some(id) {
            self.open_listed(id);
        } else if again {
            // Choosing it again tries again after a failure.
            if matches!(self.device, DeviceState::Failed(_)) {
                self.open_listed(id);
            }
        }
    }

    /// The user chose a known device that is not plugged in.
    fn select_known(&mut self, id: &str) {
        if self.selected != Some(Sel::Known(id.to_string())) {
            self.page = PageTab::Files;
            self.form = None;
            self.scan_when_ready = false;
            self.auto_opened = None;
            self.clear_scan();
        }
        self.selected = Some(Sel::Known(id.to_string()));
    }

    /// Keep the working copies in step with the list of known devices.
    fn sync_forms(&mut self) {
        if let Some(f) = &mut self.form {
            let id = f.id().to_string();
            match self.known.iter().find(|d| d.id == id) {
                Some(d) => f.refresh(d),
                None => self.form = None,
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Actions

    /// The phone stopped answering and the worker dropped its session: open the device again,
    /// and scan once it is open if `scan` says so.
    pub(super) fn reopen_dropped(&mut self, scan: bool) -> bool {
        let Some(id) = self.current.clone().filter(|_| self.session_dropped) else {
            return false;
        };
        info!("Opening the device again: its session was dropped");
        self.open_listed(&id);
        self.scan_when_ready = scan;
        true
    }

    fn refresh(&mut self) {
        info!("Refresh: looking for devices again");
        if self.reopen_dropped(false) {
            self.notice = Some("Opening the device again…".into());
        }
        self.heic_check = None;
        self.listing = true;
        self.send(Command::ListDevices);
    }

    pub(super) fn save_config(&mut self) {
        if let Err(e) = self.state.save_config(&self.config) {
            error!("Could not save the settings: {e}");
            self.error = Some(format!("Could not save the settings: {e}"));
        }
    }

    /// The Record choice in the Log panel: applies at once, and is saved like the folder.
    fn set_log_level(&mut self, level: LogLevel) {
        if level == self.config.log_level {
            return;
        }
        let line = format!(
            "Log level changed from {:?} to {level:?}",
            self.config.log_level
        );
        // Logged at whichever of the two levels records more, so the change itself is kept.
        let more = level.filter() > self.config.log_level.filter();
        if !more {
            info!("{line}");
        }
        self.config.log_level = level;
        self.logger.set_level(level.filter());
        if more {
            info!("{line}");
        }
        self.save_config();
    }

    /// The device thread has ended (it panicked, or couldn't start): nothing will answer any
    /// more, so nothing is busy, and closing the window must not wait for it.
    fn on_worker_gone(&mut self) {
        if self.worker_gone {
            return;
        }
        self.worker_gone = true;
        error!("The background worker has stopped");
        self.busy = Busy::Idle;
        self.ejecting = None;
        self.listing = false;
        let note = "The background worker stopped unexpectedly, so nothing more can be scanned or copied. Close the window and open the app again.";
        self.error = Some(match self.error.take() {
            Some(e) => format!("{e} {note}"),
            None => note.to_string(),
        });
    }

    /// Closing mid-scan or mid-copy: stop cleanly first, then close.
    fn handle_close(&mut self, ctx: &egui::Context) {
        if ctx.input(|i| i.viewport().close_requested()) && !self.idle() && !self.worker_gone {
            info!("Closing while busy: stopping first");
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.closing = true;
            self.flags.scan_cancel.store(true, Ordering::Relaxed);
            self.flags.sync.cancel.store(true, Ordering::Relaxed);
            self.notice = Some("Stopping before closing…".into());
        }
        if self.closing && (self.idle() || self.worker_gone) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}

impl eframe::App for App {
    fn logic(&mut self, _ctx: &egui::Context, _frame: &mut eframe::Frame) {
        loop {
            match self.rx.try_recv() {
                Ok(ev) => self.on_event(ev),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.on_worker_gone();
                    break;
                }
            }
        }
        self.advance_queue();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.handle_close(&ctx);
        if matches!(self.busy, Busy::Syncing(_))
            && self.confirm.is_none()
            && self.dialog.is_none()
            && !ctx.text_edit_focused()
            && ctx.input(|i| i.key_pressed(Key::S))
        {
            self.skip_current();
        }
        if self.filter_dirty {
            self.refilter();
        }
        let counts = self.counts();

        let top = egui::Frame::new()
            .fill(theme::SURFACE)
            .inner_margin(egui::Margin::symmetric(20, 0));
        egui::Panel::top("top").frame(top).show(ui, |ui| {
            self.top_bar(ui);
        });
        self.logger.set_watching(self.log_view.open);
        if self.log_view.open {
            let frame = egui::Frame::new()
                .fill(egui::Color32::WHITE)
                .inner_margin(egui::Margin {
                    left: 20,
                    right: 20,
                    top: 8,
                    bottom: 4,
                });
            // Leave the page room however far the panel is dragged up.
            let max = (ui.available_height() - 160.0).max(140.0);
            let action = egui::Panel::bottom("log")
                .frame(frame)
                .resizable(true)
                .default_size(250.0)
                .size_range(110.0..=max)
                .show(ui, |ui| {
                    self.log_view.ui(ui, self.logger, self.config.log_level)
                })
                .inner;
            match action {
                Some(log_view::Action::SetLevel(level)) => self.set_log_level(level),
                Some(log_view::Action::Cleared) => info!("Cleared the log"),
                None => {}
            }
        }
        let side = egui::Frame::new()
            .fill(theme::SURFACE)
            .inner_margin(egui::Margin::symmetric(10, 10));
        egui::Panel::left("devices")
            .frame(side)
            .resizable(false)
            .exact_size(236.0)
            .show(ui, |ui| self.device_list(ui));
        let body = egui::Frame::new()
            .fill(egui::Color32::WHITE)
            .inner_margin(egui::Margin::symmetric(20, 14));
        egui::CentralPanel::default_margins()
            .frame(body)
            .show(ui, |ui| {
                if self.global.is_some() {
                    self.global_settings(ui);
                } else {
                    self.page(ui, &counts);
                }
            });
        self.confirm_dialog(&ctx);
        self.dialog_ui(&ctx);
    }
}

fn set_backed_up(scan: &mut ScanView, entry: usize, target: &str) {
    if let (Some(row), Some(e)) = (scan.rows.get_mut(entry), scan.plan.entries.get(entry)) {
        row.status = Status::Verified;
        row.sync_failed = false;
        row.set_target(e, target);
    }
}

/// The JPG copies still to make: what the scan counted less what the sync has made or tried.
fn jpg_pending(to_make: usize, done: usize) -> usize {
    to_make.saturating_sub(done)
}

/// The notice after a sync, and an error if moving files was refused.
fn outcome_summary(o: &SyncOutcome, copied_bytes: u64) -> (String, Option<String>) {
    let mut notice = sync_summary(&o.copy, copied_bytes);
    let mut error = None;
    match &o.archive {
        Some(Ok(a)) => {
            if let Some(folder) = &a.folder {
                notice += &format!(
                    " Moved {} deleted from the device into \"{}\".",
                    format::files(a.moved),
                    folder.as_str().replace('/', "\\")
                );
            } else if a.mode == DeletedFiles::RecycleBin && a.moved > 0 {
                notice += &format!(
                    " Sent {} deleted from the device to the {}.",
                    format::files(a.moved),
                    pnk_core::platform::BIN_NAME
                );
            }
            if a.failed > 0 {
                notice += &format!(
                    " {} could not be moved and {} left in place: see Deleted from device.",
                    format::count(a.failed),
                    format::pick(a.failed, "was", "were")
                );
            }
        }
        Some(Err(e)) => error = Some(e.clone()),
        None => {}
    }
    match &o.jpg {
        Some(Ok(j)) => {
            let (text, err) = jpg_summary(j, true);
            notice += &format!(" {text}");
            error = error.or(err);
        }
        Some(Err(e)) => {
            error = error.or_else(|| Some(format!("JPG copies were not made: {e}")));
        }
        None => {}
    }
    (notice, error)
}

/// What the JPG step did, in a sentence, and what stopped it if something other than one photo
/// did.
/// `scan_kept` says the window still holds the scan the failures are listed in; if not, the
/// sentence says to scan again.
fn jpg_summary(j: &pnk_core::jpgs::JpgReport, scan_kept: bool) -> (String, Option<String>) {
    let mut s = if j.made == 0 && j.failed == 0 {
        "No JPG copies were needed.".to_string()
    } else {
        format!(
            "Made {}.",
            if j.made == 1 {
                "1 JPG copy".to_string()
            } else {
                format!("{} JPG copies", format::count(j.made))
            }
        )
    };
    if j.failed > 0 {
        s += &format!(
            " {} couldn't be made: {}see Couldn't make a JPG.",
            format::count(j.failed),
            if scan_kept { "" } else { "scan again, then " }
        );
    }
    if j.cancelled {
        s += " Stopped; the rest are made next time.";
    }
    let mut err = j.error.clone();
    if let Some(hint) = &j.unavailable {
        err = Some(hint.clone());
    }
    (s, err)
}

/// The notice and error after the backfill or "Try again".
fn sync_apply_jpg_summary(j: &pnk_core::jpgs::JpgReport) -> (String, Option<String>) {
    jpg_summary(j, false)
}

fn sync_summary(r: &SyncReport, copied_bytes: u64) -> String {
    let mut parts = vec![if r.copied > 0 {
        format!(
            "{} copied ({})",
            format::count(r.copied),
            format::bytes(copied_bytes)
        )
    } else {
        "0 copied".to_string()
    }];
    if r.already_backed_up > 0 {
        parts.push(format!(
            "{} already backed up",
            format::count(r.already_backed_up)
        ));
    }
    if r.skipped > 0 {
        parts.push(format!("{} skipped", format::count(r.skipped)));
    }
    if r.failed > 0 {
        parts.push(format!("{} failed", format::count(r.failed)));
    }
    let head = if r.cancelled || r.must_reconnect || r.disk_full || r.connection_lost.is_some() {
        "Sync stopped"
    } else if r.failed > 0 {
        "Sync finished with errors"
    } else {
        "Sync complete"
    };
    let mut s = format!("{head}: {}.", parts.join(", "));
    if r.disk_full {
        s += " The backup disk is full, so the sync stopped. The file being copied was removed and \
               nothing already backed up was touched. Free some space, or choose another backup \
               folder, and scan again.";
        if r.must_reconnect {
            s += " The transfer was cut short, so the connection to the device was reset too.";
        }
    } else if r.connection_lost.is_some() {
        s += " The device stopped answering, so the sync stopped there. Unplug it, unlock it, plug                it back in, then scan again to carry on.";
    } else if r.must_reconnect {
        s += if r.cancelled || r.skipped > 0 {
            " The file being copied was cut short, so the connection to the device was reset. \
             Scan again to carry on."
        } else {
            " A copy went wrong, so the connection to the device was reset. Scan again to \
             carry on. If it happens again, unplug the device, plug it back in and scan again."
        };
    } else if r.failed > 0 {
        s += " Right-click a failed file to try it again or skip it.";
    }
    s
}

/// A backup path as Windows shows it: `\` between folders.
fn windows_path(p: &str) -> String {
    p.replace('/', "\\")
}

fn status_label(s: &Status) -> (&'static str, Color32) {
    match s {
        Status::New => ("New", NORMAL),
        Status::Mismatch => ("Changed", CAUTION),
        Status::Verified => ("Backed up", GOOD),
        Status::Skipped => ("Skipped", GREY),
        Status::Error(_) => ("Failed", RED),
    }
}

/// One column of the progress panel's detail line: exactly `width` wide, text from the left
/// edge, so it starts in the same place however long the text is.
fn progress_cell(ui: &mut egui::Ui, width: f32, text: String) {
    let height = ui.spacing().interact_size.y;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
        |ui| {
            ui.add(egui::Label::new(RichText::new(text).size(11.5).color(theme::WEAK)).truncate());
        },
    );
}

/// `items` with those `deferred` moved to the end, otherwise in the same order.
fn defer_last(items: Vec<SyncItem>, deferred: impl Fn(usize) -> bool) -> Vec<SyncItem> {
    let (later, mut first): (Vec<_>, Vec<_>) = items.into_iter().partition(|i| deferred(i.entry));
    first.extend(later);
    first
}

/// "You can unplug Work stick now."
fn eject_done_text(label: &str) -> String {
    format!("You can unplug {label} now.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_files_go_last_and_the_rest_keep_their_order() {
        let items: Vec<SyncItem> = (0..5)
            .map(|entry| SyncItem {
                entry,
                mode: CopyMode::NoReplace,
            })
            .collect();
        let order: Vec<usize> = defer_last(items, |e| e == 1 || e == 3)
            .iter()
            .map(|i| i.entry)
            .collect();
        assert_eq!(order, [0, 2, 4, 1, 3]);
    }

    #[test]
    fn the_jpg_count_on_the_sync_button_falls_as_copies_are_made_and_ends_at_nothing() {
        assert_eq!(jpg_pending(40, 0), 40);
        assert_eq!(jpg_pending(40, 38), 2);
        // Made or failed, each is dealt with; more than counted never goes below nothing.
        assert_eq!(jpg_pending(40, 40), 0);
        assert_eq!(jpg_pending(1, 3), 0);
    }

    #[test]
    fn the_jpg_summary_says_to_scan_again_only_when_the_scan_is_gone() {
        let j = pnk_core::jpgs::JpgReport {
            made: 2,
            failed: 1,
            ..Default::default()
        };
        let (kept, _) = jpg_summary(&j, true);
        let (gone, _) = jpg_summary(&j, false);
        assert!(kept.contains("see Couldn't make a JPG") && !kept.contains("scan again"));
        assert!(gone.contains("scan again, then see Couldn't make a JPG"));
        let stopped = pnk_core::jpgs::JpgReport {
            cancelled: true,
            ..Default::default()
        };
        let (text, error) = jpg_summary(&stopped, false);
        assert!(text.contains("Stopped") && error.is_none());
    }
}
