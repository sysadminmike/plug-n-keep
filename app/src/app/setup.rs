//! What a plugged-in device needs before it can be backed up: the questions recognition may ask
//! (DESIGN.md section 5.2) and the new-device card (section 8).

use pnk_core::modes::Mode;
use pnk_core::registry::DeviceSettings;
use pnk_core::store::{Destination, S3Config};

use super::place::PlaceForm;
use super::settings::{Fields, fields_ui};
use super::*;
use crate::questions::{Answer, Question, question_text};
use crate::validate::{FolderNote, check_backup_folder, folder_note, suggested_folder};
use crate::worker::{NewDevice, Setup};

/// The new-device card's working copy.
pub(super) struct SetupForm {
    name: String,
    /// Where the backup goes: a folder (Local mode) or a folder in the bucket (S3 mode).
    place: PlaceForm,
    fields: Fields,
    kind: DeviceKind,
    /// The ID on the stick came from another PC: adopt it.
    foreign_id: Option<String>,
    use_stick_id: bool,
    /// The stick carries this known device's ID: it may be a copy.
    clash: Option<String>,
    note: FolderNote,
    /// The folder text the note was worked out for.
    noted: String,
    /// "It's one I already back up…" is open.
    linking: bool,
    problem: Option<String>,
}

impl SetupForm {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        setup: &Setup,
        kind: DeviceKind,
        signals: &pnk_core::signals::Signals,
        defaults: &NewDeviceDefaults,
        root: &std::path::Path,
        known: &[KnownDevice],
        mode: Mode,
        s3_server: Option<&S3Config>,
    ) -> Self {
        let folder = suggested_folder(root, &setup.suggested_name, known);
        let folder_name = crate::validate::folder_name_for(&setup.suggested_name);
        Self {
            name: setup.suggested_name.clone(),
            place: PlaceForm::new(
                mode,
                folder.display().to_string(),
                s3_server.cloned(),
                folder_name,
            ),
            fields: Fields::new(&defaults.settings_for(kind, signals)),
            kind,
            foreign_id: setup.foreign_id.clone(),
            use_stick_id: setup.foreign_id.is_some(),
            clash: setup.id_clash.as_ref().map(|n| n.name.clone()),
            note: FolderNote::Fine,
            noted: String::new(),
            linking: false,
            problem: None,
        }
    }

    /// Settings were saved: the card takes the S3 server as it is now.
    pub fn set_s3_server(&mut self, s3_server: Option<&S3Config>) {
        self.place.set_base(s3_server.cloned());
    }

    /// The worker says something about the stick's ID changed (a new one was written).
    pub fn refresh(&mut self, setup: &Setup) {
        self.foreign_id = setup.foreign_id.clone();
        self.use_stick_id = setup.foreign_id.is_some();
        self.clash = setup.id_clash.as_ref().map(|n| n.name.clone());
    }
}

