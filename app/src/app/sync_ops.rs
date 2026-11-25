//! Scans and syncs: turning worker events into the window's state, and clicks into commands.
use super::*;

impl App {
    pub(super) fn clear_scan(&mut self) {
        if self.scan.take().is_some() {
            self.send(Command::DiscardPlan);
            // Messages about the old scan or sync no longer apply.
            self.notice = None;
            self.error = None;
        }
        self.skip_list = None;
        self.filter_dirty = true;
    }

    pub(super) fn load_scan(&mut self, done: ScanDone) {
        let s = &done.summary;
        let mut msg = format!(
            "Scan complete: {} on the device. {} new, {} changed, {} already backed up.",
            format::files(s.device_files),
            format::count(s.new),
            format::count(s.mismatch),
            format::count(s.verified)
        );
        if s.issues > 0 {
            msg += &format!(
                " {} can't be backed up: see Problems.",
                format::plural(s.issues, "item", "items")
            );
        }
        if s.outside > 0 {
            msg += &format!(
                " {} in the backup {} not in what's backed up now; {} stay{} where {}.",
                format::files(s.outside),
                format::pick(s.outside, "is", "are"),
                format::pick(s.outside, "it", "they"),
                format::pick(s.outside, "s", ""),
                format::pick(s.outside, "it is", "they are"),
            );
        }
        if s.asides > 0 {
            msg += &if s.asides == 1 {
                " 1 old copy is kept beside its file.".to_string()
            } else {
                format!(
                    " {} old copies are kept beside their files.",
                    format::count(s.asides)
                )
            };
        }
        if s.removed > 0 {
            let n = format::files(s.removed);
            let archive = &done.plan.archive_name;
            msg += &match (done.plan.folder_blocked(), done.plan.deleted_files) {
                (Some(_), _) => format!(" {n} no longer on the device."),
                (None, DeletedFiles::Archive) => format!(
                    " {n} no longer on the device: Start sync moves {} into \"{archive}\".",
                    format::pick(s.removed, "it", "them")
                ),
                (None, DeletedFiles::RecycleBin) => format!(
                    " {n} no longer on the device: Start sync sends {} to the {}.",
                    format::pick(s.removed, "it", "them"),
                    pnk_core::platform::BIN_NAME
                ),
                (None, DeletedFiles::LeaveInPlace) => {
                    format!(
                        " {n} no longer on the device; {} stay{} in the backup folder.",
                        format::pick(s.removed, "it", "they"),
                        format::pick(s.removed, "s", "")
                    )
                }
            };
        }
        self.notice = Some(msg);
        self.error = None;
        let rows = done.plan.entries.iter().map(Row::new).collect();
        let gone = done.plan.removed.iter().map(|_| Gone::Waiting).collect();
        let jpg_failed = done.plan.jpg.failed.clone();
        let jpg_made = done
            .plan
            .made
            .iter()
            .map(|m| (m.jpg.clone(), m.heic.clone()))
            .collect();
        self.scan = Some(ScanView {
            plan_id: done.plan_id,
            plan: done.plan,
            rows,
            gone,
            backup_files: s.backup_files,
            copied: 0,
            archived: 0,
            jpg_failed,
            jpg_made,
            jpg_done: 0,
        });
        self.skip_list = Some(done.skip_list);
        self.filter_dirty = true;
    }

