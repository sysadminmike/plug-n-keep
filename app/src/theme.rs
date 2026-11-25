//! The look: light and uncluttered, with a calm blue accent, coloured status labels and tinted
//! messages, in compact text. Windows' own Segoe UI for text and Segoe MDL2 Assets for icons, both
//! read from the Windows fonts folder at start-up (nothing is bundled). Without them, egui's
//! built-in font is used and icons are left out.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, RichText,
    Stroke, TextStyle, Vec2,
};

pub const TEXT: Color32 = Color32::from_rgb(0x1F, 0x23, 0x28);
pub const WEAK: Color32 = Color32::from_rgb(0x5F, 0x67, 0x70);
pub const FAINT: Color32 = Color32::from_rgb(0x9A, 0xA1, 0xA9);
pub const BORDER: Color32 = Color32::from_rgb(0xE1, 0xE5, 0xEA);
/// Light grey-blue: the top area and quiet surfaces.
pub const SURFACE: Color32 = Color32::from_rgb(0xF5, 0xF7, 0xFA);
/// The accent: a calm blue, for the main button, the selected tab, progress and "New".
pub const ACCENT: Color32 = Color32::from_rgb(0x2F, 0x6F, 0xD0);
pub const GOOD: Color32 = Color32::from_rgb(0x2E, 0x7D, 0x32);
pub const CAUTION: Color32 = Color32::from_rgb(0xB2, 0x6B, 0x00);
pub const DANGER: Color32 = Color32::from_rgb(0xC6, 0x28, 0x28);

// Segoe MDL2 Assets code points.
pub const I_REFRESH: &str = "\u{E72C}";
pub const I_SYNC: &str = "\u{E895}";
pub const I_STOP: &str = "\u{E71A}";
pub const I_SKIP: &str = "\u{E893}";
pub const I_PHONE: &str = "\u{E8EA}";
pub const I_INFO: &str = "\u{E946}";
pub const I_ERROR: &str = "\u{EA39}";
pub const I_WARNING: &str = "\u{E7BA}";
pub const I_CLOSE: &str = "\u{E711}";
pub const I_SEARCH: &str = "\u{E721}";
pub const I_LOG: &str = "\u{E8FD}";
pub const I_SETTINGS: &str = "\u{E713}";

static ICONS: AtomicBool = AtomicBool::new(false);

/// The icon, or nothing if the icon font couldn't be loaded.
pub fn icon(i: &'static str) -> &'static str {
    if ICONS.load(Ordering::Relaxed) { i } else { "" }
}

/// `icon` followed by `text`, or just `text` without the icon font.
pub fn with_icon(i: &'static str, text: &str) -> String {
    if ICONS.load(Ordering::Relaxed) {
        format!("{i}  {text}")
    } else {
        text.to_string()
    }
}

pub fn semibold() -> FontFamily {
    FontFamily::Name("semibold".into())
}

pub fn install(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Light);
    ctx.set_fonts(fonts());
    ctx.style_mut_of(egui::Theme::Light, style);
}

fn fonts() -> FontDefinitions {
    let mut fonts = FontDefinitions::default();
    let dir = std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("Fonts");
    let mut load = |name: &str, file: &str| -> bool {
        match std::fs::read(dir.join(file)) {
            Ok(bytes) => {
                fonts
                    .font_data
                    .insert(name.into(), Arc::new(FontData::from_owned(bytes)));
                true
            }
            Err(_) => false,
        }
    };
    let regular = load("segoe", "segoeui.ttf");
    let bold = load("segoe-semibold", "seguisb.ttf");
    let icons = load("icons", "segmdl2.ttf");

    let proportional = fonts.families.entry(FontFamily::Proportional).or_default();
    if regular {
        proportional.insert(0, "segoe".into());
    }
    if icons {
        // Icon code points are in the Private Use Area, so they never clash with text.
        proportional.push("icons".into());
    }
    let mut semi = proportional.clone();
    if bold {
        semi.insert(0, "segoe-semibold".into());
    }
    fonts.families.insert(semibold(), semi);
    ICONS.store(icons, Ordering::Relaxed);
    fonts
}

