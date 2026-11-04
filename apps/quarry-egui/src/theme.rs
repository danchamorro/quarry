//! Quarry's design tokens: one palette per appearance, typography, and shared frames.
//!
//! Every surface reads colors from [`palette`] so light and dark appearances stay
//! consistent. Brand amber is reserved for selection, focus, and primary actions.
use eframe::egui::{self, Color32, CornerRadius, FontFamily, FontId, Stroke, TextStyle};

/// Semantic colors for one appearance.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Palette {
    /// Window chrome: title bar, toolbar, status bar.
    pub chrome: Color32,
    /// Hairlines between chrome and content.
    pub separator: Color32,
    /// Grid and dialog content background.
    pub canvas: Color32,
    /// Alternate grid row.
    pub zebra: Color32,
    /// Lines between grid cells.
    pub grid_line: Color32,
    /// Column header and row-number gutter.
    pub header_fill: Color32,
    pub text: Color32,
    pub text_secondary: Color32,
    pub text_tertiary: Color32,
    /// Hover and pressed fills for borderless chrome controls.
    pub hover: Color32,
    pub pressed: Color32,
    /// Framed control background and border.
    pub control: Color32,
    pub control_border: Color32,
    /// Selected cells, rows, and columns.
    pub selection: Color32,
    /// Selected header labels and focus rings.
    pub selection_strong: Color32,
    /// Text drawn over [`Palette::selection_strong`].
    pub on_accent: Color32,
    /// Amber used for links, the unsaved marker, and warnings.
    pub accent_text: Color32,
    pub error_text: Color32,
    pub error_fill: Color32,
    pub warning_text: Color32,
    pub warning_fill: Color32,
    pub success_text: Color32,
    /// Whitespace marks inside cells.
    pub mark: Color32,
}

/// Brand amber shared by both appearances.
pub(super) const ACCENT: Color32 = Color32::from_rgb(233, 196, 106);

pub(super) const LIGHT: Palette = Palette {
    chrome: Color32::from_rgb(246, 245, 243),
    separator: Color32::from_rgb(222, 220, 216),
    canvas: Color32::from_rgb(255, 255, 255),
    zebra: Color32::from_rgb(250, 249, 247),
    grid_line: Color32::from_rgb(236, 234, 230),
    header_fill: Color32::from_rgb(248, 247, 245),
    text: Color32::from_rgb(29, 29, 27),
    text_secondary: Color32::from_rgb(104, 102, 97),
    text_tertiary: Color32::from_rgb(150, 147, 141),
    hover: Color32::from_rgb(232, 230, 226),
    pressed: Color32::from_rgb(220, 218, 213),
    control: Color32::from_rgb(255, 255, 255),
    control_border: Color32::from_rgb(211, 209, 204),
    selection: Color32::from_rgb(252, 238, 200),
    selection_strong: ACCENT,
    on_accent: Color32::from_rgb(43, 33, 10),
    accent_text: Color32::from_rgb(138, 96, 10),
    error_text: Color32::from_rgb(178, 45, 35),
    error_fill: Color32::from_rgb(253, 236, 233),
    warning_text: Color32::from_rgb(128, 84, 0),
    warning_fill: Color32::from_rgb(255, 245, 219),
    success_text: Color32::from_rgb(29, 120, 74),
    mark: Color32::from_rgb(150, 147, 141),
};

pub(super) const DARK: Palette = Palette {
    chrome: Color32::from_rgb(40, 40, 38),
    separator: Color32::from_rgb(22, 22, 21),
    canvas: Color32::from_rgb(30, 30, 29),
    zebra: Color32::from_rgb(35, 35, 33),
    grid_line: Color32::from_rgb(52, 52, 49),
    header_fill: Color32::from_rgb(38, 38, 36),
    text: Color32::from_rgb(236, 235, 231),
    text_secondary: Color32::from_rgb(168, 166, 160),
    text_tertiary: Color32::from_rgb(120, 118, 113),
    hover: Color32::from_rgb(56, 56, 53),
    pressed: Color32::from_rgb(68, 68, 64),
    control: Color32::from_rgb(52, 52, 49),
    control_border: Color32::from_rgb(70, 70, 66),
    selection: Color32::from_rgb(78, 64, 30),
    selection_strong: ACCENT,
    on_accent: Color32::from_rgb(43, 33, 10),
    accent_text: Color32::from_rgb(240, 196, 98),
    error_text: Color32::from_rgb(255, 128, 115),
    error_fill: Color32::from_rgb(70, 34, 31),
    warning_text: Color32::from_rgb(240, 196, 98),
    warning_fill: Color32::from_rgb(66, 54, 26),
    success_text: Color32::from_rgb(110, 205, 150),
    mark: Color32::from_rgb(120, 118, 113),
};

