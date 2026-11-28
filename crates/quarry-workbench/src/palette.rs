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
        id: CommandId::ToggleCellInspector,
        label: "Toggle Cell Inspector",
        category: "View",
        shortcut_macos: Some("⌘I"),
        shortcut_other: Some("Ctrl+I"),
        keywords: &[
            "inspect",
            "inspector",
            "peek",
            "json",
            "stack",
            "markdown",
            "drawer",
        ],
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
/// - When `allow_row_jump` is `false` (palette `#` mode), purely numeric input `N` resolves
///   strictly to 1-based source column `#N` (never falling through to fuzzy matching on `#0`,
///   overflow, or out-of-range indices).
/// - When `allow_row_jump` is `true` (`Cmd/Ctrl+L` mode):
///   - `#N` or `#name` resolves to columns.
///   - `row:N`, `r:N`, or `:N` resolves to 1-based visible row `N`.
///   - Plain numeric `N` offers visible row `N` first (when in bounds) and source column `#N`
///     second (when in bounds), without falling through to fuzzy matching when out of bounds.
/// - Duplicate header names are explicitly distinguished by 1-based `#index` and occurrence count
///   (e.g., `duplicate name (2 of 2)`), and hidden columns carry a `hidden (will show)` badge.
pub fn resolve_jump_targets(
    raw_query: &str,
    headers: &[String],
    hidden: &[bool],
    available_rows: u64,
    allow_row_jump: bool,
) -> Vec<JumpMatch> {
    resolve_jump_targets_bounded(
        raw_query,
        headers,
        hidden,
        available_rows,
        allow_row_jump,
        usize::MAX,
    )
}

/// Bounded variant of [`resolve_jump_targets`] that formats at most `max_results` displayed
/// matches so wide schemas avoid formatting thousands of undisplayed labels.
pub fn resolve_jump_targets_bounded(
    raw_query: &str,
    headers: &[String],
    hidden: &[bool],
    available_rows: u64,
    allow_row_jump: bool,
    max_results: usize,
) -> Vec<JumpMatch> {
    if max_results == 0 {
        return Vec::new();
    }
    let trimmed = raw_query.trim();

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

    // Purely numeric input finishes numeric resolution directly so `#0`, overflow, or
    // out-of-range numbers never fall through to fuzzy matches like `#10` or `FY99`.
    if !effective.is_empty() && effective.chars().all(|c| c.is_ascii_digit()) {
        let mut results = Vec::new();
        if let Ok(num) = effective.parse::<u64>()
            && num >= 1
        {
            if !explicit_column && allow_row_jump && num <= available_rows {
                results.push(make_row_match(num, 2200));
            }
            if let Ok(col_1based) = usize::try_from(num)
                && col_1based >= 1
                && col_1based <= headers.len()
                && results.len() < max_results
            {
                let source_col = col_1based - 1;
                let col_score = if explicit_column { 2200 } else { 2000 };
                let dup = duplicate_ordinal_for_column(headers, source_col);
                results.push(format_column_match(
                    headers, hidden, source_col, dup, col_score,
                ));
            }
        }
        return results;
    }

    // Score/select column candidates first, then format only the top `max_results` entries.
    let selected_cols: Vec<(usize, i32)> = if effective.is_empty() {
        (0..headers.len().min(max_results))
            .map(|col| (col, 0))
            .collect()
    } else {
        let mut scored: Vec<(usize, i32)> = headers
            .iter()
            .enumerate()
            .filter_map(|(source_col, name)| {
                let hash_token = format!("#{}", source_col + 1);
                let name_score = fuzzy_score(effective, name);
                let hash_score = fuzzy_score(effective, &hash_token).map(|s| s - 30);
                let combined = format!("{hash_token} {name}");
                let combined_score = fuzzy_score(effective, &combined).map(|s| s - 15);
                let best = [name_score, hash_score, combined_score]
                    .into_iter()
                    .flatten()
                    .max()?;
                Some((source_col, best))
            })
            .collect();
        scored.sort_by(|(col_a, score_a), (col_b, score_b)| {
            score_b.cmp(score_a).then_with(|| col_a.cmp(col_b))
        });
        scored.truncate(max_results);
        scored
    };

    if selected_cols.is_empty() {
        return Vec::new();
    }

    // Count duplicate names only for normalized headers present in the displayed slice.
    let mut target_names: HashMap<String, usize> = HashMap::with_capacity(selected_cols.len());
    for &(source_col, _) in &selected_cols {
        let norm = headers[source_col].trim().to_lowercase();
        if !norm.is_empty() {
            target_names.entry(norm).or_insert(0);
        }
    }
    let mut ordinals: HashMap<usize, (usize, usize)> = HashMap::new();
    if !target_names.is_empty() {
        for name in headers {
            let norm = name.trim().to_lowercase();
            if let Some(total) = target_names.get_mut(&norm) {
                *total += 1;
            }
        }
        let mut seen: HashMap<String, usize> = HashMap::new();
        for (idx, name) in headers.iter().enumerate() {
            let norm = name.trim().to_lowercase();
            if let Some(&total) = target_names.get(&norm)
                && total > 1
            {
                let ord = seen.entry(norm).or_insert(0);
                *ord += 1;
                ordinals.insert(idx, (*ord, total));
            }
        }
    }

    selected_cols
        .into_iter()
        .map(|(source_col, score)| {
            let dup = ordinals.get(&source_col).copied();
            format_column_match(headers, hidden, source_col, dup, score)
        })
        .collect()
}

