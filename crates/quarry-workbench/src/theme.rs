//! Frontend-neutral design tokens, WCAG AA Light/Dark palettes, persisted theme
//! preferences, hardened Omarchy local theme sync, and numeric typography helpers.
//!
//! All colors are expressed as plain RGBA bytes so any frontend (`quarry-egui`,
//! `quarry-gpui`, or headless tests) shares the exact same semantic palette
//! without depending on a GUI crate. Brand amber is reserved for selection,
//! focus, and primary actions in Quarry's built-in Light and Dark themes.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

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

    /// Linearly blends `self` toward `other` by `t` in `[0.0, 1.0]`.
    pub fn blend(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let inv = 1.0 - t;
        Self::rgb(
            (f32::from(self.r) * inv + f32::from(other.r) * t).round() as u8,
            (f32::from(self.g) * inv + f32::from(other.g) * t).round() as u8,
            (f32::from(self.b) * inv + f32::from(other.b) * t).round() as u8,
        )
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
    /// Amber (or theme accent) used for row numbers, links, and the unsaved marker.
    pub accent_text: Rgba,
    pub error_text: Rgba,
    pub error_fill: Rgba,
    pub warning_text: Rgba,
    pub warning_fill: Rgba,
    pub success_text: Rgba,
    /// Whitespace marks inside cells.
    pub mark: Rgba,
    /// The whitespace indicator triangle/dot in a cell's corner.
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

/// Quarry's WCAG AA Dark semantic palette.
pub const DARK: Palette = Palette {
    chrome: Rgba::rgb(26, 27, 30),
    separator: Rgba::rgb(52, 54, 60),
    canvas: Rgba::rgb(19, 20, 23),
    zebra: Rgba::rgb(24, 25, 29),
    grid_line: Rgba::rgb(37, 39, 44),
    header_fill: Rgba::rgb(29, 30, 34),
    text: Rgba::rgb(236, 235, 231),
    text_secondary: Rgba::rgb(168, 166, 160),
    text_tertiary: Rgba::rgb(122, 120, 115),
    hover: Rgba::rgb(39, 41, 46),
    pressed: Rgba::rgb(49, 51, 58),
    control: Rgba::rgb(31, 33, 38),
    control_border: Rgba::rgb(66, 69, 77),
    selection: Rgba::rgb(66, 54, 27),
    selection_strong: ACCENT,
    on_accent: Rgba::rgb(33, 25, 8),
    accent_text: Rgba::rgb(239, 206, 122),
    error_text: Rgba::rgb(255, 146, 138),
    error_fill: Rgba::rgb(64, 25, 23),
    warning_text: Rgba::rgb(245, 201, 102),
    warning_fill: Rgba::rgb(59, 44, 16),
    success_text: Rgba::rgb(110, 214, 152),
    mark: Rgba::rgb(122, 120, 115),
    indicator_fill: Rgba::rgb(124, 168, 174),
    indicator_stroke: Rgba::rgb(184, 216, 220),
};

/// Grid cell text size in logical points.
pub const GRID_FONT_SIZE: f32 = 13.0;

/// Small enough that checkboxes stay square and never read as radio buttons.
pub const CONTROL_RADIUS: u8 = 4;

/// Outer corner radius for dialogs and floating windows in logical points.
pub const WINDOW_RADIUS: u8 = 10;

/// Maximum accepted file size for Omarchy local theme files (64 KiB).
pub const MAX_OMARCHY_THEME_BYTES: u64 = 64 * 1024;

const MAX_THEME_PREFERENCES_BYTES: u64 = 4 * 1024;
const MAX_OMARCHY_GENERATION_BYTES: u64 = 256;

/// WCAG 2.1 relative luminance for an sRGB color.
pub fn relative_luminance(c: Rgba) -> f64 {
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

/// WCAG 2.1 contrast ratio between two opaque colors (1.0 to 21.0).
pub fn contrast_ratio(a: Rgba, b: Rgba) -> f64 {
    let la = relative_luminance(a);
    let lb = relative_luminance(b);
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Checks whether every semantic text/surface pair in `palette` meets WCAG AA (`>= 4.5:1`).
pub fn palette_meets_wcag_aa(palette: &Palette) -> bool {
    contrast_ratio(palette.text, palette.canvas) >= 4.5
        && contrast_ratio(palette.text, palette.zebra) >= 4.5
        && contrast_ratio(palette.text, palette.chrome) >= 4.5
        && contrast_ratio(palette.text, palette.selection) >= 4.5
        && contrast_ratio(palette.text_secondary, palette.canvas) >= 4.5
        && contrast_ratio(palette.text_secondary, palette.header_fill) >= 4.5
        && contrast_ratio(palette.on_accent, palette.selection_strong) >= 4.5
        && contrast_ratio(palette.accent_text, palette.canvas) >= 4.5
        && contrast_ratio(palette.error_text, palette.error_fill) >= 4.5
        && contrast_ratio(palette.warning_text, palette.warning_fill) >= 4.5
        && contrast_ratio(palette.success_text, palette.canvas) >= 4.5
}

/// Effective Light or Dark polarity for a resolved theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ThemeAppearance {
    Light,
    Dark,
}

impl ThemeAppearance {
    pub fn fallback_palette(self) -> Palette {
        match self {
            Self::Light => LIGHT,
            Self::Dark => DARK,
        }
    }
}

/// User-selected theme preference persisted across launches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ThemePreference {
    #[default]
    System,
    Light,
    Dark,
    Omarchy,
}

impl ThemePreference {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
            Self::Omarchy => "omarchy",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
            Self::Omarchy => "Omarchy",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "system" => Some(Self::System),
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            "omarchy" => Some(Self::Omarchy),
            _ => None,
        }
    }
}

