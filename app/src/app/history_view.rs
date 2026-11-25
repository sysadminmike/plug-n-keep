//! The History tab (every sync of this device, newest first) and the sizes line on the device
//! page. Both are plain text built here from what the worker worked out, so they can be tested
//! without a window.

use pnk_core::history::SyncRecord;
use pnk_core::registry::DeletedFiles;

use super::*;
use crate::worker::Sizes;

/// "Backup 12.3 GB (archive 1.1 GB) · 98.2 GB free on the backup disk · Device 21.4 GB used of
/// 64.0 GB". Each part appears only when it is known; empty when nothing is.
pub(super) fn sizes_line(s: &Sizes) -> String {
    let mut parts = Vec::new();
    if let Some(f) = &s.folder {
        let mut p = format!("Backup {}", format::bytes(f.total));
        if f.archive > 0 {
            p += &format!(" (archive {})", format::bytes(f.archive));
        }
        parts.push(p);
    }
    if let Some(d) = &s.backup_disk {
        parts.push(format!("{} free on the backup disk", format::bytes(d.free)));
    }
    match (&s.device_disk, s.device_capacity) {
        (Some(d), _) => parts.push(format!(
            "Device {} used of {}",
            format::bytes(d.used()),
            format::bytes(d.total)
        )),
        (None, Some(total)) => parts.push(format!("Device {}", format::bytes(total))),
        (None, None) => {}
    }
    parts.join(" · ")
}

/// What a sync did to the files deleted from the device, for the table: "12 archived",
/// "3 sent to the Recycle Bin", "5 left", or a dash.
pub(super) fn moved_text(r: &SyncRecord) -> String {
    let mut parts = Vec::new();
    if r.moved > 0 {
        parts.push(match r.mode {
            DeletedFiles::RecycleBin => format!(
                "{} sent to the {}",
                format::count(r.moved as usize),
                pnk_core::platform::BIN_NAME
            ),
            _ => format!("{} archived", format::count(r.moved as usize)),
        });
    }
    if r.left > 0 {
        parts.push(format!("{} left", format::count(r.left as usize)));
    }
    if parts.is_empty() {
        "–".into()
    } else {
        parts.join(", ")
    }
}

/// "1,234 files · 5.6 GB", or a dash when nothing was copied.
pub(super) fn copied_text(r: &SyncRecord) -> String {
    if r.copied == 0 {
        "–".into()
    } else {
        format!(
            "{} · {}",
            format::files(r.copied as usize),
            format::bytes(r.copied_bytes)
        )
    }
}

/// "12 made, 2 not made", or a dash when no JPG copies were made or failed.
pub(super) fn jpgs_text(r: &SyncRecord) -> String {
    let mut parts = Vec::new();
    if r.jpgs_made > 0 {
        parts.push(format!("{} made", format::count(r.jpgs_made as usize)));
    }
    if r.jpgs_failed > 0 {
        parts.push(format!(
            "{} not made",
            format::count(r.jpgs_failed as usize)
        ));
    }
    if parts.is_empty() {
        "–".into()
    } else {
        parts.join(", ")
    }
}

/// How it ended, if not normally.
pub(super) fn ending(r: &SyncRecord) -> Option<&'static str> {
    if r.disk_full {
        Some("stopped: the backup disk was full")
    } else if r.cut_off {
        Some("cut off: a transfer ended badly and the connection was reset")
    } else if r.stopped {
        Some("stopped before it was done")
    } else {
        None
    }
}

/// The whole summary of one sync, for the line under the table.
pub(super) fn full_summary(r: &SyncRecord) -> String {
    let mut s = format!(
        "{} · took {}. ",
        format::local_time(&r.local),
        format::duration(r.secs)
    );
    if r.copied == 0 {
        s += "Nothing was copied";
    } else {
        s += &format!(
            "Copied {} ({})",
            format::files(r.copied as usize),
            format::bytes(r.copied_bytes)
        );
        if r.replaced > 0 {
            s += &format!(
                ", {} of them replacing an older copy",
                format::count(r.replaced as usize)
            );
        }
    }
    s += ". ";
    let mut more = Vec::new();
    if r.already_backed_up > 0 {
        more.push(format!(
            "{} already backed up",
            format::count(r.already_backed_up as usize)
        ));
    }
    if r.skipped > 0 {
        more.push(format!("{} skipped", format::count(r.skipped as usize)));
    }
    if r.failed > 0 {
        more.push(format!("{} failed", format::count(r.failed as usize)));
    }
    if !more.is_empty() {
        s += &format!("{}. ", more.join(", "));
    }
    let deleted = match r.mode {
        DeletedFiles::Archive => "archived",
        DeletedFiles::LeaveInPlace => "left in place",
        DeletedFiles::RecycleBin => "sent to the bin",
    };
    if r.moved > 0 || r.left > 0 {
        s += &format!(
            "Files deleted from the device ({deleted} by this device's setting): {}. ",
            moved_text(r)
        );
    }
    if r.jpgs_made > 0 || r.jpgs_failed > 0 {
        s += &format!("JPG copies of HEIC photos: {}. ", jpgs_text(r));
    }
    if let Some(e) = ending(r) {
        s += &format!("The sync {e}. ");
    }
    if !r.note.is_empty() {
        s += &format!("{}. ", r.note.trim_end_matches('.'));
    }
    s.trim_end().to_string()
}

