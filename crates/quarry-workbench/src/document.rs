//! Frontend-neutral `Document` model, background job coordination, and working-copy lifecycle.

use crate::bookmarks::{BookmarkSplitTarget, BookmarkTransform, RowBookmark, RowBookmarks};
use crate::command::DocumentId;
use crate::compressed;
use crate::history::{
    EditHistory, EditTarget, SharedCellEdits, StructuralOverlay, ValueEdit, WorkingCopySnapshot,
    WorkingCopyState,
};
use crate::selection::{
    ColumnView, GridCursor, GridSelection, RowSelection, validated_column_selection,
};
use crate::whitespace;
use crate::{
    AppMessage, BOOTSTRAP_ROWS, JOIN_INPUT_RESERVED, MAX_COPY_BYTES, OVERSCAN_ROWS, POLL_INTERVAL,
    SOURCE_CHANGED_NOTICE, STORAGE_REVIEW_BYTES, StructuralDialog, column_name, field_text,
    format_bytes, format_unavailable_scratch_notice, group_digits, headers_for, literal_contains,
    logical_viewport_start, max_viewport_start, parse_insertion_count, parse_insertion_position,
    progress_fraction, replace_literal_all_with_case, selection_fields_text, sort_direction_label,
};
use quarry_core::{
    BoundaryTrust, ByteWindow, CaseSensitivity, CleanupJob, CleanupOutcome, ColumnCleanup,
    ColumnTransformation, CompletedIndex, DateFormatSuggestion, DateFormatSuggestionConfig,
    DuplicateJob, DuplicateOutcome, DuplicateSpec, DuplicateSummary, DuplicateTarget, Encoding,
    FilterColumnSchema, FilterExportJob, FilterExportOutcome, FilterExportProgress,
    FilterExprError, FilterIndex, FilterJob, FilterMatch, FilterOperator, FilterProgress,
    FilterQuery, FilterReadJob, FilterReadOutcome, HeaderMode, IndexCacheConfig, IndexConfig,
    IndexJob, IndexProgress, JoinConfig, JoinWorkingSource, LiteralReplacement, LiveWatchFilterJob,
    LiveWatchJob, MAX_TRANSFORMATION_COLUMNS, OpenOptions, QuarryError, ReplaceAllJob,
    ReplaceAllOutcome, Row, RowInsertion, SaveAsJob, SaveAsOutcome, SaveAsProgress, SearchJob,
    SearchMatch, SearchOutcome, SearchPosition, SectionBounds, SectionOpenOptions, Session,
    SortDirection, SortJob, SortMode, SortOutcome, SortPlan, SplitAnalysisJob,
    SplitAnalysisOutcome, StructuralIndex, VerifyBoundaryJob, VerifyBoundaryOutcome,
    VerifyBoundaryProgress, WatchAlert, WatchConfig, WatchSnapshot, WatchStatus, parse_filter_expr,
    print_filter_expr,
};

/// Actionable reason surfaced when editing or modifying a document while Live Watch is active.
pub const LIVE_WATCH_READ_ONLY_REASON: &str =
    "Stop Live Watch before editing or modifying this file.";

/// Repaint/UI ingestion sync throttle (~10 Hz) while Live Watch streams appended rows.
pub const WATCH_REPAINT_THROTTLE: Duration = Duration::from_millis(100);

/// Active file-size ceiling (10 MiB) at or below which live as-you-type Query Bar filtering may
/// default on; larger files default to Enter-to-apply unless the user explicitly opts into Live.
pub const DEFAULT_LIVE_FILTER_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// Monotonic tag attached to every filter scan and filtered-page read so superseded queries,
/// post-edit revisions, or results from another document can never overwrite visible rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FilterResultTag {
    pub document_id: DocumentId,
    pub edit_revision: u64,
    pub query_generation: u64,
}
#[cfg(any(test, feature = "test-hooks"))]
use quarry_core::{SearchProgress, SortSpec};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

// Test-only accounting at the owned-job boundary catches byte copies on UI paths.
#[cfg(any(test, feature = "test-hooks"))]
std::thread_local! {
    static OWNED_EDIT_SNAPSHOT_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(any(test, feature = "test-hooks"))]
pub fn reset_owned_edit_snapshot_bytes() {
    OWNED_EDIT_SNAPSHOT_BYTES.with(|bytes| bytes.set(0));
}

#[cfg(any(test, feature = "test-hooks"))]
pub fn owned_edit_snapshot_bytes() -> usize {
    OWNED_EDIT_SNAPSHOT_BYTES.with(std::cell::Cell::get)
}

pub struct MaterializationPreparation {
    pub undo: WorkingCopySnapshot,
    pub destination: PathBuf,
    pub renames: BTreeMap<usize, Vec<u8>>,
}

pub enum StructuralJob {
    Cleaning {
        job: CleanupJob,
        destination: PathBuf,
        selected_columns: BTreeSet<usize>,
        undo: WorkingCopySnapshot,
    },
    FindingDuplicates {
        job: DuplicateJob,
        destination: PathBuf,
        selected_columns: BTreeSet<usize>,
        undo: WorkingCopySnapshot,
    },
    DuplicatePreview {
        summary: DuplicateSummary,
        selected_columns: BTreeSet<usize>,
        undo: WorkingCopySnapshot,
    },
    AnalyzingSplit {
        job: SplitAnalysisJob,
        source_column: usize,
        separator: Vec<u8>,
    },
    Materializing {
        job: SaveAsJob,
        destination: PathBuf,
        selected_columns: BTreeSet<usize>,
        undo: WorkingCopySnapshot,
    },
    Replacing {
        job: ReplaceAllJob,
        destination: PathBuf,
        selected_columns: BTreeSet<usize>,
        undo: WorkingCopySnapshot,
    },
    Sorting {
        job: SortJob,
        destination: PathBuf,
        selected_columns: BTreeSet<usize>,
        undo: WorkingCopySnapshot,
        plan: SortPlan,
    },
    InsertingRows {
        job: SaveAsJob,
        destination: PathBuf,
        undo: WorkingCopySnapshot,
        position: u64,
        count: u64,
    },
    DeletingRows {
        job: SaveAsJob,
        destination: PathBuf,
        undo: WorkingCopySnapshot,
        deleted_ranges: Vec<RangeInclusive<u64>>,
        count: u64,
    },
}

pub struct MaterializedWorkingCopy {
    pub path: PathBuf,
    pub output_index: Option<CompletedIndex>,
    pub undo: WorkingCopySnapshot,
    pub selected_columns: BTreeSet<usize>,
    pub notice: String,
    pub bookmark_transform: BookmarkTransform,
}

pub struct StructuralProgressDisplay {
    pub fraction: f32,
    pub label: String,
    pub animate: bool,
}

/// Count first, then show written bytes against the conservative output allowance.
pub fn materialization_progress(
    progress: SaveAsProgress,
    operation: &str,
) -> StructuralProgressDisplay {
    if progress.done {
        StructuralProgressDisplay {
            fraction: 1.0,
            label: format!("{operation} · complete"),
            animate: false,
        }
    } else if progress.preparing {
        let counted = progress_fraction(
            progress.preparation_bytes_scanned,
            progress.total_bytes,
            false,
        );
        StructuralProgressDisplay {
            fraction: 0.2 * counted,
            label: format!("{operation} · counting {:.1}%", counted * 100.0),
            animate: progress.preparation_bytes_scanned == 0,
        }
    } else if progress.estimated_output_bytes == 0 {
        StructuralProgressDisplay {
            fraction: 0.0,
            label: format!("{operation} · preparing…"),
            animate: true,
        }
    } else {
        StructuralProgressDisplay {
            fraction: (0.2
                + 0.8
                    * progress_fraction(
                        progress.bytes_written,
                        progress.estimated_output_bytes,
                        false,
                    ))
            .min(0.99),
            label: format!(
                "{operation} · {} written",
                format_bytes(progress.bytes_written)
            ),
            animate: progress.bytes_written == 0,
        }
    }
}

pub fn sort_progress(
    preparing: bool,
    preparation_bytes_scanned: u64,
    bytes_scanned: u64,
    total_bytes: u64,
    done: bool,
) -> StructuralProgressDisplay {
    if done {
        StructuralProgressDisplay {
            fraction: 1.0,
            label: "Sorting rows · 100.0%".into(),
            animate: false,
        }
    } else if preparing {
        StructuralProgressDisplay {
            fraction: 0.0,
            label: if preparation_bytes_scanned == 0 {
                "Preparing to sort…".into()
            } else {
                format!(
                    "Preparing to sort · counting {:.1}%",
                    progress_fraction(preparation_bytes_scanned, total_bytes, false) * 100.0
                )
            },
            animate: true,
        }
    } else if total_bytes == 0 || bytes_scanned >= total_bytes {
        StructuralProgressDisplay {
            fraction: 0.9,
            label: "Merging sorted rows…".into(),
            animate: true,
        }
    } else {
        let read_fraction = progress_fraction(bytes_scanned, total_bytes, false);
        StructuralProgressDisplay {
            fraction: 0.9 * read_fraction,
            label: format!("Sorting rows · {:.1}% read", read_fraction * 100.0),
            animate: false,
        }
    }
}

/// Snapshot of immutable inputs needed to read an uncached viewport window off the UI thread.
pub struct ViewportReadPlan {
    pub session: Arc<Session>,
    pub index: StructuralIndex,
    pub live_watch_reader: Option<quarry_core::LiveWatchReader>,
    pub start: u64,
    pub count: usize,
}

impl ViewportReadPlan {
    /// Executes the planned viewport read (off the UI thread), routing live-watched documents
    /// through the watcher's pinned descriptor and static documents through `Session::read_rows`.
    pub fn execute(&self) -> Result<Vec<Row>, QuarryError> {
        if let Some(reader) = &self.live_watch_reader {
            reader.read_rows(self.start, self.count)
        } else {
            self.session.read_rows(&self.index, self.start, self.count)
        }
    }
}

static NEXT_DOCUMENT_INSTANCE_ID: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

