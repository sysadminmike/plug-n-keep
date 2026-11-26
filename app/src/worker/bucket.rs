//! S3 mode in the worker: the bucket is the source of truth for everything but the connection.
//!
//! This PC keeps the connection, the crash record and a cache (the device list in
//! `devices.json`, what new devices start with in `config.json`). The device list and the
//! settings for every PC come from the bucket ([`pnk_core::bucket_list`],
//! [`pnk_core::bucket_settings`]): looked at on a thread at start, on Refresh, after Settings is
//! saved and on a switch to S3 mode, so a slow or absent bucket never holds up the window.
//! What the bucket says replaces the cache; a device the bucket doesn't show stays in it.

use pnk_core::bucket_list;
use pnk_core::bucket_settings::{self, BucketSettings, Loaded, WriteError};
use pnk_core::state::NewDeviceDefaults;
use pnk_core::store::find::{self, Discovery};
use pnk_core::store::{self, Tag};

use super::*;

/// How long a plugged-in device waits for the bucket's list before it is matched against the
/// list this PC has.
const WAIT_FOR_LIST: Duration = Duration::from_secs(15);

/// Where the look at the bucket stands.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum BucketStatus {
    /// Local mode, or S3 mode with no connection yet.
    #[default]
    NotSet,
    Looking,
    /// The list and the settings are the bucket's.
    Ready,
    /// The bucket can't be used now: the list is what this PC last saw, and the settings can't
    /// be changed.
    Unavailable {
        state: FolderState,
        detail: String,
    },
}

/// What the window shows of the bucket.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BucketInfo {
    pub status: BucketStatus,
    /// What new devices start with, as the bucket has it (or, if it has none yet, as this PC
    /// had it). `None` until the bucket has been read.
    pub settings: Option<NewDeviceDefaults>,
    /// The settings object is in the bucket.
    pub in_bucket: bool,
    /// IDs of the devices forgotten, as of the last look.
    pub forgotten: Vec<String>,
    /// Things worth saying: a folder that can't be used, one device in two folders.
    pub problems: Vec<String>,
    /// A line for a device's page, by its ID.
    pub notes: Vec<(String, String)>,
}

/// How a save of Settings in S3 mode ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveOutcome {
    Saved,
    /// The connection is saved; the bucket's own settings were not written (and why).
    ConnectionOnly(String),
    /// Another PC changed the settings first. They have been read again.
    Conflict,
    /// The bucket already had settings of its own: they were read, nothing was overwritten.
    Loaded,
    Failed(String),
}

#[derive(Default)]
pub(super) struct BucketCtx {
    conn: Option<S3Config>,
    seq: u64,
    scanning: bool,
    status: BucketStatus,
    /// The settings as read, with the tag they must still have to be replaced (`None`: the bucket
    /// had no settings object).
    settings: Option<(BucketSettings, Option<Tag>)>,
    problems: Vec<String>,
    /// About devices whose record is missing or damaged, from the last look.
    notes: Vec<(String, String)>,
}

/// What the thread that looked at the bucket found.
pub(super) struct ScanResult {
    seq: u64,
    outcome: Result<Scanned, (FolderState, String)>,
}

struct Scanned {
    settings: Result<Option<Loaded>, String>,
    discovery: Result<Discovery, String>,
}

/// A device's record as the bucket would hold it, as text: to tell whether it changed.
pub(super) fn record_text(d: &KnownDevice) -> String {
    let mut v = serde_json::to_value(d).unwrap_or_default();
    if let Some(o) = v.as_object_mut() {
        o.remove("backup_dir");
        o.remove("s3");
    }
    v.to_string()
}

/// Looks at the bucket: reachable, allowed, supported; its settings; its devices. Reads only
/// (the server's self-test writes and removes a small probe object).
fn scan_bucket(conn: &S3Config) -> Result<Scanned, (FolderState, String)> {
    let dest = Destination::S3(conn.clone());
    let store = dest
        .open()
        .map_err(|e| (FolderState::Unusable, s3::open_failure(&e).text()))?;
    let (state, detail) = s3::s3_folder_state(&store.root_exists(), false);
    if state != FolderState::Present {
        return Err((state, detail));
    }
    Ok(Scanned {
        settings: bucket_settings::read(&*store).map_err(|e| store::explain(&e)),
        discovery: find::discover_in(&*store, &dest),
    })
}

