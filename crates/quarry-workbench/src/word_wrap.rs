//! Frontend-neutral adaptive word-wrap policy, atomic preference persistence,
//! bounded per-cell / per-frame layout budgets, and fetched-row height cache.
//!
//! Preserves Quarry's O(1) row-based scrolling (zero height prefix sums, zero
//! full-file scans) while bounding wrap work to visible rows, a uniform 2-line
//! cap, a 4 KiB per-cell byte budget, a 128 KiB per-frame byte budget, and an
//! automatic 300 MB file-size scale-back threshold (`C16`).

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::{ADAPTIVE_MEMORY_BUDGET_BYTES, format_bytes};

/// Default file-size threshold above which word wrap auto-disables (300 MB).
pub const DEFAULT_WORD_WRAP_AUTO_DISABLE_BYTES: u64 = ADAPTIVE_MEMORY_BUDGET_BYTES;

/// Uniform maximum wrapped line count rendered inside a grid cell.
/// Full text remains accessible via inline cell editing, Copy, and inspection.
pub const MAX_WRAPPED_LINES: usize = 2;

/// Maximum raw UTF-8 bytes inspected per cell during wrapped preview layout (4 KiB).
pub const MAX_WRAP_CELL_BYTES: usize = 4 * 1024;

/// Maximum cumulative raw cell bytes wrapped during a single UI frame (128 KiB).
/// Remaining cells in an exceptionally wide/dense frame degrade gracefully to a
/// compact single-line preview so frame times stay strictly bounded.
pub const MAX_WRAP_FRAME_BUDGET_BYTES: usize = 128 * 1024;

/// Maximum number of fetched rows retained in the bounded wrap-metrics cache.
pub const MAX_CACHED_WRAP_ROWS: usize = 512;

/// Uniform row height in logical pixels when word wrap is off (matches `Size::Small`).
pub const SINGLE_LINE_ROW_HEIGHT_PX: f32 = 30.0;

/// Uniform row height in logical pixels when word wrap is active (2 lines + vertical padding).
pub const WRAPPED_ROW_HEIGHT_PX: f32 = 50.0;

/// Line height in logical pixels used inside wrapped cells (`GRID_FONT_SIZE = 13.0`).
pub const WRAPPED_LINE_HEIGHT_PX: f32 = 18.0;

/// Maximum size accepted for persisted word-wrap preferences file (256 bytes).
const MAX_WORD_WRAP_PREFS_BYTES: u64 = 256;

/// Minimum allowed configurable auto-disable threshold (1 MB) to avoid accidental 0-byte lockouts
/// unless explicitly disabled via `0`.
pub const MIN_CONFIGURABLE_AUTO_DISABLE_MB: u64 = 1;

/// Maximum allowed configurable auto-disable threshold (100,000 MB = 100 GB).
pub const MAX_CONFIGURABLE_AUTO_DISABLE_MB: u64 = 100_000;

/// Resolved word-wrap state for a specific document tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectiveWordWrap {
    /// Word wrap is disabled (either globally off or explicitly turned off).
    Off,
    /// Word wrap is active with the uniform 2-line cap and byte budgets.
    Active {
        /// True when the user explicitly overrode `AutoDisabled` for this tab.
        manual_override: bool,
    },
    /// Word wrap is enabled globally, but automatically scaled back because the
    /// active document (or section) exceeds `threshold_bytes`.
    AutoDisabled {
        file_bytes: u64,
        threshold_bytes: u64,
    },
}

