//! Frontend-neutral per-tab draft, format, notice, and close-lifecycle state.

use crate::{
    AppMessage, Document, StructuralDialog, StructuralJob, case_sensitivity, parse_file_column,
};
use quarry_core::{
    Encoding, FilterOperator, FilterPredicate, FilterQuery, HeaderMode, OpenOptions,
};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DelimiterMode {
    Auto,
    Comma,
    Tab,
    Pipe,
    Semicolon,
}

impl DelimiterMode {
    pub const ALL: [Self; 5] = [
        Self::Auto,
        Self::Comma,
        Self::Tab,
        Self::Pipe,
        Self::Semicolon,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Comma => "Comma",
            Self::Tab => "Tab",
            Self::Pipe => "Pipe",
            Self::Semicolon => "Semicolon",
        }
    }

    pub fn delimiter(self) -> Option<u8> {
        match self {
            Self::Auto => None,
            Self::Comma => Some(b','),
            Self::Tab => Some(b'\t'),
            Self::Pipe => Some(b'|'),
            Self::Semicolon => Some(b';'),
        }
    }

    pub fn from_delimiter(delimiter: Option<u8>) -> Self {
        match delimiter {
            Some(b',') => Self::Comma,
            Some(b'\t') => Self::Tab,
            Some(b'|') => Self::Pipe,
            Some(b';') => Self::Semicolon,
            _ => Self::Auto,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterRuleDraft {
    pub column_input: String,
    pub column_search: String,
    pub operator: FilterOperator,
    pub value_input: String,
    pub upper_bound_input: String,
}

impl Default for FilterRuleDraft {
    fn default() -> Self {
        Self {
            column_input: "1".into(),
            column_search: String::new(),
            operator: FilterOperator::Contains,
            value_input: String::new(),
            upper_bound_input: String::new(),
        }
    }
}

pub fn filter_predicate_from_rule(
    rule: &FilterRuleDraft,
    total_columns: usize,
) -> Result<FilterPredicate, String> {
    Ok(FilterPredicate {
        column: parse_file_column(&rule.column_input, total_columns)?,
        operator: rule.operator,
        value: rule.value_input.as_bytes().to_vec(),
        upper_bound: (rule.operator == FilterOperator::Between)
            .then(|| rule.upper_bound_input.as_bytes().to_vec()),
    })
}

pub fn filter_query_from_rules(
    rules: &[FilterRuleDraft],
    match_case: bool,
    total_columns: usize,
) -> Result<FilterQuery, String> {
    let predicates = rules
        .iter()
        .enumerate()
        .map(|(index, rule)| {
            filter_predicate_from_rule(rule, total_columns)
                .map_err(|error| format!("Rule {}: {error}", index + 1))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let query = FilterQuery {
        predicates,
        case_sensitivity: case_sensitivity(match_case),
    };
    query.validate().map_err(|error| error.to_string())?;
    Ok(query)
}

/// Result of starting a tab close review.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TabCloseState {
    /// No unsaved edits or active file operations remain; the tab can retire now.
    Ready,
    /// Unsaved edits require an explicit Save / Discard / Keep Editing decision.
    ConfirmUnsaved,
    /// Active file operations must complete or be cancelled before closing.
    WaitForJobs,
}

/// UI-neutral per-tab drafts, format choices, feedback, write reservations, and close state.
///
/// Frontend-local widget state (text editor cursor/undo snapshots, IME, focus targets,
/// cached table column widths, and painting) lives in the frontend's tab wrapper instead.
pub struct WorkbenchTab {
    pub id: u64,
    pub storage_approved: bool,
    pub jump_input: String,
    pub find_input: String,
    pub replace_input: String,
    pub find_match_case: bool,
    pub find_bar_open: bool,
    pub replace_expanded: bool,
    pub filter_match_case: bool,
    pub sort_match_case: bool,
    pub column_search_input: String,
    pub columns_open: bool,
    pub structural_dialog: Option<StructuralDialog>,
    pub filter_rules: Vec<FilterRuleDraft>,
    pub filters_open: bool,
    pub delimiter_mode: DelimiterMode,
    pub header_mode: HeaderMode,
    pub encoding_mode: Option<Encoding>,
    pub format_draft: Option<(DelimiterMode, HeaderMode, Option<Encoding>)>,
    pub document: Option<Document>,
    pub word_wrap_override: Option<bool>,
    pub notice: Option<AppMessage>,
    pub footer_status: Option<AppMessage>,
    pub close_confirmation_open: bool,
    pub close_after_save: bool,
    pub close_ready: bool,
    pub close_after_jobs: bool,
    pub write_destination: Option<PathBuf>,
}

impl WorkbenchTab {
    /// Creates an empty tab with an identity that remains stable across switches.
    pub fn new(id: u64) -> Self {
        Self {
            id,
            storage_approved: false,
            jump_input: "1".into(),
            find_input: String::new(),
            replace_input: String::new(),
            find_match_case: false,
            find_bar_open: false,
            replace_expanded: false,
            filter_match_case: false,
            sort_match_case: false,
            column_search_input: String::new(),
            columns_open: false,
            structural_dialog: None,
            filter_rules: vec![FilterRuleDraft::default()],
            filters_open: false,
            delimiter_mode: DelimiterMode::Auto,
            header_mode: HeaderMode::Auto,
            encoding_mode: None,
            format_draft: None,
            document: None,
            word_wrap_override: None,
            notice: None,
            footer_status: None,
            close_confirmation_open: false,
            close_after_save: false,
            close_ready: false,
            close_after_jobs: false,
            write_destination: None,
        }
    }

    /// The tab's accessible name: the filename plus its unsaved and busy states in words.
    pub fn title_with_extra_busy(&self, extra_busy: bool) -> String {
        let busy = extra_busy || self.document_busy();
        self.document.as_ref().map_or_else(
            || "No file".into(),
            |document| {
                let filename = document
                    .logical_path
                    .file_name()
                    .unwrap_or(document.logical_path.as_os_str())
                    .to_string_lossy();
                format!(
                    "{filename}{}{}",
                    if document.is_dirty() {
                        ", unsaved changes"
                    } else {
                        ""
                    },
                    if busy { ", busy" } else { "" }
                )
            },
        )
    }

    /// True when the tab's document owns any active background read or write worker,
    /// including a pending duplicate-removal confirmation modal (used for edit blocking and dormancy).
    pub fn document_busy(&self) -> bool {
        self.document.as_ref().is_some_and(|document| {
            document.job.is_some()
                || document.bookmark_offset_job.is_some()
                || document.bookmark_offset_refresh_pending
                || document.section_verify_job.is_some()
                || document.decompressing()
                || document.search_job.is_some()
                || document.is_filter_scanning()
                || document.filter_rows_loading()
                || document.filter_refresh_pending
                || document.save_job.is_some()
                || document.export_job.is_some()
                || document.structural_job.is_some()
        })
    }

    /// True when this tab owns an actively running background job (excluding a completed
    /// `DuplicatePreview` that is simply waiting on user confirmation).
    pub fn has_running_document_job(&self) -> bool {
        self.document.as_ref().is_some_and(|document| {
            document.job.is_some()
                || document.bookmark_offset_job.is_some()
                || document.bookmark_offset_refresh_pending
                || document.section_verify_job.is_some()
                || document.decompressing()
                || document.search_job.is_some()
                || document.is_filter_scanning()
                || document.filter_rows_loading()
                || document.filter_refresh_pending
                || document.save_job.is_some()
                || document.export_job.is_some()
                || document
                    .structural_job
                    .as_ref()
                    .is_some_and(|job| !matches!(job, StructuralJob::DuplicatePreview { .. }))
        })
    }

    /// Returns `true` when this tab has any running background job or Live Watch session
    /// requiring periodic UI polling (independent of active-tab whitespace harvesting).
    pub fn has_background_activity(&self) -> bool {
        self.has_running_document_job()
            || self.document.as_ref().is_some_and(|document| {
                document.is_live_watching() || document.live_watch_viewport_dirty
            })
    }

    /// Returns `true` when this tab has an active whitespace inspection batch waiting to be polled.
    /// Callers should check this only for visible/active tabs because hidden tabs do not poll
    /// whitespace batches until re-activated.
    pub fn has_active_whitespace_work(&self) -> bool {
        self.document
            .as_ref()
            .is_some_and(|document| document.whitespace.is_busy())
    }

    /// Counts document file operations and structural analysis against the global write limit.
    /// A completed duplicate preview does not occupy a write slot.
    pub fn document_writing(&self) -> bool {
        self.document.as_ref().is_some_and(|document| {
            document.save_job.is_some()
                || document.export_job.is_some()
                || document
                    .structural_job
                    .as_ref()
                    .is_some_and(|job| !matches!(job, StructuralJob::DuplicatePreview { .. }))
        })
    }

    /// Document-side operations that count directly against the global job cap (not via worker slots).
    pub fn unslotted_document_operations(&self, extra_writing: bool) -> usize {
        usize::from(extra_writing || self.document_writing())
            + usize::from(
                self.document
                    .as_ref()
                    .is_some_and(Document::awaiting_decompression),
            )
    }

    /// Builds `OpenOptions` from this tab's selected delimiter and header mode overrides.
    pub fn open_options(&self) -> OpenOptions {
        OpenOptions {
            delimiter: self.delimiter_mode.delimiter(),
            header_mode: self.header_mode,
            ..OpenOptions::default()
        }
    }

    /// True when the document has unsaved edits or unconfirmed drafts.
    pub fn is_dirty(&self) -> bool {
        self.document.as_ref().is_some_and(Document::is_dirty)
    }

    /// Begins reviewing a tab close against active jobs and unsaved edits.
    pub fn begin_close(&mut self, extra_writing: bool) -> TabCloseState {
        if extra_writing || self.document_writing() {
            self.close_after_jobs = true;
            self.close_confirmation_open = false;
            self.close_ready = false;
            return TabCloseState::WaitForJobs;
        }
        if self.is_dirty() {
            self.close_confirmation_open = true;
            self.close_after_jobs = false;
            self.close_ready = false;
            return TabCloseState::ConfirmUnsaved;
        }
        self.close_confirmation_open = false;
        self.close_after_save = false;
        self.close_after_jobs = false;
        self.close_ready = true;
        TabCloseState::Ready
    }

    /// Cancels a pending close (Keep Editing), preserving the document and all drafts.
    pub fn cancel_close(&mut self) {
        self.close_confirmation_open = false;
        self.close_after_save = false;
        self.close_after_jobs = false;
        self.close_ready = false;
    }

    /// Approves discarding unsaved changes so the tab can retire cleanly.
    pub fn approve_discard_and_close(&mut self) {
        self.close_confirmation_open = false;
        self.close_after_save = false;
        self.close_after_jobs = false;
        self.close_ready = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quarry_core::CaseSensitivity;
    use std::fs;
    use std::time::{Duration, Instant};

    fn open_indexed_document(contents: &[u8]) -> (tempfile::TempDir, Document) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tab-test.csv");
        fs::write(&path, contents).unwrap();
        let mut doc = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..Default::default()
            },
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while doc.job.is_some() {
            assert!(Instant::now() < deadline, "index timed out");
            doc.poll().unwrap();
            std::thread::yield_now();
        }
        (dir, doc)
    }

    #[test]
    fn switching_numeric_filter_operators_keeps_text_and_case_preferences() {
        let mut rule = FilterRuleDraft {
            column_input: "2".into(),
            column_search: "amount".into(),
            operator: FilterOperator::Contains,
            value_input: "10".into(),
            upper_bound_input: "20".into(),
        };
        let query = filter_query_from_rules(&[rule.clone()], true, 3).unwrap();
        assert_eq!(query.case_sensitivity, CaseSensitivity::Sensitive);
        assert_eq!(query.predicates[0].operator, FilterOperator::Contains);
        assert_eq!(query.predicates[0].upper_bound, None);

        rule.operator = FilterOperator::Between;
        let query = filter_query_from_rules(&[rule.clone()], true, 3).unwrap();
        assert_eq!(query.case_sensitivity, CaseSensitivity::Sensitive);
        assert_eq!(query.predicates[0].operator, FilterOperator::Between);
        assert_eq!(query.predicates[0].value, b"10");
        assert_eq!(
            query.predicates[0].upper_bound.as_deref(),
            Some(b"20".as_slice())
        );

        rule.operator = FilterOperator::GreaterThan;
        let query = filter_query_from_rules(&[rule.clone()], false, 3).unwrap();
        assert_eq!(query.case_sensitivity, CaseSensitivity::Insensitive);
        assert_eq!(query.predicates[0].operator, FilterOperator::GreaterThan);
        assert_eq!(query.predicates[0].upper_bound, None);
    }

    #[test]
    fn workbench_tab_close_lifecycle_handles_clean_dirty_and_keep_editing() {
        let (_dir, doc) = open_indexed_document(b"name,value\nalpha,1\nbeta,2\n");
        let mut tab = WorkbenchTab::new(7);
        assert_eq!(tab.title_with_extra_busy(false), "No file");
        assert_eq!(tab.begin_close(false), TabCloseState::Ready);
        assert!(tab.close_ready);

        tab.cancel_close();
        assert!(!tab.close_ready);
        tab.document = Some(doc);
        assert_eq!(tab.title_with_extra_busy(false), "tab-test.csv");
        assert_eq!(tab.title_with_extra_busy(true), "tab-test.csv, busy");

        // Dirty edit requires explicit close confirmation; Keep Editing preserves the edit.
        let doc = tab.document.as_mut().unwrap();
        let source = doc.source_cell(1, 1).unwrap().to_vec();
        doc.begin_cell_edit(1, 1, source).unwrap();
        doc.cell_edit.as_mut().unwrap().draft = "99".into();
        doc.commit_cell_edit();
        assert_eq!(
            tab.title_with_extra_busy(false),
            "tab-test.csv, unsaved changes"
        );
        assert_eq!(tab.begin_close(false), TabCloseState::ConfirmUnsaved);
        assert!(tab.close_confirmation_open);
        assert!(!tab.close_ready);

        tab.cancel_close();
        assert!(!tab.close_confirmation_open);
        assert!(!tab.close_ready);
        assert!(tab.is_dirty());

        // Active extra writer defers close into WaitForJobs.
        assert_eq!(tab.begin_close(true), TabCloseState::WaitForJobs);
        assert!(tab.close_after_jobs);
        assert!(!tab.close_confirmation_open);

        // Explicit discard marks ready to close.
        tab.approve_discard_and_close();
        assert!(tab.close_ready);
        assert!(!tab.close_confirmation_open);
        assert!(!tab.close_after_jobs);

        // Keep `_dir` alive until after `tab` drops.
        drop(tab);
    }

    #[test]
    fn workbench_tab_tracks_format_options_and_duplicate_preview_is_not_a_writer() {
        let (_dir, doc) = open_indexed_document(b"name,value\nalpha,1\nalpha,1\n");
        let mut tab = WorkbenchTab::new(1);
        tab.delimiter_mode = DelimiterMode::Tab;
        tab.header_mode = HeaderMode::NoHeader;
        let opts = tab.open_options();
        assert_eq!(opts.delimiter, Some(b'\t'));
        assert_eq!(opts.header_mode, HeaderMode::NoHeader);

        tab.document = Some(doc);
        assert!(!tab.document_busy());
        assert!(!tab.document_writing());
        assert_eq!(tab.unslotted_document_operations(false), 0);
        assert_eq!(tab.unslotted_document_operations(true), 1);
    }
}