impl Worker {
    /// The connection of S3 mode (its settings' S3 server, whose folder is the base folder).
    /// `None` in Local mode, or while it is not set up.
    pub(super) fn connection(&self) -> Option<S3Config> {
        if self.mode != Mode::S3 {
            return None;
        }
        self.state.config().default_s3
    }

    fn bucket_info(&self) -> BucketInfo {
        let mut notes = self.bucket.notes.clone();
        if let Some(base) = &self.bucket.conn {
            for (id, text) in bucket_list::not_under_base(&self.registry, base) {
                if !notes.iter().any(|(i, _)| *i == id) {
                    notes.push((id, text));
                }
            }
        }
        BucketInfo {
            status: self.bucket.status.clone(),
            settings: self
                .bucket
                .settings
                .as_ref()
                .map(|(s, _)| s.new_devices.clone()),
            in_bucket: self
                .bucket
                .settings
                .as_ref()
                .is_some_and(|(_, t)| t.is_some()),
            forgotten: self
                .bucket
                .settings
                .as_ref()
                .map(|(s, _)| s.forgotten.clone())
                .unwrap_or_default(),
            problems: self.bucket.problems.clone(),
            notes,
        }
    }

    pub(super) fn send_bucket(&self) {
        self.out.send(Event::Bucket(Box::new(self.bucket_info())));
    }

    /// Everything known about the bucket is let go (the mode changed).
    pub(super) fn reset_bucket(&mut self) {
        self.bucket = BucketCtx::default();
        self.pushed.clear();
        self.pushed_ready = false;
    }

    /// Look at the bucket again, on a thread: the answer comes back through
    /// [`Worker::drain_bucket`]. Only in S3 mode.
    pub(super) fn refresh_bucket(&mut self) {
        if self.mode != Mode::S3 {
            return;
        }
        // The list on this PC is what the bucket's is compared with from here on.
        if !self.pushed_ready {
            self.pushed_ready = true;
            for d in self.registry.devices() {
                if d.s3.is_some() {
                    self.pushed.insert(d.id.clone(), record_text(d));
                }
            }
        }
        let Some(conn) = self.connection() else {
            self.bucket.conn = None;
            self.bucket.scanning = false;
            self.bucket.status = BucketStatus::NotSet;
            self.send_bucket();
            return;
        };
        if self.bucket.conn.as_ref() != Some(&conn) {
            // Another bucket: what was read from the old one is not this one's.
            self.bucket.settings = None;
            self.bucket.problems.clear();
            self.bucket.notes.clear();
        }
        self.bucket.conn = Some(conn.clone());
        self.bucket.seq += 1;
        self.bucket.scanning = true;
        self.bucket.status = BucketStatus::Looking;
        // Made again, so a changed credentials file is read.
        self.forget_stores();
        let (seq, tx, wake) = (self.bucket.seq, self.bg_tx.clone(), self.wake.clone());
        let spawned = std::thread::Builder::new()
            .name("bucket-list".into())
            .spawn(move || {
                let outcome = scan_bucket(&conn);
                let _ = tx.send(ScanResult { seq, outcome });
                let _ = wake.send(Command::BucketPoll);
            });
        if let Err(e) = spawned {
            warn!("Could not start the thread that looks in the bucket: {e}");
            self.bucket.scanning = false;
            self.bucket.status = BucketStatus::Unavailable {
                state: FolderState::Unusable,
                detail: e.to_string(),
            };
        }
        self.send_bucket();
    }

    /// Take in what the thread that looked at the bucket has found, if it has finished.
    pub(super) fn drain_bucket(&mut self) {
        while let Ok(r) = self.bg_rx.try_recv() {
            self.bucket_scanned(r);
        }
    }