impl EffectiveWordWrap {
    /// True when the grid should render wrapped 2-line cells.
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Active { .. })
    }

    /// True when adaptive auto-disable suppressed word wrap on this document.
    pub const fn is_auto_disabled(self) -> bool {
        matches!(self, Self::AutoDisabled { .. })
    }

    /// Uniform table row height in logical pixels for this wrap state.
    pub const fn row_height_px(self) -> f32 {
        if self.is_active() {
            WRAPPED_ROW_HEIGHT_PX
        } else {
            SINGLE_LINE_ROW_HEIGHT_PX
        }
    }

    /// Compact toolbar button label.
    pub const fn toolbar_label(self) -> &'static str {
        match self {
            Self::Off => "Wrap",
            Self::Active { .. } => "Wrap: On",
            Self::AutoDisabled { .. } => "Wrap: Auto-off",
        }
    }

    /// Screen-reader accessible label for the toolbar toggle.
    pub fn accessible_label(self) -> String {
        match self {
            Self::Off => "Word Wrap: Off".into(),
            Self::Active {
                manual_override: false,
            } => format!("Word Wrap: On ({MAX_WRAPPED_LINES}-line cap)"),
            Self::Active {
                manual_override: true,
            } => format!("Word Wrap: On for this tab ({MAX_WRAPPED_LINES}-line cap)"),
            Self::AutoDisabled {
                threshold_bytes, ..
            } => format!(
                "Word Wrap: Auto-disabled (file exceeds {})",
                format_decimal_mb(threshold_bytes)
            ),
        }
    }

    /// Hover tooltip explaining the current wrap state and click behavior.
    pub fn tooltip(self) -> String {
        match self {
            Self::Off => format!(
                "Wrap long cell values up to {MAX_WRAPPED_LINES} lines (Alt+Z). Auto-disables on files larger than 300 MB by default."
            ),
            Self::Active {
                manual_override: false,
            } => format!(
                "Word wrap is on ({MAX_WRAPPED_LINES}-line cap, 4 KiB per-cell layout budget). Click or press Alt+Z to turn off."
            ),
            Self::Active {
                manual_override: true,
            } => format!(
                "Word wrap is enabled for this tab ({MAX_WRAPPED_LINES}-line cap, bounded per-cell and per-frame budgets). Click to return to auto-off."
            ),
            Self::AutoDisabled {
                file_bytes,
                threshold_bytes,
            } => format!(
                "Word wrap auto-disabled because this file ({}) exceeds the {} threshold. Click to enable for this tab.",
                format_bytes(file_bytes),
                format_decimal_mb(threshold_bytes)
            ),
        }
    }

    /// Quiet transition-only status announcement for VoiceOver / AT-SPI.
    pub fn transition_announcement(self) -> String {
        match self {
            Self::Off => "Word wrap off".into(),
            Self::Active {
                manual_override: false,
            } => format!("Word wrap on ({MAX_WRAPPED_LINES}-line cap)"),
            Self::Active {
                manual_override: true,
            } => format!("Word wrap enabled for this tab ({MAX_WRAPPED_LINES}-line cap)"),
            Self::AutoDisabled {
                threshold_bytes, ..
            } => format!(
                "Word wrap auto-disabled for files larger than {}",
                format_decimal_mb(threshold_bytes)
            ),
        }
    }
}

/// Formats a decimal byte threshold cleanly (e.g. `300_000_000` -> `"300 MB"`).
pub fn format_decimal_mb(bytes: u64) -> String {
    if bytes.is_multiple_of(1_000_000) && bytes >= 1_000_000 {
        format!("{} MB", bytes / 1_000_000)
    } else {
        format_bytes(bytes)
    }
}

/// Resolves the effective word-wrap state for a tab given global preferences,
/// any per-tab override, and the document's active byte length (section byte
/// length for bounded sections, file size for full files).
pub fn resolve_word_wrap(
    prefs: &WordWrapPreferences,
    tab_override: Option<bool>,
    active_byte_len: Option<u64>,
) -> EffectiveWordWrap {
    match tab_override {
        Some(true) => EffectiveWordWrap::Active {
            manual_override: true,
        },
        Some(false) => EffectiveWordWrap::Off,
        None => {
            if !prefs.enabled {
                EffectiveWordWrap::Off
            } else if let Some(file_bytes) = active_byte_len
                && prefs.auto_disable_bytes > 0
                && file_bytes > prefs.auto_disable_bytes
            {
                EffectiveWordWrap::AutoDisabled {
                    file_bytes,
                    threshold_bytes: prefs.auto_disable_bytes,
                }
            } else {
                EffectiveWordWrap::Active {
                    manual_override: false,
                }
            }
        }
    }
}