fn style(style: &mut egui::Style) {
    use TextStyle::*;
    style.text_styles = [
        (Small, FontId::proportional(11.0)),
        (Body, FontId::proportional(12.5)),
        (Button, FontId::proportional(12.5)),
        (Heading, FontId::new(17.0, semibold())),
        (Monospace, FontId::monospace(12.0)),
    ]
    .into();

    let s = &mut style.spacing;
    s.item_spacing = Vec2::new(7.0, 4.0);
    s.button_padding = Vec2::new(10.0, 3.0);
    s.interact_size.y = 24.0;
    s.window_margin = Margin::same(16);
    s.menu_margin = Margin::same(6);

    let v = &mut style.visuals;
    v.override_text_color = None;
    v.panel_fill = Color32::WHITE;
    v.window_fill = Color32::WHITE;
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.window_corner_radius = CornerRadius::same(10);
    v.extreme_bg_color = Color32::WHITE; // text boxes
    v.faint_bg_color = Color32::from_rgb(0xF8, 0xF9, 0xFB); // table stripes
    v.selection.bg_fill = ACCENT.gamma_multiply(0.18);
    v.selection.stroke = Stroke::new(1.0, ACCENT);
    v.hyperlink_color = ACCENT;
    v.warn_fg_color = CAUTION;
    v.error_fg_color = DANGER;

    let radius = CornerRadius::same(6);
    let w = &mut v.widgets;
    w.noninteractive.bg_stroke = Stroke::new(1.0, BORDER); // separators, table lines
    w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    w.noninteractive.corner_radius = radius;
    w.inactive.weak_bg_fill = Color32::WHITE; // buttons
    w.inactive.bg_fill = Color32::from_rgb(0xEC, 0xEF, 0xF3); // checkboxes, progress tracks
    w.inactive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0xD3, 0xD8, 0xDF));
    w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    w.inactive.corner_radius = radius;
    w.hovered.weak_bg_fill = Color32::from_rgb(0xEE, 0xF3, 0xFB);
    w.hovered.bg_fill = Color32::from_rgb(0xEE, 0xF3, 0xFB);
    w.hovered.bg_stroke = Stroke::new(1.0, ACCENT.gamma_multiply(0.6));
    w.hovered.fg_stroke = Stroke::new(1.0, TEXT);
    w.hovered.corner_radius = radius;
    w.hovered.expansion = 0.0;
    w.active.weak_bg_fill = Color32::from_rgb(0xDF, 0xE9, 0xF8);
    w.active.bg_fill = Color32::from_rgb(0xDF, 0xE9, 0xF8);
    w.active.bg_stroke = Stroke::new(1.0, ACCENT);
    w.active.fg_stroke = Stroke::new(1.0, TEXT);
    w.active.corner_radius = radius;
    w.active.expansion = 0.0;
    w.open = w.active;
}

/// The main action: filled with the accent colour.
pub fn primary(text: impl Into<String>) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.into()).color(Color32::WHITE)).fill(ACCENT)
}

/// A white panel with a hairline border.
pub fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(Color32::WHITE)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(8)
        .inner_margin(10)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
    ui.add_space(8.0);
}

#[derive(Clone, Copy)]
pub enum Note {
    Info,
    Warning,
    Error,
}

fn glyph_of(kind: Note) -> &'static str {
    match kind {
        Note::Info => I_INFO,
        Note::Warning => I_WARNING,
        Note::Error => I_ERROR,
    }
}

/// The note's colour, and its light tint for the background.
fn tint(kind: Note) -> (Color32, Color32, Color32) {
    match kind {
        Note::Info => (
            ACCENT,
            Color32::from_rgb(0xEE, 0xF4, 0xFC),
            Color32::from_rgb(0xD0, 0xE0, 0xF6),
        ),
        Note::Warning => (
            CAUTION,
            Color32::from_rgb(0xFF, 0xF7, 0xE8),
            Color32::from_rgb(0xF1, 0xDC, 0xB0),
        ),
        Note::Error => (
            DANGER,
            Color32::from_rgb(0xFD, 0xEE, 0xEE),
            Color32::from_rgb(0xF4, 0xC7, 0xC7),
        ),
    }
}

/// A coloured status label on a light tint, e.g. "New" or "Backed up".
pub fn pill(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.10))
        .corner_radius(9)
        .inner_margin(Margin::symmetric(7, 1))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(11.5).color(color));
        })
        .response
}