    /// A plugged-in device is matched against the bucket's list, not the cache: wait a little
    /// for the look at the bucket if it is still running.
    pub(super) fn wait_for_bucket(&mut self) {
        let deadline = Instant::now() + WAIT_FOR_LIST;
        while self.mode == Mode::S3 && self.bucket.scanning {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.bg_rx.recv_timeout(left) {
                Ok(r) => self.bucket_scanned(r),
                Err(_) => break,
            }
        }
    }

    fn bucket_scanned(&mut self, r: ScanResult) {
        if self.mode != Mode::S3 || r.seq != self.bucket.seq {
            return;
        }
        self.bucket.scanning = false;
        self.bucket.problems.clear();
        self.bucket.notes.clear();
        let Some(base) = self.bucket.conn.clone() else {
            return;
        };
        let scanned = match r.outcome {
            Ok(s) => s,
            Err((state, detail)) => {
                warn!("The bucket can't be used now: {detail}");
                self.bucket.status = BucketStatus::Unavailable { state, detail };
                self.send_bucket();
                return;
            }
        };
        let mut forgotten = Vec::new();
        match scanned.settings {
            Ok(Some(l)) => {
                forgotten = l.settings.forgotten.clone();
                self.cache_settings(&l.settings.new_devices);
                self.bucket.settings = Some((l.settings, Some(l.tag)));
            }
            Ok(None) => {
                // None in the bucket yet: what this PC had for new devices is where it starts.
                let start = BucketSettings {
                    new_devices: self.state.config().new_devices,
                    ..BucketSettings::default()
                };
                self.bucket.settings = Some((start, None));
            }
            Err(e) => {
                warn!("{e}");
                self.bucket.problems.push(e);
                self.bucket.settings = None;
            }
        }
        match scanned.discovery {
            Err(e) => {
                warn!("The devices in the bucket could not be listed: {e}");
                let state = if e.starts_with("No network") {
                    FolderState::Offline
                } else {
                    FolderState::Unusable
                };
                self.bucket.status = BucketStatus::Unavailable { state, detail: e };
            }
            Ok(found) => {
                self.bucket.status = BucketStatus::Ready;
                // A change made here that didn't reach the bucket goes first, so the bucket's
                // older record doesn't take its place.
                self.push_records();
                let before = self.registry.devices().to_vec();
                let applied = bucket_list::apply(
                    &mut self.registry,
                    &base,
                    &found.found,
                    &forgotten,
                    &pnk_core::utc_stamp(),
                    &still_holds,
                );
                for d in &found.found {
                    match &d.backup.device {
                        Some(rec) => {
                            self.pushed
                                .insert(d.backup.device_id.clone(), record_text(rec));
                        }
                        None => {
                            self.pushed.remove(&d.backup.device_id);
                        }
                    }
                }
                self.bucket.problems.extend(found.problems);
                self.bucket.problems.extend(applied.problems);
                self.bucket.notes = applied.notes;
                if self.registry.devices() != before.as_slice() {
                    info!(
                        "The bucket changed the list: {} added, {} taken off",
                        applied.added.len(),
                        applied.dropped.len()
                    );
                    // Only the cache on this PC; the records are the bucket's own.
                    if let Err(e) = self.registry.save(&self.state) {
                        warn!("Could not save the list of known devices: {e}");
                    }
                    self.publish();
                    self.check_stores();
                }
            }
        }
        self.send_bucket();
    }

    /// What new devices start with is kept on this PC only as a copy of the bucket's, to show
    /// before the bucket has been read.
    fn cache_settings(&self, new_devices: &NewDeviceDefaults) {
        let mut c = self.state.config();
        if c.new_devices == *new_devices {
            return;
        }
        c.new_devices = new_devices.clone();
        if let Err(e) = self.state.save_config(&c) {
            warn!("Could not keep a copy of the bucket's settings: {e}");
        }
    }