/// Outcome of toggling word wrap on the active tab.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WordWrapToggleOutcome {
    pub effective: EffectiveWordWrap,
    pub next_tab_override: Option<bool>,
    pub persist_warning: Option<String>,
    pub status_notice: Option<String>,
}

/// Ignore unusable environment roots instead of writing preferences beneath the working directory.
pub fn unix_word_wrap_preferences_path(
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
        .map(|root| root.join("quarry/word-wrap-preferences"))
}

/// Persisted adaptive word-wrap preferences (enabled default + auto-disable threshold).
#[derive(Debug, Clone)]
pub struct WordWrapPreferences {
    pub enabled: bool,
    pub auto_disable_bytes: u64,
    path: Option<PathBuf>,
}

impl Default for WordWrapPreferences {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_disable_bytes: DEFAULT_WORD_WRAP_AUTO_DISABLE_BYTES,
            path: None,
        }
    }
}

impl WordWrapPreferences {
    /// Loads preferences from an explicit path (or defaults in memory when `path` is `None`).
    pub fn load(path: Option<PathBuf>) -> Self {
        let mut prefs = Self {
            enabled: false,
            auto_disable_bytes: DEFAULT_WORD_WRAP_AUTO_DISABLE_BYTES,
            path,
        };
        if let Some(pref_path) = prefs.path.as_deref()
            && let Ok(file) = quarry_core::open_regular_file_nofollow(pref_path)
        {
            let mut raw = String::new();
            if file
                .take(MAX_WORD_WRAP_PREFS_BYTES + 1)
                .read_to_string(&mut raw)
                .is_ok()
                && raw.len() as u64 <= MAX_WORD_WRAP_PREFS_BYTES
                && let Some((enabled, threshold)) = parse_word_wrap_prefs(&raw)
            {
                prefs.enabled = enabled;
                prefs.auto_disable_bytes = threshold;
            }
        }
        prefs
    }

