//! Frontend-neutral design tokens for Quarry's Light appearance.
//!
//! All colors are expressed as plain RGBA bytes so any frontend (`quarry-egui`,
//! future native/GPUI frontends, or headless tests) shares the exact same
//! semantic palette without depending on a GUI crate. Brand amber is reserved
//! for selection, focus, and primary actions.

/// Unpremultiplied 8-bit sRGB color with alpha channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    /// Creates an opaque sRGB color (`a = 255`).
    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b, a: 255 }
    }

    /// Creates an sRGB color with an explicit alpha channel.
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// Returns `[r, g, b, a]` bytes.
    pub const fn to_array(self) -> [u8; 4] {
        [self.r, self.g, self.b, self.a]
    }

    /// Returns the packed `0x00RRGGBB` value.
    pub const fn to_rgb_hex(self) -> u32 {
        ((self.r as u32) << 16) | ((self.g as u32) << 8) | (self.b as u32)
    }
}

/// Quarry's semantic color roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    /// Window chrome: title bar, toolbar, status bar.
    pub chrome: Rgba,
    /// Hairlines between chrome and content.
    pub separator: Rgba,
    /// Grid and dialog content background.
    pub canvas: Rgba,
    /// Alternate grid row.
    pub zebra: Rgba,
    /// Lines between grid cells.
    pub grid_line: Rgba,
    /// Column header and row-number gutter.
    pub header_fill: Rgba,
    pub text: Rgba,
    pub text_secondary: Rgba,
    pub text_tertiary: Rgba,
    /// Hover and pressed fills for borderless chrome controls.
    pub hover: Rgba,
    pub pressed: Rgba,
    /// Framed control background and border.
    pub control: Rgba,
    pub control_border: Rgba,
    /// Selected cells, rows, and columns.
    pub selection: Rgba,
    /// Selected header labels and focus rings.
    pub selection_strong: Rgba,
    /// Text drawn over [`Palette::selection_strong`].
    pub on_accent: Rgba,
    /// Amber used for links, the unsaved marker, and warnings.
    pub accent_text: Rgba,
    pub error_text: Rgba,
    pub error_fill: Rgba,
    pub warning_text: Rgba,
    pub warning_fill: Rgba,
    pub success_text: Rgba,
    /// Whitespace marks inside cells.
    pub mark: Rgba,
    /// The whitespace indicator triangle in a cell's corner.
    pub indicator_fill: Rgba,
    pub indicator_stroke: Rgba,
}

/// Brand amber.
pub const ACCENT: Rgba = Rgba::rgb(233, 196, 106);

/// Quarry's Light semantic palette.
pub const LIGHT: Palette = Palette {
    chrome: Rgba::rgb(246, 245, 243),
    separator: Rgba::rgb(222, 220, 216),
    canvas: Rgba::rgb(255, 255, 255),
    zebra: Rgba::rgb(250, 249, 247),
    grid_line: Rgba::rgb(236, 234, 230),
    header_fill: Rgba::rgb(248, 247, 245),
    text: Rgba::rgb(29, 29, 27),
    text_secondary: Rgba::rgb(104, 102, 97),
    text_tertiary: Rgba::rgb(150, 147, 141),
    hover: Rgba::rgb(232, 230, 226),
    pressed: Rgba::rgb(220, 218, 213),
    control: Rgba::rgb(255, 255, 255),
    control_border: Rgba::rgb(211, 209, 204),
    selection: Rgba::rgb(252, 238, 200),
    selection_strong: ACCENT,
    on_accent: Rgba::rgb(43, 33, 10),
    accent_text: Rgba::rgb(138, 96, 10),
    error_text: Rgba::rgb(178, 45, 35),
    error_fill: Rgba::rgb(253, 236, 233),
    warning_text: Rgba::rgb(128, 84, 0),
    warning_fill: Rgba::rgb(255, 245, 219),
    success_text: Rgba::rgb(29, 120, 74),
    mark: Rgba::rgb(150, 147, 141),
    indicator_fill: Rgba::rgb(111, 145, 150),
    indicator_stroke: Rgba::rgb(57, 77, 84),
};

/// Grid cell text size in logical points.
pub const GRID_FONT_SIZE: f32 = 13.0;

/// Small enough that checkboxes stay square and never read as radio buttons.
pub const CONTROL_RADIUS: u8 = 4;

/// Outer corner radius for dialogs and floating windows in logical points.
pub const WINDOW_RADIUS: u8 = 10;

