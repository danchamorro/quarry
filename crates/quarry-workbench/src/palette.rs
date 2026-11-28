//! Frontend-neutral fuzzy Command Palette catalog, ranking, and `#`/Go-to row & column resolver.

use std::collections::HashMap;

use crate::CommandId;

/// Maximum query characters processed by fuzzy ranking and jump resolution.
pub const MAX_PALETTE_QUERY_CHARS: usize = 128;

/// Static descriptor for a Command Palette entry routed through [`CommandId`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaletteCommand {
    pub id: CommandId,
    pub label: &'static str,
    pub category: &'static str,
    pub shortcut_macos: Option<&'static str>,
    pub shortcut_other: Option<&'static str>,
    pub keywords: &'static [&'static str],
}

impl PaletteCommand {
    /// Returns the display shortcut hint for the active platform.
    pub const fn shortcut(self, is_macos: bool) -> Option<&'static str> {
        if is_macos {
            self.shortcut_macos
        } else {
            self.shortcut_other
        }
    }
}

/// Ranked Command Palette match returned by [`rank_palette_commands`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PaletteMatch {
    pub command: PaletteCommand,
    pub score: i32,
}

/// Parsed intent for the Command Palette input box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteQuery<'a> {
    /// Ordinary fuzzy command search.
    Command(&'a str),
    /// `#`-prefixed column/row jump query (with the leading `#` stripped).
    Jump(&'a str),
}

/// Resolved target for `#` column jump or `Cmd/Ctrl+L` Go to Row / Column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JumpTarget {
    /// Jump to a 0-based physical source column (auto-showing if hidden and horizontally centering).
    Column { source_col: usize },
    /// Jump to a 1-based visible display row (filtered row when a filter is active).
    Row { display_row_1based: u64 },
}

/// Ranked jump candidate displayed in the palette / Go to Row or Column overlay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JumpMatch {
    pub target: JumpTarget,
    pub title: String,
    pub detail: String,
    pub hidden: bool,
    pub duplicate_header: bool,
    pub score: i32,
}

