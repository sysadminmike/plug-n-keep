//! The Skip list and Problems tabs.
use super::*;

impl App {
    pub(super) fn skip_list_view(&mut self, ui: &mut egui::Ui) {
        let idle = self.idle();
        let Some(list) = &self.skip_list else {
            ui.label("Scan first: each device and backup folder has its own skip list.");
            return;
        };
        let mut clear = false;
        let mut remove = None;
        ui.horizontal(|ui| {
            ui.label(
                "Files on this list are left out of every sync for this device and backup folder.",
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !list.is_empty()
                    && ui
                        .add_enabled(idle, egui::Button::new("Clear skip list…"))
                        .clicked()
                {
                    clear = true;
                }
            });
        });
        ui.add_space(4.0);
        if list.is_empty() {
            ui.label("The skip list is empty.");
        } else {
            let row_height = ui.text_style_height(&egui::TextStyle::Body) + 8.0;
            TableBuilder::new(ui)
                .id_salt("skip")
                .striped(true)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::exact(56.0))
                .column(Column::remainder().clip(true))
                .column(Column::exact(90.0))
                .header(22.0, |mut h| {
                    for title in ["#", "Path on device", ""] {
                        h.col(|ui| {
                            theme::th(ui, title);
                        });
                    }
                })
                .body(|body| {
                    body.rows(row_height, list.len(), |mut row| {
                        let n = row.index();
                        let path = &list[n];
                        row.col(|ui| {
                            ui.label(RichText::new(format::count(n + 1)).weak());
                        });
                        row.col(|ui| {
                            ui.label(path).on_hover_text(path);
                        });
                        row.col(|ui| {
                            if ui.add_enabled(idle, egui::Button::new("Remove")).clicked() {
                                remove = Some(path.clone());
                            }
                        });
                    });
                });
        }
        if clear {
            info!("Clear skip list… clicked");
            self.confirm = Some(Confirm::ClearSkipList);
        }
        if let (Some(path), Some(scan)) = (remove, &self.scan) {
            info!("Remove clicked on the skip list: {path}");
            let plan_id = scan.plan_id;
            self.send(Command::SkipRemove { plan_id, path });
        }
    }

    pub(super) fn problems_view(&self, ui: &mut egui::Ui) {
        let Some(scan) = &self.scan else {
            ui.label("Scan first: what can't be backed up is listed here.");
            return;
        };
        ui.label("These were found but can't be backed up, or something in the backup folder needs a look:");
        ui.add_space(4.0);
        let row_height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        TableBuilder::new(ui)
            .id_salt("problems")
            .striped(true)
            .resizable(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::initial(380.0).at_least(80.0).clip(true))
            .column(Column::remainder().clip(true))
            .header(22.0, |mut h| {
                for title in ["Where", "Problem"] {
                    h.col(|ui| {
                        theme::th(ui, title);
                    });
                }
            })
            .body(|body| {
                body.rows(row_height, scan.plan.issues.len(), |mut row| {
                    let issue = &scan.plan.issues[row.index()];
                    row.col(|ui| {
                        ui.label(&issue.location).on_hover_text(&issue.location);
                    });
                    row.col(|ui| {
                        ui.label(&issue.problem).on_hover_text(&issue.problem);
                    });
                });
            });
    }
}