    /// A device whose backup is in a bucket has its record written there when it has changed (the folder
    /// of a device that has not been claimed yet has nothing to hold a record: the first sync
    /// writes it). Only while the bucket can be reached; a failure is said once.
    pub(super) fn push_records(&mut self) {
        // (Devices that back up to a bucket are only in S3 mode's list, bar a list from an
        // earlier version that has not been moved.)
        if matches!(self.bucket.status, BucketStatus::Unavailable { .. }) {
            return;
        }
        let devices = self.registry.devices().to_vec();
        let mut failed = None;
        for d in &devices {
            let Some(dest @ Destination::S3(_)) = d.destination() else {
                continue;
            };
            let text = record_text(d);
            if self.pushed.get(&d.id) == Some(&text) {
                continue;
            }
            let Ok(store) = self.store_for(&dest) else {
                continue;
            };
            match owner::read_in(&*store) {
                OwnerState::Owned(m) if m.device_id == d.id => {}
                _ => continue,
            }
            match pnk_core::folder_record::write_device_in(&*store, d) {
                Ok(_) => {
                    self.pushed.insert(d.id.clone(), text);
                }
                Err(e) => {
                    let why = store::explain(&e);
                    warn!(
                        "Could not write the record of \"{}\" into {}: {why}",
                        d.name,
                        dest.label().display()
                    );
                    failed.get_or_insert(format!(
                        "Could not write the record of \"{}\" into the bucket ({why}). It is tried again at the next change.",
                        d.name
                    ));
                }
            }
        }
        if let Some(why) = failed {
            self.notice(Err(why));
        }
    }

    /// Why a device's settings can't be changed now: in S3 mode they are the bucket's, so the
    /// bucket must be there.
    pub(super) fn bucket_blocks_changes(&self, id: &str) -> Option<String> {
        if self.mode != Mode::S3 {
            return None;
        }
        let d = self.registry.get(id)?;
        d.s3.as_ref()?;
        match &self.bucket.status {
            BucketStatus::Unavailable { state, detail } => Some(format!(
                "{}. The device's settings are kept in the bucket, so they can be changed once it can be reached.",
                if detail.is_empty() {
                    unavailable_text(*state)
                } else {
                    detail.clone()
                }
            )),
            _ => None,
        }
    }

    /// Why a device's place can't be what was chosen in S3 mode: it must be a folder directly
    /// inside the base folder.
    pub(super) fn s3_place_problem(&self, dest: &Destination) -> Option<String> {
        let Destination::S3(c) = dest else {
            return Some("In S3 mode a backup goes to the bucket.".into());
        };
        let Some(base) = self.connection() else {
            return Some("The S3 server isn't set up yet. Set it up in Settings first.".into());
        };
        if pnk_core::store::dest::directly_under(&base, c).is_none() {
            return Some(format!(
                "In S3 mode a device's folder goes directly inside the base folder ({}). Choose a folder name there.",
                s3::address(&base)
            ));
        }
        None
    }

    /// Forget in S3 mode: the ID goes on the bucket's list of forgotten devices (so no PC lists
    /// the device), written so that nothing another PC wrote is lost.
    pub(super) fn forget_in_bucket(&mut self, id: &str) -> Result<(), String> {
        let Some(conn) = self.connection() else {
            return Err("The S3 server isn't set up yet.".into());
        };
        if let BucketStatus::Unavailable { state, detail } = &self.bucket.status {
            return Err(format!(
                "Not forgotten: {} A forgotten device is remembered in the bucket, so it can be forgotten once the bucket can be reached.",
                if detail.is_empty() {
                    unavailable_text(*state)
                } else {
                    detail.clone()
                }
            ));
        }
        let store = self
            .store_for(&Destination::S3(conn))
            .map_err(|e| s3::open_failure(&e).text())?;
        let id = id.to_string();
        let loaded = bucket_settings::update(&*store, &self.start_settings(), &move |s| {
            if s.forgotten.contains(&id) {
                false
            } else {
                s.forgotten.push(id.clone());
                true
            }
        })
        .map_err(|e| format!("Not forgotten: {}", store::explain(&e)))?;
        self.bucket.settings = Some((loaded.settings, Some(loaded.tag)));
        Ok(())
    }

    /// What a bucket with no settings yet starts from: what this PC had for new devices.
    fn start_settings(&self) -> BucketSettings {
        match &self.bucket.settings {
            Some((s, _)) => s.clone(),
            None => BucketSettings {
                new_devices: self.state.config().new_devices,
                ..BucketSettings::default()
            },
        }
    }

