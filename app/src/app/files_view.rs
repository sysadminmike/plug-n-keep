//! The Files tab: the sub-tabs, the file table and the Deleted from device list.
use super::*;

impl App {
    pub(super) fn tabs(&mut self, ui: &mut egui::Ui, c: &Counts) {
        // The list of JPG failures emptied (they were made): back to the usual list.
        if self.tab == Tab::JpgFailed && self.scan.as_ref().is_none_or(|s| s.jpg_failed.is_empty())
        {
            self.tab = Tab::Pending;
            self.filter_dirty = true;
        }
        if self.tab == Tab::JpgMade && self.scan.as_ref().is_none_or(|s| s.jpg_made.is_empty()) {
            self.tab = Tab::Pending;
            self.filter_dirty = true;
        }
        let Some(scan) = &self.scan else { return };
        let mut tabs = vec![
            (Tab::Pending, "Pending", c.pending),
            (Tab::New, "New", c.new),
            (Tab::Mismatch, "Changed", c.mismatch),
        ];
        if c.failed > 0 {
            tabs.push((Tab::Failed, "Failed", c.failed));
        }
        if !scan.jpg_failed.is_empty() {
            tabs.push((Tab::JpgFailed, "Couldn't make a JPG", scan.jpg_failed.len()));
        }
        if !scan.jpg_made.is_empty() {
            tabs.push((Tab::JpgMade, "JPG copies made", scan.jpg_made.len()));
        }
        if !scan.plan.removed.is_empty() {
            tabs.push((Tab::Deleted, "Deleted from device", scan.plan.removed.len()));
        }
        tabs.push((Tab::All, "All", scan.rows.len()));
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 18.0;
            for (tab, name, n) in tabs {
                let count = format::count(n);
                let resp = theme::tab(ui, self.tab == tab, name, Some(&count));
                let label = format!("{name} ({count})");
                resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, &label));
                if resp.clicked() && self.tab != tab {
                    self.tab = tab;
                    self.filter_dirty = true;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let edit = egui::TextEdit::singleline(&mut self.search)
                    .hint_text("name or path")
                    .margin(egui::Margin::symmetric(8, 4))
                    .desired_width(220.0);
                let resp = ui.add(edit);
                // Right-to-left layout: the label is added after the box it names.
                let label = ui.label(RichText::new("Filter").color(theme::WEAK));
                if resp.labelled_by(label.id).changed() {
                    self.filter_dirty = true;
                }
                theme::glyph(ui, theme::I_SEARCH, 13.0, theme::FAINT);
            });
        });
    }

    pub(super) fn file_table(&mut self, ui: &mut egui::Ui) {
        let Some(scan) = &self.scan else {
            ui.add_space(48.0);
            ui.vertical_centered(|ui| {
                theme::glyph(ui, theme::I_PHONE, 40.0, theme::BORDER);
                ui.add_space(6.0);
                ui.label(
                    RichText::new("No scan yet")
                        .size(16.0)
                        .family(theme::semibold())
                        .color(theme::TEXT),
                );
                ui.label(
                    RichText::new("Click Scan to compare the device with its backup folder. Nothing is copied until you click Start sync.")
                        .color(theme::WEAK),
                );
            });
            return;
        };
        if self.filtered.is_empty() {
            ui.add_space(24.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No files here.").color(theme::WEAK));
            });
            return;
        }
        let can_sync = self.idle() && scan.plan.folder_blocked().is_none();
        let mut action = None;
        let row_height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        // Selectable labels would take the right-click before the row sees it; the row menu
        // has "Copy path" instead.
        ui.style_mut().interaction.selectable_labels = false;
        TableBuilder::new(ui)
            .id_salt("files")
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(56.0))
            .column(Column::initial(200.0).at_least(80.0).clip(true))
            .column(Column::initial(300.0).at_least(80.0).clip(true))
            .column(Column::initial(150.0).at_least(70.0).clip(true))
            .column(Column::initial(260.0).at_least(80.0).clip(true))
            .column(Column::remainder().at_least(120.0).clip(true))
            .header(22.0, |mut h| {
                for title in [
                    "#",
                    "Name",
                    "Path on device",
                    "Size",
                    "Backup copy",
                    "Status",
                ] {
                    h.col(|ui| {
                        theme::th(ui, title);
                    });
                }
            })
            .body(|body| {
                body.rows(row_height, self.filtered.len(), |mut row| {
                    let n = row.index();
                    let i = self.filtered[n];
                    let (e, r) = (&scan.plan.entries[i], &scan.rows[i]);
                    row.col(|ui| {
                        ui.label(RichText::new(format::count(n + 1)).weak());
                    });
                    row.col(|ui| {
                        ui.label(e.source.file_name());
                    });
                    row.col(|ui| {
                        ui.label(e.source.as_str()).on_hover_text(e.source.as_str());
                    });
                    row.col(|ui| {
                        let size = e.size.map(format::bytes).unwrap_or_else(|| "?".into());
                        match (&r.status, e.existing_size) {
                            (Status::Mismatch, Some(was)) => {
                                ui.label(format!("{size} (was {})", format::bytes(was)));
                            }
                            _ => {
                                ui.label(size);
                            }
                        }
                    });
                    row.col(|ui| {
                        let text = RichText::new(&r.target);
                        let text = if r.status == Status::Mismatch {
                            text.color(CAUTION)
                        } else {
                            text.color(QUIET)
                        };
                        ui.label(text).on_hover_text(&r.target);
                    });
                    row.col(|ui| {
                        let (label, color) = status_label(&r.status);
                        let resp = theme::pill(ui, label, color);
                        if let Status::Error(msg) = &r.status {
                            ui.label(RichText::new(msg).color(RED)).on_hover_text(msg);
                            resp.on_hover_text(msg);
                        }
                    });
                    row.response().context_menu(|ui| {
                        ui.label(RichText::new(e.source.file_name()).strong());
                        ui.separator();
                        let sync_label =
                            if e.status == Status::Verified && r.status == Status::Verified {
                                "Copy this file again…"
                            } else {
                                "Sync this file"
                            };
                        if ui
                            .add_enabled(can_sync, egui::Button::new(sync_label))
                            .clicked()
                        {
                            action = Some(RowAction::SyncOne(i));
                            ui.close();
                        }
                        let skip_label = match r.status {
                            Status::Skipped => Some("Remove from skip list"),
                            Status::Verified => None,
                            _ => Some("Skip this file"),
                        };
                        if let Some(label) = skip_label
                            && ui.add_enabled(can_sync, egui::Button::new(label)).clicked()
                        {
                            action = Some(RowAction::ToggleSkip(i));
                            ui.close();
                        }
                        ui.separator();
                        if ui.button("Copy path").clicked() {
                            ui.ctx().copy_text(e.source.as_str().to_string());
                            ui.close();
                        }
                    });
                });
            });
        if let Some(a) = action {
            self.row_action(a);
        }
    }
    /// The HEIC photos no JPG copy could be made from, each with its reason, and "Try again".
    pub(super) fn jpg_failed_view(&mut self, ui: &mut egui::Ui, device_id: &str) {
        // "Try again" makes JPGs, so it needs what making them needs: the device's setting on
        // and a decoder here (like the backfill button in the settings).
        let setting_on = self
            .known_by_id(device_id)
            .is_some_and(|d| d.settings.jpeg_copies);
        let decoder = self.heic_ok();
        let Some(scan) = &self.scan else { return };
        let list = scan.jpg_failed.clone();
        let q = self.search.trim().to_lowercase();
        let shown: Vec<&FailedEntry> = list
            .iter()
            .filter(|f| q.is_empty() || f.heic.to_lowercase().contains(&q))
            .collect();
        let can =
            self.idle() && scan.plan.folder_blocked().is_none() && setting_on && decoder.is_ok();
        let mut again: Option<Option<Vec<String>>> = None;
        ui.horizontal_wrapped(|ui| {
            ui.label(
                "These HEIC photos have no JPG copy. They are not tried again until the photo changes, or you try again here.",
            );
            if !setting_on {
                ui.label(
                    RichText::new("Turn on \"Make a JPG copy of each HEIC photo\" in the settings to try again.")
                        .color(theme::WEAK),
                );
            } else if let Err(hint) = &decoder {
                ui.label(RichText::new(hint).color(theme::WEAK));
            }
            if ui
                .add_enabled(can && !list.is_empty(), egui::Button::new("Try again for all"))
                .on_hover_text("Forget these failures and make the JPG copies now, from the backup folder")
                .clicked()
            {
                again = Some(None);
            }
        });
        ui.add_space(4.0);
        let row_height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        ui.style_mut().interaction.selectable_labels = false;
        TableBuilder::new(ui)
            .id_salt("jpg-failed")
            .striped(true)
            .resizable(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(44.0))
            .column(Column::initial(340.0).at_least(80.0).clip(true))
            .column(Column::exact(80.0))
            .column(Column::remainder().at_least(160.0).clip(true))
            .header(22.0, |mut h| {
                for title in ["#", "HEIC photo", "", "Why there is no JPG"] {
                    h.col(|ui| {
                        theme::th(ui, title);
                    });
                }
            })
            .body(|body| {
                body.rows(row_height, shown.len(), |mut row| {
                    let n = row.index();
                    let f = shown[n];
                    let path = windows_path(&f.heic);
                    row.col(|ui| {
                        ui.label(RichText::new(format::count(n + 1)).weak());
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(path.as_str()).truncate())
                            .on_hover_text(path.as_str());
                    });
                    row.col(|ui| {
                        if ui
                            .add_enabled(can, egui::Button::new("Try again"))
                            .on_hover_text("Make this photo's JPG copy again")
                            .clicked()
                        {
                            again = Some(Some(vec![f.heic.clone()]));
                        }
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(RichText::new(&f.reason).color(RED)).truncate())
                            .on_hover_text(&f.reason);
                    });
                });
            });
        if let Some(heics) = again {
            self.retry_jpgs(device_id, heics);
        }
    }

    /// The JPG copies the app made, each with the HEIC photo it came from.
    pub(super) fn jpg_made_view(&mut self, ui: &mut egui::Ui) {
        let Some(scan) = &self.scan else { return };
        let q = self.search.trim().to_lowercase();
        let shown: Vec<&(String, String)> = scan
            .jpg_made
            .iter()
            .filter(|(j, h)| {
                q.is_empty() || j.to_lowercase().contains(&q) || h.to_lowercase().contains(&q)
            })
            .collect();
        ui.label("JPG copies made by Plug-n-Keep, in the backup folder, with the HEIC photo each came from.");
        ui.add_space(4.0);
        let row_height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        ui.style_mut().interaction.selectable_labels = false;
        TableBuilder::new(ui)
            .id_salt("jpg-made")
            .striped(true)
            .resizable(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(56.0))
            .column(Column::initial(380.0).at_least(80.0).clip(true))
            .column(Column::remainder().at_least(160.0).clip(true))
            .header(22.0, |mut h| {
                for title in ["#", "JPG copy", "Made from"] {
                    h.col(|ui| {
                        theme::th(ui, title);
                    });
                }
            })
            .body(|body| {
                body.rows(row_height, shown.len(), |mut row| {
                    let n = row.index();
                    let (jpg, heic) = shown[n];
                    row.col(|ui| {
                        ui.label(RichText::new(format::count(n + 1)).weak());
                    });
                    for path in [windows_path(jpg), windows_path(heic)] {
                        row.col(|ui| {
                            ui.add(egui::Label::new(path.as_str()).truncate())
                                .on_hover_text(path.as_str());
                        });
                    }
                });
            });
    }

    pub(super) fn deleted_view(&mut self, ui: &mut egui::Ui) {
        let Some(scan) = &self.scan else { return };
        let blocked = scan.plan.folder_blocked();
        // The dated folder this scan's files went into, once archived.
        let moved_into = scan.gone.iter().find_map(|g| match g {
            Gone::Archived(to) => to.rsplit_once('/').map(|(dir, _)| windows_path(dir)),
            _ => None,
        });
        let waiting_left = scan.gone.iter().any(|g| matches!(g, Gone::Waiting));
        let mode = scan.plan.deleted_files;
        let bin = pnk_core::platform::BIN_NAME;
        ui.horizontal_wrapped(|ui| match (&blocked, self.archive_on(), &moved_into) {
            (Some(why), _, _) => {
                let verb = match mode {
                    DeletedFiles::RecycleBin => format!("sent to the {bin}"),
                    DeletedFiles::LeaveInPlace => "moved".to_string(),
                    DeletedFiles::Archive => "archived".to_string(),
                };
                ui.colored_label(CAUTION, format!("These won't be {verb}: {why}."));
            }
            (None, true, _) if mode == DeletedFiles::RecycleBin => {
                if waiting_left {
                    ui.label(format!(
                        "These are in the backup folder but no longer on the device. Start sync sends them to the {bin}, where they can be restored. Nothing is deleted for good."
                    ));
                } else if scan.gone.iter().any(|g| matches!(g, Gone::Binned)) {
                    ui.label(format!(
                        "Sent to the {bin}. Scan again to refresh this list."
                    ));
                } else {
                    ui.label(format!(
                        "Nothing was sent to the {bin}. Scan again to refresh this list."
                    ));
                }
            }
            (None, true, Some(dir)) if !waiting_left => {
                ui.label(RichText::new("Moved into").color(theme::WEAK));
                ui.label(
                    RichText::new(dir)
                        .family(theme::semibold())
                        .color(theme::TEXT),
                );
                ui.label(
                    RichText::new("in the backup folder. Scan again to refresh this list.")
                        .color(theme::WEAK),
                );
            }
            (None, true, _) => {
                ui.label(format!(
                    "These are in the backup folder but no longer on the device. Start sync moves them into \"{}\\<date and time>\\\". Nothing is deleted.",
                    scan.plan.archive_name
                ));
            }
            (None, false, _) => {
                ui.label("These are in the backup folder but no longer on the device. They are set to be left in place, so they stay where they are.");
            }
        });
        ui.add_space(4.0);
        let waiting = if blocked.is_none() && self.archive_on() {
            match mode {
                DeletedFiles::RecycleBin => ("Will go to the bin", NORMAL),
                _ => ("Will be archived", NORMAL),
            }
        } else {
            ("Stays", GREY)
        };
        let row_height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        ui.style_mut().interaction.selectable_labels = false;
        TableBuilder::new(ui)
            .id_salt("deleted")
            .striped(true)
            .resizable(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::exact(44.0))
            .column(Column::initial(340.0).at_least(80.0).clip(true))
            .column(Column::initial(90.0).at_least(60.0))
            .column(Column::initial(120.0).at_least(80.0))
            .column(Column::remainder().at_least(160.0).clip(true))
            .header(22.0, |mut h| {
                for title in [
                    "#",
                    "Was in the backup folder",
                    "Size",
                    "Status",
                    "Moved to",
                ] {
                    h.col(|ui| {
                        theme::th(ui, title);
                    });
                }
            })
            .body(|body| {
                body.rows(row_height, scan.plan.removed.len(), |mut row| {
                    let n = row.index();
                    let r = &scan.plan.removed[n];
                    let was = windows_path(r.path.as_str());
                    row.col(|ui| {
                        ui.label(RichText::new(format::count(n + 1)).weak());
                    });
                    row.col(|ui| {
                        ui.add(egui::Label::new(was.as_str()).truncate())
                            .on_hover_text(was.as_str());
                    });
                    row.col(|ui| {
                        ui.label(RichText::new(format::bytes(r.size)).color(QUIET));
                    });
                    row.col(|ui| match &scan.gone[n] {
                        Gone::Waiting => {
                            theme::pill(ui, waiting.0, waiting.1);
                        }
                        Gone::Archived(_) => {
                            theme::pill(ui, "Archived", GOOD);
                        }
                        Gone::Binned => {
                            theme::pill(ui, "In the bin", GOOD);
                        }
                        Gone::Failed(e) => {
                            theme::pill(ui, "Not moved", RED).on_hover_text(e);
                        }
                    });
                    row.col(|ui| match &scan.gone[n] {
                        Gone::Archived(to) => {
                            let to = windows_path(to);
                            ui.add(egui::Label::new(to.as_str()).truncate())
                                .on_hover_text(to.as_str());
                        }
                        Gone::Failed(e) => {
                            ui.add(egui::Label::new(RichText::new(e).color(RED)).truncate())
                                .on_hover_text(e);
                        }
                        Gone::Binned => {
                            ui.label(RichText::new(bin).color(GREY));
                        }
                        Gone::Waiting => {
                            ui.label(RichText::new("–").color(GREY));
                        }
                    });
                });
            });
    }

    /// The backup folder belongs to another device: say so, and offer to hand it over, unless
    /// its marker records another view of the device: that is refused, and never overridden.
    pub(super) fn folder_warning(&mut self, ui: &mut egui::Ui) {
        let Some(offer) = self
            .scan
            .as_ref()
            .and_then(|s| device_page::claim_offer(&s.plan))
        else {
            return;
        };
        match offer {
            device_page::ClaimOffer::Offer(why) => {
                let text = format!(
                    "Nothing will be copied into this backup folder or archived in it: {why}. Choose this device's own backup folder, or, if this device replaces that device, hand the folder over to it."
                );
                if theme::warning(ui, &text, "Use this folder for this device…", self.idle()) {
                    self.confirm = Some(Confirm::ClaimFolder);
                }
            }
            device_page::ClaimOffer::Refused(why) => {
                let text = format!(
                    "Nothing will be copied into this backup folder or archived in it, and it can't be handed to this device: {why}."
                );
                let _ = theme::banner(ui, theme::Note::Warning, &text);
            }
        }
    }
}