const PALETTE_COMMANDS: &[PaletteCommand] = &[
    PaletteCommand {
        id: CommandId::GoToRowOrColumn,
        label: "Go to Row or Column…",
        category: "Navigate",
        shortcut_macos: Some("⌘L"),
        shortcut_other: Some("Ctrl+L"),
        keywords: &["jump", "column", "row", "line", "#"],
    },
    PaletteCommand {
        id: CommandId::Find,
        label: "Find…",
        category: "Navigate",
        shortcut_macos: Some("⌘F"),
        shortcut_other: Some("Ctrl+F"),
        keywords: &["search", "text"],
    },
    PaletteCommand {
        id: CommandId::FindNext,
        label: "Find Next",
        category: "Navigate",
        shortcut_macos: Some("⌘G"),
        shortcut_other: Some("Ctrl+G"),
        keywords: &["search", "next", "match"],
    },
    PaletteCommand {
        id: CommandId::FindPrevious,
        label: "Find Previous",
        category: "Navigate",
        shortcut_macos: Some("⇧⌘G"),
        shortcut_other: Some("Ctrl+Shift+G"),
        keywords: &["search", "previous", "back", "match"],
    },
    PaletteCommand {
        id: CommandId::Replace,
        label: "Find and Replace…",
        category: "Edit",
        shortcut_macos: Some("⌥⌘F"),
        shortcut_other: Some("Ctrl+H"),
        keywords: &["search", "substitute"],
    },
    PaletteCommand {
        id: CommandId::ToggleQueryBar,
        label: "Toggle Filter Query Bar",
        category: "Navigate",
        shortcut_macos: Some("⇧⌘F /"),
        shortcut_other: Some("Ctrl+Shift+F /"),
        keywords: &["quick filter", "where", "expression", "query"],
    },
    PaletteCommand {
        id: CommandId::Filters,
        label: "Open Filter Rules…",
        category: "Data",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["filter", "rules", "where"],
    },
    PaletteCommand {
        id: CommandId::Columns,
        label: "Show / Hide Columns…",
        category: "View",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["columns", "hide", "show", "visibility"],
    },
    PaletteCommand {
        id: CommandId::ToggleVimMotions,
        label: "Toggle Vim Motions",
        category: "View",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["vim", "hjkl", "keyboard", "navigation", "motions"],
    },
    PaletteCommand {
        id: CommandId::ToggleWordWrap,
        label: "Toggle Word Wrap",
        category: "View",
        shortcut_macos: Some("⌥Z"),
        shortcut_other: Some("Alt+Z"),
        keywords: &["wrap", "lines", "multiline"],
    },
    PaletteCommand {
        id: CommandId::ShowAllWhitespace,
        label: "Toggle Show All Whitespace",
        category: "View",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["whitespace", "spaces", "tabs", "invisible"],
    },
    PaletteCommand {
        id: CommandId::ToggleBookmark,
        label: "Toggle Bookmark on Row",
        category: "Navigate",
        shortcut_macos: Some("⌘B"),
        shortcut_other: Some("Ctrl+B"),
        keywords: &["bookmark", "mark", "pin"],
    },
    PaletteCommand {
        id: CommandId::NextBookmark,
        label: "Next Bookmark",
        category: "Navigate",
        shortcut_macos: Some("⌘]"),
        shortcut_other: Some("Ctrl+]"),
        keywords: &["bookmark", "jump"],
    },
    PaletteCommand {
        id: CommandId::PreviousBookmark,
        label: "Previous Bookmark",
        category: "Navigate",
        shortcut_macos: Some("⌘["),
        shortcut_other: Some("Ctrl+["),
        keywords: &["bookmark", "jump"],
    },
    PaletteCommand {
        id: CommandId::ClearBookmarks,
        label: "Clear All Bookmarks",
        category: "Navigate",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["bookmark", "remove"],
    },
    PaletteCommand {
        id: CommandId::ToggleBookmarksPanel,
        label: "Open Bookmarks Panel…",
        category: "View",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["bookmarks", "list"],
    },
    PaletteCommand {
        id: CommandId::EditCell,
        label: "Edit Selected Cell…",
        category: "Edit",
        shortcut_macos: Some("F2"),
        shortcut_other: Some("F2"),
        keywords: &["cell", "modify", "value"],
    },
    PaletteCommand {
        id: CommandId::RenameHeader,
        label: "Rename Selected Column Header…",
        category: "Edit",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["header", "column", "rename"],
    },
    PaletteCommand {
        id: CommandId::Copy,
        label: "Copy Selection",
        category: "Edit",
        shortcut_macos: Some("⌘C"),
        shortcut_other: Some("Ctrl+C"),
        keywords: &["clipboard", "cell", "row"],
    },
    PaletteCommand {
        id: CommandId::Undo,
        label: "Undo",
        category: "Edit",
        shortcut_macos: Some("⌘Z"),
        shortcut_other: Some("Ctrl+Z"),
        keywords: &["revert", "history"],
    },
    PaletteCommand {
        id: CommandId::Redo,
        label: "Redo",
        category: "Edit",
        shortcut_macos: Some("⇧⌘Z"),
        shortcut_other: Some("Ctrl+Shift+Z"),
        keywords: &["history"],
    },
    PaletteCommand {
        id: CommandId::SortRows,
        label: "Sort Rows…",
        category: "Data",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["order", "ascending", "descending"],
    },
    PaletteCommand {
        id: CommandId::FindDuplicates,
        label: "Find & Remove Duplicates…",
        category: "Data",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["dedupe", "unique", "duplicate"],
    },
    PaletteCommand {
        id: CommandId::CleanupColumns,
        label: "Clean Selected Columns…",
        category: "Data",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["trim", "normalize", "cleanup"],
    },
    PaletteCommand {
        id: CommandId::InsertRows,
        label: "Insert Rows…",
        category: "Data",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["add", "row"],
    },
    PaletteCommand {
        id: CommandId::InsertColumns,
        label: "Insert Columns…",
        category: "Data",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["add", "column"],
    },
    PaletteCommand {
        id: CommandId::DeleteSelectedRows,
        label: "Delete Selected Rows",
        category: "Data",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["remove", "rows"],
    },
    PaletteCommand {
        id: CommandId::DeleteSelectedColumns,
        label: "Delete Selected Columns",
        category: "Data",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["remove", "columns"],
    },
    PaletteCommand {
        id: CommandId::MoveColumns,
        label: "Move Selected Columns…",
        category: "Data",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["reorder", "columns"],
    },
    PaletteCommand {
        id: CommandId::SplitColumn,
        label: "Split Selected Column…",
        category: "Data",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["split", "delimiter", "column"],
    },
    PaletteCommand {
        id: CommandId::CombineColumns,
        label: "Combine Selected Columns…",
        category: "Data",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["merge", "concat", "columns"],
    },
    PaletteCommand {
        id: CommandId::ToggleLiveWatch,
        label: "Toggle Live Watch",
        category: "Data",
        shortcut_macos: Some("⇧⌘L"),
        shortcut_other: Some("Ctrl+Shift+L"),
        keywords: &["tail", "follow", "stream", "log"],
    },
    PaletteCommand {
        id: CommandId::PauseResumeIndexing,
        label: "Pause / Resume Indexing",
        category: "Data",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["index", "scan"],
    },
    PaletteCommand {
        id: CommandId::VerifySectionBoundary,
        label: "Verify Section Boundary",
        category: "Data",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["section", "boundary", "splice"],
    },
    PaletteCommand {
        id: CommandId::Open,
        label: "Open File…",
        category: "File",
        shortcut_macos: Some("⌘O"),
        shortcut_other: Some("Ctrl+O"),
        keywords: &["csv", "tsv", "load"],
    },
    PaletteCommand {
        id: CommandId::OpenSection,
        label: "Open Section…",
        category: "File",
        shortcut_macos: Some("⇧⌘O"),
        shortcut_other: Some("Ctrl+Shift+O"),
        keywords: &["byte range", "slice", "large"],
    },
    PaletteCommand {
        id: CommandId::Save,
        label: "Save",
        category: "File",
        shortcut_macos: Some("⌘S"),
        shortcut_other: Some("Ctrl+S"),
        keywords: &["write", "commit"],
    },
    PaletteCommand {
        id: CommandId::SaveAs,
        label: "Save As…",
        category: "File",
        shortcut_macos: Some("⇧⌘S"),
        shortcut_other: Some("Ctrl+Shift+S"),
        keywords: &["export", "copy"],
    },
    PaletteCommand {
        id: CommandId::SaveAsUtf8,
        label: "Save As UTF-8…",
        category: "File",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["encoding", "utf8", "convert"],
    },
    PaletteCommand {
        id: CommandId::ExportFiltered,
        label: "Export Filtered Rows…",
        category: "File",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["export", "filter", "csv"],
    },
    PaletteCommand {
        id: CommandId::ReloadFromDisk,
        label: "Reload from Disk",
        category: "File",
        shortcut_macos: Some("⌘R"),
        shortcut_other: Some("Ctrl+R"),
        keywords: &["refresh", "reopen"],
    },
    PaletteCommand {
        id: CommandId::DiscardChanges,
        label: "Discard Changes",
        category: "File",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["revert", "reset"],
    },
    PaletteCommand {
        id: CommandId::CombineFiles,
        label: "Combine Files…",
        category: "File",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["append", "concat", "stack"],
    },
    PaletteCommand {
        id: CommandId::JoinFiles,
        label: "Join Files…",
        category: "File",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["merge", "lookup", "vlookup"],
    },
    PaletteCommand {
        id: CommandId::InspectFile,
        label: "Inspect File & Recovery…",
        category: "File",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["diagnostics", "malformed", "encoding"],
    },
    PaletteCommand {
        id: CommandId::TemporaryStorage,
        label: "Temporary Storage…",
        category: "File",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["cache", "scratch", "disk"],
    },
    PaletteCommand {
        id: CommandId::ThemeSystem,
        label: "Theme: System",
        category: "View",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["appearance", "auto"],
    },
    PaletteCommand {
        id: CommandId::ThemeLight,
        label: "Theme: Light",
        category: "View",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["appearance"],
    },
    PaletteCommand {
        id: CommandId::ThemeDark,
        label: "Theme: Dark",
        category: "View",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["appearance"],
    },
    PaletteCommand {
        id: CommandId::ThemeOmarchy,
        label: "Theme: Omarchy",
        category: "View",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["appearance", "linux"],
    },
    PaletteCommand {
        id: CommandId::NewWindow,
        label: "New Window",
        category: "Window",
        shortcut_macos: Some("⌘N"),
        shortcut_other: Some("Ctrl+N"),
        keywords: &["window"],
    },
    PaletteCommand {
        id: CommandId::CloseTab,
        label: "Close Tab",
        category: "Window",
        shortcut_macos: Some("⌘W"),
        shortcut_other: Some("Ctrl+W"),
        keywords: &["tab"],
    },
    PaletteCommand {
        id: CommandId::MoveTabToNewWindow,
        label: "Move Tab to New Window",
        category: "Window",
        shortcut_macos: None,
        shortcut_other: None,
        keywords: &["detach", "window", "tab"],
    },
    PaletteCommand {
        id: CommandId::NextTab,
        label: "Next Tab",
        category: "Window",
        shortcut_macos: Some("⇧⌘]"),
        shortcut_other: Some("Ctrl+Tab"),
        keywords: &["switch", "tab"],
    },
    PaletteCommand {
        id: CommandId::PreviousTab,
        label: "Previous Tab",
        category: "Window",
        shortcut_macos: Some("⇧⌘["),
        shortcut_other: Some("Ctrl+Shift+Tab"),
        keywords: &["switch", "tab"],
    },
];