/// Fully resolved theme tokens ready to apply to any frontend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTheme {
    pub preference: ThemePreference,
    pub appearance: ThemeAppearance,
    pub palette: Palette,
    pub source_label: String,
    pub warning: Option<String>,
}

/// Persisted theme preference (`theme=system|light|dark|omarchy`) stored alongside
/// Quarry's other private local preferences.
#[derive(Debug, Default)]
pub struct ThemePreferences {
    pub preference: ThemePreference,
    pub load_error: Option<String>,
    path: Option<PathBuf>,
    persist_enabled: bool,
}

impl ThemePreferences {
    pub fn load_default() -> Self {
        Self::load_from(default_theme_preferences_path(), true)
    }

    pub fn load_from(path: Option<PathBuf>, persist_enabled: bool) -> Self {
        let mut preference = ThemePreference::default();
        let mut load_error = None;
        if let Some(pref_path) = path.as_ref() {
            match std::fs::File::open(pref_path) {
                Ok(file) => {
                    let mut raw = Vec::new();
                    match file
                        .take(MAX_THEME_PREFERENCES_BYTES + 1)
                        .read_to_end(&mut raw)
                    {
                        Ok(_) if raw.len() as u64 > MAX_THEME_PREFERENCES_BYTES => {
                            load_error = Some(format!(
                                "Saved theme preference ({}) exceeds the maximum supported size; using System.",
                                pref_path.display()
                            ));
                        }
                        Ok(_) => match String::from_utf8(raw) {
                            Ok(content) if !content.trim().is_empty() => {
                                match parse_theme_pref(&content) {
                                    Some(parsed) => preference = parsed,
                                    None => {
                                        load_error = Some(format!(
                                            "Saved theme preference ({}) could not be parsed; using System.",
                                            pref_path.display()
                                        ));
                                    }
                                }
                            }
                            Ok(_) => {}
                            Err(_) => {
                                load_error = Some(format!(
                                    "Saved theme preference ({}) is not valid UTF-8; using System.",
                                    pref_path.display()
                                ));
                            }
                        },
                        Err(error) => {
                            load_error = Some(format!(
                                "Could not read saved theme preference ({}): {error}",
                                pref_path.display()
                            ));
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    load_error = Some(format!(
                        "Could not read saved theme preference ({}): {error}",
                        pref_path.display()
                    ));
                }
            }
        }
        Self {
            preference,
            load_error,
            path,
            persist_enabled,
        }
    }

    pub fn set_preference(&mut self, preference: ThemePreference) -> Result<(), String> {
        let previous = self.preference;
        self.preference = preference;
        if let Err(error) = self.save() {
            self.preference = previous;
            return Err(error);
        }
        self.load_error = None;
        Ok(())
    }

    fn save(&self) -> Result<(), String> {
        if !self.persist_enabled {
            return Ok(());
        }
        let Some(path) = self.path.as_ref() else {
            return Err(
                "Could not determine a configuration directory for theme preferences.".into(),
            );
        };
        let parent = path
            .parent()
            .ok_or_else(|| "Invalid theme preferences path.".to_owned())?;
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create config directory: {error}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }

        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("theme-preferences");
        let tmp_name = format!(
            ".{file_name}.{}.{}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        );
        let tmp_path = parent.join(tmp_name);
        let payload = format!("theme={}\n", self.preference.as_str());
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts
            .open(&tmp_path)
            .map_err(|error| format!("Could not create theme preferences file: {error}"))?;
        if let Err(error) = file
            .write_all(payload.as_bytes())
            .and_then(|_| file.sync_all())
        {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(format!("Could not write theme preferences: {error}"));
        }
        drop(file);
        if let Err(error) = std::fs::rename(&tmp_path, path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(format!("Could not save theme preferences: {error}"));
        }
        Ok(())
    }
}

fn parse_theme_pref(content: &str) -> Option<ThemePreference> {
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, val)) = line.split_once('=')
            && key.trim() == "theme"
        {
            return ThemePreference::parse(val);
        }
    }
    None
}