    pub(super) fn on_sync_event(&mut self, ev: SyncEvent) {
        let Some(scan) = self.scan.as_mut() else {
            return;
        };
        // Byte totals for the progress panel: a file that has ended, however it ended, counts
        // as done at its reported size.
        if let Busy::Syncing(v) = &mut self.busy {
            let ended = match &ev {
                SyncEvent::Copied { entry, bytes, .. } => {
                    v.copied_bytes += bytes;
                    Some(*entry)
                }
                SyncEvent::AlreadyBackedUp { entry, .. }
                | SyncEvent::Skipped { entry }
                | SyncEvent::Failed { entry, .. } => Some(*entry),
                SyncEvent::FileStarted { .. } | SyncEvent::Progress { .. } => None,
            };
            if let Some(entry) = ended {
                v.finished_bytes += scan
                    .plan
                    .entries
                    .get(entry)
                    .and_then(|e| e.size)
                    .unwrap_or(0);
                v.bytes = 0;
            }
        }
        match ev {
            SyncEvent::FileStarted {
                entry,
                ordinal,
                total,
            } => {
                if let Busy::Syncing(v) = &mut self.busy {
                    v.entry = Some(entry);
                    v.ordinal = ordinal;
                    v.total = total;
                    v.bytes = 0;
                    v.file_size = scan.plan.entries.get(entry).and_then(|e| e.size);
                    v.speed = 0.0;
                }
            }
            SyncEvent::Progress {
                bytes,
                total,
                bytes_per_sec,
                ..
            } => {
                if let Busy::Syncing(v) = &mut self.busy {
                    v.bytes = bytes;
                    v.file_size = total.or(v.file_size);
                    v.speed = bytes_per_sec;
                }
            }
            SyncEvent::Copied {
                entry,
                target,
                old_copy,
                ..
            } => {
                let replaced =
                    matches!(&self.busy, Busy::Syncing(v) if v.replacing.contains(&entry));
                // A changed file's new copy takes the old one's place: the backup holds the
                // same number of files.
                // (An old copy renamed beside it stays in the folder, so that one is one more.)
                if (!replaced && old_copy.is_none())
                    || matches!(old_copy, Some(OldCopy::RenamedAside(_)))
                {
                    scan.copied += 1;
                }
                set_backed_up(scan, entry, target.as_str());
            }
            SyncEvent::AlreadyBackedUp { entry, target } => {
                set_backed_up(scan, entry, target.as_str())
            }
            SyncEvent::Skipped { entry } => {
                if let (Some(row), Some(e)) =
                    (scan.rows.get_mut(entry), scan.plan.entries.get(entry))
                {
                    row.status = Status::Skipped;
                    row.sync_failed = false;
                    if let Some(list) = &mut self.skip_list
                        && !list.iter().any(|p| p == e.source.as_str())
                    {
                        list.push(e.source.as_str().to_string());
                        list.sort();
                    }
                }
            }
            SyncEvent::Failed { entry, error } => {
                if let (Some(row), Some(e)) =
                    (scan.rows.get_mut(entry), scan.plan.entries.get(entry))
                {
                    row.status = Status::Error(error.clone());
                    row.sync_failed = true;
                    self.error = Some(format!("{}: {error}", e.source.as_str()));
                    self.last_failed = Some(e.source.as_str().to_string());
                }
            }
        }
        if let Busy::Syncing(v) = &mut self.busy {
            v.refresh_eta();
        }
        self.filter_dirty = true;
    }

    /// The JPG step, the backfill or "Try again" says where it is.
    pub(super) fn on_jpg_event(&mut self, ev: pnk_core::jpgs::JpgEvent) {
        use pnk_core::jpgs::JpgEvent;
        match ev {
            JpgEvent::Started { done, total, heic } => {
                let view = JpgView {
                    done,
                    total,
                    name: heic,
                };
                match &mut self.busy {
                    Busy::Syncing(v) => v.jpg = Some(view),
                    Busy::Jpgs(j) => *j = view,
                    _ => {}
                }
            }
            JpgEvent::Made { heic, jpg } => {
                if let Some(scan) = self.scan.as_mut() {
                    scan.jpg_done += 1;
                    scan.jpg_failed.retain(|f| f.heic != heic);
                    scan.jpg_made.retain(|(j, _)| *j != jpg);
                    scan.jpg_made.push((jpg, heic));
                    self.filter_dirty = true;
                }
            }
            JpgEvent::Failed { heic, reason } => {
                if let Some(scan) = self.scan.as_mut() {
                    scan.jpg_done += 1;
                    scan.jpg_failed.retain(|f| f.heic != heic);
                    scan.jpg_failed.push(FailedEntry {
                        heic,
                        reason,
                        heic_size: 0,
                        heic_modified_ms: 0,
                        when: String::new(),
                        name_taken: false,
                    });
                    self.filter_dirty = true;
                }
            }
        }
    }