/// Returns the static Command Palette catalog.
pub const fn palette_commands() -> &'static [PaletteCommand] {
    PALETTE_COMMANDS
}

/// Parses raw palette input into either fuzzy command search or `#`-prefixed jump mode.
pub fn parse_palette_input(raw: &str) -> PaletteQuery<'_> {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix('#') {
        PaletteQuery::Jump(rest.trim())
    } else {
        PaletteQuery::Command(trimmed)
    }
}

/// Bounded, deterministic case-insensitive fuzzy match score (`None` when not matched).
///
/// Prefers exact matches > prefix matches > word-boundary substrings > general substrings >
/// ordered character subsequences with consecutive-run and word-boundary bonuses.
pub fn fuzzy_score(query: &str, candidate: &str) -> Option<i32> {
    let query_trimmed = query.trim();
    if query_trimmed.is_empty() {
        return Some(0);
    }
    let q_chars: Vec<char> = query_trimmed
        .chars()
        .take(MAX_PALETTE_QUERY_CHARS)
        .flat_map(char::to_lowercase)
        .collect();
    if q_chars.is_empty() {
        return Some(0);
    }
    let c_chars: Vec<char> = candidate.chars().flat_map(char::to_lowercase).collect();
    if c_chars.len() < q_chars.len() {
        return None;
    }

    if c_chars == q_chars {
        return Some(1000);
    }
    if c_chars.starts_with(&q_chars) {
        let len_penalty = (c_chars.len() - q_chars.len()).min(100) as i32;
        return Some(700 - len_penalty);
    }
    if let Some(pos) = find_slice(&c_chars, &q_chars) {
        let at_boundary = pos == 0 || is_word_sep(c_chars[pos - 1]);
        let base = if at_boundary { 520 } else { 360 };
        let pos_penalty = (pos.min(120) as i32) * 2;
        let len_penalty = (c_chars.len() - q_chars.len()).min(80) as i32;
        return Some(base - pos_penalty - len_penalty);
    }

    // Ordered subsequence scoring.
    let mut score: i32 = 120;
    let mut c_idx = 0usize;
    let mut prev_match: Option<usize> = None;
    for &qc in &q_chars {
        let mut found = None;
        for (offset, &cc) in c_chars[c_idx..].iter().enumerate() {
            if cc == qc {
                found = Some(c_idx + offset);
                break;
            }
        }
        let pos = found?;
        if let Some(prev) = prev_match {
            if pos == prev + 1 {
                score += 24;
            } else {
                let gap = (pos - prev - 1).min(20) as i32;
                score -= gap * 2;
            }
        } else {
            score -= (pos.min(40) as i32) * 2;
        }
        if pos == 0 || is_word_sep(c_chars[pos - 1]) {
            score += 18;
        }
        prev_match = Some(pos);
        c_idx = pos + 1;
    }
    let excess = (c_chars.len().saturating_sub(q_chars.len())).min(60) as i32;
    Some(score - excess)
}