pub struct Document {
    pub instance_id: u64,
    pub document_id: Option<DocumentId>,
    pub join_reservations: Arc<std::sync::atomic::AtomicUsize>,
    pub pending_diagnostic: Option<Box<quarry_core::Diagnostic>>,
    pub pending_diagnostic_message: Option<String>,
    pub working_directory: PathBuf,
    pub custom_working_directory: bool,
    pub index_cache_enabled: bool,
    pub working_cleanup: quarry_core::WorkingCopyCleanup,
    /// The app's decompression slots, which a compressed document's pass claims.
    pub decompression_slots: compressed::DecompressionSlots,
    pub pending_materialization: Option<(ColumnTransformation, BTreeSet<usize>, u64)>,
    pub session: Arc<Session>,
    /// Present when the document reads a gzip-compressed file; it is then read-only.
    pub compressed: Option<compressed::CompressedDocument>,
    pub logical_path: PathBuf,
    pub original_session: Option<Session>,
    pub working_copy: Option<WorkingCopyState>,
    pub edit_history: EditHistory,
    pub source_generation: u64,
    pub bookmarks: RowBookmarks,
    pub bookmark_offset_job: Option<BookmarkOffsetJob>,
    pub bookmark_offset_refresh_pending: bool,
    pub job: Option<IndexJob>,
    pub index: Option<StructuralIndex>,
    pub progress: IndexProgress,
    pub search_job: Option<SearchJob>,
    pub search_query: Vec<u8>,
    pub search_case_sensitivity: CaseSensitivity,
    pub last_match: Option<SearchMatch>,
    pub search_history: Vec<SearchMatch>,
    pub search_history_limits: Vec<Option<u64>>,
    pub search_history_index: Option<usize>,
    pub search_status: Option<String>,
    pub search_paused_row_limit: Option<u64>,
    pub filter_job: Option<FilterJob>,
    pub filter_job_tag: Option<FilterResultTag>,
    pub filter_index: Option<FilterIndex>,
    pub filter_index_tag: Option<FilterResultTag>,
    pub filter_query: Option<FilterQuery>,
    pub filter_query_generation: u64,
    pub filter_refresh_pending: bool,
    pub filter_progress: Option<FilterProgress>,
    pub filter_status: Option<String>,
    pub filter_paused_row_limit: Option<u64>,
    pub export_job: Option<FilterExportJob>,
    pub export_progress: Option<FilterExportProgress>,
    pub export_status: Option<String>,
    pub export_cancel_requested: bool,
    pub save_job: Option<SaveAsJob>,
    pub save_output_encoding: Option<Encoding>,
    pub save_status: Option<String>,
    pub save_cancel_requested: bool,
    pub saving_in_place: bool,
    pub last_splice_summary: Option<quarry_core::SpliceSaveSummary>,
    pub structural_job: Option<StructuralJob>,
    pub structural_status: Option<String>,
    pub structural_cancel_requested: bool,
    pub section_verify_job: Option<VerifyBoundaryJob>,
    pub section_verify_progress: Option<VerifyBoundaryProgress>,
    pub section_verify_status: Option<String>,
    pub verified_bounds_override: Option<SectionBounds>,
    pub pending_verified_reopen: Option<SectionBounds>,
    pub section_quote_free_confirmed: bool,
    pub live_watch: Option<LiveWatchJob>,
    pub live_watch_filter_attach: Option<LiveWatchFilterJob>,
    pub live_watch_filter_attach_tag: Option<FilterResultTag>,
    pub live_watch_snapshot: Option<WatchSnapshot>,
    live_watch_header_row: Option<Row>,
    pub live_watch_alert: Option<WatchAlert>,
    pub live_watch_tail_follow: bool,
    pub live_watch_async_viewport: bool,
    pub live_watch_viewport_dirty: bool,
    live_watch_last_ui_sync: Instant,
    live_watch_last_generation: u64,
    pub source_changed: bool,
    pub filter_viewport_start: u64,
    pub filter_buffer_start: u64,
    pub filtered_rows: Vec<Arc<FilterMatch>>,
    pub filtered_rows_tag: Option<FilterResultTag>,
    pub filter_read: Option<ActiveFilterRead>,
    pub pending_filter_read: Option<FilterReadWindow>,
    pub reveal_cell: Option<(u64, usize)>,
    pub selection: Option<GridSelection>,
    pub cursor: GridCursor,
    pub selected_rows: RowSelection,
    /// Selected filtered-result ordinals (`0..matches_found`) while a filter is active so
    /// Shift-range selection works across viewport buffer boundaries in O(1) memory.
    pub selected_filtered_rows: RowSelection,
    pub selected_columns: BTreeSet<usize>,
    pub column_selection_anchor: Option<usize>,
    pub headers: Vec<String>,
    pub header_renames: Arc<BTreeMap<usize, Arc<String>>>,
    pub header_edit: Option<HeaderEdit>,
    pub cell_edits: SharedCellEdits,
    pub edit_revision: u64,
    pub cell_edit: Option<CellEdit>,
    pub total_columns: usize,
    pub columns: ColumnView,
    pub data_start: u64,
    pub viewport_start: u64,
    pub buffer_start: u64,
    pub buffered_rows: Vec<Arc<Row>>,
    pub whitespace: whitespace::Cache,
    pub whitespace_all: bool,
    pub visible_rows: usize,
    pub scroll_points: f32,
    pub last_viewport_read: Option<Duration>,
    pub last_poll: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderEdit {
    pub column: usize,
    pub draft: String,
    pub focus_requested: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellEdit {
    pub row: u64,
    pub column: usize,
    pub source: Vec<u8>,
    pub draft: String,
    pub focus_requested: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FilterReadWindow {
    pub start_match: u64,
    pub count: usize,
}

pub struct ActiveFilterRead {
    pub tag: FilterResultTag,
    pub window: FilterReadWindow,
    pub job: FilterReadJob,
    pub started: Instant,
    pub cancel_requested: bool,
    pub tail_follow: bool,
}

type BookmarkOffsetBatch = Result<Option<Vec<(u64, u64)>>, QuarryError>;

pub struct BookmarkOffsetJob {
    pub generation: u64,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    rx: std::sync::mpsc::Receiver<BookmarkOffsetBatch>,
}

impl BookmarkOffsetJob {
    pub fn cancel(&self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Drop for BookmarkOffsetJob {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl Document {
    /// Keeps the typed diagnostic and its exact notice text for later details requests.
    pub fn diagnostic_error(&mut self, error: impl Into<QuarryError>) -> String {
        let error = error.into();
        self.pending_diagnostic = Some(Box::new(quarry_core::Diagnostic::from_error(&error)));
        let message = error.to_string();
        self.pending_diagnostic_message = Some(message.clone());
        message
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn open(path: &Path, options: OpenOptions) -> Result<Self, String> {
        let mut document = Self::prepare(path, options)?;
        document.start_indexing()?;
        Ok(document)
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn prepare(path: &Path, options: OpenOptions) -> Result<Self, String> {
        Self::prepare_with_encoding(path, options, None)
    }

    pub fn prepare_with_encoding(
        path: &Path,
        options: OpenOptions,
        encoding: Option<Encoding>,
    ) -> Result<Self, String> {
        Self::prepare_checked(path, options, encoding).map_err(|failure| failure.message)
    }

    /// Like `prepare_with_encoding`, and also says whether Inspect File can examine a failure.
    pub fn prepare_checked(
        path: &Path,
        options: OpenOptions,
        encoding: Option<Encoding>,
    ) -> Result<Self, compressed::OpenFailure> {
        let options = OpenOptions {
            rows: BOOTSTRAP_ROWS + 2 * OVERSCAN_ROWS + 1,
            ..options
        };
        if compressed::is_gzip(path) {
            return Self::prepare_compressed(path, options, encoding);
        }
        let session = Session::open_with_encoding(path, options, encoding).map_err(|error| {
            let inspectable = !matches!(error, QuarryError::CompressedFile { .. })
                && !compressed::compressed_source(path);
            compressed::OpenFailure {
                message: error.to_string(),
                inspectable,
            }
        })?;
        Ok(Self::from_session(session, path))
    }

    /// Prepares a bounded section document (`Session::open_section`); no indexer starts here.
    pub fn prepare_section(
        path: &Path,
        window: ByteWindow,
        mut options: SectionOpenOptions,
    ) -> Result<Self, String> {
        let quote_free = options.quote_free_confirmed;
        options.open.rows = BOOTSTRAP_ROWS + 2 * OVERSCAN_ROWS + 1;
        let session =
            Session::open_section(path, window, options).map_err(|error| error.to_string())?;
        let mut doc = Self::from_session(session, path);
        doc.section_quote_free_confirmed = quote_free;
        Ok(doc)
    }

    /// Prepares a bounded section document at previously verified/snapped `preserved_bounds`
    /// guarded by `expected_guard` and `(expected_delimiter, expected_encoding)`
    /// (`Session::open_section_preserved`).
    pub fn prepare_section_preserved(
        path: &Path,
        preserved_bounds: SectionBounds,
        expected_guard: &quarry_core::SourceGuard,
        expected_delimiter: u8,
        expected_encoding: Encoding,
        mut options: SectionOpenOptions,
    ) -> Result<Self, QuarryError> {
        let quote_free = options.quote_free_confirmed;
        options.open.rows = BOOTSTRAP_ROWS + 2 * OVERSCAN_ROWS + 1;
        let session = Session::open_section_preserved(
            path,
            preserved_bounds,
            expected_guard,
            expected_delimiter,
            expected_encoding,
            options,
        )?;
        let mut doc = Self::from_session(session, path);
        doc.section_quote_free_confirmed = quote_free;
        Ok(doc)
    }

    /// Prepares a bounded section document backed by a private section-only working copy,
    /// preserving the original parent section's bounds, trust, header, and BOM provenance.
    pub fn prepare_section_working_copy(
        working_path: &Path,
        original_parent: &Session,
        mut options: OpenOptions,
        quote_free_confirmed: bool,
    ) -> Result<Self, String> {
        options.rows = BOOTSTRAP_ROWS + 2 * OVERSCAN_ROWS + 1;
        let session = Session::open_section_working_copy(working_path, original_parent, options)
            .map_err(|error| error.to_string())?;
        let mut doc = Self::from_session(session, original_parent.path());
        doc.section_quote_free_confirmed = quote_free_confirmed;
        Ok(doc)
    }

    /// Builds the view over an opened session; no worker starts here.
    pub fn from_session(session: Session, path: &Path) -> Self {
        Self::from_arc_session(Arc::new(session), path)
    }

    /// Builds the view over a shared session (used when waking dormant dirty or compressed tabs).
    pub fn from_arc_session(session: Arc<Session>, path: &Path) -> Self {
        let buffer_rows = BOOTSTRAP_ROWS + 2 * OVERSCAN_ROWS;
        let data_start = u64::from(session.dialect.has_header && !session.section_is_interior());
        let header_cols = session
            .section_header_row()
            .map_or(0, |row| row.fields.len());
        let total_columns = session
            .first_rows
            .iter()
            .map(|row| row.fields.len())
            .max()
            .unwrap_or(0)
            .max(header_cols);
        let columns = ColumnView::new(total_columns);
        let headers = headers_for(&session, &columns.visible);
        let buffered_rows = session
            .first_rows
            .iter()
            .skip(data_start as usize)
            .take(buffer_rows)
            .cloned()
            .map(Arc::new)
            .collect();
        let progress = IndexProgress {
            bytes_scanned: 0,
            bytes_read: 0,
            rows_scanned: 0,
            columns_found: 0,
            file_size: session.active_byte_len(),
            elapsed: Duration::ZERO,
            paused: false,
            done: false,
            cancelled: false,
        };

        Self {
            session,
            compressed: None,
            logical_path: path.to_path_buf(),
            pending_diagnostic: None,
            pending_diagnostic_message: None,
            working_directory: std::env::temp_dir(),
            custom_working_directory: false,
            index_cache_enabled: true,
            working_cleanup: quarry_core::WorkingCopyCleanup::default(),
            decompression_slots: compressed::DecompressionSlots::default(),
            instance_id: NEXT_DOCUMENT_INSTANCE_ID
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            document_id: None,
            join_reservations: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            pending_materialization: None,
            original_session: None,
            working_copy: None,
            edit_history: EditHistory::default(),
            source_generation: 1,
            bookmarks: RowBookmarks::new(),
            bookmark_offset_job: None,
            bookmark_offset_refresh_pending: false,
            job: None,
            index: None,
            progress,
            search_job: None,
            search_query: Vec::new(),
            search_case_sensitivity: CaseSensitivity::Insensitive,
            last_match: None,
            search_history: Vec::new(),
            search_history_limits: Vec::new(),
            search_history_index: None,
            search_status: None,
            search_paused_row_limit: None,
            filter_job: None,
            filter_job_tag: None,
            filter_index: None,
            filter_index_tag: None,
            filter_query: None,
            filter_query_generation: 0,
            filter_refresh_pending: false,
            filter_progress: None,
            filter_status: None,
            filter_paused_row_limit: None,
            export_job: None,
            export_progress: None,
            export_status: None,
            export_cancel_requested: false,
            save_job: None,
            save_output_encoding: None,
            save_status: None,
            save_cancel_requested: false,
            saving_in_place: false,
            last_splice_summary: None,
            structural_job: None,
            structural_status: None,
            structural_cancel_requested: false,
            section_verify_job: None,
            section_verify_progress: None,
            section_verify_status: None,
            verified_bounds_override: None,
            pending_verified_reopen: None,
            section_quote_free_confirmed: false,
            live_watch: None,
            live_watch_filter_attach: None,
            live_watch_filter_attach_tag: None,
            live_watch_snapshot: None,
            live_watch_header_row: None,
            live_watch_alert: None,
            live_watch_tail_follow: false,
            live_watch_async_viewport: false,
            live_watch_viewport_dirty: false,
            live_watch_last_ui_sync: Instant::now() - WATCH_REPAINT_THROTTLE,
            live_watch_last_generation: 0,
            source_changed: false,
            filter_viewport_start: 0,
            filter_buffer_start: 0,
            filtered_rows: Vec::new(),
            filtered_rows_tag: None,
            filter_read: None,
            pending_filter_read: None,
            reveal_cell: None,
            selection: None,
            cursor: GridCursor::default(),
            selected_rows: RowSelection::default(),
            selected_filtered_rows: RowSelection::default(),
            selected_columns: BTreeSet::new(),
            column_selection_anchor: None,
            headers,
            header_renames: Arc::default(),
            header_edit: None,
            cell_edits: Arc::default(),
            edit_revision: 0,
            cell_edit: None,
            total_columns,
            columns,
            data_start,
            viewport_start: data_start,
            buffer_start: data_start,
            buffered_rows,
            whitespace: whitespace::Cache::default(),
            whitespace_all: false,
            visible_rows: BOOTSTRAP_ROWS,
            scroll_points: 0.0,
            last_viewport_read: None,
            last_poll: Instant::now() - POLL_INTERVAL,
        }
    }

    pub fn current_open_options(&self) -> OpenOptions {
        OpenOptions {
            delimiter: Some(self.session.dialect.delimiter),
            header_mode: if self.session.dialect.has_header {
                HeaderMode::FirstRow
            } else {
                HeaderMode::NoHeader
            },
            ..OpenOptions::default()
        }
    }

    fn common_structural_edit_disabled_reason(&self) -> Option<&'static str> {
        if let Some(reason) = self.compressed_read_only_reason() {
            return Some(reason);
        }
        if self.is_live_watching() {
            return Some(LIVE_WATCH_READ_ONLY_REASON);
        }
        if self.join_reserved() {
            return Some(JOIN_INPUT_RESERVED);
        }

        if self.source_changed {
            Some("Reopen the changed source before editing the document.")
        } else if self.filter_active() {
            Some("Clear the filter before editing the document.")
        } else if self.filter_refresh_pending
            || self.save_job.is_some()
            || self.export_job.is_some()
            || self.structural_job.is_some()
            || self.search_job.is_some()
            || self.filter_job.is_some()
        {
            Some("Wait for the active file operation to finish.")
        } else {
            None
        }
    }

    pub fn pending_materialization_confirm_disabled_reason(&self) -> Option<&'static str> {
        if self.is_section() {
            return Some(
                "Column schema, sort, and deduplicate operations are disabled on section documents. Use Save Section As New to edit a standalone copy.",
            );
        }
        self.common_structural_edit_disabled_reason()
    }

    pub fn structural_edit_disabled_reason(&self) -> Option<&'static str> {
        self.pending_materialization_confirm_disabled_reason()
            .or_else(|| {
                self.pending_materialization
                    .is_some()
                    .then_some("Confirm or cancel the pending split first.")
            })
    }

    /// Gating for row-scoped and content-scoped rewrites (`Insert Rows`, `Delete Selected Rows`,
    /// `Replace All`), which are allowed on `Verified` bounded sections in addition to whole files.
    pub fn row_structural_edit_disabled_reason(&self) -> Option<&'static str> {
        if self.is_section() {
            if self.pending_verified_reopen.is_some() {
                return Some(
                    "Reopen this section at the verified boundaries before inserting rows, deleting rows, or running Replace All.",
                );
            }
            if self.section_trust() != Some(BoundaryTrust::Verified) {
                return Some(
                    "Verify section boundaries before inserting rows, deleting rows, or running Replace All.",
                );
            }
        }
        self.common_structural_edit_disabled_reason().or_else(|| {
            self.pending_materialization
                .is_some()
                .then_some("Confirm or cancel the pending split first.")
        })
    }

    pub fn insertion_disabled_reason(&self, rows: bool) -> Option<&'static str> {
        let base_reason = if rows {
            self.row_structural_edit_disabled_reason()
        } else {
            self.structural_edit_disabled_reason()
        };
        base_reason
            .or_else(|| {
                let records = self.index.as_ref()?.indexed_rows();
                if self.data_start > records {
                    Some("This file is empty. Reopen with No header before adding rows.")
                } else if !rows && records == 0 {
                    Some("Add a row first so an empty file can retain its columns.")
                } else {
                    None
                }
            })
            .or_else(|| {
                self.index
                    .is_none()
                    .then_some("Wait for indexing to finish before inserting rows or columns.")
            })
    }

    pub fn row_insertion_from_dialog(
        &self,
        dialog: &StructuralDialog,
    ) -> Result<RowInsertion, String> {
        let rows = self.storage_record_count().saturating_sub(self.data_start);
        let position = parse_insertion_position(&dialog.position, rows)?;
        let count = parse_insertion_count(&dialog.count)?;
        if self.storage_record_count().checked_add(count).is_none() {
            return Err("The row count is too large.".into());
        }
        Ok(RowInsertion {
            position: position + self.data_start,
            count,
            width: self.total_columns.max(1),
        })
    }

    pub fn column_insertion_from_dialog(
        &self,
        dialog: &StructuralDialog,
    ) -> Result<ColumnTransformation, String> {
        let position =
            parse_insertion_position(&dialog.position, self.total_columns as u64)? as usize;
        let count = usize::try_from(parse_insertion_count(&dialog.count)?)
            .map_err(|_| "The column count is too large.")?;
        if self
            .total_columns
            .checked_add(count)
            .is_none_or(|width| width > MAX_TRANSFORMATION_COLUMNS)
        {
            return Err(format!(
                "The result must have at most {MAX_TRANSFORMATION_COLUMNS} columns."
            ));
        }
        let headers = self.session.dialect.has_header.then(|| {
            let mut names = vec![Vec::new(); count];
            names[0] = dialog.separator.as_bytes().to_vec();
            names
        });
        Ok(ColumnTransformation::Insert {
            source_width: self.total_columns,
            position,
            count,
            headers,
        })
    }

    pub fn insert_columns_from_dialog(
        &mut self,
        dialog: &StructuralDialog,
    ) -> Result<(), AppMessage> {
        if let Some(reason) = self.insertion_disabled_reason(false) {
            return Err(AppMessage::warning(reason));
        }
        let transformation = self
            .column_insertion_from_dialog(dialog)
            .map_err(AppMessage::warning)?;
        let ColumnTransformation::Insert {
            position, count, ..
        } = &transformation
        else {
            unreachable!()
        };
        let selected = (*position..position + count).collect();
        self.commit_edits();
        self.cancel_search();
        self.begin_materialization(transformation, selected)
    }

    pub fn insert_rows_from_dialog(&mut self, dialog: &StructuralDialog) -> Result<(), AppMessage> {
        if let Some(reason) = self.insertion_disabled_reason(true) {
            return Err(AppMessage::warning(reason));
        }
        let insertion = self
            .row_insertion_from_dialog(dialog)
            .map_err(AppMessage::warning)?;
        self.commit_edits();
        self.cancel_search();
        let MaterializationPreparation {
            undo,
            destination,
            renames,
        } = self.prepare_materialization()?;
        let job = match self.session.start_create_working_copy_inserting_rows(
            renames,
            self.owned_cell_edits(),
            insertion,
            &destination,
        ) {
            Ok(job) => job,
            Err(QuarryError::SourceChanged) => {
                self.cleanup_empty_working_copy();
                self.invalidate_changed_source();
                return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            }
            Err(error) => {
                self.cleanup_empty_working_copy();
                return Err(AppMessage::core_error(error));
            }
        };
        let job = self.retain_private_job_storage(
            job,
            &destination,
            |job, storage| job.retain_working_storage(storage),
            |job| job.cancel(),
        )?;
        self.structural_job = Some(StructuralJob::InsertingRows {
            job,
            destination,
            undo,
            position: insertion.position,
            count: insertion.count,
        });
        self.structural_status = Some(format!("Inserting {} rows…", insertion.count));
        self.structural_cancel_requested = false;
        Ok(())
    }

    pub fn start_split(
        &mut self,
        source_column: usize,
        separator: Vec<u8>,
    ) -> Result<(), AppMessage> {
        if let Some(reason) = self.structural_edit_disabled_reason() {
            return Err(AppMessage::warning(reason));
        }
        if source_column >= self.total_columns {
            return Err(AppMessage::warning(format!(
                "Column {} is outside this file.",
                source_column.saturating_add(1)
            )));
        }
        if separator.is_empty() {
            return Err(AppMessage::warning("Enter a non-empty separator."));
        }
        self.commit_edits();
        self.cancel_search();
        let max_pieces = MAX_TRANSFORMATION_COLUMNS
            .saturating_sub(self.total_columns)
            .saturating_add(1)
            .min(MAX_TRANSFORMATION_COLUMNS);
        if max_pieces < 2 {
            return Err(AppMessage::warning(format!(
                "Splitting would exceed the {MAX_TRANSFORMATION_COLUMNS}-column file limit."
            )));
        }
        let job = match self.session.start_analyze_split(
            self.owned_cell_edits(),
            source_column,
            separator.clone(),
            max_pieces,
        ) {
            Ok(job) => job,
            Err(QuarryError::SourceChanged) => {
                self.invalidate_changed_source();
                return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            }
            Err(error) => return Err(AppMessage::core_error(error)),
        };
        self.structural_job = Some(StructuralJob::AnalyzingSplit {
            job,
            source_column,
            separator,
        });
        self.structural_status = Some("Checking split width…".into());
        self.structural_cancel_requested = false;
        Ok(())
    }

    pub fn start_combine(
        &mut self,
        source_columns: Vec<usize>,
        separator: Vec<u8>,
    ) -> Result<(), AppMessage> {
        if let Some(reason) = self.structural_edit_disabled_reason() {
            return Err(AppMessage::warning(reason));
        }
        if source_columns.len() < 2 {
            return Err(AppMessage::warning(
                "Select at least two columns to combine.",
            ));
        }
        if source_columns
            .iter()
            .any(|column| *column >= self.total_columns)
        {
            return Err(AppMessage::warning(
                "A selected column is outside this file.",
            ));
        }
        let mut unique = BTreeSet::new();
        if source_columns.iter().any(|column| !unique.insert(*column)) {
            return Err(AppMessage::warning("Select each column only once."));
        }
        self.commit_edits();
        self.cancel_search();
        let output_header = self.session.dialect.has_header.then(|| {
            self.current_header_fields()
                .get(source_columns[0])
                .cloned()
                .unwrap_or_default()
        });
        let insertion = *source_columns
            .iter()
            .min()
            .expect("at least two combine columns were validated");
        self.begin_materialization(
            ColumnTransformation::Join {
                source_columns,
                separator,
                output_header,
            },
            BTreeSet::from([insertion]),
        )
    }

    pub fn start_move_columns(
        &mut self,
        columns: Vec<usize>,
        position: usize,
    ) -> Result<(), AppMessage> {
        let selected =
            validated_column_selection(columns, self.total_columns).map_err(AppMessage::warning)?;
        let remaining = (0..self.total_columns)
            .filter(|column| !selected.contains(column))
            .collect::<Vec<_>>();
        if position > remaining.len() {
            return Err(AppMessage::warning(format!(
                "Destination position must be between 1 and {}.",
                remaining.len().saturating_add(1)
            )));
        }
        let mut output_columns = Vec::with_capacity(self.total_columns);
        output_columns.extend_from_slice(&remaining[..position]);
        output_columns.extend(selected.iter().copied());
        output_columns.extend_from_slice(&remaining[position..]);
        let selected_columns =
            (position..position.saturating_add(selected.len())).collect::<BTreeSet<_>>();
        self.start_arrangement(output_columns, selected_columns)
    }

    pub fn start_delete_columns(&mut self, columns: Vec<usize>) -> Result<(), AppMessage> {
        let selected =
            validated_column_selection(columns, self.total_columns).map_err(AppMessage::warning)?;
        if selected.len() == self.total_columns {
            return Err(AppMessage::warning("At least one column must remain."));
        }
        let output_columns = (0..self.total_columns)
            .filter(|column| !selected.contains(column))
            .collect::<Vec<_>>();
        let first_deleted = *selected
            .iter()
            .next()
            .expect("validated selection is not empty");
        let nearest = first_deleted.min(output_columns.len() - 1);
        self.start_arrangement(output_columns, BTreeSet::from([nearest]))
    }

    pub fn start_delete_rows(&mut self, rows: Vec<RangeInclusive<u64>>) -> Result<(), AppMessage> {
        if let Some(reason) = self.row_structural_edit_disabled_reason() {
            return Err(AppMessage::warning(reason));
        }
        if rows.is_empty() {
            return Err(AppMessage::warning(
                "Select at least one numbered row first.",
            ));
        }
        let count = rows.iter().fold(0_u64, |count, range| {
            count.saturating_add(range.end().saturating_sub(*range.start()).saturating_add(1))
        });
        self.commit_edits();
        self.cancel_search();
        let MaterializationPreparation {
            undo,
            destination,
            renames,
        } = self.prepare_materialization()?;
        let deleted_ranges = rows.clone();
        let job = match self.session.start_create_working_copy_deleting_rows(
            renames,
            self.owned_cell_edits(),
            rows,
            &destination,
        ) {
            Ok(job) => job,
            Err(QuarryError::SourceChanged) => {
                self.cleanup_empty_working_copy();
                self.invalidate_changed_source();
                return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            }
            Err(QuarryError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                self.cleanup_empty_working_copy();
                self.invalidate_changed_source();
                return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            }
            Err(error) => {
                self.cleanup_empty_working_copy();
                return Err(AppMessage::core_error(error));
            }
        };
        let job = self.retain_private_job_storage(
            job,
            &destination,
            |job, storage| job.retain_working_storage(storage),
            |job| job.cancel(),
        )?;
        self.structural_job = Some(StructuralJob::DeletingRows {
            job,
            destination,
            undo,
            deleted_ranges,
            count,
        });
        self.structural_status = Some(format!(
            "Deleting {count} selected row{}…",
            if count == 1 { "" } else { "s" }
        ));
        self.structural_cancel_requested = false;
        Ok(())
    }

    pub fn start_arrangement(
        &mut self,
        output_columns: Vec<usize>,
        selected_columns: BTreeSet<usize>,
    ) -> Result<(), AppMessage> {
        if let Some(reason) = self.structural_edit_disabled_reason() {
            return Err(AppMessage::warning(reason));
        }
        if output_columns.iter().copied().eq(0..self.total_columns) {
            self.structural_status = Some("Columns are already in that position.".into());
            return Ok(());
        }
        self.commit_edits();
        self.cancel_search();
        self.begin_materialization(
            ColumnTransformation::Arrange {
                source_width: self.total_columns,
                output_columns,
            },
            selected_columns,
        )
    }

    #[cfg(any(test, feature = "test-hooks"))]
    /// Keep the existing single-column regression paths on the same plan-based implementation.
    pub fn start_sort_rows(
        &mut self,
        column: usize,
        direction: SortDirection,
        mode: SortMode,
        case_sensitivity: CaseSensitivity,
    ) -> Result<(), AppMessage> {
        self.start_sort_plan(SortPlan::from(SortSpec {
            column,
            direction,
            mode,
            case_sensitivity,
        }))
    }

    /// Run all levels as one structural edit, retaining one undo snapshot.
    pub fn start_sort_plan(&mut self, mut plan: SortPlan) -> Result<(), AppMessage> {
        if let Some(reason) = self.structural_edit_disabled_reason() {
            return Err(AppMessage::warning(reason));
        }
        // Whole-row operations have no user-selectable direction or deterministic seed.
        if plan.keys.len() == 1 && !plan.keys[0].mode.uses_column() {
            plan.keys[0].direction = SortDirection::Ascending;
            if matches!(plan.keys[0].mode, SortMode::Shuffle { .. }) {
                plan.keys[0].mode = SortMode::shuffle();
            }
        }
        plan.validate()
            .map_err(|error| AppMessage::warning(error.to_string()))?;
        if self.sort_plan_disk_estimate(&plan).is_none() {
            return Err(AppMessage::warning(
                "Wait for indexing to finish so Quarry can calculate temporary disk space.",
            ));
        }
        for key in &plan.keys {
            if key.mode.uses_column() {
                self.validate_column(key.column)
                    .map_err(AppMessage::warning)?;
            }
        }
        self.commit_edits();
        self.cancel_search();
        let MaterializationPreparation {
            undo,
            destination,
            renames,
        } = self.prepare_materialization()?;
        let job = match self.session.start_create_sorted_working_copy_with_options(
            renames,
            self.owned_cell_edits(),
            plan.clone(),
            &destination,
            quarry_core::SortOptions {
                temporary_directory: Some(&self.working_directory),
                source_index: self.index.as_ref(),
                threads: None,
                memory_budget_bytes: None,
            },
        ) {
            Ok(job) => job,
            Err(QuarryError::SourceChanged) => {
                self.cleanup_empty_working_copy();
                self.invalidate_changed_source();
                return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            }
            Err(QuarryError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                self.cleanup_empty_working_copy();
                self.invalidate_changed_source();
                return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            }
            Err(error) => {
                self.cleanup_empty_working_copy();
                return Err(AppMessage::core_error(error));
            }
        };
        let job = self.retain_private_job_storage(
            job,
            &destination,
            |job, storage| job.retain_working_storage(storage),
            |job| job.cancel(),
        )?;
        self.structural_job = Some(StructuralJob::Sorting {
            job,
            destination,
            selected_columns: plan.keys.iter().map(|key| key.column).collect(),
            undo,
            plan,
        });
        self.structural_status = Some("Sorting rows…".into());
        self.structural_cancel_requested = false;
        Ok(())
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn start_find_duplicates(
        &mut self,
        columns: Vec<usize>,
        case_sensitivity: CaseSensitivity,
    ) -> Result<(), AppMessage> {
        self.start_find_duplicates_target(DuplicateTarget::Columns(columns), case_sensitivity)
    }

    pub fn start_find_duplicates_target(
        &mut self,
        target: DuplicateTarget,
        case_sensitivity: CaseSensitivity,
    ) -> Result<(), AppMessage> {
        if let Some(reason) = self.structural_edit_disabled_reason() {
            return Err(AppMessage::warning(reason));
        }
        let selected_columns = match &target {
            DuplicateTarget::Columns(columns) => {
                if columns.is_empty() {
                    return Err(AppMessage::warning(
                        "Select at least one numbered column first.",
                    ));
                }
                for &column in columns {
                    self.validate_column(column).map_err(AppMessage::warning)?;
                }
                columns.iter().copied().collect()
            }
            DuplicateTarget::AllFields => BTreeSet::new(),
        };
        self.commit_edits();
        self.cancel_search();
        let MaterializationPreparation {
            undo,
            destination,
            renames,
        } = self.prepare_materialization()?;
        let job = match self.session.start_find_duplicates_with_options(
            renames,
            self.owned_cell_edits(),
            DuplicateSpec {
                target,
                case_sensitivity,
            },
            &destination,
            quarry_core::DuplicateOptions {
                temporary_directory: Some(&self.working_directory),
                source_index: self.index.as_ref(),
                threads: None,
                memory_budget_bytes: None,
            },
        ) {
            Ok(job) => job,
            Err(error) => {
                self.cleanup_empty_working_copy();
                if matches!(error, QuarryError::SourceChanged)
                    || matches!(&error, QuarryError::Io(error) if error.kind() == std::io::ErrorKind::NotFound)
                {
                    self.invalidate_changed_source();
                    return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
                }
                return Err(AppMessage::core_error(error));
            }
        };
        let job = self.retain_private_job_storage(
            job,
            &destination,
            |job, storage| job.retain_working_storage(storage),
            |job| job.cancel(),
        )?;
        self.structural_job = Some(StructuralJob::FindingDuplicates {
            job,
            destination,
            selected_columns,
            undo,
        });
        self.structural_status = Some("Finding duplicate rows…".into());
        self.structural_cancel_requested = false;
        Ok(())
    }

    /// Sample bounded rows and bytes of effective (edited) values for `column` without cloning
    /// the document's edit overlay.
    pub fn suggest_date_time_formats(&self, column: usize) -> Option<DateFormatSuggestion> {
        let active_edit = self
            .cell_edit
            .as_ref()
            .map(|edit| ((edit.row, edit.column), edit.draft.as_bytes()));
        self.session
            .suggest_date_time_formats_with_lookup(
                column,
                |row, col| {
                    if let Some(((er, ec), bytes)) = active_edit
                        && er == row
                        && ec == col
                    {
                        return Some(bytes);
                    }
                    self.cell_edits
                        .get(&(row, col))
                        .map(|value| value.as_slice())
                },
                DateFormatSuggestionConfig::default(),
            )
            .ok()
    }

    /// Dialog frames borrow effective values; only actual core jobs need owned snapshots.
    #[cfg(any(test, feature = "test-hooks"))]
    pub fn sort_temporary_disk_estimate(&self) -> Option<u64> {
        self.sort_plan_disk_estimate(&SortPlan::from(SortSpec {
            column: 0,
            mode: SortMode::Text,
            direction: SortDirection::Ascending,
            case_sensitivity: CaseSensitivity::Sensitive,
        }))
    }

    /// Include all comparison levels while borrowing the current edit overlay.
    pub fn sort_plan_disk_estimate(&self, plan: &SortPlan) -> Option<u64> {
        let records = self.index.as_ref()?.indexed_rows();
        let mut headers: BTreeMap<_, _> = self
            .header_renames
            .iter()
            .map(|(column, value)| (*column, value.as_bytes()))
            .collect();
        let mut cells: BTreeMap<_, _> = self
            .cell_edits
            .iter()
            .map(|(key, value)| (*key, value.as_slice()))
            .collect();
        if let Some(edit) = &self.header_edit {
            headers.insert(edit.column, edit.draft.as_bytes());
        }
        if let Some(edit) = &self.cell_edit {
            cells.insert((edit.row, edit.column), edit.draft.as_bytes());
        }
        Some(quarry_core::estimate_sort_plan_temporary_bytes(
            quarry_core::estimate_edited_output_bytes_from_values(
                self.session.file_size.saturating_mul(
                    if self.session.encoding() == Encoding::Utf8 {
                        1
                    } else {
                        3
                    },
                ),
                records,
                headers,
                cells,
                None,
                None,
            ),
            records.saturating_sub(self.data_start),
            plan,
        ))
    }

    pub fn prepare_materialization(&mut self) -> Result<MaterializationPreparation, AppMessage> {
        if self.original_session.is_none() {
            match self.session.ensure_source_unchanged() {
                Ok(()) => {}
                Err(QuarryError::SourceChanged) => {
                    self.invalidate_changed_source();
                    return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
                }
                Err(error) => return Err(AppMessage::core_error(error)),
            }
            let original = if let Some(bounds) = self.section_bounds() {
                let sec_opts = SectionOpenOptions {
                    open: self.current_open_options(),
                    encoding: Some(self.session.encoding()),
                    parent_index: None,
                    quote_free_confirmed: self.section_quote_free_confirmed,
                    ..SectionOpenOptions::default()
                };
                match Session::open_section_preserved(
                    &self.logical_path,
                    bounds,
                    &self.session.source_guard(),
                    self.session.dialect.delimiter,
                    self.session.encoding(),
                    sec_opts,
                ) {
                    Ok(original) => original,
                    Err(QuarryError::SourceChanged) => {
                        self.invalidate_changed_source();
                        return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
                    }
                    Err(QuarryError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                        self.invalidate_changed_source();
                        return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
                    }
                    Err(error) => return Err(AppMessage::core_error(error)),
                }
            } else {
                match Session::open_with_encoding(
                    &self.logical_path,
                    self.current_open_options(),
                    Some(self.session.encoding()),
                ) {
                    Ok(original) => original,
                    Err(QuarryError::SourceChanged) => {
                        self.invalidate_changed_source();
                        return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
                    }
                    Err(QuarryError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                        self.invalidate_changed_source();
                        return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
                    }
                    Err(error) => return Err(AppMessage::core_error(error)),
                }
            };
            self.original_session = Some(original);
        }
        if self.custom_working_directory {
            quarry_core::validate_scratch_directory(&self.working_directory).map_err(|error| {
                AppMessage::error(format_unavailable_scratch_notice(
                    &self.working_directory,
                    &error.to_string(),
                ))
            })?;
        }
        if self.working_copy.is_none() {
            self.working_copy = Some(
                WorkingCopyState::new_in_with_cleanup(
                    &self.working_directory,
                    self.working_cleanup.clone(),
                )
                .map_err(AppMessage::error)?,
            );
        }
        let undo = WorkingCopySnapshot {
            path: self.session.path().to_path_buf(),
            overlay: StructuralOverlay {
                header_renames: self.header_renames.as_ref().clone(),
                cell_edits: self.cell_edits.as_ref().clone(),
            },
            edit_history: self.edit_history.clone(),
            source_generation: self.source_generation,
            bookmarks: self.bookmarks.clone(),
        };
        let state = self
            .working_copy
            .as_mut()
            .expect("the working-copy state was created");
        state
            .use_directory(&self.working_directory)
            .map_err(AppMessage::error)?;
        state.prune_directories(self.session.path());
        let destination = state.next_path().map_err(AppMessage::error)?;
        let renames = self
            .header_renames
            .iter()
            .map(|(column, name)| (*column, name.as_bytes().to_vec()))
            .collect();
        Ok(MaterializationPreparation {
            undo,
            destination,
            renames,
        })
    }

    pub fn begin_materialization(
        &mut self,
        transformation: ColumnTransformation,
        selected_columns: BTreeSet<usize>,
    ) -> Result<(), AppMessage> {
        let MaterializationPreparation {
            undo,
            destination,
            renames,
        } = self.prepare_materialization()?;
        let job = match self.session.start_create_working_copy(
            renames,
            self.owned_cell_edits(),
            transformation,
            destination.clone(),
        ) {
            Ok(job) => job,
            Err(QuarryError::SourceChanged) => {
                self.cleanup_empty_working_copy();
                self.invalidate_changed_source();
                return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            }
            Err(QuarryError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                self.cleanup_empty_working_copy();
                self.invalidate_changed_source();
                return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            }
            Err(error) => {
                self.cleanup_empty_working_copy();
                return Err(AppMessage::core_error(error));
            }
        };
        let job = self.retain_private_job_storage(
            job,
            &destination,
            |job, storage| job.retain_working_storage(storage),
            |job| job.cancel(),
        )?;
        self.structural_job = Some(StructuralJob::Materializing {
            job,
            destination,
            selected_columns,
            undo,
        });
        self.structural_status = Some("Applying column edit…".into());
        self.structural_cancel_requested = false;
        Ok(())
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn start_replace_all(&mut self, query: &[u8], replacement: &[u8]) -> Result<(), String> {
        self.start_replace_all_with_case(query, replacement, CaseSensitivity::Insensitive)
            .map_err(|message| message.text)
    }

    pub fn start_cleanup(&mut self, cleanup: ColumnCleanup) -> Result<(), AppMessage> {
        if let Some(reason) = self.structural_edit_disabled_reason() {
            return Err(AppMessage::warning(reason));
        }
        cleanup
            .validate()
            .map_err(|e| AppMessage::warning(e.to_string()))?;
        for &column in &cleanup.columns {
            self.validate_column(column).map_err(AppMessage::warning)?;
        }
        self.commit_edits();
        self.cancel_search();
        let selected_columns = cleanup.columns.iter().copied().collect();
        let MaterializationPreparation {
            undo,
            destination,
            renames,
        } = self.prepare_materialization()?;
        let job = match self.session.start_create_cleaned_working_copy(
            renames,
            self.owned_cell_edits(),
            cleanup,
            destination.clone(),
        ) {
            Ok(job) => job,
            Err(error) => {
                self.cleanup_empty_working_copy();
                if matches!(error, QuarryError::SourceChanged) {
                    self.invalidate_changed_source();
                    return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
                }
                return Err(AppMessage::core_error(error));
            }
        };
        let job = self.retain_private_job_storage(
            job,
            &destination,
            |job, storage| job.retain_working_storage(storage),
            |job| job.cancel(),
        )?;
        self.structural_job = Some(StructuralJob::Cleaning {
            job,
            destination,
            selected_columns,
            undo,
        });
        self.structural_status = Some("Cleaning columns…".into());
        self.structural_cancel_requested = false;
        Ok(())
    }

    pub fn start_replace_all_with_case(
        &mut self,
        query: &[u8],
        replacement: &[u8],
        case_sensitivity: CaseSensitivity,
    ) -> Result<(), AppMessage> {
        if let Some(reason) = self.row_structural_edit_disabled_reason() {
            return Err(AppMessage::warning(reason));
        }
        if query.is_empty() {
            return Err(AppMessage::warning("Enter text to find."));
        }
        if query == replacement {
            self.structural_status = Some(
                "Search and replacement text are identical. The document was not changed.".into(),
            );
            return Ok(());
        }
        self.commit_edits();
        self.cancel_search();
        let selected_columns = self.selected_columns.clone();
        let MaterializationPreparation {
            undo,
            destination,
            renames,
        } = self.prepare_materialization()?;
        let job = match self
            .session
            .start_create_replaced_working_copy_with_options(
                renames,
                self.owned_cell_edits(),
                LiteralReplacement {
                    needle: query.to_vec(),
                    replacement: replacement.to_vec(),
                    case_sensitivity,
                },
                destination.clone(),
                quarry_core::ReplaceAllOptions {
                    source_index: self.index.as_ref(),
                    threads: None,
                },
            ) {
            Ok(job) => job,
            Err(QuarryError::SourceChanged) => {
                self.cleanup_empty_working_copy();
                self.invalidate_changed_source();
                return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            }
            Err(QuarryError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                self.cleanup_empty_working_copy();
                self.invalidate_changed_source();
                return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            }
            Err(error) => {
                self.cleanup_empty_working_copy();
                return Err(AppMessage::core_error(error));
            }
        };
        let job = self.retain_private_job_storage(
            job,
            &destination,
            |job, storage| job.retain_working_storage(storage),
            |job| job.cancel(),
        )?;
        self.structural_job = Some(StructuralJob::Replacing {
            job,
            destination,
            selected_columns,
            undo,
        });
        self.structural_status = Some("Replacing matches…".into());
        self.structural_cancel_requested = false;
        Ok(())
    }

    pub fn accept_materialized_change(&mut self, mut undo: WorkingCopySnapshot) {
        undo.edit_history.clear_redo();
        let state = self
            .working_copy
            .as_mut()
            .expect("materialization owns a working-copy directory");
        let current_path = self.session.path().to_path_buf();
        for obsolete in [state.undo.take(), state.redo.take()].into_iter().flatten() {
            if obsolete.path != current_path
                && state.owns(&obsolete.path)
                && let Err(error) = state.remove(&obsolete.path)
            {
                self.working_cleanup.report(error);
            }
        }
        debug_assert_eq!(undo.path, current_path);
        state.undo = Some(undo);
        state.redo = None;
        state.prune_directories(&current_path);
        self.structural_status = None;
    }

    pub fn poll_structural_edit(&mut self) -> Result<Option<MaterializedWorkingCopy>, AppMessage> {
        let Some(job) = self.structural_job.as_ref() else {
            return Ok(None);
        };
        let done = match job {
            StructuralJob::Cleaning { job, .. } => job.progress().done,
            StructuralJob::FindingDuplicates { job, .. } => job.progress().done,
            StructuralJob::DuplicatePreview { .. } => false,
            StructuralJob::AnalyzingSplit { job, .. } => job.progress().done,
            StructuralJob::Materializing { job, .. } => job.progress().done,
            StructuralJob::Replacing { job, .. } => job.progress().done,
            StructuralJob::Sorting { job, .. } => job.progress().done,
            StructuralJob::DeletingRows { job, .. } | StructuralJob::InsertingRows { job, .. } => {
                job.progress().done
            }
        };
        if !done {
            return Ok(None);
        }

        let job = self
            .structural_job
            .take()
            .expect("the completed structural job is present");
        let cancel_requested = self.structural_cancel_requested;
        self.structural_cancel_requested = false;
        match job {
            StructuralJob::FindingDuplicates {
                job,
                destination,
                selected_columns,
                undo,
            } => {
                match job.wait() {
                    Ok(DuplicateOutcome::Complete(summary))
                        if !cancel_requested && summary.duplicate_rows > 0 =>
                    {
                        self.structural_status = Some(format!(
                            "Review {} extra duplicate rows before removal.",
                            summary.duplicate_rows
                        ));
                        self.structural_job = Some(StructuralJob::DuplicatePreview {
                            summary,
                            selected_columns,
                            undo,
                        });
                    }
                    Ok(_) => {
                        self.remove_working_file(&destination);
                        self.cleanup_empty_working_copy();
                        self.structural_status = Some(
                            if cancel_requested {
                                "Duplicate search cancelled. The document was not changed."
                            } else {
                                "No duplicates found. The document was not changed."
                            }
                            .into(),
                        );
                    }
                    Err(error) => {
                        self.remove_working_file(&destination);
                        self.cleanup_empty_working_copy();
                        self.structural_status =
                            Some("Duplicate search failed. The document was not changed.".into());
                        if matches!(error, QuarryError::SourceChanged) {
                            self.remove_working_file(&destination);
                            self.invalidate_changed_source();
                            return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
                        }
                        return Err(AppMessage::core_error(error));
                    }
                }
                Ok(None)
            }
            StructuralJob::DuplicatePreview { .. } => {
                unreachable!("a preview waits for explicit removal")
            }
            StructuralJob::AnalyzingSplit {
                job,
                source_column,
                separator,
            } => match job.wait() {
                Ok(SplitAnalysisOutcome::Complete(_)) if cancel_requested => {
                    self.structural_status =
                        Some("Split cancelled. The document was not changed.".into());
                    Ok(None)
                }
                Ok(SplitAnalysisOutcome::Complete(summary)) if summary.max_pieces >= 2 => {
                    let source_header = self.session.dialect.has_header.then(|| {
                        self.current_header_fields()
                            .get(source_column)
                            .cloned()
                            .unwrap_or_default()
                    });
                    let transformation = ColumnTransformation::split_with_blank_headers(
                        source_column,
                        separator,
                        summary.max_pieces,
                        source_header,
                    )
                    .map_err(AppMessage::core_error)?;
                    let selected_columns =
                        (source_column..source_column.saturating_add(summary.max_pieces)).collect();
                    let records = summary.rows_scanned.saturating_add(self.data_start);
                    if self.materialization_storage_estimate_for_records(
                        Some(&transformation),
                        None,
                        records,
                    ) >= STORAGE_REVIEW_BYTES
                    {
                        self.pending_materialization =
                            Some((transformation, selected_columns, records));
                    } else {
                        self.begin_materialization(transformation, selected_columns)?;
                    }
                    Ok(None)
                }
                Ok(SplitAnalysisOutcome::Complete(_)) => {
                    self.structural_status = None;
                    Err(AppMessage::warning(
                        "The separator was not found in the selected column.",
                    ))
                }
                Ok(SplitAnalysisOutcome::Cancelled) => {
                    self.structural_status =
                        Some("Split cancelled. The document was not changed.".into());
                    Ok(None)
                }
                Err(QuarryError::SourceChanged) => {
                    self.invalidate_changed_source();
                    Err(AppMessage::warning(SOURCE_CHANGED_NOTICE))
                }
                Err(error) => {
                    self.structural_status =
                        Some("Split failed. The document was not changed.".into());
                    Err(AppMessage::core_error(error))
                }
            },
            StructuralJob::Cleaning {
                job,
                destination,
                selected_columns,
                undo,
            } => match job.wait() {
                Ok(CleanupOutcome::Complete(summary)) => Ok(Some(MaterializedWorkingCopy {
                    path: summary.destination,
                    output_index: None,
                    undo,
                    selected_columns,
                    notice: format!(
                        "Cleaned {} cell{}. Save to keep it, or discard changes.",
                        summary.changed_cells,
                        if summary.changed_cells == 1 { "" } else { "s" }
                    ),
                    bookmark_transform: BookmarkTransform::PreserveSameRows,
                })),
                Ok(outcome @ (CleanupOutcome::Unchanged | CleanupOutcome::Cancelled)) => {
                    self.remove_working_file(&destination);
                    self.cleanup_empty_working_copy();
                    self.structural_status = Some(
                        if matches!(outcome, CleanupOutcome::Unchanged) {
                            "No cells changed. The document was not changed."
                        } else {
                            "Cleanup cancelled. The document was not changed."
                        }
                        .into(),
                    );
                    Ok(None)
                }
                Err(error) => {
                    self.remove_working_file(&destination);
                    self.cleanup_empty_working_copy();
                    if matches!(error, QuarryError::SourceChanged) {
                        self.invalidate_changed_source();
                        return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
                    }
                    Err(AppMessage::core_error(error))
                }
            },
            StructuralJob::Materializing {
                job,
                destination,
                selected_columns,
                undo,
            } => match job.wait() {
                Ok(SaveAsOutcome::Complete(summary)) => Ok(Some(MaterializedWorkingCopy {
                    path: summary.destination,
                    output_index: None,
                    undo,
                    selected_columns,
                    notice: "Column edit applied. Save to keep it, or discard changes.".into(),
                    bookmark_transform: BookmarkTransform::PreserveSameRows,
                })),
                Ok(SaveAsOutcome::Cancelled) => {
                    self.remove_working_file(&destination);
                    self.cleanup_empty_working_copy();
                    self.structural_status =
                        Some("Column edit cancelled. The document was not changed.".into());
                    Ok(None)
                }
                Err(QuarryError::SourceChanged) => {
                    self.remove_working_file(&destination);
                    self.invalidate_changed_source();
                    Err(AppMessage::warning(SOURCE_CHANGED_NOTICE))
                }
                Err(error) => {
                    self.remove_working_file(&destination);
                    self.cleanup_empty_working_copy();
                    self.structural_status =
                        Some("Column edit failed. The document was not changed.".into());
                    Err(AppMessage::core_error(error))
                }
            },
            StructuralJob::InsertingRows {
                job,
                destination,
                undo,
                position,
                count,
            } => match job.wait() {
                Ok(SaveAsOutcome::Complete(summary)) => Ok(Some(MaterializedWorkingCopy {
                    path: summary.destination,
                    output_index: None,
                    undo,
                    selected_columns: BTreeSet::new(),
                    notice: format!(
                        "Inserted {count} row{}. Save to keep it, or discard changes.",
                        if count == 1 { "" } else { "s" }
                    ),
                    bookmark_transform: BookmarkTransform::ShiftInsertRows {
                        before_row: position,
                        count,
                    },
                })),
                Ok(SaveAsOutcome::Cancelled) => {
                    self.remove_working_file(&destination);
                    self.cleanup_empty_working_copy();
                    self.structural_status =
                        Some("Row insertion cancelled. The document was not changed.".into());
                    Ok(None)
                }
                Err(QuarryError::SourceChanged) => {
                    self.remove_working_file(&destination);
                    self.invalidate_changed_source();
                    Err(AppMessage::warning(SOURCE_CHANGED_NOTICE))
                }
                Err(error) => {
                    self.remove_working_file(&destination);
                    self.cleanup_empty_working_copy();
                    self.structural_status =
                        Some("Row insertion failed. The document was not changed.".into());
                    Err(AppMessage::core_error(error))
                }
            },
            StructuralJob::DeletingRows {
                job,
                destination,
                undo,
                deleted_ranges,
                count,
            } => match job.wait() {
                Ok(SaveAsOutcome::Complete(summary)) => Ok(Some(MaterializedWorkingCopy {
                    path: summary.destination,
                    output_index: None,
                    undo,
                    selected_columns: BTreeSet::new(),
                    notice: format!(
                        "Deleted {count} row{}. Save to keep it, or discard changes.",
                        if count == 1 { "" } else { "s" }
                    ),
                    bookmark_transform: BookmarkTransform::ShiftDeleteRows { deleted_ranges },
                })),
                Ok(SaveAsOutcome::Cancelled) => {
                    self.remove_working_file(&destination);
                    self.cleanup_empty_working_copy();
                    self.structural_status =
                        Some("Row deletion cancelled. The document was not changed.".into());
                    Ok(None)
                }
                Err(QuarryError::SourceChanged) => {
                    self.remove_working_file(&destination);
                    self.invalidate_changed_source();
                    Err(AppMessage::warning(SOURCE_CHANGED_NOTICE))
                }
                Err(error) => {
                    self.remove_working_file(&destination);
                    self.cleanup_empty_working_copy();
                    self.structural_status =
                        Some("Row deletion failed. The document was not changed.".into());
                    Err(AppMessage::core_error(error))
                }
            },
            StructuralJob::Replacing {
                job,
                destination,
                selected_columns,
                undo,
            } => match job.wait() {
                Ok(ReplaceAllOutcome::Complete(summary)) => {
                    let count = summary.replacements;
                    Ok(Some(MaterializedWorkingCopy {
                        path: summary.destination,
                        output_index: summary.output_index,
                        undo,
                        selected_columns,
                        notice: format!(
                            "Replaced {count} occurrence{}. Save to keep it, or discard changes.",
                            if count == 1 { "" } else { "s" }
                        ),
                        bookmark_transform: BookmarkTransform::PreserveSameRows,
                    }))
                }
                Ok(ReplaceAllOutcome::NoMatch) => {
                    self.remove_working_file(&destination);
                    self.cleanup_empty_working_copy();
                    self.structural_status =
                        Some("No matches found. The document was not changed.".into());
                    Ok(None)
                }
                Ok(ReplaceAllOutcome::Cancelled) => {
                    self.remove_working_file(&destination);
                    self.cleanup_empty_working_copy();
                    self.structural_status =
                        Some("Replace All cancelled. The document was not changed.".into());
                    Ok(None)
                }
                Err(QuarryError::SourceChanged) => {
                    self.remove_working_file(&destination);
                    self.invalidate_changed_source();
                    Err(AppMessage::warning(SOURCE_CHANGED_NOTICE))
                }
                Err(error) => {
                    self.remove_working_file(&destination);
                    self.cleanup_empty_working_copy();
                    self.structural_status =
                        Some("Replace All failed. The document was not changed.".into());
                    Err(AppMessage::core_error(error))
                }
            },
            StructuralJob::Sorting {
                job,
                destination,
                selected_columns,
                undo,
                plan,
            } => match job.wait() {
                Ok(SortOutcome::Complete(summary)) => {
                    let operation = match plan.keys[0].mode {
                        SortMode::Shuffle { .. } => "Shuffled rows.".to_owned(),
                        SortMode::Reverse => "Reversed row order.".to_owned(),
                        _ => format!(
                            "Sorted rows by {}.",
                            plan.keys
                                .iter()
                                .map(|key| format!(
                                    "column {} {}",
                                    key.column.saturating_add(1),
                                    sort_direction_label(key.mode, key.direction).to_lowercase()
                                ))
                                .collect::<Vec<_>>()
                                .join(", then ")
                        ),
                    };
                    Ok(Some(MaterializedWorkingCopy {
                        path: summary.destination,
                        output_index: summary.output_index,
                        undo,
                        selected_columns,
                        notice: format!("{operation} Save to keep it, or discard changes."),
                        bookmark_transform: BookmarkTransform::ClearRowOrderChanged,
                    }))
                }
                Ok(SortOutcome::Cancelled) => {
                    self.remove_working_file(&destination);
                    self.cleanup_empty_working_copy();
                    self.structural_status =
                        Some("Sort cancelled. The document was not changed.".into());
                    Ok(None)
                }
                Err(QuarryError::SourceChanged) => {
                    self.remove_working_file(&destination);
                    self.invalidate_changed_source();
                    Err(AppMessage::warning(SOURCE_CHANGED_NOTICE))
                }
                Err(error) => {
                    self.remove_working_file(&destination);
                    self.cleanup_empty_working_copy();
                    self.structural_status =
                        Some("Sort failed. The document was not changed.".into());
                    Err(AppMessage::core_error(error))
                }
            },
        }
    }

    pub fn cancel_structural_edit(&mut self) {
        if matches!(
            self.structural_job,
            Some(StructuralJob::DuplicatePreview { .. })
        ) {
            if let Some(StructuralJob::DuplicatePreview { summary, .. }) =
                self.structural_job.take()
            {
                self.remove_working_file(&summary.destination);
            }
            self.cleanup_empty_working_copy();
            self.structural_cancel_requested = false;
            self.structural_status =
                Some("Duplicate removal cancelled. The document was not changed.".into());
            return;
        }
        let Some(job) = self.structural_job.as_ref() else {
            return;
        };
        match job {
            StructuralJob::FindingDuplicates { job, .. } => job.cancel(),
            StructuralJob::DuplicatePreview { .. } => {
                unreachable!("preview cancellation handled above")
            }
            StructuralJob::AnalyzingSplit { job, .. } => job.cancel(),
            StructuralJob::Materializing { job, .. } => job.cancel(),
            StructuralJob::Replacing { job, .. } => job.cancel(),
            StructuralJob::Cleaning { job, .. } => job.cancel(),
            StructuralJob::Sorting { job, .. } => job.cancel(),
            StructuralJob::DeletingRows { job, .. } | StructuralJob::InsertingRows { job, .. } => {
                job.cancel()
            }
        }
        self.structural_cancel_requested = true;
        self.structural_status = Some("Cancelling change…".into());
    }

    pub fn reserved_working_destination(&self) -> Option<&Path> {
        match self.structural_job.as_ref()? {
            StructuralJob::FindingDuplicates { destination, .. }
            | StructuralJob::Sorting { destination, .. }
            | StructuralJob::Materializing { destination, .. }
            | StructuralJob::Replacing { destination, .. }
            | StructuralJob::Cleaning { destination, .. }
            | StructuralJob::InsertingRows { destination, .. }
            | StructuralJob::DeletingRows { destination, .. } => Some(destination),
            StructuralJob::DuplicatePreview { summary, .. } => Some(&summary.destination),
            StructuralJob::AnalyzingSplit { .. } => None,
        }
    }

    pub fn retain_private_job_storage<J>(
        &mut self,
        job: J,
        destination: &Path,
        retain: impl FnOnce(&J, Arc<quarry_core::WorkingCopyStorage>) -> Result<(), QuarryError>,
        cancel: impl FnOnce(&J),
    ) -> Result<J, AppMessage> {
        let storage = Arc::clone(
            &self
                .working_copy
                .as_ref()
                .expect("working storage prepared")
                .directory,
        );
        if let Err(error) = retain(&job, storage) {
            // Reap the worker before releasing a possibly final directory owner.
            cancel(&job);
            drop(job);
            self.remove_working_file(destination);
            self.cleanup_empty_working_copy();
            return Err(AppMessage::core_error(error));
        }
        Ok(job)
    }

    pub fn remove_working_file(&self, path: &Path) {
        if let Some(state) = &self.working_copy
            && let Err(error) = state.remove(path)
        {
            self.working_cleanup.report(error);
        }
    }

    pub fn cleanup_empty_working_copy(&mut self) {
        let empty = self.working_copy.as_ref().is_some_and(|state| {
            state.undo.is_none() && state.redo.is_none() && self.session.path() == self.logical_path
        });
        if empty {
            self.working_copy = None;
            self.original_session = None;
        }
    }

    pub fn invalidate_structural_redo(&mut self) {
        let Some(state) = self.working_copy.as_mut() else {
            return;
        };
        let Some(redo) = state.redo.take() else {
            return;
        };
        if redo.path != self.session.path()
            && state.owns(&redo.path)
            && let Err(error) = state.remove(&redo.path)
        {
            self.working_cleanup.report(error);
        }
        state.prune_directories(self.session.path());
    }

    pub fn structural_progress(&self) -> Option<StructuralProgressDisplay> {
        let job = self.structural_job.as_ref()?;
        let (bytes_scanned, total_bytes, done, operation) = match job {
            StructuralJob::FindingDuplicates { job, .. } => {
                let progress = job.progress();
                if !progress.done && progress.bytes_scanned >= progress.total_bytes {
                    return Some(StructuralProgressDisplay {
                        fraction: 0.9,
                        label: "Finding duplicates · merging records…".into(),
                        animate: true,
                    });
                }
                (
                    progress.bytes_scanned,
                    progress.total_bytes,
                    progress.done,
                    "Finding duplicates",
                )
            }
            StructuralJob::DuplicatePreview { .. } => {
                return Some(StructuralProgressDisplay {
                    fraction: 1.0,
                    label: "Review duplicates before removal".into(),
                    animate: false,
                });
            }
            StructuralJob::AnalyzingSplit { job, .. } => {
                let progress = job.progress();
                (
                    progress.bytes_scanned,
                    progress.total_bytes,
                    progress.done,
                    "Checking split width",
                )
            }
            StructuralJob::Materializing { job, .. } => {
                return Some(materialization_progress(
                    job.progress(),
                    "Applying column edit",
                ));
            }
            StructuralJob::Cleaning { job, .. } => {
                return Some(materialization_progress(job.progress(), "Cleaning columns"));
            }
            StructuralJob::Replacing { job, .. } => {
                let progress = job.progress();
                (
                    progress.bytes_scanned,
                    progress.total_bytes,
                    progress.done,
                    "Replacing matches",
                )
            }
            StructuralJob::Sorting { job, .. } => {
                let progress = job.progress();
                return Some(sort_progress(
                    progress.preparing,
                    progress.preparation_bytes_scanned,
                    progress.bytes_scanned,
                    progress.total_bytes,
                    progress.done,
                ));
            }
            StructuralJob::InsertingRows { job, .. } => {
                return Some(materialization_progress(job.progress(), "Inserting rows"));
            }
            StructuralJob::DeletingRows { job, .. } => {
                let progress = job.progress();
                (
                    progress.bytes_scanned,
                    progress.total_bytes,
                    progress.done,
                    "Deleting selected rows",
                )
            }
        };
        let fraction = if total_bytes == 0 {
            if done { 1.0 } else { 0.0 }
        } else {
            (bytes_scanned as f32 / total_bytes as f32).clamp(0.0, 1.0)
        };
        Some(StructuralProgressDisplay {
            fraction,
            label: format!("{operation} · {:.1}%", fraction * 100.0),
            animate: false,
        })
    }

    pub fn start_indexing(&mut self) -> Result<(), String> {
        if self.start_decompression()? {
            return Ok(());
        }
        let cache_config = if self.index_cache_enabled {
            quarry_core::IndexCacheConfig::default()
        } else {
            quarry_core::IndexCacheConfig::disabled()
        };
        let job = self
            .session
            .start_indexing_with_options(IndexConfig::default(), None, cache_config)
            .map_err(|error| self.diagnostic_error(error))?;
        self.progress = job.progress();
        self.job = Some(job);
        self.last_poll = Instant::now() - POLL_INTERVAL;
        Ok(())
    }

    pub fn is_indexing_paused(&self) -> bool {
        self.job.as_ref().is_some_and(IndexJob::is_paused)
    }

    pub fn is_indexing_pausing(&self) -> bool {
        self.job
            .as_ref()
            .is_some_and(|job| job.is_pause_requested() && !job.is_paused())
    }

    pub fn is_indexing_pause_pinned(&self) -> bool {
        self.job
            .as_ref()
            .is_some_and(IndexJob::is_paused_or_pausing)
    }

    pub fn can_pause_indexing(&self) -> bool {
        !self.is_compressed()
            && self.job.is_some()
            && !self.progress.done
            && !self.is_indexing_paused()
            && !self.is_indexing_pausing()
            && !self.saving_in_place
            && !self.source_changed
    }

    pub fn can_resume_indexing(&self) -> bool {
        !self.is_compressed()
            && self.job.is_some()
            && !self.progress.done
            && self.is_indexing_paused()
            && !self.saving_in_place
            && !self.source_changed
    }

    pub fn committed_data_rows(&self) -> u64 {
        self.progress.rows_scanned.saturating_sub(self.data_start)
    }

    pub fn is_live_watching(&self) -> bool {
        self.live_watch.is_some()
    }

    pub fn is_filter_scanning(&self) -> bool {
        self.filter_job.is_some() || self.live_watch_filter_attach.is_some()
    }

    pub fn live_watch_fd_count(&self) -> usize {
        self.live_watch
            .as_ref()
            .map_or(0, LiveWatchJob::held_fd_count)
    }

    pub fn live_watch_unavailable_reason(&self) -> Option<&'static str> {
        if self.is_compressed() {
            return Some("Live Watch Mode is unavailable for compressed (.gz) files.");
        }
        if self.is_section() {
            return Some(
                "Live Watch Mode is only available for whole files, not bounded sections.",
            );
        }
        if self.source_changed {
            return Some("Reload the changed source before starting Live Watch Mode.");
        }
        if self.is_dirty() {
            return Some("Save or discard unsaved changes before starting Live Watch Mode.");
        }
        if self.join_reserved() {
            return Some(JOIN_INPUT_RESERVED);
        }
        if self.save_job.is_some()
            || self.export_job.is_some()
            || self.structural_job.is_some()
            || self.search_job.is_some()
            || self.is_filter_scanning()
            || self.pending_materialization.is_some()
        {
            return Some(
                "Wait for active file operations to finish before starting Live Watch Mode.",
            );
        }
        if self.filter_active()
            && (self.filter_refresh_pending
                || self.filter_paused_row_limit.is_some()
                || self
                    .filter_progress
                    .is_some_and(|progress| !progress.done || progress.cancelled)
                || self
                    .filter_index
                    .as_ref()
                    .is_none_or(|idx| !idx.is_complete()))
        {
            return Some("Finish or clear the incomplete filter before starting Live Watch Mode.");
        }
        if self.job.is_some() || self.index.as_ref().is_none_or(|idx| !idx.is_complete()) {
            return Some("Wait for initial indexing to finish before starting Live Watch Mode.");
        }
        None
    }

    pub fn can_start_live_watch(&self) -> bool {
        !self.is_live_watching() && self.live_watch_unavailable_reason().is_none()
    }

    pub fn start_live_watch(&mut self) -> Result<(), AppMessage> {
        let cache_config = if self.index_cache_enabled {
            IndexCacheConfig::default()
        } else {
            IndexCacheConfig::disabled()
        };
        self.start_live_watch_with_config(WatchConfig {
            cache_config,
            ..WatchConfig::default()
        })
    }

    pub fn start_live_watch_with_config(&mut self, config: WatchConfig) -> Result<(), AppMessage> {
        if self.is_live_watching() {
            return Ok(());
        }
        if let Some(reason) = self.live_watch_unavailable_reason() {
            return Err(AppMessage::warning(reason));
        }
        self.commit_edits();
        self.stop_filter_read();
        self.stop_bookmark_offset_job();
        let index = self
            .index
            .as_ref()
            .ok_or_else(|| AppMessage::warning("Wait for initial indexing to finish."))?;
        let job = self
            .session
            .start_live_watch(index, self.filter_index.as_ref(), config)
            .map_err(|error| {
                if matches!(error, QuarryError::SourceChanged) {
                    self.invalidate_changed_source();
                    AppMessage::warning(SOURCE_CHANGED_NOTICE)
                } else {
                    AppMessage::core_error(error)
                }
            })?;

        let (snap, live_index, live_filter) = job.state_snapshot();
        self.index = Some(live_index);
        if let Some(filter_idx) = live_filter {
            self.filter_index = Some(filter_idx);
            if let Some(progress) = self.filter_progress.as_mut() {
                progress.matches_found = snap.filter_matches_found.unwrap_or(0);
                progress.done = true;
            }
        }
        self.progress.rows_scanned = snap.committed_rows;
        self.progress.bytes_scanned = snap.committed_record_end;
        self.progress.bytes_read = snap.observed_file_len;
        self.progress.file_size = snap.observed_file_len;
        self.progress.columns_found = snap.maximum_width;
        let initial_committed_rows = snap.committed_rows;
        self.live_watch_last_generation = snap.generation;
        self.live_watch_last_ui_sync = Instant::now() - WATCH_REPAINT_THROTTLE;
        self.live_watch_snapshot = Some(snap);
        self.live_watch_header_row =
            if self.session.dialect.has_header && initial_committed_rows > 0 {
                job.read_rows(0, 1).ok().and_then(|mut rows| rows.pop())
            } else {
                None
            };
        if self.live_watch_header_row.is_some() {
            self.refresh_column_headers();
        }
        self.live_watch_alert = None;
        self.live_watch_tail_follow = true;
        self.live_watch = Some(job);
        // Jump to the live tail immediately on opt-in start; propagate any read failure cleanly.
        if let Err(err) = self.scroll_to_live_tail(true) {
            let _ = self.stop_live_watch();
            return Err(AppMessage::error(err));
        }
        Ok(())
    }

    pub fn stop_live_watch(&mut self) -> Result<Option<WatchAlert>, AppMessage> {
        self.stop_filter_read();
        let cancelled_attach = if let Some(attach) = self.live_watch_filter_attach.take() {
            attach.cancel();
            drop(attach);
            true
        } else {
            false
        };
        let Some(job) = self.live_watch.take() else {
            return Ok(None);
        };
        let outcome = job.stop().map_err(AppMessage::core_error)?;
        self.session = Arc::new(outcome.session);
        self.index = Some(outcome.index.clone());
        self.filter_index = outcome.filter_index;
        self.live_watch_filter_attach_tag = None;
        // Reconcile `filter_query` and `filter_index_tag` with the actual `FilterIndex` returned
        // by `stop()` so cancelling an in-flight replacement filter on Stop Watch never leaves a
        // mismatched query/index pair or a missing tag that drops every static page read.
        match self.filter_index.as_ref() {
            Some(idx) => {
                if self.filter_query.as_ref() != Some(idx.query()) {
                    self.filter_query_generation = self.filter_query_generation.wrapping_add(1);
                    self.filter_query = Some(idx.query().clone());
                    self.filter_status = Some(format!(
                        "Filter complete. {} matches.",
                        group_digits(idx.matches_found())
                    ));
                }
                self.filter_index_tag = Some(self.current_filter_tag());
            }
            None if cancelled_attach => {
                self.filter_query_generation = self.filter_query_generation.wrapping_add(1);
                self.filter_query = None;
                self.filter_index_tag = None;
                self.filter_progress = None;
                self.filter_status = Some("Filter cancelled.".into());
            }
            None => {
                self.filter_index_tag = None;
            }
        }
        self.progress.rows_scanned = outcome.index.indexed_rows();
        self.progress.bytes_scanned = outcome.index.committed_record_end();
        self.progress.bytes_read = outcome.snapshot.observed_file_len;
        self.progress.file_size = self.session.file_size;
        self.progress.columns_found = outcome.index.maximum_width();
        self.progress.done = outcome.index.is_complete();
        self.live_watch_tail_follow = false;
        self.live_watch_viewport_dirty = false;
        self.live_watch_last_generation = outcome.snapshot.generation;
        self.live_watch_snapshot = Some(outcome.snapshot);
        self.live_watch_header_row = None;
        self.apply_indexed_width(outcome.index.maximum_width());
        self.refresh_column_headers();

        let alert = match outcome.final_status {
            WatchStatus::Alert(alert) => {
                if matches!(
                    alert,
                    WatchAlert::Truncated { .. } | WatchAlert::Rewritten | WatchAlert::Rotated
                ) || !outcome.index.is_complete()
                {
                    self.invalidate_changed_source();
                }
                self.live_watch_alert = Some(alert.clone());
                Some(alert)
            }
            _ => {
                self.live_watch_alert = None;
                if !self
                    .bookmarks
                    .pending_offset_rows(self.source_generation)
                    .is_empty()
                {
                    self.bookmark_offset_refresh_pending = true;
                }
                // Reload buffer cleanly from reconciled static session at current scroll anchor.
                self.buffered_rows.clear();
                self.filtered_rows.clear();
                self.filtered_rows_tag = None;
                self.whitespace.invalidate_rows();
                if self.filter_active() {
                    if let Some(progress) = self.filter_progress.as_mut() {
                        progress.matches_found = self
                            .filter_index
                            .as_ref()
                            .map_or(0, FilterIndex::matches_found);
                        progress.done = true;
                    }
                    let _ = self.navigate_filter(self.filter_viewport_start);
                } else if self.available_data_rows() > 0 {
                    let clamped = self
                        .viewport_start
                        .min(self.available_rows().saturating_sub(1))
                        .max(self.data_start);
                    let _ = self.navigate(clamped);
                }
                None
            }
        };
        Ok(alert)
    }

    pub fn dismiss_live_watch_alert(&mut self) {
        self.live_watch_alert = None;
    }

    pub fn set_live_watch_tail_follow(&mut self, follow: bool) -> Result<(), String> {
        if !self.is_live_watching() {
            self.live_watch_tail_follow = false;
            return Ok(());
        }
        self.live_watch_tail_follow = follow;
        if follow {
            self.scroll_to_live_tail(true)?;
        }
        Ok(())
    }

    fn scroll_to_live_tail(&mut self, force_reload: bool) -> Result<(), String> {
        if self.filter_active() {
            let available = self.available_filter_rows();
            if available == 0 {
                self.filter_viewport_start = 0;
                self.filtered_rows.clear();
                self.whitespace.invalidate_rows();
                self.cancel_filter_read_for_navigation();
                return Ok(());
            }
            let target = max_viewport_start(available, self.visible_rows);
            self.filter_viewport_start = target;
            // Preserve the current filtered buffer while the background tail read refreshes so
            // sustained matching appends never blank the grid or starve completed reads.
            return self.schedule_filter_buffer_inner(target, true);
        }
        let available_data = self.available_data_rows();
        if available_data == 0 {
            self.viewport_start = self.data_start;
            self.buffered_rows.clear();
            self.whitespace.invalidate_rows();
            return Ok(());
        }
        let target = self
            .data_start
            .saturating_add(max_viewport_start(available_data, self.visible_rows));
        self.viewport_start = target;
        if self.live_watch_async_viewport {
            self.live_watch_viewport_dirty = true;
            return Ok(());
        }
        if force_reload {
            self.buffered_rows.clear();
            self.whitespace.invalidate_rows();
        }
        self.load_buffer(target)
    }

    /// Takes any deferred unfiltered Live Watch viewport refresh as an off-UI-thread
    /// [`ViewportReadPlan`], ensuring active-tab tail refreshes never perform disk I/O on the UI
    /// thread and inactive watched tabs do not read from disk until activated.
    pub fn take_live_watch_viewport_plan(&mut self) -> Option<ViewportReadPlan> {
        if !self.live_watch_viewport_dirty {
            return None;
        }
        self.live_watch_viewport_dirty = false;
        if self.filter_active() {
            return None;
        }
        let watch = self.live_watch.as_ref()?;
        let available = self.available_rows();
        if available <= self.data_start {
            self.viewport_start = self.data_start;
            self.buffered_rows.clear();
            self.whitespace.invalidate_rows();
            return None;
        }
        let capacity = self.visible_rows.saturating_add(2 * OVERSCAN_ROWS) as u64;
        let last_start = available.saturating_sub(capacity).max(self.data_start);
        let start = self
            .viewport_start
            .saturating_sub(OVERSCAN_ROWS as u64)
            .max(self.data_start)
            .min(last_start);
        let count = available.saturating_sub(start).min(capacity) as usize;
        if count == 0 {
            return None;
        }
        Some(ViewportReadPlan {
            session: Arc::clone(&self.session),
            index: watch.index_snapshot(),
            live_watch_reader: Some(watch.reader()),
            start,
            count,
        })
    }

    /// Polls active Live Watch Mode, throttling steady-stream UI syncs to ~10 Hz while surfacing
    /// alerts and status transitions immediately. Returns `Ok(true)` when UI state changed.
    pub fn poll_live_watch(&mut self) -> Result<bool, String> {
        let Some(job) = self.live_watch.as_ref() else {
            return Ok(false);
        };
        let snap = job.snapshot();
        // Alerts / terminal watch transitions bypass the ~10 Hz repaint throttle immediately.
        if !snap.status.is_active() {
            match self.stop_live_watch() {
                Ok(_) => return Ok(true),
                Err(msg) => {
                    let fallback_alert = match snap.status {
                        WatchStatus::Alert(alert) => alert,
                        _ => WatchAlert::IoError(msg.text.clone()),
                    };
                    self.invalidate_changed_source();
                    self.live_watch_alert = Some(fallback_alert);
                    return Err(msg.text);
                }
            }
        }
        let status_changed = self
            .live_watch_snapshot
            .as_ref()
            .is_some_and(|prev| prev.status != snap.status);
        if snap.generation == self.live_watch_last_generation && !status_changed {
            return Ok(false);
        }
        if !status_changed && self.live_watch_last_ui_sync.elapsed() < WATCH_REPAINT_THROTTLE {
            return Ok(false);
        }

        let prev_rows = self
            .live_watch_snapshot
            .as_ref()
            .map_or(0, |prev| prev.committed_rows);
        let prev_matches = self
            .live_watch_snapshot
            .as_ref()
            .and_then(|prev| prev.filter_matches_found)
            .unwrap_or(0);

        self.live_watch_last_ui_sync = Instant::now();
        let (snap, live_index, live_filter) = job.state_snapshot();
        if !snap.status.is_active() {
            match self.stop_live_watch() {
                Ok(_) => return Ok(true),
                Err(msg) => {
                    let fallback_alert = match snap.status {
                        WatchStatus::Alert(alert) => alert,
                        _ => WatchAlert::IoError(msg.text.clone()),
                    };
                    self.invalidate_changed_source();
                    self.live_watch_alert = Some(fallback_alert);
                    return Err(msg.text);
                }
            }
        }
        let committed_header_row = if self.session.dialect.has_header
            && self.live_watch_header_row.is_none()
            && snap.committed_rows > 0
        {
            job.read_rows(0, 1).ok().and_then(|mut rows| rows.pop())
        } else {
            None
        };
        self.live_watch_last_generation = snap.generation;
        self.index = Some(live_index);
        let mut live_filter_query_changed = false;
        let mut live_filter_matched_active = false;
        if let Some(filter_idx) = live_filter {
            let query_matches_active = self.filter_query.as_ref() == Some(filter_idx.query());
            if query_matches_active {
                live_filter_matched_active = true;
                live_filter_query_changed = self
                    .filter_index
                    .as_ref()
                    .is_some_and(|prev| prev.query() != filter_idx.query());
                if live_filter_query_changed {
                    self.stop_filter_read();
                    self.filtered_rows.clear();
                    self.filtered_rows_tag = None;
                    self.whitespace.invalidate_rows();
                }
                let matches = filter_idx.matches_found();
                self.filter_index = Some(filter_idx);
                self.filter_index_tag = Some(self.current_filter_tag());
                if let Some(progress) = self.filter_progress.as_mut() {
                    progress.matches_found = matches;
                    progress.rows_scanned = snap.committed_rows;
                    progress.bytes_scanned = snap.committed_record_end;
                    progress.file_size = snap.observed_file_len;
                    progress.done = true;
                }
                self.filter_status =
                    Some(format!("Live filter: {} matches.", group_digits(matches)));
            }
        }
        self.progress.rows_scanned = snap.committed_rows;
        self.progress.bytes_scanned = snap.committed_record_end;
        self.progress.bytes_read = snap.observed_file_len;
        self.progress.file_size = snap.observed_file_len;
        self.progress.columns_found = snap.maximum_width;
        if let Some(header_row) = committed_header_row {
            self.live_watch_header_row = Some(header_row);
            self.apply_indexed_width(snap.maximum_width);
            self.refresh_column_headers();
        } else {
            self.apply_indexed_width(snap.maximum_width);
        }

        let rows_grew = snap.committed_rows > prev_rows;
        let matches_grew =
            live_filter_matched_active && snap.filter_matches_found.unwrap_or(0) > prev_matches;
        self.live_watch_snapshot = Some(snap);

        if self.live_watch_tail_follow {
            let filter_ready =
                !self.filter_active() || self.filter_index_tag == Some(self.current_filter_tag());
            if filter_ready && (rows_grew || matches_grew || live_filter_query_changed) {
                self.scroll_to_live_tail(true)?;
            }
        } else if self.filter_active() {
            if self.filter_index_tag == Some(self.current_filter_tag())
                && (live_filter_query_changed
                    || (matches_grew && self.filtered_rows.len() < self.visible_rows))
            {
                self.schedule_filter_buffer_inner(self.filter_viewport_start, true)?;
            }
        } else if rows_grew && self.buffered_rows.len() < self.visible_rows {
            if self.live_watch_async_viewport {
                self.live_watch_viewport_dirty = true;
            } else {
                self.buffered_rows.clear();
                self.whitespace.invalidate_rows();
                self.load_buffer(self.viewport_start)?;
            }
        }
        Ok(true)
    }

    pub fn pause_indexing(&mut self) -> Result<(), String> {
        let Some(job) = &self.job else {
            return Err("No active indexing job to pause.".into());
        };
        if self.progress.done || self.saving_in_place {
            return Err("Indexing is already complete.".into());
        }
        job.pause();
        self.progress = job.progress();
        self.last_poll = Instant::now() - POLL_INTERVAL;
        Ok(())
    }

    pub fn resume_indexing(&mut self) -> Result<(), String> {
        let result = {
            let Some(job) = self.job.as_mut() else {
                return Err("No paused indexing job to resume.".into());
            };
            if !job.is_paused() {
                return Err("Indexing is not paused.".into());
            }
            job.resume()
        };
        if let Err(error) = result {
            if matches!(error, QuarryError::SourceChanged) {
                self.invalidate_changed_source();
                return Err(SOURCE_CHANGED_NOTICE.into());
            }
            return Err(self.diagnostic_error(error));
        }
        if let Some(job) = &self.job {
            self.progress = job.progress();
        }
        self.last_poll = Instant::now() - POLL_INTERVAL;
        Ok(())
    }

    pub fn poll(&mut self) -> Result<(), String> {
        self.poll_bookmark_offsets()?;
        if self.poll_decompression()? {
            return Ok(());
        }
        let Some(job) = &self.job else {
            return Ok(());
        };
        let progress = job.progress();
        let transitioned_to_paused = progress.paused && !self.progress.paused;
        if self.last_poll.elapsed() < POLL_INTERVAL && !progress.done && !transitioned_to_paused {
            return Ok(());
        }
        self.last_poll = Instant::now();
        self.progress = progress;
        if self.progress.done {
            // An in-place save can publish a new source before its completion is polled.
            // Its reload owns the next index. A cancelled save leaves this job to collect.
            if self.saving_in_place {
                return Ok(());
            }
            let job = self.job.take().expect("index job is present");
            let result = job.wait().and_then(|index| {
                self.session.ensure_source_unchanged()?;
                Ok(index)
            });
            match result {
                Ok(index) => {
                    self.index = Some(index);
                    self.start_bookmark_offset_refresh();
                }
                Err(error) => {
                    if matches!(error, QuarryError::SourceChanged) {
                        self.invalidate_changed_source();
                    }
                    return Err(self.diagnostic_error(error));
                }
            }
        }
        self.apply_indexed_width(self.progress.columns_found);
        if self.progress.done || transitioned_to_paused {
            self.load_buffer(self.viewport_start)?;
        }
        Ok(())
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn start_find_next(&mut self, query: &[u8]) -> Result<(), String> {
        self.start_find_next_with_case(query, CaseSensitivity::Insensitive)
    }

    pub fn start_find_next_with_case(
        &mut self,
        query: &[u8],
        case_sensitivity: CaseSensitivity,
    ) -> Result<(), String> {
        if query.is_empty() {
            return Err("Enter text to find.".into());
        }
        if self.filter_active() {
            return Err("Clear the filter before using Find.".into());
        }
        if self.search_job.is_some() {
            return Err("A search is already running.".into());
        }
        if !self.is_search_ready() {
            return Err(self
                .compressed_unavailable_reason()
                .unwrap_or("Search is available after indexing completes.")
                .into());
        }
        self.commit_edits();
        if self.search_query != query || self.search_case_sensitivity != case_sensitivity {
            self.search_query.clear();
            self.search_query.extend_from_slice(query);
            self.search_case_sensitivity = case_sensitivity;
            self.last_match = None;
            self.search_history.clear();
            self.search_history_limits.clear();
            self.search_history_index = None;
        }
        if let Some(index) = self.search_history_index
            && let Some(found) = self.search_history.get(index.saturating_add(1)).copied()
        {
            let next_index = index + 1;
            let limit = self
                .search_history_limits
                .get(next_index)
                .copied()
                .flatten();
            self.search_history_index = Some(next_index);
            return self.show_search_match_with_limit(found, limit);
        }
        let position = self.last_match.as_ref().map_or(
            SearchPosition {
                row: self.viewport_start,
                column: 0,
            },
            |found| SearchPosition {
                row: found.row,
                column: found.column.saturating_add(1),
            },
        );
        let partial_snapshot;
        let (index, captured_paused_limit) = if let Some(index) = &self.index {
            (index, None)
        } else if let Some(job) = &self.job {
            partial_snapshot = job.snapshot();
            let limit = partial_snapshot
                .indexed_rows()
                .saturating_sub(self.data_start);
            (&partial_snapshot, Some(limit))
        } else {
            return Err("Search is available after indexing completes.".into());
        };
        let search_job = match self.session.start_search_with_cell_edits_and_case(
            index,
            query.to_vec(),
            position,
            self.owned_cell_edits(),
            case_sensitivity,
        ) {
            Ok(job) => job,
            Err(QuarryError::SourceChanged) => {
                self.invalidate_changed_source();
                return Err(SOURCE_CHANGED_NOTICE.into());
            }
            Err(error) => return Err(self.diagnostic_error(error)),
        };
        self.search_job = Some(search_job);
        self.search_paused_row_limit = captured_paused_limit;
        self.search_status = None;
        self.reveal_cell = None;
        Ok(())
    }

    pub fn can_find_previous_with_case(
        &self,
        query: &[u8],
        case_sensitivity: CaseSensitivity,
    ) -> bool {
        !query.is_empty()
            && self.search_job.is_none()
            && !self.filter_active()
            && self.is_search_ready()
            && self.search_query == query
            && self.search_case_sensitivity == case_sensitivity
            && match self.search_history_index {
                Some(index) => index > 0,
                None => !self.search_history.is_empty(),
            }
    }

    pub fn start_find_previous_with_case(
        &mut self,
        query: &[u8],
        case_sensitivity: CaseSensitivity,
    ) -> Result<(), String> {
        self.commit_edits();
        if !self.can_find_previous_with_case(query, case_sensitivity) {
            return Err("Find at least two matches before moving back.".into());
        }
        let index = self.search_history_index.map_or_else(
            || self.search_history.len().checked_sub(1),
            |index| index.checked_sub(1),
        );
        let index = index.expect("a previous match was checked above");
        let found = self.search_history[index];
        let limit = self.search_history_limits.get(index).copied().flatten();
        self.search_history_index = Some(index);
        self.show_search_match_with_limit(found, limit)
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn can_replace_current(&self, query: &[u8]) -> bool {
        self.can_replace_current_with_case(query, CaseSensitivity::Insensitive)
    }

    pub fn can_replace_current_with_case(
        &self,
        query: &[u8],
        case_sensitivity: CaseSensitivity,
    ) -> bool {
        if query.is_empty()
            || self.is_compressed()
            || self.is_live_watching()
            || self.search_job.is_some()
            || self.save_job.is_some()
            || self.structural_job.is_some()
            || self.cell_edit.is_some()
            || self.header_edit.is_some()
            || self.search_query != query
            || self.search_case_sensitivity != case_sensitivity
        {
            return false;
        }
        let Some(found) = self.last_match.as_ref() else {
            return false;
        };
        matches!(
            self.selection,
            Some(GridSelection::Cell { row, column })
                if row == found.row && column == found.column
        ) && self
            .effective_cell(found.row, found.column)
            .is_some_and(|value| literal_contains(value, query, case_sensitivity))
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn replace_current_match(
        &mut self,
        query: &[u8],
        replacement: &[u8],
    ) -> Result<(), String> {
        self.replace_current_match_with_case(query, replacement, CaseSensitivity::Insensitive)
    }

    pub fn replace_current_match_with_case(
        &mut self,
        query: &[u8],
        replacement: &[u8],
        case_sensitivity: CaseSensitivity,
    ) -> Result<(), String> {
        if !self.can_replace_current_with_case(query, case_sensitivity) {
            return Err("Find a current match before replacing it.".into());
        }
        let found = self
            .last_match
            .expect("replaceable match was checked above");
        let key = (found.row, found.column);
        let source = self
            .source_cell(found.row, found.column)
            .ok_or_else(|| {
                "The current matched cell is no longer available for replacement.".to_owned()
            })?
            .to_vec();
        let effective = self
            .cell_edits
            .get(&key)
            .map_or(source.as_slice(), |value| value.as_slice());
        let next = replace_literal_all_with_case(effective, query, replacement, case_sensitivity)
            .expect("replaceable match contains the query");
        if next.as_slice() != effective {
            self.record_value_edit(
                EditTarget::Cell(found.row, found.column),
                (next != source).then_some(next),
            );
        }
        self.selection = Some(GridSelection::Cell {
            row: found.row,
            column: found.column,
        });
        self.clear_row_selection();
        if let Some(index) = self.search_history_index.take() {
            self.search_history.truncate(index);
            self.search_history_limits.truncate(index);
        }
        self.start_find_next_with_case(query, case_sensitivity)
    }

    pub fn show_search_match(&mut self, found: SearchMatch) -> Result<(), String> {
        self.show_search_match_with_limit(found, self.search_paused_row_limit)
    }

    fn show_search_match_with_limit(
        &mut self,
        found: SearchMatch,
        paused_row_limit: Option<u64>,
    ) -> Result<(), String> {
        let row = found.row;
        let column = found.column;
        self.navigate(row)?;
        self.center_column(column);
        self.reveal_cell = Some((row, column));
        self.selection = Some(GridSelection::Cell { row, column });
        self.clear_row_selection();
        self.selected_columns.clear();
        self.column_selection_anchor = None;
        let data_row = row.saturating_sub(self.data_start).saturating_add(1);
        let data_col = column.saturating_add(1);
        self.search_status = Some(if let Some(limit) = paused_row_limit {
            format!(
                "Found row {data_row}, column {data_col} (searched up to row {limit}, indexing paused)."
            )
        } else {
            format!("Found row {data_row}, column {data_col}.")
        });
        self.last_match = Some(found);
        Ok(())
    }

    pub fn poll_search(&mut self) -> Result<(), String> {
        let Some(job) = &self.search_job else {
            return Ok(());
        };
        if !job.progress().done {
            return Ok(());
        }
        let job = self.search_job.take().expect("search job is present");
        let captured_paused_limit = self.search_paused_row_limit;
        match job.wait() {
            Ok(SearchOutcome::Match(found)) => {
                if let Some(index) = self.search_history_index {
                    self.search_history.truncate(index + 1);
                    self.search_history_limits.truncate(index + 1);
                }
                self.search_history.push(found);
                self.search_history_limits.push(captured_paused_limit);
                self.search_history_index = self.search_history.len().checked_sub(1);
                self.show_search_match_with_limit(found, captured_paused_limit)?;
            }
            Ok(SearchOutcome::NotFound) => {
                self.search_status = Some(if let Some(limit) = captured_paused_limit {
                    format!("No further matches (searched up to row {limit}, indexing paused).")
                } else {
                    "No further matches.".into()
                });
            }
            Ok(SearchOutcome::Cancelled) => {
                self.search_status = Some("Search cancelled.".into());
            }
            Err(QuarryError::RowNotIndexed { indexed_rows, .. })
                if captured_paused_limit.is_some() =>
            {
                let limit = captured_paused_limit
                    .unwrap_or_else(|| indexed_rows.saturating_sub(self.data_start));
                self.search_status = Some(format!(
                    "No match — searched up to row {limit}, indexing paused."
                ));
            }
            Err(QuarryError::SourceChanged) => {
                self.invalidate_changed_source();
                return Err(SOURCE_CHANGED_NOTICE.into());
            }
            Err(error) => return Err(self.diagnostic_error(error)),
        }
        Ok(())
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn search_progress(&self) -> Option<SearchProgress> {
        self.search_job.as_ref().map(SearchJob::progress)
    }

    /// Returns the monotonic `(document_id, edit_revision, query_generation)` tag for the
    /// document's active filter state.
    pub fn current_filter_tag(&self) -> FilterResultTag {
        FilterResultTag {
            document_id: self
                .document_id
                .unwrap_or_else(|| DocumentId::new(self.instance_id)),
            edit_revision: self.edit_revision,
            query_generation: self.filter_query_generation,
        }
    }

    /// Returns whether live as-you-type Query Bar filtering should default on for this document
    /// (true at or below [`DEFAULT_LIVE_FILTER_MAX_BYTES`], false for larger files).
    pub fn default_live_filter_enabled(&self) -> bool {
        self.session.active_byte_len() <= DEFAULT_LIVE_FILTER_MAX_BYTES
    }

    /// Returns raw header bytes for expression column resolution, honoring section headers,
    /// committed Live Watch headers, and unsaved header renames (empty when headerless).
    pub fn filter_column_headers(&self) -> Vec<Vec<u8>> {
        if !self.session.dialect.has_header {
            return Vec::new();
        }
        let mut fields = if let Some(row) = self.live_watch_header_row.as_ref() {
            row.fields.clone()
        } else {
            self.session.filter_column_schema().headers.to_vec()
        };
        for (column, name) in self.header_renames.iter() {
            if let Some(field) = fields.get_mut(*column) {
                *field = name.as_bytes().to_vec();
            }
        }
        fields
    }

    /// Parses an exact `FilterQuery` expression against this document's active column schema.
    pub fn parse_filter_expression(
        &self,
        expr: &str,
        case_sensitivity: CaseSensitivity,
    ) -> Result<FilterQuery, FilterExprError> {
        let headers = self.filter_column_headers();
        let column_count = (self.total_columns > 0).then_some(self.total_columns);
        let schema = FilterColumnSchema::with_headers(&headers, column_count);
        parse_filter_expr(expr, &schema, case_sensitivity)
    }

    /// Prints the document's active `FilterQuery` in canonical expression form, if any.
    pub fn active_filter_expression(&self) -> Option<Result<String, FilterExprError>> {
        let query = self.filter_query.as_ref()?;
        let headers = self.filter_column_headers();
        let column_count = (self.total_columns > 0).then_some(self.total_columns);
        let schema = FilterColumnSchema::with_headers(&headers, column_count);
        Some(print_filter_expr(query, &schema))
    }

    pub fn start_filter(&mut self, query: FilterQuery) -> Result<(), String> {
        if self.export_job.is_some() {
            return Err("Cancel the active export before changing filters.".into());
        }
        if self.search_job.is_some() {
            return Err("Cancel the active search before filtering.".into());
        }
        if let Some(reason) = self.compressed_unavailable_reason() {
            return Err(reason.into());
        }
        if !self.is_filter_ready() {
            return Err("Open a file with at least one column before filtering.".into());
        }
        if query.predicates.is_empty() {
            return Err("Add at least one filter rule.".into());
        }
        for (index, predicate) in query.predicates.iter().enumerate() {
            self.validate_column(predicate.column)
                .map_err(|error| format!("Rule {}: {error}", index + 1))?;
            if predicate.operator == FilterOperator::Contains && predicate.value.is_empty() {
                return Err(format!(
                    "Rule {}: enter text for a Contains filter.",
                    index + 1
                ));
            }
        }
        query
            .validate()
            .map_err(|error| self.diagnostic_error(error))?;

        self.commit_edits();
        // Supersede any in-flight scan/read only after validation succeeds so invalid queries
        // never disturb the currently active filter.
        let prev_scan_was_running = self.filter_job.is_some();
        self.stop_filter_read();
        if let Some(old_job) = self.filter_job.take() {
            old_job.cancel();
            drop(old_job);
        }
        self.filter_job_tag = None;
        if let Some(old_attach) = self.live_watch_filter_attach.take() {
            old_attach.cancel();
            drop(old_attach);
        }
        self.live_watch_filter_attach_tag = None;
        self.filter_query_generation = self.filter_query_generation.wrapping_add(1);
        let tag = self.current_filter_tag();

        if let Some(watch) = &self.live_watch {
            let attach_job = match watch.start_attach_filter_query(query.clone()) {
                Ok(job) => job,
                Err(error) => {
                    // Reconcile retained watcher filter state with the bumped generation so an
                    // attachment startup error never strands the previous filter with a stale tag.
                    if let Some(existing) = watch.filter_index_snapshot() {
                        let matches = existing.matches_found();
                        self.filter_query = Some(existing.query().clone());
                        self.filter_index = Some(existing);
                        self.filter_index_tag = Some(tag);
                        self.filter_progress = Some(FilterProgress {
                            bytes_scanned: self.progress.bytes_scanned,
                            rows_scanned: self.progress.rows_scanned,
                            matches_found: matches,
                            file_size: self.progress.file_size,
                            elapsed: Duration::ZERO,
                            done: true,
                            cancelled: false,
                        });
                        if !self.filtered_rows.is_empty() {
                            self.filtered_rows_tag = Some(tag);
                        }
                    } else {
                        self.filter_query = None;
                        self.filter_index = None;
                        self.filter_index_tag = None;
                        self.filter_progress = None;
                        self.filtered_rows.clear();
                        self.filtered_rows_tag = None;
                    }
                    return Err(self.diagnostic_error(error));
                }
            };
            self.live_watch_filter_attach = Some(attach_job);
            self.live_watch_filter_attach_tag = Some(tag);
            self.filter_index = None;
            self.filter_index_tag = None;
            self.filter_query = Some(query);
            self.filter_refresh_pending = false;
            self.filter_paused_row_limit = None;
            self.filter_progress = Some(FilterProgress {
                bytes_scanned: 0,
                rows_scanned: 0,
                matches_found: 0,
                file_size: self.progress.file_size,
                elapsed: Duration::ZERO,
                done: false,
                cancelled: false,
            });
            self.filter_status = Some("Filtering live file…".into());
            self.export_progress = None;
            self.export_status = None;
            self.export_cancel_requested = false;
            self.filter_viewport_start = 0;
            self.filter_buffer_start = 0;
            self.whitespace.invalidate_rows();
            self.filtered_rows.clear();
            self.filtered_rows_tag = None;
            self.selection = None;
            self.clear_row_selection();
            self.reveal_cell = None;
            self.scroll_points = 0.0;
            return Ok(());
        }
        let (max_bytes, paused_row_limit) = if self.is_indexing_paused() {
            let partial = self.job.as_ref().map(IndexJob::snapshot);
            (
                partial.as_ref().map(StructuralIndex::committed_record_end),
                partial
                    .as_ref()
                    .map(|idx| idx.indexed_rows().saturating_sub(self.data_start)),
            )
        } else {
            (None, None)
        };
        let job = match self.session.start_filter_with_edits_and_limit(
            query.clone(),
            Arc::new(self.cell_edits.as_ref().clone()),
            max_bytes,
        ) {
            Ok(job) => job,
            Err(error) => {
                if matches!(error, QuarryError::SourceChanged) {
                    self.invalidate_changed_source();
                } else if let Some(existing) = self.filter_index.as_ref()
                    && self.filter_query.as_ref() == Some(existing.query())
                {
                    // Re-tag the retained index so scrolling/paging continues working, and mark
                    // progress cancelled if the previous scan was still in flight when cancelled.
                    let matches = existing.matches_found();
                    self.filter_index_tag = Some(tag);
                    if !self.filtered_rows.is_empty() {
                        self.filtered_rows_tag = Some(tag);
                    }
                    if prev_scan_was_running {
                        if let Some(progress) = self.filter_progress.as_mut() {
                            progress.matches_found = matches;
                            progress.done = true;
                            progress.cancelled = true;
                        }
                        self.filter_status =
                            Some(format!("Filter cancelled after {matches} matches."));
                    }
                } else {
                    self.filter_query = None;
                    self.filter_index = None;
                    self.filter_index_tag = None;
                    self.filter_progress = None;
                    self.filtered_rows.clear();
                    self.filtered_rows_tag = None;
                }
                return Err(self.diagnostic_error(error));
            }
        };
        self.filter_progress = Some(job.progress());
        self.filter_index = Some(job.snapshot());
        self.filter_index_tag = Some(tag);
        self.filter_job = Some(job);
        self.filter_job_tag = Some(tag);
        self.filter_query = Some(query);
        self.filter_refresh_pending = false;
        self.filter_paused_row_limit = paused_row_limit;
        self.filter_status = Some(if let Some(limit) = paused_row_limit {
            format!("Filtering rows (up to row {limit}, indexing paused)…")
        } else {
            "Filtering rows…".into()
        });
        self.export_progress = None;
        self.export_status = None;
        self.export_cancel_requested = false;
        self.filter_viewport_start = 0;
        self.filter_buffer_start = 0;
        self.whitespace.invalidate_rows();
        self.filtered_rows.clear();
        self.filtered_rows_tag = None;
        self.selection = None;
        self.clear_row_selection();
        self.reveal_cell = None;
        self.scroll_points = 0.0;
        Ok(())
    }

    /// Drop rows from the old snapshot before scheduling a scan of committed values.
    pub fn queue_filter_refresh(&mut self) {
        if !self.filter_active() {
            return;
        }
        self.stop_filter_read();
        if let Some(job) = self.filter_job.take() {
            job.cancel();
        }
        self.filter_job_tag = None;
        self.filter_query_generation = self.filter_query_generation.wrapping_add(1);
        self.filter_index = None;
        self.filter_index_tag = None;
        self.filter_progress = None;
        self.filtered_rows.clear();
        self.filtered_rows_tag = None;
        self.whitespace.invalidate_rows();
        self.selection = None;
        self.clear_row_selection();
        self.reveal_cell = None;
        self.filter_refresh_pending = true;
        self.filter_status = Some("Refreshing matches after edits…".into());
    }

    pub fn poll_filter(&mut self) -> Result<(), String> {
        if self.filter_refresh_pending {
            self.filter_refresh_pending = false;
            if let Some(query) = self.filter_query.clone() {
                let viewport = self.filter_viewport_start;
                let scroll_points = self.scroll_points;
                // Starting a refresh must not commit or discard the next cell's draft.
                let editor = self.cell_edit.take();
                let result = self.start_filter(query);
                self.cell_edit = editor;
                result?;
                self.filter_viewport_start = viewport;
                self.scroll_points = scroll_points;
                if let Some(edit) = &self.cell_edit {
                    self.selection = Some(GridSelection::Cell {
                        row: edit.row,
                        column: edit.column,
                    });
                }
            }
        }
        self.poll_filter_scan()?;
        self.poll_filter_read()
    }

    /// Installs a completed filter scan index only if its `(document_id, edit_revision,
    /// query_generation)` tag still matches the document's active filter state.
    pub fn accept_filter_scan_completion(
        &mut self,
        tag: FilterResultTag,
        index: FilterIndex,
        progress: FilterProgress,
    ) -> Result<bool, String> {
        if tag != self.current_filter_tag() || !self.filter_active() {
            return Ok(false);
        }
        let matches = index.matches_found();
        self.filter_index = Some(index);
        self.filter_index_tag = Some(tag);
        self.filter_progress = Some(progress);
        self.filter_status = Some(if let Some(limit) = self.filter_paused_row_limit {
            if progress.cancelled {
                let scanned = progress.rows_scanned.saturating_sub(self.data_start);
                format!(
                    "Filter cancelled after {matches} matches (filtered up to row {scanned}, indexing paused)."
                )
            } else if matches == 0 {
                format!(
                    "Filter complete. No matching rows (filtered up to row {limit}, indexing paused)."
                )
            } else {
                format!(
                    "Filter complete. {matches} matches (filtered up to row {limit}, indexing paused)."
                )
            }
        } else if progress.cancelled {
            format!("Filter cancelled after {matches} matches.")
        } else if matches == 0 {
            "Filter complete. No matching rows.".into()
        } else {
            format!("Filter complete. {matches} matches.")
        });
        if matches > 0 {
            self.navigate_filter(self.filter_viewport_start)?;
        }
        Ok(true)
    }

    /// Installs a completed filtered-page read only if its `(document_id, edit_revision,
    /// query_generation)` tag still matches the document's active filter state and index.
    pub fn accept_filter_page_result(
        &mut self,
        tag: FilterResultTag,
        window: FilterReadWindow,
        rows: Vec<FilterMatch>,
        started: Instant,
        tail_follow: bool,
        cancel_requested: bool,
    ) -> bool {
        if tag != self.current_filter_tag()
            || self.filter_index_tag != Some(tag)
            || !self.filter_active()
        {
            return false;
        }
        let desired = self.filter_read_window(self.filter_viewport_start);
        let install_coalesced_live =
            tail_follow && !cancel_requested && self.filter_active() && self.is_live_watching();
        if desired != Some(window) && !install_coalesced_live {
            return false;
        }
        let loaded_columns = rows.iter().map(|row| row.fields.len()).max().unwrap_or(0);
        if loaded_columns > self.total_columns {
            self.ensure_column_count(loaded_columns);
            self.refresh_column_headers();
        }
        self.last_viewport_read = Some(started.elapsed());
        self.filter_buffer_start = window.start_match;
        self.whitespace.invalidate_rows();
        self.filtered_rows = rows.into_iter().map(Arc::new).collect();
        self.filtered_rows_tag = Some(tag);
        true
    }

    pub fn poll_filter_scan(&mut self) -> Result<(), String> {
        if let Some(attach) = self.live_watch_filter_attach.as_ref() {
            if !attach.is_done() {
                return Ok(());
            }
            let attach = self
                .live_watch_filter_attach
                .take()
                .expect("live watch filter attach job is present");
            let attach_tag = self.live_watch_filter_attach_tag.take();
            match attach.wait() {
                Ok(Some(attached_index)) => {
                    if attach_tag != Some(self.current_filter_tag()) || !self.filter_active() {
                        return Ok(());
                    }
                    // Collect the watcher's current synchronized filter index so rows appended
                    // after filter installation (already synced by `poll_live_watch`) are never
                    // overwritten by the older snapshot returned by the attachment worker.
                    let filter_index = self
                        .live_watch
                        .as_ref()
                        .and_then(LiveWatchJob::filter_index_snapshot)
                        .unwrap_or(attached_index);
                    let matches = filter_index.matches_found();
                    self.filter_index = Some(filter_index);
                    self.filter_index_tag = attach_tag;
                    self.filter_progress = Some(FilterProgress {
                        bytes_scanned: self.progress.bytes_scanned,
                        rows_scanned: self.progress.rows_scanned,
                        matches_found: matches,
                        file_size: self.progress.file_size,
                        elapsed: Duration::ZERO,
                        done: true,
                        cancelled: false,
                    });
                    self.filter_status =
                        Some(format!("Live filter: {} matches.", group_digits(matches)));
                    self.filter_viewport_start = if self.live_watch_tail_follow {
                        max_viewport_start(matches, self.visible_rows)
                    } else {
                        0
                    };
                    self.stop_filter_read();
                    self.filter_buffer_start = 0;
                    self.whitespace.invalidate_rows();
                    self.filtered_rows.clear();
                    self.filtered_rows_tag = None;
                    self.schedule_filter_buffer(self.filter_viewport_start)?;
                }
                Ok(None) => {
                    if attach_tag != Some(self.current_filter_tag()) {
                        return Ok(());
                    }
                    self.stop_filter_read();
                    if let Some(existing) = self
                        .live_watch
                        .as_ref()
                        .and_then(LiveWatchJob::filter_index_snapshot)
                    {
                        let matches = existing.matches_found();
                        self.filter_query_generation = self.filter_query_generation.wrapping_add(1);
                        let restored_tag = self.current_filter_tag();
                        self.filter_query = Some(existing.query().clone());
                        self.filter_index = Some(existing);
                        self.filter_index_tag = Some(restored_tag);
                        self.filter_progress = Some(FilterProgress {
                            bytes_scanned: self.progress.bytes_scanned,
                            rows_scanned: self.progress.rows_scanned,
                            matches_found: matches,
                            file_size: self.progress.file_size,
                            elapsed: Duration::ZERO,
                            done: true,
                            cancelled: false,
                        });
                        self.filter_status = Some(format!(
                            "Filter change cancelled; keeping previous live filter ({} matches).",
                            group_digits(matches)
                        ));
                        self.filter_viewport_start = if self.live_watch_tail_follow {
                            max_viewport_start(matches, self.visible_rows)
                        } else {
                            0
                        };
                        self.filter_buffer_start = 0;
                        self.whitespace.invalidate_rows();
                        self.filtered_rows.clear();
                        self.filtered_rows_tag = None;
                        self.schedule_filter_buffer(self.filter_viewport_start)?;
                    } else {
                        self.filter_query = None;
                        self.filter_index = None;
                        self.filter_index_tag = None;
                        self.filtered_rows_tag = None;
                        self.filter_progress = None;
                        self.filter_status = Some("Filter cancelled.".into());
                    }
                }
                Err(error) => {
                    if attach_tag != Some(self.current_filter_tag()) {
                        return Ok(());
                    }
                    self.stop_filter_read();
                    if matches!(error, QuarryError::SourceChanged) {
                        self.invalidate_changed_source();
                    }
                    // Keep `filter_query`, `filter_index`, and the watcher's live filter aligned
                    // when a replacement attachment fails instead of leaving the failed query
                    // paired with the previous filter's index.
                    if let Some(existing) = self
                        .live_watch
                        .as_ref()
                        .and_then(LiveWatchJob::filter_index_snapshot)
                    {
                        let matches = existing.matches_found();
                        self.filter_query_generation = self.filter_query_generation.wrapping_add(1);
                        let restored_tag = self.current_filter_tag();
                        self.filter_query = Some(existing.query().clone());
                        self.filter_index = Some(existing);
                        self.filter_index_tag = Some(restored_tag);
                        self.filter_progress = Some(FilterProgress {
                            bytes_scanned: self.progress.bytes_scanned,
                            rows_scanned: self.progress.rows_scanned,
                            matches_found: matches,
                            file_size: self.progress.file_size,
                            elapsed: Duration::ZERO,
                            done: true,
                            cancelled: false,
                        });
                        if !matches!(error, QuarryError::SourceChanged) {
                            self.filter_buffer_start = 0;
                            self.whitespace.invalidate_rows();
                            self.filtered_rows.clear();
                            self.filtered_rows_tag = None;
                            let _ = self.schedule_filter_buffer(self.filter_viewport_start);
                        }
                    } else {
                        self.filter_query = None;
                        self.filter_index = None;
                        self.filter_index_tag = None;
                        self.filtered_rows_tag = None;
                        self.filter_progress = None;
                    }
                    self.filter_status = Some("Filter failed.".into());
                    return Err(self.diagnostic_error(error));
                }
            }
            return Ok(());
        }
        let Some(job) = self.filter_job.as_ref() else {
            return Ok(());
        };
        let current_tag = self.current_filter_tag();
        if self.filter_job_tag != Some(current_tag) {
            if let Some(stale) = self.filter_job.take() {
                stale.cancel();
                drop(stale);
            }
            self.filter_job_tag = None;
            return Ok(());
        }
        let progress = job.progress();
        self.filter_progress = Some(progress);

        if progress.matches_found > 0 {
            let required = self
                .filter_viewport_start
                .saturating_add(self.visible_rows as u64)
                .min(progress.matches_found);
            // A partial scan cannot yet clamp a later page against the final match count.
            if progress.done
                || self.filter_viewport_start
                    <= max_viewport_start(progress.matches_found, self.visible_rows)
            {
                self.refresh_filter_snapshot_for(required);
                self.filter_index_tag = Some(current_tag);
                self.navigate_filter(self.filter_viewport_start)?;
            }
        } else {
            self.filtered_rows.clear();
            self.filtered_rows_tag = None;
            self.filter_buffer_start = 0;
            if progress.done {
                self.filter_viewport_start = 0;
            }
        }
        if !progress.done {
            return Ok(());
        }

        let job = self.filter_job.take().expect("filter job is present");
        let job_tag = self.filter_job_tag.take().unwrap_or(current_tag);
        match job.wait() {
            Ok(index) => {
                let _ = self.accept_filter_scan_completion(job_tag, index, progress)?;
                Ok(())
            }
            Err(error) => {
                if job_tag != self.current_filter_tag() {
                    return Ok(());
                }
                if matches!(error, QuarryError::SourceChanged) {
                    self.invalidate_changed_source();
                    return Err(SOURCE_CHANGED_NOTICE.into());
                }
                self.stop_filter_read();
                self.filter_index = None;
                self.filter_index_tag = None;
                self.filter_query = None;
                self.filter_refresh_pending = false;
                self.filter_progress = None;
                self.filter_status = None;
                self.filter_paused_row_limit = None;
                self.whitespace.invalidate_rows();
                self.filtered_rows.clear();
                self.filtered_rows_tag = None;
                self.filter_viewport_start = 0;
                self.filter_buffer_start = 0;
                Err(self.diagnostic_error(error))
            }
        }
    }

    pub fn poll_filter_read(&mut self) -> Result<(), String> {
        let done = self
            .filter_read
            .as_ref()
            .is_some_and(|active| active.job.progress().done);
        if !done {
            return Ok(());
        }

        let active = self
            .filter_read
            .take()
            .expect("completed filter read should be active");
        let desired = self.filter_read_window(self.filter_viewport_start);
        match active.job.wait() {
            Ok(FilterReadOutcome::Complete(rows)) => {
                let _ = self.accept_filter_page_result(
                    active.tag,
                    active.window,
                    rows,
                    active.started,
                    active.tail_follow,
                    active.cancel_requested,
                );
            }
            Ok(FilterReadOutcome::Cancelled) => {}
            Err(error) => {
                if active.tag != self.current_filter_tag() {
                    return Ok(());
                }
                if matches!(error, QuarryError::SourceChanged) {
                    self.invalidate_changed_source();
                    return Err(SOURCE_CHANGED_NOTICE.into());
                }
                let message = self.diagnostic_error(error);
                if !self.filter_waiting_for_page()
                    && desired.is_some_and(|window| window != active.window)
                {
                    self.pending_filter_read = desired;
                    let _ = self.schedule_filter_buffer_inner(
                        self.filter_viewport_start,
                        self.is_live_watching() && self.live_watch_tail_follow,
                    );
                } else {
                    self.pending_filter_read = None;
                }
                return Err(message);
            }
        }

        if self.filter_waiting_for_page() || !self.filter_active() {
            return Ok(());
        }
        self.schedule_filter_buffer_inner(
            self.filter_viewport_start,
            self.is_live_watching() && self.live_watch_tail_follow,
        )
    }

    pub fn start_filtered_export(&mut self, destination: PathBuf) -> Result<(), AppMessage> {
        if self.save_job.is_some() {
            return Err(AppMessage::warning(
                "Cancel the active save before exporting filtered rows.",
            ));
        }
        self.commit_edits();
        if self.filter_refresh_pending {
            return Err(AppMessage::warning(
                "Wait for matches to refresh before exporting.",
            ));
        }
        if self.export_job.is_some() {
            return Err(AppMessage::warning("A filtered export is already running."));
        }
        let query = self
            .filter_query
            .clone()
            .ok_or_else(|| AppMessage::warning("Apply a filter before exporting rows."))?;
        let progress = self.filter_progress().ok_or_else(|| {
            AppMessage::warning("Wait for filtering to complete before exporting.")
        })?;
        if self.filter_job.is_some() || !progress.done {
            return Err(AppMessage::warning(
                "Wait for filtering to complete before exporting.",
            ));
        }
        if progress.cancelled {
            return Err(AppMessage::warning(
                "Run the filter to completion before exporting.",
            ));
        }
        if self.filter_paused_row_limit.is_some() {
            return Err(AppMessage::warning(
                "Resume indexing and re-run the filter over the full file before exporting filtered rows.",
            ));
        }

        let job = self
            .session
            .start_filtered_export_with_edits(
                query,
                Arc::new(self.cell_edits.as_ref().clone()),
                self.header_renames
                    .iter()
                    .map(|(&column, name)| (column, name.as_bytes().to_vec()))
                    .collect(),
                destination,
            )
            .map_err(|error| {
                if matches!(error, QuarryError::SourceChanged) {
                    self.invalidate_changed_source();
                }
                AppMessage::core_error(error)
            })?;
        self.export_progress = Some(job.progress());
        self.export_job = Some(job);
        self.export_status = Some(
            "Export in progress. To open or reapply a file, cancel this export and wait for it to finish."
                .into(),
        );
        self.export_cancel_requested = false;
        Ok(())
    }

    pub fn poll_filtered_export(&mut self) -> Result<(), String> {
        let Some(job) = self.export_job.as_ref() else {
            return Ok(());
        };
        let progress = job.progress();
        self.export_progress = Some(progress);
        if !progress.done {
            return Ok(());
        }

        let job = self
            .export_job
            .take()
            .expect("filter export job is present");
        self.export_cancel_requested = false;
        match job.wait() {
            Ok(FilterExportOutcome::Complete(summary)) => {
                self.export_status = Some(format!(
                    "Export complete: {} rows ({}) saved to {}.",
                    summary.rows_written,
                    format_bytes(summary.bytes_written),
                    summary.destination.display()
                ));
                Ok(())
            }
            Ok(FilterExportOutcome::Cancelled) => {
                self.export_status = Some("Export cancelled. No output file was created.".into());
                Ok(())
            }
            Err(error) => {
                if matches!(error, QuarryError::SourceChanged) {
                    self.invalidate_changed_source();
                    return Err(SOURCE_CHANGED_NOTICE.into());
                }
                self.export_status = Some("Export failed. No output file was created.".into());
                Err(self.diagnostic_error(error))
            }
        }
    }

    pub fn cancel_filtered_export(&mut self) {
        if let Some(job) = &self.export_job {
            job.cancel();
            self.export_cancel_requested = true;
            self.export_status = Some(
                "Cancelling export… Wait for it to finish before opening or reapplying the file."
                    .into(),
            );
        }
    }

    pub fn is_filtered_export_ready(&self) -> bool {
        !self.source_changed
            && !self.is_live_watching()
            && self.filter_query.is_some()
            && self.filter_paused_row_limit.is_none()
            && !self.filter_refresh_pending
            && self.filter_job.is_none()
            && self.export_job.is_none()
            && self.save_job.is_none()
            && self
                .filter_progress
                .is_some_and(|progress| progress.done && !progress.cancelled)
    }

    pub fn is_section(&self) -> bool {
        self.session.is_section()
    }

    pub fn section_bounds(&self) -> Option<SectionBounds> {
        self.verified_bounds_override
            .or_else(|| self.session.section_bounds())
    }

    pub fn section_trust(&self) -> Option<BoundaryTrust> {
        self.section_bounds().map(|bounds| bounds.trust)
    }

    pub fn section_first_global_row(&self) -> Option<u64> {
        self.section_bounds()
            .and_then(|bounds| bounds.first_global_row())
    }

    /// Returns the 1-based global data row number for a local physical `row` when a counted scan
    /// proved exact global ordinals, or `None` (*"global row unknown"*).
    pub fn global_data_row_number(&self, local_physical_row: u64) -> Option<u64> {
        let first_physical = self.section_first_global_row()?;
        let global_physical = first_physical.saturating_add(local_physical_row);
        if self.session.dialect.has_header {
            // Global physical row 0 is the header; physical row 1 is data row 1.
            Some(global_physical)
        } else {
            Some(global_physical.saturating_add(1))
        }
    }

    /// Returns the 1-based global data row ordinal for bookmark keying: always `Some` on whole
    /// files, `Some` on counted-scan sections, and `None` on Speculative or uncounted sections.
    pub fn bookmark_global_ordinal(&self, local_row: u64) -> Option<u64> {
        if self.is_section() {
            self.global_data_row_number(local_row)
        } else {
            Some(local_row.saturating_add(1).saturating_sub(self.data_start))
        }
    }

    /// Human-readable row label for one bookmark (explicitly notes when global ordinal is unknown).
    pub fn bookmark_row_label(&self, bookmark: &RowBookmark) -> String {
        if let Some(global) = bookmark.key.global_ordinal {
            format!("Row {}", group_digits(global))
        } else {
            let local = bookmark
                .local_row
                .saturating_add(1)
                .saturating_sub(self.data_start);
            format!("Section row {} (global row unknown)", group_digits(local))
        }
    }

    pub fn can_toggle_bookmark(&self) -> bool {
        if self.source_changed {
            return false;
        }
        if self.filter_active() {
            self.bookmark_target_row().is_some()
        } else {
            self.available_data_rows() > 0
        }
    }

    fn is_loaded_filter_row(&self, row: u64) -> bool {
        self.filtered_rows.iter().any(|matched| matched.row == row)
    }

    /// Resolves the best active target data row for toggling a bookmark from menus or shortcuts.
    pub fn bookmark_target_row(&self) -> Option<u64> {
        let filter_active = self.filter_active();
        let available = self.available_rows();
        let valid = |row: u64| {
            row >= self.data_start
                && if filter_active {
                    self.is_loaded_filter_row(row)
                } else {
                    row < available
                }
        };
        if let Some(selection) = self.selection
            && valid(selection.row())
        {
            return Some(selection.row());
        }
        if let Some(row) = self.selected_rows.first()
            && valid(row)
        {
            return Some(row);
        }
        if let Some(coord) = self.cursor.active()
            && valid(coord.row)
        {
            return Some(coord.row);
        }
        if filter_active {
            return self
                .visible_filter_rows()
                .first()
                .map(|matched| matched.row)
                .filter(|&row| row >= self.data_start);
        }
        let fallback = self.viewport_start.max(self.data_start);
        valid(fallback).then_some(fallback)
    }

    fn resolve_record_offset_for_row(&mut self, local_row: u64) -> Result<Option<u64>, String> {
        if let Some(rel) = local_row.checked_sub(self.buffer_start)
            && let Ok(idx) = usize::try_from(rel)
            && let Some(row) = self.buffered_rows.get(idx)
        {
            return Ok(Some(row.offset));
        }
        if let Some(found) = self
            .filtered_rows
            .iter()
            .find(|matched| matched.row == local_row)
        {
            return Ok(Some(found.record_offset));
        }
        let result = self
            .session
            .record_start_offset(self.index.as_ref(), local_row);
        result.map_err(|error| self.diagnostic_error(error))
    }

    /// Toggles a session-only bookmark on logical physical row `local_row`.
    ///
    /// Returns `Ok(true)` when added and `Ok(false)` when removed. Never marks the document dirty.
    pub fn toggle_bookmark_row(&mut self, local_row: u64) -> Result<bool, String> {
        if self.source_changed {
            return Err(SOURCE_CHANGED_NOTICE.into());
        }
        if local_row < self.data_start {
            return Err("Header rows cannot be bookmarked.".into());
        }
        if local_row >= self.available_rows()
            && !(self.filter_active() && self.is_loaded_filter_row(local_row))
        {
            return Err("Wait for indexing to reach this row before bookmarking it.".into());
        }
        let offset = self.resolve_record_offset_for_row(local_row)?;
        let global_ordinal = self.bookmark_global_ordinal(local_row);
        self.bookmarks
            .toggle(local_row, self.source_generation, offset, global_ordinal)
    }

    /// Toggles a bookmark on the currently selected/active data row.
    pub fn toggle_bookmark_current(&mut self) -> Result<(u64, bool), String> {
        let Some(row) = self.bookmark_target_row() else {
            return Err("Select a data row to bookmark.".into());
        };
        let added = self.toggle_bookmark_row(row)?;
        Ok((row, added))
    }

    pub fn remove_bookmark_row(&mut self, local_row: u64) -> bool {
        self.bookmarks.remove_row(local_row)
    }

    pub fn clear_bookmarks(&mut self) -> usize {
        self.bookmarks.clear()
    }

    /// Returns the next bookmarked row after `from_row` (wrapping around).
    pub fn next_bookmark(&self, from_row: Option<u64>) -> Option<RowBookmark> {
        let anchor = from_row.or_else(|| self.bookmark_target_row());
        self.bookmarks.next_after(anchor).copied()
    }

    /// Returns the previous bookmarked row before `from_row` (wrapping around).
    pub fn previous_bookmark(&self, from_row: Option<u64>) -> Option<RowBookmark> {
        let anchor = from_row.or_else(|| self.bookmark_target_row());
        self.bookmarks.prev_before(anchor).copied()
    }

    /// Resolves any pending bookmark `record_start_offset`s using currently loaded viewport/filter
    /// buffers and `session.first_rows` without issuing disk I/O.
    pub fn refresh_bookmark_offsets_from_buffers(&mut self) {
        let pending = self.bookmarks.pending_offset_rows(self.source_generation);
        if pending.is_empty() {
            return;
        }
        let generation = self.source_generation;
        for row in pending {
            if let Ok(first_idx) = usize::try_from(row)
                && let Some(found) = self.session.first_rows.get(first_idx)
            {
                self.bookmarks
                    .set_record_offset(row, generation, found.offset);
                continue;
            }
            if let Some(rel) = row.checked_sub(self.buffer_start)
                && let Ok(idx) = usize::try_from(rel)
                && let Some(found) = self.buffered_rows.get(idx)
            {
                self.bookmarks
                    .set_record_offset(row, generation, found.offset);
                continue;
            }
            if let Some(found) = self.filtered_rows.iter().find(|matched| matched.row == row) {
                self.bookmarks
                    .set_record_offset(row, generation, found.record_offset);
            }
        }
    }

    /// Cancels and drops any running background bookmark offset resolver.
    pub fn stop_bookmark_offset_job(&mut self) {
        self.bookmark_offset_refresh_pending = false;
        if let Some(job) = self.bookmark_offset_job.take() {
            job.cancel();
        }
    }

    /// Starts an asynchronous, checkpoint-grouped resolver for any bookmarks whose
    /// `record_start_offset` could not be satisfied from loaded buffers.
    ///
    /// Returns `true` when a background worker was spawned.
    pub fn start_bookmark_offset_refresh(&mut self) -> bool {
        if self.is_live_watching() {
            return false;
        }
        self.refresh_bookmark_offsets_from_buffers();
        let generation = self.source_generation;
        let pending = self.bookmarks.pending_offset_rows(generation);
        if pending.is_empty() {
            self.stop_bookmark_offset_job();
            return false;
        }
        let Some(index) = self.index.as_ref().cloned() else {
            return false;
        };
        self.stop_bookmark_offset_job();
        let session = Arc::clone(&self.session);
        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        if std::thread::Builder::new()
            .name("quarry-bookmark-offsets".into())
            .spawn(move || {
                let result = session.record_start_offsets(&index, &pending, &worker_cancel);
                let _ = tx.send(result);
            })
            .is_ok()
        {
            self.bookmark_offset_refresh_pending = false;
            self.bookmark_offset_job = Some(BookmarkOffsetJob {
                generation,
                cancel,
                rx,
            });
            true
        } else {
            self.bookmark_offset_refresh_pending = true;
            false
        }
    }

    /// Polls the background bookmark offset resolver without blocking.
    pub fn poll_bookmark_offsets(&mut self) -> Result<(), String> {
        if self.saving_in_place {
            return Ok(());
        }
        let Some(job) = &self.bookmark_offset_job else {
            return Ok(());
        };
        match job.rx.try_recv() {
            Ok(result) => {
                let job = self
                    .bookmark_offset_job
                    .take()
                    .expect("bookmark offset job is present");
                match result {
                    Ok(Some(resolved)) => {
                        if job.generation == self.source_generation {
                            for (row, offset) in resolved {
                                self.bookmarks
                                    .set_record_offset(row, job.generation, offset);
                            }
                        }
                        Ok(())
                    }
                    Ok(None) => Ok(()),
                    Err(QuarryError::SourceChanged) => {
                        self.invalidate_changed_source();
                        Err(SOURCE_CHANGED_NOTICE.into())
                    }
                    Err(error) => Err(self.diagnostic_error(error)),
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => Ok(()),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.bookmark_offset_job = None;
                Ok(())
            }
        }
    }

    fn refresh_bookmark_offsets_checked(&mut self) -> Result<(), String> {
        self.refresh_bookmark_offsets_from_buffers();
        let Some(index) = self.index.as_ref() else {
            return Ok(());
        };
        let generation = self.source_generation;
        let pending = self.bookmarks.pending_offset_rows(generation);
        if pending.is_empty() {
            return Ok(());
        }
        let cancel = std::sync::atomic::AtomicBool::new(false);
        match self.session.record_start_offsets(index, &pending, &cancel) {
            Ok(Some(resolved)) => {
                for (row, offset) in resolved {
                    self.bookmarks.set_record_offset(row, generation, offset);
                }
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(QuarryError::SourceChanged) => {
                self.invalidate_changed_source();
                Err(SOURCE_CHANGED_NOTICE.into())
            }
            Err(error) => Err(self.diagnostic_error(error)),
        }
    }

    /// Resolves all pending bookmark `record_start_offset`s for the active `source_generation`,
    /// using buffers first and a single checkpoint-grouped scan over `self.index` when available.
    pub fn refresh_bookmark_offsets(&mut self) {
        let _ = self.refresh_bookmark_offsets_checked();
    }

    /// Recomputes `global_ordinal` for every bookmark (e.g. after in-place Verify Boundary).
    pub fn refresh_bookmark_global_ordinals(&mut self) {
        let is_section = self.is_section();
        let data_start = self.data_start;
        let has_header = self.session.dialect.has_header;
        let first_global = self.section_first_global_row();
        self.bookmarks.backfill_global_ordinals(|local_row| {
            if is_section {
                let first_physical = first_global?;
                let global_physical = first_physical.saturating_add(local_row);
                if has_header {
                    Some(global_physical)
                } else {
                    Some(global_physical.saturating_add(1))
                }
            } else {
                Some(local_row.saturating_add(1).saturating_sub(data_start))
            }
        });
    }

    /// Returns a clear explanation when Split-at-Bookmarks is unavailable (including Speculative
    /// sections per Phase 5A-5 / Phase 7A-1), or `None` when ready to snapshot split targets.
    pub fn split_at_bookmarks_disabled_reason(&self) -> Option<String> {
        if self.is_live_watching() {
            return Some("Stop Live Watch before splitting at bookmarks.".into());
        }
        if self.source_changed {
            return Some(SOURCE_CHANGED_NOTICE.into());
        }
        if self.section_trust() == Some(BoundaryTrust::Speculative) {
            return Some(
                "Split at bookmarks is unavailable for speculative sections. Run Data > Verify Section Boundary first."
                    .into(),
            );
        }
        if self.pending_verified_reopen.is_some() {
            return Some(
                "Verified boundaries shifted. Reopen at the verified boundaries before splitting at bookmarks."
                    .into(),
            );
        }
        if self.progress.cancelled {
            return Some(
                "Indexing was cancelled before completing. Reopen the file to finish indexing before splitting at bookmarks."
                    .into(),
            );
        }
        if self.is_indexing_paused() {
            return Some(
                "Indexing is paused. Resume indexing to completion before splitting at bookmarks."
                    .into(),
            );
        }
        if self.job.is_some() || self.index.is_none() || !self.progress.done {
            return Some("Wait for indexing to finish before splitting at bookmarks.".into());
        }
        if self.bookmark_offset_job.is_some() || self.bookmark_offset_refresh_pending {
            return Some(
                "Wait for bookmark byte offsets to finish resolving before splitting at bookmarks."
                    .into(),
            );
        }
        if !self
            .bookmarks
            .iter()
            .any(|bookmark| bookmark.local_row > self.data_start)
        {
            return Some(
                "Bookmark at least one data row after row 1 before splitting at bookmarks.".into(),
            );
        }
        None
    }

    /// Snapshots verified, deduplicated, row-0-dropped bookmark targets for Phase 7 Split-at-Bookmarks.
    pub fn bookmark_split_targets(&mut self) -> Result<Vec<BookmarkSplitTarget>, String> {
        if let Some(reason) = self.split_at_bookmarks_disabled_reason() {
            return Err(reason);
        }
        if let Err(error) = self.session.ensure_source_unchanged() {
            if matches!(error, QuarryError::SourceChanged) {
                self.invalidate_changed_source();
                return Err(SOURCE_CHANGED_NOTICE.into());
            }
            return Err(self.diagnostic_error(error));
        }
        self.refresh_bookmark_offsets_checked()?;
        self.bookmarks.split_targets(self.data_start)
    }

    pub fn speculative_save_warning(&self) -> Option<&'static str> {
        (self.section_trust() == Some(BoundaryTrust::Speculative)).then_some(
            "This section has speculative boundaries: the first or last record may be torn if a quoted field crosses a section boundary. Verify Boundary first or inspect the first and last rows before exporting.",
        )
    }

    pub fn can_verify_section_boundary(&self) -> bool {
        self.is_section()
            && !self.session.section_backed_by_working_copy()
            && !self.source_changed
            && self.section_verify_job.is_none()
            && self.section_bounds().is_some_and(|bounds| {
                bounds.trust == BoundaryTrust::Speculative || bounds.first_global_row().is_none()
            })
    }

    pub fn is_section_verifying(&self) -> bool {
        self.section_verify_job.is_some()
    }

    pub fn is_section_verify_paused(&self) -> bool {
        self.section_verify_job
            .as_ref()
            .is_some_and(VerifyBoundaryJob::is_paused)
    }

    pub fn is_section_verify_pausing(&self) -> bool {
        self.section_verify_job
            .as_ref()
            .is_some_and(|job| job.is_pause_requested() && !job.is_paused())
    }

    pub fn start_verify_section_boundary(
        &mut self,
        parent_index: Option<&StructuralIndex>,
    ) -> Result<(), AppMessage> {
        if !self.is_section() {
            return Err(AppMessage::warning(
                "Verify Boundary is only available for section documents.",
            ));
        }
        if self.session.section_backed_by_working_copy() {
            return Err(AppMessage::warning(
                "Save or discard structural section edits before verifying global row numbers.",
            ));
        }
        if self.source_changed {
            return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
        }
        if self.section_verify_job.is_some() {
            return Err(AppMessage::warning(
                "Boundary verification is already running.",
            ));
        }
        let job = match self.session.start_verify_section_boundary(parent_index) {
            Ok(job) => job,
            Err(QuarryError::SourceChanged) => {
                self.invalidate_changed_source();
                return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            }
            Err(error) => return Err(AppMessage::core_error(error)),
        };
        self.section_verify_progress = Some(job.progress());
        self.section_verify_job = Some(job);
        self.section_verify_status = Some("Verifying section boundaries…".into());
        Ok(())
    }

    pub fn pause_verify_section_boundary(&mut self) -> Result<(), String> {
        let Some(job) = &self.section_verify_job else {
            return Err("No active boundary verification job to pause.".into());
        };
        job.pause();
        self.section_verify_progress = Some(job.progress());
        self.section_verify_status = Some("Pausing boundary verification…".into());
        Ok(())
    }

    pub fn resume_verify_section_boundary(&mut self) -> Result<(), String> {
        let result = {
            let Some(job) = self.section_verify_job.as_mut() else {
                return Err("No paused boundary verification job to resume.".into());
            };
            job.resume()
        };
        if let Err(error) = result {
            if matches!(error, QuarryError::SourceChanged) {
                self.invalidate_changed_source();
                return Err(SOURCE_CHANGED_NOTICE.into());
            }
            return Err(self.diagnostic_error(error));
        }
        if let Some(job) = &self.section_verify_job {
            self.section_verify_progress = Some(job.progress());
        }
        self.section_verify_status = Some("Verifying section boundaries…".into());
        Ok(())
    }

    pub fn cancel_verify_section_boundary(&mut self) {
        if let Some(job) = self.section_verify_job.take() {
            job.cancel();
            let _ = job.wait();
        }
        self.section_verify_progress = None;
        self.section_verify_status = Some("Boundary verification cancelled.".into());
    }

    pub fn poll_section_verify(&mut self) -> Option<Result<VerifyBoundaryOutcome, AppMessage>> {
        let progress = self.section_verify_job.as_ref()?.progress();
        self.section_verify_progress = Some(progress);
        if progress.paused {
            self.section_verify_status = Some("Boundary verification paused.".into());
        }
        if !progress.done {
            return None;
        }
        let job = self.section_verify_job.take()?;
        self.section_verify_progress = None;
        match job.wait() {
            Ok(VerifyBoundaryOutcome::Verified {
                bounds,
                boundary_changed,
            }) => {
                if !boundary_changed {
                    self.verified_bounds_override = Some(bounds);
                    self.pending_verified_reopen = None;
                    if let Ok(upgraded) = self.session.with_verified_section_bounds(bounds) {
                        self.session = Arc::new(upgraded);
                    }
                    if let Some(orig) = self.original_session.as_mut()
                        && let Ok(upgraded) = orig.with_verified_section_bounds(bounds)
                    {
                        *orig = upgraded;
                    }
                    self.refresh_bookmark_global_ordinals();
                    self.section_verify_status = Some(
                        if let Some(first) = self.global_data_row_number(self.data_start) {
                            format!(
                                "Section boundaries verified (starts at row {}).",
                                group_digits(first)
                            )
                        } else {
                            "Section boundaries verified.".into()
                        },
                    );
                } else {
                    self.pending_verified_reopen = Some(bounds);
                    self.section_verify_status = Some(if self.is_dirty() {
                        "Verified boundaries shifted. Save your draft as a new file first, or reopen at the verified bounds."
                            .into()
                    } else {
                        "Verified boundaries shifted. Ready to reopen at exact record boundaries."
                            .into()
                    });
                }
                Some(Ok(VerifyBoundaryOutcome::Verified {
                    bounds,
                    boundary_changed,
                }))
            }
            Ok(VerifyBoundaryOutcome::Cancelled) => {
                self.section_verify_status = Some("Boundary verification cancelled.".into());
                Some(Ok(VerifyBoundaryOutcome::Cancelled))
            }
            Err(QuarryError::SourceChanged) => {
                self.invalidate_changed_source();
                self.section_verify_status = Some(SOURCE_CHANGED_NOTICE.into());
                Some(Err(AppMessage::warning(SOURCE_CHANGED_NOTICE)))
            }
            Err(error) => {
                let msg = AppMessage::core_error(error);
                self.section_verify_status = Some(msg.text.clone());
                Some(Err(msg))
            }
        }
    }

    /// Returns an actionable reason when in-place section splice-save is unavailable for this
    /// document, or `None` when this is a whole file or a `Verified` section ready to splice-save.
    pub fn section_splice_unavailable_reason(&self) -> Option<&'static str> {
        if !self.is_section() {
            return None;
        }
        if self.pending_verified_reopen.is_some() {
            return Some(
                "Verified boundaries shifted. Reopen at the verified boundaries before saving back to the parent file, or use Save Section As New.",
            );
        }
        if self.section_trust() != Some(BoundaryTrust::Verified) {
            return Some(
                "Verify section boundaries before saving changes back to the parent file, or use Save Section As New.",
            );
        }
        if self.has_reordered_columns() {
            return Some(
                "Reset the column layout before saving changes back to the parent file, or use Save Section As New.",
            );
        }
        if !self.header_renames.is_empty() {
            return Some(
                "Header edits cannot be saved back to the parent file. Use Save Section As New instead.",
            );
        }
        None
    }

    pub fn can_splice_save_section(&self) -> bool {
        self.is_section() && self.section_splice_unavailable_reason().is_none()
    }

    pub fn is_save_ready(&self) -> bool {
        !self.is_compressed()
            && self.section_splice_unavailable_reason().is_none()
            && self.is_dirty()
            && self.save_operations_ready()
    }

    pub fn has_reordered_columns(&self) -> bool {
        !self.columns.order.iter().copied().eq(0..self.total_columns)
    }

    pub fn save_as_transformation(&self) -> Option<ColumnTransformation> {
        self.has_reordered_columns()
            .then(|| ColumnTransformation::Arrange {
                source_width: self.total_columns,
                // Visibility never removes data from the saved copy.
                output_columns: self.columns.order.clone(),
            })
    }

    pub fn is_save_as_ready(&self) -> bool {
        let recoverable_section_copy = self.source_changed
            && self.is_section()
            && self.session.section_backed_by_working_copy()
            && !self.join_reserved()
            && self.save_job.is_none()
            && self.export_job.is_none()
            && self.structural_job.is_none();
        recoverable_section_copy
            || ((self.is_dirty()
                || self.has_reordered_columns()
                || self.compressed_save_as_ready()
                || self.is_section())
                && self.save_operations_ready())
    }

    pub fn save_operations_ready(&self) -> bool {
        !self.join_reserved()
            && !self.is_live_watching()
            && !self.source_changed
            && self.save_job.is_none()
            && self.export_job.is_none()
            && self.structural_job.is_none()
            && self.compressed_unavailable_reason().is_none()
    }

    pub fn start_save(&mut self) -> Result<(), AppMessage> {
        self.start_save_operation(None)
    }

    pub fn start_save_as(&mut self, destination: PathBuf) -> Result<(), AppMessage> {
        self.start_save_operation(Some(destination))
    }

    pub fn start_save_operation(&mut self, destination: Option<PathBuf>) -> Result<(), AppMessage> {
        self.start_save_operation_format(destination, false)
    }

    pub fn start_save_operation_format(
        &mut self,
        destination: Option<PathBuf>,
        utf8: bool,
    ) -> Result<(), AppMessage> {
        if self.join_reserved() {
            return Err(AppMessage::warning(JOIN_INPUT_RESERVED));
        }
        if self.source_changed {
            let recoverable_section_copy = destination.is_some()
                && self.is_section()
                && self.session.section_backed_by_working_copy()
                && self.session.ensure_source_unchanged().is_ok();
            if !recoverable_section_copy {
                return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            }
        }
        if self.structural_job.is_some() {
            return Err(AppMessage::warning(
                "Finish or cancel the active change before saving.",
            ));
        }
        if self.save_job.is_some() {
            return Err(AppMessage::warning("A save operation is already running."));
        }
        if self.export_job.is_some() {
            return Err(AppMessage::warning(
                "Cancel the active export before saving.",
            ));
        }
        if utf8 && destination.is_none() {
            return Err(AppMessage::warning(
                "Choose a destination file to save a UTF-8 copy.",
            ));
        }
        if let Some(warning) = destination
            .as_deref()
            .and_then(compressed::gzip_destination_warning)
        {
            return Err(warning);
        }
        if self.is_compressed() {
            if destination.is_none() {
                return Err(AppMessage::warning(compressed::READ_ONLY));
            }
            if let Some(reason) = self.compressed_unavailable_reason() {
                return Err(AppMessage::warning(reason));
            }
        }
        if self.is_section()
            && destination.is_none()
            && let Some(reason) = self.section_splice_unavailable_reason()
        {
            return Err(AppMessage::warning(reason));
        }
        self.commit_edits();
        // A compressed document has no edits; its Save As writes an uncompressed copy.
        // A section document can always be exported via Save Section As New File.
        if !(self.is_dirty()
            || destination.is_some()
                && (self.has_reordered_columns()
                    || utf8
                    || self.is_compressed()
                    || self.is_section()))
        {
            return Err(AppMessage::warning("Make a change before saving."));
        }
        let renames = self
            .header_renames
            .iter()
            .map(|(column, name)| (*column, name.as_bytes().to_vec()))
            .collect();
        let saving_in_place = destination.is_none();
        let save_as_transformation = self.save_as_transformation();
        let result = match destination {
            Some(destination) if utf8 => self.session.start_save_as_utf8(
                renames,
                self.owned_cell_edits(),
                save_as_transformation,
                destination,
            ),
            Some(destination) => match save_as_transformation {
                Some(transformation) => self.session.start_save_as_with_transformation(
                    renames,
                    self.owned_cell_edits(),
                    transformation,
                    destination,
                ),
                None => self.session.start_save_as_with_edits(
                    renames,
                    self.owned_cell_edits(),
                    destination,
                ),
            },
            None if self.has_structural_edits() => self.session.start_save_to_original(
                renames,
                self.owned_cell_edits(),
                self.original_session
                    .as_ref()
                    .expect("a structural working copy retains the original source guard"),
            ),
            None => self
                .session
                .start_save_with_edits(renames, self.owned_cell_edits()),
        };
        let job = match result {
            Ok(job) => job,
            Err(QuarryError::SourceChanged) => {
                self.invalidate_changed_source();
                return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            }
            Err(error) => return Err(AppMessage::core_error(error)),
        };
        if self.filter_active() {
            self.reset_filter_state();
        }
        self.save_output_encoding = Some(if utf8 {
            Encoding::Utf8
        } else {
            self.session.encoding()
        });
        self.save_job = Some(job);
        self.last_splice_summary = None;
        self.save_status = Some(if saving_in_place && self.is_section() {
            "Splicing edited section into parent file…".into()
        } else {
            "Saving edited file…".into()
        });
        self.save_cancel_requested = false;
        self.saving_in_place = saving_in_place;
        Ok(())
    }

    pub fn poll_save(&mut self) -> Result<Option<(PathBuf, bool)>, AppMessage> {
        let Some(job) = self.save_job.as_ref() else {
            return Ok(None);
        };
        let progress = job.progress();
        if !progress.done {
            return Ok(None);
        }

        let job = self.save_job.take().expect("save job is present");
        let in_place = self.saving_in_place;
        self.save_cancel_requested = false;
        self.saving_in_place = false;
        match job.wait() {
            Ok(SaveAsOutcome::Complete(summary)) => {
                self.last_splice_summary = summary.splice.map(|boxed| *boxed);
                Ok(Some((summary.destination, in_place)))
            }
            Ok(SaveAsOutcome::Cancelled) => {
                self.save_status = Some(if in_place {
                    "Save cancelled. Quarry did not replace the current file.".into()
                } else {
                    "Save As cancelled. No output file was created.".into()
                });
                Ok(None)
            }
            Err(QuarryError::SourceChanged) => {
                self.invalidate_changed_source();
                Err(AppMessage::warning(SOURCE_CHANGED_NOTICE))
            }
            Err(error) => {
                self.save_status = Some(if in_place {
                    "Save failed. Quarry did not replace the current file.".into()
                } else {
                    "Save As failed. No output file was created.".into()
                });
                Err(AppMessage::core_error(error))
            }
        }
    }

    pub fn invalidate_changed_source(&mut self) {
        self.shutdown();
        self.source_changed = true;
        self.progress.columns_found = 0;
        self.index = None;
        self.filter_job_tag = None;
        self.live_watch_filter_attach_tag = None;
        self.filter_query_generation = self.filter_query_generation.wrapping_add(1);
        self.filter_index = None;
        self.filter_index_tag = None;
        self.filter_query = None;
        self.filter_refresh_pending = false;
        self.filter_progress = None;
        self.filter_paused_row_limit = None;
        self.search_paused_row_limit = None;
        self.whitespace.invalidate_rows();
        self.buffered_rows.clear();
        self.whitespace.invalidate_rows();
        self.filtered_rows.clear();
        self.filtered_rows_tag = None;
        self.selection = None;
        self.clear_row_selection();
        self.reveal_cell = None;
        self.save_status = Some(SOURCE_CHANGED_NOTICE.into());
    }

    pub fn cancel_save(&mut self) {
        if let Some(job) = &self.save_job {
            job.cancel();
            self.save_cancel_requested = true;
            self.save_status = Some(if self.saving_in_place {
                "Cancelling Save…".into()
            } else {
                "Cancelling Save As…".into()
            });
        }
    }

    pub fn cancel_filter(&self) {
        if let Some(job) = &self.filter_job {
            job.cancel();
        }
        if let Some(attach) = &self.live_watch_filter_attach {
            attach.cancel();
        }
    }

    pub fn clear_filter(&mut self) -> Result<(), String> {
        if self.export_job.is_some() {
            return Err("Cancel the active export before clearing filters.".into());
        }
        self.commit_edits();
        self.reset_filter_state();
        if self.available_data_rows() > 0 {
            self.navigate(self.viewport_start)?;
        }
        Ok(())
    }

    /// Drop filter state without starting a read while a save is being handed off.
    pub fn reset_filter_state(&mut self) {
        if let Some(attach) = self.live_watch_filter_attach.take() {
            attach.cancel();
            drop(attach);
        }
        if let Some(watch) = &self.live_watch {
            watch.clear_filter();
        }
        self.stop_filter_read();
        if let Some(job) = self.filter_job.take() {
            job.cancel();
            drop(job);
        }
        self.filter_job_tag = None;
        self.live_watch_filter_attach_tag = None;
        self.filter_query_generation = self.filter_query_generation.wrapping_add(1);
        self.filter_index = None;
        self.filter_index_tag = None;
        self.filter_query = None;
        self.filter_refresh_pending = false;
        self.filter_progress = None;
        self.filter_status = None;
        self.filter_paused_row_limit = None;
        self.export_progress = None;
        self.export_status = None;
        self.export_cancel_requested = false;
        self.filter_viewport_start = 0;
        self.filter_buffer_start = 0;
        self.whitespace.invalidate_rows();
        self.filtered_rows.clear();
        self.filtered_rows_tag = None;
        self.selection = None;
        self.clear_row_selection();
        self.reveal_cell = None;
        self.scroll_points = 0.0;
    }

    pub fn filter_active(&self) -> bool {
        self.filter_query.is_some()
    }

    pub fn is_filter_ready(&self) -> bool {
        !self.source_changed
            && self.compressed_unavailable_reason().is_none()
            && self.total_columns > 0
            && self.structural_job.is_none()
            && self.save_job.is_none()
    }

    pub fn filter_progress(&self) -> Option<FilterProgress> {
        self.filter_job
            .as_ref()
            .map(FilterJob::progress)
            .or(self.filter_progress)
    }

    pub fn filter_rows_loading(&self) -> bool {
        self.filter_read.is_some() || self.pending_filter_read.is_some()
    }

    pub fn available_filter_rows(&self) -> u64 {
        self.filter_progress
            .map(|progress| progress.matches_found)
            .unwrap_or(0)
            .max(
                self.filter_index
                    .as_ref()
                    .map(FilterIndex::matches_found)
                    .unwrap_or(0),
            )
    }

    pub fn refresh_filter_snapshot_for(&mut self, required_matches: u64) {
        let indexed = self
            .filter_index
            .as_ref()
            .map(FilterIndex::matches_found)
            .unwrap_or(0);
        if indexed >= required_matches {
            return;
        }
        if let Some(job) = &self.filter_job {
            self.filter_index = Some(job.snapshot());
        }
    }

    pub fn is_search_ready(&self) -> bool {
        self.structural_job.is_none()
            && !self.source_changed
            && !self.is_live_watching()
            && ((self.index.is_some() && self.progress.done && !self.progress.cancelled)
                || (self.is_indexing_paused() && self.job.is_some()))
    }

    pub fn cancel_search(&self) {
        if let Some(job) = &self.search_job {
            job.cancel();
        }
    }

    pub fn center_column(&mut self, column: usize) {
        self.commit_edits();
        self.ensure_column_count(column.saturating_add(1));
        self.columns.view(column);
        self.refresh_column_headers();
        self.clear_hidden_selection();
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn view_column(&mut self, column: usize) -> Result<(), String> {
        self.commit_edits();
        self.validate_column(column)?;
        self.columns.view(column);
        self.refresh_column_headers();
        self.reveal_cell = Some((self.current_source_row(), column));
        self.clear_hidden_selection();
        Ok(())
    }

    pub fn set_column_shown(&mut self, column: usize, shown: bool) -> Result<(), String> {
        self.commit_edits();
        self.validate_column(column)?;
        self.columns.set_shown(column, shown);
        self.refresh_column_headers();
        if !shown
            && self
                .reveal_cell
                .is_some_and(|(_, revealed)| revealed == column)
        {
            self.reveal_cell = None;
        }
        self.clear_hidden_selection();
        Ok(())
    }

    pub fn move_column(&mut self, column: usize, position: usize) -> Result<(), String> {
        self.commit_edits();
        self.validate_column(column)?;
        if position >= self.total_columns {
            return Err(format!(
                "Display position must be between 1 and {}.",
                self.total_columns
            ));
        }
        self.columns.move_column(column, position);
        self.refresh_column_headers();
        self.clear_hidden_selection();
        Ok(())
    }

    pub fn reset_columns(&mut self) {
        self.commit_edits();
        self.columns.reset();
        self.refresh_column_headers();
        self.reveal_cell = self
            .columns
            .visible
            .first()
            .copied()
            .map(|column| (self.current_source_row(), column));
        self.clear_hidden_selection();
    }

    pub fn known_width(&self) -> usize {
        self.total_columns.max(self.progress.columns_found)
    }

    pub fn width_confirmed(&self) -> bool {
        self.index.is_some()
            && self.progress.done
            && !self.progress.cancelled
            && !self.source_changed
    }

    pub fn apply_indexed_width(&mut self, width: usize) {
        // Report the full count but bound eager layout allocations for extremely wide rows.
        let layout_width = width.min(quarry_core::MAX_TRANSFORMATION_COLUMNS);
        if layout_width > self.total_columns {
            self.ensure_column_count(layout_width);
            self.refresh_column_headers();
        }
    }

    pub fn ensure_column_count(&mut self, total_columns: usize) {
        if total_columns > self.total_columns {
            self.total_columns = total_columns;
            self.columns.extend_to(total_columns);
        }
    }

    pub fn refresh_column_headers(&mut self) {
        self.headers = self
            .columns
            .visible
            .iter()
            .map(|column| self.column_name(*column))
            .collect();
    }

    pub fn column_name(&self, column: usize) -> String {
        if let Some(edit) = self
            .header_edit
            .as_ref()
            .filter(|edit| edit.column == column)
        {
            return field_text(edit.draft.as_bytes());
        }
        self.header_renames
            .get(&column)
            .map(|name| field_text(name.as_bytes()))
            .unwrap_or_else(|| {
                if self.session.dialect.has_header
                    && let Some(row) = self.live_watch_header_row.as_ref()
                {
                    return row
                        .fields
                        .get(column)
                        .map(|field| field_text(field))
                        .unwrap_or_else(|| format!("Column {}", column + 1));
                }
                column_name(&self.session, column)
            })
    }

    pub fn current_header_fields(&self) -> Vec<Vec<u8>> {
        let mut fields = self
            .live_watch_header_row
            .as_ref()
            .or_else(|| self.session.first_rows.first())
            .map(|row| row.fields.clone())
            .unwrap_or_default();
        for (column, name) in self.header_renames.iter() {
            if let Some(field) = fields.get_mut(*column) {
                *field = name.as_bytes().to_vec();
            }
        }
        if let Some(edit) = &self.header_edit
            && let Some(field) = fields.get_mut(edit.column)
        {
            *field = edit.draft.as_bytes().to_vec();
        }
        fields
    }

    pub fn header_is_editable(&self, column: usize) -> bool {
        !self.is_compressed()
            && !self.is_section()
            && !self.is_live_watching()
            && !self.join_reserved()
            && self.save_job.is_none()
            && self.export_job.is_none()
            && self.search_job.is_none()
            && self.structural_job.is_none()
            && self.source_header_name(column).is_some()
    }

    pub fn source_header_name(&self, column: usize) -> Option<&str> {
        if !self.session.dialect.has_header {
            return None;
        }
        let header_row = self
            .session
            .section_header_row()
            .or_else(|| self.session.first_rows.first())?;
        let field = header_row.fields.get(column)?;
        std::str::from_utf8(field).ok()
    }

    pub fn begin_header_edit(&mut self, column: usize) {
        if !self.header_is_editable(column) {
            return;
        }
        if let Some(source_name) = self.source_header_name(column) {
            let draft = self
                .header_renames
                .get(&column)
                .map(|name| name.as_ref().clone())
                .unwrap_or_else(|| source_name.to_owned());
            self.commit_cell_edit();
            self.commit_header_edit();
            self.header_edit = Some(HeaderEdit {
                column,
                draft,
                focus_requested: true,
            });
        }
    }

    pub fn rename_header(&mut self, column: usize, name: String) -> Result<(), String> {
        if let Some(reason) = self.compressed_read_only_reason() {
            return Err(reason.into());
        }
        if self.is_live_watching() {
            return Err(LIVE_WATCH_READ_ONLY_REASON.into());
        }
        if self.is_section() {
            return Err(
                "Header edits are disabled on section documents. Use Save Section As New to rename columns on a standalone copy.".into(),
            );
        }
        if self.join_reserved() {
            return Err(JOIN_INPUT_RESERVED.into());
        }

        if self.save_job.is_some() {
            return Err("Wait for the save to finish before editing headers.".into());
        }
        if self.export_job.is_some() {
            return Err("Wait for the filtered export to finish before editing headers.".into());
        }
        if self.search_job.is_some() {
            return Err("Wait for the search to finish before editing headers.".into());
        }
        if !self.header_is_editable(column) {
            return Err("Only columns in the source header row can be renamed.".into());
        }
        let source_name = self
            .source_header_name(column)
            .expect("editable header has source text");
        let next = (name.as_bytes() != source_name.as_bytes()).then_some(name);
        if self.header_renames.get(&column).map(|name| name.as_str()) != next.as_deref() {
            self.record_value_edit(EditTarget::Header(column), next.map(String::into_bytes));
        }
        Ok(())
    }

    pub fn commit_header_edit(&mut self) {
        if self.join_reserved() {
            return;
        }

        if self.save_job.is_some() {
            return;
        }
        if let Some(edit) = self.header_edit.take() {
            let _ = self.rename_header(edit.column, edit.draft);
        }
    }

    pub fn cell_is_editable(&self, source: Option<&[u8]>) -> bool {
        self.cell_edit_disabled_reason(source).is_none()
    }

    pub fn cell_edit_disabled_reason(&self, source: Option<&[u8]>) -> Option<&'static str> {
        if let Some(reason) = self.compressed_read_only_reason() {
            return Some(reason);
        }
        if self.is_live_watching() {
            return Some(LIVE_WATCH_READ_ONLY_REASON);
        }
        if self.join_reserved() {
            return Some(JOIN_INPUT_RESERVED);
        }

        if self.source_changed {
            Some("Reopen the changed source before editing data cells.")
        } else if self.save_job.is_some()
            || self.export_job.is_some()
            || self.search_job.is_some()
            || self.filter_job.is_some()
            || self.structural_job.is_some()
        {
            Some("Wait for the active file operation before editing data cells.")
        } else if source.is_none() {
            Some("This row has no cell in this file column.")
        } else if source.is_some_and(|source| std::str::from_utf8(source).is_err()) {
            Some("This cell is not valid UTF-8 and cannot be edited.")
        } else {
            None
        }
    }

    pub fn begin_cell_edit(
        &mut self,
        row: u64,
        column: usize,
        source: Vec<u8>,
    ) -> Result<(), String> {
        if let Some(reason) = self.cell_edit_disabled_reason(Some(&source)) {
            return Err(reason.into());
        }
        self.commit_header_edit();
        self.commit_cell_edit();
        let draft = self
            .cell_edits
            .get(&(row, column))
            .map_or(source.as_slice(), |value| value.as_slice());
        let draft = std::str::from_utf8(draft)
            .expect("editable cell text was checked as UTF-8")
            .to_owned();
        self.selection = Some(GridSelection::Cell { row, column });
        self.clear_row_selection();
        self.cell_edit = Some(CellEdit {
            row,
            column,
            source,
            draft,
            focus_requested: true,
        });
        Ok(())
    }

    pub fn commit_cell_edit(&mut self) {
        if self.join_reserved() {
            return;
        }

        if self.save_job.is_some() {
            return;
        }
        let Some(edit) = self.cell_edit.take() else {
            return;
        };
        let key = (edit.row, edit.column);
        let next =
            (edit.draft.as_bytes() != edit.source.as_slice()).then(|| edit.draft.into_bytes());
        if self.cell_edits.get(&key).map(|value| value.as_slice()) == next.as_deref() {
            return;
        }
        self.record_value_edit(EditTarget::Cell(edit.row, edit.column), next);
        self.clear_edit_search();
    }

    pub fn clear_edit_search(&mut self) {
        self.search_query.clear();
        self.last_match = None;
        self.search_history.clear();
        self.search_history_limits.clear();
        self.search_history_index = None;
        self.search_status = None;
        self.search_paused_row_limit = None;
        self.reveal_cell = None;
    }

    pub fn record_value_edit(&mut self, target: EditTarget, after: Option<Vec<u8>>) {
        if self.is_compressed() {
            return;
        }
        if self.join_reserved() {
            return;
        }

        let after = after.map(|value| value.into_boxed_slice().into_vec());
        let before = match target {
            EditTarget::Header(column) => self
                .header_renames
                .get(&column)
                .map(|name| name.as_bytes().to_vec()),
            EditTarget::Cell(row, column) => self
                .cell_edits
                .get(&(row, column))
                .map(|value| value.as_ref().clone()),
        };
        if before == after {
            return;
        }
        self.invalidate_structural_redo();
        self.set_edit_value(target, after.clone());
        self.edit_history.record(ValueEdit {
            target,
            before,
            after,
        });
    }

    pub fn set_edit_value(&mut self, target: EditTarget, value: Option<Vec<u8>>) {
        if self.join_reserved() {
            return;
        }

        self.whitespace.invalidate();
        match target {
            EditTarget::Header(column) => {
                if let Some(value) = value {
                    Arc::make_mut(&mut self.header_renames).insert(
                        column,
                        Arc::new(
                            String::from_utf8(value).expect("header history contains valid UTF-8"),
                        ),
                    );
                } else {
                    Arc::make_mut(&mut self.header_renames).remove(&column);
                }
                self.refresh_column_headers();
            }
            EditTarget::Cell(row, column) => {
                if let Some(value) = value {
                    Arc::make_mut(&mut self.cell_edits).insert((row, column), Arc::new(value));
                } else {
                    Arc::make_mut(&mut self.cell_edits).remove(&(row, column));
                }
                self.edit_revision = self.edit_revision.wrapping_add(1);
                self.queue_filter_refresh();
            }
        }
    }

    pub fn commit_edits(&mut self) {
        self.commit_header_edit();
        self.commit_cell_edit();
    }

    pub fn discard_header_edits(&mut self) {
        if self.join_reserved() {
            return;
        }

        if self.save_job.is_some() {
            return;
        }
        self.header_edit = None;
        self.whitespace.invalidate();
        self.header_renames = Arc::default();
        self.refresh_column_headers();
    }

    pub fn discard_edits(&mut self) {
        if self.join_reserved() {
            return;
        }

        if self.save_job.is_some() || self.export_job.is_some() || self.structural_job.is_some() {
            return;
        }
        if self.filter_active() && self.clear_filter().is_err() {
            return;
        }
        self.discard_header_edits();
        self.cell_edit = None;
        self.cell_edits = Arc::default();
        self.edit_revision = self.edit_revision.wrapping_add(1);
        self.edit_history = EditHistory::default();
        self.selection = None;
        self.clear_row_selection();
        self.reveal_cell = None;
        self.search_query.clear();
        self.last_match = None;
        self.search_history.clear();
        self.search_history_limits.clear();
        self.search_history_index = None;
        self.search_status = None;
        self.search_paused_row_limit = None;
        if self.session.path() == self.logical_path {
            self.working_copy = None;
            self.original_session = None;
        }
    }

    pub fn has_cell_edits(&self) -> bool {
        !self.cell_edits.is_empty()
            || self
                .cell_edit
                .as_ref()
                .is_some_and(|edit| edit.draft.as_bytes() != edit.source.as_slice())
    }

    pub fn is_dirty(&self) -> bool {
        let header_dirty = self.header_edit.as_ref().map_or_else(
            || !self.header_renames.is_empty(),
            |edit| {
                self.header_renames
                    .keys()
                    .any(|column| *column != edit.column)
                    || edit.draft.as_bytes()
                        != self
                            .source_header_name(edit.column)
                            .expect("active header edit has source text")
                            .as_bytes()
            },
        );
        header_dirty || self.has_cell_edits() || self.has_structural_edits()
    }

    pub fn has_structural_edits(&self) -> bool {
        !self.is_compressed() && self.session.path() != self.logical_path
    }

    pub fn history_ready(&self) -> bool {
        !self.join_reserved()
            && !self.is_live_watching()
            && !self.source_changed
            && !self.filter_refresh_pending
            && self.header_edit.is_none()
            && self.cell_edit.is_none()
            && self.search_job.is_none()
            && self.filter_job.is_none()
            && self.save_job.is_none()
            && self.export_job.is_none()
            && self.structural_job.is_none()
    }

    pub fn can_undo(&self) -> bool {
        self.history_ready() && (!self.edit_history.undo.is_empty() || self.can_undo_structural())
    }

    pub fn can_redo(&self) -> bool {
        self.history_ready() && (!self.edit_history.redo.is_empty() || self.can_redo_structural())
    }

    pub fn can_undo_structural(&self) -> bool {
        !self.filter_active()
            && self
                .working_copy
                .as_ref()
                .is_some_and(|state| state.undo.is_some())
            && self.header_renames.is_empty()
            && !self.has_cell_edits()
            && self.header_edit.is_none()
            && self.cell_edit.is_none()
            && self.save_job.is_none()
            && self.export_job.is_none()
            && self.structural_job.is_none()
    }

    pub fn can_redo_structural(&self) -> bool {
        !self.filter_active()
            && self
                .working_copy
                .as_ref()
                .is_some_and(|state| state.redo.is_some())
            && self.header_edit.is_none()
            && self.cell_edit.is_none()
            && self.save_job.is_none()
            && self.export_job.is_none()
            && self.structural_job.is_none()
    }

    pub fn validate_column(&self, column: usize) -> Result<(), String> {
        if column < self.total_columns {
            Ok(())
        } else if self.total_columns == 0 {
            Err("This file has no columns.".into())
        } else {
            Err(format!(
                "File column must be between 1 and {}.",
                self.total_columns
            ))
        }
    }

    pub fn index_status(&self) -> &'static str {
        if self.is_live_watching() {
            if self.live_watch_tail_follow {
                "Live Watch"
            } else {
                "Live Watch (paused scroll)"
            }
        } else if self.source_changed {
            "Source changed"
        } else if let Some(status) = self.compressed_index_status() {
            status
        } else if self.job.is_none() && self.index.is_none() {
            "Index failed"
        } else if self.progress.cancelled {
            "Index cancelled"
        } else if self.job.is_some() {
            "Indexing"
        } else {
            "Index complete"
        }
    }

    pub fn shutdown(&mut self) {
        if let Some(attach) = self.live_watch_filter_attach.take() {
            attach.cancel();
            drop(attach);
        }
        if let Some(watch) = self.live_watch.take() {
            drop(watch);
        }
        self.live_watch_tail_follow = false;
        self.stop_decompression();
        self.stop_bookmark_offset_job();
        self.whitespace.invalidate();
        self.stop_filter_read();
        if let Some(job) = self.structural_job.take() {
            match job {
                StructuralJob::FindingDuplicates { job, .. } => job.cancel(),
                StructuralJob::DuplicatePreview { summary, .. } => {
                    self.remove_working_file(&summary.destination);
                }
                StructuralJob::AnalyzingSplit { job, .. } => job.cancel(),
                StructuralJob::Materializing { job, .. } => job.cancel(),
                StructuralJob::Replacing { job, .. } => job.cancel(),
                StructuralJob::Cleaning { job, .. } => job.cancel(),
                StructuralJob::Sorting { job, .. } => job.cancel(),
                StructuralJob::DeletingRows { job, .. }
                | StructuralJob::InsertingRows { job, .. } => job.cancel(),
            }
        }
        self.structural_cancel_requested = false;
        if let Some(job) = self.save_job.take() {
            job.cancel();
            drop(job);
        }
        self.save_cancel_requested = false;
        self.saving_in_place = false;
        if let Some(job) = self.section_verify_job.take() {
            job.cancel();
            drop(job);
        }
        self.section_verify_progress = None;
        if let Some(job) = self.export_job.take() {
            job.cancel();
            drop(job);
        }
        self.export_cancel_requested = false;
        if let Some(job) = self.filter_job.take() {
            job.cancel();
            drop(job);
        }
        if let Some(job) = self.search_job.take() {
            job.cancel();
            drop(job);
        }
        if let Some(job) = self.job.take() {
            drop(job);
        }
    }

    pub fn stop_filter_read(&mut self) {
        self.pending_filter_read = None;
        if let Some(active) = self.filter_read.take() {
            active.job.cancel_without_waiting();
        }
    }

    pub fn cancel(&self) {
        if let Some(job) = &self.job {
            job.cancel();
        }
    }

    pub fn available_rows(&self) -> u64 {
        if self.is_live_watching() {
            return self
                .index
                .as_ref()
                .map_or(self.progress.rows_scanned, StructuralIndex::indexed_rows);
        }
        self.index
            .as_ref()
            .map(StructuralIndex::indexed_rows)
            .unwrap_or(self.progress.rows_scanned)
            .max(self.session.first_rows.len() as u64)
    }

    pub fn available_data_rows(&self) -> u64 {
        self.available_rows().saturating_sub(self.data_start)
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub fn indexed_fraction(&self) -> f32 {
        if self.session.file_size == 0 && (self.progress.done || self.index.is_some()) {
            1.0
        } else {
            self.progress.bytes_scanned as f32 / self.session.file_size.max(1) as f32
        }
    }

    pub fn set_visible_rows(&mut self, visible_rows: usize) -> Result<(), String> {
        let visible_rows = visible_rows.max(1);
        if visible_rows == self.visible_rows {
            return Ok(());
        }
        self.commit_edits();
        self.visible_rows = visible_rows;
        if self.filter_active() {
            return self.navigate_filter(self.filter_viewport_start);
        }
        if self.available_data_rows() == 0 {
            return Ok(());
        }
        let target = self.reveal_cell.map_or(self.viewport_start, |(row, _)| row);
        self.navigate(target)
    }

    pub fn page(&mut self, direction: i64) -> Result<(), String> {
        let page = i64::try_from(self.visible_rows).unwrap_or(i64::MAX);
        self.scroll_rows(direction.saturating_mul(page))
    }

    pub fn scroll_by_points(&mut self, delta_y: f32, row_stride: f32) -> Result<(), String> {
        let row_stride = row_stride.max(f32::EPSILON);
        self.scroll_points -= delta_y;
        let rows = (self.scroll_points / row_stride).trunc() as i64;
        if rows == 0 {
            return Ok(());
        }
        self.scroll_points -= rows as f32 * row_stride;
        self.scroll_rows(rows)
    }

    /// Keep controls from clamping a restored page against an incomplete match count.
    pub fn filter_waiting_for_page(&self) -> bool {
        self.filter_refresh_pending
            || (self.filter_job.is_some()
                || self.filter_progress.is_some_and(|progress| !progress.done))
                && self.filter_viewport_start
                    > max_viewport_start(self.available_filter_rows(), self.visible_rows)
    }

    pub fn scroll_rows(&mut self, rows: i64) -> Result<(), String> {
        if self.filter_active() {
            self.commit_edits();
            if self.filter_waiting_for_page() {
                return Ok(());
            }
            let available = self.available_filter_rows();
            let maximum = max_viewport_start(available, self.visible_rows);
            let current = self.filter_viewport_start;
            let target = if rows.is_negative() {
                current.saturating_sub(rows.unsigned_abs())
            } else {
                current.saturating_add(rows as u64).min(maximum)
            };
            if target == current {
                self.scroll_points = 0.0;
                return Ok(());
            }
            if self.is_live_watching() {
                if rows.is_negative() || target < maximum {
                    self.live_watch_tail_follow = false;
                } else if target >= maximum {
                    self.live_watch_tail_follow = true;
                }
            }
            return self.navigate_filter(target);
        }
        let available = self.available_data_rows();
        let maximum = max_viewport_start(available, self.visible_rows);
        let current = self.viewport_start.saturating_sub(self.data_start);
        let target = if rows.is_negative() {
            current.saturating_sub(rows.unsigned_abs())
        } else {
            current.saturating_add(rows as u64).min(maximum)
        };
        if target == current {
            self.scroll_points = 0.0;
            return Ok(());
        }
        if self.is_live_watching() {
            if rows.is_negative() || target < maximum {
                self.live_watch_tail_follow = false;
            } else if target >= maximum {
                self.live_watch_tail_follow = true;
            }
        }
        self.navigate(self.data_start.saturating_add(target))
    }

    pub fn navigate_filter(&mut self, requested: u64) -> Result<(), String> {
        if self.filter_refresh_pending {
            return Ok(());
        }
        let available = self.available_filter_rows();
        let scan_incomplete = self.filter_job.is_some()
            || self.filter_progress.is_some_and(|progress| !progress.done);
        if scan_incomplete && requested > max_viewport_start(available, self.visible_rows) {
            self.cancel_filter_read_for_navigation();
            self.filter_viewport_start = requested;
            self.clear_hidden_selection();
            return Ok(());
        }
        if available == 0 {
            self.cancel_filter_read_for_navigation();
            self.filter_viewport_start = 0;
            self.filter_buffer_start = 0;
            self.whitespace.invalidate_rows();
            self.filtered_rows.clear();
            self.clear_hidden_selection();
            return Ok(());
        }
        let start = requested.min(max_viewport_start(available, self.visible_rows));
        self.schedule_filter_buffer(start)?;
        self.filter_viewport_start = start;
        self.clear_hidden_selection();
        Ok(())
    }

    pub fn navigate(&mut self, requested: u64) -> Result<(), String> {
        self.commit_edits();
        let available = self.available_rows();
        if requested.max(self.data_start) >= available {
            return Err(format!(
                "Data row {} is not indexed yet ({} available).",
                requested.saturating_sub(self.data_start).saturating_add(1),
                self.available_data_rows()
            ));
        }
        let start = logical_viewport_start(
            requested,
            self.data_start,
            self.available_data_rows(),
            self.visible_rows,
        );
        if self.is_live_watching() {
            let max_start = self.data_start.saturating_add(max_viewport_start(
                self.available_data_rows(),
                self.visible_rows,
            ));
            self.live_watch_tail_follow = start >= max_start;
        }
        self.load_buffer(start)?;
        self.viewport_start = start;
        self.clear_hidden_selection();
        Ok(())
    }

    pub fn load_buffer(&mut self, viewport_start: u64) -> Result<(), String> {
        let available = self.available_rows();
        if available <= self.data_start || viewport_start >= available {
            self.buffer_start = self.data_start;
            if !self.buffered_rows.is_empty() {
                self.whitespace.invalidate_rows();
                self.buffered_rows.clear();
            }
            return Ok(());
        }
        let visible_end = viewport_start.saturating_add(
            available
                .saturating_sub(viewport_start)
                .min(self.visible_rows as u64),
        );
        let loaded_end = self
            .buffer_start
            .saturating_add(self.buffered_rows.len() as u64);
        let capacity = self.visible_rows.saturating_add(2 * OVERSCAN_ROWS) as u64;
        if self.buffer_start <= viewport_start
            && loaded_end >= visible_end
            && self.buffered_rows.len() as u64 <= capacity
        {
            return Ok(());
        }

        let last_start = available.saturating_sub(capacity).max(self.data_start);
        let start = viewport_start
            .saturating_sub(OVERSCAN_ROWS as u64)
            .max(self.data_start)
            .min(last_start);
        let count = available.saturating_sub(start).min(capacity) as usize;
        let end = start.saturating_add(count as u64);
        let began = Instant::now();
        let rows = if let Some(watch) = &self.live_watch {
            watch
                .read_rows(start, count)
                .map_err(|error| self.diagnostic_error(error))?
        } else if end <= self.session.first_rows.len() as u64 {
            self.session.first_rows[start as usize..end as usize].to_vec()
        } else if let Some(index) = &self.index {
            self.session
                .read_rows(index, start, count)
                .map_err(|error| self.diagnostic_error(error))?
        } else if let Some(rows) = self.read_decompressed_rows(start, count) {
            rows.map_err(|error| self.diagnostic_error(error))?
        } else if let Some(job) = &self.job {
            self.session
                .read_rows(&job.snapshot(), start, count)
                .map_err(|error| self.diagnostic_error(error))?
        } else {
            return Err("No structural index is available.".into());
        };
        let loaded_columns = rows.iter().map(|row| row.fields.len()).max().unwrap_or(0);
        if loaded_columns > self.total_columns {
            self.ensure_column_count(loaded_columns);
            self.refresh_column_headers();
        }
        self.last_viewport_read = Some(began.elapsed());
        self.buffer_start = start;
        self.whitespace.invalidate_rows();
        self.buffered_rows = rows.into_iter().map(Arc::new).collect();
        self.refresh_bookmark_offsets_from_buffers();
        Ok(())
    }

    /// Updates viewport position for `position` and returns a [`ViewportReadPlan`] when an
    /// uncached indexed disk read is needed so callers can execute `Session::read_rows` off
    /// the UI thread. Cached buffers, bootstrap `first_rows`, and active filters (which already
    /// use background `FilterReadJob` workers) are handled immediately and return `Ok(None)`.
    pub fn prepare_async_buffer_read(
        &mut self,
        position: u64,
        visible_rows: usize,
    ) -> Result<Option<ViewportReadPlan>, String> {
        self.visible_rows = visible_rows.max(1);
        self.commit_edits();
        if self.filter_active() {
            if !self.filter_waiting_for_page() {
                self.navigate_filter(position)?;
            }
            return Ok(None);
        }
        let available = self.available_rows();
        let requested = self.data_start.saturating_add(position);
        if available <= self.data_start || requested >= available {
            self.buffer_start = self.data_start;
            if !self.buffered_rows.is_empty() {
                self.whitespace.invalidate_rows();
                self.buffered_rows.clear();
            }
            return Ok(None);
        }
        let viewport_start = logical_viewport_start(
            requested,
            self.data_start,
            self.available_data_rows(),
            self.visible_rows,
        );
        let prev_viewport_start = self.viewport_start;
        self.viewport_start = viewport_start;
        if self.is_live_watching() {
            let max_start = self.data_start.saturating_add(max_viewport_start(
                self.available_data_rows(),
                self.visible_rows,
            ));
            if viewport_start < prev_viewport_start && viewport_start < max_start {
                self.live_watch_tail_follow = false;
            } else if viewport_start >= max_start {
                self.live_watch_tail_follow = true;
            }
        }
        self.clear_hidden_selection();

        let visible_end = viewport_start.saturating_add(
            available
                .saturating_sub(viewport_start)
                .min(self.visible_rows as u64),
        );
        let loaded_end = self
            .buffer_start
            .saturating_add(self.buffered_rows.len() as u64);
        let capacity = self.visible_rows.saturating_add(2 * OVERSCAN_ROWS) as u64;
        if self.buffer_start <= viewport_start
            && loaded_end >= visible_end
            && self.buffered_rows.len() as u64 <= capacity
        {
            return Ok(None);
        }

        let last_start = available.saturating_sub(capacity).max(self.data_start);
        let start = viewport_start
            .saturating_sub(OVERSCAN_ROWS as u64)
            .max(self.data_start)
            .min(last_start);
        let count = available.saturating_sub(start).min(capacity) as usize;
        let end = start.saturating_add(count as u64);
        if let Some(watch) = &self.live_watch {
            return Ok(Some(ViewportReadPlan {
                session: Arc::clone(&self.session),
                index: watch.index_snapshot(),
                live_watch_reader: Some(watch.reader()),
                start,
                count,
            }));
        }
        if end <= self.session.first_rows.len() as u64 {
            let began = Instant::now();
            let rows = self.session.first_rows[start as usize..end as usize].to_vec();
            self.install_buffer_rows(start, rows, began.elapsed());
            return Ok(None);
        }
        if let Some(index) = &self.index {
            return Ok(Some(ViewportReadPlan {
                session: Arc::clone(&self.session),
                index: index.clone(),
                live_watch_reader: None,
                start,
                count,
            }));
        }
        if let Some(rows) = self.read_decompressed_rows(start, count) {
            let began = Instant::now();
            let rows = rows.map_err(|error| self.diagnostic_error(error))?;
            self.install_buffer_rows(start, rows, began.elapsed());
            return Ok(None);
        }
        if let Some(job) = &self.job {
            return Ok(Some(ViewportReadPlan {
                session: Arc::clone(&self.session),
                index: job.snapshot(),
                live_watch_reader: None,
                start,
                count,
            }));
        }
        Err("No structural index is available.".into())
    }

    /// Installs rows fetched by an off-UI-thread [`ViewportReadPlan`].
    pub fn install_buffer_rows(&mut self, start: u64, rows: Vec<Row>, elapsed: Duration) {
        let loaded_columns = rows.iter().map(|row| row.fields.len()).max().unwrap_or(0);
        if loaded_columns > self.total_columns {
            self.ensure_column_count(loaded_columns);
            self.refresh_column_headers();
        }
        self.last_viewport_read = Some(elapsed);
        self.buffer_start = start;
        self.whitespace.invalidate_rows();
        self.buffered_rows = rows.into_iter().map(Arc::new).collect();
    }

    pub fn filter_read_window(&self, viewport_start: u64) -> Option<FilterReadWindow> {
        let available = self.available_filter_rows();
        if available == 0 {
            return None;
        }
        let capacity = self.visible_rows.saturating_add(2 * OVERSCAN_ROWS) as u64;
        let last_start = available.saturating_sub(capacity);
        let start_match = viewport_start
            .saturating_sub(OVERSCAN_ROWS as u64)
            .min(last_start);
        let count = (available - start_match).min(capacity) as usize;
        Some(FilterReadWindow { start_match, count })
    }

    pub fn filter_buffer_covers(&self, viewport_start: u64) -> bool {
        let available = self.available_filter_rows();
        if available == 0 || viewport_start >= available {
            return false;
        }
        let visible_end = viewport_start.saturating_add(
            available
                .saturating_sub(viewport_start)
                .min(self.visible_rows as u64),
        );
        let loaded_end = self
            .filter_buffer_start
            .saturating_add(self.filtered_rows.len() as u64);
        let capacity = self.visible_rows.saturating_add(2 * OVERSCAN_ROWS) as u64;
        self.filter_buffer_start <= viewport_start
            && loaded_end >= visible_end
            && self.filtered_rows.len() as u64 <= capacity
    }

    pub fn schedule_filter_buffer(&mut self, viewport_start: u64) -> Result<(), String> {
        self.schedule_filter_buffer_inner(viewport_start, false)
    }

    fn schedule_filter_buffer_inner(
        &mut self,
        viewport_start: u64,
        is_tail_follow: bool,
    ) -> Result<(), String> {
        let Some(window) = self.filter_read_window(viewport_start) else {
            self.cancel_filter_read_for_navigation();
            self.filter_buffer_start = 0;
            self.whitespace.invalidate_rows();
            self.filtered_rows.clear();
            return Ok(());
        };

        if self.filtered_rows.len() > window.count && window.start_match >= self.filter_buffer_start
        {
            let offset =
                usize::try_from(window.start_match.saturating_sub(self.filter_buffer_start))
                    .unwrap_or(self.filtered_rows.len());
            if offset.saturating_add(window.count) <= self.filtered_rows.len() {
                self.filtered_rows.drain(..offset);
                self.filtered_rows.truncate(window.count);
                self.filter_buffer_start = window.start_match;
            }
        }

        if self.filter_buffer_covers(viewport_start) {
            self.cancel_filter_read_for_navigation();
            return Ok(());
        }

        if self.live_watch.is_none() {
            self.refresh_filter_snapshot_for(
                window.start_match.saturating_add(window.count as u64),
            );
        }
        if let Some(active) = self.filter_read.as_mut() {
            if active.window == window && !active.cancel_requested {
                self.pending_filter_read = None;
            } else if is_tail_follow && active.tail_follow && !active.cancel_requested {
                // Keep one live-watch filtered tail read in flight and coalesce subsequent tail
                // window updates instead of cancelling and discarding completed results.
                self.pending_filter_read = Some(window);
            } else {
                if !active.cancel_requested {
                    active.job.cancel();
                    active.cancel_requested = true;
                }
                self.pending_filter_read = Some(window);
            }
            return Ok(());
        }

        self.start_filter_read_with_mode(window, is_tail_follow)
    }

    pub fn start_filter_read(&mut self, window: FilterReadWindow) -> Result<(), String> {
        self.start_filter_read_with_mode(window, false)
    }

    fn start_filter_read_with_mode(
        &mut self,
        window: FilterReadWindow,
        tail_follow: bool,
    ) -> Result<(), String> {
        self.pending_filter_read = None;
        let job = if let Some(watch) = &self.live_watch {
            watch
                .start_filtered_read(window.start_match, window.count)
                .map_err(|error| self.diagnostic_error(error))?
        } else {
            let index = self
                .filter_index
                .as_ref()
                .ok_or_else(|| "No filter index is available.".to_owned())?;
            self.session
                .start_filtered_read(index, window.start_match, window.count)
                .map_err(|error| {
                    if matches!(error, QuarryError::SourceChanged) {
                        self.invalidate_changed_source();
                        SOURCE_CHANGED_NOTICE.to_owned()
                    } else {
                        self.diagnostic_error(error)
                    }
                })?
        };
        let tag = self.current_filter_tag();
        self.filter_read = Some(ActiveFilterRead {
            tag,
            window,
            job,
            started: Instant::now(),
            cancel_requested: false,
            tail_follow,
        });
        Ok(())
    }

    pub fn cancel_filter_read_for_navigation(&mut self) {
        self.pending_filter_read = None;
        if let Some(active) = self.filter_read.as_mut()
            && !active.cancel_requested
        {
            active.job.cancel();
            active.cancel_requested = true;
        }
    }

    pub fn visible_rows(&self) -> &[Arc<Row>] {
        let offset = usize::try_from(self.viewport_start.saturating_sub(self.buffer_start))
            .unwrap_or(self.buffered_rows.len())
            .min(self.buffered_rows.len());
        let end = offset
            .saturating_add(self.visible_rows)
            .min(self.buffered_rows.len());
        &self.buffered_rows[offset..end]
    }

    pub fn visible_filter_rows(&self) -> &[Arc<FilterMatch>] {
        let Some(relative) = self
            .filter_viewport_start
            .checked_sub(self.filter_buffer_start)
        else {
            return &[];
        };
        let offset = usize::try_from(relative)
            .unwrap_or(self.filtered_rows.len())
            .min(self.filtered_rows.len());
        let end = offset
            .saturating_add(self.visible_rows)
            .min(self.filtered_rows.len());
        &self.filtered_rows[offset..end]
    }

    pub fn visible_row_count(&self) -> usize {
        if self.filter_active() {
            self.visible_filter_rows().len()
        } else {
            self.visible_rows().len()
        }
    }

    pub fn visible_row(&self, index: usize) -> Option<(u64, &[Vec<u8>])> {
        if self.filter_active() {
            self.visible_filter_rows()
                .get(index)
                .map(|row| (row.row, row.fields.as_slice()))
        } else {
            self.visible_rows().get(index).map(|row| {
                (
                    self.viewport_start.saturating_add(index as u64),
                    row.fields.as_slice(),
                )
            })
        }
    }

    pub fn source_cell(&self, row: u64, column: usize) -> Option<&[u8]> {
        if self.filter_active() {
            return self
                .filtered_rows
                .iter()
                .find(|value| value.row == row)?
                .fields
                .get(column)
                .map(Vec::as_slice);
        }
        let index = usize::try_from(row.checked_sub(self.buffer_start)?).ok()?;
        self.buffered_rows
            .get(index)?
            .fields
            .get(column)
            .map(|value| value.as_slice())
    }

    pub fn effective_cell(&self, row: u64, column: usize) -> Option<&[u8]> {
        self.cell_edits
            .get(&(row, column))
            .map(|value| value.as_slice())
            .or_else(|| self.source_cell(row, column))
    }

    pub fn cell_value<'a>(&'a self, row: u64, column: usize, source: &'a [u8]) -> &'a [u8] {
        if let Some(edit) = self
            .cell_edit
            .as_ref()
            .filter(|edit| edit.row == row && edit.column == column)
        {
            return edit.draft.as_bytes();
        }
        self.cell_edits
            .get(&(row, column))
            .map_or(source, |value| value.as_slice())
    }

    pub fn header_value(&self, column: usize) -> Option<&[u8]> {
        if !self.session.dialect.has_header {
            return None;
        }
        if let Some(name) = self.header_renames.get(&column) {
            return Some(name.as_bytes());
        }
        self.session
            .section_header_row()
            .or_else(|| self.session.first_rows.first())?
            .fields
            .get(column)
            .map(|value| value.as_slice())
    }

    /// Share immutable loaded rows with the inspector; never copy huge source cells on a frame.
    pub fn whitespace_input(&self, row: u64, column: usize) -> whitespace::Input {
        if let Some(value) = self.cell_edits.get(&(row, column)) {
            return whitespace::Input::Owned(value.clone());
        }
        if self.filter_active() {
            let found = self
                .filtered_rows
                .iter()
                .find(|value| value.row == row)
                .expect("inspected filtered row is loaded");
            whitespace::Input::Filtered(found.clone(), column)
        } else {
            let index = usize::try_from(row.saturating_sub(self.buffer_start))
                .expect("loaded row offset fits usize");
            whitespace::Input::Row(self.buffered_rows[index].clone(), column)
        }
    }

    pub fn whitespace_header_input(&self, column: usize) -> whitespace::Input {
        self.header_renames.get(&column).map_or_else(
            || whitespace::Input::Header(self.session.clone(), column),
            |name| whitespace::Input::Text(name.clone()),
        )
    }

    /// Queries the bounded async whitespace cache for a loaded cell without cloning on cache hits.
    pub fn inspect_cell_whitespace(
        &mut self,
        row: u64,
        column: usize,
        raw_len: usize,
    ) -> Option<whitespace::Summary> {
        let id = whitespace::CellId {
            row: Some(row),
            column,
        };
        let cell_edits = &self.cell_edits;
        let filter_active = self.filter_active();
        let filtered_rows = &self.filtered_rows;
        let buffered_rows = &self.buffered_rows;
        let buffer_start = self.buffer_start;
        self.whitespace.get(id, raw_len, || {
            if let Some(value) = cell_edits.get(&(row, column)) {
                return whitespace::Input::Owned(value.clone());
            }
            if filter_active {
                let found = filtered_rows
                    .iter()
                    .find(|value| value.row == row)
                    .expect("inspected filtered row is loaded");
                whitespace::Input::Filtered(found.clone(), column)
            } else {
                let index = usize::try_from(row.saturating_sub(buffer_start))
                    .expect("loaded row offset fits usize");
                whitespace::Input::Row(buffered_rows[index].clone(), column)
            }
        })
    }

    /// Queries the bounded async whitespace cache for a column header without cloning on cache hits.
    pub fn inspect_header_whitespace(
        &mut self,
        column: usize,
        raw_len: usize,
    ) -> Option<whitespace::Summary> {
        let id = whitespace::CellId { row: None, column };
        let header_renames = &self.header_renames;
        let session = &self.session;
        self.whitespace.get(id, raw_len, || {
            header_renames.get(&column).map_or_else(
                || whitespace::Input::Header(session.clone(), column),
                |name| whitespace::Input::Text(name.clone()),
            )
        })
    }

    /// Core jobs own their snapshots; copying is deferred until starting a data operation.
    pub fn owned_cell_edits(&self) -> BTreeMap<(u64, usize), Vec<u8>> {
        self.cell_edits
            .iter()
            .map(|(key, value)| {
                #[cfg(any(test, feature = "test-hooks"))]
                OWNED_EDIT_SNAPSHOT_BYTES.with(|bytes| {
                    bytes.set(bytes.get().saturating_add(value.len()));
                });
                (*key, value.as_ref().clone())
            })
            .collect()
    }

    pub fn row_with_edits(&self, row: u64, fields: &[Vec<u8>]) -> Vec<Vec<u8>> {
        fields
            .iter()
            .enumerate()
            .map(|(column, source)| self.cell_value(row, column, source).to_vec())
            .collect()
    }

    /// Clears both unfiltered physical row selections and filtered result-ordinal selections.
    pub fn clear_row_selection(&mut self) {
        self.selected_rows.clear();
        self.selected_filtered_rows.clear();
    }

    /// Returns the number of selected rows in the active view (matching result ordinals while
    /// filtered, or physical source rows when unfiltered).
    pub fn selected_row_count(&self) -> u64 {
        if self.filter_active() {
            self.selected_filtered_rows.count()
        } else {
            self.selected_rows.count()
        }
    }

    /// Reports whether rendered `(table_row, source_row)` is highlighted as a selected row.
    pub fn is_table_row_selected(&self, table_row: usize, source_row: u64) -> bool {
        if self.filter_active() {
            self.selected_filtered_rows.contains(table_row as u64)
        } else {
            self.selected_rows.contains(source_row)
        }
    }

    /// Resolves a loaded filtered `source_row` to its result ordinal (`0..matches_found`).
    pub fn filtered_ordinal_for_source_row(&self, source_row: u64) -> Option<u64> {
        self.filtered_rows
            .iter()
            .position(|r| r.row == source_row)
            .map(|rel| self.filter_buffer_start.saturating_add(rel as u64))
    }

    pub fn current_source_row(&self) -> u64 {
        self.visible_row(0)
            .map(|(row, _)| row)
            .unwrap_or(self.viewport_start)
    }

    pub fn clear_hidden_selection(&mut self) {
        self.selected_columns.retain(|column| {
            self.columns
                .hidden
                .get(*column)
                .is_some_and(|hidden| !hidden)
        });
        if !self
            .column_selection_anchor
            .is_some_and(|anchor| self.selected_columns.contains(&anchor))
        {
            self.column_selection_anchor = self
                .columns
                .order
                .iter()
                .copied()
                .find(|column| self.selected_columns.contains(column));
        }
        let Some(selection) = self.selection else {
            return;
        };
        let row_visible = if self.filter_active() {
            self.visible_filter_rows()
                .iter()
                .any(|row| row.row == selection.row())
        } else {
            let row_end = self
                .viewport_start
                .saturating_add(self.visible_rows().len() as u64);
            (self.viewport_start..row_end).contains(&selection.row())
        };
        let column_visible = match selection {
            GridSelection::Row { .. } => true,
            GridSelection::Cell { column, .. } => self.columns.visible.contains(&column),
        };
        if !row_visible || !column_visible {
            self.selection = None;
        }
    }

    pub fn copy_selection_text(&self) -> Result<String, String> {
        let selection = self
            .selection
            .ok_or_else(|| "Select a cell or row before copying.".to_owned())?;
        let fields = if self.filter_active() {
            let row = self
                .visible_filter_rows()
                .iter()
                .find(|row| row.row == selection.row())
                .ok_or_else(|| {
                    "The selected row is no longer visible. Select it again.".to_owned()
                })?;
            self.row_with_edits(row.row, &row.fields)
        } else {
            let relative = selection
                .row()
                .checked_sub(self.buffer_start)
                .ok_or_else(|| {
                    "The selected row is no longer visible. Select it again.".to_owned()
                })?;
            let offset = usize::try_from(relative).map_err(|_| {
                "The selected row is no longer visible. Select it again.".to_owned()
            })?;
            let row = self.buffered_rows.get(offset).ok_or_else(|| {
                "The selected row is no longer visible. Select it again.".to_owned()
            })?;
            self.row_with_edits(selection.row(), &row.fields)
        };
        selection_fields_text(&fields, selection, MAX_COPY_BYTES)
    }

    pub fn grid_total_rows(&self) -> u64 {
        if self.filter_active() {
            self.available_filter_rows()
        } else {
            self.available_data_rows()
        }
    }

    pub fn grid_position(&self) -> u64 {
        if self.filter_active() {
            self.filter_viewport_start
        } else {
            self.viewport_start.saturating_sub(self.data_start)
        }
    }

    pub fn navigate_grid_position(&mut self, position: u64) -> Result<(), String> {
        self.commit_edits();
        if self.filter_active() {
            if self.filter_waiting_for_page() {
                return Ok(());
            }
            self.navigate_filter(position)
        } else {
            self.navigate(self.data_start.saturating_add(position))
        }
    }

    pub fn display_start(&self) -> u64 {
        if self.available_data_rows() == 0 {
            return 0;
        }
        self.viewport_start
            .saturating_sub(self.data_start)
            .saturating_add(1)
    }

    pub fn display_end(&self) -> u64 {
        if self.visible_rows().is_empty() {
            return 0;
        }
        self.display_start()
            .saturating_add(self.visible_rows().len().saturating_sub(1) as u64)
    }
}

impl Drop for Document {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl Document {
    pub fn storage_record_count(&self) -> u64 {
        self.index.as_ref().map_or(
            self.session.file_size.saturating_add(1),
            StructuralIndex::indexed_rows,
        )
    }

    pub fn materialization_storage_estimate(
        &self,
        transformation: Option<&ColumnTransformation>,
        replacement: Option<&LiteralReplacement>,
    ) -> u64 {
        self.materialization_storage_estimate_for_records(
            transformation,
            replacement,
            if transformation.is_some() || replacement.is_some() {
                self.storage_record_count()
            } else {
                0
            },
        )
    }

    pub fn materialization_storage_estimate_for_records(
        &self,
        transformation: Option<&ColumnTransformation>,
        replacement: Option<&LiteralReplacement>,
        records: u64,
    ) -> u64 {
        quarry_core::estimate_edited_output_bytes_from_values(
            self.session.file_size,
            records,
            self.header_renames
                .iter()
                .map(|(column, value)| (*column, value.as_bytes())),
            self.cell_edits
                .iter()
                .map(|(key, value)| (*key, value.as_slice())),
            transformation,
            replacement,
        )
        .saturating_mul(if self.session.encoding() == Encoding::Utf8 {
            1
        } else {
            3
        })
    }

    pub fn reordered_storage_estimate(&self) -> u64 {
        quarry_core::estimate_edited_output_bytes_from_values(
            self.session.file_size,
            self.storage_record_count(),
            self.header_renames
                .iter()
                .map(|(column, value)| (*column, value.as_bytes())),
            self.cell_edits
                .iter()
                .map(|(key, value)| (*key, value.as_slice())),
            None,
            None,
        )
        .saturating_mul(if self.session.encoding() == Encoding::Utf8 {
            1
        } else {
            3
        })
    }

    pub fn retained_storage_paths(&self) -> Vec<PathBuf> {
        let Some(state) = self.working_copy.as_ref() else {
            return Vec::new();
        };
        std::iter::once(self.session.path())
            .chain(
                state
                    .undo
                    .iter()
                    .chain(state.redo.iter())
                    .map(|snapshot| snapshot.path.as_path()),
            )
            .filter(|path| state.owns(path))
            .map(Path::to_path_buf)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

pub struct DocumentLease {
    pub counter: Arc<std::sync::atomic::AtomicUsize>,
    pub generation: Option<Arc<quarry_core::WorkingCopyStorage>>,
}
impl Drop for DocumentLease {
    fn drop(&mut self) {
        // Release generation ownership before allowing another document action.
        drop(self.generation.take());
        self.counter
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

type CellEditMap = BTreeMap<(u64, usize), Arc<Vec<u8>>>;

/// Weak metadata signatures detect changed drafts without retaining idle input data.
pub struct DocumentProbe {
    pub tab_id: u64,
    pub session: std::sync::Weak<Session>,
    pub headers: std::sync::Weak<BTreeMap<usize, Arc<String>>>,
    pub cells: std::sync::Weak<CellEditMap>,
    pub header_cells: bool,
}
impl DocumentProbe {
    pub fn capture(tab_id: u64, document: &Document) -> Self {
        Self {
            tab_id,
            session: Arc::downgrade(&document.session),
            headers: Arc::downgrade(&document.header_renames),
            cells: Arc::downgrade(&document.cell_edits),
            header_cells: Self::has_header_cells(document),
        }
    }
    pub fn has_header_cells(document: &Document) -> bool {
        document.session.dialect.has_header
            && document
                .cell_edits
                .first_key_value()
                .is_some_and(|((row, _), _)| *row == 0)
    }

    pub fn matches(&self, tab_id: u64, document: &Document) -> bool {
        self.tab_id == tab_id
            && !document.source_changed
            && self.session.ptr_eq(&Arc::downgrade(&document.session))
            && self
                .headers
                .ptr_eq(&Arc::downgrade(&document.header_renames))
            && (self.cells.ptr_eq(&Arc::downgrade(&document.cell_edits))
                || !self.header_cells && !Self::has_header_cells(document))
    }
}

pub struct CapturedDocument {
    pub path: PathBuf,
    pub source: JoinWorkingSource,
    pub probe: DocumentProbe,
    pub columns: usize,
}

impl Document {
    pub fn join_reserved(&self) -> bool {
        self.join_reservations
            .load(std::sync::atomic::Ordering::Acquire)
            != 0
    }

    pub fn join_capture_disabled_reason(&self) -> Option<&'static str> {
        if let Some(reason) = self.compressed_join_reason() {
            return Some(reason);
        }
        if self.join_reserved() {
            return Some(JOIN_INPUT_RESERVED);
        }
        if self.filter_active() || self.filter_refresh_pending {
            return Some("Clear this document's filters before using it in a join.");
        }
        if self.source_changed {
            return Some("Reopen this changed document before using it in a join.");
        }
        if self.job.is_some()
            || self.pending_materialization.is_some()
            || self.save_job.is_some()
            || self.export_job.is_some()
            || self.structural_job.is_some()
            || self.search_job.is_some()
            || self.filter_job.is_some()
        {
            return Some("Wait for this document's active operation to finish before joining.");
        }
        None
    }

    pub fn commit_join_drafts(&mut self) -> Result<(), String> {
        if let Some(reason) = self.join_capture_disabled_reason() {
            return Err(reason.into());
        }
        let limit = JoinConfig::default().max_record_bytes;
        if self
            .header_edit
            .as_ref()
            .is_some_and(|edit| edit.draft.len() > limit)
            || self
                .cell_edit
                .as_ref()
                .is_some_and(|edit| edit.draft.len() > limit)
        {
            return Err(
                "An editor draft exceeds the record limit. Shorten it before joining.".into(),
            );
        }
        if self
            .header_edit
            .as_ref()
            .is_some_and(|edit| !self.header_is_editable(edit.column))
        {
            return Err("Finish or cancel the invalid header draft before joining.".into());
        }
        if let Some(edit) = &self.cell_edit
            && self.cell_edit_disabled_reason(Some(&edit.source)).is_some()
        {
            return Err("Finish or cancel the invalid cell draft before joining.".into());
        }
        self.commit_edits();
        Ok(())
    }

    pub fn capture_join_document(&mut self, tab_id: u64) -> Result<CapturedDocument, String> {
        if self.is_section() {
            return Err(
                "Open-tab Join is not supported for bounded section tabs yet; save the section as a new file to join it."
                    .into(),
            );
        }
        self.commit_join_drafts()?;
        if self.has_structural_edits() && self.original_session.is_none() {
            return Err("The original source cannot be identified. Save a copy and reopen it before joining.".into());
        }
        let generation =
            if self.has_structural_edits() {
                let name = self
                    .session
                    .path()
                    .file_name()
                    .ok_or("Missing private generation name.")?;
                Some(self.working_copy.as_ref()
                .and_then(|state| std::iter::once(&state.directory)
                    .chain(state.previous_directories.iter())
                    .find(|directory| directory.has_generation_name(name)))
                .cloned()
                .ok_or("The current working generation is no longer owned. Reopen this document.")?)
            } else {
                None
            };
        let probe = DocumentProbe::capture(tab_id, self);
        self.join_reservations
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let owner = Arc::new(DocumentLease {
            counter: Arc::clone(&self.join_reservations),
            generation,
        });
        let source = JoinWorkingSource::capture(
            Arc::clone(&self.session),
            self.original_session.as_ref().unwrap_or(&self.session),
            Arc::clone(&self.header_renames),
            Arc::clone(&self.cell_edits),
            owner,
        );
        Ok(CapturedDocument {
            path: self.logical_path.clone(),
            source,
            probe,
            columns: self.total_columns,
        })
    }
}

pub fn footer_range_text(document: &Document) -> String {
    if document.filter_active() {
        let total = document.available_filter_rows();
        let visible = document.visible_row_count() as u64;
        if document.is_filter_scanning() && total == 0 {
            return "Finding matching rows…".into();
        }
        if document.filter_rows_loading() && visible == 0 {
            return "Loading matching rows…".into();
        }
        if total == 0 {
            return "No matching rows".into();
        }
        if visible == 0 {
            return format!("{} filter matches", group_digits(total));
        }
        let start = document.filter_viewport_start.saturating_add(1);
        let end = document
            .filter_viewport_start
            .saturating_add(visible)
            .min(total);
        format!(
            "Matches {}–{} of {}",
            group_digits(start),
            group_digits(end),
            group_digits(total)
        )
    } else {
        format!(
            "Rows {}–{} of {}",
            group_digits(document.display_start()),
            group_digits(document.display_end()),
            group_digits(document.available_data_rows())
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::SelectionModifiers;
    use crate::{
        MAX_GRID_TEXT_CHARS, grid_field_text, parse_data_row, parse_file_column,
        row_for_scroll_fraction, scroll_fraction_for_row,
    };
    use quarry_core::{FilterOperator, FilterQuery, SearchMatch};
    use std::fs::{self, File};
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    const ROW_HEIGHT: f32 = 22.0;

    fn commit_test_cell(document: &mut Document, row: u64, column: usize, value: &str) {
        let source = document.source_cell(row, column).unwrap().to_vec();
        document.begin_cell_edit(row, column, source).unwrap();
        document.cell_edit.as_mut().unwrap().draft = value.into();
        document.commit_cell_edit();
    }

    fn selection_text(
        row: &Row,
        selection: GridSelection,
        max_bytes: usize,
    ) -> Result<String, String> {
        selection_fields_text(&row.fields, selection, max_bytes)
    }

    fn finish_index(document: &mut Document) {
        if document.job.is_some() {
            await_index_worker(document);
            document.poll().unwrap();
        } else {
            assert!(
                document.index.is_some() && document.progress.done,
                "document must either have an active index job or an adopted completed index"
            );
        }
    }

    fn finish_search(document: &mut Document) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !document
            .search_job
            .as_ref()
            .expect("search job should be active")
            .progress()
            .done
        {
            assert!(Instant::now() < deadline, "search timed out");
            std::thread::yield_now();
        }
        document.poll_search().unwrap();
    }

    fn finish_filter(document: &mut Document) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            document.poll_filter().unwrap();
            if document.filter_job.is_none() && !document.filter_rows_loading() {
                break;
            }
            assert!(Instant::now() < deadline, "filter timed out");
            std::thread::yield_now();
        }
    }

    fn finish_filter_read(document: &mut Document) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while document.filter_rows_loading() {
            document.poll_filter().unwrap();
            assert!(Instant::now() < deadline, "filter read timed out");
            std::thread::yield_now();
        }
    }

    fn finish_filtered_export(document: &mut Document) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while document.export_job.is_some() {
            document.poll_filtered_export().unwrap();
            assert!(Instant::now() < deadline, "filtered export timed out");
            std::thread::yield_now();
        }
    }

    fn await_index_worker(doc: &Document) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !doc.job.as_ref().unwrap().progress().done {
            assert!(Instant::now() < deadline, "index timed out");
            std::thread::yield_now();
        }
    }

    #[test]
    fn header_renames_keep_lossless_source_identity_and_clear_when_restored() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-header-rename-{name}.csv"));
        let long_header = "x".repeat(140);
        let original_header = "\u{feff}line\nname";
        let source = format!("\"{original_header}\",,{long_header}\nfirst,second,third\n");
        fs::write(&path, source.as_bytes()).unwrap();
        let mut document = Document::prepare(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();

        assert_eq!(document.source_header_name(0), Some(original_header));
        assert_eq!(document.source_header_name(1), Some(""));
        assert_eq!(document.source_header_name(2), Some(long_header.as_str()));
        assert!(document.column_name(2).ends_with("..."));

        document.begin_header_edit(0);
        assert_eq!(
            document
                .header_edit
                .as_ref()
                .map(|edit| edit.draft.as_str()),
            Some(original_header)
        );
        document.header_edit.as_mut().unwrap().draft = "renamed".into();
        document.move_column(0, 2).unwrap();
        assert!(document.is_dirty());
        assert_eq!(document.column_name(0), "renamed");

        document.set_column_shown(0, false).unwrap();
        document.set_column_shown(0, true).unwrap();
        assert_eq!(document.column_name(0), "renamed");
        assert_eq!(
            document.header_renames.get(&0).map(|value| value.as_str()),
            Some("renamed")
        );

        document.rename_header(0, original_header.into()).unwrap();
        assert!(!document.is_dirty());
        document.rename_header(1, "Column 2".into()).unwrap();
        assert!(document.is_dirty());
        document.rename_header(1, String::new()).unwrap();
        assert!(!document.is_dirty());
        assert_eq!(fs::read(&path).unwrap(), source.as_bytes());

        document.shutdown();
        let mut no_header = Document::prepare(
            &path,
            OpenOptions {
                header_mode: HeaderMode::NoHeader,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        assert!(!no_header.header_is_editable(0));
        no_header.begin_header_edit(0);
        assert!(no_header.header_edit.is_none());
        assert_eq!(
            no_header.rename_header(0, "renamed".into()).unwrap_err(),
            "Only columns in the source header row can be renamed."
        );
        no_header.shutdown();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn filtered_export_reports_success_and_never_overwrites() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-export-success-{name}.csv"));
        let destination =
            std::env::temp_dir().join(format!("quarry-export-success-{name}-filtered.csv"));
        let source = b"id,note,status\n1,\"line one\nline two\",keep\n2,skip,skip\n3,\"quote \"\"yes\"\"\",keep\n";
        let expected =
            b"id,note,status\n1,\"line one\nline two\",keep\n3,\"quote \"\"yes\"\"\",keep\n";
        fs::write(&path, source).unwrap();
        let mut document = Document::prepare(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let query = FilterQuery::single(2, FilterOperator::Equals, b"keep".to_vec());
        document.start_filter(query.clone()).unwrap();
        finish_filter(&mut document);

        document.start_filtered_export(destination.clone()).unwrap();
        assert_eq!(
            document.rename_header(0, "renamed".into()).unwrap_err(),
            "Wait for the filtered export to finish before editing headers."
        );
        assert_eq!(
            document.clear_filter().unwrap_err(),
            "Cancel the active export before clearing filters."
        );
        assert_eq!(
            document.start_filter(query.clone()).unwrap_err(),
            "Cancel the active export before changing filters."
        );
        finish_filtered_export(&mut document);

        assert_eq!(fs::read(&destination).unwrap(), expected);
        let status = document.export_status.as_deref().unwrap();
        assert!(status.contains("2 rows"));
        assert!(status.contains(&destination.display().to_string()));
        assert_eq!(
            document
                .start_filtered_export(destination.clone())
                .unwrap_err(),
            "export destination already exists"
        );
        assert_eq!(fs::read(&destination).unwrap(), expected);
        assert_eq!(
            document.start_filtered_export(path.clone()).unwrap_err(),
            "export destination must differ from the source file"
        );
        assert_eq!(fs::read(&path).unwrap(), source);

        document.start_filter(query).unwrap();
        assert!(document.export_progress.is_none());
        assert!(document.export_status.is_none());
        finish_filter(&mut document);
        document.shutdown();
        for path in [path, destination] {
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn filtered_grid_is_match_only_bounded_and_keeps_source_column_identity() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-filter-grid-{name}.csv"));
        let mut file = File::create(&path).unwrap();
        writeln!(file, "id,status,note").unwrap();
        for row in 1..=200 {
            let status = if row % 3 == 0 { "keep" } else { "skip" };
            writeln!(file, "{row},{status},note{row}").unwrap();
        }
        drop(file);

        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        document.move_column(1, 0).unwrap();
        document.set_column_shown(1, false).unwrap();
        let order = document.columns.order.clone();
        let hidden = document.columns.hidden.clone();

        document
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::Equals,
                b"keep".to_vec(),
            ))
            .unwrap();
        finish_filter(&mut document);
        document.set_visible_rows(7).unwrap();

        assert_eq!(document.available_filter_rows(), 66);
        assert_eq!(document.filter_viewport_start, 0);
        assert_eq!(document.visible_row_count(), 7);
        assert!(document.visible_filter_rows().iter().all(|row| {
            row.fields.get(1).map(|value| value.as_slice()) == Some(b"keep".as_slice())
        }));
        assert_eq!(document.visible_filter_rows()[0].match_ordinal, 0);
        assert_eq!(document.visible_filter_rows()[0].row, 3);
        assert_eq!(document.columns.order, order);
        assert_eq!(document.columns.hidden, hidden);

        let first_row = document.visible_filter_rows()[0].row;
        document.selection = Some(GridSelection::Cell {
            row: first_row,
            column: 2,
        });
        assert_eq!(document.copy_selection_text().unwrap(), "note3");
        document.selection = Some(GridSelection::Row { row: first_row });
        assert_eq!(document.copy_selection_text().unwrap(), "3\tkeep\tnote3");

        document.page(1).unwrap();
        assert_eq!(document.filter_viewport_start, 7);
        assert_eq!(document.visible_filter_rows()[0].match_ordinal, 7);
        assert_eq!(document.visible_filter_rows()[0].row, 24);
        assert!(document.filtered_rows.len() <= document.visible_rows + 2 * OVERSCAN_ROWS);

        let final_start =
            max_viewport_start(document.available_filter_rows(), document.visible_rows);
        document.navigate_filter(final_start).unwrap();
        assert!(document.filter_rows_loading());
        finish_filter_read(&mut document);
        assert_eq!(document.visible_filter_rows().last().unwrap().row, 198);
        assert!(document.filtered_rows.len() <= document.visible_rows + 2 * OVERSCAN_ROWS);
        document.page(-1).unwrap();
        assert_eq!(
            document.filter_viewport_start,
            final_start.saturating_sub(document.visible_rows as u64)
        );

        document.clear_filter().unwrap();
        assert!(!document.filter_active());
        assert!(document.filtered_rows.is_empty());
        assert_eq!(document.columns.order, order);
        assert_eq!(document.columns.hidden, hidden);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn active_filter_navigation_refreshes_a_stale_snapshot_before_reading() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-filter-stale-{name}.csv"));
        let prefix_path =
            std::env::temp_dir().join(format!("quarry-filter-stale-prefix-{name}.csv"));
        let mut file = File::create(&path).unwrap();
        file.write_all(b"name,status\n").unwrap();
        file.write_all(&b"row,keep\n".repeat(2_000)).unwrap();
        drop(file);
        let mut prefix = File::create(&prefix_path).unwrap();
        prefix.write_all(b"name,status\n").unwrap();
        prefix.write_all(&b"row,keep\n".repeat(1_000)).unwrap();
        drop(prefix);

        let mut document = Document::prepare(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        document.visible_rows = 5;
        let query = FilterQuery::single(1, FilterOperator::Equals, b"keep".to_vec());
        let stale_session = Session::open(
            &prefix_path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        let stale_job = stale_session.start_filter(query.clone()).unwrap();
        let stale_index = stale_job.wait().unwrap();
        let stale_matches = stale_index.matches_found();
        assert_eq!(stale_matches, 1_000);

        document.start_filter(query).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let progress = loop {
            let progress = document.filter_job.as_ref().unwrap().progress();
            if progress.done {
                break progress;
            }
            assert!(Instant::now() < deadline, "filter did not complete");
            std::thread::yield_now();
        };
        assert_eq!(progress.matches_found, 2_000);
        document.filter_progress = Some(progress);
        document.filter_index = Some(stale_index);
        let target = max_viewport_start(progress.matches_found, document.visible_rows);
        let required = target
            .saturating_add(document.visible_rows as u64)
            .min(progress.matches_found);
        assert!(stale_matches < required);

        document.navigate_filter(target).unwrap();
        assert!(document.filter_rows_loading());
        finish_filter_read(&mut document);

        assert!(document.filter_index.as_ref().unwrap().matches_found() >= required);
        assert_eq!(document.visible_filter_rows()[0].match_ordinal, target);
        assert!(document.filtered_rows.len() <= document.visible_rows + 2 * OVERSCAN_ROWS);
        document.shutdown();
        for path in [path, prefix_path] {
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn filter_navigation_is_async_and_latest_request_wins() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-filter-latest-{name}.csv"));
        let mut file = File::create(&path).unwrap();
        writeln!(file, "id,status").unwrap();
        for row in 0..25_000 {
            let status = if row % 20 == 0 { "keep" } else { "skip" };
            writeln!(file, "{row},{status}").unwrap();
        }
        drop(file);

        let mut document = Document::prepare(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        document.visible_rows = 5;
        document
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::Equals,
                b"keep".to_vec(),
            ))
            .unwrap();
        finish_filter(&mut document);

        document.navigate_filter(100).unwrap();
        assert!(document.filter_read.is_some());
        assert!(document.visible_filter_rows().is_empty());
        assert!(document.filter_rows_loading());
        assert_eq!(footer_range_text(&document), "Loading matching rows…");

        document.navigate_filter(500).unwrap();
        assert!(document.filter_read.as_ref().unwrap().cancel_requested);
        document.navigate_filter(900).unwrap();
        assert_eq!(
            document.pending_filter_read,
            document.filter_read_window(900)
        );
        assert_eq!(document.filter_viewport_start, 900);

        finish_filter_read(&mut document);
        assert_eq!(document.visible_filter_rows()[0].match_ordinal, 900);
        assert_eq!(document.visible_filter_rows()[0].fields[0], b"18000");
        assert!(
            document
                .visible_filter_rows()
                .iter()
                .all(|row| row.fields[1] == b"keep")
        );
        assert!(document.filtered_rows.len() <= document.visible_rows + 2 * OVERSCAN_ROWS);
        assert!(document.filter_read.is_none());
        assert!(document.pending_filter_read.is_none());

        document.shutdown();
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn grid_previews_preserve_long_unicode_values_and_bound_huge_fields() {
        for value in [
            "description ".repeat(150),
            "é界🙂".repeat(600),
            "quoted\r\nmultiline ".repeat(100),
        ] {
            assert_eq!(
                grid_field_text(value.as_bytes()),
                value.replace('\n', "\\n").replace('\r', "\\r")
            );
            assert_eq!(field_text(value.as_bytes()).chars().count(), 120);
        }
        let limit = MAX_GRID_TEXT_CHARS;
        let exact = "🙂".repeat(limit);
        assert_eq!(grid_field_text(exact.as_bytes()), exact);
        let huge = "🙂".repeat(limit * 128);
        assert_eq!(
            grid_field_text(huge.as_bytes()),
            "🙂".repeat(limit - 3) + "..."
        );
        assert_eq!(grid_field_text(b"a\xffb\n"), "a�b\\n");
        // The byte lookahead ends inside an emoji; the retained prefix remains valid.
        let boundary = "a".to_owned() + &huge;
        let preview = grid_field_text(boundary.as_bytes());
        assert!(!preview.contains('�'));
        assert_eq!(preview.chars().count(), limit);
    }

    #[test]
    fn opening_discovers_late_columns_and_preserves_the_working_view() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ragged.csv");
        let source = format!("{}4,\"a,b\",\"c\nd\",,,6", "1,2,3\n".repeat(1000));
        fs::write(&path, &source).unwrap();
        let mut doc = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::NoHeader,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        assert_eq!(doc.total_columns, 3);
        assert!(!doc.width_confirmed());
        doc.move_column(2, 0).unwrap();
        doc.set_column_shown(1, false).unwrap();
        doc.begin_cell_edit(0, 2, b"3".to_vec()).unwrap();
        doc.cell_edit.as_mut().unwrap().draft = "unsaved value".into();
        let selection = doc.selection;
        await_index_worker(&doc);
        doc.poll().unwrap();
        assert!(doc.width_confirmed());
        assert_eq!(doc.known_width(), 6);
        assert_eq!(doc.columns.visible, [2, 0, 3, 4, 5]);
        assert_eq!(
            doc.headers,
            ["Column 3", "Column 1", "Column 4", "Column 5", "Column 6"]
        );
        assert_eq!(doc.selection, selection);
        assert_eq!(doc.cell_edit.as_ref().unwrap().draft, "unsaved value");
        assert_eq!(doc.viewport_start, 0);
        assert_eq!(fs::read_to_string(path).unwrap(), source);
    }

    #[test]
    fn discovery_counts_columns_beyond_the_eager_layout_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wide.csv");
        let limit = quarry_core::MAX_TRANSFORMATION_COLUMNS;
        fs::write(
            &path,
            format!("{}{}", "1,2\n".repeat(1000), ",".repeat(limit + 2)),
        )
        .unwrap();
        let mut doc = Document::open(
            &path,
            OpenOptions {
                delimiter: Some(b','),
                header_mode: HeaderMode::NoHeader,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        await_index_worker(&doc);
        doc.poll().unwrap();
        assert!(doc.width_confirmed());
        assert_eq!(doc.known_width(), limit + 3);
        assert_eq!(doc.total_columns, limit);
    }

    #[test]
    fn column_discovery_respects_the_save_destination() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.csv");
        let copy = dir.path().join("copy.csv");
        for in_place in [true, false] {
            fs::write(&source, "name,id\nAda,1\n").unwrap();
            let mut doc = Document::open(
                &source,
                OpenOptions {
                    header_mode: HeaderMode::FirstRow,
                    ..Default::default()
                },
            )
            .unwrap();
            doc.rename_header(0, "person".into()).unwrap();
            await_index_worker(&doc);
            if in_place {
                doc.start_save().unwrap();
            } else {
                doc.start_save_as(copy.clone()).unwrap();
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            while !doc.save_job.as_ref().unwrap().progress().done {
                assert!(Instant::now() < deadline, "save timed out");
                std::thread::yield_now();
            }
            // Exercise the same completion order as the app when both workers finished
            // between frames, including the new source identity after in-place save.
            doc.poll().unwrap();
            if in_place {
                assert!(!doc.width_confirmed());
            } else {
                assert!(doc.width_confirmed());
                assert_eq!(doc.known_width(), 2);
                assert_eq!(fs::read_to_string(&source).unwrap(), "name,id\nAda,1\n");
            }
            let destination = if in_place { &source } else { &copy };
            assert_eq!(
                doc.poll_save().unwrap(),
                Some((destination.clone(), in_place))
            );
            assert_eq!(
                fs::read_to_string(destination).unwrap(),
                "person,id\nAda,1\n"
            );
            assert!(!doc.source_changed);
        }
    }

    #[test]
    fn a_replaced_source_cannot_confirm_a_completed_scan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.csv");
        fs::write(&path, "1,2\n").unwrap();
        let mut doc = Document::open(&path, OpenOptions::default()).unwrap();
        await_index_worker(&doc);
        let replacement = dir.path().join("replacement");
        fs::write(&replacement, "3,4,5\n").unwrap();
        fs::rename(replacement, path).unwrap();
        assert!(doc.poll().is_err());
        assert!(doc.source_changed);
        assert!(!doc.width_confirmed());
    }

    #[test]
    fn row_selection_modifiers_are_compact_and_match_editor_conventions() {
        let mut selected = RowSelection::default();

        selected.select(2, SelectionModifiers::NONE);
        assert_eq!(selected.ranges, vec![2..=2]);
        assert_eq!(selected.anchor, Some(2));

        selected.select(100_000_000, SelectionModifiers::SHIFT);
        assert_eq!(selected.ranges, vec![2..=100_000_000]);
        assert_eq!(selected.count(), 99_999_999);
        assert_eq!(selected.ranges.len(), 1);
        assert_eq!(selected.anchor, Some(2));

        selected.select(10, SelectionModifiers::NONE);
        selected.select(20, SelectionModifiers::COMMAND);
        assert_eq!(selected.ranges, vec![10..=10, 20..=20]);
        assert_eq!(selected.anchor, Some(20));

        selected.select(10, SelectionModifiers::COMMAND);
        assert_eq!(selected.ranges, vec![20..=20]);
        selected.select(20, SelectionModifiers::COMMAND);
        assert!(selected.is_empty());
        assert_eq!(selected.anchor, None);
    }

    #[test]
    fn filtering_clears_row_selection_and_blocks_row_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("filtered-row-deletion.csv");
        fs::write(&source, b"name,city\nAda,London\nGrace,Arlington\n").unwrap();
        let mut document = Document::open(
            &source,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        document.selected_rows.select(1, SelectionModifiers::NONE);
        document.selection = Some(GridSelection::Row { row: 1 });

        document
            .start_filter(FilterQuery::single(
                0,
                FilterOperator::Equals,
                b"Ada".to_vec(),
            ))
            .unwrap();
        assert!(document.selected_rows.is_empty());
        assert!(document.selection.is_none());
        assert_eq!(
            document
                .start_delete_rows(std::iter::once(1..=1).collect())
                .unwrap_err(),
            "Clear the filter before editing the document."
        );
        assert!(document.structural_job.is_none());
        assert!(document.working_copy.is_none());
    }

    #[test]
    fn sort_progress_advances_through_phases_without_resetting() {
        let mut previous = 0.0;
        for (preparing, bytes, done, label, animated) in [
            (true, 0, false, "Preparing to sort…", true),
            (true, 25, false, "Preparing to sort · counting 25.0%", true),
            (
                true,
                100,
                false,
                "Preparing to sort · counting 100.0%",
                true,
            ),
            (false, 0, false, "Sorting rows · 0.0% read", false),
            (false, 50, false, "Sorting rows · 50.0% read", false),
            (false, 99, false, "Sorting rows · 99.0% read", false),
            (false, 100, false, "Merging sorted rows…", true),
            (false, 100, true, "Sorting rows · 100.0%", false),
        ] {
            let progress = sort_progress(
                preparing,
                if preparing { bytes } else { 0 },
                bytes,
                100,
                done,
            );
            assert!(progress.fraction >= previous);
            assert_eq!(progress.label, label);
            assert_eq!(progress.animate, animated);
            if !done {
                assert!(progress.fraction <= 0.9);
            }
            previous = progress.fraction;
        }
        assert_eq!(previous, 1.0);
        assert_eq!(sort_progress(false, 0, 0, 0, false).fraction, 0.9);
        assert_eq!(sort_progress(false, 0, 0, 0, true).fraction, 1.0);
    }

    #[test]
    fn selected_cells_and_rows_copy_full_bounded_text() {
        let row = Row {
            offset: 0,
            fields: vec![
                vec![b'x'; 200],
                b"has\ttab".to_vec(),
                b"say \"hi\"".to_vec(),
                Vec::new(),
            ],
        };
        let cell = GridSelection::Cell { row: 0, column: 0 };
        assert_eq!(selection_text(&row, cell, 200).unwrap(), "x".repeat(200));
        assert!(selection_text(&row, cell, 199).is_err());

        let selected_row = GridSelection::Row { row: 0 };
        assert_eq!(
            selection_text(&row, selected_row, 1024).unwrap(),
            format!("{}\t\"has\ttab\"\t\"say \"\"hi\"\"\"\t", "x".repeat(200))
        );

        let invalid = Row {
            offset: 0,
            fields: vec![vec![0xff]],
        };
        assert!(selection_text(&invalid, cell, 1024).is_err());
    }

    #[test]
    fn find_next_navigates_resumes_resets_and_reports_no_match() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-find-{name}.csv"));
        let mut file = File::create(&path).unwrap();
        writeln!(file, "one,two,three,four").unwrap();
        for row in 1..=50 {
            match row {
                25 => writeln!(file, "row25,x,needle,x").unwrap(),
                30 => writeln!(file, "fresh,needle,x,x").unwrap(),
                _ => writeln!(file, "row{row},x,x,x").unwrap(),
            }
        }
        drop(file);

        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        document.set_visible_rows(5).unwrap();

        document.start_find_next(b"needle").unwrap();
        assert!(!document.header_is_editable(0));
        assert_eq!(
            document.rename_header(0, "renamed".into()).unwrap_err(),
            "Wait for the search to finish before editing headers."
        );
        finish_search(&mut document);
        let first = document.last_match.as_ref().unwrap();
        assert_eq!((first.row, first.column), (25, 2));
        assert_eq!(document.viewport_start, 25);
        assert_eq!(document.reveal_cell, Some((25, 2)));
        assert_eq!(
            document.selection,
            Some(GridSelection::Cell { row: 25, column: 2 })
        );
        assert_eq!(
            document.search_status.as_deref(),
            Some("Found row 25, column 3.")
        );

        document.start_find_next(b"needle").unwrap();
        finish_search(&mut document);
        let second = document.last_match.as_ref().unwrap();
        assert_eq!((second.row, second.column), (30, 1));
        assert_eq!(document.viewport_start, 30);

        document
            .start_find_previous_with_case(b"needle", CaseSensitivity::Insensitive)
            .unwrap();
        let previous = document.last_match.as_ref().unwrap();
        assert_eq!((previous.row, previous.column), (25, 2));
        assert_eq!(
            document.selection,
            Some(GridSelection::Cell { row: 25, column: 2 })
        );
        document.start_find_next(b"needle").unwrap();
        assert!(document.search_job.is_none());
        let forward = document.last_match.as_ref().unwrap();
        assert_eq!((forward.row, forward.column), (30, 1));

        document.start_find_next(b"fresh").unwrap();
        finish_search(&mut document);
        let reset = document.last_match.as_ref().unwrap();
        assert_eq!((reset.row, reset.column), (30, 0));

        document.set_visible_rows(25).unwrap();
        document.start_find_next(b"row50").unwrap();
        finish_search(&mut document);
        assert_eq!(document.viewport_start, 26);
        document.set_visible_rows(10).unwrap();
        assert_eq!(document.viewport_start, 41);
        assert_eq!(document.reveal_cell, Some((50, 0)));

        let before = document.viewport_start;
        document.start_find_next(b"missing").unwrap();
        finish_search(&mut document);
        assert_eq!(document.viewport_start, before);
        assert!(document.last_match.is_none());
        assert_eq!(
            document.search_status.as_deref(),
            Some("No further matches.")
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn find_mode_is_part_of_the_cursor_and_replace_in_cell_uses_it() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("find-case.csv");
        fs::write(&path, b"value\nNeedle needle\n").unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);

        document
            .start_find_next_with_case(b"needle", CaseSensitivity::Insensitive)
            .unwrap();
        finish_search(&mut document);
        assert_eq!(
            document.selection,
            Some(GridSelection::Cell { row: 1, column: 0 })
        );
        document.reveal_cell.take();
        assert!(document.can_replace_current_with_case(b"needle", CaseSensitivity::Insensitive));
        assert!(!document.can_replace_current_with_case(b"needle", CaseSensitivity::Sensitive));

        document
            .start_find_next_with_case(b"needle", CaseSensitivity::Sensitive)
            .unwrap();
        finish_search(&mut document);
        document
            .replace_current_match_with_case(b"needle", b"X", CaseSensitivity::Sensitive)
            .unwrap();
        assert_eq!(
            document
                .cell_edits
                .get(&(1, 0))
                .map(|value| value.as_slice()),
            Some(b"Needle X".as_slice())
        );

        document.cancel_search();
        document.shutdown();
    }

    #[test]
    fn find_next_uses_unsaved_cell_values() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("find-overlay.csv");
        fs::write(&path, b"first,second\nneedle,one\nother,two\n").unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);

        document.begin_cell_edit(1, 0, b"needle".to_vec()).unwrap();
        document.cell_edit.as_mut().unwrap().draft = "hidden".into();
        document.commit_cell_edit();
        document.begin_cell_edit(2, 1, b"two".to_vec()).unwrap();
        document.cell_edit.as_mut().unwrap().draft = "overlay needle".into();
        document.commit_cell_edit();

        document.start_find_next(b"needle").unwrap();
        finish_search(&mut document);
        let found = document.last_match.as_ref().unwrap();
        assert_eq!((found.row, found.column), (2, 1));
        assert_eq!(document.reveal_cell, Some((2, 1)));
        assert!(document.is_dirty());
    }

    #[test]
    fn replace_current_replaces_all_non_overlapping_matches_and_advances() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("replace-current.csv");
        fs::write(&path, b"first,second\naaaa,x\nx,aa\n").unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);

        document.start_find_next(b"aa").unwrap();
        finish_search(&mut document);
        assert!(document.can_replace_current(b"aa"));
        document.replace_current_match(b"aa", b"x").unwrap();
        assert_eq!(document.cell_edits.get(&(1, 0)).unwrap().as_slice(), b"xx");
        finish_search(&mut document);
        let next = document.last_match.as_ref().unwrap();
        assert_eq!((next.row, next.column), (2, 1));

        document.replace_current_match(b"aa", b"").unwrap();
        assert_eq!(document.cell_edits.get(&(2, 1)).unwrap().as_slice(), b"");
        finish_search(&mut document);
        assert_eq!(
            document.search_status.as_deref(),
            Some("No further matches.")
        );
        assert!(!document.can_replace_current(b"aa"));
        assert!(document.is_dirty());
    }

    #[test]
    fn replace_current_reports_an_overlay_match_without_a_source_cell() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("overlay-only-replace.csv");
        fs::write(&path, b"first,second\nvalue\n").unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        document.search_query = b"needle".to_vec();
        document.last_match = Some(SearchMatch {
            row: 1,
            column: 1,
            record_offset: 0,
        });
        document.selection = Some(GridSelection::Cell { row: 1, column: 1 });
        Arc::make_mut(&mut document.cell_edits).insert((1, 1), b"needle".to_vec().into());

        assert!(document.can_replace_current(b"needle"));
        assert_eq!(
            document
                .replace_current_match(b"needle", b"replacement")
                .unwrap_err(),
            "The current matched cell is no longer available for replacement."
        );
        assert_eq!(
            document
                .cell_edits
                .get(&(1, 1))
                .map(|value| value.as_slice()),
            Some(b"needle".as_slice())
        );
        document.shutdown();
    }

    #[test]
    fn replace_current_is_blocked_during_background_rewrites() {
        let directory = tempfile::tempdir().unwrap();
        let open_replaceable = |name: &str| {
            let path = directory.path().join(name);
            fs::write(&path, b"name,other\nneedle,x\n").unwrap();
            let mut document = Document::open(
                &path,
                OpenOptions {
                    header_mode: HeaderMode::FirstRow,
                    ..OpenOptions::default()
                },
            )
            .unwrap();
            finish_index(&mut document);
            document.start_find_next(b"needle").unwrap();
            finish_search(&mut document);
            assert!(document.can_replace_current(b"needle"));
            document
        };

        let mut saving = open_replaceable("replace-during-save.csv");
        saving.rename_header(0, "renamed".into()).unwrap();
        saving
            .start_save_as(directory.path().join("saved.csv"))
            .unwrap();
        assert!(saving.save_job.is_some());
        assert!(!saving.can_replace_current(b"needle"));
        assert!(saving.replace_current_match(b"needle", b"changed").is_err());
        assert!(saving.cell_edits.is_empty());
        saving.shutdown();

        let mut materializing = open_replaceable("replace-during-structural.csv");
        materializing.start_replace_all(b"x", b"y").unwrap();
        assert!(materializing.structural_job.is_some());
        assert!(!materializing.can_replace_current(b"needle"));
        assert!(
            materializing
                .replace_current_match(b"needle", b"changed")
                .is_err()
        );
        assert!(materializing.cell_edits.is_empty());
        materializing.shutdown();
    }

    #[test]
    fn logical_scrollbar_maps_the_full_row_range() {
        let data_start = 1;
        let total_rows = 1_000;
        let visible_rows = 100;

        assert_eq!(
            row_for_scroll_fraction(0.0, data_start, total_rows, visible_rows),
            1
        );
        assert_eq!(
            row_for_scroll_fraction(0.5, data_start, total_rows, visible_rows),
            451
        );
        assert_eq!(
            row_for_scroll_fraction(1.0, data_start, total_rows, visible_rows),
            901
        );
        assert_eq!(
            scroll_fraction_for_row(1, data_start, total_rows, visible_rows),
            0.0
        );
        assert_eq!(
            scroll_fraction_for_row(901, data_start, total_rows, visible_rows),
            1.0
        );

        assert_eq!(max_viewport_start(10, 100), 0);
        assert_eq!(
            row_for_scroll_fraction(1.0, data_start, 10, 100),
            data_start
        );
        assert_eq!(
            logical_viewport_start(u64::MAX, data_start, 10, 100),
            data_start
        );
    }

    #[test]
    fn logical_scrollbar_is_monotonic_at_117_million_rows() {
        let data_start = 1;
        let total_rows = 117_168_829;
        let visible_rows = 24;
        let final_row = data_start + total_rows - visible_rows as u64;
        assert_eq!(
            row_for_scroll_fraction(1.0, data_start, total_rows, visible_rows),
            final_row
        );

        let mut previous = data_start;
        for step in 0..=10_000 {
            let row = row_for_scroll_fraction(
                step as f64 / 10_000.0,
                data_start,
                total_rows,
                visible_rows,
            );
            assert!(row >= previous);
            assert!(row <= final_row);
            previous = row;
        }
        assert_eq!(previous, final_row);
        assert_eq!(row_for_scroll_fraction(1.0, 1, u64::MAX, 1), u64::MAX);
    }

    #[test]
    fn continuous_scrolling_refills_stays_bounded_and_reaches_the_last_record() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-scroll-{name}.csv"));
        let mut file = File::create(&path).unwrap();
        writeln!(file, "name,value").unwrap();
        for row in 1..=250 {
            writeln!(file, "row{row},{row}").unwrap();
        }
        drop(file);

        let mut document = Document::open(&path, OpenOptions::default()).unwrap();
        let index = document.job.take().unwrap().wait().unwrap();

        document.progress.rows_scanned = document.session.first_rows.len() as u64;
        let bootstrap_data_rows = document
            .session
            .first_rows
            .len()
            .saturating_sub(document.data_start as usize);
        document.set_visible_rows(bootstrap_data_rows + 1).unwrap();
        assert_eq!(document.visible_rows().len(), bootstrap_data_rows);

        document.index = Some(index);
        document.set_visible_rows(25).unwrap();
        assert_eq!(document.visible_rows().len(), 25);

        let capacity = document.visible_rows + 2 * OVERSCAN_ROWS;
        assert!(document.buffered_rows.len() <= capacity);

        let first = document.data_start;
        document.navigate(first + 100).unwrap();
        document.last_viewport_read = None;
        let mut refills = 0;
        for _ in 0..48 {
            document
                .scroll_by_points(-ROW_HEIGHT / 4.0, ROW_HEIGHT)
                .unwrap();
            refills += usize::from(document.last_viewport_read.take().is_some());
        }
        assert_eq!(document.viewport_start, first + 112);
        assert!(
            refills <= 1,
            "trackpad-sized scrolling refilled {refills} times"
        );

        document.navigate(first).unwrap();

        let row_stride = ROW_HEIGHT;
        document
            .scroll_by_points(-(row_stride - 1.0), row_stride)
            .unwrap();
        assert_eq!(document.viewport_start, first);
        document.scroll_by_points(-1.0, row_stride).unwrap();
        assert_eq!(document.viewport_start, first + 1);
        document.scroll_by_points(row_stride, row_stride).unwrap();
        assert_eq!(document.viewport_start, first);

        document.page(1).unwrap();
        assert_eq!(document.viewport_start, first + 25);
        assert!(document.buffered_rows.len() <= capacity);
        document.page(-1).unwrap();
        assert_eq!(document.viewport_start, first);

        let target = row_for_scroll_fraction(
            1.0,
            document.data_start,
            document.available_data_rows(),
            document.visible_rows,
        );
        document.navigate(target).unwrap();

        assert_eq!(document.display_end(), 250);
        assert_eq!(document.visible_rows().last().unwrap().fields[0], b"row250");
        assert!(document.buffered_rows.len() <= capacity);
        document
            .scroll_by_points(-1_000.0 * row_stride, row_stride)
            .unwrap();
        assert_eq!(document.viewport_start, target);
        assert!(document.buffered_rows.len() <= capacity);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn completed_index_refills_a_short_visible_buffer_without_scrolling() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-index-refill-{name}.csv"));
        let mut file = File::create(&path).unwrap();
        writeln!(file, "name,value").unwrap();
        for row in 1..=250 {
            writeln!(file, "row{row},{row}").unwrap();
        }
        drop(file);

        let mut document = Document::open(&path, OpenOptions::default()).unwrap();
        document.visible_rows = document.buffered_rows.len() + 3;
        let deadline = Instant::now() + Duration::from_secs(2);
        while !document.job.as_ref().unwrap().progress().done {
            assert!(Instant::now() < deadline, "index refill timed out");
            std::thread::yield_now();
        }

        document.poll().unwrap();

        assert_eq!(document.visible_rows().len(), document.visible_rows);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn later_wider_rows_expand_visible_columns() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-ragged-{name}.csv"));
        let mut file = File::create(&path).unwrap();
        let narrow_rows = BOOTSTRAP_ROWS + 2 * OVERSCAN_ROWS + 1;
        for row in 1..=narrow_rows {
            writeln!(file, "{row},{}", row + 100).unwrap();
        }
        writeln!(file, "{},121,visible", narrow_rows + 1).unwrap();
        drop(file);

        let mut document = Document::open(&path, OpenOptions::default()).unwrap();
        assert!(!document.session.dialect.has_header);
        assert_eq!(document.total_columns, 2);
        document.move_column(1, 0).unwrap();
        document.set_column_shown(0, false).unwrap();

        let index = document.job.take().unwrap().wait().unwrap();
        document.index = Some(index);
        document.navigate(narrow_rows as u64).unwrap();

        assert_eq!(document.total_columns, 3);
        assert_eq!(document.columns.order, [1, 0, 2]);
        assert!(document.columns.hidden[0]);
        assert_eq!(document.columns.visible, [1, 2]);
        assert_eq!(document.headers, ["Column 2", "Column 3"]);
        assert_eq!(
            document.visible_rows().last().unwrap().fields[2],
            b"visible"
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn late_index_error_is_reported_as_failed() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-malformed-{name}.csv"));
        let mut file = File::create(&path).unwrap();
        writeln!(file, "name,value").unwrap();
        for _ in 0..300_000 {
            file.write_all(b"a,1\n").unwrap();
        }
        file.write_all(b"\"unterminated").unwrap();
        drop(file);

        let mut document = Document::open(&path, OpenOptions::default()).unwrap();
        document.last_poll = Instant::now();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !document.job.as_ref().unwrap().progress().done {
            assert!(Instant::now() < deadline, "malformed indexing timed out");
            std::thread::yield_now();
        }
        document.cancel();
        let error = document.poll().unwrap_err();

        assert!(error.contains("unterminated quoted field"));
        let diagnostic = document.pending_diagnostic.as_ref().unwrap();
        assert_eq!(diagnostic.kind, quarry_core::DiagnosticKind::Structure);
        assert_eq!(diagnostic.byte_offset, None);
        assert!(
            diagnostic
                .details()
                .contains("Original-file byte position: unavailable")
        );
        assert!(document.job.is_none());
        assert!(document.index.is_none());
        assert_eq!(document.index_status(), "Index failed");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn empty_file_has_no_active_row() {
        let name = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("quarry-empty-{name}.csv"));
        File::create(&path).unwrap();

        let mut document = Document::prepare(&path, OpenOptions::default()).unwrap();
        assert!(document.job.is_none());
        document.start_indexing().unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !document.job.as_ref().unwrap().progress().done {
            assert!(Instant::now() < deadline, "empty file indexing timed out");
            std::thread::yield_now();
        }
        document.poll().unwrap();
        document.set_visible_rows(25).unwrap();

        assert!(document.job.is_none());
        assert!(document.index.is_some());
        assert_eq!(document.index_status(), "Index complete");
        assert_eq!(document.indexed_fraction(), 1.0);
        assert_eq!(document.available_data_rows(), 0);
        assert!(document.visible_rows().is_empty());
        assert_eq!(document.display_start(), 0);
        assert_eq!(document.display_end(), 0);
        document.index = None;
        assert_eq!(document.index_status(), "Index failed");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn row_and_file_column_inputs_are_one_based() {
        assert_eq!(parse_data_row("1", 1).unwrap(), 1);
        assert_eq!(parse_data_row("100000000", 1).unwrap(), 100_000_000);
        assert_eq!(parse_data_row("0", 1).unwrap_err(), "Data rows start at 1.");
        assert_eq!(parse_file_column("40", 40).unwrap(), 39);
        assert_eq!(
            parse_file_column("0", 40).unwrap_err(),
            "File columns start at 1."
        );
        assert_eq!(
            parse_file_column("41", 40).unwrap_err(),
            "File column must be between 1 and 40."
        );
    }

    #[test]
    fn insertion_progress_tracks_counting_and_writing_before_source_copy() {
        let mut progress = SaveAsProgress {
            preparing: true,
            preparation_bytes_scanned: 50,
            estimated_output_bytes: 0,
            bytes_scanned: 0,
            bytes_written: 0,
            total_bytes: 100,
            elapsed: Duration::ZERO,
            done: false,
            cancelled: false,
        };
        let counted = materialization_progress(progress, "Inserting rows");
        assert_eq!(counted.fraction, 0.1);
        assert!(counted.label.contains("counting 50.0%"));
        progress.preparing = false;
        progress.estimated_output_bytes = 1_000;
        progress.bytes_written = 500;
        let writing = materialization_progress(progress, "Inserting rows");
        assert!((writing.fraction - 0.6).abs() < 0.001);
        assert!(writing.label.contains("500.00 B written"));
        assert!(!writing.animate);
        progress.done = true;
        assert_eq!(
            materialization_progress(progress, "Inserting rows").fraction,
            1.0
        );
    }

    #[test]
    fn cleanup_cancel_after_worker_completion_preserves_document_and_history() {
        use quarry_core::{CleanupOperation, ColumnCleanup};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("cleanup-cancel.csv");
        let original = b"name,other\n  Alice  ,keep\n";
        fs::write(&path, original).unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..OpenOptions::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        document
            .begin_cell_edit(1, 0, b"  Alice  ".to_vec())
            .unwrap();
        document.cell_edit.as_mut().unwrap().draft = "  Carol  ".into();
        document.commit_edits();
        let edits = document.cell_edits.clone();
        let history = document.edit_history.clone();
        document
            .start_cleanup(ColumnCleanup {
                columns: vec![0],
                operation: CleanupOperation::Trim,
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let StructuralJob::Cleaning { job, .. } = document.structural_job.as_ref().unwrap()
            else {
                panic!("expected cleanup job");
            };
            if job.progress().done {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        document.cancel_structural_edit();
        let deadline = Instant::now() + Duration::from_secs(5);
        while document.structural_job.is_some() {
            assert!(
                document.poll_structural_edit().unwrap().is_none(),
                "a cancelled completed result must not be accepted"
            );
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert_eq!(document.session.path(), path);
        assert_eq!(document.cell_edits, edits);
        assert_eq!(document.edit_history, history);
        assert!(
            document.working_copy.is_none(),
            "session={:?} logical={:?} undo={:?} redo={:?}",
            document.session.path(),
            document.logical_path,
            document.working_copy.as_ref().unwrap().undo,
            document.working_copy.as_ref().unwrap().redo
        );
        assert_eq!(fs::read(path).unwrap(), original);
        assert!(
            document
                .structural_status
                .as_deref()
                .unwrap()
                .contains("cancelled")
        );
    }

    #[test]
    fn whitespace_inspection_shares_source_headers_and_pending_edit_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("shared.csv");
        fs::write(&path, b"name,value\nrow,original\n").unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..Default::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        let input = document.whitespace_header_input(0);
        assert_eq!(
            input.bytes().as_ptr(),
            document.session.first_rows[0].fields[0].as_ptr()
        );
        document
            .rename_header(0, "header ".repeat(128 * 1024))
            .unwrap();
        let header = document.whitespace_header_input(0);
        assert_eq!(
            header.bytes().as_ptr(),
            document.header_renames[&0].as_bytes().as_ptr()
        );
        commit_test_cell(&mut document, 1, 1, &"edited ".repeat(128 * 1024));
        let cell = document.whitespace_input(1, 1);
        assert_eq!(cell.bytes().as_ptr(), document.cell_edits[&(1, 1)].as_ptr());
        commit_test_cell(&mut document, 1, 1, "new");
        assert!(
            cell.bytes().starts_with(b"edited "),
            "worker snapshots remain immutable after later edits"
        );
        assert_eq!(input.bytes(), b"name");
    }

    #[test]
    fn shared_join_overlays_retain_captured_values_after_edits_and_tab_drop() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("join-capture.csv");
        fs::write(&path, b"name,value\nrow,x\n").unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..Default::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        let pending = "pending".repeat(16 * 1024);
        commit_test_cell(&mut document, 1, 1, &pending);
        document.rename_header(1, "Captured value".into()).unwrap();
        let captured_cells = Arc::clone(&document.cell_edits);
        let captured_headers = Arc::clone(&document.header_renames);
        assert!(Arc::ptr_eq(&captured_cells, &document.cell_edits));
        assert!(Arc::ptr_eq(&captured_headers, &document.header_renames));
        let captured_payload = Arc::clone(&captured_cells[&(1, 1)]);
        commit_test_cell(&mut document, 1, 1, "later value");
        document.rename_header(1, "Later header".into()).unwrap();
        assert_eq!(document.cell_edits[&(1, 1)].as_slice(), b"later value");
        assert_eq!(document.header_renames[&1].as_str(), "Later header");
        drop(document);
        let reader = std::thread::spawn(move || {
            assert!(Arc::ptr_eq(&captured_payload, &captured_cells[&(1, 1)]));
            assert_eq!(captured_headers[&1].as_str(), "Captured value");
            assert_eq!(captured_cells[&(1, 1)].as_slice(), pending.as_bytes());
        });
        reader.join().unwrap();
        assert_eq!(fs::read(path).unwrap(), b"name,value\nrow,x\n");
    }

    #[test]
    fn rejected_private_job_handoff_reaps_worker_and_releases_first_working_state() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.csv");
        fs::write(&source, b"name,value\nrow,original\n").unwrap();
        let mut document = Document::open(
            &source,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..Default::default()
            },
        )
        .unwrap();
        finish_index(&mut document);
        document.working_directory = directory.path().to_owned();
        commit_test_cell(&mut document, 1, 1, "pending");
        let preparation = document.prepare_materialization().unwrap();
        let root = document
            .working_copy
            .as_ref()
            .unwrap()
            .directory
            .path()
            .to_owned();
        let job = document
            .session
            .start_create_working_copy(
                preparation.renames,
                document.owned_cell_edits(),
                quarry_core::ColumnTransformation::Arrange {
                    source_width: 2,
                    output_columns: vec![1, 0],
                },
                &preparation.destination,
            )
            .unwrap();
        job.retain_working_storage(Arc::clone(
            &document.working_copy.as_ref().unwrap().directory,
        ))
        .unwrap();
        let failure = document
            .retain_private_job_storage(
                job,
                &preparation.destination,
                |job, storage| job.retain_working_storage(storage),
                |job| job.cancel(),
            )
            .err()
            .expect("a repeated real handoff must be rejected");
        assert!(failure.text.contains("already retained"));
        assert!(
            document.working_copy.is_none(),
            "a failed handoff must release a first-time empty working state"
        );
        assert!(document.original_session.is_none());
        assert!(
            !root.exists(),
            "the rejected worker must be reaped before its last storage owner drops"
        );
        assert!(!preparation.destination.exists());
        assert!(document.structural_job.is_none());
        assert_eq!(document.cell_edits[&(1, 1)].as_slice(), b"pending");
        assert!(document.can_undo());
        assert_eq!(fs::read(source).unwrap(), b"name,value\nrow,original\n");
    }

    #[test]
    fn empty_file_with_first_row_header_polls_without_underflow() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("empty.csv");
        fs::write(&path, b"").unwrap();
        let mut document = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(document.data_start, 1);
        assert_eq!(document.viewport_start, 1);
        finish_index(&mut document);
        assert!(document.progress.done);
        assert_eq!(document.available_rows(), 0);
        assert_eq!(document.available_data_rows(), 0);
        assert!(document.visible_rows().is_empty());
        document.load_buffer(1).unwrap();
        assert!(document.visible_rows().is_empty());
    }

    #[test]
    fn visible_filter_rows_returns_empty_when_viewport_precedes_buffer() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("filter-backward.csv");
        fs::write(&path, b"name\na\nb\nc\n").unwrap();
        let mut document = Document::open(&path, OpenOptions::default()).unwrap();
        finish_index(&mut document);
        document.filter_buffer_start = 84;
        document.filter_viewport_start = 0;
        document.filtered_rows = vec![Arc::new(FilterMatch {
            match_ordinal: 84,
            row: 85,
            record_offset: 0,
            fields: vec![b"stale".to_vec()],
        })];
        assert!(document.visible_filter_rows().is_empty());
        document.filter_viewport_start = 84;
        assert_eq!(document.visible_filter_rows().len(), 1);
    }

    #[test]
    fn start_save_utf8_requires_destination() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("latin1.csv");
        fs::write(&path, b"name\ncaf\xe9\n").unwrap();
        let mut document = Document::prepare_with_encoding(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..Default::default()
            },
            Some(Encoding::Latin1),
        )
        .unwrap();
        document.start_indexing().unwrap();
        finish_index(&mut document);
        let err = document
            .start_save_operation_format(None, true)
            .expect_err("UTF-8 save without destination must be rejected");
        assert!(err.text.contains("destination"));
        assert!(document.save_job.is_none());
        assert!(document.save_output_encoding.is_none());
    }

    #[test]
    fn filter_generation_tag_discards_out_of_order_stale_query_edit_and_tab_results() {
        let directory = tempfile::tempdir().unwrap();
        let path_a = directory.path().join("tab-a.csv");
        let path_b = directory.path().join("tab-b.csv");
        fs::write(&path_a, b"state,amount\nTX,10\nFL,20\nTX,30\nCA,40\n").unwrap();
        fs::write(&path_b, b"state,amount\nTX,15\nFL,40\n").unwrap();

        let mut doc_a = Document::open(
            &path_a,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..Default::default()
            },
        )
        .unwrap();
        doc_a.document_id = Some(DocumentId::new(1));
        finish_index(&mut doc_a);

        let mut doc_b = Document::open(
            &path_b,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..Default::default()
            },
        )
        .unwrap();
        doc_b.document_id = Some(DocumentId::new(2));
        finish_index(&mut doc_b);

        // Expression parsing & canonical printing honor renamed headers and round-trip.
        doc_a.rename_header(1, "order total".into()).unwrap();
        let q1 = doc_a
            .parse_filter_expression(
                "state == \"TX\" AND [order total] >= 10",
                CaseSensitivity::Sensitive,
            )
            .unwrap();
        doc_a.start_filter(q1.clone()).unwrap();
        let tag_q1 = doc_a.current_filter_tag();
        assert_eq!(tag_q1.document_id, DocumentId::new(1));
        assert_eq!(
            doc_a.active_filter_expression().unwrap().unwrap(),
            "state == \"TX\" AND [order total] >= 10"
        );
        finish_filter(&mut doc_a);
        let q1_index = doc_a.filter_index.clone().unwrap();
        let q1_rows: Vec<FilterMatch> = doc_a.filtered_rows.iter().map(|r| (**r).clone()).collect();
        assert_eq!(q1_rows.len(), 2);

        // Invalid replacement expression must NOT disturb the active filter.
        assert!(
            doc_a
                .parse_filter_expression(
                    "state == \"FL\" OR [order total] > 5",
                    CaseSensitivity::Sensitive,
                )
                .is_err()
        );
        assert_eq!(doc_a.filter_query.as_ref(), Some(&q1));
        assert_eq!(doc_a.current_filter_tag(), tag_q1);

        // Supersede with query 2 (`state == "FL"`) while forcing chunk 2 to arrive BEFORE chunk 1.
        let q2 = doc_a
            .parse_filter_expression("state == \"FL\"", CaseSensitivity::Sensitive)
            .unwrap();
        doc_a.start_filter(q2.clone()).unwrap();
        let tag_q2 = doc_a.current_filter_tag();
        assert_ne!(tag_q2.query_generation, tag_q1.query_generation);
        finish_filter(&mut doc_a);
        assert_eq!(doc_a.filtered_rows.len(), 1);
        assert_eq!(doc_a.filtered_rows[0].fields[0], b"FL");
        assert_eq!(doc_a.filtered_rows_tag, Some(tag_q2));

        // Late scan completion AND late page chunk from query 1 (older query_generation) are discarded.
        let stale_progress = FilterProgress {
            bytes_scanned: 100,
            rows_scanned: 5,
            matches_found: q1_index.matches_found(),
            file_size: 100,
            elapsed: Duration::ZERO,
            done: true,
            cancelled: false,
        };
        assert!(
            !doc_a
                .accept_filter_scan_completion(tag_q1, q1_index.clone(), stale_progress)
                .unwrap()
        );
        let window = doc_a.filter_read_window(0).unwrap();
        assert!(!doc_a.accept_filter_page_result(
            tag_q1,
            window,
            q1_rows.clone(),
            Instant::now(),
            false,
            false,
        ));
        assert_eq!(doc_a.filtered_rows.len(), 1);
        assert_eq!(doc_a.filtered_rows[0].fields[0], b"FL");

        // Late page chunk tagged with Tab A's DocumentId is rejected by Tab B even when Tab B
        // has matching rows and the EXACT same `(edit_revision, query_generation)`.
        doc_b.start_filter(q1.clone()).unwrap();
        finish_filter(&mut doc_b);
        doc_b.start_filter(q2.clone()).unwrap();
        finish_filter(&mut doc_b);
        let tag_b = doc_b.current_filter_tag();
        assert_ne!(tag_b.document_id, tag_q2.document_id);
        assert_eq!(tag_b.edit_revision, tag_q2.edit_revision);
        assert_eq!(tag_b.query_generation, tag_q2.query_generation);
        let window_b = doc_b
            .filter_read_window(0)
            .expect("tab B must have matching rows");
        assert!(!doc_b.accept_filter_page_result(
            tag_q2,
            window_b,
            q1_rows.clone(),
            Instant::now(),
            false,
            false,
        ));
        assert_eq!(doc_b.filtered_rows.len(), 1);
        assert_eq!(doc_b.filtered_rows[0].fields[0], b"FL");
        assert_eq!(doc_b.filtered_rows_tag, Some(tag_b));

        // Cell edit bumps edit_revision: any late page chunk tagged with the pre-edit tag is rejected.
        let pre_edit_tag = doc_a.current_filter_tag();
        doc_a.begin_cell_edit(2, 0, b"FL".to_vec()).unwrap();
        doc_a.cell_edit.as_mut().unwrap().draft = "NY".into();
        doc_a.commit_edits();
        assert_ne!(
            doc_a.current_filter_tag().edit_revision,
            pre_edit_tag.edit_revision
        );
        assert!(!doc_a.accept_filter_page_result(
            pre_edit_tag,
            window,
            q1_rows,
            Instant::now(),
            false,
            false,
        ));

        doc_a.shutdown();
        doc_b.shutdown();
    }

    #[test]
    fn start_filter_startup_failure_reconciles_retained_filter_tag() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("orders.csv");
        std::fs::write(&path, "state,amount\nTX,25\nFL,100\nTX,50\n").unwrap();

        let mut doc = Document::open(
            &path,
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..Default::default()
            },
        )
        .unwrap();
        doc.document_id = Some(DocumentId::new(11));
        finish_index(&mut doc);