impl App {
    /// "Looks like X, but its format ID changed…": the question, and its buttons.
    pub(super) fn question_card(&mut self, ui: &mut egui::Ui, q: &Question) {
        let label = match &self.device {
            DeviceState::Open(o) => o.identity.label(),
            _ => String::new(),
        };
        ui.add_space(4.0);
        ui.label(
            RichText::new("Which device is this?")
                .size(17.0)
                .family(theme::semibold())
                .color(theme::TEXT),
        );
        if !label.is_empty() {
            ui.label(RichText::new(label).color(theme::WEAK));
        }
        ui.add_space(8.0);
        let mut reply: Option<Answer> = None;
        theme::card(ui, |ui| {
            ui.label(RichText::new(question_text(q)).color(theme::TEXT));
            ui.add_space(6.0);
            match q {
                Question::Which { candidates } => {
                    for c in candidates {
                        if ui
                            .button(format!("It's \"{}\"", c.name))
                            .on_hover_text("Remember what this device looks like now, for this one")
                            .clicked()
                        {
                            reply = Some(Answer::Pick(c.id.clone()));
                        }
                    }
                    ui.add_space(2.0);
                    if ui.button("A new device").clicked() {
                        reply = Some(Answer::NewDevice);
                    }
                }
                Question::Changed { candidate, .. } => {
                    ui.horizontal(|ui| {
                        if ui
                            .add(theme::primary(format!("Yes, it's \"{}\"", candidate.name)))
                            .on_hover_text("Learn the new values: it is recognised next time")
                            .clicked()
                        {
                            reply = Some(Answer::Yes);
                        }
                        if ui
                            .button("A different device")
                            .on_hover_text(
                                "It is not proposed as that device again; set it up as a new one",
                            )
                            .clicked()
                        {
                            reply = Some(Answer::No);
                        }
                    });
                }
                Question::Maybe { candidate } => {
                    ui.horizontal(|ui| {
                        if ui
                            .add(theme::primary(format!("Yes, it's \"{}\"", candidate.name)))
                            .on_hover_text("Archiving will still ask before it moves anything, because only weak signals agree")
                            .clicked()
                        {
                            reply = Some(Answer::Yes);
                        }
                        if ui.button("No").clicked() {
                            reply = Some(Answer::No);
                        }
                    });
                }
            }
        });
        ui.label(
            RichText::new("Nothing is copied or changed until you answer.").color(theme::FAINT),
        );
        if let Some(r) = reply {
            info!("Question answered: {r:?}");
            self.send(Command::Answer {
                device: self.current.clone().unwrap_or_default(),
                reply: r,
            });
        }
    }

