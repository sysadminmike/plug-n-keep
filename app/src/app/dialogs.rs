//! Confirmations, and the questions that need an explicit answer: the scan's (clear the skip
//! list, archive, use this folder), and the device's (rename, forget, write an ID file, rename
//! the archive folder).
use pnk_core::registry::DeviceSettings;

use super::*;

/// A question about a device, asked in a window of its own.
pub(super) enum Dialog {
    Rename {
        id: String,
        text: String,
    },
    Forget {
        id: String,
        name: String,
    },
    /// Write the ID file to a stick. The default answer is No.
    IdFile {
        name: String,
        /// The listed device (the stick) the question is about: the write is refused if another
        /// device is the open one by the time of the answer.
        device: String,
        /// Asked after the user answered a question about the stick.
        after_question: bool,
    },
    /// The archive name changed on an archive that exists: rename its folder too?
    RenameArchive {
        id: String,
        plan_id: u64,
        old: String,
        new: String,
        settings: Box<DeviceSettings>,
    },
}

/// What the user chose in a dialog.
enum Choice {
    /// The first button, the one that does it.
    Yes,
    /// A second way to do it (only Rename archive has one).
    Other,
    No,
}

impl App {
    pub(super) fn confirm_dialog(&mut self, ctx: &egui::Context) {
        let Some(c) = &self.confirm else { return };
        let mut alt_label: Option<&str> = None;
        let (title, text, ok) = match c {
            Confirm::ClearSkipList => (
                "Clear the skip list?".to_string(),
                "Every file on it will be included in the next sync again.".to_string(),
                "Clear",
            ),
            Confirm::Archive {
                items,
                count,
                changed,
            } => {
                if !items.is_empty() {
                    alt_label = Some("Sync without moving files");
                }
                let (archive, why) = self
                    .scan
                    .as_ref()
                    .map(|s| (s.plan.archive_name.clone(), s.plan.archive_ok_needed()))
                    .unwrap_or_else(|| (ARCHIVE_DIR.to_string(), Vec::new()));
                let reasons: String = why.iter().map(|w| format!("\n- {w}")).collect();
                let mut what = String::new();
                if *count > 0 {
                    what += &format!(
                        "{} in the backup folder {} no longer on the device. ",
                        format::files(*count),
                        format::pick(*count, "is", "are")
                    );
                }
                if *changed > 0 {
                    what += &format!(
                        "{} changed on the device, so {} replaced. ",
                        format::files(*changed),
                        format::pick(*changed, "its old copy is", "their old copies are")
                    );
                }
                let mode = self
                    .scan
                    .as_ref()
                    .map(|s| s.plan.deleted_files)
                    .unwrap_or_default();
                let bin = pnk_core::platform::BIN_NAME;
                let (verb, goes, button) = match mode {
                    DeletedFiles::Archive => (
                        "Archive",
                        format!("They will be moved into \"{archive}\\<date and time>\"; nothing is deleted."),
                        "Sync and archive",
                    ),
                    DeletedFiles::RecycleBin => (
                        "Send to the bin",
                        format!("They will be sent to the {bin}, where they can be restored; nothing is deleted for good."),
                        "Sync and send to the bin",
                    ),
                    DeletedFiles::LeaveInPlace => (
                        "Rename",
                        "Each old copy will be renamed beside the new one, \"(old <date and time>)\" before the extension; nothing is deleted.".to_string(),
                        "Sync and rename",
                    ),
                };
                let title = match (*count > 0, *changed > 0) {
                    (true, true) => format!("{verb} deleted and replaced files?"),
                    (true, false) => {
                        format!(
                            "{verb} {} deleted {}?",
                            format::count(*count),
                            format::pick(*count, "file", "files")
                        )
                    }
                    _ => format!(
                        "Replace {} changed {}?",
                        format::count(*changed),
                        format::pick(*changed, "file", "files")
                    ),
                };
                (
                    title,
                    format!(
                        "{what}{goes}\n\n\
                         Asking first because:{reasons}\n\n\
                         If the phone was locked, or iCloud keeps some photos only online \
                         (Optimize iPhone Storage), files can look deleted when they aren't. \
                         The Deleted from device tab lists them. Without an OK, changed files \
                         are left as they are."
                    ),
                    button,
                )
            }
            Confirm::RebuildCache(_) => (
                "Rebuild the modified-time cache?".to_string(),
                "The next scan reads every file's date again, then saves a fresh cache. Nothing is copied or deleted.".to_string(),
                "Rebuild",
            ),
            Confirm::ClaimFolder => {
                let why = self
                    .scan
                    .as_ref()
                    .and_then(|s| s.plan.folder_blocked())
                    .unwrap_or_default();
                (
                    "Use this folder for this device?".to_string(),
                    format!(
                        "Nothing is copied into this folder or archived in it: {why}.\n\n\
                         If this device replaces that device, make the folder this device's. The \
                         folder is scanned again, and from then on files in it that aren't on this \
                         device count as deleted from it. No files are moved now."
                    ),
                    "Use for this device",
                )
            }
            Confirm::Recopy(i) => {
                let name = self
                    .scan
                    .as_ref()
                    .and_then(|s| s.plan.entries.get(*i))
                    .map(|e| e.target.as_str().to_string())
                    .unwrap_or_default();
                // Say where this device's setting sends the old copy, so a bin is never a surprise.
                let goes = match self.scan.as_ref().map(|s| s.plan.deleted_files) {
                    Some(DeletedFiles::LeaveInPlace) => {
                        "renamed beside the new one, with the date in its name".to_string()
                    }
                    Some(DeletedFiles::RecycleBin) => format!(
                        "sent to the {} (you can restore it from there)",
                        pnk_core::platform::BIN_NAME
                    ),
                    _ => "moved into the archive folder".to_string(),
                };
                (
                    "Copy this file again?".to_string(),
                    format!(
                        "A fresh copy of {name} will be made from the device. The old backup copy is \
                         first {goes}, so nothing is overwritten or lost. If the copy fails, the old \
                         backup stays as it is."
                    ),
                    "Copy again",
                )
            }
        };
        // None: still open. Some(None): cancelled. Some(Some(alt)): chosen.
        let mut answer: Option<Option<bool>> = None;
        let modal = egui::Modal::new(egui::Id::unique("confirm")).show(ctx, |ui| {
            ui.set_max_width(460.0);
            ui.heading(title);
            ui.add_space(4.0);
            ui.label(text);
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.add(theme::primary(ok)).clicked() {
                    answer = Some(Some(false));
                }
                if let Some(alt) = alt_label
                    && ui.button(alt).clicked()
                {
                    answer = Some(Some(true));
                }
                if ui.button("Cancel").clicked() {
                    answer = Some(None);
                }
            });
        });
        if answer.is_none() && modal.should_close() {
            answer = Some(None);
        }
        match answer {
            Some(Some(alt)) => {
                if let Some(c) = self.confirm.take() {
                    self.confirmed(c, alt);
                }
            }
            Some(None) => self.confirm = None,
            None => {}
        }
    }

    /// The dialogs in [`Dialog`]. Nothing here happens until a button is clicked; Escape and
    /// Cancel do nothing.
    pub(super) fn dialog_ui(&mut self, ctx: &egui::Context) {
        let Some(d) = &mut self.dialog else { return };
        let mut choice: Option<Choice> = None;
        let modal = egui::Modal::new(egui::Id::unique("device-dialog")).show(ctx, |ui| {
            ui.set_max_width(460.0);
            match d {
                Dialog::Rename { text, .. } => {
                    ui.heading("Rename the device");
                    ui.add_space(4.0);
                    let edit = ui.add(
                        egui::TextEdit::singleline(text)
                            .margin(egui::Margin::symmetric(8, 4))
                            .desired_width(320.0),
                    );
                    edit.request_focus();
                    ui.add_space(8.0);
                    let enter = ui.input(|i| i.key_pressed(Key::Enter));
                    ui.horizontal(|ui| {
                        let named = !text.trim().is_empty();
                        if ui.add_enabled(named, theme::primary("Rename")).clicked()
                            || (enter && named)
                        {
                            choice = Some(Choice::Yes);
                        }
                        if ui.button("Cancel").clicked() {
                            choice = Some(Choice::No);
                        }
                    });
                }
                Dialog::Forget { name, .. } => {
                    ui.heading(format!("Forget \"{name}\"?"));
                    ui.add_space(4.0);
                    ui.label(if self.mode == Mode::S3 {
                        "This takes the device off the list on every PC that uses this bucket. Its folder in the bucket and everything in it, history included, stay where they are, and nothing on the device is touched. \"Add an existing backup…\" brings it back."
                    } else {
                        "This only takes the device off the app's list. Its backup folder and everything in it, history included, stay where they are, and nothing on the device is touched. \"Add an existing backup…\" brings it back."
                    });
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.add(theme::primary("Forget")).clicked() {
                            choice = Some(Choice::Yes);
                        }
                        if ui.button("Cancel").clicked() {
                            choice = Some(Choice::No);
                        }
                    });
                }
                Dialog::IdFile {
                    name,
                    after_question,
                    ..
                } => {
                    ui.heading(format!("Write an ID file to \"{name}\"?"));
                    ui.add_space(4.0);
                    if *after_question {
                        ui.label(
                            "So that you are not asked which device this is again, the app can write a small hidden file, .plug-n-keep-id, at the top of this stick. It is recognised by that file from then on, even after a reformat.",
                        );
                    } else {
                        ui.label(
                            "The app has nothing strong to recognise this stick by. It can write a small hidden file, .plug-n-keep-id, at the top of the stick, so it is recognised even after a reformat.",
                        );
                    }
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(
                            "This is the only thing the app ever writes to a device. It never overwrites anything, and never touches anything else on the stick. The scan never copies it. You can delete it yourself at any time.",
                        )
                        .color(theme::WEAK),
                    );
                    ui.add_space(8.0);
                    // No is the default: it is the filled button, and Escape is No.
                    ui.horizontal(|ui| {
                        if ui.add(theme::primary("No, thanks")).clicked() {
                            choice = Some(Choice::No);
                        }
                        if ui.button("Yes, write the ID file").clicked() {
                            choice = Some(Choice::Yes);
                        }
                    });
                }
                Dialog::RenameArchive { old, new, .. } => {
                    ui.heading("Rename the archive folder?");
                    ui.add_space(4.0);
                    ui.label(format!(
                        "This device's archive folder in the backup is called \"{old}\". Rename it to \"{new}\" too, as one move? If you only change the name in the settings, the old folder stays where it is and the next sync starts a new archive under the new name."
                    ));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.add(theme::primary("Rename the folder too")).clicked() {
                            choice = Some(Choice::Yes);
                        }
                        if ui.button("Only change the setting").clicked() {
                            choice = Some(Choice::Other);
                        }
                        if ui.button("Cancel").clicked() {
                            choice = Some(Choice::No);
                        }
                    });
                }
            }
        });
        if choice.is_none() && modal.should_close() {
            choice = Some(Choice::No);
        }
        let Some(choice) = choice else { return };
        let Some(d) = self.dialog.take() else { return };
        self.answered(d, choice);
    }

    fn answered(&mut self, d: Dialog, choice: Choice) {
        match (d, choice) {
            (Dialog::Rename { id, text }, Choice::Yes) => {
                self.send(Command::Rename { id, name: text });
            }
            (Dialog::Forget { id, name }, Choice::Yes) => {
                // The dialog may have sat open while a scan or sync started.
                if !self.idle() {
                    self.error = Some(format!(
                        "\"{name}\" was not forgotten: something is running. Try again when it has finished."
                    ));
                    return;
                }
                info!("Forget \"{name}\" confirmed");
                self.history.remove(&id);
                self.sizes.remove(&id);
                if self.shown_device().is_some_and(|s| s.id == id) {
                    self.clear_scan();
                }
                if matches!(&self.selected, Some(Sel::Known(k)) if *k == id) {
                    self.selected = None;
                }
                self.send(Command::Forget { id });
            }
            (Dialog::IdFile { name, device, .. }, Choice::Yes) => {
                // The user's explicit Yes: the only way an ID file is written.
                info!("Write an ID file on \"{name}\": Yes");
                self.send(Command::WriteIdFile { device });
            }
            (Dialog::IdFile { name, .. }, _) => info!("Write an ID file on \"{name}\": No"),
            (
                Dialog::RenameArchive {
                    id,
                    plan_id,
                    new,
                    settings,
                    ..
                },
                Choice::Yes,
            ) => {
                // The folder first (it needs the scan); the other settings follow if it worked.
                self.after_rename = Some((id, settings));
                self.send(Command::RenameArchive {
                    plan_id,
                    new_name: new,
                });
            }
            (Dialog::RenameArchive { id, settings, .. }, Choice::Other) => {
                self.clear_scan();
                self.send(Command::SetSettings { id, settings });
                self.notice = Some("Settings saved. They apply from the next scan.".into());
            }
            (_, _) => {}
        }
    }
}