pub(super) fn palette(visuals: &egui::Visuals) -> &'static Palette {
    if visuals.dark_mode { &DARK } else { &LIGHT }
}

pub(super) fn ui_palette(ui: &egui::Ui) -> &'static Palette {
    palette(ui.visuals())
}

pub(super) fn ctx_palette(ctx: &egui::Context) -> &'static Palette {
    palette(&ctx.style().visuals)
}

/// Grid cell text. Proportional text reads like a document; whitespace marks stay visible.
pub(super) const GRID_FONT_SIZE: f32 = 13.0;

pub(super) fn grid_font() -> FontId {
    FontId::proportional(GRID_FONT_SIZE)
}

/// Per-digit padding that widens each digit 0–9 to the widest digit in the grid font.
///
/// egui can't enable SF Pro's tabular-figures feature, so numeric cells pad each
/// digit instead. Right-aligned numbers then line up without a second font.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct DigitPadding {
    /// Extra width per digit, split evenly before and after it.
    pub digits: [f32; 10],
    /// Width of a space in the grid font, used to size trailing padding.
    pub space: f32,
}

pub(super) fn digit_padding(ui: &egui::Ui) -> DigitPadding {
    let font = grid_font();
    let widths: [f32; 10] = std::array::from_fn(|digit| {
        let digit = char::from_digit(digit as u32, 10).expect("0-9 are digits");
        ui.fonts_mut(|fonts| fonts.glyph_width(&font, digit))
    });
    let widest = widths.iter().copied().fold(0.0, f32::max);
    DigitPadding {
        digits: widths.map(|width| widest - width),
        space: ui.fonts_mut(|fonts| fonts.glyph_width(&font, ' ')),
    }
}

/// Rebuilds `job` so every ASCII digit occupies the same width (tabular figures).
///
/// Each digit is centered in its slot: half its padding goes before it (as the
/// section's leading space) and half after (carried into the next section). A
/// small trailing space holds the last digit's right half so right-aligned
/// numbers keep their columns.
pub(super) fn tabular_digits(
    job: &egui::text::LayoutJob,
    padding: &DigitPadding,
) -> egui::text::LayoutJob {
    let mut out = job.clone();
    out.text.clear();
    out.sections.clear();
    let mut carry = 0.0;
    let mut last_format = None;
    for section in &job.sections {
        let text = &job.text[section.byte_range.clone()];
        let mut leading = section.leading_space;
        let mut run_start = 0;
        for (index, character) in text.char_indices() {
            let Some(digit) = character.to_digit(10) else {
                continue;
            };
            if run_start < index {
                out.append(
                    &text[run_start..index],
                    leading + carry,
                    section.format.clone(),
                );
                leading = 0.0;
                carry = 0.0;
            }
            let half = padding.digits[digit as usize] / 2.0;
            let end = index + character.len_utf8();
            out.append(
                &text[index..end],
                leading + carry + half,
                section.format.clone(),
            );
            leading = 0.0;
            carry = half;
            run_start = end;
        }
        if run_start < text.len() {
            out.append(&text[run_start..], leading + carry, section.format.clone());
            carry = 0.0;
        }
        last_format = Some(section.format.clone());
    }
    if carry > 0.0
        && padding.space > 0.0
        && let Some(mut format) = last_format
    {
        // A space scaled to exactly `carry` points wide; the row keeps its height
        // because the full-size sections set it.
        format.font_id.size *= carry / padding.space;
        out.append(" ", 0.0, format);
    }
    out
}

/// Extra width `tabular_digits` adds to `text`.
pub(super) fn tabular_extra_width(text: &str, padding: &DigitPadding) -> f32 {
    text.chars()
        .filter_map(|character| character.to_digit(10))
        .map(|digit| padding.digits[digit as usize])
        .sum()
}

