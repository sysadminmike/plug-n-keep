//! The Log panel: the app's log (logging.rs), newest at the top, with a level filter, a text
//! filter, Copy and Clear. Only the rows in view are drawn, from a cached list of the lines that
//! pass the filters; it is brought up to date each frame by looking at new lines only.

use eframe::egui::{self, Color32, FontId, RichText, TextFormat, text::LayoutJob};
use log::Level;
use pnk_core::state::LogLevel;

use crate::format;
use crate::logging::{Entry, Filtered, Logger, REPAINT_EVERY, Ring, Show, Update};
use crate::theme;

/// What the window has to do after the panel was drawn.
pub enum Action {
    /// The Record choice changed: apply and save it.
    SetLevel(LogLevel),
    Cleared,
}

pub struct LogView {
    pub open: bool,
    show: Show,
    text: String,
    filtered: Filtered,
    /// The scroll position last frame. While the view is scrolled down, it moves down with the
    /// lines added at the top, so the lines being read stay still.
    scroll_y: f32,
    /// `Ring::next_seq` when last drawn: new lines since then mean another repaint soon.
    seen: u64,
}

impl LogView {
    pub fn new() -> Self {
        Self {
            open: false,
            show: Show::Everything,
            text: String::new(),
            filtered: Filtered::new(Show::Everything),
            scroll_y: 0.0,
            seen: 0,
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, logger: &Logger, record: LogLevel) -> Option<Action> {
        let mut action = None;
        let mut copy = false;
        let mut clear = false;
        let mut update = Update::default();
        let mut total = 0;
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("Log")
                    .family(theme::semibold())
                    .color(theme::TEXT),
            );
            ui.add_space(10.0);
            let label = ui.label(RichText::new("Record").color(theme::WEAK));
            ui.push_id("log-record", |ui| {
                egui::ComboBox::from_id_salt("log-record")
                    .selected_text(level_name(record))
                    .width(84.0)
                    .show_ui(ui, |ui| {
                        for l in LogLevel::ALL {
                            if ui.selectable_label(l == record, level_name(l)).clicked() {
                                action = Some(Action::SetLevel(l));
                            }
                        }
                    })
                    .response
                    .labelled_by(label.id)
                    .on_hover_text(
                        "What the app records from now on: this level and the more serious ones. Saved with the settings.",
                    );
            });
            ui.add_space(8.0);
            let label = ui.label(RichText::new("Show").color(theme::WEAK));
            ui.push_id("log-show", |ui| {
                egui::ComboBox::from_id_salt("log-show")
                    .selected_text(self.show.label())
                    .width(120.0)
                    .show_ui(ui, |ui| {
                        for s in Show::ALL {
                            ui.selectable_value(&mut self.show, s, s.label());
                        }
                    })
                    .response
                    .labelled_by(label.id);
            });
            ui.add_space(8.0);
            theme::glyph(ui, theme::I_SEARCH, 13.0, theme::FAINT);
            ui.label(RichText::new("Filter").color(theme::WEAK));
            let edit = egui::TextEdit::singleline(&mut self.text)
                .hint_text("text in the line")
                .margin(egui::Margin::symmetric(8, 3))
                .desired_width(200.0);
            // The file table has a "Filter" box too: this one gets its own name.
            ui.add(edit).accessible_name("Log filter");

            // Up to date before the counts are drawn. The lock is held only for this, and not
            // across any egui call.
            self.filtered.set(self.show, &self.text);
            let next = {
                let ring = logger.lines();
                update = self.filtered.update(&ring);
                total = ring.len();
                ring.next_seq()
            };
            if next != self.seen {
                self.seen = next;
                // More may be on the way: look again soon, whatever wakes the window.
                ui.ctx().request_repaint_after(REPAINT_EVERY);
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add_enabled(total > 0, egui::Button::new("Clear"))
                    .on_hover_text("Empty the log")
                    .clicked()
                {
                    clear = true;
                }
                if ui
                    .add_enabled(!self.filtered.is_empty(), egui::Button::new("Copy"))
                    .on_hover_text("Copy the lines shown, as text, newest first")
                    .clicked()
                {
                    copy = true;
                }
                ui.add_space(6.0);
                let count = if self.filtered.len() == total {
                    lines(total)
                } else {
                    format!(
                        "{} of {}",
                        format::count(self.filtered.len()),
                        lines(total)
                    )
                };
                ui.label(RichText::new(count).color(theme::WEAK));
            });
        });
        if copy {
            let text = copy_text(&logger.lines(), &self.filtered);
            ui.ctx().copy_text(text);
        }
        if clear {
            let mut ring = logger.lines();
            ring.clear();
            self.filtered.update(&ring);
            action = Some(Action::Cleared);
        }
        ui.add_space(2.0);
        self.rows(ui, logger, update);
        action
    }

    fn rows(&mut self, ui: &mut egui::Ui, logger: &Logger, update: Update) {
        let n = self.filtered.len();
        if n == 0 {
            let empty = logger.lines().is_empty();
            ui.add_space(6.0);
            ui.label(
                RichText::new(if empty {
                    "Nothing logged yet."
                } else {
                    "No lines match the filter."
                })
                .color(theme::FAINT),
            );
            self.scroll_y = 0.0;
            return;
        }
        ui.spacing_mut().item_spacing.y = 1.0;
        let row_height = ui.text_style_height(&egui::TextStyle::Body) + 1.0;
        let step = row_height + ui.spacing().item_spacing.y;
        let mut area = egui::ScrollArea::vertical()
            .id_salt("log-rows")
            .auto_shrink([false, false]);
        if update.rebuilt {
            area = area.vertical_scroll_offset(0.0);
        } else if update.added > 0 && self.scroll_y > 0.5 {
            area = area.vertical_scroll_offset(self.scroll_y + update.added as f32 * step);
        }
        let filtered = &self.filtered;
        let out = area.show_rows(ui, row_height, n, |ui, range| {
            // Copied out, so the lock isn't held while egui draws.
            let rows: Vec<Entry> = {
                let ring = logger.lines();
                range
                    .filter_map(|row| filtered.newest(row))
                    .filter_map(|seq| ring.get(seq).cloned())
                    .collect()
            };
            for e in &rows {
                ui.add(egui::Label::new(row_text(e)).truncate());
            }
        });
        self.scroll_y = out.state.clamped_offset().y;
    }
}