/// A tinted message strip with a close button. Returns true when closed.
pub fn banner(ui: &mut egui::Ui, kind: Note, text: &str) -> bool {
    let (color, fill, edge) = tint(kind);
    let mut closed = false;
    egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, edge))
        .corner_radius(8)
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // Top-aligned, so the icon, the first line of text and the close button line up.
            ui.horizontal_top(|ui| {
                glyph(ui, glyph_of(kind), 15.0, color);
                let close = if icon(I_CLOSE).is_empty() {
                    "×"
                } else {
                    I_CLOSE
                };
                let text_color = if matches!(kind, Note::Error) {
                    DANGER
                } else {
                    TEXT
                };
                let width = ui.available_width() - 32.0;
                ui.allocate_ui(Vec2::new(width.max(50.0), 0.0), |ui| {
                    ui.add(egui::Label::new(RichText::new(text).color(text_color)).wrap());
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    let b = egui::Button::new(RichText::new(close).color(FAINT).size(10.0))
                        .frame(false);
                    let resp = ui.add(b).on_hover_text("Dismiss");
                    resp.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::Role::Button, true, "Dismiss")
                    });
                    if resp.clicked() {
                        closed = true;
                    }
                });
            });
        });
    ui.add_space(8.0);
    closed
}

/// A tab: plain text, the selected one darker and underlined. The count is shown in grey.
pub fn tab(ui: &mut egui::Ui, selected: bool, name: &str, count: Option<&str>) -> egui::Response {
    let mut job = egui::text::LayoutJob::default();
    let font = |family: FontFamily| FontId::new(12.5, family);
    job.append(
        name,
        0.0,
        egui::TextFormat::simple(
            font(if selected {
                semibold()
            } else {
                FontFamily::Proportional
            }),
            if selected { TEXT } else { WEAK },
        ),
    );
    if let Some(c) = count {
        job.append(
            c,
            6.0,
            egui::TextFormat::simple(font(FontFamily::Proportional), FAINT),
        );
    }
    let resp = ui.add(egui::Button::new(job).frame(false));
    if selected {
        let r = resp.rect;
        ui.painter().hline(
            r.x_range().shrink(4.0),
            r.bottom() + 1.0,
            Stroke::new(2.0, ACCENT),
        );
    }
    resp
}

/// A button with an icon before its text. Screen readers (and UI Automation) get just `text`.
pub fn button(
    ui: &mut egui::Ui,
    enabled: bool,
    glyph: &'static str,
    text: &str,
    main: bool,
) -> egui::Response {
    let label = with_icon(glyph, text);
    let b = if main {
        primary(label)
    } else {
        egui::Button::new(label)
    };
    let resp = ui.add_enabled(enabled, b);
    let name = text.to_string();
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, enabled, &name));
    resp
}

/// A button that stays pressed in while `on`, e.g. Log. Screen readers get `text` and whether it
/// is on.
pub fn toggle(ui: &mut egui::Ui, on: bool, glyph: &'static str, text: &str) -> egui::Response {
    let resp = ui.add(egui::Button::new(with_icon(glyph, text)).selected(on));
    let name = text.to_string();
    resp.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, on, &name));
    resp
}

/// A decorative icon, painted rather than added as text, so screen readers skip it.
pub fn glyph(ui: &mut egui::Ui, g: &'static str, size: f32, color: Color32) {
    if icon(g).is_empty() {
        return;
    }
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size + 4.0), egui::Sense::hover());
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        g,
        FontId::proportional(size),
        color,
    );
}

/// A table column heading: small and grey.
pub fn th(ui: &mut egui::Ui, title: &str) {
    ui.label(RichText::new(title).size(11.5).color(WEAK));
}

/// A warning strip with one action button on the right. Returns true when the button is clicked.
pub fn warning(ui: &mut egui::Ui, text: &str, action: &str, enabled: bool) -> bool {
    let (color, fill, edge) = tint(Note::Warning);
    let mut clicked = false;
    egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, edge))
        .corner_radius(8)
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                glyph(ui, I_WARNING, 15.0, color);
                let width = ui.available_width() - 230.0;
                ui.allocate_ui(Vec2::new(width.max(80.0), 0.0), |ui| {
                    ui.add(egui::Label::new(RichText::new(text).color(TEXT)).wrap());
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
                    if ui.add_enabled(enabled, egui::Button::new(action)).clicked() {
                        clicked = true;
                    }
                });
            });
        });
    ui.add_space(8.0);
    clicked
}