    /// The backfill: "Make JPG copies of HEIC photos already in the backup".
    pub(super) fn start_backfill(&mut self, id: &str) {
        info!("Make JPG copies of HEIC photos already in the backup: clicked");
        self.begin_jpg_job("Making JPG copies…");
        self.send(Command::MakeJpgs { id: id.to_string() });
    }

    /// "Try again" for these HEICs, or for every failed one.
    pub(super) fn retry_jpgs(&mut self, id: &str, heics: Option<Vec<String>>) {
        info!("Try again clicked for the JPG copies");
        self.begin_jpg_job("Trying again to make JPG copies…");
        self.send(Command::RetryJpgs {
            id: id.to_string(),
            heics,
        });
    }

    fn begin_jpg_job(&mut self, notice: &str) {
        self.flags.sync.cancel.store(false, Ordering::Relaxed);
        self.notice = Some(notice.to_string());
        self.error = None;
        self.busy = Busy::Jpgs(JpgView::default());
    }

    pub(super) fn apply_skip_list(&mut self, list: Vec<String>) {
        if let Some(scan) = self.scan.as_mut() {
            let set: HashSet<&str> = list.iter().map(String::as_str).collect();
            for (row, e) in scan.rows.iter_mut().zip(&scan.plan.entries) {
                let listed = set.contains(e.source.as_str());
                match (&row.status, listed) {
                    (Status::Skipped, false) => {
                        // Back to what the scan would have said without the skip list.
                        row.status = if e.existing_size.is_some() {
                            Status::Mismatch
                        } else {
                            Status::New
                        };
                    }
                    (Status::New | Status::Mismatch, true) => row.status = Status::Skipped,
                    (Status::Error(_), true) if row.sync_failed => {
                        row.status = Status::Skipped;
                        row.sync_failed = false;
                    }
                    _ => {}
                }
            }
        }
        self.skip_list = Some(list);
        self.filter_dirty = true;
    }
    pub(super) fn start_scan(&mut self) {
        // The phone stopped answering last time: open it afresh, and scan when it is open.
        if self.reopen_dropped(true) {
            self.notice = Some("Opening the device again, then scanning…".into());
            return;
        }
        let Some(folder) = self.active().and_then(|(_, d, _)| d.backup_dir.clone()) else {
            warn!("Scan not started: no device is ready, or it has no backup folder");
            self.error = Some("Choose a backup folder for this device first.".into());
            return;
        };
        if !folder.is_absolute() {
            warn!(
                "Scan not started: \"{}\" is not a full path",
                folder.display()
            );
            self.error = Some(format!(
                "Enter the backup folder's full path, such as {}.",
                format::example_folder()
            ));
            return;
        }
        info!("Scan clicked: backup folder {}", folder.display());
        self.clear_scan();
        self.notice = None;
        self.error = None;
        self.flags.scan_cancel.store(false, Ordering::Relaxed);
        self.busy = Busy::Scanning(None);
        self.send(Command::Scan(folder));
    }

    pub(super) fn cancel_scan(&mut self) {
        info!("Cancel scan clicked");
        self.flags.scan_cancel.store(true, Ordering::Relaxed);
        self.notice = Some("Cancelling the scan…".into());
    }