#[cfg(test)]
mod tests {
    use super::*;

    fn relative_luminance(c: Rgba) -> f64 {
        fn channel(v: u8) -> f64 {
            let s = f64::from(v) / 255.0;
            if s <= 0.03928 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * channel(c.r) + 0.7152 * channel(c.g) + 0.0722 * channel(c.b)
    }

    fn contrast_ratio(a: Rgba, b: Rgba) -> f64 {
        let la = relative_luminance(a);
        let lb = relative_luminance(b);
        let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn light_palette_matches_expected_rgba_tokens_and_helpers() {
        assert_eq!(ACCENT, Rgba::rgb(233, 196, 106));
        assert_eq!(ACCENT.to_array(), [233, 196, 106, 255]);
        assert_eq!(ACCENT.to_rgb_hex(), 0xE9C46A);
        assert_eq!(Rgba::new(1, 2, 3, 4).to_array(), [1, 2, 3, 4]);
        assert_eq!(LIGHT.selection_strong, ACCENT);
        assert_eq!(LIGHT.chrome.to_array(), [246, 245, 243, 255]);
        assert_eq!(LIGHT.separator.to_array(), [222, 220, 216, 255]);
        assert_eq!(LIGHT.canvas.to_array(), [255, 255, 255, 255]);
        assert_eq!(LIGHT.zebra.to_array(), [250, 249, 247, 255]);
        assert_eq!(LIGHT.grid_line.to_array(), [236, 234, 230, 255]);
        assert_eq!(LIGHT.header_fill.to_array(), [248, 247, 245, 255]);
        assert_eq!(LIGHT.text.to_array(), [29, 29, 27, 255]);
        assert_eq!(LIGHT.text_secondary.to_array(), [104, 102, 97, 255]);
        assert_eq!(LIGHT.text_tertiary.to_array(), [150, 147, 141, 255]);
        assert_eq!(LIGHT.hover.to_array(), [232, 230, 226, 255]);
        assert_eq!(LIGHT.pressed.to_array(), [220, 218, 213, 255]);
        assert_eq!(LIGHT.control.to_array(), [255, 255, 255, 255]);
        assert_eq!(LIGHT.control_border.to_array(), [211, 209, 204, 255]);
        assert_eq!(LIGHT.selection.to_array(), [252, 238, 200, 255]);
        assert_eq!(LIGHT.on_accent.to_array(), [43, 33, 10, 255]);
        assert_eq!(LIGHT.accent_text.to_array(), [138, 96, 10, 255]);
        assert_eq!(LIGHT.error_text.to_array(), [178, 45, 35, 255]);
        assert_eq!(LIGHT.error_fill.to_array(), [253, 236, 233, 255]);
        assert_eq!(LIGHT.warning_text.to_array(), [128, 84, 0, 255]);
        assert_eq!(LIGHT.warning_fill.to_array(), [255, 245, 219, 255]);
        assert_eq!(LIGHT.success_text.to_array(), [29, 120, 74, 255]);
        assert_eq!(LIGHT.mark.to_array(), [150, 147, 141, 255]);
        assert_eq!(LIGHT.indicator_fill.to_array(), [111, 145, 150, 255]);
        assert_eq!(LIGHT.indicator_stroke.to_array(), [57, 77, 84, 255]);
        assert_eq!(GRID_FONT_SIZE, 13.0);
        assert_eq!(CONTROL_RADIUS, 4);
        assert_eq!(WINDOW_RADIUS, 10);
    }

    #[test]
    fn light_palette_foreground_pairs_meet_wcag_aa_contrast() {
        assert!(contrast_ratio(LIGHT.text, LIGHT.canvas) >= 10.0);
        assert!(contrast_ratio(LIGHT.text, LIGHT.zebra) >= 10.0);
        assert!(contrast_ratio(LIGHT.text, LIGHT.chrome) >= 10.0);
        assert!(contrast_ratio(LIGHT.text_secondary, LIGHT.canvas) >= 4.5);
        assert!(contrast_ratio(LIGHT.on_accent, LIGHT.selection_strong) >= 7.0);
        assert!(contrast_ratio(LIGHT.accent_text, LIGHT.canvas) >= 4.5);
        assert!(contrast_ratio(LIGHT.error_text, LIGHT.error_fill) >= 4.5);
        assert!(contrast_ratio(LIGHT.warning_text, LIGHT.warning_fill) >= 4.5);
        assert!(contrast_ratio(LIGHT.success_text, LIGHT.canvas) >= 4.5);
    }
}
