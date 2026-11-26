//! Frontend-neutral document, selection, edit history, and workbench state for Quarry.

pub mod bookmarks;
pub mod command;
pub mod compressed;
pub mod document;
pub mod dormancy;
pub mod history;
pub mod resource_hud;
pub mod selection;
pub mod storage_prefs;
pub mod tab;
pub mod theme;
pub mod whitespace;
pub mod word_wrap;
pub mod workflows;

pub use bookmarks::{
    BookmarkKey, BookmarkSplitTarget, BookmarkTransform, MAX_ROW_BOOKMARKS, RowBookmark,
    RowBookmarks,
};
pub use command::{
    CloseCommandOutcome, CommandContext, CommandId, CommandResolution, CommandTarget, DocumentId,
    FileCommands, WindowId, evaluate_command,
};
pub use compressed::CompressedDormancyPolicy;
pub use dormancy::{
    DEFAULT_MAX_DIRTY_OVERLAY_BYTES, DEFAULT_MAX_HYDRATED_TABS, DEFAULT_NOFILE_TARGET,
    ExportCompletion, FdBudgetLedger, FdBudgetSnapshot, FdCategory, FdLease, LazyDocumentStub,
    MAX_PROCESS_FD_BUDGET, MIN_DORMANT_SCRATCH_HEADROOM_BYTES, PinReason, ResidencyState,
    SaveCompletion, SchedulerError, SearchCompletion, SectionVerifyCompletion,
    StructuralCompletion, WakeKind, WakeOutcome, WorkbenchScheduler, WorkbenchSchedulerConfig,
    document_overlay_bytes,
};
pub use storage_prefs::{
    ADAPTIVE_MEMORY_BUDGET_BYTES, StoragePreferences, clear_index_cache_summary,
    inspect_temporary_storage,
};
pub use workflows::{
    CombineWorkflow, InspectWorkflow, JoinOutputColumnDraft, JoinSideSource, JoinWorkflow,
};

pub use document::{
    ActiveFilterRead, CapturedDocument, CellEdit, Document, DocumentLease, DocumentProbe,
    FilterReadWindow, HeaderEdit, MaterializationPreparation, MaterializedWorkingCopy,
    StructuralJob, StructuralProgressDisplay, ViewportReadPlan, footer_range_text,
    materialization_progress, sort_progress,
};
pub use history::{
    EditHistory, EditTarget, MAX_EDIT_HISTORY_BYTES, MAX_EDIT_HISTORY_ENTRIES, SharedCellEdits,
    StructuralOverlay, ValueEdit, WorkingCopySnapshot, WorkingCopyState,
};
pub use resource_hud::{
    DEFAULT_HUD_SAMPLE_INTERVAL, HudAnnouncement, HudAnnouncementTracker, HudIndexPhase,
    LOW_DISK_WARNING_BYTES, ResourceHudSample, ResourceHudSampler, ResourceHudTargets,
    VolumeSpaceSample, sample_resource_hud,
};
pub use selection::{
    CellCoord, ColumnView, CursorSelectionMode, GridCursor, GridSelection, RowSelection,
    SelectionModifiers, select_column, selected_split_column, validated_column_selection,
};
pub use tab::{
    DelimiterMode, FilterRuleDraft, TabCloseState, WorkbenchTab, filter_predicate_from_rule,
    filter_query_from_rules,
};
pub use theme::{
    ACCENT, CONTROL_RADIUS, DARK, GRID_FONT_SIZE, LIGHT, MAX_OMARCHY_THEME_BYTES, Palette,
    ResolvedTheme, Rgba, ThemeAppearance, ThemePreference, ThemePreferences, WINDOW_RADIUS,
    contrast_ratio, load_omarchy_theme_from_home, looks_numeric, omarchy_allowed_roots,
    omarchy_theme_fingerprint, palette_meets_wcag_aa, relative_luminance, resolve_theme,
    resolve_theme_for_current_user, source_looks_numeric,
};
pub use word_wrap::{
    DEFAULT_WORD_WRAP_AUTO_DISABLE_BYTES, EffectiveWordWrap, MAX_CACHED_WRAP_ROWS,
    MAX_CONFIGURABLE_AUTO_DISABLE_MB, MAX_WRAP_CELL_BYTES, MAX_WRAP_FRAME_BUDGET_BYTES,
    MAX_WRAPPED_LINES, MIN_CONFIGURABLE_AUTO_DISABLE_MB, RowWrapCache, RowWrapCacheKey,
    RowWrapMetrics, SINGLE_LINE_ROW_HEIGHT_PX, WRAPPED_LINE_HEIGHT_PX, WRAPPED_ROW_HEIGHT_PX,
    WordWrapPreferences, WordWrapToggleOutcome, WrapCellMetrics, WrapFrameBudget,
    column_widths_fingerprint, format_decimal_mb, measure_wrapped_cell, resolve_word_wrap,
    unix_word_wrap_preferences_path, utf8_bounded_prefix_len,
};