fn find_slice(haystack: &[char], needle: &[char]) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn is_word_sep(ch: char) -> bool {
    matches!(ch, ' ' | '-' | '_' | '/' | ':' | '.' | '(' | '[' | '#')
}

/// Ranks `commands` against `query` in deterministic descending score order (ties preserve catalog order).
pub fn rank_palette_commands(query: &str, commands: &[PaletteCommand]) -> Vec<PaletteMatch> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return commands
            .iter()
            .copied()
            .map(|command| PaletteMatch { command, score: 0 })
            .collect();
    }
    let mut matches: Vec<(usize, PaletteMatch)> = commands
        .iter()
        .copied()
        .enumerate()
        .filter_map(|(idx, command)| {
            let label_score = fuzzy_score(trimmed, command.label);
            let combined = format!("{} {}", command.category, command.label);
            let combined_score = fuzzy_score(trimmed, &combined).map(|s| s - 20);
            let keyword_score = command
                .keywords
                .iter()
                .filter_map(|kw| fuzzy_score(trimmed, kw))
                .max()
                .map(|s| s - 40);
            let best = [label_score, combined_score, keyword_score]
                .into_iter()
                .flatten()
                .max()?;
            Some((
                idx,
                PaletteMatch {
                    command,
                    score: best,
                },
            ))
        })
        .collect();
    matches.sort_by(|(idx_a, a), (idx_b, b)| b.score.cmp(&a.score).then_with(|| idx_a.cmp(idx_b)));
    matches.into_iter().map(|(_, m)| m).collect()
}

