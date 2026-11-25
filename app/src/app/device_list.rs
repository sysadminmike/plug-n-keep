//! The device list on the left: plugged-in devices first with their status, then known devices
//! that are not plugged in, greyed out with their last sync. Each recognised device has a menu.

use std::path::Path;

use eframe::egui::text::LayoutJob;

use super::*;

/// One line of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ListRow {
    pub sel: Sel,
    pub name: String,
    pub status: String,
    pub tone: Tone,
    /// The known device this is, if it is one (it has a menu).
    pub device_id: Option<String>,
    pub connected: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tone {
    Good,
    Attention,
    Quiet,
    Bad,
}

/// "last sync 17 Nov 2026", or when it was last seen, or that it never synced.
pub(super) fn last_sync_text(d: &KnownDevice) -> String {
    if d.offline.is_some() {
        return "backup folder not found".to_string();
    }
    match &d.last_sync {
        Some(s) => format!("last sync {}", format::date(&s.when)),
        None => "not synced yet".to_string(),
    }
}

/// The rows of the list, in order: connected devices first (as they were listed), then the
/// known devices that are not plugged in (in the order they were added).
pub(super) fn build_rows(
    devices: &[ListedDevice],
    links: &HashMap<String, Link>,
    known: &[KnownDevice],
) -> Vec<ListRow> {
    let mut rows = Vec::new();
    let mut connected_ids: Vec<&str> = Vec::new();
    for d in devices {
        let link = links.get(&d.id).cloned().unwrap_or(Link::Waiting);
        let (name, status, tone, device_id) = match &link {
            Link::Recognised(id) => match known.iter().find(|k| &k.id == id) {
                Some(k) => {
                    connected_ids.push(&k.id);
                    let (status, tone) = if k.offline.is_some() {
                        ("folder not found", Tone::Attention)
                    } else {
                        ("connected", Tone::Good)
                    };
                    (k.name.clone(), status.to_string(), tone, Some(k.id.clone()))
                }
                None => (d.label.clone(), "connected".into(), Tone::Good, None),
            },
            Link::Waiting => (d.label.clone(), "waiting".into(), Tone::Quiet, None),
            Link::Checking => (d.label.clone(), "looking…".into(), Tone::Quiet, None),
            Link::Asking => (
                d.label.clone(),
                "needs an answer".into(),
                Tone::Attention,
                None,
            ),
            Link::NewDevice => (d.label.clone(), "new device".into(), Tone::Attention, None),
            Link::Failed(_) => (d.label.clone(), "can't be opened".into(), Tone::Bad, None),
        };
        rows.push(ListRow {
            sel: Sel::Connected(d.id.clone()),
            name,
            status,
            tone,
            device_id,
            connected: true,
        });
    }
    for k in known {
        if connected_ids.contains(&k.id.as_str()) {
            continue;
        }
        rows.push(ListRow {
            sel: Sel::Known(k.id.clone()),
            name: k.name.clone(),
            status: last_sync_text(k),
            tone: Tone::Quiet,
            device_id: Some(k.id.clone()),
            connected: false,
        });
    }
    rows
}

enum ListAction {
    Rename(String),
    Forget(String),
    OpenFolder(String),
    WriteIdFile(String),
}

/// Show a folder in the system's file manager.
fn open_folder(path: &Path) -> std::io::Result<()> {
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(program)
        .arg(path)
        .spawn()
        .map(|_| ())
}

