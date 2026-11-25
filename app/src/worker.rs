//! The one thread that talks to devices.
//!
//! It owns the system session (COM on Windows), the open device session, the current scan plan,
//! the list of known devices and the app's state files. The window only sends [`Command`]s and
//! receives [`Event`]s. A sync always uses the worker's own copy of the plan, picked by its ID, so
//! the window can't hand it a stale or altered plan. While the worker is busy scanning or
//! copying it can't read commands, so Cancel, Stop and "skip current" are shared flags
//! ([`Flags`]) instead.
//!
//! A device that is opened is matched against the known ones. If that needs a question or the
//! new-device card, the connection is held back ("pending") with nothing learned, and no scan
//! or sync is possible until the user has answered.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use eframe::egui;
use log::{debug, error, info, warn};
use pnk_core::device::{
    DeviceError, DeviceIdentity, DeviceSource, Guarded, is_connection_lost, plain_error,
};
use pnk_core::heic::made::{FailedEntry, MadeList};
use pnk_core::history::{History, SyncRecord};
use pnk_core::idfile::{self, IdFile};
use pnk_core::jpgs::{self, JpgEvent, JpgReport};
use pnk_core::owner::{self, OwnerState};
use pnk_core::plan::{self, ScanOptions, ScanPlan, ScanProgress, Status, Summary};
use pnk_core::platform::{self, DiskSpace, ListedDevice, Session, Transport};
use pnk_core::registry::{
    DeviceSettings, KnownDevice, MovedBackup, Registry, SyncSummary, new_device_id,
};
use pnk_core::signals::{DeviceKind, SignalKind, Signals};
use pnk_core::space::{self, FolderSizes};
use pnk_core::state::{Scope, StateDir};
use pnk_core::sync::{
    ArchiveEvent, ArchiveReport, CopyMode, SyncControl, SyncEvent, SyncItem, SyncReport,
    archive_removed, archive_stamp, rename_archive, run_sync,
};

use crate::questions::{self, Answer, Decision, Named, Question, Resolution, WeakSession};

pub enum Command {
    ListDevices,
    /// Open a session with this device (closing any other), match it against the known devices
    /// and report what came of it.
    Open(ListedDevice),
    Close,
    /// Let a stick or drive go, the way the system's Eject does. Everything held open on it (the
    /// session, the source, the scan) is dropped first, but only if it is the open device that is
    /// ejected. Answered with [`Event::Ejected`].
    Eject(ListedDevice),
    Scan(PathBuf),
    Sync {
        plan_id: u64,
        items: Vec<SyncItem>,
        /// Also move the plan's files deleted from the device into the archive folder.
        archive: bool,
        /// The user said yes to the reasons the scan gave for asking first
        /// ([`ScanPlan::cautions`]). Without it, archiving that needs an OK does not happen.
        confirmed: bool,
    },
    /// The one-off backfill: make JPG copies of the HEIC photos already in this device's backup
    /// folder (DESIGN.md section 7.6). Uses only the folder. Progress comes as
    /// [`Event::Jpg`], the end as [`Event::JpgDone`]; Stop is the sync's flag.
    MakeJpgs {
        id: String,
    },
    /// "Try again": forget these HEICs' failures (all of them if `None`) and make their JPGs.
    RetryJpgs {
        id: String,
        heics: Option<Vec<String>>,
    },
    /// Record the backup folder as this device's, so its deleted files can be archived. Refused
    /// when the folder's marker records another view of the device.
    ClaimFolder {
        plan_id: u64,
    },
    SkipAdd {
        plan_id: u64,
        entry: usize,
    },
    SkipRemove {
        plan_id: u64,
        path: String,
    },
    SkipClear {
        plan_id: u64,
    },
    /// The window dropped its scan (folder edited, device changed): drop ours too.
    DiscardPlan,
    /// The user's reply to the question about the opened device `device` (the listed ID the
    /// window means; refused if another device is open by now).
    Answer {
        device: String,
        reply: Answer,
    },
    /// "It's one I already back up…": the opened device `device` is this known one.
    LinkExisting {
        device: String,
        id: String,
    },
    /// Set the opened device up as a new known device.
    CreateDevice(Box<NewDevice>),
    /// "Not this device?": forget what was learned when it was recognised, and ask again.
    NotThisDevice {
        device: String,
    },
    /// Write an ID file on the opened stick `device` (the user said Yes). Refused if the open
    /// device is another one by now.
    WriteIdFile {
        device: String,
    },
    /// Replace the ID file on the opened stick `device`, which is a copy of another (the user
    /// said Yes).
    ReplaceIdFile {
        device: String,
    },
    Rename {
        id: String,
        name: String,
    },
    /// Remove a device from the list (its pointer and cache). Its backup folder, with its
    /// history, and the device itself are not touched.
    Forget {
        id: String,
    },
    /// Read a device's sync history; the answer is [`Event::History`].
    LoadHistory {
        id: String,
    },
    SetSettings {
        id: String,
        settings: Box<DeviceSettings>,
    },
    /// Choose another backup folder for a device.
    SetBackupDir {
        id: String,
        dir: PathBuf,
    },
    /// A device's folder moved: point at the new place. Its marker must say it is the device's.
    Relocate {
        id: String,
        dir: PathBuf,
    },
    /// Rename the archive folder on disk, after the scan the window holds.
    RenameArchive {
        plan_id: u64,
        new_name: String,
    },
    /// "Find my backups": read the markers one level under `root`. `apply` adds the devices
    /// found; without it nothing changes.
    FindBackups {
        root: PathBuf,
        apply: bool,
    },
    /// "Add an existing backup…": `dir` is a backup folder (it has the marker), or holds some one
    /// level down. Each device found is added from its folder's own record; one that is known
    /// with another folder gets this one. Answered with [`Event::Notice`].
    AddExisting {
        dir: PathBuf,
    },
}

/// What the new-device card gives back.
pub struct NewDevice {
    /// The listed device the card was filled in for.
    pub device: String,
    pub name: String,
    pub backup_dir: PathBuf,
    pub settings: DeviceSettings,
    /// Use the ID on the stick (written on another PC) as this device's own.
    pub use_id_file_id: bool,
}

/// Where a known device's backup folder stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderState {
    NotSet,
    Present,
    Missing,
}

/// How an opened device came to be a known one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    Recognised(SignalKind),
    Answered,
    Linked,
    Created,
    /// The same connection was opened again within the session.
    Reconnected,
}

/// The new-device card's starting point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setup {
    pub suggested_name: String,
    /// The ID file on the stick holds an ID this app doesn't know: it was written on another PC.
    pub foreign_id: Option<String>,
    /// The ID file on the stick holds the ID of this known device: the stick may be its copy.
    pub id_clash: Option<Named>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    Ready { device_id: String, how: How },
    Ask(Question),
    Setup(Setup),
}

/// An opened device, and what recognition made of it.
pub struct Opened {
    pub identity: DeviceIdentity,
    pub kind: DeviceKind,
    pub stage: Stage,
    /// What the ID file on a stick or drive is like (`Missing` for anything else).
    pub id_file: IdFile,
    /// The device is a stick or drive that an ID file can be written to.
    pub is_volume: bool,
}

/// "Find my backups", for the window.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Found {
    pub apply: bool,
    pub added: Vec<String>,
    pub already_known: Vec<String>,
    pub moved: Vec<MovedBackup>,
    pub problems: Vec<String>,
}

#[allow(clippy::large_enum_variant)] // a few events per device, not per file
pub enum Event {
    /// Startup: the previous run was interrupted mid-copy and was cleaned up.
    Recovered(String),
    Devices(Result<Vec<ListedDevice>, String>),
    /// Something that stops some devices from being listed at all (usbmuxd not running), or
    /// `None` when nothing does. Sent after every listing.
    DeviceHint(Option<String>),
    Opened {
        id: String,
        result: Result<Box<Opened>, String>,
    },
    /// The known devices changed (or were loaded), with where each backup folder stands.
    Registry {
        devices: Vec<KnownDevice>,
        folders: Vec<(String, FolderState)>,
    },
    ScanProgress(ScanProgress),
    ScanDone(Box<ScanDone>),
    /// A scan that didn't happen or didn't finish, on the listed device `listed` (the window
    /// ignores it if another device is open by now).
    ScanFailed {
        listed: String,
        message: String,
    },
    ScanCancelled,
    Sync(SyncEvent),
    Archive(ArchiveEvent),
    SyncDone {
        /// The listed device the sync ran on (see [`Event::ScanFailed`]).
        listed: String,
        result: Result<SyncOutcome, String>,
    },
    /// Progress of the JPG step of a sync, of the backfill, or of "Try again".
    Jpg(JpgEvent),
    /// The backfill or "Try again" ended.
    JpgDone(Result<JpgReport, String>),
    /// The HEICs no JPG could be made from, as the backup folder's list has them now.
    JpgFailed {
        id: String,
        failed: Vec<FailedEntry>,
    },
    /// The open device stopped answering, so the worker let go of its session: the next Scan or
    /// Refresh must open the device again.
    SessionDropped,
    Claimed(Result<(), String>),
    SkipList {
        plan_id: u64,
        result: Result<Vec<String>, String>,
    },
    /// A message for the window: what happened, or why a change was refused.
    Notice(Result<String, String>),
    /// The answer to [`Command::Eject`]: the listed device's ID and name, and whether it can be
    /// unplugged now (or why not, in words for the user).
    Ejected {
        id: String,
        label: String,
        result: Result<(), String>,
    },
    /// The result of writing or replacing an ID file.
    IdFile(Result<String, String>),
    /// The archive folder was renamed (or only its name in the settings changed).
    ArchiveRenamed(Result<String, String>),
    Found(Box<Found>),
    /// How big a device's backup and disks are, worked out after a scan or a sync.
    Sizes {
        id: String,
        sizes: Box<Sizes>,
    },
    /// A device's sync history, newest first.
    History {
        id: String,
        records: Vec<SyncRecord>,
    },
}

/// The sizes the device page shows. Each part is `None` when it could not be found out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sizes {
    /// The backup folder, and the archive inside it.
    pub folder: Option<FolderSizes>,
    /// The disk the backup folder is on.
    pub backup_disk: Option<DiskSpace>,
    /// A stick or drive: its used and total size, from the volume itself.
    pub device_disk: Option<DiskSpace>,
    /// What the device reported as its size, when it gave no free space (a phone).
    pub device_capacity: Option<u64>,
}

pub struct SyncOutcome {
    pub copy: SyncReport,
    /// `None` when archiving wasn't asked for, or the copy was stopped first.
    pub archive: Option<Result<ArchiveReport, String>>,
    /// The JPG step: `None` when the device doesn't make JPG copies, or the sync stopped first.
    pub jpg: Option<Result<JpgReport, String>>,
}

pub struct ScanDone {
    /// The listed device that was scanned.
    pub listed: String,
    pub plan_id: u64,
    pub plan: Arc<ScanPlan>,
    pub summary: Summary,
    /// The skip list for this device and folder, sorted.
    pub skip_list: Vec<String>,
}

/// Set by the window, read by the worker while it is busy. The window clears them before
/// sending the command they belong to, so a click that comes early is never lost.
#[derive(Clone, Default)]
pub struct Flags {
    pub scan_cancel: Arc<AtomicBool>,
    pub sync: SyncControl,
}

/// Device ID of the folder-backed fake device (debug builds with the `fake-device` feature).
#[cfg(all(debug_assertions, feature = "fake-device"))]
const FAKE_PNP_ID: &str = "fake-device";

/// The fake device as the device list shows it, named after its folder.
#[cfg(all(debug_assertions, feature = "fake-device"))]
fn fake_device(folder: &Path) -> ListedDevice {
    let label = folder
        .file_name()
        .map_or_else(|| "iPhone".into(), |n| n.to_string_lossy().into_owned());
    ListedDevice {
        id: FAKE_PNP_ID.into(),
        label,
        kind_hint: Some(pnk_core::signals::DeviceKind::Ptp),
        transport: Transport::Fake,
    }
}

/// The folder-backed fake device. Only debug builds with the `fake-device` feature use it.
#[derive(Clone)]
#[cfg_attr(not(all(debug_assertions, feature = "fake-device")), allow(dead_code))]
pub struct Fake {
    pub folder: PathBuf,
    /// Read at about 3 MB/s, slow enough to try Skip current and Stop.
    pub slow: bool,
    /// Fall one file behind after a transfer is cut short, until the device is opened again.
    pub lag: bool,
    /// Name the device after its folder and give it a serial number.
    pub named: bool,
}

pub fn spawn(
    ctx: egui::Context,
    state: StateDir,
    fake: Option<Fake>,
    flags: Flags,
) -> (Sender<Command>, Receiver<Event>) {
    spawn_with(ctx, state, fake, flags, pnk_core::platform::eject)
}