#[cfg(any(test, feature = "test-hooks"))]
pub use document::{owned_edit_snapshot_bytes, reset_owned_edit_snapshot_bytes};

use quarry_core::{
    CaseSensitivity, CleanupOperation, ColumnCleanup, DateFormatSuggestion, DuplicateTarget,
    Encoding, OpenOptions, QuarryError, Session, SortDirection, SortMode, SortPlan, SortSpec,
};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const BOOTSTRAP_ROWS: usize = 40;
pub const OVERSCAN_ROWS: usize = 16;
pub const MAX_GRID_TEXT_CHARS: usize = 16 * 1024;
pub const MAX_COPY_BYTES: usize = 64 * 1024 * 1024;
pub const POLL_INTERVAL: Duration = Duration::from_millis(100);
pub const SOURCE_CHANGED_NOTICE: &str =
    "The source file changed outside Quarry. Discard changes and reopen it.";
// Small operations still receive the engine's space check, without another dialog.
pub const STORAGE_REVIEW_BYTES: u64 = 256 * 1024 * 1024;
pub const JOIN_INPUT_RESERVED: &str =
    "This document is being used by Join Files. Reset or close the join before changing it.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageSeverity {
    Error,
    Warning,
    Status,
}

/// The attempted format travels with an open failure even when another tab becomes active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedOpen {
    pub path: PathBuf,
    pub options: OpenOptions,
    pub encoding: Option<Encoding>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppMessage {
    pub inspection_target: Option<Box<FailedOpen>>,
    pub diagnostic: Option<Box<quarry_core::Diagnostic>>,
    pub text: String,
    pub severity: MessageSeverity,
}

/// Names the path before an open failure unless the message already starts with it, as
/// compressed-file errors do.
pub fn open_failure_line(path: &Path, text: &str) -> String {
    let name = path.display().to_string();
    if text.starts_with(&name) {
        text.to_owned()
    } else {
        format!("{name}: {text}")
    }
}

impl AppMessage {
    /// Retains the failed path, attempted format and cause independently of the active tab.
    pub fn file_open_error(
        path: PathBuf,
        options: OpenOptions,
        encoding: Option<Encoding>,
        message: String,
    ) -> Self {
        let context = FailedOpen {
            path,
            options,
            encoding,
            message: message.clone(),
        };
        let mut result = Self::error(message);
        result.inspection_target = Some(Box::new(context));
        result
    }

    /// Attaches structured data locations without losing the original operation message.
    pub fn core_error(error: impl Into<QuarryError>) -> Self {
        let error = error.into();
        let mut result = Self::error(error.to_string());
        result.diagnostic = Some(Box::new(quarry_core::Diagnostic::from_error(&error)));
        result
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self::new(text, MessageSeverity::Error)
    }

    pub fn warning(text: impl Into<String>) -> Self {
        Self::new(text, MessageSeverity::Warning)
    }

    pub fn status(text: impl Into<String>) -> Self {
        Self::new(text, MessageSeverity::Status)
    }

    pub fn new(text: impl Into<String>, severity: MessageSeverity) -> Self {
        Self {
            inspection_target: None,
            diagnostic: None,
            text: text.into(),
            severity,
        }
    }
}

impl std::ops::Deref for AppMessage {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.text
    }
}

impl std::fmt::Display for AppMessage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.text)
    }
}