    /// The new-device card: suggestions for everything, then Save and scan.
    pub(super) fn setup_card(&mut self, ui: &mut egui::Ui) {
        let Some(mut form) = self.setup.take() else {
            ui.add_space(8.0);
            ui.label(RichText::new("Preparing…").color(theme::WEAK));
            return;
        };
        let (label, is_volume) = match &self.device {
            DeviceState::Open(o) => (o.identity.label(), o.is_volume),
            _ => (String::new(), false),
        };
        let idle = self.idle();
        let jpeg = self.heic_ok();
        // The note about the folder is worked out when the text changes, not every frame.
        if form.place.mode == Mode::Local && form.noted != form.place.folder {
            form.noted = form.place.folder.clone();
            form.note = match check_backup_folder(&form.place.folder) {
                Ok(p) => folder_note(&p, None, &self.known),
                Err(_) => FolderNote::Fine,
            };
        }
        // The folder in the bucket follows the device's name until it is typed over.
        form.place.follow_name(&form.name);
        ui.add_space(4.0);
        ui.label(
            RichText::new("New device")
                .size(17.0)
                .family(theme::semibold())
                .color(theme::TEXT),
        );
        if !label.is_empty() {
            ui.label(RichText::new(format!("{label} · {}", form.kind.label())).color(theme::WEAK));
        }
        ui.add_space(8.0);
        let mut save = false;
        let mut link: Option<String> = None;
        let mut write_new_id = false;
        let mut open_settings = false;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                theme::card(ui, |ui| {
                    egui::Grid::new("new-device")
                        .num_columns(2)
                        .spacing([12.0, 7.0])
                        .min_col_width(110.0)
                        .show(ui, |ui| {
                            ui.label(RichText::new("Name").color(theme::WEAK));
                            ui.add(
                                egui::TextEdit::singleline(&mut form.name)
                                    .margin(egui::Margin::symmetric(8, 4))
                                    .desired_width(320.0),
                            );
                            ui.end_row();
                        });
                    ui.add_space(4.0);
                    open_settings = self.place_ui(
                        ui,
                        &mut form.place,
                        "new-device",
                        "Choose the backup folder",
                        "Folder name",
                        true,
                    );
                    if form.place.mode == Mode::Local
                        && let Some(text) = form.note.text()
                    {
                        ui.add_space(2.0);
                        ui.label(
                            RichText::new(text)
                                .color(if form.note.blocks() { theme::DANGER } else { theme::CAUTION }),
                        );
                    }
                    ui.add_space(6.0);
                    fields_ui(
                        ui,
                        &mut form.fields,
                        form.kind,
                        false,
                        &Vec::new(),
                        "new",
                        &jpeg,
                        form.place.mode == Mode::S3,
                    );
                });
                if let Some(name) = form.clash.clone() {
                    let text = format!(
                        "This stick carries the ID file of \"{name}\", so it may be a copy of it. Write a new ID on this stick so the two are told apart? It replaces the ID file on this stick only if you click Yes."
                    );
                    ui.add_space(2.0);
                    if is_volume {
                        if theme::warning(ui, &text, "Yes, write a new ID", idle) {
                            write_new_id = true;
                        }
                    } else {
                        let _ = theme::banner(ui, theme::Note::Warning, &text);
                    }
                }
                if form.foreign_id.is_some() {
                    ui.checkbox(
                        &mut form.use_stick_id,
                        "Use the ID on the stick as this device's ID, so both PCs know it as the same device",
                    )
                    .on_hover_text("Its ID file was written on another PC. Each PC still has its own backup folder.");
                }
                ui.add_space(6.0);
                if let Some(p) = &form.problem {
                    ui.label(RichText::new(p).color(theme::DANGER));
                }
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(idle && form.place.ready(), theme::primary("Save and scan"))
                        .on_hover_text("Remember this device and compare it with the backup folder. Nothing is copied until you click Start sync.")
                        .clicked()
                    {
                        save = true;
                    }
                    let can_link = !self.known.is_empty();
                    if ui
                        .add_enabled(can_link, egui::Button::new("It's one I already back up…"))
                        .on_hover_text("For example a memory card that was seen through a camera before")
                        .clicked()
                    {
                        form.linking = !form.linking;
                    }
                });
                if form.linking {
                    ui.add_space(4.0);
                    theme::card(ui, |ui| {
                        ui.label(RichText::new("Which device is it?").color(theme::WEAK));
                        for d in &self.known {
                            if ui.button(&d.name).clicked() {
                                link = Some(d.id.clone());
                            }
                        }
                    });
                }
            });
        if open_settings {
            self.open_settings();
        }
        if write_new_id {
            info!("Write a new ID on the copied stick: Yes");
            self.send(Command::ReplaceIdFile {
                device: self.current.clone().unwrap_or_default(),
            });
        }
        if let Some(id) = link {
            info!("It's one I already back up: {id}");
            self.send(Command::LinkExisting {
                device: self.current.clone().unwrap_or_default(),
                id,
            });
        } else if save {
            match self.new_device_from(&form) {
                Ok(n) => {
                    form.problem = None;
                    self.scan_when_ready = true;
                    self.send(Command::CreateDevice(Box::new(n)));
                }
                Err(e) => form.problem = Some(e),
            }
        }
        if matches!(&self.device, DeviceState::Open(o) if matches!(o.stage, Stage::Setup(_))) {
            self.setup = Some(form);
        }
    }

    /// What the card says, checked.
    fn new_device_from(&self, form: &SetupForm) -> Result<NewDevice, String> {
        let name = form.name.trim().to_string();
        if name.is_empty() {
            return Err("Give the device a name.".into());
        }
        let destination: Destination = form.place.destination(&self.known, None)?;
        let mut settings: DeviceSettings = form.fields.apply(&[]).map_err(|p| {
            p.into_iter()
                .map(|(what, why)| format!("{what}: {why}"))
                .collect::<Vec<_>>()
                .join(" ")
        })?;
        // A bucket has no Recycle Bin: those files are archived instead.
        if matches!(destination, Destination::S3(_))
            && settings.deleted_files == pnk_core::registry::DeletedFiles::RecycleBin
        {
            settings.deleted_files = pnk_core::registry::DeletedFiles::Archive;
        }
        Ok(NewDevice {
            device: self.current.clone().unwrap_or_default(),
            name,
            destination,
            settings,
            use_id_file_id: form.use_stick_id && form.foreign_id.is_some(),
        })
    }
}
