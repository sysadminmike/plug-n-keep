//! The Settings tab of a device and the global settings page (with Find my backups). The
//! checking of what is typed is in `validate.rs`; this file holds the working copies and the
//! drawing.

use std::path::PathBuf;

use pnk_core::modes::Mode;
use pnk_core::owner::OwnerState;
use pnk_core::platform::BIN_NAME;
use pnk_core::registry::{ArchiveLayout, DeletedFiles, DeviceSettings, FileTypes};

use super::*;
use crate::s3::S3Form;
use crate::validate::{
    check_archive_name, check_backup_folder, check_name, check_settings, names_text,
    parse_extensions, parse_names,
};

type Problems = Vec<(&'static str, String)>;

/// A device's settings as they are typed: the lists are text until they are applied.
pub(super) struct Fields {
    pub s: DeviceSettings,
    folders: String,
    extensions: String,
    left_out: String,
    ignore: String,
}

impl Fields {
    pub fn new(s: &DeviceSettings) -> Self {
        Self {
            s: s.clone(),
            folders: s.folders.join(", "),
            extensions: s.media_extensions.join(", "),
            left_out: names_text(&s.always_left_out),
            ignore: names_text(&s.ignore_names),
        }
    }

    /// The settings these fields say, or what is wrong with them. `top_names` are the names the
    /// device uses at the top of the backup, which the archive folder can't be called.
    pub fn apply(&self, top_names: &[String]) -> Result<DeviceSettings, Problems> {
        let mut out = self.s.clone();
        out.archive_name = self.s.archive_name.trim().to_string();
        out.folders = parse_names(&self.folders);
        out.always_left_out = parse_names(&self.left_out);
        out.ignore_names = parse_names(&self.ignore);
        let mut problems = Vec::new();
        match parse_extensions(&self.extensions) {
            Ok(e) => out.media_extensions = e,
            Err(e) => problems.push(("file types", e)),
        }
        problems.extend(check_settings(&out, top_names));
        if problems.is_empty() {
            Ok(out)
        } else {
            Err(problems)
        }
    }
}

/// The Settings tab's working copy for one device.
pub(super) struct SettingsForm {
    id: String,
    kind: DeviceKind,
    /// Its backup is in a bucket: no Recycle Bin, no archive folder rename.
    bucket: bool,
    base: DeviceSettings,
    fields: Fields,
}

impl SettingsForm {
    pub fn new(d: &KnownDevice) -> Self {
        Self {
            id: d.id.clone(),
            kind: d.kind,
            bucket: d.s3.is_some(),
            base: d.settings.clone(),
            fields: Fields::new(&d.settings),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Whether the fields say something other than what is saved.
    pub fn dirty(&self) -> bool {
        self.fields.apply(&[]).ok().as_ref() != Some(&self.base)
    }

    /// The device changed under the form (a save, a scan that chose the storage layout): take
    /// the new settings, unless the user is in the middle of editing.
    pub fn refresh(&mut self, d: &KnownDevice) {
        if !self.dirty() {
            self.fields = Fields::new(&d.settings);
        }
        self.base = d.settings.clone();
        self.kind = d.kind;
        self.bucket = d.s3.is_some();
    }
}

/// A row of the global settings grid (after the S3 server section, in S3 mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GlobalRow {
    BackupsFolder,
    ArchiveFolder,
    DeletedFiles,
    ScanAutomatically,
    IgnoreList,
}

/// The rows each mode's settings page shows: the backups folder is Local's only.
pub(super) fn global_rows(mode: Mode) -> &'static [GlobalRow] {
    match mode {
        Mode::Local => &[
            GlobalRow::BackupsFolder,
            GlobalRow::ArchiveFolder,
            GlobalRow::DeletedFiles,
            GlobalRow::ScanAutomatically,
            GlobalRow::IgnoreList,
        ],
        Mode::S3 => &[
            GlobalRow::ArchiveFolder,
            GlobalRow::DeletedFiles,
            GlobalRow::ScanAutomatically,
            GlobalRow::IgnoreList,
        ],
    }
}

/// The global settings as they are typed.
pub(super) struct GlobalForm {
    mode: Mode,
    root: String,
    archive_name: String,
    layout: ArchiveLayout,
    deleted_files: DeletedFiles,
    /// The file's value for it was not understood ([`NewDeviceDefaults::deleted_files_not_understood`]).
    deleted_files_unclear: bool,
    scan_automatically: bool,
    ignore: String,
    /// S3 mode: the S3 server.
    s3: S3Form,
    pub(super) problems: Vec<String>,
}

impl GlobalForm {
    pub fn new(config: &Config, root: PathBuf, mode: Mode) -> Self {
        let defaults = &config.new_devices;
        Self {
            mode,
            root: root.display().to_string(),
            archive_name: defaults.archive_name.clone(),
            layout: defaults.archive_layout,
            deleted_files: defaults.deleted_files,
            deleted_files_unclear: defaults.deleted_files_not_understood,
            scan_automatically: defaults.scan_automatically,
            ignore: names_text(&defaults.ignore_names),
            s3: crate::s3::default_form(config),
            problems: Vec::new(),
        }
    }

    /// What new devices start with is now `d` (the bucket's).
    pub fn set_defaults(&mut self, d: &NewDeviceDefaults) {
        self.archive_name = d.archive_name.clone();
        self.layout = d.archive_layout;
        self.deleted_files = d.deleted_files;
        self.deleted_files_unclear = d.deleted_files_not_understood;
        self.scan_automatically = d.scan_automatically;
        self.ignore = names_text(&d.ignore_names);
    }

    /// Whether what is typed for new devices differs from `base`.
    pub fn defaults_edited(&self, base: &NewDeviceDefaults) -> bool {
        let mut base = base.clone();
        base.deleted_files_not_understood = false;
        if self.mode == Mode::S3 && base.deleted_files == DeletedFiles::RecycleBin {
            base.deleted_files = DeletedFiles::Archive;
        }
        self.apply().map_or(true, |(_, d)| d != base)
    }

    /// S3 mode: the S3 server (none if the fields are empty), or what is wrong with it. Its
    /// folder is only the base each device's own folder goes under. Local mode has none.
    pub fn s3_config(&self) -> Result<Option<pnk_core::store::S3Config>, String> {
        match self.mode {
            Mode::Local => Ok(None),
            Mode::S3 => self.s3.default_config(),
        }
    }

    /// The backups root folder (Local mode only) and what new devices start with, or what is
    /// wrong.
    pub fn apply(&self) -> Result<(Option<PathBuf>, NewDeviceDefaults), Vec<String>> {
        let mut problems = Vec::new();
        let root = match self.mode {
            Mode::Local => check_backup_folder(&self.root)
                .map(Some)
                .map_err(|e| problems.push(e))
                .ok(),
            Mode::S3 => Some(None),
        };
        // A bucket has no Recycle Bin.
        let deleted_files =
            if self.mode == Mode::S3 && self.deleted_files == DeletedFiles::RecycleBin {
                DeletedFiles::Archive
            } else {
                self.deleted_files
            };
        let name = self.archive_name.trim().to_string();
        if let Err(e) = check_archive_name(&name, &[]) {
            problems.push(e);
        }
        let ignore = parse_names(&self.ignore);
        for n in &ignore {
            if let Err(e) = check_name(n) {
                problems.push(e);
            }
        }
        match root {
            Some(root) if problems.is_empty() => Ok((
                root,
                NewDeviceDefaults {
                    archive_name: name,
                    archive_layout: self.layout,
                    deleted_files,
                    deleted_files_not_understood: false,
                    scan_automatically: self.scan_automatically,
                    ignore_names: ignore,
                },
            )),
            _ => Err(problems),
        }
    }
}

/// What happens to files deleted from the device, as the choice is worded.
pub(super) fn deleted_label(d: DeletedFiles) -> String {
    match d {
        DeletedFiles::Archive => "Move to the archive folder".to_string(),
        DeletedFiles::LeaveInPlace => "Leave them in the backup folder".to_string(),
        DeletedFiles::RecycleBin => format!("Send to the {BIN_NAME}"),
    }
}

/// The choice of what happens to files deleted from the device, with the line under it.
pub(super) fn deleted_files_combo(
    ui: &mut egui::Ui,
    value: &mut DeletedFiles,
    id: impl std::hash::Hash + std::fmt::Debug,
    tip: Option<String>,
    bucket: bool,
) {
    if bucket && *value == DeletedFiles::RecycleBin {
        *value = DeletedFiles::Archive;
    }
    let tip = tip.unwrap_or_else(|| {
        if bucket {
            "Nothing is ever deleted for good. A bucket has no Recycle Bin, so files go to the archive folder or stay where they are.".to_string()
        } else {
            format!(
                "Nothing is ever deleted for good. Files sent to the {BIN_NAME} can be restored from it."
            )
        }
    });
    egui::ComboBox::from_id_salt(id)
        .selected_text(deleted_label(*value))
        .width(240.0)
        .show_ui(ui, |ui| {
            for d in DeletedFiles::ALL {
                // A bucket has no Recycle Bin.
                if bucket && d == DeletedFiles::RecycleBin {
                    continue;
                }
                ui.selectable_value(value, d, deleted_label(d));
            }
        })
        .response
        .on_hover_text(tip);
}

/// The layout as the settings name it.
pub(super) fn layout_label(l: ArchiveLayout) -> &'static str {
    match l {
        ArchiveLayout::KeepFolders => "Keep the device's folders",
        ArchiveLayout::Flat => "Flat: every file in the dated folder",
    }
}