fn duplicate_ordinal_for_column(headers: &[String], source_col: usize) -> Option<(usize, usize)> {
    let target = headers.get(source_col)?.trim().to_lowercase();
    if target.is_empty() {
        return None;
    }
    let mut total = 0usize;
    let mut ord = 0usize;
    for (idx, name) in headers.iter().enumerate() {
        if name.trim().to_lowercase() == target {
            total += 1;
            if idx <= source_col {
                ord += 1;
            }
        }
    }
    (total > 1).then_some((ord, total))
}

fn format_column_match(
    headers: &[String],
    hidden: &[bool],
    source_col: usize,
    dup: Option<(usize, usize)>,
    score: i32,
) -> JumpMatch {
    let index_1based = source_col + 1;
    let name = headers.get(source_col).map(String::as_str).unwrap_or("");
    let is_hidden = hidden.get(source_col).copied().unwrap_or(false);
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

    #[test]
    fn resolve_jump_targets_rejects_zero_out_of_range_and_overflow_without_fuzzy_fallthrough() {
        let mut headers: Vec<String> = (1..=10).map(|i| format!("c{i}")).collect();
        headers[4] = "FY99".into();
        let hidden = vec![false; headers.len()];

        // `#0` on a 10-column file must NOT fuzzy-match `#10`.
        assert!(resolve_jump_targets("0", &headers, &hidden, 100, false).is_empty());
        assert!(resolve_jump_targets("#0", &headers, &hidden, 100, true).is_empty());

        // Out-of-range `#99` must NOT fuzzy-match header `FY99`.
        assert!(resolve_jump_targets("99", &headers, &hidden, 50, false).is_empty());
        assert!(resolve_jump_targets("99", &headers, &hidden, 50, true).is_empty());

        // Overflowing numeric input returns empty cleanly.
        assert!(
            resolve_jump_targets("999999999999999999999999999", &headers, &hidden, 50, true)
                .is_empty()
        );

        // Bounded variant caps empty-query formatting cleanly.
        let wide_headers: Vec<String> = (1..=300).map(|i| format!("col_{i}")).collect();
        let wide_hidden = vec![false; 300];
        let bounded =
            resolve_jump_targets_bounded("", &wide_headers, &wide_hidden, 1000, false, 80);
        assert_eq!(bounded.len(), 80);
        assert_eq!(bounded[0].target, JumpTarget::Column { source_col: 0 });
        assert_eq!(bounded[79].target, JumpTarget::Column { source_col: 79 });
    }
}