    /// "Save settings" in S3 mode: the connection is already saved on this PC (and is in the
    /// config); this writes what new devices start with into the bucket, only if the bucket's
    /// settings are as they were read. A bucket that already has settings of its own, met for
    /// the first time with a new connection, is not overwritten: its settings are read instead.
    pub(super) fn save_bucket_settings(
        &mut self,
        new_devices: NewDeviceDefaults,
        connection_changed: bool,
    ) {
        let outcome = self.write_bucket_settings(new_devices, connection_changed);
        info!("Saved the settings in the bucket: {outcome:?}");
        self.out.send(Event::BucketSaved(outcome));
        if connection_changed {
            self.bucket.settings = None;
            self.refresh_bucket();
        } else {
            self.send_bucket();
        }
    }

    fn write_bucket_settings(
        &mut self,
        new_devices: NewDeviceDefaults,
        connection_changed: bool,
    ) -> SaveOutcome {
        let Some(conn) = self.connection() else {
            return SaveOutcome::Failed("The S3 server isn't set up yet.".into());
        };
        // Fresh: a changed credentials file or server is used.
        self.forget_stores();
        let store = match Destination::S3(conn).open() {
            Ok(s) => s,
            Err(e) => return SaveOutcome::ConnectionOnly(s3::open_failure(&e).text()),
        };
        let (mut current, tag) = match (connection_changed, &self.bucket.settings) {
            (false, Some((s, t))) => (s.clone(), t.clone()),
            _ => match bucket_settings::read(&*store) {
                Ok(Some(l)) => {
                    // The bucket has settings of its own: they are not replaced by what was
                    // typed for another one.
                    self.cache_settings(&l.settings.new_devices);
                    self.bucket.settings = Some((l.settings, Some(l.tag)));
                    return SaveOutcome::Loaded;
                }
                Ok(None) => (BucketSettings::default(), None),
                Err(e) => return SaveOutcome::ConnectionOnly(store::explain(&e)),
            },
        };
        current.new_devices = new_devices;
        match bucket_settings::write(&*store, &current, tag.as_ref()) {
            Ok(tag) => {
                self.cache_settings(&current.new_devices);
                self.bucket.settings = Some((current, Some(tag)));
                SaveOutcome::Saved
            }
            Err(WriteError::Conflict) => {
                // Theirs counts; read it so the window can show it.
                match bucket_settings::read(&*store) {
                    Ok(Some(l)) => {
                        self.cache_settings(&l.settings.new_devices);
                        self.bucket.settings = Some((l.settings, Some(l.tag)));
                        SaveOutcome::Conflict
                    }
                    Ok(None) => {
                        SaveOutcome::Failed("The settings in the bucket changed. Try again.".into())
                    }
                    Err(e) => SaveOutcome::Failed(store::explain(&e)),
                }
            }
            Err(WriteError::Other(e)) => SaveOutcome::Failed(store::explain(&e)),
        }
    }

