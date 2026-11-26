//! Where a backup goes. Each mode offers one kind of place: Local a folder on this PC, S3 a
//! folder in the bucket set up under Settings. Used by the new-device card, Change…, "Add an
//! existing backup…" and the Settings page (the S3 server fields and their Test button). Also
//! those dialogs and the overlap checks.

use pnk_core::modes::Mode;
use pnk_core::store::s3::provider::Provider;
use pnk_core::store::{Destination, S3Config};

use super::*;
use crate::s3::{self, Fld, S3Form, TestOutcome};
use crate::validate::{check_backup_folder, folder_note};

/// A backup place as it is typed: a folder (Local mode) or, in S3 mode, only the name of the
/// device's folder, which goes directly inside the base folder of the connection.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct PlaceForm {
    pub mode: Mode,
    pub folder: String,
    /// S3 mode: the connection as Settings has it. `None` while it is not set up.
    pub base: Option<S3Config>,
    /// S3 mode: the folder's name inside the base folder.
    pub name: String,
    /// The folder name was made from the device's name and is still that: it follows the name
    /// while the user has not changed it.
    pub auto_name: String,
}

impl PlaceForm {
    pub fn new(mode: Mode, folder: String, base: Option<S3Config>, name: String) -> Self {
        Self {
            mode,
            folder,
            base,
            auto_name: name.clone(),
            name,
        }
    }

    /// Starts on what a device has now. A bucket place that is not directly inside the base
    /// folder starts on `fallback_name` instead.
    pub fn of(
        mode: Mode,
        dest: Option<&Destination>,
        fallback_folder: String,
        base: Option<S3Config>,
        fallback_name: String,
    ) -> Self {
        let mut f = Self::new(mode, fallback_folder, base, fallback_name);
        match (mode, dest) {
            (Mode::Local, Some(Destination::Local(p))) => f.folder = p.display().to_string(),
            (Mode::S3, Some(Destination::S3(c))) => {
                if let Some(name) = f
                    .base
                    .as_ref()
                    .and_then(|b| pnk_core::store::dest::directly_under(b, c))
                {
                    f.name = name;
                    f.auto_name = String::new();
                }
            }
            _ => {}
        }
        f
    }

    /// While the folder name is still the made-up one, it follows the device's name.
    pub fn follow_name(&mut self, device_name: &str) {
        if self.name == self.auto_name {
            let n = crate::validate::folder_name_for(device_name);
            self.name = n.clone();
            self.auto_name = n;
        }
    }

    /// Settings were saved: take the connection as it is now.
    pub fn set_base(&mut self, base: Option<S3Config>) {
        self.base = base;
    }

    /// Whether a place can be made at all: S3 needs its connection set up first.
    pub fn ready(&self) -> bool {
        self.mode == Mode::Local || self.base.is_some()
    }

    /// The place this says, or what is wrong with it. `own`: the device it is for (its own place
    /// doesn't count as overlapping).
    pub fn destination(
        &self,
        known: &[KnownDevice],
        own: Option<&str>,
    ) -> Result<Destination, String> {
        match self.mode {
            Mode::Local => {
                let dir = check_backup_folder(&self.folder)?;
                let note = folder_note(&dir, own, known);
                if note.blocks() {
                    return Err(note.text().unwrap_or_default());
                }
                Ok(Destination::Local(dir))
            }
            Mode::S3 => {
                let dest = Destination::S3(self.bucket_place()?);
                if let Some(other) = s3::overlapping_device(known, own, &dest) {
                    return Err(s3::overlap_text(&other.name));
                }
                Ok(dest)
            }
        }
    }

    /// The device's folder directly inside the base folder, with the name typed here.
    pub fn bucket_place(&self) -> Result<S3Config, String> {
        let base = self.base.as_ref().ok_or(NOT_SET_UP)?;
        pnk_core::bucket_list::place_under(base, &self.name)
    }
}

/// Said where an S3 place is wanted before the server is set up.
pub(super) const NOT_SET_UP: &str = "The S3 server isn't set up yet. Set it up in Settings first.";

/// What the Test button last did, and for which fields: the answer is shown only while the
/// fields are still those.
#[derive(Default)]
pub(super) struct TestState {
    tested: Option<S3Form>,
    running: bool,
    outcome: Option<TestOutcome>,
}

impl TestState {
    pub fn finished(&mut self, outcome: TestOutcome) {
        self.running = false;
        self.outcome = Some(outcome);
    }
}