/// [`spawn`] with the system's eject passed in, so a test never ejects anything real.
fn spawn_with(
    ctx: egui::Context,
    state: StateDir,
    fake: Option<Fake>,
    flags: Flags,
    eject: EjectFn,
) -> (Sender<Command>, Receiver<Event>) {
    let (cmd_tx, cmd_rx) = mpsc::channel();
    let (ev_tx, ev_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("device".into())
        .spawn(move || {
            let out = Out { tx: ev_tx, ctx };
            debug!("The device thread has started");
            let session = match Session::init() {
                Ok(c) => c,
                Err(e) => {
                    error!("The device thread can't start: {e}");
                    out.send(Event::Devices(Err(e)));
                    return;
                }
            };
            let registry = Registry::load(&state);
            let mut w = Worker {
                source: None,
                device: None,
                weak_match: false,
                pending: None,
                bound: None,
                weak: WeakSession::default(),
                info: None,
                plan: None,
                next_plan_id: 1,
                history: History::new(&state),
                state,
                registry,
                fake,
                flags,
                out,
                eject_fn: eject,
                session,
                sizes_generation: Arc::new(AtomicU64::new(0)),
            };
            w.recover();
            w.publish();
            // Ends when the window is gone. Dropping `w` then closes the session before the system session.
            while let Ok(cmd) = cmd_rx.recv() {
                w.handle(cmd);
            }
        })
        .expect("start the device thread");
    (cmd_tx, ev_rx)
}

/// What ejects a device: the system's, or a stand-in in a test.
type EjectFn = fn(&ListedDevice) -> Result<(), String>;

#[derive(Clone)]
struct Out {
    tx: Sender<Event>,
    ctx: egui::Context,
}

/// An opened device waiting for the user (see [`Worker::pending`]).
struct Pending {
    inner: Box<dyn DeviceSource>,
    identity: DeviceIdentity,
    kind: DeviceKind,
    question: Option<Question>,
}

struct OpenInfo {
    listed_id: String,
    identity: DeviceIdentity,
    kind: DeviceKind,
    id_file: IdFile,
    is_volume: bool,
}

struct Bound {
    device_id: String,
    /// The device as it was before this recognition learned anything, if recognition did.
    before: Option<KnownDevice>,
    signals: Signals,
}

impl Out {
    fn send(&self, event: Event) {
        let _ = self.tx.send(event);
        self.ctx.request_repaint();
    }
}

struct Worker {
    // Field order matters: fields drop in this order, and the session must close before the system session.
    source: Option<Box<dyn DeviceSource>>,
    /// The device `source` was opened from (or last tried), for opening it again.
    device: Option<ListedDevice>,
    /// The open device was recognised only by weak signals: archiving asks first.
    weak_match: bool,
    /// An opened device that no known device has been chosen for yet: a question or the
    /// new-device card is waiting for the user.
    pending: Option<Pending>,
    /// The known device the open one is, and what to put back if the user says "not this
    /// device".
    bound: Option<Bound>,
    /// Devices whose identity rests on an "is this X?" answer.
    weak: WeakSession,
    /// What the window was last told about the open device, for telling it again.
    info: Option<OpenInfo>,
    plan: Option<(u64, Arc<ScanPlan>)>,
    next_plan_id: u64,
    state: StateDir,
    /// The sync history, one file per device.
    history: History,
    /// The known devices (`devices.json`).
    registry: Registry,
    #[cfg_attr(not(all(debug_assertions, feature = "fake-device")), allow(dead_code))]
    fake: Option<Fake>,
    flags: Flags,
    out: Out,
    eject_fn: EjectFn,
    session: Session,
    /// Which walk of a backup folder is the newest ([`Worker::refresh_sizes`]); an older one's
    /// answer is dropped, and forgetting a device makes every walk older.
    sizes_generation: Arc<AtomicU64>,
}

impl Worker {
    fn handle(&mut self, cmd: Command) {
        debug!("Command received: {}", describe(&cmd));
        match cmd {
            Command::ListDevices => {
                // A drive plugged in since the last look brings its devices back.
                if self.registry.refresh_offline(&self.state) {
                    self.publish();
                }
                let r = self.list();
                match &r {
                    Ok(list) if list.is_empty() => info!("No devices found"),
                    Ok(list) => info!(
                        "Found {}: {}",
                        if list.len() == 1 {
                            "1 device".to_string()
                        } else {
                            format!("{} devices", list.len())
                        },
                        list.iter()
                            .map(|d| format!("\"{}\"", d.label))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    Err(e) => warn!("Could not list the devices: {e}"),
                }
                self.out.send(Event::Devices(r));
                self.out.send(Event::DeviceHint(self.session.list_hint()));
            }
            Command::Open(device) => {
                if self.registry.refresh_offline(&self.state) {
                    self.publish();
                }
                self.open_with(device, None)
            }
            Command::Close => {
                if self.source.is_some() || self.pending.is_some() {
                    info!("Closed the device session: no device is chosen");
                }
                self.drop_session();
            }
            Command::Eject(device) => self.eject(device),
            Command::Scan(root) => self.scan(root),
            Command::Sync {
                plan_id,
                items,
                archive,
                confirmed,
            } => self.sync(plan_id, items, archive, confirmed),
            Command::MakeJpgs { id } => self.jpg_job(&id, None),
            Command::RetryJpgs { id, heics } => self.jpg_job(&id, Some(heics)),
            Command::ClaimFolder { plan_id } => {
                let r = self.claim_folder(plan_id);
                if let Err(e) = &r {
                    error!("Could not make the backup folder this device's: {e}");
                }
                self.out.send(Event::Claimed(r));
            }
            Command::SkipAdd { plan_id, entry } => {
                self.edit_skip_list(plan_id, |state, scope, plan| {
                    let e = plan
                        .entries
                        .get(entry)
                        .ok_or("That file is not in the scan.")?;
                    state.skip_add(scope, &e.source).map_err(|e| e.to_string())
                })
            }
            Command::SkipRemove { plan_id, path } => self
                .edit_skip_list(plan_id, |state, scope, _| {
                    state.skip_remove(scope, &path).map_err(|e| e.to_string())
                }),
            Command::SkipClear { plan_id } => self.edit_skip_list(plan_id, |state, scope, _| {
                state.skip_clear(scope).map_err(|e| e.to_string())
            }),
            Command::DiscardPlan => self.plan = None,
            Command::Answer { device, reply } => {
                if self.is_open_device(&device) {
                    self.answer(reply)
                }
            }
            Command::LinkExisting { device, id } => {
                if self.is_open_device(&device) {
                    self.link_existing(&id)
                }
            }
            Command::CreateDevice(new) => {
                if self.is_open_device(&new.device) {
                    self.create_device(*new)
                }
            }
            Command::NotThisDevice { device } => {
                if self.is_open_device(&device) {
                    self.not_this_device()
                }
            }
            Command::WriteIdFile { device } => {
                if self.is_open_device(&device) {
                    self.write_id_file()
                }
            }
            Command::ReplaceIdFile { device } => {
                if self.is_open_device(&device) {
                    self.replace_id_file()
                }
            }
            Command::Rename { id, name } => self.rename(&id, &name),
            Command::Forget { id } => self.forget(&id),
            Command::LoadHistory { id } => {
                let records = self.history_load(&id);
                self.out.send(Event::History { id, records });
            }
            Command::SetSettings { id, settings } => self.set_settings(&id, *settings),
            Command::SetBackupDir { id, dir } => self.set_backup_dir(&id, dir),
            Command::Relocate { id, dir } => self.relocate(&id, &dir),
            Command::RenameArchive { plan_id, new_name } => {
                let r = self.rename_the_archive(plan_id, &new_name);
                self.out.send(Event::ArchiveRenamed(r));
                self.publish();
            }
            Command::FindBackups { root, apply } => self.find_backups(&root, apply),
            Command::AddExisting { dir } => self.add_existing(&dir),
        }
    }

    /// Whether the open device is the listed one `expected` (the one the window's question or
    /// dialog was about). If not, says so and changes nothing: a second device may have been
    /// opened since the window asked.
    fn is_open_device(&self, expected: &str) -> bool {
        if self.device.as_ref().is_some_and(|d| d.id == expected) {
            return true;
        }
        warn!("Refused: the open device is not {expected}");
        self.notice(Err(
            "That device is no longer the open one, so nothing was changed.".into(),
        ));
        false
    }

    /// Lets go of everything held on the device, then asks the system to eject it. The session
    /// is dropped even when the eject is then refused: the window opens the device again.
    fn eject(&mut self, device: ListedDevice) {
        // Only what is held on this very device is let go. Another stick being ejected leaves
        // the open phone, its scan and its question alone.
        if self.device.as_ref().is_some_and(|d| d.id == device.id) {
            self.plan = None;
            self.source = None;
            self.pending = None;
            self.bound = None;
            self.info = None;
            self.device = None;
            self.weak_match = false;
        }
        info!("Ejecting the device {}", device.id);
        let result = if pnk_core::platform::can_eject(&device) {
            (self.eject_fn)(&device)
        } else {
            Err("This kind of device can't be ejected from here".to_string())
        };
        match &result {
            Ok(()) => info!("Ejected \"{}\": it can be unplugged", device.label),
            Err(e) => warn!("Could not eject \"{}\": {e}", device.label),
        }
        self.out.send(Event::Ejected {
            id: device.id,
            label: device.label,
            result,
        });
    }

    fn recover(&self) {
        // The record is only acted on inside a backup folder the registry holds for its device.
        let known = |device: &str| -> Vec<PathBuf> {
            self.registry
                .get(device)
                .and_then(|d| d.backup_dir.clone())
                .into_iter()
                .collect()
        };
        match self.state.recover_interrupted(&known) {
            Ok(Some(i)) => {
                let msg = match (&i.record, i.temp_removed) {
                    (Some(r), _) if i.completed => format!(
                        "The last copy was interrupted ({}). The change was finished: {}.",
                        r.source,
                        sentence(&i.note)
                    ),
                    (Some(r), true) => format!(
                        "The last copy was interrupted ({}). Its unfinished file was removed; the next sync copies it again.",
                        r.source
                    ),
                    (Some(r), false) => format!(
                        "The last copy was interrupted ({}). {}.",
                        r.source,
                        sentence(&i.note)
                    ),
                    (None, _) => format!("{}.", sentence(&i.note)),
                };
                self.out.send(Event::Recovered(msg));
            }
            Ok(None) => {}
            Err(e) => {
                warn!("Could not check whether the last copy was interrupted: {e}");
                self.out.send(Event::Recovered(format!(
                    "Could not check whether the last copy was interrupted: {e}"
                )))
            }
        }
    }

    fn list(&self) -> Result<Vec<ListedDevice>, String> {
        #[cfg(all(debug_assertions, feature = "fake-device"))]
        if let Some(fake) = &self.fake {
            return Ok(vec![fake_device(&fake.folder)]);
        }
        self.session.list_devices()
    }

    /// The known devices and where their backup folders stand, sent to the window.
    fn publish(&self) {
        let folders = self
            .registry
            .devices()
            .iter()
            .map(|d| {
                let state = match &d.backup_dir {
                    None => FolderState::NotSet,
                    Some(_) if d.offline.is_some() => FolderState::Missing,
                    Some(p) if p.is_dir() => FolderState::Present,
                    Some(_) => FolderState::Missing,
                };
                (d.id.clone(), state)
            })
            .collect();
        self.out.send(Event::Registry {
            devices: self.registry.devices().to_vec(),
            folders,
        });
    }

    /// Save the list of known devices; say so if it can't be.
    fn save_registry(&self) -> bool {
        match self.registry.save(&self.state) {
            Ok(()) => true,
            Err(e) => {
                warn!("Could not save the list of known devices: {e}");
                self.out.send(Event::Notice(Err(format!(
                    "Could not save the list of known devices: {e}"
                ))));
                false
            }
        }
    }

    /// Why the device's backup folder can't be used for now, if it can't: the device is listed,
    /// but nothing is scanned, synced or changed until the folder is found.
    fn offline_reason(&self, id: &str) -> Option<String> {
        self.registry.get(id).and_then(|d| d.offline.clone())
    }

    /// The device's sync history: from its backup folder, or, if it has no record there yet, from
    /// the settings folder.
    fn history_load(&self, id: &str) -> Vec<SyncRecord> {
        match self.registry.history_file(id) {
            Some(path) => pnk_core::history::load_at(&path),
            None => self.history.load(id),
        }
    }

    fn history_append(&self, id: &str, record: &SyncRecord) -> std::io::Result<()> {
        match self.registry.history_file(id) {
            Some(path) => pnk_core::history::append_at(&path, record),
            None => self.history.append(id, record),
        }
    }

    fn notice(&self, r: Result<String, String>) {
        self.out.send(Event::Notice(r));
    }

    /// Open `device` and match it. `rebind`: the same connection reopened after a stopped
    /// transfer, so it is the known device it was a moment ago, without asking again.
    fn open_with(&mut self, device: ListedDevice, rebind: Option<String>) {
        self.plan = None;
        self.source = None; // close any other session first
        self.pending = None;
        self.bound = None;
        self.info = None;
        self.weak_match = false;
        self.device = Some(device.clone());
        info!("Opening the device {}", device.id);
        let result = self
            .open_unguarded(&device)
            .map(|inner| self.recognise_opened(inner, &device, rebind));
        match &result {
            Ok(o) => info!("The device is open: {}", o.identity.label()),
            // The message already says what failed ("Could not open the device: …").
            Err(e) => warn!("{e}"),
        }
        // The known devices first, so the window can find the one it is told about.
        self.publish();
        self.out.send(Event::Opened {
            id: device.id,
            result: result.map(Box::new),
        });
    }

    /// Match an opened device against the known ones. Nothing is learned or rejected unless
    /// recognition is certain (one strong match and no clash); a question or a new device
    /// leaves the connection pending.
    fn recognise_opened(
        &mut self,
        inner: Box<dyn DeviceSource>,
        device: &ListedDevice,
        rebind: Option<String>,
    ) -> Opened {
        let identity = inner.identity().clone();
        // The opened device's own word first, then what the listing said, then a guess.
        let kind = identity
            .kind_hint
            .or(device.kind_hint)
            .unwrap_or_else(|| DeviceKind::guess_from(&identity.signals));
        let is_volume = device.transport == Transport::Volume;
        let id_file = if is_volume {
            idfile::read(Path::new(&device.id))
        } else {
            IdFile::Missing
        };
        let stage = match rebind.filter(|id| self.registry.get(id).is_some()) {
            Some(id) => {
                info!("Opened the same connection again: still the same known device");
                self.bind(inner, &id, None, identity.signals.clone());
                Stage::Ready {
                    device_id: id,
                    how: How::Reconnected,
                }
            }
            None => match questions::decide(self.registry.devices(), &identity.signals, kind) {
                Decision::Known { id, by } => {
                    self.recognised(inner, &identity, &id, by);
                    Stage::Ready {
                        device_id: id,
                        how: How::Recognised(by),
                    }
                }
                Decision::Ask(q) => {
                    info!("Asking the user which device this is: {q:?}");
                    self.pending = Some(Pending {
                        inner,
                        identity: identity.clone(),
                        kind,
                        question: Some(q.clone()),
                    });
                    Stage::Ask(q)
                }
                Decision::New => {
                    info!("Nothing known fits this device: it needs to be set up");
                    let setup = self.setup_for(&identity.signals, kind);
                    self.pending = Some(Pending {
                        inner,
                        identity: identity.clone(),
                        kind,
                        question: None,
                    });
                    Stage::Setup(setup)
                }
            },
        };
        self.info = Some(OpenInfo {
            listed_id: device.id.clone(),
            identity: identity.clone(),
            kind,
            id_file: id_file.clone(),
            is_volume,
        });
        Opened {
            identity,
            kind,
            stage,
            id_file,
            is_volume,
        }
    }

    /// Every connection is `Guarded`: once a transfer over it goes wrong, it reads no more file
    /// data, whatever the caller does next. The connection also gets the device's app-made ID.
    fn bind(
        &mut self,
        inner: Box<dyn DeviceSource>,
        id: &str,
        before: Option<KnownDevice>,
        signals: Signals,
    ) {
        self.weak_match = self.weak.is_weak(id);
        self.source = Some(Box::new(Guarded::new(inner).with_device_id(id)));
        self.bound = Some(Bound {
            device_id: id.to_string(),
            before,
            signals,
        });
    }

    /// Recognised outright: remember the signal values it has now (a new name, say).
    fn recognised(
        &mut self,
        inner: Box<dyn DeviceSource>,
        identity: &DeviceIdentity,
        id: &str,
        by: SignalKind,
    ) {
        let now = pnk_core::utc_stamp();
        let before = self.registry.get(id).cloned();
        if let Some(d) = self.registry.get_mut(id) {
            d.view = Some(identity.view);
            let gained = d.learn(&identity.signals, &now);
            info!(
                "Recognised \"{}\" by {}{}",
                d.name,
                by.describe(),
                if gained.is_empty() {
                    String::new()
                } else {
                    format!("; it now also knows {}", gained_text(&gained))
                }
            );
        }
        self.save_registry();
        self.bind(inner, id, before, identity.signals.clone());
    }

    fn setup_for(&self, signals: &Signals, kind: DeviceKind) -> Setup {
        let id_file_id = signals.id_file_id.clone();
        let id_clash = id_file_id.as_deref().and_then(|f| {
            let norm = f.trim().to_lowercase();
            self.registry
                .devices()
                .iter()
                .find(|d| d.history.contains(SignalKind::IdFile, &norm))
                .map(|d| Named {
                    id: d.id.clone(),
                    name: d.name.clone(),
                })
        });
        Setup {
            suggested_name: questions::suggested_name(signals, kind),
            foreign_id: id_file_id.filter(|_| id_clash.is_none()),
            id_clash,
        }
    }

    /// Tell the window what the open device is now.
    fn emit(&self, stage: Stage) {
        let Some(info) = &self.info else { return };
        self.publish();
        self.out.send(Event::Opened {
            id: info.listed_id.clone(),
            result: Ok(Box::new(Opened {
                identity: info.identity.clone(),
                kind: info.kind,
                stage,
                id_file: info.id_file.clone(),
                is_volume: info.is_volume,
            })),
        });
    }

    /// An answer or a choice settles the pending device.
    fn resolved(&mut self, p: Pending, r: Resolution, before: Vec<KnownDevice>, how: How) {
        self.weak.note(&r);
        let stage = match r {
            Resolution::Bound { id, .. } => {
                let was = before.into_iter().find(|d| d.id == id);
                if let Some(d) = self.registry.get_mut(&id) {
                    d.view = Some(p.identity.view);
                    info!("The user said this is \"{}\"", d.name);
                }
                self.save_registry();
                let signals = p.identity.signals.clone();
                self.bind(p.inner, &id, was, signals);
                Stage::Ready { device_id: id, how }
            }
            Resolution::Ask(q) => {
                self.save_registry();
                let stage = Stage::Ask(q.clone());
                self.pending = Some(Pending {
                    question: Some(q),
                    ..p
                });
                stage
            }
            Resolution::New => {
                self.save_registry();
                let stage = Stage::Setup(self.setup_for(&p.identity.signals, p.kind));
                self.pending = Some(Pending {
                    question: None,
                    ..p
                });
                stage
            }
        };
        self.emit(stage);
    }

    fn answer(&mut self, reply: Answer) {
        let Some(p) = self.pending.take() else {
            return self.notice(Err("No question is waiting.".into()));
        };
        let Some(q) = p.question.clone() else {
            self.pending = Some(p);
            return self.notice(Err("No question is waiting.".into()));
        };
        let before = self.registry.devices().to_vec();
        match questions::answer(
            &mut self.registry,
            &q,
            &reply,
            &p.identity.signals,
            p.kind,
            &pnk_core::utc_stamp(),
        ) {
            Ok(r) => self.resolved(p, r, before, How::Answered),
            Err(e) => {
                self.pending = Some(p);
                self.notice(Err(e));
            }
        }
    }

    fn link_existing(&mut self, id: &str) {
        let Some(p) = self.pending.take() else {
            return self.notice(Err("No device is waiting to be set up.".into()));
        };
        let before = self.registry.devices().to_vec();
        match questions::link_existing(
            &mut self.registry,
            id,
            &p.identity.signals,
            &pnk_core::utc_stamp(),
        ) {
            Ok(r) => self.resolved(p, r, before, How::Linked),
            Err(e) => {
                self.pending = Some(p);
                self.notice(Err(e));
            }
        }
    }

    fn create_device(&mut self, new: NewDevice) {
        let Some(p) = self.pending.take() else {
            return self.notice(Err("No device is waiting to be set up.".into()));
        };
        let name = new.name.trim().to_string();
        let problem = if name.is_empty() {
            Some("Give the device a name.".to_string())
        } else if !new.backup_dir.is_absolute() {
            Some(format!(
                "Enter the backup folder's full path, such as {}.",
                crate::format::example_folder()
            ))
        } else {
            crate::validate::check_settings(&new.settings, &[])
                .into_iter()
                .next()
                .map(|(_, e)| e)
        };
        if let Some(e) = problem {
            self.pending = Some(p);
            return self.notice(Err(e));
        }
        let mut signals = p.identity.signals.clone();
        let stick_id = signals.id_file_id.clone().filter(|f| is_sane_id(f));
        let taken = |reg: &Registry, f: &str| {
            reg.get(f).is_some()
                || reg.devices().iter().any(|d| {
                    d.history
                        .contains(SignalKind::IdFile, &f.trim().to_lowercase())
                })
        };
        let adopt =
            new.use_id_file_id && stick_id.as_ref().is_some_and(|f| !taken(&self.registry, f));
        let id = match (&stick_id, adopt) {
            (Some(f), true) => f.clone(),
            _ => new_device_id(),
        };
        // Another known device's ID file ID is not this device's signal.
        if !adopt && stick_id.as_ref().is_some_and(|f| taken(&self.registry, f)) {
            signals.id_file_id = None;
        }
        let now = pnk_core::utc_stamp();
        let mut d = KnownDevice::with_id(id.clone(), p.kind, &name, &signals, &now);
        d.view = Some(p.identity.view);
        d.backup_dir = Some(new.backup_dir);
        d.settings = new.settings;
        info!(
            "Set up \"{name}\" as a new {} backing up to {}",
            p.kind.label(),
            d.backup_dir
                .as_ref()
                .map_or_else(String::new, |b| b.display().to_string())
        );
        self.registry.add(d);
        self.save_registry();
        self.bind(p.inner, &id, None, signals);
        self.emit(Stage::Ready {
            device_id: id,
            how: How::Created,
        });
    }

    fn not_this_device(&mut self) {
        let has_undo = self.bound.as_ref().is_some_and(|b| b.before.is_some());
        let Some(device) = self.device.clone().filter(|_| has_undo) else {
            return self.notice(Err(
                "Nothing to take back: this device was just set up.".into()
            ));
        };
        let Some(b) = self.bound.take() else { return };
        if let (Some(before), Some(d)) = (b.before, self.registry.get_mut(&b.device_id)) {
            // What was learned when it was taken for this device is taken back.
            d.history = before.history;
            d.last_seen = before.last_seen;
            d.view = before.view;
            d.reject(&b.signals);
            info!("The user said this is not \"{}\"", d.name);
        }
        self.weak.forget(&b.device_id);
        self.save_registry();
        self.open_with(device, None);
    }

    /// The folder an ID file goes in: only a stick or drive (never a phone), and only if the
    /// device has been identified.
    fn id_file_target(&self) -> Result<PathBuf, String> {
        let device = self.device.as_ref().ok_or("No device is open.")?;
        if device.transport != Transport::Volume {
            return Err(
                "An ID file can only be written to a USB stick, SD card or drive, never to a phone."
                    .into(),
            );
        }
        Ok(PathBuf::from(&device.id))
    }

    fn write_id_file(&mut self) {
        let result = (|| -> Result<(PathBuf, String), String> {
            let root = self.id_file_target()?;
            let id = self
                .bound
                .as_ref()
                .map(|b| b.device_id.clone())
                .ok_or("Say which device this is first.")?;
            match idfile::read(&root) {
                IdFile::Missing => Ok((root, id)),
                IdFile::Found(_) => Err("This stick already has an ID file.".into()),
                IdFile::Damaged(why) => Err(format!(
                    "{} on the stick isn't a usable ID file ({why}). It is left alone.",
                    idfile::FILE_NAME
                )),
            }
        })()
        .and_then(|(root, id)| {
            // The only write to a device, and only after the user's Yes.
            idfile::write(&root, &id).map_err(|e| {
                format!(
                    "Could not write the ID file: {e}. Nothing was left on the stick; it is recognised by its other signals."
                )
            })?;
            Ok(id)
        });
        let result = result.map(|id| {
            if let Some(d) = self.registry.get_mut(&id) {
                d.history.learn(&Signals {
                    id_file_id: Some(id.clone()),
                    ..Signals::default()
                });
            }
            self.save_registry();
            format!("Wrote the ID file {} on the stick.", idfile::FILE_NAME)
        });
        self.out.send(Event::IdFile(result));
        self.publish();
    }

    /// The stick carries another device's ID (it is a copy): give this copy an ID of its own.
    fn replace_id_file(&mut self) {
        let result = (|| -> Result<String, String> {
            let root = self.id_file_target()?;
            let p = self
                .pending
                .as_mut()
                .ok_or("No device is waiting to be set up.")?;
            if p.identity.signals.id_file_id.is_none() {
                return Err("This stick has no ID file to replace.".into());
            }
            let new = new_device_id();
            idfile::replace(&root, &new).map_err(|e| {
                format!("Could not write the new ID: {e}. The old ID file is as it was.")
            })?;
            p.identity.signals.id_file_id = Some(new.clone());
            Ok(new)
        })();
        match result {
            Ok(new) => {
                if let Some(info) = &mut self.info {
                    info.identity.signals.id_file_id = Some(new);
                    info.id_file = IdFile::Found(String::new());
                }
                self.out
                    .send(Event::IdFile(Ok("Wrote a new ID on this stick.".into())));
                if let Some(p) = &self.pending {
                    let setup = self.setup_for(&p.identity.signals, p.kind);
                    self.emit(Stage::Setup(setup));
                }
            }
            Err(e) => self.out.send(Event::IdFile(Err(e))),
        }
    }

    fn rename(&mut self, id: &str, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return self.notice(Err("Give the device a name.".into()));
        }
        if let Some(why) = self.offline_reason(id) {
            return self.notice(Err(format!("Not renamed: {why}")));
        }
        if let Some(d) = self.registry.get_mut(id) {
            info!("Renamed \"{}\" to \"{name}\"", d.name);
            d.name = name.to_string();
            self.save_registry();
            self.publish();
        }
    }

    fn forget(&mut self, id: &str) {
        let Some(d) = self.registry.forget(id) else {
            return;
        };
        info!(
            "Forgot \"{}\"; its backup folder and the device were not touched",
            d.name
        );
        self.weak.forget(id);
        // A size walk still running for it has nothing to report to.
        self.sizes_generation.fetch_add(1, AtomicOrdering::SeqCst);
        self.save_registry();
        self.notice(Ok(format!(
            "Forgot \"{}\". Its backup folder and the device itself were not touched.",
            d.name
        )));
        if self.bound.as_ref().is_some_and(|b| b.device_id == id)
            && let Some(device) = self.device.clone()
        {
            // It is a new device now: open it again to be asked about it.
            self.open_with(device, None);
        } else {
            self.publish();
        }
    }

    fn set_settings(&mut self, id: &str, mut settings: DeviceSettings) {
        if let Some((what, why)) = crate::validate::check_settings(&settings, &[])
            .into_iter()
            .next()
        {
            return self.notice(Err(format!("Not saved ({what}): {why}")));
        }
        if let Some(why) = self.offline_reason(id) {
            return self.notice(Err(format!("Not saved: {why}")));
        }
        let Some(d) = self.registry.get_mut(id) else {
            return;
        };
        // Chosen by the first scan, not by the user.
        settings.storage_layout = d.settings.storage_layout;
        settings.top_level_storage = d.settings.top_level_storage.clone();
        settings.storage_folders = d.settings.storage_folders.clone();
        if d.settings == settings {
            return;
        }
        info!("Saved the settings of \"{}\"", d.name);
        d.settings = settings;
        if self.plan.as_ref().is_some_and(|(_, p)| p.device_key == id) {
            // The scan was made with the old settings.
            self.plan = None;
        }
        self.save_registry();
        self.publish();
    }

    fn set_backup_dir(&mut self, id: &str, dir: PathBuf) {
        if !dir.is_absolute() {
            return self.notice(Err(format!(
                "Enter the backup folder's full path, such as {}.",
                crate::format::example_folder()
            )));
        }
        if let Some(d) = self.registry.get(id) {
            info!(
                "The backup folder of \"{}\" is now {}",
                d.name,
                dir.display()
            );
            // The record goes into the new folder, and the history is copied across; the old
            // folder's files stay where they are.
            if let Err(e) = self.registry.set_backup_dir(&self.state, id, dir) {
                warn!("{e}");
                self.notice(Err(e));
            }
            if self.plan.as_ref().is_some_and(|(_, p)| p.device_key == id) {
                self.plan = None;
            }
            self.save_registry();
            self.publish();
        }
    }

    /// Whether the folder `dir` may become the backup folder of the device `id`, whose folder
    /// moved: its marker must say it is this device's.
    fn check_relocation(&self, id: &str, dir: &Path) -> Result<(), String> {
        let name = self
            .registry
            .get(id)
            .map_or_else(String::new, |d| d.name.clone());
        match owner::read(dir) {
            OwnerState::Owned(m) if m.device_id == id => Ok(()),
            OwnerState::Owned(m) => Err(format!(
                "That folder belongs to \"{}\", not to \"{name}\".",
                m.device_name
            )),
            OwnerState::Unclaimed => Err(format!(
                "That folder has no {} file, so it can't be confirmed as \"{name}\"'s backup. Pick the folder that was moved, with its files.",
                owner::MARKER
            )),
            OwnerState::Unreadable(e) => {
                Err(format!("The marker in that folder can't be read ({e})."))
            }
        }
    }

    /// The folder moved: its marker must say it is this device's. The record in the folder, if
    /// it has one, is the device's from now on (it is the source of truth). Returns the device's
    /// name if it was moved.
    fn relocate_checked(&mut self, id: &str, dir: &Path) -> Result<String, String> {
        self.check_relocation(id, dir)?;
        let name = self
            .registry
            .get(id)
            .map_or_else(String::new, |d| d.name.clone());
        if let Some(problem) = self.registry.rehome(id, dir) {
            warn!("{problem}");
        }
        // A scan held for this device was made against the old folder: drop it (the
        // window drops its copy when it sees the new folder), as a folder change does.
        if self.plan.as_ref().is_some_and(|(_, p)| p.device_key == id) {
            self.plan = None;
        }
        info!("The backup of \"{name}\" is now found at {}", dir.display());
        self.save_registry();
        Ok(name)
    }

    fn relocate(&mut self, id: &str, dir: &Path) {
        if self.registry.get(id).is_none() {
            return;
        }
        match self.relocate_checked(id, dir) {
            Ok(name) => {
                let name = self.registry.get(id).map_or(name, |d| d.name.clone());
                self.notice(Ok(format!(
                    "Found the backup of \"{name}\": its marker confirms the folder."
                )));
                self.publish();
            }
            Err(e) => self.notice(Err(e)),
        }
    }

    /// "Add an existing backup…": see [`Command::AddExisting`].
    fn add_existing(&mut self, dir: &Path) {
        self.registry.refresh_offline(&self.state);
        let now = pnk_core::utc_stamp();
        let report = self.registry.find_my_backups(dir, &now);
        let mut relocated = Vec::new();
        let mut problems = report.problems.clone();
        for m in &report.moved {
            match self.relocate_checked(&m.device_id, &m.found) {
                Ok(_) => relocated.push(
                    self.registry
                        .get(&m.device_id)
                        .map_or_else(|| m.name.clone(), |d| d.name.clone()),
                ),
                Err(e) => problems.push(e),
            }
        }
        self.save_registry();
        let n = report.added.len() + relocated.len() + report.already_known.len();
        let mut parts = Vec::new();
        if !report.added.is_empty() {
            parts.push(format!(
                "Added {} backup{}: {}.",
                report.added.len(),
                if report.added.len() == 1 { "" } else { "s" },
                report.added.join(", ")
            ));
        }
        if !relocated.is_empty() {
            parts.push(format!(
                "Now using this folder for {}.",
                relocated.join(", ")
            ));
        }
        if !report.already_known.is_empty() {
            parts.push(format!(
                "Already known: {}.",
                report.already_known.join(", ")
            ));
        }
        parts.extend(problems.iter().map(|p| format!("{p}.")));
        info!(
            "Added existing backups from {}: {}",
            dir.display(),
            parts.join(" ")
        );
        self.notice(if n == 0 && problems.is_empty() {
            Err(format!(
                "No backup found in {}. A backup folder has a {} file; this folder doesn't, and neither do the folders directly inside it.",
                dir.display(),
                owner::MARKER
            ))
        } else if n == 0 {
            Err(parts.join(" "))
        } else {
            Ok(parts.join(" "))
        });
        self.publish();
    }

    fn rename_the_archive(&mut self, plan_id: u64, new_name: &str) -> Result<String, String> {
        let plan = self.current_plan(plan_id)?;
        let old = plan.archive_name.clone();
        let done = rename_archive(&plan, new_name).map_err(|e| e.to_string())?;
        if let Some(d) = self.registry.get_mut(&plan.device_key) {
            d.settings.archive_name = new_name.to_string();
        }
        self.plan = None;
        self.save_registry();
        Ok(if done.moved {
            format!(
                "Renamed the archive folder \"{old}\" to \"{new_name}\". Scan again to carry on."
            )
        } else {
            format!("The archive is now named \"{new_name}\". Scan again to carry on.")
        })
    }

    fn find_backups(&mut self, root: &Path, apply: bool) {
        self.registry.refresh_offline(&self.state);
        let now = pnk_core::utc_stamp();
        let report = if apply {
            self.registry.find_my_backups(root, &now)
        } else {
            // Only a look: the real list is not touched.
            Registry::new(self.registry.devices().to_vec()).find_my_backups(root, &now)
        };
        if apply {
            self.save_registry();
        }
        self.out.send(Event::Found(Box::new(Found {
            apply,
            added: report.added,
            already_known: report.already_known,
            moved: report.moved,
            problems: report.problems,
        })));
        if apply {
            self.publish();
        }
    }

    fn open_unguarded(&self, device: &ListedDevice) -> Result<Box<dyn DeviceSource>, String> {
        #[cfg(all(debug_assertions, feature = "fake-device"))]
        if device.transport == Transport::Fake
            && let Some(fake) = &self.fake
        {
            use pnk_core::fake::{FolderSource, Lagging};
            let src = if fake.named {
                FolderSource::named(&fake.folder)
            } else {
                FolderSource::new(&fake.folder)
            }
            .map_err(|e| format!("Could not open the fake device folder: {e}"))?;
            // Each open is a new connection, so a new `Lagging` starts in step.
            return Ok(match (fake.slow, fake.lag) {
                (false, false) => Box::new(src),
                (true, false) => Box::new(slow::Slow(src)),
                (false, true) => Box::new(Lagging::new(src)),
                (true, true) => Box::new(Lagging::new(slow::Slow(src))),
            });
        }
        self.session.open(device)
    }

    /// The listed ID of the open device, for the events that say which device they are about.
    fn listed_id(&self) -> String {
        self.device
            .as_ref()
            .map(|d| d.id.clone())
            .unwrap_or_default()
    }

    fn scan(&mut self, root: PathBuf) {
        self.plan = None;
        let listed = self.listed_id();
        let failed = |message: String| Event::ScanFailed {
            listed: listed.clone(),
            message,
        };
        let Some(src) = self.source.as_mut() else {
            warn!("Scan refused: no device is open");
            return self
                .out
                .send(failed("No device is open. Click Refresh.".into()));
        };
        if !root.is_absolute() {
            warn!("Scan refused: {} is not a full path", root.display());
            return self.out.send(failed(format!(
                "Enter the backup folder's full path, such as {}.",
                crate::format::example_folder()
            )));
        }
        let scope_key = src.identity().key.clone();
        if let Some(why) = self
            .registry
            .get(&scope_key)
            .and_then(|d| d.offline.clone())
        {
            warn!("Scan refused: {why}");
            return self.out.send(failed(format!(
                "{why}. Plug the drive in, or use Find it… to point to the folder."
            )));
        }
        let scope = Scope::new(&scope_key, &root);
        let skip = match self.state.skip_list(&scope) {
            Ok(s) => s,
            Err(e) => {
                error!("Scan refused: could not read the skip list: {e}");
                return self
                    .out
                    .send(failed(format!("Could not read the skip list: {e}")));
            }
        };
        // The device's own settings, from its entry in the registry.
        let opts = ScanOptions {
            settings: self
                .registry
                .get(&scope_key)
                .map(|d| d.settings.clone())
                .unwrap_or_default(),
            weak_match: self.weak_match,
        };
        let out = &self.out;
        let mut last: Option<Instant> = None;
        let mut progress = |p: ScanProgress| {
            let due = last.is_none_or(|t| t.elapsed() >= Duration::from_millis(100));
            if due || matches!(p, ScanProgress::Comparing { .. }) {
                last = Some(Instant::now());
                out.send(Event::ScanProgress(p));
            }
        };
        match plan::scan(
            src.as_mut(),
            &root,
            &skip,
            &opts,
            &self.flags.scan_cancel,
            &mut progress,
        ) {
            // A phone that stopped answering partway shows as folders that could not be
            // listed, not as a failed scan. What was read can't be trusted, and the session is
            // dead: let go of it, so the next Scan reopens the device.
            Ok((plan, _)) if plan.issues.iter().any(|i| is_connection_lost(&i.problem)) => {
                let why = plan
                    .issues
                    .iter()
                    .map(|i| i.problem.as_str())
                    .find(|p| is_connection_lost(p))
                    .map(|p| p.rsplit_once(": ").map_or(p, |(_, rest)| rest).to_string())
                    .map(|p| plain_error(&p))
                    .unwrap_or_default();
                error!("Scan failed: the device stopped answering: {why}");
                warn!("Closing the session with the device: it stopped answering");
                self.drop_session();
                self.out.send(failed(why));
                self.out.send(Event::SessionDropped);
            }
            Ok((plan, summary)) => {
                self.store_storage_choice(&plan);
                let plan_id = self.next_plan_id;
                self.next_plan_id += 1;
                let plan = Arc::new(plan);
                self.plan = Some((plan_id, plan.clone()));
                let mut skip_list: Vec<String> = skip.into_iter().collect();
                skip_list.sort();
                info!(
                    "Scan {plan_id} is ready: {} to copy",
                    summary.new + summary.mismatch
                );
                self.refresh_sizes(&plan);
                self.out.send(Event::ScanDone(Box::new(ScanDone {
                    listed,
                    plan_id,
                    plan,
                    summary,
                    skip_list,
                })));
            }
            Err(DeviceError::Cancelled) => {
                info!("Scan cancelled");
                self.out.send(Event::ScanCancelled)
            }
            Err(DeviceError::Failed(m)) => {
                error!("Scan failed: {m}");
                let lost = is_connection_lost(&m);
                self.out.send(failed(plain_error(&m)));
                if lost {
                    warn!("Closing the session with the device: it stopped answering");
                    self.drop_session();
                    self.out.send(Event::SessionDropped);
                }
            }
        }
    }

    /// Lets go of the open device and everything held on it, as Close does.
    fn drop_session(&mut self) {
        self.plan = None;
        self.source = None;
        self.device = None;
        self.pending = None;
        self.bound = None;
        self.info = None;
    }

    fn sync(&mut self, plan_id: u64, items: Vec<SyncItem>, archive: bool, confirmed: bool) {
        let listed = self.listed_id();
        let began = (
            Instant::now(),
            pnk_core::utc_stamp(),
            platform::local_stamp(),
        );
        let result = (|| -> Result<SyncOutcome, String> {
            let plan = self.current_plan(plan_id)?;
            if let Some(why) = self.offline_reason(&plan.device_key) {
                return Err(why);
            }
            let src = self
                .source
                .as_mut()
                .ok_or("No device is open. Click Refresh.")?;
            for item in &items {
                let e = plan
                    .entries
                    .get(item.entry)
                    .ok_or("A file to copy is not in the scan.")?;
                // Replace only ever overwrites a backup that matched the device at scan time.
                if item.mode == CopyMode::Replace && e.status != Status::Verified {
                    return Err(
                        "Only a file that was already backed up when you scanned can be copied over its backup."
                            .into(),
                    );
                }
            }
            let out = &self.out;
            // One dated archive folder for the whole sync: changed files' old copies and the
            // files deleted from the device go into the same one.
            let stamp = archive_stamp();
            let copy = run_sync(
                src.as_mut(),
                &plan,
                &items,
                &self.state,
                &self.flags.sync,
                &stamp,
                confirmed,
                &mut |ev| out.send(Event::Sync(ev)),
            )
            .map_err(|e| e.to_string())?;
            let finished = !copy.cancelled
                && !copy.must_reconnect
                && !copy.disk_full
                && copy.connection_lost.is_none();
            let archive = (archive && finished && !plan.removed.is_empty()).then(|| {
                archive_removed(&plan, &stamp, confirmed, &mut |ev| {
                    out.send(Event::Archive(ev))
                })
                .map_err(|e| e.to_string())
            });
            // The JPG step, last: after the copying and the archiving, never failing the sync.
            let jpg = (plan.jpg.enabled && finished).then(|| {
                jpgs::make_jpegs(&plan, &self.flags.sync.cancel, &mut |ev| {
                    out.send(Event::Jpg(ev))
                })
                .map_err(|e| e.to_string())
            });
            Ok(SyncOutcome { copy, archive, jpg })
        })();
        if let Ok(plan) = self.current_plan(plan_id)
            && plan.jpg.enabled
        {
            self.send_jpg_failures(&plan.device_key, &plan.root);
        }
        if let Ok(o) = &result {
            let archived = match &o.archive {
                Some(Ok(a)) => a.moved,
                _ => 0,
            };
            self.record_sync(plan_id, &o.copy, archived);
            self.log_sync(plan_id, o, &began);
        }
        match &result {
            Ok(o) => {
                let c = &o.copy;
                let archived = match &o.archive {
                    Some(Ok(a)) => format!(", {} archived, {} not archived", a.moved, a.failed),
                    Some(Err(e)) => {
                        error!("Archiving refused: {e}");
                        ", archiving refused".to_string()
                    }
                    None => String::new(),
                };
                let jpgs_text = match &o.jpg {
                    Some(Ok(j)) => format!(", {} JPG copies made, {} not made", j.made, j.failed),
                    Some(Err(e)) => {
                        error!("JPG copies were not made: {e}");
                        ", JPG copies not made".to_string()
                    }
                    None => String::new(),
                };
                let archived = format!("{archived}{jpgs_text}");
                info!(
                    "Sync done: {} copied, {} already backed up, {} skipped, {} failed{archived}{}",
                    c.copied,
                    c.already_backed_up,
                    c.skipped,
                    c.failed,
                    if c.cancelled { " (stopped)" } else { "" }
                );
            }
            Err(e) => error!("Sync not started: {e}"),
        }
        let reconnect = matches!(&result, Ok(o) if o.copy.must_reconnect);
        let lost = matches!(&result, Ok(o) if o.copy.connection_lost.is_some());
        self.out.send(Event::SyncDone { listed, result });
        if lost && !reconnect {
            // The phone stopped answering partway: this session is dead. Let go of it, so the
            // next Scan or Refresh opens the device again (as a scan that loses it does).
            warn!("Closing the session with the device: it stopped answering");
            self.drop_session();
            self.out.send(Event::SessionDropped);
        } else if reconnect {
            // A transfer didn't finish cleanly, so this session can't be trusted any more:
            // close it and open a new one. Object IDs change with the session, so the scan
            // goes too. `open` reports the new session with `Event::Opened`.
            warn!(
                "A transfer didn't finish cleanly: closing the session with the device and opening a new one, and the scan is cleared"
            );
            let id = self.bound.as_ref().map(|b| b.device_id.clone());
            if let Some(device) = self.device.clone() {
                self.open_with(device, id);
            } else {
                self.plan = None;
                self.source = None;
            }
        }
    }

    /// Tells the window which HEICs have no JPG copy, as the made list in `root` has them.
    fn send_jpg_failures(&self, id: &str, root: &Path) {
        self.out.send(Event::JpgFailed {
            id: id.to_string(),
            failed: MadeList::load_readonly(root).failed().to_vec(),
        });
    }

    /// The backfill (`retry` is `None`) or "Try again" (`Some`) for a known device's backup
    /// folder. Needs only the folder, not the device.
    fn jpg_job(&mut self, id: &str, retry: Option<Option<Vec<String>>>) {
        let result = (|| -> Result<JpgReport, String> {
            let d = self
                .registry
                .get(id)
                .ok_or("That device is not in the list.")?;
            if let Some(why) = &d.offline {
                return Err(why.clone());
            }
            let root = d
                .backup_dir
                .clone()
                .ok_or("This device has no backup folder yet.")?;
            let (view, settings) = (d.view, d.settings.clone());
            let out = &self.out;
            let cancel = &self.flags.sync.cancel;
            let on_event = &mut |ev| out.send(Event::Jpg(ev));
            let r = match retry {
                None => jpgs::backfill(
                    &root,
                    id,
                    view,
                    &settings.archive_name,
                    &settings.ignore_names,
                    cancel,
                    on_event,
                ),
                Some(heics) => {
                    jpgs::retry_failures(&root, id, view, heics.as_deref(), cancel, on_event)
                }
            };
            self.send_jpg_failures(id, &root);
            r.map_err(|e| e.to_string())
        })();
        match &result {
            Ok(r) => info!(
                "JPG copies: {} made, {} not made{}",
                r.made,
                r.failed,
                if r.cancelled { " (stopped)" } else { "" }
            ),
            Err(e) => error!("JPG copies were not made: {e}"),
        }
        self.out.send(Event::JpgDone(result));
    }

    /// A device's first scan chooses how its storages are laid out in the backup (DESIGN.md
    /// section 6.1): keep that choice with the device, so it never changes later. So are the
    /// storages that have a folder of their own, so one that is taken out is never read as
    /// deleted.
    fn store_storage_choice(&mut self, plan: &ScanPlan) {
        let Some(d) = self.registry.get_mut(&plan.device_key) else {
            return;
        };
        let mut changed = false;
        if let Some(choice) = &plan.storage_choice {
            d.settings.storage_layout = choice.layout;
            d.settings.top_level_storage = choice.top_level_storage.clone();
            info!(
                "Chose how \"{}\" lays out its storages: {:?}",
                d.name, choice.layout
            );
            changed = true;
        }
        for name in &plan.storage_folders {
            if !d
                .settings
                .storage_folders
                .iter()
                .any(|n| n.eq_ignore_ascii_case(name))
            {
                d.settings.storage_folders.push(name.clone());
                changed = true;
            }
        }
        if changed && let Err(e) = self.registry.save(&self.state) {
            warn!("Could not save the list of known devices: {e}");
        }
    }

    /// After a sync: the device's backup folder and what the sync did, for its page.
    fn record_sync(&mut self, plan_id: u64, copy: &SyncReport, archived: usize) {
        let Ok(plan) = self.current_plan(plan_id) else {
            return;
        };
        let Some(d) = self.registry.get_mut(&plan.device_key) else {
            return;
        };
        d.backup_dir = Some(plan.root.clone());
        // A sync that was stopped or failed early and copied, changed and archived nothing
        // doesn't replace what the page says of the last one that did something.
        let did_something = copy.copied > 0 || copy.replaced > 0 || archived > 0;
        let completed = !copy.cancelled
            && !copy.must_reconnect
            && !copy.disk_full
            && copy.connection_lost.is_none();
        if completed || did_something {
            d.last_sync = Some(SyncSummary {
                when: pnk_core::utc_stamp(),
                files: copy.copied as u64,
                bytes: copy.copied_bytes,
            });
        }
        self.save_registry();
        self.publish();
    }

    /// Add this sync to the device's history, tell the window, and work the sizes out again (the
    /// backup folder and its disk have changed).
    fn log_sync(&mut self, plan_id: u64, o: &SyncOutcome, began: &(Instant, String, String)) {
        let Ok(plan) = self.current_plan(plan_id) else {
            return;
        };
        let record = sync_record(&plan, o, began);
        let id = plan.device_key.clone();
        if let Err(e) = self.history_append(&id, &record) {
            warn!("Could not add this sync to the history: {e}");
        }
        self.out.send(Event::History {
            records: self.history_load(&id),
            id,
        });
        self.refresh_sizes(&plan);
    }

    /// Work out the sizes of the device's backup folder and disks without holding up this
    /// thread: the walk of a big folder takes a while, so it runs on a thread of its own and the
    /// answer comes as [`Event::Sizes`]. The device's own size is asked here (it is quick, and
    /// the open device is this thread's).
    fn refresh_sizes(&self, plan: &ScanPlan) {
        let (id, root, name) = (
            plan.device_key.clone(),
            plan.root.clone(),
            plan.archive_name.clone(),
        );
        // The fake device's folder is on this PC's disk, which is not its size.
        let on_this_pc =
            cfg!(all(debug_assertions, feature = "fake-device")) && self.fake.is_some();
        let device_disk = self
            .source
            .as_ref()
            .and_then(|s| s.local_root())
            .filter(|_| !on_this_pc)
            .and_then(platform::disk_space);
        let device_capacity = self
            .source
            .as_ref()
            .and_then(|s| s.identity().signals.capacity);
        let out = self.out.clone();
        let latest = self.sizes_generation.clone();
        let generation = latest.fetch_add(1, AtomicOrdering::SeqCst) + 1;
        let spawned = std::thread::Builder::new()
            .name("sizes".into())
            .spawn(move || {
                let names = match owner::read(&root) {
                    OwnerState::Owned(o) => o.archive_names,
                    _ => vec![name],
                };
                let started = Instant::now();
                let folder = space::folder_sizes(&root, &names);
                // The folder may not exist yet (before the first sync): ask the disk it will be on.
                let mut at = root.as_path();
                let backup_disk = loop {
                    if at.exists() {
                        break platform::disk_space(at);
                    }
                    match at.parent() {
                        Some(up) => at = up,
                        None => break None,
                    }
                };
                debug!(
                    "Worked out the size of {} in {:.1} s: {}",
                    root.display(),
                    started.elapsed().as_secs_f64(),
                    crate::format::files(folder.files as usize)
                );
                if latest.load(AtomicOrdering::SeqCst) != generation {
                    debug!("The size of {} was worked out, but a newer walk or a forgotten device made it stale", root.display());
                    return;
                }
                out.send(Event::Sizes {
                    id,
                    sizes: Box::new(Sizes {
                        folder: Some(folder),
                        backup_disk,
                        device_disk,
                        device_capacity,
                    }),
                });
            });
        if let Err(e) = spawned {
            warn!("Could not start the thread that works out sizes: {e}");
        }
    }

    /// "Use this folder for this device": through [`owner::hand_over`], which checks again that
    /// the folder is still blocked and refuses when its marker records another view.
    fn claim_folder(&self, plan_id: u64) -> Result<(), String> {
        let plan = self.current_plan(plan_id)?;
        if let Some(why) = self.offline_reason(&plan.device_key) {
            return Err(why);
        }
        owner::hand_over(
            &plan.root,
            &plan.device_key,
            &plan.device_label,
            plan.signals.clone(),
            plan.view,
            &plan.archive_name,
            &pnk_core::utc_stamp(),
        )
    }

    fn current_plan(&self, plan_id: u64) -> Result<Arc<ScanPlan>, String> {
        match &self.plan {
            Some((id, plan)) if *id == plan_id => Ok(plan.clone()),
            _ => Err("The scan is out of date. Scan again.".into()),
        }
    }

    fn edit_skip_list(
        &mut self,
        plan_id: u64,
        edit: impl FnOnce(&StateDir, &Scope, &ScanPlan) -> Result<(), String>,
    ) {
        let result = (|| -> Result<Vec<String>, String> {
            let plan = self.current_plan(plan_id)?;
            let scope = Scope::new(&plan.device_key, &plan.root);
            edit(&self.state, &scope, &plan)?;
            let mut list: Vec<String> = self
                .state
                .skip_list(&scope)
                .map_err(|e| e.to_string())?
                .into_iter()
                .collect();
            list.sort();
            Ok(list)
        })();
        if let Err(e) = &result {
            warn!("Could not change the skip list: {e}");
        }
        self.out.send(Event::SkipList { plan_id, result });
    }
}

/// An ID taken from an ID file to be a device's own: short and printable, as a marker's is.
fn is_sane_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_graphic())
}