fn unix_theme_preferences_path(
    config_root: Option<PathBuf>,
    user_root: Option<PathBuf>,
) -> Option<PathBuf> {
    config_root
        .filter(|root| root.is_absolute())
        .or_else(|| {
            user_root
                .filter(|root| root.is_absolute())
                .map(|root| root.join(".config"))
        })
        .map(|root| root.join("quarry/theme-preferences"))
}

fn default_theme_preferences_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .filter(|root| root.is_absolute())
            .map(|root| root.join("quarry").join("theme-preferences"))
    }
    #[cfg(not(windows))]
    {
        unix_theme_preferences_path(
            std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
            std::env::var_os("HOME").map(PathBuf::from),
        )
    }
}

/// Returns the strictly allowlisted Omarchy theme roots beneath `home`:
/// `{ ~/.config/omarchy, ~/.local/share/omarchy, ~/.local/state/omarchy }`.
pub fn omarchy_allowed_roots(home: &Path) -> [PathBuf; 3] {
    [
        home.join(".config/omarchy"),
        home.join(".local/share/omarchy"),
        home.join(".local/state/omarchy"),
    ]
}

fn omarchy_candidate_color_files(home: &Path) -> [PathBuf; 3] {
    [
        home.join(".local/state/omarchy/current/theme/colors.toml"),
        home.join(".config/omarchy/current/theme/colors.toml"),
        home.join(".local/share/omarchy/current/theme/colors.toml"),
    ]
}

fn omarchy_candidate_name_files(home: &Path) -> [PathBuf; 3] {
    [
        home.join(".local/state/omarchy/current/theme.name"),
        home.join(".config/omarchy/current/theme.name"),
        home.join(".local/share/omarchy/current/theme.name"),
    ]
}

fn omarchy_candidate_generation_files(home: &Path) -> [PathBuf; 3] {
    [
        home.join(".local/state/omarchy/current/theme.generation"),
        home.join(".config/omarchy/current/theme.generation"),
        home.join(".local/share/omarchy/current/theme.generation"),
    ]
}

/// Reads an allowlisted local file with strict canonical-root validation, regular-file check,
/// and `max_bytes` cap so symlink escapes and oversized files are rejected cleanly.
fn read_allowlisted_file(
    candidate: &Path,
    canonical_roots: &[PathBuf],
    max_bytes: u64,
) -> Result<Option<String>, String> {
    if !candidate.exists() && std::fs::symlink_metadata(candidate).is_err() {
        return Ok(None);
    }
    let canonical = std::fs::canonicalize(candidate).map_err(|error| {
        format!(
            "Omarchy theme path {} could not be resolved safely: {error}",
            candidate.display()
        )
    })?;
    if !canonical_roots
        .iter()
        .any(|root| canonical.starts_with(root))
    {
        return Err(format!(
            "Refused to read Omarchy theme file outside allowed roots ({}).",
            candidate.display()
        ));
    }
    let file = std::fs::File::open(&canonical).map_err(|error| {
        format!(
            "Could not open Omarchy theme file {}: {error}",
            canonical.display()
        )
    })?;
    let meta = file.metadata().map_err(|error| {
        format!(
            "Could not inspect Omarchy theme file {}: {error}",
            canonical.display()
        )
    })?;
    if !meta.is_file() {
        return Err(format!(
            "Omarchy theme path {} is not a regular file.",
            candidate.display()
        ));
    }
    if meta.len() > max_bytes {
        return Err(format!(
            "Omarchy theme file {} exceeds the {} KiB safety limit.",
            candidate.display(),
            max_bytes / 1024
        ));
    }
    let mut raw = Vec::new();
    file.take(max_bytes + 1)
        .read_to_end(&mut raw)
        .map_err(|error| {
            format!(
                "Could not read Omarchy theme file {}: {error}",
                canonical.display()
            )
        })?;
    if raw.len() as u64 > max_bytes {
        return Err(format!(
            "Omarchy theme file {} exceeds the {} KiB safety limit.",
            candidate.display(),
            max_bytes / 1024
        ));
    }
    String::from_utf8(raw).map(Some).map_err(|_| {
        format!(
            "Omarchy theme file {} is not valid UTF-8.",
            canonical.display()
        )
    })
}