/// Resolves `#` column-jump and `Cmd/Ctrl+L` Go to Row / Column queries.
///
/// - When `allow_row_jump` is `false` (palette `#` mode), numeric input `N` resolves strictly to
///   1-based source column `#N`.
/// - When `allow_row_jump` is `true` (`Cmd/Ctrl+L` mode):
///   - `#N` or `#name` resolves to columns.
///   - `row:N`, `r:N`, or `:N` resolves to 1-based visible row `N`.
///   - Plain `N` offers visible row `N` first (when in bounds) and source column `#N` second
///     (when in bounds).
/// - Duplicate header names are explicitly distinguished by 1-based `#index` and occurrence count
///   (e.g., `duplicate name (2 of 2)`), and hidden columns carry a `hidden (will show)` badge.
pub fn resolve_jump_targets(
    raw_query: &str,
    headers: &[String],
    hidden: &[bool],
    available_rows: u64,
    allow_row_jump: bool,
) -> Vec<JumpMatch> {
    let trimmed = raw_query.trim();

    // Count duplicate header names case-insensitively across all source columns.
    let mut name_totals: HashMap<String, usize> = HashMap::with_capacity(headers.len());
    let normalized_names: Vec<String> = headers.iter().map(|h| h.trim().to_lowercase()).collect();
    for norm in &normalized_names {
        if !norm.is_empty() {
            *name_totals.entry(norm.clone()).or_insert(0) += 1;
        }
    }
    let mut seen_counts: HashMap<String, usize> = HashMap::new();
    let duplicate_ordinals: Vec<Option<(usize, usize)>> = normalized_names
        .iter()
        .map(|norm| {
            let total = name_totals.get(norm).copied().unwrap_or(0);
            if total > 1 {
                let ord = seen_counts.entry(norm.clone()).or_insert(0);
                *ord += 1;
                Some((*ord, total))
            } else {
                None
            }
        })
        .collect();

    let make_column_match = |source_col: usize, score: i32| -> JumpMatch {
        let index_1based = source_col + 1;
        let name = headers.get(source_col).map(String::as_str).unwrap_or("");
        let is_hidden = hidden.get(source_col).copied().unwrap_or(false);
        let dup = duplicate_ordinals.get(source_col).copied().flatten();
        let title = if name.is_empty() {
            format!("#{index_1based} (unnamed)")
        } else {
            format!("#{index_1based}: {name}")
        };
        let mut detail_parts = vec![format!("Column {index_1based} of {}", headers.len())];
        if let Some((ord, total)) = dup {
            detail_parts.push(format!("duplicate name ({ord} of {total})"));
        }
        if is_hidden {
            detail_parts.push("hidden (will show)".into());
        }
        JumpMatch {
            target: JumpTarget::Column { source_col },
            title,
            detail: detail_parts.join(" · "),
            hidden: is_hidden,
            duplicate_header: dup.is_some(),
            score,
        }
    };

    let make_row_match = |row_1based: u64, score: i32| -> JumpMatch {
        JumpMatch {
            target: JumpTarget::Row {
                display_row_1based: row_1based,
            },
            title: format!("Row {row_1based}"),
            detail: format!("Visible row {row_1based} of {available_rows}"),
            hidden: false,
            duplicate_header: false,
            score,
        }
    };

    // Explicit row-only prefixes in Go-to mode: `row:N`, `r:N`, `:N`.
    if allow_row_jump
        && let Some(row_spec) = trimmed
            .strip_prefix("row:")
            .or_else(|| trimmed.strip_prefix("ROW:"))
            .or_else(|| trimmed.strip_prefix("r:"))
            .or_else(|| trimmed.strip_prefix("R:"))
            .or_else(|| trimmed.strip_prefix(':'))
    {
        let digits = row_spec.trim();
        if let Ok(row_1based) = digits.parse::<u64>()
            && row_1based >= 1
            && row_1based <= available_rows
        {
            return vec![make_row_match(row_1based, 2000)];
        }
        return Vec::new();
    }

    let (explicit_column, effective) = if let Some(rest) = trimmed.strip_prefix('#') {
        (true, rest.trim())
    } else {
        (!allow_row_jump, trimmed)
    };

    if effective.is_empty() {
        return (0..headers.len())
            .map(|col| make_column_match(col, 0))
            .collect();
    }

    let mut results: Vec<(usize, JumpMatch)> = Vec::new();
    let mut matched_cols = vec![false; headers.len()];

    // Numeric target parsing (1-based).
    if let Ok(num) = effective.parse::<u64>()
        && num >= 1
    {
        if !explicit_column && allow_row_jump && num <= available_rows {
            results.push((0, make_row_match(num, 2200)));
        }
        if let Ok(col_1based) = usize::try_from(num)
            && col_1based >= 1
            && col_1based <= headers.len()
        {
            let source_col = col_1based - 1;
            let col_score = if explicit_column { 2200 } else { 2000 };
            results.push((source_col + 1, make_column_match(source_col, col_score)));
            matched_cols[source_col] = true;
        }
    }

    // Name / `#index` fuzzy matches across all source columns.
    for (source_col, name) in headers.iter().enumerate() {
        if matched_cols[source_col] {
            continue;
        }
        let hash_token = format!("#{}", source_col + 1);
        let name_score = fuzzy_score(effective, name);
        let hash_score = fuzzy_score(effective, &hash_token).map(|s| s - 30);
        let combined = format!("{hash_token} {name}");
        let combined_score = fuzzy_score(effective, &combined).map(|s| s - 15);
        if let Some(best) = [name_score, hash_score, combined_score]
            .into_iter()
            .flatten()
            .max()
        {
            results.push((source_col + 1, make_column_match(source_col, best)));
        }
    }

    results.sort_by(|(ord_a, a), (ord_b, b)| b.score.cmp(&a.score).then_with(|| ord_a.cmp(ord_b)));
    results.into_iter().map(|(_, m)| m).collect()
}