/// The Record choices, with the names shown in the Log panel.
pub(crate) fn level_name(l: LogLevel) -> &'static str {
    match l {
        LogLevel::Error => "Errors",
        LogLevel::Warn => "Warnings",
        LogLevel::Info => "Info",
        LogLevel::Debug => "Debug",
    }
}

fn level_color(level: Level) -> Color32 {
    match level {
        Level::Error => theme::DANGER,
        Level::Warn => theme::CAUTION,
        Level::Info => theme::TEXT,
        Level::Debug | Level::Trace => theme::FAINT,
    }
}

/// One compact line: time, level and source in small fixed-width type, then the message. The
/// gaps are real spaces, so a screen reader reads the parts as separate words.
fn row_text(e: &Entry) -> LayoutJob {
    let mono = FontId::monospace(11.0);
    let mut job = LayoutJob::default();
    job.append(
        &format!("{}  ", e.time()),
        0.0,
        TextFormat::simple(mono.clone(), theme::FAINT),
    );
    job.append(
        &format!("{:<5}  ", e.level),
        0.0,
        TextFormat::simple(mono.clone(), level_color(e.level)),
    );
    job.append(
        &format!("{:<7} ", e.source),
        0.0,
        TextFormat::simple(mono, theme::WEAK),
    );
    let message = match e.level {
        Level::Error => theme::DANGER,
        Level::Debug | Level::Trace => theme::WEAK,
        _ => theme::TEXT,
    };
    job.append(
        &e.message,
        4.0,
        TextFormat::simple(FontId::proportional(12.0), message),
    );
    job
}

/// "1 line", "1,234 lines".
fn lines(n: usize) -> String {
    if n == 1 {
        "1 line".into()
    } else {
        format!("{} lines", format::count(n))
    }
}

/// The lines shown, newest first, one per line.
fn copy_text(ring: &Ring, filtered: &Filtered) -> String {
    let mut out = String::new();
    for e in filtered.newest_first().filter_map(|seq| ring.get(seq)) {
        out.push_str(&e.line());
        out.push_str("\r\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    #[test]
    fn a_row_reads_as_words_and_counts_are_plain_english() {
        let e = Entry {
            seq: 0,
            time_ms: ((13 * 60 + 5) * 60 + 9) * 1000 + 42,
            level: Level::Warn,
            source: Cow::Borrowed("sync"),
            message: "Skipped a.jpg".into(),
        };
        assert_eq!(
            row_text(&e).text,
            "13:05:09.042  WARN   sync    Skipped a.jpg"
        );
        assert_eq!(lines(0), "0 lines");
        assert_eq!(lines(1), "1 line");
        assert_eq!(lines(100_000), "100,000 lines");
    }

    #[test]
    fn copy_gives_the_lines_shown_newest_first() {
        let mut ring = Ring::new(10);
        ring.push(
            1000,
            Level::Info,
            Cow::Borrowed("sync"),
            "Copied a.jpg".into(),
        );
        ring.push(
            2000,
            Level::Debug,
            Cow::Borrowed("plan"),
            "New: b.jpg".into(),
        );
        ring.push(
            3000,
            Level::Error,
            Cow::Borrowed("sync"),
            "Failed: c.jpg".into(),
        );
        let mut f = Filtered::new(Show::Info);
        f.update(&ring);
        assert_eq!(
            copy_text(&ring, &f),
            "00:00:03.000  ERROR  sync     Failed: c.jpg\r\n\
             00:00:01.000  INFO   sync     Copied a.jpg\r\n"
        );
    }
}