fn canonical_omarchy_roots(home: &Path) -> Vec<PathBuf> {
    if !home.is_absolute() {
        return Vec::new();
    }
    omarchy_allowed_roots(home)
        .into_iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .collect()
}

/// Computes a lightweight fingerprint for the active Omarchy theme beneath `home` so a UI
/// polling loop can detect live theme switches without extra watcher dependencies.
pub fn omarchy_theme_fingerprint(home: &Path) -> Option<String> {
    let roots = canonical_omarchy_roots(home);
    if roots.is_empty() {
        return None;
    }
    for gen_file in omarchy_candidate_generation_files(home) {
        if let Ok(Some(contents)) =
            read_allowlisted_file(&gen_file, &roots, MAX_OMARCHY_GENERATION_BYTES)
        {
            let trimmed = contents.trim();
            if !trimmed.is_empty() {
                return Some(format!("gen:{trimmed}"));
            }
        }
    }
    for color_file in omarchy_candidate_color_files(home) {
        if let Ok(canonical) = std::fs::canonicalize(&color_file)
            && roots.iter().any(|root| canonical.starts_with(root))
            && let Ok(meta) = std::fs::metadata(&canonical)
            && meta.is_file()
        {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            return Some(format!(
                "file:{}:{}:{}",
                canonical.display(),
                meta.len(),
                mtime
            ));
        }
    }
    None
}

/// Loads and validates the active Omarchy theme from `home` using strict allowlist,
/// symlink-escape, 64 KiB cap, and WCAG AA contrast enforcement.
pub fn load_omarchy_theme_from_home(
    home: &Path,
    fallback_appearance: ThemeAppearance,
) -> Result<ResolvedTheme, String> {
    if !home.is_absolute() {
        return Err("Home directory is not absolute; cannot load Omarchy theme.".into());
    }
    let canonical_roots = canonical_omarchy_roots(home);
    if canonical_roots.is_empty() {
        return Err("No Omarchy theme directory was found.".into());
    }

    let mut last_error: Option<String> = None;
    let mut loaded_contents: Option<String> = None;
    for candidate in omarchy_candidate_color_files(home) {
        match read_allowlisted_file(&candidate, &canonical_roots, MAX_OMARCHY_THEME_BYTES) {
            Ok(Some(contents)) => {
                loaded_contents = Some(contents);
                break;
            }
            Ok(None) => {}
            Err(error) => {
                last_error = Some(error);
            }
        }
    }

    let Some(contents) = loaded_contents else {
        return Err(last_error.unwrap_or_else(|| {
            "No Omarchy colors.toml file was found in the allowed Omarchy directories.".into()
        }));
    };

    let theme_name = omarchy_candidate_name_files(home)
        .into_iter()
        .find_map(|path| {
            read_allowlisted_file(&path, &canonical_roots, MAX_OMARCHY_GENERATION_BYTES)
                .ok()
                .flatten()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        });

    let (appearance, palette) = parse_omarchy_palette(&contents, fallback_appearance)?;
    let source_label = match theme_name {
        Some(name) => format!("Omarchy ({name})"),
        None => "Omarchy".into(),
    };
    Ok(ResolvedTheme {
        preference: ThemePreference::Omarchy,
        appearance,
        palette,
        source_label,
        warning: None,
    })
}

/// Resolves a [`ThemePreference`] against the current system polarity and optional user home.
pub fn resolve_theme(
    preference: ThemePreference,
    system_appearance: ThemeAppearance,
    home: Option<&Path>,
) -> ResolvedTheme {
    match preference {
        ThemePreference::System => ResolvedTheme {
            preference,
            appearance: system_appearance,
            palette: system_appearance.fallback_palette(),
            source_label: format!(
                "System ({})",
                match system_appearance {
                    ThemeAppearance::Light => "Light",
                    ThemeAppearance::Dark => "Dark",
                }
            ),
            warning: None,
        },
        ThemePreference::Light => ResolvedTheme {
            preference,
            appearance: ThemeAppearance::Light,
            palette: LIGHT,
            source_label: "Light".into(),
            warning: None,
        },
        ThemePreference::Dark => ResolvedTheme {
            preference,
            appearance: ThemeAppearance::Dark,
            palette: DARK,
            source_label: "Dark".into(),
            warning: None,
        },
        ThemePreference::Omarchy => {
            let result = match home {
                Some(h) => load_omarchy_theme_from_home(h, system_appearance),
                None => Err("Could not determine home directory for Omarchy theme sync.".into()),
            };
            match result {
                Ok(resolved) => resolved,
                Err(error) => ResolvedTheme {
                    preference,
                    appearance: system_appearance,
                    palette: system_appearance.fallback_palette(),
                    source_label: match system_appearance {
                        ThemeAppearance::Light => "Light (Omarchy fallback)".into(),
                        ThemeAppearance::Dark => "Dark (Omarchy fallback)".into(),
                    },
                    warning: Some(error),
                },
            }
        }
    }
}