impl App {
    pub(super) fn history_view(&mut self, ui: &mut egui::Ui, d: &KnownDevice) {
        let empty = Vec::new();
        let records = self.history.get(&d.id).unwrap_or(&empty);
        if records.is_empty() {
            ui.label("No sync has been recorded for this device yet. Each sync adds a line here.");
            return;
        }
        match self.history_row.and_then(|n| records.get(n)) {
            Some(r) => {
                ui.label(full_summary(r));
            }
            None => {
                ui.label(
                    RichText::new("Newest first. Click a line for its full summary.")
                        .color(theme::WEAK),
                );
            }
        }
        ui.add_space(4.0);
        let row_height = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        let mut clicked = None;
        let selected = self.history_row;
        TableBuilder::new(ui)
            .id_salt("history")
            .striped(true)
            .resizable(true)
            .sense(egui::Sense::click())
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::initial(150.0).at_least(100.0).clip(true))
            .column(Column::initial(160.0).at_least(80.0).clip(true))
            .column(Column::initial(200.0).at_least(80.0).clip(true))
            .column(Column::initial(130.0).at_least(60.0).clip(true))
            .column(Column::initial(60.0).at_least(40.0))
            .column(Column::remainder().at_least(70.0).clip(true))
            .header(22.0, |mut h| {
                for title in [
                    "Date",
                    "Copied",
                    "Archived, binned or left",
                    "JPG copies",
                    "Failed",
                    "Took",
                ] {
                    h.col(|ui| {
                        theme::th(ui, title);
                    });
                }
            })
            .body(|body| {
                body.rows(row_height, records.len(), |mut row| {
                    let n = row.index();
                    let r = &records[n];
                    row.set_selected(selected == Some(n));
                    row.col(|ui| {
                        ui.label(format::local_time(&r.local));
                    });
                    row.col(|ui| {
                        ui.label(copied_text(r));
                    });
                    row.col(|ui| {
                        ui.label(moved_text(r));
                    });
                    row.col(|ui| {
                        ui.label(jpgs_text(r));
                    });
                    row.col(|ui| {
                        if r.failed > 0 {
                            ui.label(
                                RichText::new(format::count(r.failed as usize))
                                    .color(theme::DANGER),
                            );
                        } else {
                            ui.label(RichText::new("–").color(theme::WEAK));
                        }
                    });
                    row.col(|ui| {
                        let text = format::duration(r.secs);
                        let text = if r.ended_early() {
                            format!("{text} (stopped)")
                        } else {
                            text
                        };
                        ui.label(text);
                    });
                    if row.response().clicked() {
                        clicked = Some(n);
                    }
                });
            });
        if let Some(n) = clicked {
            self.history_row = if self.history_row == Some(n) {
                None
            } else {
                Some(n)
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pnk_core::platform::DiskSpace;
    use pnk_core::space::FolderSizes;

    const GB: u64 = 1 << 30;

    #[test]
    fn the_sizes_line_names_only_what_is_known() {
        assert_eq!(sizes_line(&Sizes::default()), "");
        let s = Sizes {
            folder: Some(FolderSizes {
                total: 12 * GB,
                archive: GB,
                files: 10,
            }),
            backup_disk: Some(DiskSpace {
                total: 500 * GB,
                free: 98 * GB,
            }),
            device_disk: Some(DiskSpace {
                total: 64 * GB,
                free: 40 * GB,
            }),
            device_capacity: None,
        };
        assert_eq!(
            sizes_line(&s),
            "Backup 12.0 GB (archive 1.0 GB) · 98.0 GB free on the backup disk · Device 24.0 GB used of 64.0 GB"
        );
        // No archive yet, and a phone that only says how big it is.
        let phone = Sizes {
            folder: Some(FolderSizes {
                total: 5 * GB,
                ..FolderSizes::default()
            }),
            device_capacity: Some(128 * GB),
            ..Sizes::default()
        };
        assert_eq!(sizes_line(&phone), "Backup 5.0 GB · Device 128.0 GB");
    }

    fn record() -> SyncRecord {
        SyncRecord {
            local: "2026-11-18 09-30-12".into(),
            secs: 192.0,
            copied: 12,
            copied_bytes: 5 * GB,
            ..SyncRecord::default()
        }
    }

    #[test]
    fn table_cells_say_what_happened() {
        let mut r = record();
        assert_eq!(copied_text(&r), "12 files · 5.0 GB");
        assert_eq!(moved_text(&r), "–");
        r.moved = 3;
        r.left = 2;
        assert_eq!(moved_text(&r), "3 archived, 2 left");
        r.mode = DeletedFiles::RecycleBin;
        assert!(
            moved_text(&r).starts_with("3 sent to the "),
            "{}",
            moved_text(&r)
        );
        r.copied = 0;
        assert_eq!(copied_text(&r), "–");
    }

    #[test]
    fn the_full_summary_tells_the_whole_story() {
        let mut r = record();
        r.replaced = 2;
        r.skipped = 1;
        r.failed = 3;
        r.moved = 4;
        r.disk_full = true;
        r.note = "something else".into();
        let s = full_summary(&r);
        assert!(
            s.starts_with("18 Nov 2026 09:30 · took 3 min 12 s. "),
            "{s}"
        );
        assert!(
            s.contains("Copied 12 files (5.0 GB), 2 of them replacing an older copy"),
            "{s}"
        );
        assert!(s.contains("1 skipped, 3 failed"), "{s}");
        assert!(s.contains("4 archived"), "{s}");
        assert!(s.contains("backup disk was full"), "{s}");
        assert!(s.ends_with("something else."), "{s}");
        let quiet = SyncRecord::default();
        assert!(full_summary(&quiet).contains("Nothing was copied"));
    }
}