/// Which dialog the place form is in.
pub(super) enum PlaceDialogKind {
    /// "Change…" for this device.
    Change { id: String, name: String },
    /// "Add an existing backup…".
    AddExisting,
    /// "Find it…": the device's backup moved; its marker must confirm the new place.
    Find { id: String, name: String },
}

pub(super) struct PlaceDialog {
    pub kind: PlaceDialogKind,
    pub form: PlaceForm,
    pub problem: Option<String>,
}

fn label(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).color(theme::WEAK));
}

fn edit(ui: &mut egui::Ui, text: &mut String, hint: &str) -> egui::Response {
    ui.add(
        egui::TextEdit::singleline(text)
            .hint_text(hint)
            .margin(egui::Margin::symmetric(8, 4))
            .desired_width(320.0),
    )
}

/// A region: typed, or picked from the provider's list.
fn region_edit(ui: &mut egui::Ui, form: &mut S3Form, regions: &[&str]) {
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(&mut form.region)
                .hint_text("us-east-1")
                .margin(egui::Margin::symmetric(8, 4))
                .desired_width(190.0),
        );
        if !regions.is_empty() {
            egui::ComboBox::from_id_salt(("s3-regions", form.provider.label()))
                .selected_text("Choose…")
                .width(110.0)
                .show_ui(ui, |ui| {
                    for r in regions {
                        if ui.selectable_label(form.region == *r, *r).clicked() {
                            form.region = (*r).to_string();
                        }
                    }
                });
        }
    });
}

impl App {
    /// The "S3 server" fields of the Settings page: the provider, then only the fields that
    /// provider uses, named as its own console names them.
    fn s3_server_fields(&mut self, ui: &mut egui::Ui, form: &mut S3Form) {
        egui::Grid::new("s3-server")
            .num_columns(2)
            .spacing([12.0, 7.0])
            .min_col_width(110.0)
            .show(ui, |ui| {
                label(ui, "Provider");
                egui::ComboBox::from_id_salt("s3-provider")
                    .selected_text(form.provider.label())
                    .width(320.0)
                    .show_ui(ui, |ui| {
                        for p in Provider::ALL {
                            ui.selectable_value(&mut form.provider, p, p.label());
                        }
                    });
                ui.end_row();
                for f in s3::main_fields(form.provider) {
                    label(ui, f.label());
                    match f {
                        Fld::Region => region_edit(ui, form, form.provider.regions()),
                        Fld::AccountId => {
                            edit(ui, &mut form.account_id, "from your Cloudflare dashboard")
                                .on_hover_text("The Account ID shown in the R2 section of the Cloudflare dashboard.");
                        }
                        Fld::Endpoint => {
                            edit(ui, &mut form.server, "s3.us-west-004.backblazeb2.com")
                                .on_hover_text("The Endpoint shown on the bucket's page.");
                        }
                        Fld::Server => {
                            edit(ui, &mut form.server, "http://s3.example.com:9000")
                                .on_hover_text("The S3 server's address, with http:// or https://.");
                        }
                        Fld::Bucket => {
                            edit(ui, &mut form.bucket, "backups");
                        }
                    }
                    ui.end_row();
                }
                label(ui, "Base folder");
                edit(ui, &mut form.prefix, "plug-n-keep/").on_hover_text(
                    "Each device gets a folder of its own inside this one, in the bucket. Can be empty.",
                );
                ui.end_row();
                label(ui, "Profile");
                edit(ui, &mut form.profile, "default").on_hover_text(
                    "Which profile of the credentials file to use. The keys are never kept by this app.",
                );
                ui.end_row();
                ui.label("");
                ui.label(
                    RichText::new(form.provider.keys_help())
                        .small()
                        .color(theme::WEAK),
                );
                ui.end_row();
                label(ui, "Credentials file");
                ui.horizontal(|ui| {
                    edit(ui, &mut form.credentials_file, "optional: ~/.aws/credentials");
                    if ui.button("Browse…").clicked() {
                        let mut dialog =
                            rfd::FileDialog::new().set_title("Choose the credentials file");
                        if let Some(dir) = std::path::Path::new(form.credentials_file.trim())
                            .parent()
                            .filter(|p| p.is_dir())
                        {
                            dialog = dialog.set_directory(dir);
                        }
                        if let Some(f) = dialog.pick_file() {
                            form.credentials_file = f.display().to_string();
                        }
                    }
                });
                ui.end_row();
            });
        if !s3::more_fields(form.provider).is_empty() {
            egui::CollapsingHeader::new(RichText::new("More").color(theme::WEAK))
                .id_salt("s3-more")
                .show(ui, |ui| {
                    egui::Grid::new("s3-more-fields")
                        .num_columns(2)
                        .spacing([12.0, 7.0])
                        .min_col_width(110.0)
                        .show(ui, |ui| {
                            label(ui, "Region");
                            edit(ui, &mut form.region, "us-east-1");
                            ui.end_row();
                            label(ui, "Addressing");
                            ui.checkbox(
                                &mut form.path_style,
                                "Path-style addressing (needed by most servers you run yourself)",
                            );
                            ui.end_row();
                            label(ui, "CA file");
                            ui.horizontal(|ui| {
                                edit(ui, &mut form.ca_file, "optional: for a self-signed server");
                                if ui.button("Browse…").clicked()
                                    && let Some(f) = rfd::FileDialog::new()
                                        .set_title("Choose the CA file")
                                        .pick_file()
                                {
                                    form.ca_file = f.display().to_string();
                                }
                            });
                            ui.end_row();
                        });
                });
        }
    }