        let q1 = doc
            .parse_filter_expression("state == \"TX\"", CaseSensitivity::Sensitive)
            .unwrap();
        doc.start_filter(q1.clone()).unwrap();
        finish_filter(&mut doc);
        assert_eq!(doc.filtered_rows.len(), 2);

        // Revoke read permissions so starting a replacement filter fails at file open with
        // `PermissionDenied` (not `SourceChanged`), then verify the retained filter's index tag
        // stays reconciled with `current_filter_tag()` instead of stranding future page reads.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
            // Privileged users (e.g. root in CI containers) bypass mode 000 read checks;
            // restore permissions and skip cleanly when the OS cannot enforce PermissionDenied.
            if std::fs::File::open(&path).is_ok() {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
                doc.shutdown();
                return;
            }
            let q2 = doc
                .parse_filter_expression("state == \"FL\"", CaseSensitivity::Sensitive)
                .unwrap();
            let start_res = doc.start_filter(q2);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(start_res.is_err());
            assert!(!doc.source_changed);
            assert_eq!(doc.filter_query.as_ref(), Some(&q1));
            let reconciled_tag = doc.current_filter_tag();
            assert_eq!(doc.filter_index_tag, Some(reconciled_tag));
            assert_eq!(doc.filtered_rows_tag, Some(reconciled_tag));
            let window = doc.filter_read_window(0).unwrap();
            let rows: Vec<FilterMatch> = doc.filtered_rows.iter().map(|r| (**r).clone()).collect();
            assert!(doc.accept_filter_page_result(
                reconciled_tag,
                window,
                rows,
                Instant::now(),
                false,
                false,
            ));
        }

        doc.shutdown();
    }
}