/// "its serial number and its name", for the log.
fn gained_text(kinds: &[pnk_core::signals::SignalKind]) -> String {
    kinds
        .iter()
        .map(|k| k.describe())
        .collect::<Vec<_>>()
        .join(", ")
}

/// A command, for the log.
fn describe(cmd: &Command) -> String {
    match cmd {
        Command::ListDevices => "list devices".into(),
        Command::Open(d) => format!("open {}", d.id),
        Command::Close => "close the device".into(),
        Command::Eject(d) => format!("eject {}", d.id),
        Command::Scan(root) => format!("scan against {}", root.display()),
        Command::Sync {
            plan_id,
            items,
            archive,
            confirmed,
        } => format!(
            "sync {} from scan {plan_id}{}",
            crate::format::files(items.len()),
            match (*archive, *confirmed) {
                (true, true) => ", then archive (confirmed)",
                (true, false) => ", then archive",
                _ => "",
            }
        ),
        Command::MakeJpgs { id } => format!("make JPG copies of the HEICs in the backup of {id}"),
        Command::RetryJpgs { id, heics } => match heics {
            Some(h) => format!("try again to make JPG copies of {} for {id}", h.len()),
            None => format!("try again to make every failed JPG copy for {id}"),
        },
        Command::ClaimFolder { plan_id } => {
            format!("make the backup folder of scan {plan_id} this device's")
        }
        Command::SkipAdd { plan_id, entry } => {
            format!("add file {entry} of scan {plan_id} to the skip list")
        }
        Command::SkipRemove { plan_id, path } => {
            format!("remove {path} from the skip list (scan {plan_id})")
        }
        Command::SkipClear { plan_id } => format!("clear the skip list (scan {plan_id})"),
        Command::DiscardPlan => "discard the scan".into(),
        Command::Answer { reply, .. } => format!("answer the question: {reply:?}"),
        Command::LinkExisting { id, .. } => format!("the open device is the known device {id}"),
        Command::CreateDevice(n) => format!("set up \"{}\" as a new device", n.name),
        Command::NotThisDevice { .. } => "not this device".into(),
        Command::WriteIdFile { .. } => "write an ID file".into(),
        Command::ReplaceIdFile { .. } => "replace the ID file with a new ID".into(),
        Command::Rename { id, name } => format!("rename {id} to \"{name}\""),
        Command::Forget { id } => format!("forget {id}"),
        Command::LoadHistory { id } => format!("load the sync history of {id}"),
        Command::SetSettings { id, .. } => format!("save the settings of {id}"),
        Command::SetBackupDir { id, dir } => {
            format!("backup folder of {id}: {}", dir.display())
        }
        Command::Relocate { id, dir } => {
            format!("the backup of {id} moved to {}", dir.display())
        }
        Command::RenameArchive { plan_id, new_name } => {
            format!("rename the archive folder to \"{new_name}\" (scan {plan_id})")
        }
        Command::FindBackups { root, apply } => format!(
            "find my backups in {}{}",
            root.display(),
            if *apply { ", add them" } else { "" }
        ),
        Command::AddExisting { dir } => format!("add the existing backup in {}", dir.display()),
    }
}