impl PartialEq<&str> for AppMessage {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructuralRequest {
    CleanupTrim,
    CleanupCollapseSpaces,
    CleanupUppercase,
    CleanupLowercase,
    CleanupFill,
    InsertRows,
    InsertColumns,
    Split,
    Combine,
    Move,
    Sort,
    Duplicates,
}

impl StructuralRequest {
    /// Identifies operations using the selected-column cleanup workflow.
    pub fn is_cleanup(self) -> bool {
        matches!(
            self,
            Self::CleanupTrim
                | Self::CleanupCollapseSpaces
                | Self::CleanupUppercase
                | Self::CleanupLowercase
                | Self::CleanupFill
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuralDialog {
    pub request: StructuralRequest,
    pub columns: Vec<usize>,
    pub separator: String,
    pub count: String,
    pub position: String,
    pub sort_direction: SortDirection,
    pub sort_mode: SortMode,
    pub sort_levels: Vec<SortSpec>,
    pub match_case: bool,
    pub duplicate_all_fields: bool,
    pub date_format_suggestions: BTreeMap<usize, Option<DateFormatSuggestion>>,
}

impl StructuralDialog {
    pub fn cleanup_columns(columns: Vec<usize>) -> Self {
        Self {
            request: StructuralRequest::CleanupTrim,
            columns,
            ..Self::sort(0)
        }
    }

    /// Builds the selected cleanup operation for the physical file columns.
    pub fn cleanup(&self) -> ColumnCleanup {
        ColumnCleanup {
            columns: self.columns.clone(),
            operation: match self.request {
                StructuralRequest::CleanupTrim => CleanupOperation::Trim,
                StructuralRequest::CleanupCollapseSpaces => CleanupOperation::CollapseSpaces,
                StructuralRequest::CleanupUppercase => CleanupOperation::Uppercase,
                StructuralRequest::CleanupLowercase => CleanupOperation::Lowercase,
                StructuralRequest::CleanupFill => {
                    CleanupOperation::FillEmpty(self.separator.as_bytes().to_vec())
                }
                _ => unreachable!("not a cleanup dialog"),
            },
        }
    }

    pub fn insert_rows(position: u64) -> Self {
        Self {
            request: StructuralRequest::InsertRows,
            columns: Vec::new(),
            position: position.saturating_add(1).to_string(),
            ..Self::sort(0)
        }
    }

    pub fn insert_columns(position: usize) -> Self {
        Self {
            request: StructuralRequest::InsertColumns,
            columns: Vec::new(),
            position: position.saturating_add(1).to_string(),
            ..Self::sort(0)
        }
    }

    /// Open split settings for one physical source column.
    pub fn split(column: usize) -> Self {
        Self {
            request: StructuralRequest::Split,
            columns: vec![column],
            separator: String::new(),
            count: "1".into(),
            position: String::new(),
            sort_direction: SortDirection::Ascending,
            sort_mode: SortMode::Text,
            sort_levels: Vec::new(),
            match_case: false,
            duplicate_all_fields: false,
            date_format_suggestions: BTreeMap::new(),
        }
    }

    /// Open combine settings while retaining the selected column order.
    pub fn combine(columns: Vec<usize>) -> Self {
        Self {
            request: StructuralRequest::Combine,
            columns,
            separator: String::new(),
            count: "1".into(),
            position: String::new(),
            sort_direction: SortDirection::Ascending,
            sort_mode: SortMode::Text,
            sort_levels: Vec::new(),
            match_case: false,
            duplicate_all_fields: false,
            date_format_suggestions: BTreeMap::new(),
        }
    }

    /// Start with the current first selected column as the destination position.
    pub fn move_columns(columns: Vec<usize>) -> Self {
        let position = columns
            .first()
            .map_or_else(String::new, |column| column.saturating_add(1).to_string());
        Self {
            request: StructuralRequest::Move,
            columns,
            separator: String::new(),
            count: "1".into(),
            position,
            sort_direction: SortDirection::Ascending,
            sort_mode: SortMode::Text,
            sort_levels: Vec::new(),
            match_case: false,
            duplicate_all_fields: false,
            date_format_suggestions: BTreeMap::new(),
        }
    }

    /// Start a text sort using the clicked physical column as the primary level.
    pub fn sort(column: usize) -> Self {
        Self {
            request: StructuralRequest::Sort,
            columns: vec![column],
            separator: String::new(),
            count: "1".into(),
            position: String::new(),
            sort_direction: SortDirection::Ascending,
            sort_mode: SortMode::Text,
            sort_levels: Vec::new(),
            match_case: false,
            duplicate_all_fields: false,
            date_format_suggestions: BTreeMap::new(),
        }
    }

    /// Combine the primary level and tie-breakers without copying document edits.
    pub fn sort_plan(&self, match_case: bool) -> SortPlan {
        let mut keys = vec![SortSpec {
            column: self.columns[0],
            mode: self.sort_mode,
            direction: self.sort_direction,
            case_sensitivity: case_sensitivity(match_case),
        }];
        if self.sort_mode.uses_column() {
            keys.extend_from_slice(&self.sort_levels);
        } else {
            keys[0].direction = SortDirection::Ascending;
        }
        SortPlan { keys }
    }

    /// Apply a level reorder, including the primary level's remembered case option.
    pub fn set_sort_keys(&mut self, keys: Vec<SortSpec>, match_case: &mut bool) {
        let first = keys[0];
        self.columns[0] = first.column;
        self.sort_mode = first.mode;
        self.sort_direction = first.direction;
        *match_case = first.case_sensitivity == CaseSensitivity::Sensitive;
        self.sort_levels = keys[1..].to_vec();
    }

    pub fn duplicates(columns: Vec<usize>) -> Self {
        let duplicate_all_fields = columns.is_empty();
        Self {
            request: StructuralRequest::Duplicates,
            columns,
            duplicate_all_fields,
            ..Self::sort(0)
        }
    }

    pub fn duplicate_target(&self) -> DuplicateTarget {
        if self.duplicate_all_fields {
            DuplicateTarget::AllFields
        } else {
            DuplicateTarget::Columns(self.columns.clone())
        }
    }
}

pub fn case_sensitivity(match_case: bool) -> CaseSensitivity {
    if match_case {
        CaseSensitivity::Sensitive
    } else {
        CaseSensitivity::Insensitive
    }
}

pub fn format_unavailable_scratch_notice(directory: &Path, error: &str) -> String {
    format!(
        "Saved temporary folder {} is unavailable ({error}). Open File > Temporary storage… to choose another folder or switch back to the system temporary folder.",
        directory.display()
    )
}

pub fn progress_fraction(bytes_scanned: u64, total_bytes: u64, done: bool) -> f32 {
    if total_bytes == 0 {
        if done { 1.0 } else { 0.0 }
    } else {
        (bytes_scanned as f32 / total_bytes as f32).clamp(0.0, 1.0)
    }
}

/// Formats counts with thousands separators: 50000 → "50,000".
pub fn group_digits(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.2} {}", UNITS[unit])
}

pub fn field_text(field: &[u8]) -> String {
    field_preview(field, 120)
}

pub fn grid_field_text(field: &[u8]) -> String {
    field_preview(field, MAX_GRID_TEXT_CHARS)
}

pub fn field_preview(field: &[u8], max_chars: usize) -> String {
    // Decode only enough bytes for the preview plus one character of lookahead.
    // A partial UTF-8 sequence at this byte boundary is beyond the retained text.
    let prefix = &field[..field.len().min(4 * (max_chars + 1))];
    let decoded = String::from_utf8_lossy(prefix);
    let mut characters = decoded
        .chars()
        .flat_map(|ch| match ch {
            '\n' => [Some('\\'), Some('n')],
            '\r' => [Some('\\'), Some('r')],
            _ => [Some(ch), None],
        })
        .flatten();
    let mut rendered: String = characters.by_ref().take(max_chars).collect();
    if characters.next().is_some() {
        if max_chars < 3 {
            return ".".repeat(max_chars);
        }
        let end = rendered.char_indices().nth(max_chars - 3).unwrap().0;
        rendered.truncate(end);
        rendered.push_str("...");
    }
    rendered
}

pub fn selection_fields_text(
    fields: &[Vec<u8>],
    selection: GridSelection,
    max_bytes: usize,
) -> Result<String, String> {
    match selection {
        GridSelection::Cell { column, .. } => {
            let mut output = String::new();
            append_clipboard_field(
                &mut output,
                fields.get(column).map_or(&[], |value| value.as_slice()),
                false,
                max_bytes,
            )?;
            Ok(output)
        }
        GridSelection::Row { .. } => {
            let mut output = String::new();
            for (index, field) in fields.iter().enumerate() {
                if index > 0 {
                    ensure_copy_capacity(&output, 1, max_bytes)?;
                    output.push('\t');
                }
                append_clipboard_field(&mut output, field, true, max_bytes)?;
            }
            Ok(output)
        }
    }
}

fn append_clipboard_field(
    output: &mut String,
    field: &[u8],
    quote_for_tsv: bool,
    max_bytes: usize,
) -> Result<(), String> {
    let text = std::str::from_utf8(field)
        .map_err(|_| "The selected data is not valid UTF-8 and cannot be copied yet.".to_owned())?;
    let needs_quotes = quote_for_tsv
        && text
            .bytes()
            .any(|byte| matches!(byte, b'\t' | b'\r' | b'\n' | b'"'));
    let added = text.len().saturating_add(if needs_quotes {
        2 + text.bytes().filter(|byte| *byte == b'"').count()
    } else {
        0
    });
    ensure_copy_capacity(output, added, max_bytes)?;

    if needs_quotes {
        output.push('"');
        for character in text.chars() {
            if character == '"' {
                output.push('"');
            }
            output.push(character);
        }
        output.push('"');
    } else {
        output.push_str(text);
    }
    Ok(())
}

fn ensure_copy_capacity(output: &str, added: usize, max_bytes: usize) -> Result<(), String> {
    if output.len().saturating_add(added) > max_bytes {
        Err(format!(
            "Copy exceeds the {} clipboard limit.",
            format_bytes(max_bytes as u64)
        ))
    } else {
        Ok(())
    }
}

pub fn max_viewport_start(total_rows: u64, visible_rows: usize) -> u64 {
    total_rows.saturating_sub(visible_rows as u64)
}

pub fn logical_viewport_start(
    requested: u64,
    data_start: u64,
    total_rows: u64,
    visible_rows: usize,
) -> u64 {
    data_start.saturating_add(
        requested
            .saturating_sub(data_start)
            .min(max_viewport_start(total_rows, visible_rows)),
    )
}

pub fn scroll_fraction_for_row(
    row: u64,
    data_start: u64,
    total_rows: u64,
    visible_rows: usize,
) -> f64 {
    let maximum = max_viewport_start(total_rows, visible_rows);
    if maximum == 0 {
        0.0
    } else {
        row.saturating_sub(data_start).min(maximum) as f64 / maximum as f64
    }
}

pub fn row_for_scroll_fraction(
    fraction: f64,
    data_start: u64,
    total_rows: u64,
    visible_rows: usize,
) -> u64 {
    let maximum = max_viewport_start(total_rows, visible_rows);
    let relative = (fraction.clamp(0.0, 1.0) * maximum as f64).round() as u64;
    data_start.saturating_add(relative.min(maximum))
}

pub fn parse_data_row(value: &str, data_start: u64) -> Result<u64, String> {
    let row: u64 = value
        .trim()
        .parse()
        .map_err(|_| "Data row must be a positive whole number.".to_owned())?;
    if row == 0 {
        return Err("Data rows start at 1.".into());
    }
    Ok(data_start.saturating_add(row - 1))
}

pub fn parse_file_column(value: &str, total_columns: usize) -> Result<usize, String> {
    let column: usize = value
        .trim()
        .parse()
        .map_err(|_| "File column must be a positive whole number.".to_owned())?;
    if column == 0 {
        return Err("File columns start at 1.".into());
    }
    if column > total_columns {
        return Err(if total_columns == 0 {
            "This file has no columns.".into()
        } else {
            format!("File column must be between 1 and {total_columns}.")
        });
    }
    Ok(column - 1)
}

pub fn parse_move_position(
    value: &str,
    total_columns: usize,
    selected_columns: usize,
) -> Result<usize, String> {
    if selected_columns == 0 {
        return Err("Select at least one numbered column first.".into());
    }
    if selected_columns > total_columns {
        return Err("A selected column is outside this file.".into());
    }
    let position: usize = value
        .trim()
        .parse()
        .map_err(|_| "Destination position must be a positive whole number.".to_owned())?;
    if position == 0 {
        return Err("Destination positions start at 1.".into());
    }
    let maximum = total_columns - selected_columns + 1;
    if position > maximum {
        return Err(format!(
            "Destination position must be between 1 and {maximum}."
        ));
    }
    Ok(position - 1)
}

pub fn parse_insertion_count(input: &str) -> Result<u64, String> {
    input
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|count| *count > 0)
        .ok_or_else(|| "Enter a positive whole-number count.".into())
}

pub fn parse_insertion_position(input: &str, size: u64) -> Result<u64, String> {
    let last = size
        .checked_add(1)
        .ok_or("This document is too large to insert into.")?;
    input
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|position| (1..=last).contains(position))
        .map(|position| position - 1)
        .ok_or_else(|| format!("Enter a position between 1 and {last}."))
}

pub fn literal_contains(value: &[u8], query: &[u8], case_sensitivity: CaseSensitivity) -> bool {
    !query.is_empty()
        && value
            .windows(query.len())
            .any(|part| literal_equals(part, query, case_sensitivity))
}

pub fn literal_equals(left: &[u8], right: &[u8], case_sensitivity: CaseSensitivity) -> bool {
    match case_sensitivity {
        CaseSensitivity::Insensitive => left.eq_ignore_ascii_case(right),
        CaseSensitivity::Sensitive => left == right,
    }
}

pub fn replace_literal_all_with_case(
    value: &[u8],
    query: &[u8],
    replacement: &[u8],
    case_sensitivity: CaseSensitivity,
) -> Option<Vec<u8>> {
    if query.is_empty() {
        return None;
    }
    let mut start = 0;
    let mut output = Vec::with_capacity(value.len());
    let mut replaced = false;
    while let Some(relative) = value[start..]
        .windows(query.len())
        .position(|part| literal_equals(part, query, case_sensitivity))
    {
        let found = start + relative;
        output.extend_from_slice(&value[start..found]);
        output.extend_from_slice(replacement);
        start = found + query.len();
        replaced = true;
    }
    if !replaced {
        return None;
    }
    output.extend_from_slice(&value[start..]);
    Some(output)
}

pub fn headers_for(session: &Session, columns: &[usize]) -> Vec<String> {
    columns
        .iter()
        .map(|column| column_name(session, *column))
        .collect()
}

pub fn column_name(session: &Session, column: usize) -> String {
    if session.dialect.has_header
        && let Some(field) = session
            .section_header_row()
            .or_else(|| session.first_rows.first())
            .and_then(|row| row.fields.get(column))
    {
        return field_text(field);
    }
    format!("Column {}", column + 1)
}

/// Describe direction in the selected mode's units, including oldest/newest dates.
pub fn sort_direction_label(mode: SortMode, direction: SortDirection) -> &'static str {
    match (mode, direction) {
        (SortMode::DateTime { .. }, SortDirection::Ascending) => "Oldest first",
        (SortMode::DateTime { .. }, SortDirection::Descending) => "Newest first",
        (SortMode::CharacterCount, SortDirection::Ascending) => "Shortest first",
        (SortMode::CharacterCount, SortDirection::Descending) => "Longest first",
        (SortMode::WordCount, SortDirection::Ascending) => "Fewest first",
        (SortMode::WordCount, SortDirection::Descending) => "Most first",
        (_, SortDirection::Ascending) => "Ascending",
        (_, SortDirection::Descending) => "Descending",
    }
}

#[cfg(test)]
mod tests {
    use super::field_preview;

    #[test]
    fn field_preview_handles_short_limits_without_underflow() {
        assert_eq!(field_preview(b"abcd", 0), "");
        assert_eq!(field_preview(b"abcd", 1), ".");
        assert_eq!(field_preview(b"abcd", 2), "..");
        assert_eq!(field_preview(b"abcd", 3), "...");
        assert_eq!(field_preview(b"abcd", 4), "abcd");
        assert_eq!(field_preview(b"abcde", 4), "a...");
        assert_eq!(field_preview(b"ab", 2), "ab");
    }
}