impl App {
    pub(super) fn device_list(&mut self, ui: &mut egui::Ui) {
        ui.label(
            RichText::new("Devices")
                .family(theme::semibold())
                .color(theme::TEXT),
        );
        ui.add_space(6.0);
        let rows = build_rows(&self.devices, &self.links, &self.known);
        let mut chosen: Option<Sel> = None;
        let mut action: Option<ListAction> = None;
        let mut add_backup = false;
        let idle = self.idle();
        if rows.is_empty() {
            let text = if self.listing {
                "Looking for devices…"
            } else {
                "No device is plugged in, and none is known yet. Plug one in, or click Refresh."
            };
            ui.label(RichText::new(text).color(theme::WEAK));
        }
        if let Some(hint) = &self.device_hint {
            ui.label(RichText::new(hint).size(11.0).color(theme::WEAK));
            ui.add_space(4.0);
        }
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut shown_divider = false;
                for row in &rows {
                    if !row.connected && !shown_divider {
                        shown_divider = true;
                        if rows.iter().any(|r| r.connected) {
                            ui.add_space(6.0);
                        }
                        ui.label(
                            RichText::new("Not plugged in")
                                .size(11.0)
                                .color(theme::FAINT),
                        );
                        ui.add_space(2.0);
                    }
                    let selected = self.selected.as_ref() == Some(&row.sel);
                    let (clicked, act) = self.list_row(ui, row, selected, idle);
                    if clicked {
                        chosen = Some(row.sel.clone());
                    }
                    if act.is_some() {
                        action = act;
                    }
                }
                ui.add_space(8.0);
                if ui
                    .add_enabled(
                        idle,
                        egui::Button::new(RichText::new("Add an existing backup…").size(11.5)).small(),
                    )
                    .on_hover_text(
                        "Pick a backup folder, such as one copied from another PC. The device is set up again from the files in it.",
                    )
                    .clicked()
                {
                    add_backup = true;
                }
            });
        match chosen {
            Some(Sel::Connected(id)) => {
                info!("Device chosen from the list: {id}");
                self.select_connected(&id);
            }
            Some(Sel::Known(id)) => self.select_known(&id),
            None => {}
        }
        if let Some(a) = action {
            self.list_action(a);
        }
        if add_backup {
            self.add_existing_backup();
        }
    }

    /// "Add an existing backup…": the user picks a backup folder (or the folder that holds some),
    /// and the worker adds the devices it finds from the files in it.
    pub(super) fn add_existing_backup(&mut self) {
        let mut dialog = rfd::FileDialog::new()
            .set_title("Choose a backup folder, or the folder that holds backup folders");
        if self.backups_root().is_dir() {
            dialog = dialog.set_directory(self.backups_root());
        }
        let Some(dir) = dialog.pick_folder() else {
            return;
        };
        info!("Adding the existing backup in {}", dir.display());
        self.send(Command::AddExisting { dir });
    }

    /// One row. Returns whether it was clicked, and a menu choice.
    fn list_row(
        &self,
        ui: &mut egui::Ui,
        row: &ListRow,
        selected: bool,
        idle: bool,
    ) -> (bool, Option<ListAction>) {
        let width = ui.available_width().min(216.0);
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 40.0), egui::Sense::click());
        let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
        let painter = ui.painter();
        if selected {
            painter.rect(
                rect,
                6,
                Color32::WHITE,
                egui::Stroke::new(1.0, theme::BORDER),
                egui::StrokeKind::Inside,
            );
        } else if resp.hovered() {
            painter.rect_filled(rect, 6, Color32::WHITE.gamma_multiply(0.6));
        }
        let grey = !row.connected;
        let dot = match row.tone {
            Tone::Good => theme::GOOD,
            Tone::Attention => theme::CAUTION,
            Tone::Bad => theme::DANGER,
            Tone::Quiet => theme::FAINT,
        };
        let centre = egui::pos2(rect.left() + 12.0, rect.center().y);
        if row.connected {
            painter.circle_filled(centre, 4.0, dot);
        } else {
            painter.circle_stroke(centre, 4.0, egui::Stroke::new(1.2, theme::FAINT));
        }
        let text_left = rect.left() + 24.0;
        let max_w = (rect.right() - text_left - 36.0).max(30.0);
        let line = |text: &str, font: egui::FontId, color: Color32| {
            let mut job = LayoutJob::simple_singleline(text.to_string(), font, color);
            job.wrap.max_width = max_w;
            job.wrap.max_rows = 1;
            job.wrap.break_anywhere = true;
            painter.layout_job(job)
        };
        let name = line(
            &row.name,
            egui::FontId::new(12.5, theme::semibold()),
            if grey { theme::WEAK } else { theme::TEXT },
        );
        let status = line(
            &row.status,
            egui::FontId::proportional(11.0),
            if grey { theme::FAINT } else { theme::WEAK },
        );
        painter.galley(egui::pos2(text_left, rect.top() + 4.0), name, theme::TEXT);
        painter.galley(
            egui::pos2(text_left, rect.top() + 21.0),
            status,
            theme::WEAK,
        );

        let mut action = None;
        let menu = |ui: &mut egui::Ui, action: &mut Option<ListAction>| {
            let Some(id) = &row.device_id else { return };
            let device = self.known_by_id(id);
            if ui.add_enabled(idle, egui::Button::new("Rename…")).clicked() {
                *action = Some(ListAction::Rename(id.clone()));
                ui.close();
            }
            let folder = device.and_then(|d| d.backup_dir.clone());
            if ui
                .add_enabled(folder.is_some(), egui::Button::new("Open backup folder"))
                .clicked()
            {
                *action = Some(ListAction::OpenFolder(id.clone()));
                ui.close();
            }
            let can_id = row.connected
                && self.current_is(&row.sel)
                && self.can_write_id_file()
                && device.is_some();
            if ui
                .add_enabled(can_id, egui::Button::new("Write ID file…"))
                .on_hover_text(
                    "Put a small hidden file on the stick so it is always recognised, even after it is reformatted. You are asked first.",
                )
                .clicked()
            {
                *action = Some(ListAction::WriteIdFile(id.clone()));
                ui.close();
            }
            ui.separator();
            if ui.add_enabled(idle, egui::Button::new("Forget…")).clicked() {
                *action = Some(ListAction::Forget(id.clone()));
                ui.close();
            }
        };
        resp.context_menu(|ui| menu(ui, &mut action));
        if row.device_id.is_some() {
            // The button's right edge sits one margin (8 px) inside the card, whatever width the
            // button takes: it is laid out from the right.
            let b = egui::Rect::from_min_max(
                egui::pos2(rect.right() - 8.0 - 22.0, rect.center().y - 11.0),
                egui::pos2(rect.right() - 8.0, rect.center().y + 11.0),
            );
            let layout = egui::Layout::right_to_left(egui::Align::Center);
            ui.scope_builder(egui::UiBuilder::new().max_rect(b).layout(layout), |ui| {
                ui.spacing_mut().interact_size.x = 22.0;
                ui.spacing_mut().button_padding.x = 4.0;
                ui.menu_button(RichText::new("…").color(theme::WEAK), |ui| {
                    menu(ui, &mut action)
                })
                .response
                .on_hover_text("Rename, forget, open the backup folder");
            });
        }
        (resp.clicked() && idle, action)
    }

    /// Whether the open device is the one this row stands for.
    fn current_is(&self, sel: &Sel) -> bool {
        matches!(sel, Sel::Connected(id) if self.current.as_deref() == Some(id.as_str()))
    }

    /// The open device is a stick or drive that has no ID file yet.
    fn can_write_id_file(&self) -> bool {
        matches!(&self.device, DeviceState::Open(o)
            if o.is_volume && matches!(o.id_file, IdFile::Missing) && matches!(o.stage, Stage::Ready { .. }))
    }

    fn list_action(&mut self, action: ListAction) {
        match action {
            ListAction::Rename(id) => {
                if let Some(d) = self.known_by_id(&id) {
                    self.dialog = Some(Dialog::Rename {
                        id,
                        text: d.name.clone(),
                    });
                }
            }
            ListAction::Forget(id) => {
                if let Some(d) = self.known_by_id(&id) {
                    self.dialog = Some(Dialog::Forget {
                        name: d.name.clone(),
                        id,
                    });
                }
            }
            ListAction::OpenFolder(id) => {
                if let Some(dir) = self.known_by_id(&id).and_then(|d| d.backup_dir.clone()) {
                    self.open_backup_folder(&dir);
                }
            }
            ListAction::WriteIdFile(id) => {
                if let Some(d) = self.known_by_id(&id) {
                    self.dialog = Some(Dialog::IdFile {
                        name: d.name.clone(),
                        device: self.current.clone().unwrap_or_default(),
                        after_question: false,
                    });
                }
            }
        }
    }

    pub(super) fn open_backup_folder(&mut self, dir: &Path) {
        if !dir.is_dir() {
            self.error = Some(format!("The backup folder {} isn't there.", dir.display()));
            return;
        }
        info!("Opening the backup folder {}", dir.display());
        if let Err(e) = open_folder(dir) {
            self.error = Some(format!("Could not open {}: {e}", dir.display()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pnk_core::registry::SyncSummary;
    use pnk_core::signals::{DeviceKind, Signals};

    fn listed(id: &str, label: &str) -> ListedDevice {
        ListedDevice {
            id: id.into(),
            label: label.into(),
            kind_hint: None,
            transport: pnk_core::platform::Transport::Volume,
        }
    }

    fn known(name: &str, synced: Option<&str>) -> KnownDevice {
        let mut d = KnownDevice::new(
            DeviceKind::Storage,
            name,
            &Signals::default(),
            "2026-10-03T09:00:00Z",
        );
        d.last_sync = synced.map(|w| SyncSummary {
            when: w.into(),
            files: 1,
            bytes: 1,
        });
        d
    }

    #[test]
    fn connected_devices_come_first_then_the_known_ones_that_are_away() {
        let phone = known("My phone", Some("2026-11-17T18:00:00Z"));
        let work = known("Work stick", None);
        let devices = [listed("E:", "SanDisk (E:)"), listed("F:", "My phone (F:)")];
        let mut links = HashMap::new();
        links.insert("E:".to_string(), Link::NewDevice);
        links.insert("F:".to_string(), Link::Recognised(phone.id.clone()));
        let rows = build_rows(&devices, &links, &[work.clone(), phone.clone()]);
        let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["SanDisk (E:)", "My phone", "Work stick"]);
        assert_eq!(rows[0].status, "new device");
        assert_eq!(rows[0].tone, Tone::Attention);
        assert!(rows[0].device_id.is_none(), "no menu until it is known");
        assert_eq!(rows[1].status, "connected");
        assert!(rows[1].connected && rows[1].device_id.as_deref() == Some(phone.id.as_str()));
        assert!(!rows[2].connected);
        assert_eq!(rows[2].status, "not synced yet");
        assert_eq!(rows[2].sel, Sel::Known(work.id));
    }

    #[test]
    fn a_device_whose_backup_folder_is_away_says_so() {
        let mut away = known("Work stick", Some("2026-11-17T18:00:00Z"));
        away.offline = Some("Backup folder not found: E:/Backups".into());
        let rows = build_rows(&[], &HashMap::new(), std::slice::from_ref(&away));
        assert_eq!(rows[0].status, "backup folder not found");
        // Plugged in, it is still recognised, and says why it can't be used.
        let devices = [listed("E:", "Work stick (E:)")];
        let mut links = HashMap::new();
        links.insert("E:".to_string(), Link::Recognised(away.id.clone()));
        let rows = build_rows(&devices, &links, std::slice::from_ref(&away));
        assert_eq!(
            (rows[0].status.as_str(), rows[0].tone),
            ("folder not found", Tone::Attention)
        );
    }

    #[test]
    fn a_device_waiting_its_turn_says_so() {
        let rows = build_rows(&[listed("E:", "Stick (E:)")], &HashMap::new(), &[]);
        assert_eq!(rows[0].status, "waiting");
        let mut links = HashMap::new();
        links.insert("E:".to_string(), Link::Failed("locked".into()));
        let rows = build_rows(&[listed("E:", "Stick (E:)")], &links, &[]);
        assert_eq!(
            (rows[0].status.as_str(), rows[0].tone),
            ("can't be opened", Tone::Bad)
        );
    }
}