/// What a finished (or stopped) sync adds to the device's history.
fn sync_record(plan: &ScanPlan, o: &SyncOutcome, began: &(Instant, String, String)) -> SyncRecord {
    let c = &o.copy;
    let (moved, note) = match &o.archive {
        Some(Ok(a)) => (a.moved as u64, String::new()),
        Some(Err(e)) => (
            0,
            format!("moving files deleted from the device was refused: {e}"),
        ),
        None => (0, String::new()),
    };
    SyncRecord {
        started: began.1.clone(),
        local: began.2.clone(),
        secs: began.0.elapsed().as_secs_f64(),
        copied: c.copied as u64,
        copied_bytes: c.copied_bytes,
        replaced: c.replaced as u64,
        already_backed_up: c.already_backed_up as u64,
        skipped: c.skipped as u64,
        failed: c.failed as u64,
        mode: plan.deleted_files,
        moved,
        // Whatever left the device and is still in the backup folder after this sync.
        left: (plan.removed.len() as u64).saturating_sub(moved),
        move_refused: matches!(&o.archive, Some(Err(_))),
        jpgs_made: o
            .jpg
            .as_ref()
            .and_then(|j| j.as_ref().ok())
            .map_or(0, |j| j.made as u64),
        jpgs_failed: o
            .jpg
            .as_ref()
            .and_then(|j| j.as_ref().ok())
            .map_or(0, |j| j.failed as u64),
        stopped: c.cancelled,
        // A full backup disk is what ended it, even though the cut-short transfer also made
        // the connection start afresh; the history names one reason.
        cut_off: c.must_reconnect && !c.disk_full,
        disk_full: c.disk_full,
        note,
    }
}