    /// "Find my backups" and "Add an existing backup…" in S3 mode: look in the base folder again.
    /// A forgotten device is offered back; with `apply`, the ones found are taken off the
    /// forgotten list and added.
    pub(super) fn bucket_find(&mut self, apply: bool) -> Found {
        let mut found = Found {
            apply,
            ..Found::default()
        };
        let Some(conn) = self.connection() else {
            found
                .problems
                .push("The S3 server isn't set up yet.".into());
            return found;
        };
        let dest = Destination::S3(conn.clone());
        let discovery = match find::discover(&dest) {
            Ok(d) => d,
            Err(e) => {
                found.problems.push(e);
                return found;
            }
        };
        found.problems.extend(discovery.problems.clone());
        let forgotten = self
            .store_for(&dest)
            .ok()
            .and_then(|s| bucket_settings::read(&*s).ok().flatten())
            .map(|l| l.settings.forgotten)
            .unwrap_or_default();
        let back: Vec<String> = discovery
            .found
            .iter()
            .filter(|d| forgotten.contains(&d.backup.device_id))
            .map(|d| d.backup.device_id.clone())
            .collect();
        let now = pnk_core::utc_stamp();
        let applied = if apply {
            if !back.is_empty() {
                match self.unforget(&dest, &back) {
                    Ok(()) => {}
                    Err(e) => {
                        found.problems.push(e);
                        return found;
                    }
                }
            }
            let applied = bucket_list::apply(
                &mut self.registry,
                &conn,
                &discovery.found,
                &[],
                &now,
                &still_holds,
            );
            self.bucket.notes = applied.notes.clone();
            self.bucket.problems = applied.problems.clone();
            if let Err(e) = self.registry.save(&self.state) {
                warn!("Could not save the list of known devices: {e}");
            }
            applied
        } else {
            // Only a look: the real list is not touched.
            let mut copy = Registry::new(self.registry.devices().to_vec());
            bucket_list::apply(&mut copy, &conn, &discovery.found, &[], &now, &still_holds)
        };
        let back_names: Vec<String> = discovery
            .found
            .iter()
            .filter(|d| back.contains(&d.backup.device_id))
            .map(|d| d.backup.device_name.clone())
            .collect();
        found.added = applied
            .added
            .into_iter()
            .map(|n| {
                if !apply && back_names.contains(&n) {
                    format!("{n} (forgotten earlier)")
                } else {
                    n
                }
            })
            .collect();
        found.already_known = applied.already_known;
        found.problems.extend(applied.problems);
        found
    }

    /// Take `ids` off the bucket's list of forgotten devices.
    fn unforget(&mut self, dest: &Destination, ids: &[String]) -> Result<(), String> {
        let store = self
            .store_for(dest)
            .map_err(|e| s3::open_failure(&e).text())?;
        let ids = ids.to_vec();
        let loaded = bucket_settings::update(&*store, &self.start_settings(), &move |s| {
            let before = s.forgotten.len();
            s.forgotten.retain(|f| !ids.contains(f));
            s.forgotten.len() != before
        })
        .map_err(|e| store::explain(&e))?;
        self.bucket.settings = Some((loaded.settings, Some(loaded.tag)));
        Ok(())
    }

    /// The sentence "Add an existing backup…" ends with, in S3 mode.
    pub(super) fn bucket_add_existing(&mut self) {
        let found = self.bucket_find(true);
        let mut parts = Vec::new();
        if !found.added.is_empty() {
            parts.push(format!(
                "Added {} backup{}: {}.",
                found.added.len(),
                if found.added.len() == 1 { "" } else { "s" },
                found.added.join(", ")
            ));
        }
        if !found.already_known.is_empty() {
            parts.push(format!(
                "Already known: {}.",
                found.already_known.join(", ")
            ));
        }
        parts.extend(found.problems.iter().map(|p| format!("{p}.")));
        let n = found.added.len() + found.already_known.len();
        info!("Looked for backups in the bucket: {}", parts.join(" "));
        self.notice(if n == 0 && found.problems.is_empty() {
            Err(format!(
                "No backup found directly inside the base folder. A backup has a {} file.",
                owner::MARKER
            ))
        } else if n == 0 {
            Err(parts.join(" "))
        } else {
            Ok(parts.join(" "))
        });
        self.publish();
        self.send_bucket();
        self.check_stores();
    }
}

/// Whether `place` still holds the backup of the device `id`.
fn still_holds(place: &Destination, id: &str) -> bool {
    place
        .open()
        .is_ok_and(|s| matches!(owner::read_in(&*s), OwnerState::Owned(m) if m.device_id == id))
}

/// What the list is, said at the top of it, when the bucket can't be used.
pub fn unavailable_text(state: FolderState) -> String {
    match state {
        FolderState::Offline => "No network: the bucket can't be reached.".into(),
        FolderState::NoAccess => "No access: the server refused these credentials.".into(),
        FolderState::ServerUnsupported => "This server can't be used.".into(),
        FolderState::Missing => "The bucket was not found.".into(),
        _ => "The bucket can't be used.".into(),
    }
}