    /// The Test button and what it found, for `form`.
    fn s3_test_row(&mut self, ui: &mut egui::Ui, form: &S3Form) {
        ui.horizontal(|ui| {
            let running = self.s3_test.running;
            if ui
                .add_enabled(!running, egui::Button::new("Test"))
                .on_hover_text(
                    "Opens the bucket and checks that the server can be used. It writes and removes a few small files; nothing else is touched.",
                )
                .clicked()
            {
                self.s3_test.outcome = None;
                self.s3_test.tested = Some(form.clone());
                match form.config() {
                    Ok(config) => {
                        self.s3_test.running = true;
                        self.send(Command::TestS3 { config });
                    }
                    Err(e) => self.s3_test.outcome = Some(TestOutcome::BadSettings(e)),
                }
            }
            if running {
                ui.add(egui::Spinner::new().size(14.0));
                ui.label(RichText::new("Trying the bucket…").color(theme::WEAK));
            } else if let Some(o) = self.s3_test.outcome.as_ref()
                && self.s3_test.tested.as_ref() == Some(form)
            {
                ui.label(RichText::new(o.text()).color(if o.is_ok() {
                    theme::GOOD
                } else {
                    theme::DANGER
                }));
            }
        });
    }

    /// The S3 server section of Settings: the fields and the Test button.
    pub(super) fn s3_server_ui(&mut self, ui: &mut egui::Ui, form: &mut S3Form) {
        self.s3_server_fields(ui, form);
        ui.add_space(4.0);
        self.s3_test_row(ui, &form.clone());
    }