/// The editable fields of a device's settings. `full`: all of them, as in the Settings tab; the
/// new-device card shows the few that matter at set-up.
#[allow(clippy::too_many_arguments)]
pub(super) fn fields_ui(
    ui: &mut egui::Ui,
    f: &mut Fields,
    kind: DeviceKind,
    full: bool,
    problems: &Problems,
    id_salt: &str,
    jpeg: &Result<(), String>,
    bucket: bool,
) {
    let label = |ui: &mut egui::Ui, text: &str| {
        ui.label(RichText::new(text).color(theme::WEAK));
    };
    egui::Grid::new(("fields", id_salt))
        .num_columns(2)
        .spacing([12.0, 7.0])
        .min_col_width(110.0)
        .show(ui, |ui| {
            label(ui, "What to back up");
            ui.horizontal(|ui| {
                ui.radio_value(&mut f.s.file_types, FileTypes::PhotosAndVideos, "Photos and videos");
                ui.radio_value(&mut f.s.file_types, FileTypes::All, "Everything");
            });
            ui.end_row();

            label(ui, "Folders");
            ui.add(
                egui::TextEdit::singleline(&mut f.folders)
                    .hint_text("everything")
                    .margin(egui::Margin::symmetric(8, 4))
                    .desired_width(320.0),
            )
            .on_hover_text("Top-level folders of the device to back up, separated by commas. Empty means everything.");
            ui.end_row();

            if f.s.file_types == FileTypes::PhotosAndVideos {
                label(ui, "File types");
                ui.add(
                    egui::TextEdit::singleline(&mut f.extensions)
                        .margin(egui::Margin::symmetric(8, 4))
                        .desired_width(320.0),
                )
                .on_hover_text("The file name endings that count as photos and videos, separated by commas.");
                ui.end_row();
            }

            label(ui, "Deleted from the device");
            ui.vertical(|ui| {
                deleted_files_combo(ui, &mut f.s.deleted_files, ("deleted", id_salt), None, bucket);
                ui.label(
                    RichText::new("A changed file's old copy goes the same way.")
                        .small()
                        .color(theme::WEAK),
                );
            });
            ui.end_row();

            // The archive's name and layout only matter when files go to the archive; they keep
            // their values when it isn't chosen.
            let archive_chosen = f.s.deleted_files == DeletedFiles::Archive;
            ui.add_enabled_ui(archive_chosen, |ui| label(ui, "Archive folder"));
            ui.add_enabled_ui(archive_chosen, |ui| ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut f.s.archive_name)
                        .margin(egui::Margin::symmetric(8, 4))
                        .desired_width(150.0),
                )
                .on_hover_text("Files deleted from the device, and old copies of changed files, go into a dated folder inside this one, inside the backup folder.");
                egui::ComboBox::from_id_salt(("layout", id_salt))
                    .selected_text(layout_label(f.s.archive_layout))
                    .width(240.0)
                    .show_ui(ui, |ui| {
                        for l in [ArchiveLayout::KeepFolders, ArchiveLayout::Flat] {
                            ui.selectable_value(&mut f.s.archive_layout, l, layout_label(l));
                        }
                    });
            }));
            ui.end_row();

            label(ui, "HEIC photos");
            ui.vertical(|ui| {
                ui.add_enabled(
                    jpeg.is_ok(),
                    egui::Checkbox::new(&mut f.s.jpeg_copies, "Make a JPG copy of each HEIC photo"),
                )
                .on_hover_text("A JPG is made beside each HEIC in the backup, for programs that can't open HEIC. The HEIC stays the backup. Turning this off keeps the JPGs already made.");
                if let Err(hint) = jpeg {
                    ui.label(RichText::new(hint).small().color(theme::WEAK));
                }
            });
            ui.end_row();

            label(ui, "When plugged in");
            ui.checkbox(&mut f.s.scan_automatically, "Scan automatically")
                .on_hover_text("A scan only reads. Nothing is copied until you click Start sync.");
            ui.end_row();

            if full {
                if kind == DeviceKind::Storage {
                    label(ui, "Compare");
                    ui.checkbox(&mut f.s.compare_by_date, "Also compare modified dates, not only sizes")
                        .on_hover_text("A file with the same size but a different date counts as changed.");
                    ui.end_row();
                }
                label(ui, "Left out");
                ui.add(
                    egui::TextEdit::multiline(&mut f.left_out)
                        .desired_rows(2)
                        .margin(egui::Margin::symmetric(8, 4))
                        .desired_width(320.0),
                )
                .on_hover_text("Names never backed up, wherever they are on the device. One per line.");
                ui.end_row();
                label(ui, "Ignore list");
                ui.add(
                    egui::TextEdit::multiline(&mut f.ignore)
                        .desired_rows(2)
                        .margin(egui::Margin::symmetric(8, 4))
                        .desired_width(320.0),
                )
                .on_hover_text("PC clutter such as Thumbs.db: never backed up, and never archived from the backup folder. One per line.");
                ui.end_row();
            }
        });
    for (what, why) in problems {
        ui.label(RichText::new(format!("{what}: {why}")).color(theme::DANGER));
    }
}