    /// Loads application-wide word-wrap preferences from the standard per-user config directory.
    pub fn application() -> Self {
        let path = if cfg!(target_os = "macos") {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|root| root.is_absolute())
                .map(|root| root.join("Library/Application Support/Quarry/word-wrap-preferences"))
        } else if cfg!(target_os = "windows") {
            std::env::var_os("APPDATA")
                .map(PathBuf::from)
                .filter(|root| root.is_absolute())
                .map(|root| root.join("Quarry/word-wrap-preferences"))
        } else {
            unix_word_wrap_preferences_path(
                std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
                std::env::var_os("HOME").map(PathBuf::from),
            )
        };
        Self::load(path)
    }

    /// Returns the configured path when persistence is enabled.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Sets the global default word-wrap flag and persists it atomically.
    pub fn set_enabled(&mut self, enabled: bool) -> io::Result<()> {
        let previous = self.enabled;
        self.enabled = enabled;
        if let Err(err) = self.persist_current() {
            self.enabled = previous;
            return Err(err);
        }
        Ok(())
    }

    /// Sets the adaptive auto-disable file-size threshold in bytes and persists it atomically.
    pub fn set_auto_disable_bytes(&mut self, bytes: u64) -> io::Result<()> {
        let previous = self.auto_disable_bytes;
        self.auto_disable_bytes = bytes;
        if let Err(err) = self.persist_current() {
            self.auto_disable_bytes = previous;
            return Err(err);
        }
        Ok(())
    }

    /// Toggles word wrap for the active tab, honoring the adaptive auto-disable threshold.
    pub fn toggle_for_tab(
        &mut self,
        current_tab_override: Option<bool>,
        active_byte_len: Option<u64>,
    ) -> WordWrapToggleOutcome {
        let current = resolve_word_wrap(self, current_tab_override, active_byte_len);
        match current {
            EffectiveWordWrap::AutoDisabled {
                threshold_bytes, ..
            } => {
                let effective = EffectiveWordWrap::Active {
                    manual_override: true,
                };
                WordWrapToggleOutcome {
                    effective,
                    next_tab_override: Some(true),
                    persist_warning: None,
                    status_notice: Some(format!(
                        "Word wrap enabled for this tab (exceeds {}; {MAX_WRAPPED_LINES}-line and 4 KiB per-cell budgets remain active).",
                        format_decimal_mb(threshold_bytes)
                    )),
                }
            }
            EffectiveWordWrap::Active {
                manual_override: true,
            } => {
                let effective = resolve_word_wrap(self, None, active_byte_len);
                WordWrapToggleOutcome {
                    effective,
                    next_tab_override: None,
                    persist_warning: None,
                    status_notice: None,
                }
            }
            EffectiveWordWrap::Active {
                manual_override: false,
            } => {
                let persist_warning = self
                    .set_enabled(false)
                    .err()
                    .map(|e| format!("Could not save word wrap preference: {e}"));
                WordWrapToggleOutcome {
                    effective: EffectiveWordWrap::Off,
                    next_tab_override: None,
                    persist_warning,
                    status_notice: None,
                }
            }
            EffectiveWordWrap::Off => {
                let exceeds = active_byte_len.is_some_and(|len| {
                    self.auto_disable_bytes > 0 && len > self.auto_disable_bytes
                });
                let persist_warning = self
                    .set_enabled(true)
                    .err()
                    .map(|e| format!("Could not save word wrap preference: {e}"));
                if exceeds {
                    WordWrapToggleOutcome {
                        effective: EffectiveWordWrap::Active {
                            manual_override: true,
                        },
                        next_tab_override: Some(true),
                        persist_warning,
                        status_notice: Some(format!(
                            "Word wrap enabled for this tab (files over {} auto-disable by default; {MAX_WRAPPED_LINES}-line and 4 KiB per-cell budgets remain active).",
                            format_decimal_mb(self.auto_disable_bytes)
                        )),
                    }
                } else {
                    WordWrapToggleOutcome {
                        effective: EffectiveWordWrap::Active {
                            manual_override: false,
                        },
                        next_tab_override: None,
                        persist_warning,
                        status_notice: None,
                    }
                }
            }
        }
    }

    fn persist_current(&self) -> io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("Invalid word wrap preference location"))?;
        std::fs::create_dir_all(parent)?;
        let encoded = format!(
            "word_wrap={}\nauto_disable_bytes={}\n",
            if self.enabled { 1 } else { 0 },
            self.auto_disable_bytes
        );
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        quarry_core::restrict_private_file(file.path(), file.as_file())?;
        file.write_all(encoded.as_bytes())?;
        file.flush()?;
        file.persist(path).map_err(|e| e.error)?;
        Ok(())
    }
}

fn parse_word_wrap_prefs(content: &str) -> Option<(bool, u64)> {
    let mut enabled: Option<bool> = None;
    let mut auto_disable_bytes = DEFAULT_WORD_WRAP_AUTO_DISABLE_BYTES;
    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line.split_once('=')?;
        match key {
            "word_wrap" => match value {
                "0" => enabled = Some(false),
                "1" => enabled = Some(true),
                _ => return None,
            },
            "auto_disable_bytes" => {
                let parsed: u64 = value.parse().ok()?;
                auto_disable_bytes = parsed;
            }
            _ => return None,
        }
    }
    Some((enabled?, auto_disable_bytes))
}