/// Resolves a [`ThemePreference`] using the process's `HOME` environment variable.
pub fn resolve_theme_for_current_user(
    preference: ThemePreference,
    system_appearance: ThemeAppearance,
) -> ResolvedTheme {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    resolve_theme(preference, system_appearance, home.as_deref())
}

fn parse_hex_color(value: &str) -> Option<Rgba> {
    let raw = value.trim().trim_matches('"').trim_matches('\'');
    let hex = raw
        .strip_prefix('#')
        .or_else(|| raw.strip_prefix("0x"))
        .or_else(|| raw.strip_prefix("0X"))?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Rgba::rgb(r, g, b))
}

fn parse_toml_kv(content: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for raw_line in content.lines() {
        let mut line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim().to_ascii_lowercase();
        let mut val = v.trim();
        // Strip inline `# comment` only when outside quotes.
        if let Some(rest) = val.strip_prefix('"') {
            if let Some(end) = rest.find('"') {
                val = &rest[..end];
            }
        } else if let Some(rest) = val.strip_prefix('\'') {
            if let Some(end) = rest.find('\'') {
                val = &rest[..end];
            }
        } else {
            if let Some((before_comment, _)) = val.split_once('#') {
                line = before_comment.trim();
                val = line;
            }
        }
        if !key.is_empty() && !val.is_empty() {
            map.insert(key, val.trim().to_owned());
        }
    }
    map
}

fn ensure_contrast(fg: Rgba, bg: Rgba, fallback: Rgba, min_ratio: f64) -> Rgba {
    if contrast_ratio(fg, bg) >= min_ratio {
        return fg;
    }
    if contrast_ratio(fallback, bg) >= min_ratio {
        return fallback;
    }
    let white = Rgba::rgb(250, 250, 250);
    let black = Rgba::rgb(18, 18, 18);
    if contrast_ratio(white, bg) >= contrast_ratio(black, bg) {
        white
    } else {
        black
    }
}