/// Small enough that checkboxes stay square and never read as radio buttons.
pub(super) const CONTROL_RADIUS: u8 = 4;
pub(super) const WINDOW_RADIUS: u8 = 10;

/// Loads the macOS system faces so Quarry reads like a native app.
///
/// The bundled egui faces remain as fallbacks for symbols the system faces lack,
/// and for platforms where these files do not exist.
pub(super) fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for (name, path, family) in [
        (
            "sf-pro",
            "/System/Library/Fonts/SFNS.ttf",
            FontFamily::Proportional,
        ),
        (
            "sf-mono",
            "/System/Library/Fonts/SFNSMono.ttf",
            FontFamily::Monospace,
        ),
    ] {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        fonts
            .font_data
            .insert(name.to_owned(), egui::FontData::from_owned(bytes).into());
        fonts
            .families
            .entry(family)
            .or_default()
            .insert(0, name.to_owned());
    }
    ctx.set_fonts(fonts);
}

/// Applies Quarry's style to both appearances; egui follows the system setting.
pub(super) fn configure(ctx: &egui::Context) {
    for (theme, palette) in [(egui::Theme::Light, &LIGHT), (egui::Theme::Dark, &DARK)] {
        ctx.set_style_of(theme, style(theme, palette));
    }
}

fn style(theme: egui::Theme, palette: &Palette) -> egui::Style {
    let mut style = egui::Style {
        visuals: theme.default_visuals(),
        ..egui::Style::default()
    };
    style.text_styles = [
        (TextStyle::Small, FontId::proportional(11.0)),
        (TextStyle::Body, FontId::proportional(13.0)),
        (TextStyle::Button, FontId::proportional(13.0)),
        (TextStyle::Heading, FontId::proportional(17.0)),
        (TextStyle::Monospace, FontId::monospace(12.5)),
    ]
    .into();
    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(10.0, 4.0);
    style.spacing.interact_size.y = 24.0;
    style.spacing.menu_margin = egui::Margin::same(6);
    style.spacing.window_margin = egui::Margin::same(16);
    style.spacing.scroll = egui::style::ScrollStyle::floating();
    style.spacing.scroll.bar_width = 8.0;
    style.spacing.scroll.floating_allocated_width = 0.0;

    let visuals = &mut style.visuals;
    visuals.override_text_color = Some(palette.text);
    visuals.weak_text_color = Some(palette.text_secondary);
    visuals.hyperlink_color = palette.accent_text;
    visuals.selection.bg_fill = palette.selection;
    visuals.selection.stroke = Stroke::new(1.0, palette.text);
    visuals.text_cursor.stroke = Stroke::new(2.0, palette.selection_strong);
    visuals.panel_fill = palette.chrome;
    visuals.window_fill = palette.canvas;
    visuals.window_stroke = Stroke::new(1.0, palette.control_border);
    visuals.window_corner_radius = CornerRadius::same(WINDOW_RADIUS);
    visuals.window_shadow = egui::epaint::Shadow {
        offset: [0, 10],
        blur: 30,
        spread: 0,
        color: Color32::from_black_alpha(if theme == egui::Theme::Dark { 110 } else { 38 }),
    };
    visuals.popup_shadow = egui::epaint::Shadow {
        offset: [0, 4],
        blur: 16,
        spread: 0,
        color: Color32::from_black_alpha(if theme == egui::Theme::Dark { 90 } else { 30 }),
    };
    visuals.menu_corner_radius = CornerRadius::same(8);
    visuals.faint_bg_color = palette.zebra;
    visuals.extreme_bg_color = palette.canvas;
    visuals.text_edit_bg_color = Some(palette.control);
    visuals.code_bg_color = palette.header_fill;
    visuals.warn_fg_color = palette.warning_text;
    visuals.error_fg_color = palette.error_text;
    visuals.striped = true;
    visuals.indent_has_left_vline = false;

    let radius = CornerRadius::same(CONTROL_RADIUS);
    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_fill = palette.canvas;
    widgets.noninteractive.weak_bg_fill = palette.canvas;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0, palette.grid_line);
    widgets.noninteractive.fg_stroke = Stroke::new(1.0, palette.text);
    widgets.noninteractive.corner_radius = radius;

    widgets.inactive.bg_fill = palette.control;
    widgets.inactive.weak_bg_fill = palette.control;
    widgets.inactive.bg_stroke = Stroke::new(1.0, palette.control_border);
    widgets.inactive.fg_stroke = Stroke::new(1.0, palette.text);
    widgets.inactive.corner_radius = radius;

    widgets.hovered.bg_fill = palette.hover;
    widgets.hovered.weak_bg_fill = palette.hover;
    widgets.hovered.bg_stroke = Stroke::new(1.0, palette.control_border);
    widgets.hovered.fg_stroke = Stroke::new(1.5, palette.text);
    widgets.hovered.corner_radius = radius;
    widgets.hovered.expansion = 0.0;

    widgets.active.bg_fill = palette.pressed;
    widgets.active.weak_bg_fill = palette.pressed;
    widgets.active.bg_stroke = Stroke::new(1.0, palette.selection_strong);
    widgets.active.fg_stroke = Stroke::new(1.5, palette.text);
    widgets.active.corner_radius = radius;
    widgets.active.expansion = 0.0;

    widgets.open.bg_fill = palette.hover;
    widgets.open.weak_bg_fill = palette.hover;
    widgets.open.bg_stroke = Stroke::new(1.0, palette.control_border);
    widgets.open.fg_stroke = Stroke::new(1.0, palette.text);
    widgets.open.corner_radius = radius;
    style
}