/// Returns the largest index `<= max_bytes` that does not split a UTF-8 codepoint
/// when `bytes` is valid UTF-8 (falls back cleanly on invalid UTF-8 bytes).
pub fn utf8_bounded_prefix_len(bytes: &[u8], max_bytes: usize) -> usize {
    if bytes.len() <= max_bytes {
        return bytes.len();
    }
    let mut end = max_bytes;
    // Walk back at most 3 continuation bytes (0b10xx_xxxx) to stay on a codepoint boundary.
    let mut back = 0;
    while end > 0 && back < 3 && (bytes[end] & 0xC0) == 0x80 {
        end -= 1;
        back += 1;
    }
    end
}

/// Measured wrapped-layout metrics for a single cell under strict byte and line caps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WrapCellMetrics {
    /// Estimated number of visual lines needed (`1..=MAX_WRAPPED_LINES`).
    pub lines: u8,
    /// True when the cell exceeded the `MAX_WRAPPED_LINES` visual capacity or `MAX_WRAP_CELL_BYTES`.
    pub truncated: bool,
    /// Number of raw input bytes inspected (always `<= MAX_WRAP_CELL_BYTES`).
    pub inspected_bytes: usize,
    /// Bounded display character budget to pass into text rendering / shaping.
    pub display_char_budget: usize,
}

/// Computes bounded wrapped-line metrics for a single cell without scanning past `max_cell_bytes`.
pub fn measure_wrapped_cell(
    raw_bytes: &[u8],
    col_width_px: f32,
    font_size_px: f32,
    max_lines: usize,
    max_cell_bytes: usize,
) -> WrapCellMetrics {
    let max_lines = max_lines.clamp(1, u8::MAX as usize);
    let prefix_len = utf8_bounded_prefix_len(raw_bytes, max_cell_bytes);
    let byte_clamped = prefix_len < raw_bytes.len();
    let slice = &raw_bytes[..prefix_len];

    // Conservative character-width estimate for proportional UI fonts (~0.56 em) with 16 px horizontal padding.
    let usable_width = (col_width_px - 16.0).max(24.0);
    let char_width = (font_size_px * 0.56).max(4.0);
    let chars_per_line = ((usable_width / char_width).floor() as usize).max(4);
    // Allow slight slack (1.5x lines) so proportional narrow characters still fill the 2nd line before GPUI clamps.
    let display_char_budget = (chars_per_line.saturating_mul(max_lines + 1)).clamp(64, 600);

    if slice.is_empty() {
        return WrapCellMetrics {
            lines: 1,
            truncated: false,
            inspected_bytes: 0,
            display_char_budget,
        };
    }

    let mut lines: usize = 1;
    let mut line_chars: usize = 0;
    let mut word_chars: usize = 0;
    let mut truncated = byte_clamped;
    let mut inspected = 0usize;

    let text = String::from_utf8_lossy(slice);
    for ch in text.chars() {
        inspected = inspected.saturating_add(ch.len_utf8());
        if ch == '\n' {
            if lines >= max_lines {
                truncated = true;
                break;
            }
            lines += 1;
            line_chars = 0;
            word_chars = 0;
            continue;
        }
        if ch == '\r' {
            continue;
        }

        if ch.is_whitespace() {
            word_chars = 0;
            if line_chars < chars_per_line {
                line_chars += 1;
            } else if lines >= max_lines {
                truncated = true;
                break;
            } else {
                lines += 1;
                line_chars = 0;
            }
            continue;
        }

        word_chars += 1;
        line_chars += 1;
        if line_chars > chars_per_line {
            if lines >= max_lines {
                truncated = true;
                break;
            }
            lines += 1;
            // Word-wrap when the current token fits cleanly on the new line; otherwise hard-break.
            if word_chars < chars_per_line {
                line_chars = word_chars;
            } else {
                line_chars = 1;
                word_chars = 1;
            }
        }
    }

    WrapCellMetrics {
        lines: lines.min(max_lines) as u8,
        truncated,
        inspected_bytes: inspected.min(prefix_len),
        display_char_budget,
    }
}

