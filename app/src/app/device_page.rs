//! The top bar, and the chosen device's page: how it was recognised, its backup folder, the last
//! sync, the Scan and Start sync buttons and the tabs.

use pnk_core::owner::{self, OwnerState};
use pnk_core::signals::{DeviceKind, View};

use super::settings::deleted_files_combo;
use super::*;
use crate::questions::Question;

/// "Recognised by its serial number.", and how the other ways a device became known read.
pub(super) fn recognition_text(how: How) -> String {
    match how {
        How::Recognised(by) => format!("Recognised by {}.", by.describe()),
        How::Answered => "You said this is the device.".into(),
        How::Linked => "You said it's one you already back up.".into(),
        How::Created => "Set up just now.".into(),
        How::Reconnected => "Recognised, and reconnected.".into(),
    }
}

/// "Last sync 17 Nov 2026 · 4,865 files · 9.3 GB", or that there has been none.
pub(super) fn sync_line(d: &KnownDevice) -> String {
    match &d.last_sync {
        Some(s) => format!(
            "Last sync {} · {} · {}",
            format::date(&s.when),
            format::files(s.files as usize),
            format::bytes(s.bytes)
        ),
        None => "Not synced yet".into(),
    }
}

/// What to do about a backup folder that belongs to another device or another view of this one.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ClaimOffer {
    /// The folder may be handed to this device, after the user confirms.
    Offer(String),
    /// The marker records another view of the device: handing it over is refused. Why.
    Refused(String),
}

/// What a backup folder's owner allows for the device `device_key` read through `view`. A
/// folder whose marker records a different view (the files look different that way) is never
/// offered, whoever owns it.
pub(super) fn claim_offer_for(
    state: &OwnerState,
    device_key: &str,
    view: View,
) -> Option<ClaimOffer> {
    let why = match state {
        OwnerState::Unclaimed => return None,
        OwnerState::Owned(o) => {
            if let Some(refusal) = owner::view_conflict(o, view) {
                // Another device's folder in another view, or this device's in another view.
                return owner::refusal(o, device_key, view)
                    .is_some()
                    .then_some(ClaimOffer::Refused(refusal));
            }
            owner::refusal(o, device_key, view)?
        }
        OwnerState::Unreadable(e) => {
            format!("{} in the backup folder can't be read ({e})", owner::MARKER)
        }
    };
    Some(ClaimOffer::Offer(why))
}

/// What the scan's blocked folder allows.
pub(super) fn claim_offer(plan: &ScanPlan) -> Option<ClaimOffer> {
    claim_offer_for(&plan.owner, &plan.device_key, plan.view)
}

/// What the Eject button does on a device's page: `None` when there is none (not a stick or
/// drive, not plugged in, or this system can't eject), else whether it can be clicked. It is
/// clickable only when nothing is being scanned, copied or ejected.
pub(super) fn eject_button(
    kind: DeviceKind,
    connected: bool,
    system_can_eject: bool,
    idle: bool,
) -> Option<bool> {
    (kind == DeviceKind::Storage && connected && system_can_eject).then_some(idle)
}

/// What the window shows for the open device.
enum Shown {
    Opening,
    Failed(String),
    Ask(Question),
    Setup,
    Ready(String, How),
}