/// Fake-device reads slowed to about 3 MB/s, so Skip current and Stop can be tried by hand.
#[cfg(all(debug_assertions, feature = "fake-device"))]
mod slow {
    use std::io::Read;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

    use pnk_core::device::{DeviceError, DeviceIdentity, DeviceSource, Enumeration, ObjectInfo};

    pub struct Slow<S>(pub S);

    impl<S: DeviceSource> DeviceSource for Slow<S> {
        fn identity(&self) -> &DeviceIdentity {
            self.0.identity()
        }
        fn enumerate(
            &mut self,
            cancel: &AtomicBool,
            progress: &mut dyn FnMut(usize, &str),
        ) -> Result<Enumeration, DeviceError> {
            self.0.enumerate(cancel, progress)
        }
        fn object_info(&mut self, object_id: &str) -> Result<ObjectInfo, DeviceError> {
            self.0.object_info(object_id)
        }
        fn local_root(&self) -> Option<&std::path::Path> {
            self.0.local_root()
        }
        fn open_read(&mut self, object_id: &str) -> Result<Box<dyn Read + '_>, DeviceError> {
            Ok(Box::new(SlowReader(self.0.open_read(object_id)?)))
        }
    }

    struct SlowReader<R>(R);

    impl<R: Read> Read for SlowReader<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            std::thread::sleep(Duration::from_millis(20));
            let n = buf.len().min(64 * 1024);
            self.0.read(&mut buf[..n])
        }
    }
}