/// Per-frame byte budget guarding total wrapped-text work across one viewport render.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WrapFrameBudget {
    limit_bytes: usize,
    used_bytes: usize,
    clamped_cells: usize,
}

impl WrapFrameBudget {
    pub const fn new(limit_bytes: usize) -> Self {
        Self {
            limit_bytes,
            used_bytes: 0,
            clamped_cells: 0,
        }
    }

    pub const fn default_frame() -> Self {
        Self::new(MAX_WRAP_FRAME_BUDGET_BYTES)
    }

    /// Resets the budget at the start of a new frame.
    pub fn reset(&mut self) {
        self.used_bytes = 0;
        self.clamped_cells = 0;
    }

    /// Attempts to reserve `cell_bytes` (already capped at `MAX_WRAP_CELL_BYTES`).
    /// Returns `true` when wrapped layout is permitted, or `false` when the frame
    /// budget is exhausted and the caller should fall back to compact single-line preview.
    pub fn try_consume(&mut self, raw_cell_len: usize) -> bool {
        let cost = raw_cell_len.clamp(1, MAX_WRAP_CELL_BYTES);
        if self.used_bytes.saturating_add(cost) > self.limit_bytes {
            self.clamped_cells = self.clamped_cells.saturating_add(1);
            return false;
        }
        self.used_bytes = self.used_bytes.saturating_add(cost);
        true
    }

    pub const fn used_bytes(&self) -> usize {
        self.used_bytes
    }

    pub const fn clamped_cells(&self) -> usize {
        self.clamped_cells
    }
}

/// Deterministic FNV-1a fingerprint of visible column IDs and their pixel widths (in tenths of px).
pub fn column_widths_fingerprint(columns: impl IntoIterator<Item = (usize, f32)>) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for (col, width_px) in columns {
        let tenths = (width_px.max(0.0) * 10.0).round() as u32;
        for byte in (col as u64).to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        for byte in tenths.to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    hash
}

/// Cache key identifying one fetched row's wrapped layout metrics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RowWrapCacheKey {
    pub row: u64,
    pub source_generation: u64,
    pub edit_revision: u64,
    pub show_all_whitespace: bool,
    pub font_size_tenths: u16,
    pub width_fingerprint: u64,
}

/// Cached wrapped metrics for one fetched row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RowWrapMetrics {
    pub max_lines: u8,
    pub measured_bytes: usize,
    pub clamped_cells: usize,
}

/// Bounded cache storing wrapped layout metrics strictly for fetched rows.
/// Uses FIFO/ring eviction at `max_entries` so RAM stays O(1) independent of file size.
#[derive(Debug)]
pub struct RowWrapCache {
    max_entries: usize,
    entries: HashMap<RowWrapCacheKey, RowWrapMetrics>,
    order: VecDeque<RowWrapCacheKey>,
    hits: u64,
    misses: u64,
}

impl Default for RowWrapCache {
    fn default() -> Self {
        Self::new(MAX_CACHED_WRAP_ROWS)
    }
}

impl RowWrapCache {
    pub fn new(max_entries: usize) -> Self {
        let cap = max_entries.max(1);
        Self {
            max_entries: cap,
            entries: HashMap::with_capacity(cap),
            order: VecDeque::with_capacity(cap),
            hits: 0,
            misses: 0,
        }
    }

    pub fn get(&mut self, key: &RowWrapCacheKey) -> Option<RowWrapMetrics> {
        if let Some(&metrics) = self.entries.get(key) {
            self.hits = self.hits.saturating_add(1);
            Some(metrics)
        } else {
            self.misses = self.misses.saturating_add(1);
            None
        }
    }