/// Borderless controls for the toolbar, tab strip, and status bar.
pub(super) fn chrome_controls(style: &mut egui::Style) {
    let palette = palette(&style.visuals);
    let widgets = &mut style.visuals.widgets;
    widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
    widgets.inactive.bg_fill = Color32::TRANSPARENT;
    widgets.inactive.bg_stroke = Stroke::NONE;
    widgets.hovered.bg_stroke = Stroke::NONE;
    widgets.active.bg_stroke = Stroke::NONE;
    widgets.open.bg_stroke = Stroke::NONE;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0, palette.separator);
    style.visuals.text_edit_bg_color = Some(palette.control);
    style.spacing.button_padding = egui::vec2(8.0, 4.0);
}

/// A primary button: amber fill with dark text in both appearances.
pub(super) fn primary_button<'a>(text: impl Into<String>) -> egui::Button<'a> {
    egui::Button::new(egui::RichText::new(text.into()).color(LIGHT.on_accent)).fill(ACCENT)
}

/// Chrome surface frame with a hairline below or above.
pub(super) fn chrome_frame(palette: &Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(palette.chrome)
        .inner_margin(egui::Margin::symmetric(12, 6))
}

/// Paints a one-pixel hairline along the bottom or top of a panel rect.
pub(super) fn hairline(painter: &egui::Painter, rect: egui::Rect, bottom: bool, color: Color32) {
    let y = if bottom {
        rect.bottom() - 0.5
    } else {
        rect.top() + 0.5
    };
    painter.hline(rect.x_range(), y, Stroke::new(1.0, color));
}

/// A segmented control: mutually exclusive options in one framed group.
///
/// Returns each option's response so callers can add accessible labels.
pub(super) fn segmented<T: PartialEq + Copy>(
    ui: &mut egui::Ui,
    current: &mut T,
    options: &[(T, &str)],
) -> Vec<egui::Response> {
    let palette = ui_palette(ui);
    let count = options.len().max(1) as f32;
    egui::Frame::new()
        .fill(palette.header_fill)
        .stroke(Stroke::new(1.0, palette.control_border))
        .corner_radius(CornerRadius::same(CONTROL_RADIUS + 1))
        .inner_margin(egui::Margin::same(2))
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            let width = (ui.available_width() - 2.0 * (count - 1.0)) / count;
            ui.horizontal(|ui| {
                options
                    .iter()
                    .map(|(value, label)| {
                        let selected = *current == *value;
                        let response = ui.add_sized(
                            [width, 26.0],
                            egui::Button::selectable(selected, *label)
                                .corner_radius(CONTROL_RADIUS)
                                .stroke(if selected {
                                    Stroke::new(1.0, palette.selection_strong)
                                } else {
                                    Stroke::NONE
                                }),
                        );
                        if response.clicked() {
                            *current = *value;
                        }
                        response
                    })
                    .collect()
            })
            .inner
        })
        .inner
}