impl App {
    pub(super) fn top_bar(&mut self, ui: &mut egui::Ui) {
        let idle = self.idle();
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Plug-n-Keep")
                    .size(17.0)
                    .family(theme::semibold())
                    .color(theme::TEXT),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Always available, also while scanning or copying, to watch what happens.
                let open = self.log_view.open;
                // An ID per state, as for Start and Stop sync: opening the panel changes the
                // layout, and a discard pass re-run with the same click mustn't close it again.
                let clicked = ui
                    .push_id(if open { "log-open" } else { "log-closed" }, |ui| {
                        theme::toggle(ui, open, theme::I_LOG, "Log")
                            .on_hover_text(if open {
                                "Hide the log"
                            } else {
                                "Show the log: what the app is doing, newest first"
                            })
                            .clicked()
                    })
                    .inner;
                if clicked {
                    self.log_view.open = !open;
                    debug!(
                        "Log panel {}",
                        if self.log_view.open {
                            "opened"
                        } else {
                            "closed"
                        }
                    );
                }
                ui.add_space(2.0);
                let on = self.global.is_some();
                let settings = ui
                    .push_id(if on { "settings-open" } else { "settings-closed" }, |ui| {
                        theme::toggle(ui, on, theme::I_SETTINGS, "Settings")
                            .on_hover_text("The backups folder, what new devices start with, the log level, Find my backups")
                            .clicked()
                    })
                    .inner;
                if settings {
                    if on {
                        self.global = None;
                    } else {
                        self.global = Some(GlobalForm::new(&self.config, self.backups_root()));
                        self.found = None;
                    }
                }
                ui.add_space(2.0);
                let text = if self.listing {
                    "Looking…"
                } else {
                    "Refresh"
                };
                if theme::button(ui, idle && !self.listing, theme::I_REFRESH, text, false)
                    .on_hover_text("Look for connected devices again")
                    .clicked()
                {
                    self.refresh();
                }
            });
        });
        ui.add_space(8.0);
    }

    pub(super) fn banners(&mut self, ui: &mut egui::Ui) {
        if let Some(e) = &self.error
            && theme::banner(ui, theme::Note::Error, e)
        {
            self.error = None;
        }
        if let Some(n) = &self.notice
            && theme::banner(ui, theme::Note::Info, n)
        {
            self.notice = None;
        }
    }

    /// The right-hand side: whatever is chosen in the list.
    pub(super) fn page(&mut self, ui: &mut egui::Ui, counts: &Counts) {
        self.banners(ui);
        match self.selected.clone() {
            None => self.empty_page(ui),
            Some(Sel::Known(id)) => self.device_body(ui, counts, &id, false, None),
            Some(Sel::Connected(listed)) => {
                if self.ejecting.as_deref() == Some(listed.as_str()) {
                    self.ejecting_page(ui);
                    return;
                }
                if self.ejected.as_deref() == Some(listed.as_str()) {
                    self.ejected_page(ui, &listed);
                    return;
                }
                if self.current.as_deref() != Some(listed.as_str()) {
                    self.waiting_page(ui, &listed);
                    return;
                }
                let view = match &self.device {
                    DeviceState::None | DeviceState::Opening => Shown::Opening,
                    DeviceState::Failed(e) => Shown::Failed(e.clone()),
                    DeviceState::Open(o) => match &o.stage {
                        Stage::Ask(q) => Shown::Ask(q.clone()),
                        Stage::Setup(_) => Shown::Setup,
                        Stage::Ready { device_id, how } => Shown::Ready(device_id.clone(), *how),
                    },
                };
                match view {
                    Shown::Opening => {
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                RichText::new(
                                    "Opening the device… Unlock the phone and tap Trust if it asks.",
                                )
                                .color(theme::WEAK),
                            );
                        });
                    }
                    Shown::Failed(e) => {
                        ui.add_space(8.0);
                        ui.label(RichText::new(e).color(theme::DANGER));
                        ui.add_space(6.0);
                        if ui.button("Try again").clicked() {
                            self.open_listed(&listed);
                        }
                    }
                    Shown::Ask(q) => self.question_card(ui, &q),
                    Shown::Setup => self.setup_card(ui),
                    Shown::Ready(id, how) => self.device_body(ui, counts, &id, true, Some(how)),
                }
            }
        }
    }

    fn empty_page(&self, ui: &mut egui::Ui) {
        ui.add_space(48.0);
        ui.vertical_centered(|ui| {
            theme::glyph(ui, theme::I_PHONE, 40.0, theme::BORDER);
            ui.add_space(6.0);
            ui.label(
                RichText::new("No device chosen")
                    .size(16.0)
                    .family(theme::semibold())
                    .color(theme::TEXT),
            );
            ui.label(
                RichText::new(
                    "Plug in a phone, camera or USB stick, or choose a device from the list.",
                )
                .color(theme::WEAK),
            );
        });
    }

    /// While the system ejects the stick or drive.
    fn ejecting_page(&self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(
                RichText::new("Ejecting… the system is closing the drive.").color(theme::WEAK),
            );
        });
    }

    /// After a successful eject, until the device is unplugged.
    fn ejected_page(&mut self, ui: &mut egui::Ui, listed: &str) {
        let label = self
            .devices
            .iter()
            .find(|d| d.id == listed)
            .map_or_else(|| listed.to_string(), |d| d.label.clone());
        ui.add_space(8.0);
        ui.label(
            RichText::new(label)
                .size(17.0)
                .family(theme::semibold())
                .color(theme::TEXT),
        );
        ui.label(RichText::new("Ejected. You can unplug it now.").color(theme::WEAK));
        ui.add_space(6.0);
        if ui
            .add_enabled(self.idle(), egui::Button::new("Open it again"))
            .on_hover_text("If it is still plugged in, look at it again")
            .clicked()
        {
            self.forget_ejected();
            self.select_connected(listed);
        }
    }

    /// A device that is plugged in but waits behind another.
    fn waiting_page(&mut self, ui: &mut egui::Ui, listed: &str) {
        let label = self
            .devices
            .iter()
            .find(|d| d.id == listed)
            .map_or_else(|| listed.to_string(), |d| d.label.clone());
        ui.add_space(8.0);
        ui.label(
            RichText::new(label)
                .size(17.0)
                .family(theme::semibold())
                .color(theme::TEXT),
        );
        ui.label(
            RichText::new(
                "Plugged in. It is looked at after the device before it is done; or look at it now.",
            )
            .color(theme::WEAK),
        );
        ui.add_space(6.0);
        if ui
            .add_enabled(self.idle(), egui::Button::new("Look at it now"))
            .clicked()
        {
            self.select_connected(listed);
        }
    }

    /// A known device's page: connected (`how` says how it became known) or not.
    fn device_body(
        &mut self,
        ui: &mut egui::Ui,
        counts: &Counts,
        id: &str,
        connected: bool,
        how: Option<How>,
    ) {
        let Some(d) = self.known_by_id(id).cloned() else {
            return;
        };
        let idle = self.idle();
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(&d.name)
                    .size(17.0)
                    .family(theme::semibold())
                    .color(theme::TEXT),
            );
            theme::pill(
                ui,
                if connected {
                    "Connected"
                } else {
                    "Not plugged in"
                },
                if connected { theme::GOOD } else { theme::FAINT },
            );
            // Eject: only for a stick or drive that is plugged in, and only while nothing is
            // being scanned or copied.
            let listed = self
                .current
                .clone()
                .filter(|id| connected && self.devices.iter().any(|d| &d.id == id));
            let can_eject = listed.as_ref().is_some_and(|id| {
                self.devices
                    .iter()
                    .find(|d| &d.id == id)
                    .is_some_and(pnk_core::platform::can_eject)
            });
            if let (Some(enabled), Some(id)) =
                (eject_button(d.kind, connected, can_eject, idle), listed)
                && ui
                    .add_enabled(enabled, egui::Button::new("Eject"))
                    .on_hover_text(
                        "Let the drive go safely, then unplug it. Not while scanning or copying.",
                    )
                    .clicked()
            {
                self.eject_device(&id);
            }
        });
        // How it was recognised, with the way to correct it.
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            match how {
                Some(h) => {
                    ui.label(RichText::new(recognition_text(h)).color(theme::WEAK));
                    if !matches!(h, How::Created | How::Reconnected)
                        && ui
                            .add_enabled(
                                idle,
                                egui::Button::new(
                                    RichText::new("Not this device?").color(theme::ACCENT),
                                )
                                .frame(false),
                            )
                            .on_hover_text("Take that back, and ask again which device this is")
                            .clicked()
                    {
                        info!("Not this device? clicked");
                        self.clear_scan();
                        self.send(Command::NotThisDevice {
                            device: self.current.clone().unwrap_or_default(),
                        });
                    }
                }
                None => {
                    // With the folder missing, the warning below says what can't be done.
                    let settings = if self.folder_state(&d) == FolderState::Missing {
                        ""
                    } else {
                        " You can still change its settings."
                    };
                    ui.label(
                        RichText::new(format!(
                            "Not plugged in. Last seen {}.{settings}",
                            format::date(&d.last_seen)
                        ))
                        .color(theme::WEAK),
                    );
                }
            }
        });
        self.folder_row(ui, &d);
        ui.label(RichText::new(sync_line(&d)).color(theme::WEAK));
        if let Some(text) = self
            .sizes
            .get(&d.id)
            .map(history_view::sizes_line)
            .filter(|t| !t.is_empty())
        {
            ui.label(RichText::new(text).color(theme::WEAK));
        }
        ui.add_space(8.0);
        if connected {
            self.controls(ui, counts, &d);
            self.folder_warning(ui);
            self.progress_card(ui);
        } else if matches!(self.busy, Busy::Jpgs(_)) {
            // The backfill needs only the backup folder, so it also runs for a device that
            // isn't plugged in.
            if theme::button(ui, true, theme::I_STOP, "Stop", false).clicked() {
                self.stop_sync();
            }
            ui.add_space(4.0);
            self.progress_card(ui);
        }
        self.page_tabs(ui, counts, &d.id);
        ui.add_space(2.0);
        ui.separator();
        ui.add_space(4.0);
        match self.page {
            PageTab::Settings => self.settings_tab(ui, &d),
            PageTab::Problems => self.problems_view(ui),
            PageTab::SkipList => self.skip_list_view(ui),
            PageTab::History => self.history_view(ui, &d),
            PageTab::Files => {
                if self.scan.is_some() && connected {
                    self.summary(ui, counts);
                    ui.add_space(6.0);
                    self.tabs(ui, counts);
                    ui.add_space(2.0);
                    ui.separator();
                    ui.add_space(4.0);
                    match self.tab {
                        Tab::Deleted => self.deleted_view(ui),
                        Tab::JpgFailed => self.jpg_failed_view(ui, &d.id),
                        Tab::JpgMade => self.jpg_made_view(ui),
                        _ => self.file_table(ui),
                    }
                } else if connected {
                    self.file_table(ui);
                } else {
                    ui.add_space(40.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            RichText::new("Plug the device in to scan it")
                                .size(16.0)
                                .family(theme::semibold())
                                .color(theme::TEXT),
                        );
                        if self.folder_state(&d) != FolderState::Missing {
                            ui.label(
                                RichText::new(
                                    "Its settings can be changed meanwhile, in the Settings tab.",
                                )
                                .color(theme::WEAK),
                            );
                        }
                    });
                }
            }
        }
    }

    /// Whether the device's backup folder is there. A folder the first sync has not made yet is
    /// not "missing".
    pub(super) fn folder_state(&self, d: &KnownDevice) -> FolderState {
        match self
            .folders
            .get(&d.id)
            .copied()
            .unwrap_or(FolderState::NotSet)
        {
            _ if d.offline.is_some() => FolderState::Missing,
            FolderState::Missing if d.last_sync.is_none() => FolderState::NotSet,
            other => other,
        }
    }

    /// "Backs up to <folder>", with the ways to change it, and what to do if it has gone.
    fn folder_row(&mut self, ui: &mut egui::Ui, d: &KnownDevice) {
        let idle = self.idle();
        let state = self.folder_state(d);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.label(RichText::new("Backs up to").color(theme::WEAK));
            match &d.backup_dir {
                Some(p) => {
                    ui.label(
                        RichText::new(p.display().to_string())
                            .family(theme::semibold())
                            .color(if state == FolderState::Missing {
                                theme::CAUTION
                            } else {
                                theme::TEXT
                            }),
                    );
                }
                None => {
                    ui.label(RichText::new("no folder chosen yet").color(theme::CAUTION));
                }
            }
            if ui
                .add_enabled(idle, egui::Button::new("Change…"))
                .on_hover_text("Choose another backup folder for this device")
                .clicked()
            {
                self.choose_backup_folder(d);
            }
            if state == FolderState::Present
                && let Some(p) = &d.backup_dir
                && ui.button("Open").on_hover_text("Show the folder").clicked()
            {
                self.open_backup_folder(p);
            }
        });
        if state == FolderState::Missing {
            let folder = d
                .backup_dir
                .as_deref()
                .map_or_else(String::new, |p| p.display().to_string());
            let text = format!(
                "Backup folder not found: {folder}. Nothing can be backed up without it. Plug in the drive it is on, use Find it… if it was moved or renamed, or Change… to start a new backup folder."
            );
            if theme::warning(ui, &text, "Find it…", idle) {
                self.point_to_new_place(d);
            }
        }
    }

    /// "Change…": any folder the user picks becomes the device's backup folder. The scan checks
    /// whose it is.
    fn choose_backup_folder(&mut self, d: &KnownDevice) {
        let mut dialog = rfd::FileDialog::new().set_title("Choose the backup folder");
        if let Some(dir) = d.backup_dir.as_deref().filter(|p| p.is_dir()) {
            dialog = dialog.set_directory(dir);
        } else if self.backups_root().is_dir() {
            dialog = dialog.set_directory(self.backups_root());
        }
        let Some(dir) = dialog.pick_folder() else {
            return;
        };
        let note = crate::validate::folder_note(&dir, Some(&d.id), &self.known);
        if note.blocks() {
            self.error = note.text();
            return;
        }
        info!("Backup folder chosen for \"{}\": {}", d.name, dir.display());
        self.clear_scan();
        self.send(Command::SetBackupDir {
            id: d.id.clone(),
            dir,
        });
        if let Some(text) = note.text() {
            self.notice = Some(text);
        }
    }

    /// The folder moved: the user points at its new place, and the marker must confirm it.
    pub(super) fn point_to_new_place(&mut self, d: &KnownDevice) {
        let dialog =
            rfd::FileDialog::new().set_title(format!("Where did the backup of \"{}\" go?", d.name));
        let Some(dir) = dialog.pick_folder() else {
            return;
        };
        info!("Pointing \"{}\" at {}", d.name, dir.display());
        self.clear_scan();
        self.send(Command::Relocate {
            id: d.id.clone(),
            dir,
        });
    }

    fn page_tabs(&mut self, ui: &mut egui::Ui, _counts: &Counts, device_id: &str) {
        let problems = self.scan.as_ref().map(|s| s.plan.issues.len());
        let skip = self.skip_list.as_ref().map(Vec::len);
        let files = self.scan.as_ref().map(|s| s.rows.len());
        let syncs = self.history.get(device_id).map(Vec::len);
        let tabs = [
            (PageTab::Files, "Files", files),
            (PageTab::Problems, "Problems", problems),
            (PageTab::SkipList, "Skip list", skip),
            (PageTab::History, "History", syncs),
            (PageTab::Settings, "Settings", None),
        ];
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 18.0;
            for (tab, name, n) in tabs {
                let count = n.map(format::count);
                let resp = theme::tab(ui, self.page == tab, name, count.as_deref());
                let label = match &count {
                    Some(c) => format!("{name} ({c})"),
                    None => name.to_string(),
                };
                resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, &label));
                if resp.clicked() && self.page != tab {
                    self.page = tab;
                    if tab == PageTab::History {
                        self.history_row = None;
                        self.send(Command::LoadHistory {
                            id: device_id.to_string(),
                        });
                    }
                }
            }
        });
    }

    /// The Scan and Start sync buttons.
    fn controls(&mut self, ui: &mut egui::Ui, counts: &Counts, d: &KnownDevice) {
        let idle = self.idle();
        // Each state's buttons get their own ID scope. Otherwise "Stop sync" takes the ID
        // "Start sync" had, and when egui re-runs a frame with the same input (a discard pass
        // after a layout change), the click that started the sync also stops it.
        let scope = match &self.busy {
            Busy::Idle => "controls-idle",
            Busy::Scanning(_) => "controls-scanning",
            Busy::Syncing(_) => "controls-syncing",
            Busy::Jpgs(_) => "controls-jpgs",
        };
        ui.horizontal(|ui| {
            ui.push_id(scope, |ui| match &self.busy {
                Busy::Idle => {
                    let ready = d.backup_dir.is_some() && d.offline.is_none();
                    if ui
                        .add_enabled(ready, egui::Button::new("Scan"))
                        .on_hover_text("Compare the device with the backup folder")
                        .clicked()
                    {
                        self.start_scan();
                    }
                    let pending = counts.pending;
                    let to_archive = self.archive_count();
                    let jpgs = self.scan.as_ref().map_or(0, ScanView::jpg_pending);
                    let size = if pending > 0 {
                        format!(" · {}", format::bytes(counts.pending_bytes))
                    } else {
                        String::new()
                    };
                    let jpg_text = if jpgs > 0 {
                        format!(
                            ", {} JPG {}",
                            format::count(jpgs),
                            if jpgs == 1 { "copy" } else { "copies" }
                        )
                    } else {
                        String::new()
                    };
                    let text = if to_archive > 0 {
                        let where_to = match d.settings.deleted_files {
                            DeletedFiles::RecycleBin => {
                                format!("to send to the {}", pnk_core::platform::BIN_NAME)
                            }
                            _ => "to archive".to_string(),
                        };
                        format!(
                            "Start sync ({}{size} to copy, {} {where_to}{jpg_text})",
                            format::count(pending),
                            format::count(to_archive)
                        )
                    } else {
                        format!("Start sync ({}{size}{jpg_text})", format::count(pending))
                    };
                    let blocked = self
                        .scan
                        .as_ref()
                        .is_some_and(|s| s.plan.folder_blocked().is_some());
                    let can = (pending > 0 || to_archive > 0 || jpgs > 0) && !blocked;
                    if theme::button(ui, can, theme::I_SYNC, &text, true)
                        .on_hover_text(
                            "Copy every pending file, and deal with files deleted from the device as this device's setting says",
                        )
                        .clicked()
                    {
                        self.start_full_sync();
                    }
                    ui.add_space(4.0);
                    // This device's own setting for files deleted from it (and the old copy of a
                    // changed file). The plan is made with it, so changing it clears the scan.
                    ui.label(RichText::new("Deleted files:").color(theme::WEAK));
                    let mut mode = d.settings.deleted_files;
                    let tip = format!(
                        "What happens to files deleted from this device, and to the old copy of a changed file. \"{}\" is this device's archive folder. Nothing is ever deleted for good.",
                        d.settings.archive_name
                    );
                    deleted_files_combo(ui, &mut mode, ("page-deleted", &d.id), Some(tip));
                    if mode != d.settings.deleted_files {
                        info!(
                            "Files deleted from \"{}\": {} chosen",
                            d.name,
                            mode.did()
                        );
                        let mut settings = d.settings.clone();
                        settings.deleted_files = mode;
                        self.clear_scan();
                        self.send(Command::SetSettings {
                            id: d.id.clone(),
                            settings: Box::new(settings),
                        });
                        self.notice =
                            Some("Saved. Scan again to use it.".into());
                    }
                }
                Busy::Scanning(_) => {
                    if theme::button(ui, true, theme::I_STOP, "Cancel scan", false).clicked() {
                        self.cancel_scan();
                    }
                }
                Busy::Syncing(v) => {
                    // Skipping is about one file being copied; not while JPG copies are made.
                    if v.jpg.is_none()
                        && theme::button(ui, true, theme::I_SKIP, "Skip current (S)", false)
                            .on_hover_text(
                                "Skip this file and add it to the skip list. The sync stops; scan again to carry on.",
                            )
                            .clicked()
                    {
                        self.skip_current();
                    }
                    if theme::button(ui, true, theme::I_STOP, "Stop sync", false).clicked() {
                        self.stop_sync();
                    }
                }
                Busy::Jpgs(_) => {
                    if theme::button(ui, true, theme::I_STOP, "Stop", false)
                        .on_hover_text("Stop making JPG copies. It carries on next time.")
                        .clicked()
                    {
                        self.stop_sync();
                    }
                }
            });
        });
        let _ = idle;
        if d.settings.deleted_files_not_understood {
            ui.add_space(4.0);
            ui.label(
                RichText::new(pnk_core::registry::DELETED_FILES_NOT_UNDERSTOOD).color(theme::WEAK),
            );
        }
        ui.add_space(8.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pnk_core::registry::SyncSummary;
    use pnk_core::signals::{DeviceKind, SignalKind, Signals, View as FilesView};

    #[test]
    fn eject_is_for_a_plugged_in_stick_and_only_clickable_when_idle() {
        // A stick: there, and clickable only when nothing is running.
        assert_eq!(
            eject_button(DeviceKind::Storage, true, true, true),
            Some(true)
        );
        assert_eq!(
            eject_button(DeviceKind::Storage, true, true, false),
            Some(false)
        );
        // Phones and cameras have none, whatever else is true.
        for kind in [DeviceKind::Ptp, DeviceKind::Mtp, DeviceKind::Camera] {
            assert_eq!(eject_button(kind, true, true, true), None, "{kind:?}");
        }
        // Not plugged in, or a system that can't eject (FreeBSD): no button.
        assert_eq!(eject_button(DeviceKind::Storage, false, true, true), None);
        assert_eq!(eject_button(DeviceKind::Storage, true, false, true), None);
    }

    #[test]
    fn the_recognition_line_names_the_signal_that_decided() {
        assert_eq!(
            recognition_text(How::Recognised(SignalKind::DeviceSerial)),
            "Recognised by its serial number."
        );
        assert_eq!(
            recognition_text(How::Recognised(SignalKind::IdFile)),
            "Recognised by its ID file."
        );
        assert!(recognition_text(How::Answered).contains("You said"));
    }

    #[test]
    fn the_last_sync_line_has_the_date_files_and_size() {
        let mut d = KnownDevice::new(
            DeviceKind::Ptp,
            "P",
            &Signals::default(),
            "2026-01-01T00:00:00Z",
        );
        assert_eq!(sync_line(&d), "Not synced yet");
        d.last_sync = Some(SyncSummary {
            when: "2026-11-17T18:00:00Z".into(),
            files: 4865,
            bytes: 10 * 1024 * 1024 * 1024,
        });
        assert_eq!(
            sync_line(&d),
            "Last sync 17 Nov 2026 · 4,865 files · 10.0 GB"
        );
    }

    fn marker(id: &str, view: FilesView) -> OwnerState {
        OwnerState::Owned(Box::new(owner::FolderOwner::new(
            id,
            "Old phone",
            Signals::default(),
            view,
            "2026-01-01T00:00:00Z",
        )))
    }

    #[test]
    fn another_devices_folder_can_be_handed_over_unless_its_view_differs() {
        use FilesView::{Afc, WindowsWpd};
        let own = marker("me", WindowsWpd);
        assert_eq!(
            claim_offer_for(&own, "me", WindowsWpd),
            None,
            "its own folder needs nothing"
        );
        assert_eq!(
            claim_offer_for(&OwnerState::Unclaimed, "me", WindowsWpd),
            None
        );
        let other = marker("someone-else", WindowsWpd);
        assert!(matches!(
            claim_offer_for(&other, "me", WindowsWpd),
            Some(ClaimOffer::Offer(_))
        ));
        let Some(ClaimOffer::Refused(why)) =
            claim_offer_for(&marker("someone-else", Afc), "me", WindowsWpd)
        else {
            panic!("a different view must not be handed over");
        };
        assert!(why.contains("afc") && why.contains("windows-wpd"), "{why}");
        // The same device through another view is refused too, not offered.
        assert!(matches!(
            claim_offer_for(&marker("me", Afc), "me", WindowsWpd),
            Some(ClaimOffer::Refused(_))
        ));
        assert!(matches!(
            claim_offer_for(&OwnerState::Unreadable("bad".into()), "me", WindowsWpd),
            Some(ClaimOffer::Offer(_))
        ));
    }
}