/// Core's notes are lower-case fragments; start one with a capital for the window.
fn sentence(note: &str) -> String {
    let mut chars = note.trim().trim_end_matches('.').chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::sentence;

    #[test]
    fn notes_become_sentences() {
        assert_eq!(
            sentence("no temporary file was left"),
            "No temporary file was left"
        );
        assert_eq!(sentence("left x alone."), "Left x alone");
        assert_eq!(sentence(""), "");
    }
}

/// The worker with the folder-backed fake device: the parts of the "transfer cut short" fix
/// that live outside core.
#[cfg(all(test, debug_assertions, feature = "fake-device"))]
mod fake_device_tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::Ordering;

    const DEV: &str = FAKE_PNP_ID;

    fn next<T>(rx: &Receiver<Event>, what: &str, mut pick: impl FnMut(Event) -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(ev) => {
                    if let Some(t) = pick(ev) {
                        return t;
                    }
                }
                Err(e) => panic!("no {what} from the worker: {e}"),
            }
        }
    }

    fn opened(tx: &Sender<Command>, rx: &Receiver<Event>) -> Box<Opened> {
        tx.send(Command::Open(fake_device(Path::new("iPhone"))))
            .unwrap();
        next(rx, "Opened", |e| match e {
            Event::Opened { result, .. } => Some(result.expect("open the fake")),
            _ => None,
        })
    }

    fn ready(rx: &Receiver<Event>) -> (String, How) {
        let o = next(rx, "Opened", |e| match e {
            Event::Opened { result, .. } => Some(result.expect("open the fake")),
            _ => None,
        });
        match o.stage {
            Stage::Ready { device_id, how } => (device_id, how),
            s => panic!("expected a known device, got {s:?}"),
        }
    }

    /// Open the fake device (a new device: the card) and set it up with the default settings.
    fn set_up(tx: &Sender<Command>, rx: &Receiver<Event>, backup: &Path) -> String {
        let o = opened(tx, rx);
        assert!(matches!(o.stage, Stage::Setup(_)), "{:?}", o.stage);
        tx.send(Command::CreateDevice(Box::new(NewDevice {
            device: DEV.into(),
            name: "Fake phone".into(),
            backup_dir: backup.to_path_buf(),
            settings: DeviceSettings::defaults_for(DeviceKind::Ptp),
            use_id_file_id: false,
        })))
        .unwrap();
        let (id, how) = ready(rx);
        assert_eq!(how, How::Created);
        id
    }

    /// A command about "the open device" that names another device is refused, and nothing is
    /// changed: a second device may have been opened since the window asked.
    #[test]
    fn a_command_for_another_device_than_the_open_one_is_refused() {
        let t = tempfile::tempdir().unwrap();
        let phone = t.path().join("phone");
        fs::create_dir_all(phone.join("DCIM")).unwrap();
        fs::write(phone.join("DCIM/A.JPG"), [1u8; 100]).unwrap();
        let backup = t.path().join("backup");
        let state = StateDir::open(t.path().join("state")).unwrap();
        let fake = Fake {
            folder: phone,
            slow: false,
            lag: false,
            named: false,
        };
        let (tx, rx) = spawn(
            egui::Context::default(),
            state,
            Some(fake),
            Flags::default(),
        );
        set_up(&tx, &rx, &backup);
        for cmd in [
            Command::WriteIdFile {
                device: "/media/other".into(),
            },
            Command::ReplaceIdFile {
                device: "/media/other".into(),
            },
            Command::NotThisDevice {
                device: "/media/other".into(),
            },
        ] {
            tx.send(cmd).unwrap();
            let refused = next(&rx, "Notice", |e| match e {
                Event::Notice(r) => Some(r),
                _ => None,
            });
            assert!(
                refused.unwrap_err().contains("no longer the open one"),
                "refused"
            );
        }
    }

    /// Eject drops everything held on the device first, asks the system only for a stick or
    /// drive, and says what the system answered.
    #[test]
    fn eject_lets_go_of_the_open_device_and_never_ejects_a_phone() {
        use std::sync::atomic::AtomicUsize;
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        fn system_eject(_d: &ListedDevice) -> Result<(), String> {
            CALLS.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        let t = tempfile::tempdir().unwrap();
        let phone = t.path().join("phone");
        fs::create_dir_all(phone.join("DCIM")).unwrap();
        fs::write(phone.join("DCIM/A.JPG"), [1u8; 100]).unwrap();
        let backup = t.path().join("backup");
        let state = StateDir::open(t.path().join("state")).unwrap();
        let fake = Fake {
            folder: phone,
            slow: false,
            lag: false,
            named: false,
        };
        let (tx, rx) = spawn_with(
            egui::Context::default(),
            state,
            Some(fake),
            Flags::default(),
            system_eject,
        );
        set_up(&tx, &rx, &backup);
        let listed = |transport| ListedDevice {
            id: "/media/me/STICK".into(),
            label: "Stick".into(),
            kind_hint: None,
            transport,
        };
        let answer = |rx: &Receiver<Event>| {
            next(rx, "Ejected", |e| match e {
                Event::Ejected { id, result, .. } => Some((id, result)),
                _ => None,
            })
        };
        // A phone is refused without asking the system.
        tx.send(Command::Eject(listed(Transport::Mtp))).unwrap();
        let (_, result) = answer(&rx);
        assert!(result.is_err());
        assert_eq!(CALLS.load(Ordering::SeqCst), 0);
        // Another stick is ejected, and the open device is left alone: it can still be scanned.
        tx.send(Command::Eject(listed(Transport::Volume))).unwrap();
        let (id, result) = answer(&rx);
        assert_eq!((id.as_str(), result), ("/media/me/STICK", Ok(())));
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
        let _ = scan(&tx, &rx, &backup);
        // The open device itself, ejected: what was held on it is dropped first.
        let mut open = listed(Transport::Volume);
        open.id = FAKE_PNP_ID.into();
        tx.send(Command::Eject(open)).unwrap();
        let (_, result) = answer(&rx);
        assert_eq!(result, Ok(()));
        assert_eq!(CALLS.load(Ordering::SeqCst), 2);
        tx.send(Command::Scan(backup)).unwrap();
        let failed = next(&rx, "ScanFailed", |e| match e {
            Event::ScanFailed { message: m, .. } => Some(m),
            _ => None,
        });
        assert!(failed.contains("No device is open"), "{failed}");
    }

    fn history_size(state: &Path) -> usize {
        let reg = Registry::load(&StateDir::open(state).unwrap());
        reg.devices()
            .iter()
            .map(|d| {
                [
                    SignalKind::Maker,
                    SignalKind::Model,
                    SignalKind::FriendlyName,
                ]
                .iter()
                .map(|k| d.history.values(*k).len())
                .sum::<usize>()
                    + d.not_this.len()
            })
            .sum()
    }

    fn scan(tx: &Sender<Command>, rx: &Receiver<Event>, root: &Path) -> (u64, Arc<ScanPlan>) {
        tx.send(Command::Scan(root.to_path_buf())).unwrap();
        next(rx, "ScanDone", |e| match e {
            Event::ScanDone(d) => Some((d.plan_id, d.plan)),
            Event::ScanFailed { message: m, .. } => panic!("scan failed: {m}"),
            _ => None,
        })
    }

    fn new_items(plan: &ScanPlan) -> Vec<SyncItem> {
        plan.entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.status == Status::New)
            .map(|(entry, _)| SyncItem {
                entry,
                mode: CopyMode::NoReplace,
            })
            .collect()
    }

    /// After a sync the device's entry holds its backup folder and what the sync did, and the
    /// first scan stored how its storages are laid out.
    #[test]
    fn a_sync_records_the_backup_folder_and_the_last_sync_in_the_registry() {
        let t = tempfile::tempdir().unwrap();
        let phone = t.path().join("phone");
        fs::create_dir_all(phone.join("DCIM")).unwrap();
        fs::write(phone.join("DCIM/A.JPG"), [1u8; 1_000]).unwrap();
        fs::write(phone.join("DCIM/B.JPG"), [2u8; 2_000]).unwrap();
        let backup = t.path().join("backup");
        let state_path = t.path().join("state");
        let state = StateDir::open(&state_path).unwrap();
        let fake = Fake {
            folder: phone,
            slow: false,
            lag: false,
            named: false,
        };
        let (tx, rx) = spawn(
            egui::Context::default(),
            state,
            Some(fake),
            Flags::default(),
        );
        set_up(&tx, &rx, &backup);
        let (plan_id, plan) = scan(&tx, &rx, &backup);
        tx.send(Command::Sync {
            plan_id,
            items: new_items(&plan),
            archive: false,
            confirmed: false,
        })
        .unwrap();
        let done = next(&rx, "SyncDone", |e| match e {
            Event::SyncDone { result: r, .. } => Some(r.expect("the sync ran")),
            _ => None,
        });
        assert_eq!(done.copy.copied, 2);
        let reg = Registry::load(&StateDir::open(&state_path).unwrap());
        let d = &reg.devices()[0];
        assert_eq!(d.backup_dir.as_deref(), Some(plan.root.as_path()));
        let last = d.last_sync.as_ref().expect("the last sync is recorded");
        assert_eq!((last.files, last.bytes), (2, 3_000));
        assert!(last.when.ends_with('Z'), "{}", last.when);
        assert_eq!(
            d.settings.storage_layout,
            pnk_core::registry::StorageLayout::TopLevel
        );
    }

    /// The whole path with the system's own decoder (skipped where there is none): a sync makes
    /// the JPG copies after the copying, reports them as events and in the outcome, a photo
    /// that can't be converted is recorded without failing the sync, and the history line has
    /// the counts. "Try again" and the backfill then run on the backup folder alone.
    #[test]
    fn a_sync_makes_jpg_copies_and_records_them_in_the_history() {
        if pnk_core::heic::available().is_err() {
            eprintln!("skipped: no HEIC decoder here");
            return;
        }
        let t = tempfile::tempdir().unwrap();
        let phone = t.path().join("phone");
        fs::create_dir_all(phone.join("DCIM/100APPLE")).unwrap();
        let dir = phone.join("DCIM/100APPLE");
        fs::write(
            dir.join("IMG_0001.HEIC"),
            include_bytes!("../../core/src/heic/fixtures/plain.heic"),
        )
        .unwrap();
        fs::write(
            dir.join("IMG_0002.HEIC"),
            include_bytes!("../../core/src/heic/fixtures/rotated.heic"),
        )
        .unwrap();
        fs::write(dir.join("IMG_0003.HEIC"), b"not a photo at all, just bytes").unwrap();
        let backup = t.path().join("backup");
        let state_path = t.path().join("state");
        let state = StateDir::open(&state_path).unwrap();
        let fake = Fake {
            folder: phone,
            slow: false,
            lag: false,
            named: false,
        };
        let (tx, rx) = spawn(
            egui::Context::default(),
            state,
            Some(fake),
            Flags::default(),
        );
        let o = opened(&tx, &rx);
        assert!(matches!(o.stage, Stage::Setup(_)));
        tx.send(Command::CreateDevice(Box::new(NewDevice {
            device: DEV.into(),
            name: "Fake phone".into(),
            backup_dir: backup.clone(),
            settings: DeviceSettings {
                jpeg_copies: true,
                ..DeviceSettings::defaults_for(DeviceKind::Ptp)
            },
            use_id_file_id: false,
        })))
        .unwrap();
        let (id, _) = ready(&rx);
        let (plan_id, plan) = scan(&tx, &rx, &backup);
        assert!(plan.jpg.enabled);
        assert_eq!(plan.jpg.to_make, 3);
        assert_eq!(plan.summary(0).jpg_to_make, 3);
        tx.send(Command::Sync {
            plan_id,
            items: new_items(&plan),
            archive: false,
            confirmed: false,
        })
        .unwrap();
        let mut started = 0;
        let mut dealt_with = 0;
        let mut failed = Vec::new();
        let done = next(&rx, "SyncDone", |e| match e {
            // The window counts these to keep "Start sync (…, N JPG copies)" true.
            Event::Jpg(
                pnk_core::jpgs::JpgEvent::Made { .. } | pnk_core::jpgs::JpgEvent::Failed { .. },
            ) => {
                dealt_with += 1;
                None
            }
            Event::JpgFailed { failed: f, .. } => {
                failed = f;
                None
            }
            Event::Jpg(pnk_core::jpgs::JpgEvent::Started { total, .. }) => {
                assert_eq!(total, 3);
                started += 1;
                None
            }
            Event::SyncDone { result: r, .. } => Some(r.expect("the sync ran")),
            _ => None,
        });
        assert_eq!(started, 3);
        assert_eq!(dealt_with, plan.jpg.to_make);
        assert_eq!((done.copy.copied, done.copy.failed), (3, 0));
        let j = done.jpg.as_ref().unwrap().as_ref().unwrap();
        assert_eq!((j.made, j.failed), (2, 1), "{j:?}");
        assert!(backup.join("DCIM/100APPLE/IMG_0001.JPG").exists());
        assert!(backup.join("DCIM/100APPLE/IMG_0002.JPG").exists());
        assert!(!backup.join("DCIM/100APPLE/IMG_0003.JPG").exists());
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].heic, "DCIM/100APPLE/IMG_0003.HEIC");
        // The history is in the backup folder, not the settings folder.
        let hist = pnk_core::history::load_at(&backup.join(pnk_core::folder_record::HISTORY_FILE));
        assert!(
            History::new(&StateDir::open(&state_path).unwrap())
                .load(&id)
                .is_empty()
        );
        assert_eq!((hist[0].jpgs_made, hist[0].jpgs_failed), (2, 1));
        // Try again for all: the photo is still no photo, so it fails again, in the folder alone.
        tx.send(Command::RetryJpgs {
            id: id.clone(),
            heics: None,
        })
        .unwrap();
        let r = next(&rx, "JpgDone", |e| match e {
            Event::JpgDone(r) => Some(r.expect("it ran")),
            _ => None,
        });
        assert_eq!((r.made, r.failed), (0, 1));
        // The backfill finds nothing left to make.
        tx.send(Command::MakeJpgs { id }).unwrap();
        let r = next(&rx, "JpgDone", |e| match e {
            Event::JpgDone(r) => Some(r.expect("it ran")),
            _ => None,
        });
        assert_eq!((r.made, r.failed, r.already_failed), (0, 0, 1));
    }

    /// Stop part-way through B on a phone that then falls one file behind (as an iPhone did).
    /// C and D have B's size, so only a new connection keeps them from getting B's data.
    /// A new device is not registered by itself: the card comes first. Then, with only weak
    /// signals to go on, it is asked about, and nothing is learned or rejected until the answer.
    #[test]
    fn a_new_device_waits_for_its_card_and_a_weak_match_asks_before_learning() {
        let t = tempfile::tempdir().unwrap();
        let phone = t.path().join("phone");
        fs::create_dir_all(&phone).unwrap();
        let state_path = t.path().join("state");
        let state = StateDir::open(&state_path).unwrap();
        let fake = Fake {
            folder: phone,
            slow: false,
            lag: false,
            named: false,
        };
        let (tx, rx) = spawn(
            egui::Context::default(),
            state,
            Some(fake),
            Flags::default(),
        );
        // First time: the new-device card; nothing is registered yet.
        let o = opened(&tx, &rx);
        assert!(matches!(o.stage, Stage::Setup(_)));
        assert!(
            Registry::load(&StateDir::open(&state_path).unwrap())
                .devices()
                .is_empty()
        );
        // A scan is refused until the device is set up.
        tx.send(Command::Scan(t.path().join("backup"))).unwrap();
        next(&rx, "ScanFailed", |e| {
            matches!(e, Event::ScanFailed { .. }).then_some(())
        });
        tx.send(Command::CreateDevice(Box::new(NewDevice {
            device: DEV.into(),
            name: "Fake phone".into(),
            backup_dir: t.path().join("backup"),
            settings: DeviceSettings::defaults_for(DeviceKind::Ptp),
            use_id_file_id: false,
        })))
        .unwrap();
        let (id, how) = ready(&rx);
        assert_eq!(how, How::Created);
        assert_eq!(id.len(), 36, "an app-made ID, not {id}");
        let before = history_size(&state_path);

        // Opened again: only the weak signals (maker, model, name) agree. It asks, and leaves
        // the device unidentified with nothing learned.
        let o = opened(&tx, &rx);
        let Stage::Ask(Question::Maybe { candidate }) = o.stage else {
            panic!("expected \"is this X?\", got {:?}", o.stage);
        };
        assert_eq!(candidate.id, id);
        assert_eq!(
            history_size(&state_path),
            before,
            "nothing learned before the answer"
        );
        tx.send(Command::Scan(t.path().join("backup"))).unwrap();
        next(&rx, "ScanFailed", |e| {
            matches!(e, Event::ScanFailed { .. }).then_some(())
        });

        // No: it is rejected, and the card comes up for a new device.
        tx.send(Command::Answer {
            device: DEV.into(),
            reply: Answer::No,
        })
        .unwrap();
        let o = next(&rx, "Opened", |e| match e {
            Event::Opened { result, .. } => Some(result.unwrap()),
            _ => None,
        });
        assert!(matches!(o.stage, Stage::Setup(_)), "{:?}", o.stage);
        let reg = Registry::load(&StateDir::open(&state_path).unwrap());
        assert_eq!(reg.devices().len(), 1);
        assert_eq!(reg.devices()[0].not_this.len(), 1);

        // "Not this device?" on a device the user confirmed puts back what it learned.
        let o = opened(&tx, &rx);
        assert!(
            matches!(o.stage, Stage::Setup(_)),
            "rejected: not proposed again"
        );
    }

    /// "Is this X?" answered Yes: the device is X, and archiving asks first, because only weak
    /// signals agree. The caution is not lost when the device is opened again.
    #[test]
    fn yes_to_is_this_x_passes_the_weak_match_caution_on_to_archiving() {
        let t = tempfile::tempdir().unwrap();
        let phone = t.path().join("phone");
        fs::create_dir_all(phone.join("DCIM")).unwrap();
        fs::write(phone.join("DCIM/A.JPG"), [1u8; 100]).unwrap();
        fs::write(phone.join("DCIM/B.JPG"), [2u8; 100]).unwrap();
        let backup = t.path().join("backup");
        let state = StateDir::open(t.path().join("state")).unwrap();
        let fake = Fake {
            folder: phone.clone(),
            slow: false,
            lag: false,
            named: false,
        };
        let (tx, rx) = spawn(
            egui::Context::default(),
            state,
            Some(fake),
            Flags::default(),
        );
        set_up(&tx, &rx, &backup);
        let (plan_id, plan) = scan(&tx, &rx, &backup);
        tx.send(Command::Sync {
            plan_id,
            items: new_items(&plan),
            archive: false,
            confirmed: false,
        })
        .unwrap();
        next(&rx, "SyncDone", |e| {
            matches!(e, Event::SyncDone { .. }).then_some(())
        });
        fs::remove_file(phone.join("DCIM/B.JPG")).unwrap();

        for round in 0..2 {
            let o = opened(&tx, &rx);
            assert!(
                matches!(o.stage, Stage::Ask(Question::Maybe { .. })),
                "round {round}"
            );
            tx.send(Command::Answer {
                device: DEV.into(),
                reply: Answer::Yes,
            })
            .unwrap();
            let (_, how) = ready(&rx);
            assert_eq!(how, How::Answered);
            let (_, plan) = scan(&tx, &rx, &backup);
            assert_eq!(plan.removed.len(), 1);
            assert!(
                plan.cautions
                    .iter()
                    .any(|c| matches!(c, pnk_core::plan::Caution::WeakMatch)),
                "round {round}: {:?}",
                plan.cautions
            );
        }
    }

    /// "Not this device?" takes back what recognition learned, and asks again.
    #[test]
    fn not_this_device_restores_what_was_learned_and_rejects_it() {
        let t = tempfile::tempdir().unwrap();
        let phone = t.path().join("phone");
        fs::create_dir_all(&phone).unwrap();
        let state_path = t.path().join("state");
        let state = StateDir::open(&state_path).unwrap();
        let fake = Fake {
            folder: phone,
            slow: false,
            lag: false,
            named: false,
        };
        let (tx, rx) = spawn(
            egui::Context::default(),
            state,
            Some(fake),
            Flags::default(),
        );
        let backup = t.path().join("backup");
        set_up(&tx, &rx, &backup);
        // Said yes to "is this X?", then "Not this device?".
        opened(&tx, &rx);
        tx.send(Command::Answer {
            device: DEV.into(),
            reply: Answer::Yes,
        })
        .unwrap();
        ready(&rx);
        tx.send(Command::NotThisDevice { device: DEV.into() })
            .unwrap();
        let o = next(&rx, "Opened", |e| match e {
            Event::Opened { result, .. } => Some(result.unwrap()),
            _ => None,
        });
        assert!(matches!(o.stage, Stage::Setup(_)), "{:?}", o.stage);
        let reg = Registry::load(&StateDir::open(&state_path).unwrap());
        assert_eq!(reg.devices()[0].not_this.len(), 1);
    }

    #[test]
    fn after_a_stop_part_way_the_device_is_reopened_and_the_scan_dropped() {
        let t = tempfile::tempdir().unwrap();
        let phone = t.path().join("phone");
        fs::create_dir_all(phone.join("DCIM")).unwrap();
        fs::write(phone.join("DCIM/A.JPG"), [1u8; 1_000]).unwrap();
        for (name, byte) in [("B.MOV", 2u8), ("C.JPG", 3), ("D.JPG", 4)] {
            fs::write(phone.join("DCIM").join(name), vec![byte; 3_000_000]).unwrap();
        }
        let backup = t.path().join("backup");
        let state = StateDir::open(t.path().join("state")).unwrap();
        let flags = Flags::default();
        let fake = Fake {
            folder: phone.clone(),
            slow: true,
            lag: true,
            named: false,
        };
        let (tx, rx) = spawn(egui::Context::default(), state, Some(fake), flags.clone());

        set_up(&tx, &rx, &backup);
        let (plan_id, plan) = scan(&tx, &rx, &backup);
        let b = plan
            .entries
            .iter()
            .position(|e| e.source.as_str() == "DCIM/B.MOV")
            .unwrap();
        tx.send(Command::Sync {
            plan_id,
            items: new_items(&plan),
            archive: false,
            confirmed: false,
        })
        .unwrap();
        // Stop once B is part-way through.
        let outcome = next(&rx, "SyncDone", |e| match e {
            Event::Sync(SyncEvent::Progress { entry, bytes, .. }) if entry == b && bytes > 0 => {
                flags.sync.cancel.store(true, Ordering::SeqCst);
                None
            }
            Event::SyncDone { result: r, .. } => Some(r.expect("the sync ran")),
            _ => None,
        });
        assert!(
            outcome.copy.cancelled && outcome.copy.must_reconnect,
            "{:?}",
            outcome.copy
        );
        // A new connection is opened by itself, as the same device, without asking again...
        let (_, how) = ready(&rx);
        assert_eq!(how, How::Reconnected);
        // ...and the old scan is gone with the old connection.
        flags.sync.cancel.store(false, Ordering::SeqCst);
        tx.send(Command::Sync {
            plan_id,
            items: new_items(&plan),
            archive: false,
            confirmed: false,
        })
        .unwrap();
        let stale = next(&rx, "SyncDone", |e| match e {
            Event::SyncDone { result: r, .. } => Some(r),
            _ => None,
        });
        match stale {
            Err(m) => assert!(m.contains("out of date"), "{m}"),
            Ok(o) => panic!("a sync ran on the old scan: {:?}", o.copy),
        }

        // Scan again and carry on: every file arrives with its own data.
        let (plan_id, plan) = scan(&tx, &rx, &backup);
        tx.send(Command::Sync {
            plan_id,
            items: new_items(&plan),
            archive: false,
            confirmed: false,
        })
        .unwrap();
        let done = next(&rx, "SyncDone", |e| match e {
            Event::SyncDone { result: r, .. } => Some(r.expect("the sync ran")),
            _ => None,
        });
        assert!(
            !done.copy.must_reconnect && done.copy.failed == 0,
            "{:?}",
            done.copy
        );
        for name in ["A.JPG", "B.MOV", "C.JPG", "D.JPG"] {
            assert_eq!(
                fs::read(backup.join("DCIM").join(name)).unwrap(),
                fs::read(phone.join("DCIM").join(name)).unwrap(),
                "{name} holds another file's data"
            );
        }
    }

    /// A worker on `state_path`, with the fake device serving `phone`.
    fn worker_on(state_path: &Path, phone: &Path) -> (Sender<Command>, Receiver<Event>) {
        let state = StateDir::open(state_path).unwrap();
        let fake = Fake {
            folder: phone.to_path_buf(),
            slow: false,
            lag: false,
            named: false,
        };
        spawn(
            egui::Context::default(),
            state,
            Some(fake),
            Flags::default(),
        )
    }

    /// The known devices the worker last published (waits for the next `Registry` event).
    fn registry_event(rx: &Receiver<Event>) -> (Vec<KnownDevice>, Vec<(String, FolderState)>) {
        next(rx, "Registry", |e| match e {
            Event::Registry { devices, folders } => Some((devices, folders)),
            _ => None,
        })
    }

    fn notice(rx: &Receiver<Event>) -> Result<String, String> {
        next(rx, "Notice", |e| match e {
            Event::Notice(r) => Some(r),
            _ => None,
        })
    }

    /// A phone set up with settings of its own and synced once, in `t`: returns the backup
    /// folder, the device's ID and the settings folder it was done with.
    fn synced_phone(t: &Path) -> (PathBuf, String, PathBuf) {
        let phone = t.join("phone");
        fs::create_dir_all(phone.join("DCIM")).unwrap();
        fs::write(phone.join("DCIM/A.JPG"), [1u8; 500]).unwrap();
        let backup = t.join("backups").join("Fake phone");
        let state_path = t.join("state");
        let (tx, rx) = worker_on(&state_path, &phone);
        let o = opened(&tx, &rx);
        assert!(matches!(o.stage, Stage::Setup(_)));
        tx.send(Command::CreateDevice(Box::new(NewDevice {
            device: DEV.into(),
            name: "Fake phone".into(),
            backup_dir: backup.clone(),
            settings: DeviceSettings {
                scan_automatically: true,
                archive_name: "_Old stuff".into(),
                ..DeviceSettings::defaults_for(DeviceKind::Ptp)
            },
            use_id_file_id: false,
        })))
        .unwrap();
        let (id, _) = ready(&rx);
        let (plan_id, plan) = scan(&tx, &rx, &backup);
        tx.send(Command::Sync {
            plan_id,
            items: new_items(&plan),
            archive: false,
            confirmed: false,
        })
        .unwrap();
        next(&rx, "SyncDone", |e| match e {
            Event::SyncDone { result: r, .. } => Some(r.expect("the sync ran")),
            _ => None,
        });
        (backup, id, state_path)
    }

    /// The backup folder holds the device's record and history, and the settings folder keeps
    /// only a pointer and what is needed to list the device.
    #[test]
    fn the_backup_folder_holds_the_record_and_the_profile_only_points() {
        let t = tempfile::tempdir().unwrap();
        let (backup, id, state_path) = synced_phone(t.path());
        let record = fs::read_to_string(backup.join(pnk_core::folder_record::DEVICE_FILE)).unwrap();
        assert!(record.contains("_Old stuff"), "{record}");
        let hist = fs::read_to_string(backup.join(pnk_core::folder_record::HISTORY_FILE)).unwrap();
        assert_eq!(hist.lines().count(), 1, "{hist}");
        let profile = fs::read_to_string(state_path.join("devices.json")).unwrap();
        assert!(profile.contains("\"in_folder\": true"), "{profile}");
        assert!(!profile.contains("_Old stuff"), "{profile}");
        assert!(
            !state_path
                .join("history")
                .join(format!("{id}.jsonl"))
                .exists()
        );
    }

    /// New computer: a fresh settings folder, the backup folder copied across, and "Add an
    /// existing backup" gives back the device with its settings and history, and the same
    /// device plugged in is known (it is asked about, as before, not set up as new).
    #[test]
    fn add_an_existing_backup_to_a_fresh_profile_restores_the_device() {
        let t = tempfile::tempdir().unwrap();
        let (backup, id, _old_state) = synced_phone(t.path());
        let phone = t.path().join("phone");
        let (tx, rx) = worker_on(&t.path().join("fresh-state"), &phone);
        let (devices, _) = registry_event(&rx);
        assert!(devices.is_empty());
        // A folder that isn't a backup says so.
        tx.send(Command::AddExisting {
            dir: t.path().join("phone"),
        })
        .unwrap();
        assert!(notice(&rx).unwrap_err().contains("No backup found"));
        // The folder that holds backups works, and so would the backup folder itself.
        tx.send(Command::AddExisting {
            dir: backup.parent().unwrap().to_path_buf(),
        })
        .unwrap();
        let said = notice(&rx).unwrap();
        assert!(said.contains("Added 1 backup"), "{said}");
        let (devices, folders) = registry_event(&rx);
        let d = &devices[0];
        assert_eq!(d.id, id);
        assert!(d.settings.scan_automatically);
        assert_eq!(d.settings.archive_name, "_Old stuff");
        assert!(d.last_sync.is_some());
        assert_eq!(folders, [(id.clone(), FolderState::Present)]);
        // Adding it again changes nothing.
        tx.send(Command::AddExisting {
            dir: backup.clone(),
        })
        .unwrap();
        assert!(notice(&rx).unwrap().contains("Already known"));
        tx.send(Command::LoadHistory { id: id.clone() }).unwrap();
        let records = next(&rx, "History", |e| match e {
            Event::History { records, .. } => Some(records),
            _ => None,
        });
        assert_eq!(records.len(), 1);
        // Recognition has what it needs: the device is known, not new.
        let o = opened(&tx, &rx);
        let Stage::Ask(Question::Maybe { candidate }) = o.stage else {
            panic!("expected the device to be known, got {:?}", o.stage);
        };
        assert_eq!(candidate.id, id);
        // And it is the same after a restart on the new computer.
        drop(tx);
        let (tx, rx) = worker_on(&t.path().join("fresh-state"), &phone);
        let (devices, _) = registry_event(&rx);
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].id, id);
        drop(tx);
    }

    /// A backup folder that is away (an unplugged drive) leaves the device in the list, marked,
    /// and neither scanned, synced nor changed; "Find it…" (relocate) brings it back.
    #[test]
    fn a_device_whose_backup_folder_is_away_is_listed_and_cannot_be_used() {
        let t = tempfile::tempdir().unwrap();
        let (backup, id, state_path) = synced_phone(t.path());
        let phone = t.path().join("phone");
        let away = t.path().join("backups").join("unplugged");
        fs::rename(&backup, &away).unwrap();
        let (tx, rx) = worker_on(&state_path, &phone);
        let (devices, folders) = registry_event(&rx);
        assert_eq!(devices.len(), 1, "never silently dropped");
        let why = devices[0].offline.clone().expect("offline");
        assert!(why.starts_with("Backup folder not found: "), "{why}");
        assert_eq!(folders, [(id.clone(), FolderState::Missing)]);
        // It is still recognised when plugged in (its recognition data is kept), but a scan,
        // settings and a rename are refused.
        let o = opened(&tx, &rx);
        assert!(matches!(o.stage, Stage::Ask(_)), "{:?}", o.stage);
        tx.send(Command::Answer {
            device: DEV.into(),
            reply: Answer::Yes,
        })
        .unwrap();
        let (bound, _) = ready(&rx);
        assert_eq!(bound, id);
        tx.send(Command::Scan(backup.clone())).unwrap();
        let failed = next(&rx, "ScanFailed", |e| match e {
            Event::ScanFailed { message: m, .. } => Some(m),
            _ => None,
        });
        assert!(failed.contains("Backup folder not found"), "{failed}");
        tx.send(Command::SetSettings {
            id: id.clone(),
            settings: Box::new(DeviceSettings::defaults_for(DeviceKind::Ptp)),
        })
        .unwrap();
        assert!(notice(&rx).unwrap_err().contains("Backup folder not found"));
        tx.send(Command::Rename {
            id: id.clone(),
            name: "Other".into(),
        })
        .unwrap();
        assert!(notice(&rx).is_err());
        assert!(!backup.exists(), "nothing recreated the folder");
        // Found: the record comes from the folder.
        tx.send(Command::Relocate {
            id: id.clone(),
            dir: away.clone(),
        })
        .unwrap();
        assert!(notice(&rx).is_ok());
        let (devices, folders) = registry_event(&rx);
        assert!(devices[0].offline.is_none());
        assert_eq!(devices[0].settings.archive_name, "_Old stuff");
        assert_eq!(folders, [(id, FolderState::Present)]);
        drop(tx);
    }

    /// "Change…" writes the record into the new folder and copies the history, leaving the old
    /// folder's files alone.
    #[test]
    fn changing_the_folder_copies_the_record_and_history_across() {
        let t = tempfile::tempdir().unwrap();
        let (backup, id, state_path) = synced_phone(t.path());
        let phone = t.path().join("phone");
        let (tx, rx) = worker_on(&state_path, &phone);
        let _ = registry_event(&rx);
        let new = t.path().join("new-place");
        fs::create_dir_all(&new).unwrap();
        tx.send(Command::SetBackupDir {
            id,
            dir: new.clone(),
        })
        .unwrap();
        let _ = registry_event(&rx);
        for f in [
            pnk_core::folder_record::DEVICE_FILE,
            pnk_core::folder_record::HISTORY_FILE,
        ] {
            assert!(new.join(f).is_file(), "{f} in the new folder");
            assert!(backup.join(f).is_file(), "{f} left in the old folder");
        }
        drop(tx);
    }
}