impl App {
    /// The device's Settings tab.
    pub(super) fn settings_tab(&mut self, ui: &mut egui::Ui, d: &KnownDevice) {
        if self.form.as_ref().map(SettingsForm::id) != Some(d.id.as_str()) {
            self.form = Some(SettingsForm::new(d));
        }
        let Some(mut form) = self.form.take() else {
            return;
        };
        let scan_for_device = self
            .scan
            .as_ref()
            .filter(|s| s.plan.device_key == d.id)
            .map(|s| (s.plan_id, s.plan.top_names.clone(), s.plan.owner.clone()));
        let top_names = scan_for_device
            .as_ref()
            .map(|(_, t, _)| t.clone())
            .unwrap_or_default();
        // A device whose backup folder can't be found keeps its settings as they are.
        let folder_missing = self.folder_state(d) == crate::worker::FolderState::Missing;
        let mut rebuild = false;
        let idle = self.idle() && d.offline.is_none() && !folder_missing;
        let jpeg = self.heic_ok();
        let mut backfill = false;
        let problems = match form.fields.apply(&top_names) {
            Ok(_) => Vec::new(),
            Err(p) => p,
        };
        ui.label(
            RichText::new(if folder_missing {
                "These can't be changed while the backup folder is missing. Find it or change it first."
            } else {
                "Changes are saved when you click Save changes. They apply from the next scan."
            })
            .color(theme::WEAK),
        );
        ui.add_space(6.0);
        let dirty = form.dirty();
        let mut save = false;
        let mut revert = false;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_enabled_ui(idle, |ui| {
                    fields_ui(
                        ui,
                        &mut form.fields,
                        form.kind,
                        true,
                        &problems,
                        &d.id,
                        &jpeg,
                        form.bucket,
                    );
                });
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(
                            dirty && problems.is_empty() && idle,
                            theme::primary("Save changes"),
                        )
                        .clicked()
                    {
                        save = true;
                    }
                    if ui
                        .add_enabled(dirty && idle, egui::Button::new("Revert"))
                        .clicked()
                    {
                        revert = true;
                    }
                });
                ui.add_space(14.0);
                ui.separator();
                ui.add_space(6.0);
                // The one-off: only with the saved setting on and a decoder here, and only
                // when there is a backup folder to look in.
                // From the worker's last word on the folder, not a disk call on every frame.
                let folder_ok = d.destination().is_some()
                    && self.folder_state(d) == crate::worker::FolderState::Present;
                let can = idle && d.settings.jpeg_copies && jpeg.is_ok() && folder_ok;
                if ui
                    .add_enabled(
                        can,
                        egui::Button::new("Make JPG copies of HEIC photos already in the backup"),
                    )
                    .on_hover_text("Looks only in the backup folder (not the archive); the device isn't needed. Names that are taken are left alone. You can stop it, and it carries on next time.")
                    .clicked()
                {
                    backfill = true;
                }
                let why = if !d.settings.jpeg_copies {
                    Some("Turn on \"Make a JPG copy of each HEIC photo\" and save first.".to_string())
                } else if let Err(hint) = &jpeg {
                    Some(hint.clone())
                } else if !folder_ok {
                    Some("This device has no backup folder yet.".to_string())
                } else {
                    None
                };
                if let Some(why) = why {
                    ui.label(RichText::new(why).small().color(theme::WEAK));
                }
                if d.s3.is_some() {
                    ui.add_space(10.0);
                    ui.separator();
                    ui.add_space(6.0);
                    if ui
                        .add_enabled(idle, egui::Button::new("Rebuild modified-time cache"))
                        .on_hover_text("For a backup in a bucket: forget the saved file dates. The next scan reads them again.")
                        .clicked()
                    {
                        rebuild = true;
                    }
                    ui.label(
                        RichText::new("Only needed if dates look wrong. Nothing is copied or deleted.")
                            .small()
                            .color(theme::WEAK),
                    );
                }
            });
        if revert {
            form.fields = Fields::new(&d.settings);
        }
        if save && let Ok(new) = form.fields.apply(&top_names) {
            self.save_settings(d, new, scan_for_device.map(|(id, _, o)| (id, o)));
        }
        self.form = Some(form);
        if rebuild {
            self.confirm = Some(Confirm::RebuildCache(d.id.clone()));
        }
        if backfill {
            self.start_backfill(&d.id);
        }
    }

    /// Save a device's settings. Changing the archive name on an existing archive offers to
    /// rename the folder too, after a confirmation.
    fn save_settings(
        &mut self,
        d: &KnownDevice,
        new: DeviceSettings,
        scan: Option<(u64, OwnerState)>,
    ) {
        let renamed = !new
            .archive_name
            .eq_ignore_ascii_case(&d.settings.archive_name)
            // A bucket's archive folder is not renamed: that would copy every file in it.
            && d.s3.is_none();
        if renamed && let Some((plan_id, OwnerState::Owned(_))) = &scan {
            self.dialog = Some(Dialog::RenameArchive {
                id: d.id.clone(),
                plan_id: *plan_id,
                old: d.settings.archive_name.clone(),
                new: new.archive_name.clone(),
                settings: Box::new(new),
            });
            return;
        }
        info!("Saving the settings of \"{}\"", d.name);
        self.clear_scan();
        self.send(Command::SetSettings {
            id: d.id.clone(),
            settings: Box::new(new),
        });
        self.notice = Some("Settings saved. They apply from the next scan.".into());
    }

    /// The global settings page: only the current mode's.
    pub(super) fn global_settings(&mut self, ui: &mut egui::Ui) {
        let Some(mut form) = self.global.take() else {
            return;
        };
        let idle = self.idle();
        let mode = self.mode;
        let mut save = false;
        let mut find = false;
        let mut add = false;
        let mut apply_found = false;
        let mut relocate: Option<(String, Destination)> = None;
        // A successful save closes the page.
        let mut close = false;
        let mut log_level = self.config.log_level;
        let (editable, bucket_note) = self.new_devices_state();
        let forgotten_count = self.bucket.forgotten.len();
        let saving = self.settings_saving;
        self.banners(ui);
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("Settings")
                            .size(17.0)
                            .family(theme::semibold())
                            .color(theme::TEXT),
                    );
                    theme::pill(ui, mode.label(), theme::ACCENT);
                });
                ui.add_space(8.0);
                if mode == Mode::S3 {
                    ui.label(
                        RichText::new("S3 connection")
                            .family(theme::semibold())
                            .color(theme::TEXT),
                    );
                    ui.label(
                        RichText::new(
                            "Kept on this PC: where the bucket is and how to reach it. Each device gets a folder of its own directly inside the base folder. The keys are never kept by this app; the profile names them.",
                        )
                        .color(theme::WEAK),
                    );
                    ui.add_space(4.0);
                    self.s3_server_ui(ui, &mut form.s3);
                    ui.add_space(12.0);
                    ui.label(
                        RichText::new("New devices")
                            .family(theme::semibold())
                            .color(theme::TEXT),
                    );
                    ui.label(
                        RichText::new(
                            "Kept in the bucket: every PC that uses it starts new devices the same way, and a new PC gets them from the bucket.",
                        )
                        .color(theme::WEAK),
                    );
                    if let Some(t) = &bucket_note {
                        ui.label(RichText::new(t).color(theme::CAUTION));
                    }
                    ui.add_space(4.0);
                }
                ui.add_enabled_ui(editable, |ui| {
                egui::Grid::new("global-fields")
                    .num_columns(2)
                    .spacing([12.0, 7.0])
                    .min_col_width(150.0)
                    .show(ui, |ui| {
                        let label = |ui: &mut egui::Ui, text: &str| {
                            ui.label(RichText::new(text).color(theme::WEAK));
                        };
                        for row in global_rows(mode) {
                            match row {
                                GlobalRow::BackupsFolder => {
                                    label(ui, "Backups folder");
                                    ui.horizontal(|ui| {
                                        ui.add(
                                            egui::TextEdit::singleline(&mut form.root)
                                                .hint_text(default_folder())
                                                .margin(egui::Margin::symmetric(8, 4))
                                                .desired_width(320.0),
                                        )
                                        .on_hover_text("Each new device gets a folder of its own inside this one.");
                                        if ui.button("Browse…").clicked() {
                                            let mut dialog = rfd::FileDialog::new().set_title("Choose the backups folder");
                                            if std::path::Path::new(form.root.trim()).is_dir() {
                                                dialog = dialog.set_directory(form.root.trim());
                                            }
                                            if let Some(dir) = dialog.pick_folder() {
                                                form.root = dir.display().to_string();
                                            }
                                        }
                                    });
                                }
                                GlobalRow::ArchiveFolder => {
                                    label(ui, "New devices: archive folder");
                                    ui.horizontal(|ui| {
                                        ui.add(
                                            egui::TextEdit::singleline(&mut form.archive_name)
                                                .margin(egui::Margin::symmetric(8, 4))
                                                .desired_width(150.0),
                                        );
                                        egui::ComboBox::from_id_salt("global-layout")
                                            .selected_text(layout_label(form.layout))
                                            .width(240.0)
                                            .show_ui(ui, |ui| {
                                                for l in [ArchiveLayout::KeepFolders, ArchiveLayout::Flat] {
                                                    ui.selectable_value(&mut form.layout, l, layout_label(l));
                                                }
                                            });
                                    });
                                }
                                GlobalRow::DeletedFiles => {
                                    label(ui, "New devices: deleted files");
                                    ui.vertical(|ui| {
                                        deleted_files_combo(
                                            ui,
                                            &mut form.deleted_files,
                                            "global-deleted",
                                            None,
                                            mode == Mode::S3,
                                        );
                                        ui.label(
                                            RichText::new("A changed file's old copy goes the same way.")
                                                .small()
                                                .color(theme::WEAK),
                                        );
                                        if form.deleted_files_unclear {
                                            ui.label(
                                                RichText::new(
                                                    "A setting in config.json wasn't understood, so deleted files are left in place; check this choice.",
                                                )
                                                .small()
                                                .color(theme::WEAK),
                                            );
                                        }
                                    });
                                }
                                GlobalRow::ScanAutomatically => {
                                    label(ui, "New devices");
                                    ui.checkbox(&mut form.scan_automatically, "Scan automatically when plugged in");
                                }
                                GlobalRow::IgnoreList => {
                                    label(ui, "New devices: ignore list");
                                    ui.add(
                                        egui::TextEdit::multiline(&mut form.ignore)
                                            .desired_rows(2)
                                            .margin(egui::Margin::symmetric(8, 4))
                                            .desired_width(320.0),
                                    )
                                    .on_hover_text("PC clutter such as Thumbs.db. One name per line.");
                                }
                            }
                            ui.end_row();
                        }
                    });
                });
                ui.add_space(7.0);
                egui::Grid::new("global-log-level")
                    .num_columns(2)
                    .spacing([12.0, 7.0])
                    .min_col_width(150.0)
                    .show(ui, |ui| {
                        ui.label(RichText::new("Log level").color(theme::WEAK));
                        egui::ComboBox::from_id_salt("global-log")
                            .selected_text(crate::log_view::level_name(log_level))
                            .show_ui(ui, |ui| {
                                for l in LogLevel::ALL {
                                    ui.selectable_value(&mut log_level, l, crate::log_view::level_name(l));
                                }
                            });
                        ui.end_row();
                    });
                ui.add_space(8.0);
                for p in &form.problems {
                    ui.label(RichText::new(p).color(theme::DANGER));
                }
                ui.add_space(8.0);
                if ui
                    .add_enabled(idle && !saving, theme::primary("Save settings"))
                    .on_hover_text(match mode {
                        Mode::Local => "The backups folder and what new devices start with. Devices already set up keep their own settings.",
                        Mode::S3 => "The connection (kept on this PC) and what new devices start with (kept in the bucket). Devices already set up keep their own settings.",
                    })
                    .clicked()
                {
                    save = true;
                }
                ui.add_space(14.0);
                ui.separator();
                ui.add_space(8.0);
                ui.label(
                    RichText::new("Find my backups")
                        .family(theme::semibold())
                        .color(theme::TEXT),
                );
                ui.label(
                    RichText::new(match mode {
                        Mode::Local => "Looks inside each folder directly in the backups folder for a backup (a folder with a .plug-n-keep.json file) and lists the devices they belong to. It doesn't look any deeper. Use it on a new PC, or if this app's own records are lost.",
                        Mode::S3 => "Looks in the base folder of the bucket for backups (a folder with a .plug-n-keep.json file, directly inside it) and lists the devices they belong to, including any that were forgotten. The list is also read from the bucket at start and on Refresh.",
                    })
                    .color(theme::WEAK),
                );
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let ready = match mode {
                        Mode::Local => true,
                        Mode::S3 => form.s3_config().is_ok_and(|c| c.is_some()),
                    };
                    if ui
                        .add_enabled(idle && ready, egui::Button::new("Find my backups"))
                        .on_disabled_hover_text(if idle {
                            "Fill in the S3 server first."
                        } else {
                            "Not while something is running."
                        })
                        .clicked()
                    {
                        find = true;
                    }
                    if ui
                        .add_enabled(idle, egui::Button::new("Add an existing backup…"))
                        .on_hover_text(
                            "Pick one backup (or a place that holds several), such as one copied from another PC. Each device is set up again from the files in it, with its settings and history.",
                        )
                        .clicked()
                    {
                        add = true;
                    }
                });
                ui.add_space(6.0);
                if mode == Mode::S3 && forgotten_count > 0 {
                    ui.label(
                        RichText::new(format!(
                            "{} forgotten device{} hidden on every PC. \"Add an existing backup…\" or Find my backups brings back any whose folder is still in the bucket.",
                            forgotten_count,
                            if forgotten_count == 1 { " is" } else { "s are" }
                        ))
                        .small()
                        .color(theme::WEAK),
                    );
                }
                if let Some(found) = &self.found {
                    found_view(ui, found, idle, &mut apply_found, &mut relocate);
                }
                ui.add_space(14.0);
                ui.label(
                    RichText::new(
                        "This software is based in part on the work of the Independent JPEG Group.",
                    )
                    .small()
                    .color(theme::WEAK),
                );
            });
        // Applied outside the closure that borrows the form.
        if log_level != self.config.log_level {
            self.set_log_level(log_level);
        }
        if save {
            match (form.apply(), form.s3_config()) {
                (Ok((root, defaults)), Ok(s3)) => {
                    form.problems.clear();
                    let mut connection_changed = false;
                    match mode {
                        Mode::Local => {
                            self.config.backup_dir = root;
                            self.config.new_devices = defaults.clone();
                        }
                        Mode::S3 => {
                            connection_changed = self.config.default_s3 != s3;
                            self.config.default_s3 = s3;
                        }
                    }
                    self.save_config();
                    if self.error.is_none() {
                        // Forms that were opened before the save take the server as it is now.
                        if let Some(setup) = &mut self.setup {
                            setup.set_s3_server(self.config.default_s3.as_ref());
                        }
                        if let Some(dlg) = &mut self.place_dialog {
                            dlg.form.base = self.config.default_s3.clone();
                        }
                        // S3 mode: what new devices start with goes into the bucket, if it
                        // changed (and the bucket can take it); the bucket's answer ends the save.
                        let defaults_changed = editable && defaults != self.config.new_devices;
                        if mode == Mode::S3
                            && self.config.default_s3.is_some()
                            && (connection_changed || defaults_changed)
                        {
                            self.settings_saving = true;
                            self.send(Command::SaveBucketSettings {
                                new_devices: Box::new(defaults),
                                connection_changed,
                            });
                        } else {
                            self.notice = Some("Settings saved.".into());
                            // Back to where the user was.
                            close = true;
                        }
                    }
                }
                (folder, s3) => {
                    let mut problems = folder.err().unwrap_or_default();
                    problems.extend(s3.err());
                    form.problems = problems;
                }
            }
        }
        if find {
            let root = match mode {
                Mode::Local => check_backup_folder(&form.root).map(Destination::Local),
                Mode::S3 => match form.s3_config() {
                    Ok(Some(cfg)) if Some(&cfg) == self.config.default_s3.as_ref() => {
                        Ok(Destination::S3(cfg))
                    }
                    Ok(Some(_)) => Err(
                        "Click Save settings first, so the connection typed here is the one that is looked in."
                            .to_string(),
                    ),
                    Ok(None) => Err(crate::app::place::NOT_SET_UP.to_string()),
                    Err(e) => Err(e),
                },
            };
            match root {
                Ok(root) => {
                    self.found = None;
                    self.found_in = Some(root.clone());
                    self.send(Command::FindBackups { root, apply: false });
                }
                Err(e) => self.error = Some(e),
            }
        }
        if add {
            self.add_existing_backup();
        }
        if apply_found && let Some(root) = self.found_in.clone() {
            self.send(Command::FindBackups { root, apply: true });
        }
        if let Some((id, dest)) = relocate {
            // A scan of this device was made against the old folder.
            if self.scan.as_ref().is_some_and(|s| s.plan.device_key == id) {
                self.clear_scan();
            }
            self.send(Command::Relocate { id, dest });
            self.found = None;
        }
        // The page may have been closed by the click on its button this frame, or by a save.
        if self.global.is_none() && !close {
            self.global = Some(form);
        }
    }
}