/// A small secondary label above a group of fields.
pub(super) fn section_label(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let color = ui_palette(ui).text_secondary;
    ui.label(egui::RichText::new(text).size(12.0).color(color))
}

/// A numbered step in a multi-step dialog: a soft card with a step badge and title.
pub(super) fn step_card<R>(
    ui: &mut egui::Ui,
    step: u8,
    title: &str,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    let palette = ui_palette(ui);
    egui::Frame::new()
        .fill(palette.zebra)
        .stroke(Stroke::new(1.0, palette.grid_line))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (badge, _) =
                    ui.allocate_exact_size(egui::vec2(20.0, 20.0), egui::Sense::hover());
                ui.painter()
                    .circle_filled(badge.center(), 10.0, palette.selection);
                ui.painter().text(
                    badge.center(),
                    egui::Align2::CENTER_CENTER,
                    step.to_string(),
                    FontId::proportional(12.0),
                    palette.accent_text,
                );
                ui.label(egui::RichText::new(title).size(15.0).color(palette.text));
            });
            ui.add_space(2.0);
            add_contents(ui)
        })
        .inner
}

/// Width of a row of framed buttons with these labels, for right-aligning them
/// while keeping left-to-right focus order.
pub(super) fn buttons_width(ui: &egui::Ui, labels: &[&str]) -> f32 {
    let font = TextStyle::Button.resolve(ui.style());
    let padding = ui.spacing().button_padding.x;
    let gaps = labels.len().saturating_sub(1) as f32 * ui.spacing().item_spacing.x;
    labels
        .iter()
        .map(|label| {
            ui.fonts_mut(|fonts| {
                fonts
                    .layout_no_wrap((*label).to_owned(), font.clone(), Color32::WHITE)
                    .size()
                    .x
            }) + 2.0 * padding
        })
        .sum::<f32>()
        + gaps
}

/// Moves the cursor so the next `width` points end at the row's right edge.
pub(super) fn push_right(ui: &mut egui::Ui, width: f32) {
    ui.add_space((ui.available_width() - width - 1.0).max(0.0));
}

/// A headline number with a caption, for summaries such as a join review.
pub(super) fn stat(ui: &mut egui::Ui, value: String, caption: &str) {
    let palette = ui_palette(ui);
    egui::Frame::new()
        .fill(palette.canvas)
        .stroke(Stroke::new(1.0, palette.grid_line))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.label(egui::RichText::new(value).size(18.0).color(palette.text));
                ui.label(
                    egui::RichText::new(caption)
                        .size(11.5)
                        .color(palette.text_secondary),
                );
            });
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabular_digits_center_each_digit_and_keep_the_text() {
        let mut job = egui::text::LayoutJob::default();
        job.append("$1,204.50", 0.0, egui::TextFormat::default());
        let padding = DigitPadding {
            digits: std::array::from_fn(|digit| 2.0 * digit as f32),
            space: 4.0,
        };
        let tabular = tabular_digits(&job, &padding);
        assert_eq!(tabular.text, "$1,204.50");
        let leading: Vec<_> = tabular
            .sections
            .iter()
            .map(|section| {
                (
                    &tabular.text[section.byte_range.clone()],
                    section.leading_space,
                )
            })
            .collect();
        // Halves: 1→1, 2→2, 0→0, 4→4, 5→5, 0→0, each split before and after.
        assert_eq!(
            leading,
            [
                ("$", 0.0),
                ("1", 1.0),
                (",", 1.0),
                ("2", 2.0),
                ("0", 2.0),
                ("4", 4.0),
                (".", 4.0),
                ("5", 5.0),
                ("0", 5.0),
            ]
        );
        assert_eq!(tabular_extra_width("$1,204.50", &padding), 24.0);

        // A narrow final digit keeps its right half as a scaled trailing space.
        let mut job = egui::text::LayoutJob::default();
        job.append("21", 0.0, egui::TextFormat::default());
        let tabular = tabular_digits(&job, &padding);
        assert_eq!(tabular.text, "21 ");
        let trailing = tabular.sections.last().unwrap();
        assert_eq!(
            trailing.format.font_id.size,
            egui::TextFormat::default().font_id.size * 1.0 / 4.0
        );
    }
}
