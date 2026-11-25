//! The progress card and the totals line.
use super::*;

impl App {
    pub(super) fn progress_card(&self, ui: &mut egui::Ui) {
        match &self.busy {
            Busy::Idle => {}
            Busy::Scanning(p) => {
                let (title, detail) = match p {
                    None => ("Starting the scan…".to_string(), String::new()),
                    Some(ScanProgress::Device { files, folder }) => (
                        "Reading the device".into(),
                        format!("{} found  ·  {folder}", format::files(*files)),
                    ),
                    Some(ScanProgress::Backup { files }) => {
                        ("Indexing the backup folder".into(), format::files(*files))
                    }
                    Some(ScanProgress::Comparing { files }) => {
                        ("Comparing".into(), format::files(*files))
                    }
                };
                theme::card(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(
                            RichText::new(title)
                                .family(theme::semibold())
                                .color(theme::TEXT),
                        );
                        ui.add(
                            egui::Label::new(RichText::new(detail).color(theme::WEAK)).truncate(),
                        );
                    });
                });
            }
            Busy::Jpgs(j) => self.jpg_card(ui, j),
            Busy::Syncing(SyncView { jpg: Some(j), .. }) => self.jpg_card(ui, j),
            Busy::Syncing(v) => {
                let entry = v
                    .entry
                    .and_then(|i| self.scan.as_ref()?.plan.entries.get(i));
                let file_frac = match v.file_size {
                    Some(0) => 1.0,
                    Some(t) => (v.bytes as f64 / t as f64).clamp(0.0, 1.0),
                    None => 0.0,
                };
                // By bytes when the sizes are known: videos take most of the time.
                let done_bytes = v.done_bytes();
                let batch_frac = if v.total_bytes > 0 {
                    (done_bytes as f64 / v.total_bytes as f64).clamp(0.0, 1.0)
                } else if v.total > 0 {
                    ((v.ordinal.saturating_sub(1) as f64 + file_frac) / v.total as f64)
                        .clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let rate = v.rate();
                theme::card(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        match entry {
                            Some(e) => {
                                ui.label(
                                    RichText::new(e.source.file_name())
                                        .family(theme::semibold())
                                        .color(theme::TEXT),
                                );
                                if let Some(size) = v.file_size {
                                    ui.label(RichText::new(format::bytes(size)).color(theme::WEAK));
                                }
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(e.source.as_str()).color(theme::FAINT),
                                    )
                                    .truncate(),
                                );
                            }
                            None => {
                                ui.label(RichText::new("Starting…").color(theme::WEAK));
                            }
                        }
                        // Right-aligned, next to the percentage, so it stays put while the
                        // numbers on the line below change.
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                RichText::new(format!("{:.0}%", batch_frac * 100.0))
                                    .size(17.0)
                                    .family(theme::semibold())
                                    .color(theme::TEXT),
                            );
                            if !v.eta.is_empty() {
                                ui.add_space(10.0);
                                ui.label(RichText::new(&v.eta).color(theme::WEAK));
                            }
                        });
                    });
                    ui.add_space(4.0);
                    ui.add(
                        egui::ProgressBar::new(batch_frac as f32)
                            .desired_height(6.0)
                            .corner_radius(3)
                            .fill(theme::ACCENT),
                    );
                    ui.add_space(2.0);
                    ui.add(
                        egui::ProgressBar::new(file_frac as f32)
                            .desired_height(3.0)
                            .corner_radius(2)
                            .fill(theme::FAINT),
                    );
                    ui.add_space(4.0);
                    // Fixed-width columns, so nothing shifts sideways as the numbers change.
                    // The current file's own progress is the thin bar above.
                    ui.horizontal(|ui| {
                        progress_cell(
                            ui,
                            150.0,
                            format!(
                                "File {} of {}",
                                format::count(v.ordinal),
                                format::count(v.total)
                            ),
                        );
                        progress_cell(
                            ui,
                            230.0,
                            format!(
                                "{} of {} copied",
                                format::bytes(done_bytes),
                                format::bytes(v.total_bytes)
                            ),
                        );
                        progress_cell(ui, 110.0, format::speed(rate));
                    });
                });
            }
        }
    }

    /// "Making JPG copies 12 of 340", with the photo being converted.
    fn jpg_card(&self, ui: &mut egui::Ui, j: &JpgView) {
        let frac = if j.total > 0 {
            (j.done as f32 / j.total as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        theme::card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spinner();
                let text = if j.total == 0 {
                    "Making JPG copies…".to_string()
                } else {
                    format!(
                        "Making JPG copies {} of {}",
                        format::count((j.done + 1).min(j.total)),
                        format::count(j.total)
                    )
                };
                ui.label(
                    RichText::new(text)
                        .family(theme::semibold())
                        .color(theme::TEXT),
                );
                ui.add(egui::Label::new(RichText::new(&j.name).color(theme::FAINT)).truncate());
            });
            ui.add_space(4.0);
            ui.add(
                egui::ProgressBar::new(frac)
                    .desired_height(6.0)
                    .corner_radius(3)
                    .fill(theme::ACCENT),
            );
        });
    }

    /// One line with the totals the tabs don't show: numbers in semibold, captions in grey.
    pub(super) fn summary(&self, ui: &mut egui::Ui, c: &Counts) {
        let Some(scan) = &self.scan else { return };
        let in_backup = (scan.backup_files + scan.copied).saturating_sub(scan.archived);
        let parts = [
            (scan.rows.len(), "on the device", Some(c.device_bytes)),
            (in_backup, "in the backup folder", None),
            (c.verified, "already backed up", Some(c.verified_bytes)),
            (c.pending, "to copy", Some(c.pending_bytes)),
        ];
        let strong =
            egui::TextFormat::simple(egui::FontId::new(12.5, theme::semibold()), theme::TEXT);
        let weak = egui::TextFormat::simple(egui::FontId::proportional(12.5), theme::WEAK);
        let mut job = egui::text::LayoutJob::default();
        for (i, (n, caption, size)) in parts.into_iter().enumerate() {
            let gap = if i == 0 { 0.0 } else { 14.0 };
            job.append(&format::count(n), gap, strong.clone());
            job.append(caption, 5.0, weak.clone());
            if let Some(size) = size {
                job.append(&format!("({})", format::bytes(size)), 4.0, weak.clone());
            }
        }
        ui.label(job);
    }
}