impl App {
    /// S3 mode's "New devices" settings: whether they can be edited (they are the bucket's, so
    /// the bucket must have been read), and a line saying why not.
    fn new_devices_state(&self) -> (bool, Option<String>) {
        use crate::worker::BucketStatus;
        if self.mode == Mode::Local {
            return (true, None);
        }
        match &self.bucket.status {
            // No connection yet: what is typed goes into the bucket when it is saved.
            BucketStatus::NotSet => (true, None),
            BucketStatus::Looking => (
                false,
                Some("Looking in the bucket… these show what this PC last saw.".into()),
            ),
            BucketStatus::Ready if self.bucket.settings.is_some() => (true, None),
            BucketStatus::Ready => (
                false,
                Some(
                    "The settings in the bucket can't be read, so they can't be changed here."
                        .into(),
                ),
            ),
            BucketStatus::Unavailable { state, detail } => (
                false,
                Some(format!(
                    "{} These show what this PC last saw, and can't be changed until the bucket can be reached.",
                    if detail.is_empty() {
                        crate::worker::unavailable_text(*state)
                    } else {
                        detail.clone()
                    }
                )),
            ),
        }
    }
}

/// What Find my backups found: the devices it would add, those it knows, the ones whose folder
/// is somewhere else, and what it couldn't read.
fn found_view(
    ui: &mut egui::Ui,
    found: &Found,
    idle: bool,
    apply: &mut bool,
    relocate: &mut Option<(String, Destination)>,
) {
    let nothing = found.added.is_empty()
        && found.already_known.is_empty()
        && found.moved.is_empty()
        && found.problems.is_empty();
    theme::card(ui, |ui| {
        if nothing {
            ui.label("No backups found directly inside that place.");
        }
        if !found.added.is_empty() {
            let n = found.added.len();
            ui.label(
                RichText::new(if found.apply {
                    format!("Added {} to the list of known devices:", format::count(n))
                } else {
                    format!("{} to add:", format::count(n))
                })
                .family(theme::semibold())
                .color(theme::TEXT),
            );
            for name in &found.added {
                ui.label(format!("  {name}"));
            }
            if !found.apply
                && ui
                    .add(theme::primary(format!(
                        "Add {}",
                        if n == 1 {
                            "it".to_string()
                        } else {
                            format!("these {n}")
                        }
                    )))
                    .clicked()
            {
                *apply = true;
            }
            ui.add_space(4.0);
        }
        if !found.already_known.is_empty() {
            ui.label(
                RichText::new(format!("Already known: {}", found.already_known.join(", ")))
                    .color(theme::WEAK),
            );
        }
        for m in &found.moved {
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                ui.label(format!(
                    "\"{}\" is known, but its recorded place is {}. A marker for it was found in",
                    m.name,
                    m.recorded
                        .as_ref()
                        .map_or_else(|| "not set".to_string(), |p| p.display().to_string())
                ));
                ui.label(RichText::new(m.found.display().to_string()).family(theme::semibold()));
                if ui
                    .add_enabled(idle, egui::Button::new("Use this place"))
                    .clicked()
                {
                    *relocate = Some((
                        m.device_id.clone(),
                        m.destination
                            .clone()
                            .unwrap_or_else(|| Destination::Local(m.found.clone())),
                    ));
                }
            });
        }
        for p in &found.problems {
            ui.label(RichText::new(p).color(theme::CAUTION));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(kind: DeviceKind) -> Fields {
        Fields::new(&DeviceSettings::defaults_for(kind))
    }

    #[test]
    fn untouched_fields_give_the_same_settings_back() {
        for kind in [DeviceKind::Ptp, DeviceKind::Storage] {
            let s = DeviceSettings::defaults_for(kind);
            assert_eq!(Fields::new(&s).apply(&[]).unwrap(), s);
        }
    }

    #[test]
    fn typed_lists_are_parsed_and_the_name_is_trimmed() {
        let mut f = fields(DeviceKind::Ptp);
        f.folders = "DCIM, dcim,Pictures\nMovies".into();
        f.extensions = ".JPG, mp4".into();
        f.s.archive_name = "  Old stuff ".into();
        f.ignore = "x.tmp\nThumbs.db".into();
        let s = f.apply(&[]).unwrap();
        assert_eq!(s.folders, ["DCIM", "Pictures", "Movies"]);
        assert_eq!(s.media_extensions, ["jpg", "mp4"]);
        assert_eq!(s.archive_name, "Old stuff");
        assert_eq!(s.ignore_names, ["x.tmp", "Thumbs.db"]);
    }

    #[test]
    fn bad_input_is_reported_against_its_setting_and_nothing_is_applied() {
        let mut f = fields(DeviceKind::Ptp);
        f.s.archive_name = "a/b".into();
        f.folders = "DCIM, x:y".into();
        f.extensions = "j/pg".into();
        let which: Vec<&str> = f.apply(&[]).unwrap_err().iter().map(|p| p.0).collect();
        assert_eq!(which, ["file types", "archive name", "folders"]);
        // The archive name also can't be one the device uses at the top.
        let mut g = fields(DeviceKind::Ptp);
        g.s.archive_name = "DCIM".into();
        assert!(g.apply(&["dcim".to_string()]).is_err());
        assert!(g.apply(&[]).is_ok());
    }

    #[test]
    fn the_form_knows_when_it_has_been_edited() {
        let d = KnownDevice::new(
            DeviceKind::Storage,
            "S",
            &pnk_core::signals::Signals::default(),
            "2026-01-01T00:00:00Z",
        );
        let mut form = SettingsForm::new(&d);
        assert!(!form.dirty());
        form.fields.s.scan_automatically = true;
        assert!(form.dirty());
        // A refresh keeps the user's edits...
        form.refresh(&d);
        assert!(form.fields.s.scan_automatically);
        // ...but a bad entry counts as edited too.
        form.fields.s.scan_automatically = false;
        form.fields.s.archive_name = "x/y".into();
        assert!(form.dirty());
    }

    #[test]
    fn the_global_form_needs_a_full_path_and_a_safe_archive_name() {
        let defaults = NewDeviceDefaults::default();
        let root = std::env::temp_dir();
        let mut f = GlobalForm::new(&Config::default(), root.clone(), Mode::Local);
        let (got_root, got) = f.apply().unwrap();
        assert_eq!(got_root, Some(root.clone()));
        assert_eq!(got, defaults);
        f.root = "relative".into();
        f.archive_name = "NUL".into();
        f.ignore = "ok.txt\n".into();
        assert_eq!(f.apply().unwrap_err().len(), 2);
        f.root = root.display().to_string();
        f.archive_name = "Old".into();
        f.layout = ArchiveLayout::Flat;
        f.scan_automatically = true;
        let (_, d) = f.apply().unwrap();
        assert_eq!(d.archive_name, "Old");
        assert_eq!(d.archive_layout, ArchiveLayout::Flat);
        assert!(d.scan_automatically);
    }

    /// What the page shows is what the one settings file holds for new devices.
    #[test]
    fn the_global_form_starts_from_the_settings_for_new_devices() {
        let mut config = Config::default();
        config.new_devices.archive_name = "Gone".into();
        config.new_devices.scan_automatically = true;
        config.new_devices.ignore_names = vec!["a.tmp".into(), "b.tmp".into()];
        let f = GlobalForm::new(&config, std::env::temp_dir(), Mode::Local);
        assert_eq!(f.archive_name, "Gone");
        assert!(f.scan_automatically);
        let (_, got) = f.apply().unwrap();
        assert_eq!(got, config.new_devices);
    }

    /// The bucket's word for new devices replaces what the page shows, unless it was edited.
    #[test]
    fn the_s3_page_follows_the_buckets_settings_until_it_is_edited() {
        let mut f = GlobalForm::new(&Config::default(), PathBuf::new(), Mode::S3);
        let before = NewDeviceDefaults::default();
        assert!(!f.defaults_edited(&before));
        let mut theirs = before.clone();
        theirs.archive_name = "_Theirs".into();
        theirs.scan_automatically = true;
        f.set_defaults(&theirs);
        assert_eq!(f.archive_name, "_Theirs");
        assert!(f.scan_automatically);
        assert!(!f.defaults_edited(&theirs));
        assert!(f.defaults_edited(&before));
        f.ignore = "x.tmp".into();
        assert!(f.defaults_edited(&theirs));
        // A bad entry counts as edited too.
        f.set_defaults(&theirs);
        f.archive_name = "a/b".into();
        assert!(f.defaults_edited(&theirs));
    }

    #[test]
    fn each_mode_shows_only_its_own_settings() {
        assert!(global_rows(Mode::Local).contains(&GlobalRow::BackupsFolder));
        assert!(!global_rows(Mode::S3).contains(&GlobalRow::BackupsFolder));
        // What applies to new devices is in both.
        for row in [
            GlobalRow::ArchiveFolder,
            GlobalRow::DeletedFiles,
            GlobalRow::ScanAutomatically,
            GlobalRow::IgnoreList,
        ] {
            assert!(global_rows(Mode::S3).contains(&row));
            assert!(global_rows(Mode::Local).contains(&row));
        }
    }

    #[test]
    fn local_settings_never_carry_s3_and_s3_settings_never_need_a_folder() {
        // Local: whatever S3 fields hold is ignored, and the S3 settings stay empty.
        let mut f = GlobalForm::new(&Config::default(), std::env::temp_dir(), Mode::Local);
        f.s3.server = "http://h:9000".into();
        f.s3.bucket = "b".into();
        assert_eq!(f.s3_config(), Ok(None));
        // S3: no backups folder is asked for, the server is, and no Recycle Bin.
        let mut g = GlobalForm::new(&Config::default(), PathBuf::new(), Mode::S3);
        g.root = "relative".into();
        g.deleted_files = DeletedFiles::RecycleBin;
        let (root, defaults) = g.apply().unwrap();
        assert_eq!(root, None);
        assert_eq!(defaults.deleted_files, DeletedFiles::Archive);
        assert_eq!(g.s3_config(), Ok(None));
        g.s3.server = "http://h:9000".into();
        assert!(
            g.s3_config().is_err(),
            "a server without a bucket is refused"
        );
        g.s3.bucket = "b".into();
        assert_eq!(g.s3_config().unwrap().unwrap().bucket, "b");
    }
}