    pub fn insert(&mut self, key: RowWrapCacheKey, metrics: RowWrapMetrics) {
        if let Some(existing) = self.entries.get_mut(&key) {
            *existing = metrics;
            return;
        }
        while self.entries.len() >= self.max_entries {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            } else {
                break;
            }
        }
        self.entries.insert(key, metrics);
        self.order.push_back(key);
    }

    /// Measures and caches a fetched row's cells under `MAX_WRAPPED_LINES` and `MAX_WRAP_CELL_BYTES`.
    pub fn get_or_measure<'a>(
        &mut self,
        key: RowWrapCacheKey,
        font_size_px: f32,
        cells: impl IntoIterator<Item = (&'a [u8], f32)>,
    ) -> RowWrapMetrics {
        if let Some(hit) = self.get(&key) {
            return hit;
        }
        let mut max_lines: u8 = 1;
        let mut measured_bytes = 0usize;
        let mut clamped_cells = 0usize;
        for (raw, width_px) in cells {
            let m = measure_wrapped_cell(
                raw,
                width_px,
                font_size_px,
                MAX_WRAPPED_LINES,
                MAX_WRAP_CELL_BYTES,
            );
            max_lines = max_lines.max(m.lines);
            measured_bytes = measured_bytes.saturating_add(m.inspected_bytes);
            if m.truncated {
                clamped_cells = clamped_cells.saturating_add(1);
            }
        }
        let metrics = RowWrapMetrics {
            max_lines,
            measured_bytes,
            clamped_cells,
        };
        self.insert(key, metrics);
        metrics
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn hits(&self) -> u64 {
        self.hits
    }

    pub fn misses(&self) -> u64 {
        self.misses
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_299mb_active_and_301mb_auto_disabled_transitions() {
        let mut prefs = WordWrapPreferences::load(None);
        assert_eq!(
            resolve_word_wrap(&prefs, None, Some(299_000_000)),
            EffectiveWordWrap::Off
        );

        prefs.set_enabled(true).unwrap();
        assert_eq!(
            resolve_word_wrap(&prefs, None, Some(299_000_000)),
            EffectiveWordWrap::Active {
                manual_override: false
            }
        );
        assert_eq!(
            resolve_word_wrap(&prefs, None, Some(300_000_000)),
            EffectiveWordWrap::Active {
                manual_override: false
            }
        );
        assert_eq!(
            resolve_word_wrap(&prefs, None, Some(301_000_000)),
            EffectiveWordWrap::AutoDisabled {
                file_bytes: 301_000_000,
                threshold_bytes: DEFAULT_WORD_WRAP_AUTO_DISABLE_BYTES,
            }
        );

        // Clicking Wrap on an auto-disabled file enables a per-tab override,
        // and clicking again returns to AutoDisabled without disabling global wrap.
        let override_on = prefs.toggle_for_tab(None, Some(301_000_000));
        assert_eq!(
            override_on.effective,
            EffectiveWordWrap::Active {
                manual_override: true
            }
        );
        assert_eq!(override_on.next_tab_override, Some(true));
        assert!(override_on.status_notice.is_some());
        assert!(prefs.enabled);

        let override_off = prefs.toggle_for_tab(override_on.next_tab_override, Some(301_000_000));
        assert!(override_off.effective.is_auto_disabled());
        assert_eq!(override_off.next_tab_override, None);
        assert!(prefs.enabled);
    }

    #[test]
    fn preferences_round_trip_and_reject_corrupt_or_oversized_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("word-wrap-preferences");

        let mut prefs = WordWrapPreferences::load(Some(path.clone()));
        assert!(!prefs.enabled);
        assert_eq!(
            prefs.auto_disable_bytes,
            DEFAULT_WORD_WRAP_AUTO_DISABLE_BYTES
        );

        prefs.set_enabled(true).unwrap();
        prefs.set_auto_disable_bytes(150_000_000).unwrap();

        let reloaded = WordWrapPreferences::load(Some(path.clone()));
        assert!(reloaded.enabled);
        assert_eq!(reloaded.auto_disable_bytes, 150_000_000);

        for corrupt in [
            "invalid",
            "word_wrap=2\n",
            "word_wrap=1\nunknown=3\n",
            &"x".repeat(512),
        ] {
            std::fs::write(&path, corrupt).unwrap();
            let fallback = WordWrapPreferences::load(Some(path.clone()));
            assert!(!fallback.enabled);
            assert_eq!(
                fallback.auto_disable_bytes,
                DEFAULT_WORD_WRAP_AUTO_DISABLE_BYTES
            );
        }
    }

    #[test]
    fn unix_preferences_path_ignores_relative_env_roots() {
        assert_eq!(
            unix_word_wrap_preferences_path(Some(PathBuf::from("relative")), None),
            None
        );
        assert_eq!(
            unix_word_wrap_preferences_path(
                Some(PathBuf::from("/tmp/xdg")),
                Some(PathBuf::from("/home/user"))
            ),
            Some(PathBuf::from("/tmp/xdg/quarry/word-wrap-preferences"))
        );
    }

    #[test]
    fn measure_wrapped_cell_caps_lines_and_cell_byte_budget() {
        let short = measure_wrapped_cell(
            b"hello",
            140.0,
            13.0,
            MAX_WRAPPED_LINES,
            MAX_WRAP_CELL_BYTES,
        );
        assert_eq!(short.lines, 1);
        assert!(!short.truncated);

        let multiline = measure_wrapped_cell(
            b"line one\nline two\nline three",
            240.0,
            13.0,
            MAX_WRAPPED_LINES,
            MAX_WRAP_CELL_BYTES,
        );
        assert_eq!(multiline.lines, 2);
        assert!(multiline.truncated);

        let huge = vec![b'a'; 2 * 1024 * 1024];
        let bounded =
            measure_wrapped_cell(&huge, 140.0, 13.0, MAX_WRAPPED_LINES, MAX_WRAP_CELL_BYTES);
        assert_eq!(bounded.lines, 2);
        assert!(bounded.truncated);
        assert!(bounded.inspected_bytes <= MAX_WRAP_CELL_BYTES);
    }

    #[test]
    fn frame_budget_caps_hostile_viewport_cells() {
        let mut budget = WrapFrameBudget::new(8 * 1024);
        assert!(budget.try_consume(4 * 1024));
        assert!(budget.try_consume(4 * 1024));
        assert!(!budget.try_consume(4 * 1024));
        assert_eq!(budget.clamped_cells(), 1);
        budget.reset();
        assert_eq!(budget.used_bytes(), 0);
        assert_eq!(budget.clamped_cells(), 0);
    }

    #[test]
    fn row_wrap_cache_bounds_capacity_and_invalidates_on_key_change() {
        let mut cache = RowWrapCache::new(4);
        let fp = column_widths_fingerprint([(0, 120.0), (1, 180.0)]);
        for row in 0..10u64 {
            let key = RowWrapCacheKey {
                row,
                source_generation: 1,
                edit_revision: 0,
                show_all_whitespace: false,
                font_size_tenths: 130,
                width_fingerprint: fp,
            };
            cache.get_or_measure(
                key,
                13.0,
                [
                    (&b"short"[..], 120.0),
                    (&b"longer wrapped text here"[..], 180.0),
                ],
            );
        }
        assert_eq!(cache.len(), 4);
        let latest_key = RowWrapCacheKey {
            row: 9,
            source_generation: 1,
            edit_revision: 0,
            show_all_whitespace: false,
            font_size_tenths: 130,
            width_fingerprint: fp,
        };
        assert!(cache.get(&latest_key).is_some());
        assert_eq!(cache.hits(), 1);

        // Changing width fingerprint produces a fresh cache miss.
        let resized_key = RowWrapCacheKey {
            width_fingerprint: column_widths_fingerprint([(0, 120.0), (1, 240.0)]),
            ..latest_key
        };
        assert!(cache.get(&resized_key).is_none());
    }
}