fn parse_omarchy_palette(
    content: &str,
    fallback_appearance: ThemeAppearance,
) -> Result<(ThemeAppearance, Palette), String> {
    let kv = parse_toml_kv(content);
    let color = |keys: &[&str]| -> Option<Rgba> {
        keys.iter()
            .find_map(|k| kv.get(*k).and_then(|v| parse_hex_color(v)))
    };

    let background = color(&["background", "color0"])
        .ok_or_else(|| "Omarchy theme file is missing a valid `background` color.".to_owned())?;
    let foreground = color(&["foreground", "bright_foreground", "color7", "color15"])
        .ok_or_else(|| "Omarchy theme file is missing a valid `foreground` color.".to_owned())?;

    if contrast_ratio(foreground, background) < 4.5 {
        return Err(
            "Omarchy theme foreground/background contrast is below WCAG AA (4.5:1); falling back."
                .into(),
        );
    }

    let appearance = match kv.get("mode").map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("light") => ThemeAppearance::Light,
        Some("dark") => ThemeAppearance::Dark,
        _ => {
            if relative_luminance(background) < 0.25 {
                ThemeAppearance::Dark
            } else if relative_luminance(background) > 0.45 {
                ThemeAppearance::Light
            } else {
                fallback_appearance
            }
        }
    };
    let base = appearance.fallback_palette();
    let is_dark = appearance == ThemeAppearance::Dark;

    let canvas = background;
    let chrome = color(&["dark_background", "darker_background"]).unwrap_or_else(|| {
        if is_dark {
            canvas.blend(Rgba::rgb(0, 0, 0), 0.18)
        } else {
            canvas.blend(Rgba::rgb(0, 0, 0), 0.04)
        }
    });
    let lighter = color(&["lighter_background"]).unwrap_or_else(|| {
        if is_dark {
            canvas.blend(foreground, 0.08)
        } else {
            canvas.blend(Rgba::rgb(255, 255, 255), 0.08)
        }
    });
    let muted = color(&["muted", "color8"]).unwrap_or_else(|| canvas.blend(foreground, 0.28));
    let accent = color(&["accent", "blue", "color4"]).unwrap_or(base.selection_strong);
    let raw_selection =
        color(&["selection", "selection_background"]).unwrap_or_else(|| canvas.blend(accent, 0.24));
    let zebra = canvas.blend(lighter, if is_dark { 0.35 } else { 0.55 });
    let header_fill = canvas.blend(chrome, 0.65);
    let separator = canvas.blend(muted, 0.45);
    let grid_line = canvas.blend(muted, 0.24);
    let hover = canvas.blend(lighter, 0.75);
    let pressed = canvas.blend(muted, 0.45);
    let control = if is_dark { lighter } else { canvas };
    let control_border = canvas.blend(muted, 0.60);

    // Ensure primary text stays WCAG AA against canvas, zebra, chrome, and selection.
    let bright_fg = color(&["bright_foreground", "light_foreground"]).unwrap_or(foreground);
    let text = ensure_contrast(foreground, canvas, bright_fg, 4.5);
    let selection = if contrast_ratio(text, raw_selection) >= 4.5 {
        raw_selection
    } else {
        canvas.blend(accent, if is_dark { 0.22 } else { 0.20 })
    };
    let text = if contrast_ratio(text, selection) >= 4.5
        && contrast_ratio(text, chrome) >= 4.5
        && contrast_ratio(text, zebra) >= 4.5
    {
        text
    } else {
        ensure_contrast(bright_fg, selection, base.text, 4.5)
    };

    let raw_secondary =
        color(&["dark_foreground", "color8"]).unwrap_or_else(|| canvas.blend(text, 0.68));
    let mut text_secondary = ensure_contrast(raw_secondary, canvas, text, 4.5);
    if contrast_ratio(text_secondary, header_fill) < 4.5 {
        text_secondary = ensure_contrast(text_secondary.blend(text, 0.45), header_fill, text, 4.5);
    }
    let text_tertiary = canvas.blend(text, 0.45);

    let on_accent = ensure_contrast(
        color(&["selection_foreground", "darker_background", "background"])
            .unwrap_or(base.on_accent),
        accent,
        base.on_accent,
        4.5,
    );
    let accent_text = ensure_contrast(accent, canvas, text, 4.5);

    let raw_red = color(&["bright_red", "red", "color1", "color9"]).unwrap_or(base.error_text);
    let raw_yellow =
        color(&["bright_yellow", "yellow", "color3", "color11"]).unwrap_or(base.warning_text);
    let raw_green =
        color(&["bright_green", "green", "color2", "color10"]).unwrap_or(base.success_text);
    let raw_cyan = color(&["cyan", "bright_cyan", "color6"]).unwrap_or(base.indicator_fill);

    let error_fill = canvas.blend(raw_red, if is_dark { 0.18 } else { 0.12 });
    let warning_fill = canvas.blend(raw_yellow, if is_dark { 0.18 } else { 0.14 });
    let error_text = ensure_contrast(raw_red, error_fill, base.error_text, 4.5);
    let warning_text = ensure_contrast(raw_yellow, warning_fill, base.warning_text, 4.5);
    let success_text = ensure_contrast(raw_green, canvas, base.success_text, 4.5);

    let palette = Palette {
        chrome,
        separator,
        canvas,
        zebra,
        grid_line,
        header_fill,
        text,
        text_secondary,
        text_tertiary,
        hover,
        pressed,
        control,
        control_border,
        selection,
        selection_strong: accent,
        on_accent,
        accent_text,
        error_text,
        error_fill,
        warning_text,
        warning_fill,
        success_text,
        mark: text_tertiary,
        indicator_fill: raw_cyan,
        indicator_stroke: raw_cyan.blend(text, 0.4),
    };

    if !palette_meets_wcag_aa(&palette) {
        return Err(
            "Derived Omarchy palette failed WCAG AA contrast validation; falling back.".into(),
        );
    }

    Ok((appearance, palette))
}

/// Returns `true` when `text` looks like a formatted scalar number (currency, sign,
/// thousands separators, optional decimal, optional scientific exponent, optional `%`).
/// Used by desktop frontends to right-align numeric cells with OpenType `tnum` tabular figures.
pub fn looks_numeric(text: &str) -> bool {
    let text = text.trim();
    let body = text
        .trim_start_matches(['-', '+', '$', '€', '£'])
        .trim_end_matches('%');
    if body.is_empty() || body.len() > 32 {
        return false;
    }
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(index) => (&body[..index], Some(&body[index + 1..])),
        None => (body, None),
    };
    let mantissa_ok = mantissa.bytes().any(|byte| byte.is_ascii_digit())
        && mantissa
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'.' | b','))
        && mantissa.bytes().filter(|byte| *byte == b'.').count() <= 1;
    let exponent_ok = exponent.is_none_or(|exponent| {
        let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        !digits.is_empty() && digits.len() <= 7 && digits.bytes().all(|byte| byte.is_ascii_digit())
    });
    mantissa_ok && exponent_ok
}