/// Helpful empty-state message when a jump query matches no rows or columns.
pub fn jump_empty_message(
    raw_query: &str,
    total_columns: usize,
    available_rows: u64,
    allow_row_jump: bool,
) -> String {
    let trimmed = raw_query.trim();
    if total_columns == 0 && available_rows == 0 {
        return "Open a file to jump to a row or column.".into();
    }
    if allow_row_jump {
        format!(
            "No row (1–{available_rows}) or column (#1–#{total_columns}) matches \"{trimmed}\"."
        )
    } else {
        format!("No column (#1–#{total_columns}) matches \"#{trimmed}\".")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_palette_input_detects_hash_column_mode() {
        assert_eq!(parse_palette_input("sort"), PaletteQuery::Command("sort"));
        assert_eq!(parse_palette_input("  #142  "), PaletteQuery::Jump("142"));
        assert_eq!(
            parse_palette_input("# amount"),
            PaletteQuery::Jump("amount")
        );
        assert_eq!(parse_palette_input("#"), PaletteQuery::Jump(""));
    }

    #[test]
    fn fuzzy_score_ranks_exact_prefix_boundary_subsequence() {
        let exact = fuzzy_score("save", "Save").unwrap();
        let prefix = fuzzy_score("save", "Save As…").unwrap();
        let boundary = fuzzy_score("wrap", "Toggle Word Wrap").unwrap();
        let subseq = fuzzy_score("tww", "Toggle Word Wrap").unwrap();
        assert!(exact > prefix);
        assert!(prefix > boundary);
        assert!(boundary > subseq);
        assert_eq!(fuzzy_score("zzz", "Save"), None);
    }

    #[test]
    fn rank_palette_commands_finds_vim_and_query_bar() {
        let vim = rank_palette_commands("vim", palette_commands());
        assert_eq!(vim[0].command.id, CommandId::ToggleVimMotions);

        let query = rank_palette_commands("query bar", palette_commands());
        assert_eq!(query[0].command.id, CommandId::ToggleQueryBar);

        let go = rank_palette_commands("go to", palette_commands());
        assert_eq!(go[0].command.id, CommandId::GoToRowOrColumn);
    }

    #[test]
    fn resolve_jump_targets_handles_1based_index_duplicates_and_hidden() {
        let headers = vec![
            "id".to_string(),
            "amount".to_string(),
            "state".to_string(),
            "amount".to_string(),
        ];
        let hidden = vec![false, false, false, true];

        // Exact 1-based `#4` selects source column 3 even though it is hidden and a duplicate.
        let by_idx = resolve_jump_targets("4", &headers, &hidden, 100, false);
        assert_eq!(by_idx[0].target, JumpTarget::Column { source_col: 3 });
        assert!(by_idx[0].hidden);
        assert!(by_idx[0].duplicate_header);
        assert!(by_idx[0].title.contains("#4: amount"));
        assert!(by_idx[0].detail.contains("duplicate name (2 of 2)"));
        assert!(by_idx[0].detail.contains("hidden (will show)"));

        // Searching duplicate header name lists BOTH columns with distinct #indices.
        let by_name = resolve_jump_targets("amount", &headers, &hidden, 100, false);
        assert_eq!(by_name.len(), 2);
        assert_eq!(by_name[0].target, JumpTarget::Column { source_col: 1 });
        assert_eq!(by_name[1].target, JumpTarget::Column { source_col: 3 });
        assert!(by_name[0].detail.contains("(1 of 2)"));
        assert!(by_name[1].detail.contains("(2 of 2)"));
        assert!(by_name[1].hidden);

        // Out-of-bounds `#99` returns empty.
        let oob = resolve_jump_targets("99", &headers, &hidden, 100, false);
        assert!(oob.is_empty());
    }

    #[test]
    fn resolve_jump_targets_cmd_l_supports_rows_and_hash_columns() {
        let headers = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let hidden = vec![false, false, false];

        // Plain `2` in Cmd+L mode prefers Row 2, then Column #2.
        let numeric = resolve_jump_targets("2", &headers, &hidden, 50, true);
        assert_eq!(
            numeric[0].target,
            JumpTarget::Row {
                display_row_1based: 2
            }
        );
        assert_eq!(numeric[1].target, JumpTarget::Column { source_col: 1 });

        // `#2` in Cmd+L mode targets Column #2 directly.
        let hash_col = resolve_jump_targets("#2", &headers, &hidden, 50, true);
        assert_eq!(hash_col[0].target, JumpTarget::Column { source_col: 1 });

        // `row:42` in Cmd+L mode targets Row 42 only.
        let explicit_row = resolve_jump_targets("row:42", &headers, &hidden, 50, true);
        assert_eq!(explicit_row.len(), 1);
        assert_eq!(
            explicit_row[0].target,
            JumpTarget::Row {
                display_row_1based: 42
            }
        );
    }
}