    /// The place fields of the current mode. `folder_title` is for the folder picker. With
    /// `settings_link`, an S3 mode that isn't set up says so and offers to open Settings;
    /// the answer is whether that was clicked.
    pub(super) fn place_ui(
        &mut self,
        ui: &mut egui::Ui,
        form: &mut PlaceForm,
        salt: &str,
        folder_title: &str,
        prefix_label: &str,
        settings_link: bool,
    ) -> bool {
        let mut open_settings = false;
        match form.mode {
            Mode::Local => {
                egui::Grid::new(("place-folder", salt))
                    .num_columns(2)
                    .spacing([12.0, 7.0])
                    .min_col_width(110.0)
                    .show(ui, |ui| {
                        label(ui, "Backup folder");
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut form.folder)
                                    .margin(egui::Margin::symmetric(8, 4))
                                    .desired_width(320.0),
                            );
                            if ui.button("Browse…").clicked() {
                                let mut dialog = rfd::FileDialog::new().set_title(folder_title);
                                let current = std::path::Path::new(form.folder.trim());
                                if let Some(parent) = current
                                    .parent()
                                    .filter(|p| p.is_dir())
                                    .or_else(|| current.is_dir().then_some(current))
                                {
                                    dialog = dialog.set_directory(parent);
                                }
                                if let Some(dir) = dialog.pick_folder() {
                                    form.folder = dir.display().to_string();
                                }
                            }
                        });
                        ui.end_row();
                    });
            }
            Mode::S3 => match form.base.clone() {
                None => {
                    ui.label(RichText::new(NOT_SET_UP).color(theme::CAUTION));
                    if settings_link && ui.button("Open Settings").clicked() {
                        open_settings = true;
                    }
                }
                Some(base) => {
                    egui::Grid::new(("place-bucket", salt))
                        .num_columns(2)
                        .spacing([12.0, 7.0])
                        .min_col_width(110.0)
                        .show(ui, |ui| {
                            label(ui, "Bucket");
                            ui.label(
                                RichText::new(s3::address(&S3Config {
                                    prefix: String::new(),
                                    ..base
                                }))
                                .color(theme::TEXT),
                            );
                            ui.end_row();
                            label(ui, "Base folder");
                            ui.label(RichText::new(if base.prefix.trim().is_empty() {
                                "(the top of the bucket)".to_string()
                            } else {
                                base.prefix.clone()
                            }).color(theme::TEXT));
                            ui.end_row();
                            label(ui, prefix_label);
                            edit(ui, &mut form.name, "my-phone").on_hover_text(
                                "The device's folder, directly inside the base folder. One name, with no / in it. Each device needs a folder of its own.",
                            );
                            ui.end_row();
                            if let Ok(place) = form.bucket_place() {
                                label(ui, "Backs up to");
                                ui.label(
                                    RichText::new(s3::address(&place)).small().color(theme::WEAK),
                                );
                                ui.end_row();
                            }
                        });
                }
            },
        }
        open_settings
    }

    /// The place form for a device in the current mode, when nothing is set yet.
    fn blank_place(&self, folder: String, name: String) -> PlaceForm {
        PlaceForm::new(self.mode, folder, self.config.default_s3.clone(), name)
    }

    /// "Change…": any place the user gives becomes the device's backup place. The scan checks
    /// whose it is.
    pub(super) fn choose_backup_place(&mut self, d: &KnownDevice) {
        let folder = d.backup_dir.as_ref().map_or_else(
            || self.backups_root().display().to_string(),
            |p| p.display().to_string(),
        );
        let form = PlaceForm::of(
            self.mode,
            d.destination().as_ref(),
            folder,
            self.config.default_s3.clone(),
            crate::validate::folder_name_for(&d.name),
        );
        self.s3_test = TestState::default();
        self.place_dialog = Some(PlaceDialog {
            kind: PlaceDialogKind::Change {
                id: d.id.clone(),
                name: d.name.clone(),
            },
            form,
            problem: None,
        });
    }

    /// "Find it…" for a device whose bucket place is gone: where did it go?
    pub(super) fn find_backup_place(&mut self, d: &KnownDevice) {
        self.choose_backup_place(d);
        if let Some(dlg) = &mut self.place_dialog {
            dlg.kind = PlaceDialogKind::Find {
                id: d.id.clone(),
                name: d.name.clone(),
            };
        }
    }

    /// "Add an existing backup…": in Local mode a folder; in S3 mode the base folder of the
    /// bucket is looked at again (nothing to pick), and a forgotten device comes back.
    pub(super) fn add_existing_backup(&mut self) {
        if self.mode == Mode::S3 {
            match self.config.default_s3.clone() {
                Some(c) => {
                    self.error = None;
                    self.notice = Some("Looking in the base folder…".into());
                    self.send(Command::AddExisting {
                        dir: Destination::S3(c),
                    });
                }
                None => self.error = Some(NOT_SET_UP.to_string()),
            }
            return;
        }
        self.s3_test = TestState::default();
        self.place_dialog = Some(PlaceDialog {
            kind: PlaceDialogKind::AddExisting,
            form: self.blank_place(self.backups_root().display().to_string(), String::new()),
            problem: None,
        });
    }

    /// The two dialogs that hold a place form. Nothing happens until a button is clicked.
    pub(super) fn place_dialog_ui(&mut self, ctx: &egui::Context) {
        let Some(mut dlg) = self.place_dialog.take() else {
            return;
        };
        let (title, ok, folder_title, prefix_label) = match &dlg.kind {
            PlaceDialogKind::Change { name, .. } => (
                format!("Where does \"{name}\" back up to?"),
                "Use this place",
                "Choose the backup folder",
                "Folder name",
            ),
            PlaceDialogKind::Find { name, .. } => (
                format!("Where did the backup of \"{name}\" go?"),
                "Look here",
                "Choose the folder the backup was moved to",
                "Folder name",
            ),
            PlaceDialogKind::AddExisting => (
                "Add an existing backup".to_string(),
                "Add",
                "Choose a backup folder, or the folder that holds backup folders",
                "Folder name",
            ),
        };
        let mut answer: Option<bool> = None;
        let busy = !self.idle();
        let modal = egui::Modal::new(egui::Id::unique("place-dialog")).show(ctx, |ui| {
            ui.set_max_width(560.0);
            ui.heading(&title);
            ui.add_space(4.0);
            ui.label(
                RichText::new(match &dlg.kind {
                    PlaceDialogKind::Change { .. } if dlg.form.mode == Mode::S3 => {
                        "Choose a folder name directly inside the base folder, so other PCs find the device. The old folder's files are left where they are and nothing is moved: the next sync copies what is missing into the new folder."
                    }
                    PlaceDialogKind::Change { .. } => {
                        "The old place's files are left where they are. The next scan checks whose place it is."
                    }
                    PlaceDialogKind::Find { .. } if dlg.form.mode == Mode::S3 => {
                        "Give the name of the folder, directly inside the base folder, that the backup was moved to. Its marker, the .plug-n-keep.json file, must say the backup is this device's."
                    }
                    PlaceDialogKind::Find { .. } => {
                        "Its marker, the .plug-n-keep.json file, must say the backup is this device's."
                    }
                    PlaceDialogKind::AddExisting => {
                        "Pick a backup (such as one copied from another PC) or a place that holds several. Each device is set up again from the files in it, with its settings and history."
                    }
                })
                .color(theme::WEAK),
            );
            ui.add_space(6.0);
            self.place_ui(ui, &mut dlg.form, "dialog", folder_title, prefix_label, false);
            if let Some(p) = &dlg.problem {
                ui.add_space(4.0);
                ui.label(RichText::new(p).color(theme::DANGER));
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!busy && dlg.form.ready(), theme::primary(ok))
                    .clicked()
                {
                    answer = Some(true);
                }
                if ui.button("Cancel").clicked() {
                    answer = Some(false);
                }
            });
        });
        if answer.is_none() && modal.should_close() {
            answer = Some(false);
        }
        match answer {
            None => self.place_dialog = Some(dlg),
            Some(false) => {}
            Some(true) => {
                let own = match &dlg.kind {
                    PlaceDialogKind::Change { id, .. } | PlaceDialogKind::Find { id, .. } => {
                        Some(id.as_str())
                    }
                    PlaceDialogKind::AddExisting => None,
                };
                // Adding looks for backups, so what it is pointed at may be a known place.
                let checked = match dlg.kind {
                    PlaceDialogKind::AddExisting => match dlg.form.mode {
                        Mode::Local => {
                            check_backup_folder(&dlg.form.folder).map(Destination::Local)
                        }
                        Mode::S3 => dlg.form.bucket_place().map(Destination::S3),
                    },
                    PlaceDialogKind::Change { .. } | PlaceDialogKind::Find { .. } => {
                        dlg.form.destination(&self.known, own)
                    }
                };
                match checked {
                    Err(e) => {
                        dlg.problem = Some(e);
                        self.place_dialog = Some(dlg);
                    }
                    Ok(dest) => match dlg.kind {
                        PlaceDialogKind::Change { id, name } => {
                            info!(
                                "Backup place chosen for \"{name}\": {}",
                                dest.label().display()
                            );
                            self.clear_scan();
                            self.send(Command::SetDestination { id, dest });
                        }
                        PlaceDialogKind::Find { id, name } => {
                            info!("Pointing \"{name}\" at {}", dest.label().display());
                            self.clear_scan();
                            self.send(Command::Relocate { id, dest });
                        }
                        PlaceDialogKind::AddExisting => {
                            info!("Adding the existing backup in {}", dest.label().display());
                            self.send(Command::AddExisting { dir: dest });
                        }
                    },
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> S3Config {
        S3Form {
            server: "http://h:9000".into(),
            bucket: "b".into(),
            prefix: "plug-n-keep/".into(),
            ..S3Form::default()
        }
        .config()
        .unwrap()
    }

    #[test]
    fn local_mode_makes_folders_and_ignores_any_s3_settings() {
        let t = tempfile::tempdir().unwrap();
        let mut f = PlaceForm::new(
            Mode::Local,
            t.path().join("phone").display().to_string(),
            Some(base()),
            "x/".into(),
        );
        assert!(matches!(
            f.destination(&[], None).unwrap(),
            Destination::Local(_)
        ));
        f.folder = "relative".into();
        assert!(f.destination(&[], None).is_err());
    }

    #[test]
    fn s3_mode_makes_a_folder_directly_inside_the_base_folder_from_a_name() {
        let mut f = PlaceForm::new(Mode::S3, String::new(), Some(base()), String::new());
        f.follow_name("Pixel 8");
        assert_eq!(f.name, "Pixel 8");
        // It follows the device's name until it is typed over.
        f.follow_name("Pixel 9");
        assert_eq!(f.name, "Pixel 9");
        f.name = "mine".into();
        f.follow_name("Other");
        assert_eq!(f.name, "mine");
        let Destination::S3(c) = f.destination(&[], None).unwrap() else {
            panic!("a bucket place");
        };
        assert_eq!(
            (c.endpoint.as_str(), c.bucket.as_str(), c.prefix.as_str()),
            ("http://h:9000", "b", "plug-n-keep/mine/")
        );
        // Only a name: nothing deeper, nothing odd.
        for bad in ["a/b", "a/../b", "..", "", "x:y", ".plug-n-keep-z"] {
            f.name = bad.into();
            assert!(f.destination(&[], None).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_name_that_is_another_devices_folder_is_refused() {
        let mut other = KnownDevice::new(
            pnk_core::signals::DeviceKind::Ptp,
            "Pixel",
            &pnk_core::signals::Signals::default(),
            "now",
        );
        other.set_destination(Destination::S3(S3Config {
            prefix: "plug-n-keep/Pixel/".into(),
            ..base()
        }));
        let mut f = PlaceForm::new(Mode::S3, String::new(), Some(base()), "Pixel".into());
        assert!(f.destination(std::slice::from_ref(&other), None).is_err());
        // Its own folder is fine for itself.
        assert!(
            f.destination(std::slice::from_ref(&other), Some(&other.id))
                .is_ok()
        );
        f.name = "Pixel 2".into();
        assert!(f.destination(std::slice::from_ref(&other), None).is_ok());
    }

    #[test]
    fn s3_mode_without_a_server_cannot_make_a_place_and_says_where_to_set_it_up() {
        let f = PlaceForm::new(Mode::S3, String::new(), None, String::new());
        assert!(!f.ready());
        assert_eq!(f.destination(&[], None).unwrap_err(), NOT_SET_UP);
        assert!(NOT_SET_UP.contains("Settings"));
        assert!(PlaceForm::new(Mode::Local, String::new(), None, String::new()).ready());
    }

    #[test]
    fn a_form_opened_before_the_server_was_set_up_takes_it_when_settings_are_saved() {
        // The new-device card was made while Settings had no server.
        let mut f = PlaceForm::new(Mode::S3, String::new(), None, "iPhone".into());
        assert!(!f.ready());
        f.set_base(Some(base()));
        assert!(f.ready());
        assert_eq!(
            f.destination(&[], None).map(|d| d.label()).unwrap(),
            std::path::PathBuf::from("s3://b/plug-n-keep/iPhone/")
        );
    }

    #[test]
    fn a_form_starts_on_the_devices_own_place_in_its_mode() {
        let mut d = KnownDevice::new(
            pnk_core::signals::DeviceKind::Ptp,
            "P",
            &pnk_core::signals::Signals::default(),
            "now",
        );
        d.set_destination(Destination::S3(S3Config {
            prefix: "plug-n-keep/pixel/".into(),
            ..base()
        }));
        let f = PlaceForm::of(
            Mode::S3,
            d.destination().as_ref(),
            String::new(),
            Some(base()),
            "fallback".into(),
        );
        assert_eq!(f.name, "pixel");
        // A place that is not directly inside the base folder starts on a name to move to.
        d.set_destination(Destination::S3(S3Config {
            prefix: "elsewhere/pixel/".into(),
            ..base()
        }));
        let f = PlaceForm::of(
            Mode::S3,
            d.destination().as_ref(),
            String::new(),
            Some(base()),
            "fallback".into(),
        );
        assert_eq!(f.name, "fallback");
        // A place of the other kind is not carried over.
        let f = PlaceForm::of(
            Mode::Local,
            d.destination().as_ref(),
            "D:/Backups".into(),
            None,
            String::new(),
        );
        assert_eq!(f.folder, "D:/Backups");
    }
}