    pub(super) fn pending_items(&self) -> Vec<SyncItem> {
        let Some(scan) = &self.scan else {
            return Vec::new();
        };
        let items = scan
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.is_pending())
            .map(|(entry, _)| SyncItem {
                entry,
                mode: CopyMode::NoReplace,
            })
            .collect();
        defer_last(items, |entry| {
            scan.plan
                .entries
                .get(entry)
                .is_some_and(|e| self.deferred.contains(e.source.as_str()))
        })
    }

    /// How many files deleted from the device the next full sync would archive.
    pub(super) fn archive_count(&self) -> usize {
        match &self.scan {
            Some(scan) if self.archive_on() && scan.plan.folder_blocked().is_none() => scan
                .gone
                .iter()
                .filter(|g| matches!(g, Gone::Waiting))
                .count(),
            _ => 0,
        }
    }

    /// Start sync: copy every pending file, and archive files deleted from the device (asking
    /// first if there are a lot of them).
    pub(super) fn start_full_sync(&mut self) {
        let items = self.pending_items();
        let count = self.archive_count();
        info!(
            "Start sync clicked: {} to copy, {count} deleted files to move ({})",
            items.len(),
            self.deleted_mode().did()
        );
        // A changed file's old copy is moved too, so it waits for the same OK.
        let (needs_ok, blocking, changed) = match &self.scan {
            Some(s) => (
                !s.plan.cautions.is_empty(),
                s.plan.cautions.iter().any(|c| c.blocks_all()),
                items
                    .iter()
                    .filter(|i| {
                        s.plan
                            .entries
                            .get(i.entry)
                            .is_some_and(|e| e.status == Status::Mismatch)
                    })
                    .count(),
            ),
            None => (false, false, 0),
        };
        if (count > 0 && needs_ok) || (changed > 0 && blocking) {
            info!("Asking before moving {count} deleted files and replacing {changed}");
            self.confirm = Some(Confirm::Archive {
                items,
                count,
                changed,
            });
        } else {
            self.start_sync(items, count > 0, false);
        }
    }

    /// `confirmed`: the user said yes to the reasons archiving asked first.
    pub(super) fn start_sync(&mut self, items: Vec<SyncItem>, archive: bool, confirmed: bool) {
        let Some(scan) = &self.scan else { return };
        // A sync with nothing to copy or move can still have JPG copies to make.
        if items.is_empty() && !archive && scan.jpg_pending() == 0 {
            return;
        }
        let plan_id = scan.plan_id;
        self.flags.sync.cancel.store(false, Ordering::Relaxed);
        self.flags.sync.skip_current.store(false, Ordering::Relaxed);
        self.flags
            .sync
            .skip_entry
            .store(usize::MAX, Ordering::Relaxed);
        self.last_failed = None;
        self.notice = Some(if items.is_empty() && !archive {
            "Making JPG copies…".to_string()
        } else if items.is_empty() {
            "Moving the files deleted from the device…".to_string()
        } else {
            format!("Copying {}…", format::files(items.len()))
        });
        self.error = None;
        let total_bytes = items
            .iter()
            .filter_map(|i| scan.plan.entries.get(i.entry)?.size)
            .sum();
        self.busy = Busy::Syncing(SyncView {
            total: items.len(),
            replacing: items
                .iter()
                .filter(|i| i.mode == CopyMode::Replace)
                .map(|i| i.entry)
                .collect(),
            total_bytes,
            started: Some(std::time::Instant::now()),
            ..Default::default()
        });
        self.send(Command::Sync {
            plan_id,
            items,
            archive,
            confirmed,
        });
    }

    /// Skip the file the window shows as being copied (not whatever the copy has reached by
    /// the time the request is seen).
    pub(super) fn skip_current(&mut self) {
        if let Busy::Syncing(SyncView {
            entry: Some(entry), ..
        }) = &self.busy
        {
            info!("Skip current clicked (or S pressed) for entry {entry}");
            self.flags.sync.request_skip(*entry);
            self.notice = Some("Skipping the current file…".into());
        }
    }

    pub(super) fn stop_sync(&mut self) {
        info!("Stop sync clicked");
        self.flags.sync.cancel.store(true, Ordering::Relaxed);
        self.notice = Some("Stopping…".into());
    }

    pub(super) fn row_action(&mut self, action: RowAction) {
        let Some(scan) = &self.scan else { return };
        let plan_id = scan.plan_id;
        match action {
            RowAction::SyncOne(i) => {
                let (Some(row), Some(e)) = (scan.rows.get(i), scan.plan.entries.get(i)) else {
                    return;
                };
                // A file backed up at scan time and untouched since: copying it again means
                // replacing that backup, so ask first.
                if e.status == Status::Verified && row.status == Status::Verified {
                    info!("Copy this file again… chosen: {}", e.source);
                    self.confirm = Some(Confirm::Recopy(i));
                } else {
                    info!("Sync this file chosen: {}", e.source);
                    let item = SyncItem {
                        entry: i,
                        mode: CopyMode::NoReplace,
                    };
                    self.start_sync(vec![item], false, false);
                }
            }
            RowAction::ToggleSkip(i) => {
                let (Some(row), Some(e)) = (scan.rows.get(i), scan.plan.entries.get(i)) else {
                    return;
                };
                if row.status == Status::Skipped {
                    info!("Remove from skip list chosen: {}", e.source);
                    let path = e.source.as_str().to_string();
                    self.send(Command::SkipRemove { plan_id, path });
                } else {
                    info!("Skip this file chosen: {}", e.source);
                    self.send(Command::SkipAdd { plan_id, entry: i });
                }
            }
        }
    }

    /// `alt`: the dialog's second choice (only for Archive: sync without archiving).
    pub(super) fn confirmed(&mut self, c: Confirm, alt: bool) {
        let Some(scan) = &self.scan else { return };
        let plan_id = scan.plan_id;
        match c {
            Confirm::ClearSkipList => {
                info!("Clear skip list confirmed");
                self.send(Command::SkipClear { plan_id })
            }
            Confirm::Recopy(i) => {
                info!("Copy again confirmed");
                let item = SyncItem {
                    entry: i,
                    mode: CopyMode::Replace,
                };
                self.start_sync(vec![item], false, false);
            }
            Confirm::Archive { items, count, .. } => {
                if alt {
                    info!("Sync without archiving chosen");
                } else {
                    info!("Sync and archive confirmed");
                }
                self.start_sync(items, !alt && count > 0, !alt)
            }
            Confirm::ClaimFolder => {
                if let Some(device_page::ClaimOffer::Refused(why)) =
                    device_page::claim_offer(&scan.plan)
                {
                    self.error = Some(format!("This folder can't be handed over: {why}."));
                    return;
                }
                info!(
                    "Use this folder for this device confirmed: {}",
                    scan.plan.root.display()
                );
                self.send(Command::ClaimFolder { plan_id })
            }
        }
    }

    pub(super) fn on_archive_event(&mut self, ev: ArchiveEvent) {
        let Some(scan) = self.scan.as_mut() else {
            return;
        };
        match ev {
            ArchiveEvent::Moved { removed, to } => {
                if let Some(g) = scan.gone.get_mut(removed) {
                    *g = Gone::Archived(to.as_str().to_string());
                    scan.archived += 1;
                }
            }
            ArchiveEvent::Binned { removed } => {
                if let Some(g) = scan.gone.get_mut(removed) {
                    *g = Gone::Binned;
                    scan.archived += 1;
                }
            }
            ArchiveEvent::Failed { removed, error } => {
                if let Some(g) = scan.gone.get_mut(removed) {
                    *g = Gone::Failed(error);
                }
            }
        }
    }

    pub(super) fn counts(&self) -> Counts {
        let mut c = Counts::default();
        let rows = self
            .scan
            .iter()
            .flat_map(|s| s.rows.iter().zip(&s.plan.entries));
        for (r, e) in rows {
            let size = e.size.unwrap_or(0);
            c.device_bytes += size;
            match r.status {
                Status::New => c.new += 1,
                Status::Mismatch => c.mismatch += 1,
                Status::Verified => {
                    c.verified += 1;
                    c.verified_bytes += size;
                }
                Status::Skipped => c.skipped += 1,
                Status::Error(_) => c.failed += 1,
            }
            if r.is_pending() {
                c.pending += 1;
                c.pending_bytes += size;
            }
        }
        c.deleted = self.scan.as_ref().map_or(0, |s| {
            s.gone.iter().filter(|g| matches!(g, Gone::Waiting)).count()
        });
        c
    }

    pub(super) fn refilter(&mut self) {
        self.filtered.clear();
        self.filter_dirty = false;
        let Some(scan) = &self.scan else { return };
        let q = self.search.trim().to_lowercase();
        for (i, r) in scan.rows.iter().enumerate() {
            let in_tab = match self.tab {
                Tab::Pending => r.is_pending(),
                Tab::New => r.status == Status::New,
                Tab::Mismatch => r.status == Status::Mismatch,
                Tab::Failed => matches!(r.status, Status::Error(_)),
                Tab::All => true,
                Tab::Deleted | Tab::JpgFailed | Tab::JpgMade => false,
            };
            if in_tab && (q.is_empty() || r.search.contains(&q)) {
                self.filtered.push(i);
            }
        }
    }
}