/// Classifies a cell's raw bytes before whitespace rendering turns edge spaces into marks.
pub fn source_looks_numeric(value: &[u8]) -> bool {
    value.len() <= 64 && std::str::from_utf8(value).is_ok_and(looks_numeric)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(palette_meets_wcag_aa(&LIGHT));
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

    #[test]
    fn dark_palette_foreground_pairs_meet_wcag_aa_contrast() {
        assert!(palette_meets_wcag_aa(&DARK));
        assert_eq!(DARK.selection_strong, ACCENT);
        assert!(contrast_ratio(DARK.text, DARK.canvas) >= 10.0);
        assert!(contrast_ratio(DARK.text, DARK.zebra) >= 10.0);
        assert!(contrast_ratio(DARK.text, DARK.chrome) >= 10.0);
        assert!(contrast_ratio(DARK.text, DARK.selection) >= 7.0);
        assert!(contrast_ratio(DARK.text_secondary, DARK.canvas) >= 4.5);
        assert!(contrast_ratio(DARK.text_secondary, DARK.header_fill) >= 4.5);
        assert!(contrast_ratio(DARK.on_accent, DARK.selection_strong) >= 7.0);
        assert!(contrast_ratio(DARK.accent_text, DARK.canvas) >= 4.5);
        assert!(contrast_ratio(DARK.error_text, DARK.error_fill) >= 4.5);
        assert!(contrast_ratio(DARK.warning_text, DARK.warning_fill) >= 4.5);
        assert!(contrast_ratio(DARK.success_text, DARK.canvas) >= 4.5);
    }

    #[test]
    fn theme_preferences_round_trip_and_handle_missing_or_corrupt_files() {
        let dir = tempfile::tempdir().unwrap();
        let pref_path = dir.path().join("quarry/theme-preferences");
        let mut prefs = ThemePreferences::load_from(Some(pref_path.clone()), true);
        assert_eq!(prefs.preference, ThemePreference::System);
        assert!(prefs.load_error.is_none());

        prefs.set_preference(ThemePreference::Dark).unwrap();
        let reloaded = ThemePreferences::load_from(Some(pref_path.clone()), true);
        assert_eq!(reloaded.preference, ThemePreference::Dark);
        assert!(reloaded.load_error.is_none());

        prefs.set_preference(ThemePreference::Omarchy).unwrap();
        let reloaded_omarchy = ThemePreferences::load_from(Some(pref_path.clone()), true);
        assert_eq!(reloaded_omarchy.preference, ThemePreference::Omarchy);

        std::fs::write(&pref_path, "theme=invalid-mode\n").unwrap();
        let bad = ThemePreferences::load_from(Some(pref_path), true);
        assert_eq!(bad.preference, ThemePreference::System);
        assert!(bad.load_error.is_some());
    }

    #[test]
    fn omarchy_theme_sync_parses_tokyo_night_and_meets_wcag_aa() {
        let home = tempfile::tempdir().unwrap();
        let theme_dir = home.path().join(".local/state/omarchy/current/theme");
        std::fs::create_dir_all(&theme_dir).unwrap();
        std::fs::write(
            home.path().join(".local/state/omarchy/current/theme.name"),
            "tokyo-night\n",
        )
        .unwrap();
        std::fs::write(
            home.path()
                .join(".local/state/omarchy/current/theme.generation"),
            "704-1794797536195763\n",
        )
        .unwrap();
        std::fs::write(
            theme_dir.join("colors.toml"),
            r##"
mode = "dark"
accent = "#7aa2f7"
selection = "#292e42"
muted = "#414868"
background = "#1a1b26"
dark_background = "#13141c"
lighter_background = "#24283b"
foreground = "#a9b1d6"
dark_foreground = "#565f89"
bright_foreground = "#c0caf5"
red = "#f7768e"
yellow = "#e0af68"
green = "#9ece6a"
cyan = "#449dab"
"##,
        )
        .unwrap();

        let resolved = resolve_theme(
            ThemePreference::Omarchy,
            ThemeAppearance::Light,
            Some(home.path()),
        );
        assert_eq!(resolved.appearance, ThemeAppearance::Dark);
        assert_eq!(resolved.source_label, "Omarchy (tokyo-night)");
        assert!(resolved.warning.is_none());
        assert_eq!(resolved.palette.canvas, Rgba::rgb(0x1a, 0x1b, 0x26));
        assert_eq!(
            resolved.palette.selection_strong,
            Rgba::rgb(0x7a, 0xa2, 0xf7)
        );
        assert!(palette_meets_wcag_aa(&resolved.palette));
        assert_eq!(
            omarchy_theme_fingerprint(home.path()).as_deref(),
            Some("gen:704-1794797536195763")
        );
    }

    #[test]
    fn omarchy_theme_sync_rejects_oversized_files_malformed_contrast_and_missing_dirs() {
        let home = tempfile::tempdir().unwrap();
        // Missing Omarchy dirs fall back cleanly to system appearance.
        let missing = resolve_theme(
            ThemePreference::Omarchy,
            ThemeAppearance::Light,
            Some(home.path()),
        );
        assert_eq!(missing.palette, LIGHT);
        assert!(missing.warning.is_some());

        // Oversized (> 64 KiB) colors.toml is rejected cleanly.
        let theme_dir = home.path().join(".config/omarchy/current/theme");
        std::fs::create_dir_all(&theme_dir).unwrap();
        let mut huge = String::from("background = \"#1a1b26\"\nforeground = \"#c0caf5\"\n");
        huge.push_str(&"#".repeat((MAX_OMARCHY_THEME_BYTES as usize) + 128));
        std::fs::write(theme_dir.join("colors.toml"), huge).unwrap();
        let oversized = resolve_theme(
            ThemePreference::Omarchy,
            ThemeAppearance::Dark,
            Some(home.path()),
        );
        assert_eq!(oversized.palette, DARK);
        assert!(
            oversized
                .warning
                .as_deref()
                .is_some_and(|w| w.contains("64 KiB"))
        );

        // Low-contrast (unreadable) foreground/background is rejected with fallback.
        std::fs::write(
            theme_dir.join("colors.toml"),
            "background = \"#222222\"\nforeground = \"#2a2a2a\"\n",
        )
        .unwrap();
        let low_contrast = resolve_theme(
            ThemePreference::Omarchy,
            ThemeAppearance::Dark,
            Some(home.path()),
        );
        assert_eq!(low_contrast.palette, DARK);
        assert!(low_contrast.warning.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn omarchy_theme_sync_rejects_symlink_escapes_and_allows_in_root_symlinks() {
        use std::os::unix::fs::symlink;

        let home = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let evil_file = outside.path().join("evil-colors.toml");
        std::fs::write(
            &evil_file,
            "mode = \"dark\"\nbackground = \"#111111\"\nforeground = \"#eeeeee\"\n",
        )
        .unwrap();

        let theme_dir = home.path().join(".config/omarchy/current/theme");
        std::fs::create_dir_all(&theme_dir).unwrap();
        symlink(&evil_file, theme_dir.join("colors.toml")).unwrap();

        let escaped = resolve_theme(
            ThemePreference::Omarchy,
            ThemeAppearance::Light,
            Some(home.path()),
        );
        assert_eq!(escaped.palette, LIGHT);
        assert!(
            escaped
                .warning
                .as_deref()
                .is_some_and(|w| w.contains("outside allowed roots"))
        );

        // A symlink from ~/.config/omarchy/current/theme/colors.toml into
        // ~/.local/share/omarchy/themes/custom/colors.toml stays inside the 3-root allowlist.
        std::fs::remove_file(theme_dir.join("colors.toml")).unwrap();
        let share_theme = home.path().join(".local/share/omarchy/themes/custom");
        std::fs::create_dir_all(&share_theme).unwrap();
        let valid_target = share_theme.join("colors.toml");
        std::fs::write(
            &valid_target,
            "mode = \"light\"\nbackground = \"#f7f5f0\"\nforeground = \"#1c1c1a\"\naccent = \"#2e6fd9\"\n",
        )
        .unwrap();
        symlink(&valid_target, theme_dir.join("colors.toml")).unwrap();

        let allowed = resolve_theme(
            ThemePreference::Omarchy,
            ThemeAppearance::Dark,
            Some(home.path()),
        );
        assert!(allowed.warning.is_none());
        assert_eq!(allowed.appearance, ThemeAppearance::Light);
        assert_eq!(allowed.palette.canvas, Rgba::rgb(0xf7, 0xf5, 0xf0));
        assert!(palette_meets_wcag_aa(&allowed.palette));
    }

    #[test]
    fn numeric_classifier_identifies_numbers_for_tabular_alignment() {
        assert!(looks_numeric("42"));
        assert!(looks_numeric(" -1,204.50 "));
        assert!(looks_numeric("$1,204.50"));
        assert!(looks_numeric("99.5%"));
        assert!(looks_numeric("1.5e-3"));
        assert!(!looks_numeric(""));
        assert!(!looks_numeric("alpha"));
        assert!(!looks_numeric("1.2.3"));
        assert!(!looks_numeric("2026-11-23"));
        assert!(source_looks_numeric(b"12345"));
        assert!(!source_looks_numeric(&[b'1'; 65]));
    }
}
