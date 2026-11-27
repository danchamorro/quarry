//! Dormant-tab scheduler, lazy 200-file open, and code-counted FD budget.
//!
//! Lifts the fixed 8-tab cap (ADR 0007 amendment) by keeping at most a bounded
//! number of hydrated [`Document`] instances in memory (at least one per open
//! window) while evicting idle tabs to lightweight stubs or dormant records.
//!
//! Safety invariants:
//! - Clean idle tabs drop their `StructuralIndex` (up to 16 MiB/tab), row
//!   buffers, and `Session` while keeping canonical path + OS `FileIdentity`
//!   (`dev`, `ino`) + `SourceGuard` for hard-link deduplication, path
//!   reservations, and external-modification conflict checks.
//! - Dirty tabs retain sparse edits, `EditHistory`, and `WorkingCopyState`
//!   (protecting working copies and Undo/Redo directories from scratch cleanup)
//!   while enforcing an explicit sparse-overlay memory cap and disk-space
//!   check. Exceeding the budget or hitting disk-full / all-pinned never
//!   silently loses work.
//! - Active filters cancel workers on dormancy, retain query rules and source
//!   row anchors, and rebuild matches deterministically on wake.
//! - `.gz` tabs honor phase rules: `Pending` and `Decompressing` are pinned;
//!   `Complete` closes readers while keeping the private decompressed copy;
//!   `Preview` never silently restarts decompression on wake.
//! - External source changes while dormant always surface a conflict instead of
//!   reloading silently.
//! - Process file descriptors are accounted explicitly via [`FdBudgetLedger`]
//!   under a strict `< 128` cap (`MAX_PROCESS_FD_BUDGET`).

use crate::compressed::CompressedDormancyPolicy;
use crate::{
    AppMessage, BookmarkTransform, ColumnView, Document, DocumentId, EditHistory, GridCursor,
    GridSelection, JOIN_INPUT_RESERVED, MaterializedWorkingCopy, RowBookmarks, RowSelection,
    SOURCE_CHANGED_NOTICE, SharedCellEdits, StructuralJob, StructuralOverlay, WindowId,
    WorkbenchTab, WorkingCopySnapshot, WorkingCopyState, compressed,
};
use quarry_core::{
    ByteWindow, CompletedIndex, Encoding, FileIdentity, FilterQuery, OpenOptions, QuarryError,
    SectionBounds, SectionOpenOptions, Session, SourceGuard, StructuralIndex,
    VerifyBoundaryOutcome, WorkingCopyCleanup, inspect_storage,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Strict upper bound on code-counted process file descriptors (`total < 128`).
pub const MAX_PROCESS_FD_BUDGET: usize = 128;

/// Default maximum hydrated tabs across the workbench (automatically raised to
/// at least the number of open windows so every window keeps `>= 1` hydrated tab).
pub const DEFAULT_MAX_HYDRATED_TABS: usize = 8;

/// Default maximum sparse dirty-overlay bytes across dormant + active dirty tabs
/// (256 MiB) before refusing new dormancy/opens without discarding any edits.
pub const DEFAULT_MAX_DIRTY_OVERLAY_BYTES: usize = 256 * 1024 * 1024;

/// Default soft `RLIMIT_NOFILE` target requested at process startup.
pub const DEFAULT_NOFILE_TARGET: u64 = 4_096;

/// Shared global limit on concurrent write/decompression operations (ADR 0007).
pub const MAX_WRITE_JOBS: usize = 2;

/// Minimum free scratch bytes required before allowing a dirty tab with working
/// copies to cycle through dormancy when scratch space is checked.
pub const MIN_DORMANT_SCRATCH_HEADROOM_BYTES: u64 = 16 * 1024 * 1024;

/// Categories tracked by [`FdBudgetLedger`] so every subsystem's FD footprint is explicit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FdCategory {
    ActiveDocument,
    UndoRedo,
    SidecarCache,
    TempDirectory,
    GuiSystem,
    CleanupWorker,
    BackgroundJob,
}

/// Snapshot of code-counted file descriptor reservations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FdBudgetSnapshot {
    pub cap: usize,
    pub total_reserved: usize,
    pub peak_reserved: usize,
    pub by_category: BTreeMap<FdCategory, usize>,
}

#[derive(Debug)]
struct FdBudgetInner {
    cap: usize,
    total_reserved: usize,
    peak_reserved: usize,
    by_category: BTreeMap<FdCategory, usize>,
}

/// Code-counted internal file-descriptor budget enforcing `total_reserved < cap` (`< 128`).
#[derive(Clone, Debug)]
pub struct FdBudgetLedger {
    inner: Arc<Mutex<FdBudgetInner>>,
}

impl Default for FdBudgetLedger {
    fn default() -> Self {
        Self::new(MAX_PROCESS_FD_BUDGET)
    }
}

impl FdBudgetLedger {
    /// Creates a ledger enforcing `total_reserved < cap` (at most `cap - 1` simultaneous FDs).
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(FdBudgetInner {
                cap: cap.max(1),
                total_reserved: 0,
                peak_reserved: 0,
                by_category: BTreeMap::new(),
            })),
        }
    }

    /// Reserves `count` descriptors in `category` if `total + count < cap`.
    pub fn try_reserve(
        &self,
        category: FdCategory,
        count: usize,
    ) -> Result<FdLease, SchedulerError> {
        let mut inner = self.inner.lock().unwrap();
        let next_total = inner.total_reserved.saturating_add(count);
        if next_total >= inner.cap {
            return Err(SchedulerError::FdBudgetExhausted {
                requested: count,
                reserved: inner.total_reserved,
                cap: inner.cap,
                category,
            });
        }
        inner.total_reserved = next_total;
        inner.peak_reserved = inner.peak_reserved.max(next_total);
        *inner.by_category.entry(category).or_insert(0) += count;
        Ok(FdLease {
            ledger: Arc::clone(&self.inner),
            category,
            count,
        })
    }

    pub fn snapshot(&self) -> FdBudgetSnapshot {
        let inner = self.inner.lock().unwrap();
        FdBudgetSnapshot {
            cap: inner.cap,
            total_reserved: inner.total_reserved,
            peak_reserved: inner.peak_reserved,
            by_category: inner.by_category.clone(),
        }
    }
}

/// RAII lease releasing its code-counted FD reservation on drop.
#[derive(Debug)]
pub struct FdLease {
    ledger: Arc<Mutex<FdBudgetInner>>,
    category: FdCategory,
    count: usize,
}

impl FdLease {
    pub fn count(&self) -> usize {
        self.count
    }

    /// Atomically adjusts this lease's reservation count under the ledger mutex.
    pub fn try_resize(&mut self, new_count: usize) -> Result<(), SchedulerError> {
        if new_count == self.count {
            return Ok(());
        }
        let mut inner = self.ledger.lock().unwrap();
        if new_count > self.count {
            let delta = new_count - self.count;
            let next_total = inner.total_reserved.saturating_add(delta);
            if next_total >= inner.cap {
                return Err(SchedulerError::FdBudgetExhausted {
                    requested: delta,
                    reserved: inner.total_reserved,
                    cap: inner.cap,
                    category: self.category,
                });
            }
            inner.total_reserved = next_total;
            inner.peak_reserved = inner.peak_reserved.max(next_total);
            *inner.by_category.entry(self.category).or_insert(0) += delta;
        } else {
            let delta = self.count - new_count;
            inner.total_reserved = inner.total_reserved.saturating_sub(delta);
            if let Some(slot) = inner.by_category.get_mut(&self.category) {
                *slot = slot.saturating_sub(delta);
                if *slot == 0 {
                    inner.by_category.remove(&self.category);
                }
            }
        }
        self.count = new_count;
        Ok(())
    }
}

impl Drop for FdLease {
    fn drop(&mut self) {
        let mut inner = self.ledger.lock().unwrap_or_else(|e| e.into_inner());
        inner.total_reserved = inner.total_reserved.saturating_sub(self.count);
        if let Some(slot) = inner.by_category.get_mut(&self.category) {
            *slot = slot.saturating_sub(self.count);
            if *slot == 0 {
                inner.by_category.remove(&self.category);
            }
        }
    }
}

/// Reason a hydrated tab is currently pinned and cannot be evicted to dormancy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinReason {
    ActiveInWindow(WindowId),
    ActiveWriteJob,
    PendingMaterialization,
    PendingDuplicatePreview,
    CompressedDecompressing,
    CompressedPendingStorage,
    IndexingPaused,
    SectionVerifying,
    LiveWatching,
    JoinReserved,
    CloseFlowPending,
}

/// Classification of how a tab was hydrated or woken (keeps latency claims honest).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WakeKind {
    /// Tab was already hydrated in memory.
    AlreadyHydrated,
    /// Lazy stub opened for the first time.
    StubInitialOpen,
    /// Clean dormant tab reopened from disk (sidecar cache eligible).
    CleanCold,
    /// Dormant tab with an active filter reopened and scheduled deterministic re-filtering.
    FilteredRebuilding,
    /// Dormant dirty tab restored edits, history, and working copy, and scheduled re-indexing.
    DirtyRehydrated,
    /// Dormant `.gz` (`Phase::Complete`) tab restored from its retained private decompressed copy.
    CompressedCompleteRehydrated,
    /// Dormant `.gz` (`Phase::Preview`) tab restored its preview without restarting decompression.
    CompressedPreviewRestored,
}

/// Result of activating or waking a tab through [`WorkbenchScheduler`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WakeOutcome {
    Ready {
        document_id: DocumentId,
        kind: WakeKind,
        evicted: Vec<DocumentId>,
    },
    /// The underlying external file changed or vanished while the tab was dormant.
    /// The document is never silently reloaded; `source_changed` is set and edits are preserved.
    SourceChangedConflict {
        document_id: DocumentId,
        notice: String,
        evicted: Vec<DocumentId>,
    },
}

/// Residency state reported for observability and tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResidencyState {
    Stub,
    Hydrated,
    DormantClean,
    DormantDirty,
    DormantCompressedComplete,
    DormantCompressedPreview,
}

/// Completion or failure of a background save job reported by [`WorkbenchScheduler::poll_saves`].
#[derive(Debug)]
pub struct SaveCompletion {
    pub document_id: DocumentId,
    pub result: Result<(PathBuf, bool), AppMessage>,
}

/// Completion or failure of a background search job reported by [`WorkbenchScheduler::poll_searches`].
#[derive(Debug)]
pub struct SearchCompletion {
    pub document_id: DocumentId,
    pub result: Result<Option<(u64, usize)>, String>,
}

/// Completion or failure of a background filtered export job reported by
/// [`WorkbenchScheduler::poll_filtered_exports`].
#[derive(Debug)]
pub struct ExportCompletion {
    pub document_id: DocumentId,
    pub result: Result<Option<String>, String>,
}

/// Completion or failure of a background structural edit job reported by
/// [`WorkbenchScheduler::poll_structural_edits`].
#[derive(Debug)]
pub struct StructuralCompletion {
    pub document_id: DocumentId,
    pub result: Result<Option<String>, AppMessage>,
}

/// Completion or failure of a background `VerifyBoundaryJob` reported by
/// [`WorkbenchScheduler::poll_section_verifications`].
#[derive(Debug)]
pub struct SectionVerifyCompletion {
    pub document_id: DocumentId,
    pub result: Result<VerifyBoundaryOutcome, AppMessage>,
    /// True when a clean section was automatically reopened at its newly verified boundaries.
    pub reopened: bool,
}

/// Actionable scheduler failure that always preserves every open document and edit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SchedulerError {
    NotFound(DocumentId),
    WindowNotFound(WindowId),
    AllTabsPinned {
        hydrated: usize,
        limit: usize,
        reasons: Vec<(DocumentId, PinReason)>,
    },
    OverlayBudgetExceeded {
        used_bytes: usize,
        cap_bytes: usize,
    },
    ScratchDiskFull {
        required_bytes: u64,
        available_bytes: u64,
    },
    FdBudgetExhausted {
        requested: usize,
        reserved: usize,
        cap: usize,
        category: FdCategory,
    },
    UnapprovedDirtyTabs(Vec<DocumentId>),
    OpenFailed(String),
    /// An open failure that Inspect File cannot inspect (for example damaged gzip or a tar archive).
    OpenFailedUninspectable(String),
}

impl fmt::Display for SchedulerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "Document {id} was not found."),
            Self::WindowNotFound(id) => write!(f, "Window {id} was not found."),
            Self::AllTabsPinned {
                hydrated, limit, ..
            } => write!(
                f,
                "All {hydrated} hydrated documents (limit {limit}) are busy or pinned. Finish or close an active operation before opening another document."
            ),
            Self::OverlayBudgetExceeded {
                used_bytes,
                cap_bytes,
            } => write!(
                f,
                "Unsaved edit overlays use {used_bytes} bytes (limit {cap_bytes} bytes). Save or discard an edited document before making more tabs dormant."
            ),
            Self::ScratchDiskFull {
                required_bytes,
                available_bytes,
            } => write!(
                f,
                "Not enough temporary storage space ({available_bytes} bytes free, {required_bytes} bytes required). Existing open documents and unsaved edits have been preserved."
            ),
            Self::FdBudgetExhausted {
                requested,
                reserved,
                cap,
                category,
            } => write!(
                f,
                "Internal file-descriptor budget reached ({reserved} reserved, requested {requested} for {category:?}, cap < {cap}). Wait for active operations to finish."
            ),
            Self::UnapprovedDirtyTabs(ids) => write!(
                f,
                "{} modified document(s) have not been approved for quit. Save, discard, or approve each modified tab before finalizing quit.",
                ids.len()
            ),
            Self::OpenFailed(message) | Self::OpenFailedUninspectable(message) => {
                write!(f, "{message}")
            }
        }
    }
}

impl std::error::Error for SchedulerError {}

/// Lightweight metadata stub created lazily for bulk opens (e.g. 200 files) without
/// allocating a `StructuralIndex` or keeping any `File` handle open.
#[derive(Clone, Debug)]
pub struct LazyDocumentStub {
    pub logical_path: PathBuf,
    pub canonical_path: PathBuf,
    pub file_identity: Option<FileIdentity>,
    pub open_options: OpenOptions,
    pub encoding: Option<Encoding>,
}

/// View, selection, and filter-anchor metadata preserved across dormancy.
#[derive(Clone)]
struct DormantViewSnapshot {
    viewport_start: u64,
    scroll_points: f32,
    visible_rows: usize,
    data_start: u64,
    selection: Option<GridSelection>,
    cursor: GridCursor,
    selected_rows: RowSelection,
    selected_columns: BTreeSet<usize>,
    column_selection_anchor: Option<usize>,
    columns: ColumnView,
    headers: Vec<String>,
    total_columns: usize,
    filter_query: Option<FilterQuery>,
    filter_viewport_start: u64,
    edit_history: EditHistory,
    source_generation: u64,
    bookmarks: RowBookmarks,
}

struct DormantDirtyState {
    session: Arc<Session>,
    original_session: Option<Session>,
    working_copy: Option<WorkingCopyState>,
    header_renames: Arc<BTreeMap<usize, Arc<String>>>,
    cell_edits: SharedCellEdits,
    overlay_bytes: usize,
    verified_bounds_override: Option<SectionBounds>,
    pending_verified_reopen: Option<SectionBounds>,
    section_quote_free_confirmed: bool,
    /// Retains code-counted directory FD reservations for any live `WorkingCopyStorage`.
    _working_dir_fd_lease: Option<FdLease>,
}

#[derive(Clone, Copy)]
struct DormantSectionSpec {
    bounds: SectionBounds,
    quote_free_confirmed: bool,
    delimiter: u8,
    encoding: Encoding,
}

enum DormantKind {
    /// Clean plain file: `Session`, `StructuralIndex`, and row buffers fully dropped.
    CleanPlain {
        open_options: OpenOptions,
        encoding: Option<Encoding>,
        section: Option<DormantSectionSpec>,
    },
    /// Dirty document: sparse edits, history, working copy, and Undo/Redo dirs retained.
    Dirty(Box<DormantDirtyState>),
    /// Completed `.gz`: private decompressed `Session` retained (keeps private copy alive),
    /// while `StructuralIndex` and row buffers are evicted; holds its workspace directory FD lease.
    CompressedComplete {
        session: Arc<Session>,
        compressed: compressed::CompressedDocument,
        _workspace_fd_lease: Option<FdLease>,
    },
    /// Preview-only `.gz`: bootstrap `Session` and `Phase::Preview` retained; waking restores
    /// preview directly without restarting decompression.
    CompressedPreview {
        session: Arc<Session>,
        compressed: compressed::CompressedDocument,
    },
}

struct DormantDocument {
    logical_path: PathBuf,
    canonical_path: PathBuf,
    file_identity: Option<FileIdentity>,
    source_guard: SourceGuard,
    source_changed: bool,
    view: DormantViewSnapshot,
    kind: DormantKind,
}

#[derive(Clone)]
struct PendingFilterRestore {
    filter_viewport_start: u64,
    scroll_points: f32,
    selection: Option<GridSelection>,
    cursor: GridCursor,
    selected_rows: RowSelection,
    selected_columns: BTreeSet<usize>,
    column_selection_anchor: Option<usize>,
}

impl PendingFilterRestore {
    fn from_view(view: &DormantViewSnapshot) -> Self {
        Self {
            filter_viewport_start: view.filter_viewport_start,
            scroll_points: view.scroll_points,
            selection: view.selection,
            cursor: view.cursor.clone(),
            selected_rows: view.selected_rows.clone(),
            selected_columns: view.selected_columns.clone(),
            column_selection_anchor: view.column_selection_anchor,
        }
    }

    fn apply_to(&self, doc: &mut Document) {
        doc.filter_viewport_start = self.filter_viewport_start;
        doc.scroll_points = self.scroll_points;
        doc.selection = self.selection;
        doc.cursor = self.cursor.clone();
        doc.selected_rows = self.selected_rows.clone();
        doc.selected_columns = self.selected_columns.clone();
        doc.column_selection_anchor = self.column_selection_anchor;
    }
}

enum SlotResidency {
    Stub(LazyDocumentStub),
    Hydrated {
        /// Code-counted active-document FD lease (`session` + optional `original_session`);
        /// released immediately on dormancy.
        fd_lease: FdLease,
        /// Code-counted directory FD lease for live `WorkingCopyStorage` or `CompressedBacking`
        /// workspaces; retained across hydrate <-> dormant transitions until the owner drops.
        temp_dir_fd_lease: Option<FdLease>,
        /// Code-counted background-job FD lease covering active index, filter, filtered-read,
        /// search, filtered-export, save, and structural workers for the lifetime of those jobs.
        job_fd_lease: Option<FdLease>,
        /// Saved selection and scroll anchor reapplied once a woken filter rebuild finishes
        /// loading its filtered row buffer (so `navigate_filter` does not discard it).
        pending_filter_restore: Option<PendingFilterRestore>,
    },
    Dormant(Box<DormantDocument>),
}

struct ManagedTab {
    window: WindowId,
    tab: WorkbenchTab,
    residency: SlotResidency,
    last_used_tick: u64,
}

/// Configuration for [`WorkbenchScheduler`].
#[derive(Clone, Debug)]
pub struct WorkbenchSchedulerConfig {
    pub max_hydrated_tabs: usize,
    pub max_dirty_overlay_bytes: usize,
    pub max_process_fds: usize,
    pub working_directory: PathBuf,
    pub custom_working_directory: bool,
    pub index_cache_enabled: bool,
    /// When true, opening a `.gz` file pauses in `Phase::Pending` after loading bootstrap preview
    /// rows so the user can choose between decompressing the full file or staying in preview.
    pub defer_gzip_decompression: bool,
    /// Optional test hook simulating available scratch bytes (None uses OS `check_storage`).
    pub simulated_scratch_available_bytes: Option<u64>,
}

impl Default for WorkbenchSchedulerConfig {
    fn default() -> Self {
        Self {
            max_hydrated_tabs: DEFAULT_MAX_HYDRATED_TABS,
            max_dirty_overlay_bytes: DEFAULT_MAX_DIRTY_OVERLAY_BYTES,
            max_process_fds: MAX_PROCESS_FD_BUDGET,
            working_directory: quarry_core::default_temp_dir(),
            custom_working_directory: false,
            index_cache_enabled: true,
            defer_gzip_decompression: false,
            simulated_scratch_available_bytes: None,
        }
    }
}

/// Multi-window, dormant-tab workbench scheduler enforcing ADR 0007's amended resource policy.
pub struct WorkbenchScheduler {
    config: WorkbenchSchedulerConfig,
    fd_budget: FdBudgetLedger,
    working_cleanup: WorkingCopyCleanup,
    decompression_slots: compressed::DecompressionSlots,
    next_document_id: u64,
    access_clock: u64,
    tabs: BTreeMap<DocumentId, ManagedTab>,
    window_tabs: BTreeMap<WindowId, Vec<DocumentId>>,
    active_by_window: BTreeMap<WindowId, DocumentId>,
    quit_pending: bool,
    quit_approved: BTreeMap<DocumentId, u64>,
    close_approved: BTreeMap<DocumentId, u64>,
}

impl WorkbenchScheduler {
    pub fn new(config: WorkbenchSchedulerConfig) -> Self {
        let fd_budget = FdBudgetLedger::new(config.max_process_fds);
        Self {
            config,
            fd_budget,
            working_cleanup: WorkingCopyCleanup::default(),
            decompression_slots: compressed::DecompressionSlots::default(),
            next_document_id: 1,
            access_clock: 0,
            tabs: BTreeMap::new(),
            window_tabs: BTreeMap::new(),
            active_by_window: BTreeMap::new(),
            quit_pending: false,
            quit_approved: BTreeMap::new(),
            close_approved: BTreeMap::new(),
        }
    }

    /// Registers an open top-level window so empty windows are tracked alongside tabbed windows.
    pub fn register_window(&mut self, window: WindowId) {
        self.window_tabs.entry(window).or_default();
    }

    /// Returns all currently registered window IDs in deterministic order.
    pub fn windows(&self) -> Vec<WindowId> {
        self.window_tabs.keys().copied().collect()
    }

    /// Returns ordered document tabs owned by `window`.
    pub fn tabs_in_window(&self, window: WindowId) -> Vec<DocumentId> {
        self.window_tabs.get(&window).cloned().unwrap_or_default()
    }

    /// Returns the window currently owning `id`.
    pub fn window_of(&self, id: DocumentId) -> Option<WindowId> {
        self.tabs.get(&id).map(|slot| slot.window)
    }

    /// Returns the display filename of `id` across hydrated, dormant, and stub states.
    pub fn tab_display_name(&self, id: DocumentId) -> Option<String> {
        let path = self.tab_logical_path(id)?;
        let base = path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
            .into_owned();
        if let Some(bounds) = self.tab_section_bounds(id) {
            return Some(format!(
                "{base} [{}–{}]",
                crate::format_bytes(bounds.snapped.base),
                crate::format_bytes(bounds.snapped_end()),
            ));
        }
        if let Some(slot) = self.tabs.get(&id)
            && let SlotResidency::Dormant(dormant) = &slot.residency
        {
            let dormant_bounds = match &dormant.kind {
                DormantKind::CleanPlain {
                    section: Some(spec),
                    ..
                } => Some(spec.bounds),
                DormantKind::Dirty(dirty) => dirty
                    .verified_bounds_override
                    .or_else(|| dirty.session.section_bounds()),
                _ => None,
            };
            if let Some(bounds) = dormant_bounds {
                return Some(format!(
                    "{base} [{}–{}]",
                    crate::format_bytes(bounds.snapped.base),
                    crate::format_bytes(bounds.snapped_end()),
                ));
            }
        }
        Some(base)
    }

    /// Returns section bounds for a hydrated section tab (if applicable).
    pub fn tab_section_bounds(&self, id: DocumentId) -> Option<SectionBounds> {
        self.tabs
            .get(&id)
            .and_then(|slot| slot.tab.document.as_ref())
            .and_then(Document::section_bounds)
    }

    /// Returns the logical source path of `id` across hydrated, dormant, and stub states.
    pub fn tab_logical_path(&self, id: DocumentId) -> Option<PathBuf> {
        let slot = self.tabs.get(&id)?;
        match &slot.residency {
            SlotResidency::Stub(stub) => Some(stub.logical_path.clone()),
            SlotResidency::Dormant(dormant) => Some(dormant.logical_path.clone()),
            SlotResidency::Hydrated { .. } => slot
                .tab
                .document
                .as_ref()
                .map(|doc| doc.logical_path.clone()),
        }
    }

    /// Returns the `(file_identity, canonical_path)` alias key for `id` across hydrated, dormant,
    /// and stub states so parent/section tabs (including hard-link aliases) coordinate cleanly.
    fn tab_file_alias_key(&self, id: DocumentId) -> Option<(Option<FileIdentity>, PathBuf)> {
        let slot = self.tabs.get(&id)?;
        match &slot.residency {
            SlotResidency::Stub(stub) => Some((None, canonicalize_best_effort(&stub.logical_path))),
            SlotResidency::Dormant(dormant) => {
                Some((dormant.file_identity, dormant.canonical_path.clone()))
            }
            SlotResidency::Hydrated { .. } => {
                let doc = slot.tab.document.as_ref()?;
                let identity = doc
                    .original_session
                    .as_ref()
                    .unwrap_or(&doc.session)
                    .file_identity();
                Some((identity, canonicalize_best_effort(&doc.logical_path)))
            }
        }
    }

    /// ADR 0015 dirty mutual exclusion: across all windows and hard-link aliases (including
    /// hydrated and dormant dirty tabs), at most one tab (parent or section) per underlying file
    /// may hold unsaved edits at a time. Returns an actionable warning if another tab aliasing the
    /// same file already has unsaved edits or an active in-place save/structural write.
    pub fn sibling_dirty_conflict(&self, id: DocumentId) -> Option<String> {
        let (target_identity, target_canonical) = self.tab_file_alias_key(id)?;
        for (&other_id, slot) in &self.tabs {
            if other_id == id {
                continue;
            }
            let other_dirty_or_mutating = slot_is_dirty(slot)
                || slot.tab.document.as_ref().is_some_and(|doc| {
                    (doc.save_job.is_some() && doc.saving_in_place) || doc.structural_job.is_some()
                });
            if !other_dirty_or_mutating {
                continue;
            }
            let Some((other_identity, other_canonical)) = self.tab_file_alias_key(other_id) else {
                continue;
            };
            let same_file = match (target_identity, other_identity) {
                (Some(a), Some(b)) => a == b || target_canonical == other_canonical,
                _ => target_canonical == other_canonical,
            };
            if same_file {
                let other_label = self
                    .tab_display_name(other_id)
                    .unwrap_or_else(|| "another tab".into());
                return Some(format!(
                    "Another tab for this file ({other_label}) already has unsaved changes. Save or discard edits in that tab before editing this one."
                ));
            }
        }
        None
    }

    /// True when `id` has unsaved edits (whether currently hydrated or dormant-dirty).
    pub fn tab_is_dirty(&self, id: DocumentId) -> bool {
        self.tabs.get(&id).is_some_and(slot_is_dirty)
    }

    /// Lists all dirty `(WindowId, DocumentId)` tabs across every open window in window/tab order.
    pub fn dirty_tabs(&self) -> Vec<(WindowId, DocumentId)> {
        let mut out = Vec::new();
        for (&window, ids) in &self.window_tabs {
            for &id in ids {
                if self.tab_is_dirty(id) {
                    out.push((window, id));
                }
            }
        }
        out
    }

    /// Lists all dirty `DocumentId`s in `window` in tab order.
    pub fn dirty_tabs_in_window(&self, window: WindowId) -> Vec<DocumentId> {
        self.window_tabs
            .get(&window)
            .into_iter()
            .flatten()
            .copied()
            .filter(|&id| self.tab_is_dirty(id))
            .collect()
    }

    pub fn config(&self) -> &WorkbenchSchedulerConfig {
        &self.config
    }

    pub fn config_mut(&mut self) -> &mut WorkbenchSchedulerConfig {
        &mut self.config
    }

    pub fn fd_budget(&self) -> &FdBudgetLedger {
        &self.fd_budget
    }

    pub fn decompression_slots(&self) -> &compressed::DecompressionSlots {
        &self.decompression_slots
    }

    pub fn total_tabs(&self) -> usize {
        self.tabs.len()
    }

    pub fn hydrated_count(&self) -> usize {
        self.tabs
            .values()
            .filter(|slot| matches!(slot.residency, SlotResidency::Hydrated { .. }))
            .count()
    }

    pub fn dormant_count(&self) -> usize {
        self.tabs
            .values()
            .filter(|slot| matches!(slot.residency, SlotResidency::Dormant(_)))
            .count()
    }

    pub fn stub_count(&self) -> usize {
        self.tabs
            .values()
            .filter(|slot| matches!(slot.residency, SlotResidency::Stub(_)))
            .count()
    }

    pub fn active_document(&self, window: WindowId) -> Option<DocumentId> {
        self.active_by_window.get(&window).copied()
    }

    pub fn residency(&self, id: DocumentId) -> Option<ResidencyState> {
        self.tabs.get(&id).map(|slot| match &slot.residency {
            SlotResidency::Stub(_) => ResidencyState::Stub,
            SlotResidency::Hydrated { .. } => ResidencyState::Hydrated,
            SlotResidency::Dormant(dormant) => match &dormant.kind {
                DormantKind::CleanPlain { .. } => ResidencyState::DormantClean,
                DormantKind::Dirty(_) => ResidencyState::DormantDirty,
                DormantKind::CompressedComplete { .. } => ResidencyState::DormantCompressedComplete,
                DormantKind::CompressedPreview { .. } => ResidencyState::DormantCompressedPreview,
            },
        })
    }

    pub fn tab(&self, id: DocumentId) -> Option<&WorkbenchTab> {
        self.tabs.get(&id).map(|slot| &slot.tab)
    }

    pub fn tab_mut(&mut self, id: DocumentId) -> Option<&mut WorkbenchTab> {
        self.tabs.get_mut(&id).map(|slot| &mut slot.tab)
    }

    /// Total tracked sparse-overlay bytes across all hydrated and dormant dirty documents.
    pub fn total_dirty_overlay_bytes(&self) -> usize {
        self.tabs
            .values()
            .map(|slot| match &slot.residency {
                SlotResidency::Hydrated { .. } => {
                    slot.tab.document.as_ref().map_or(0, document_overlay_bytes)
                }
                SlotResidency::Dormant(dormant) => match &dormant.kind {
                    DormantKind::Dirty(dirty) => dirty.overlay_bytes,
                    _ => dormant.view.edit_history.bytes,
                },
                SlotResidency::Stub(_) => 0,
            })
            .sum()
    }

    /// Effective hydration ceiling: `max(config.max_hydrated_tabs, open_windows.len().max(1))`
    /// so every open window can keep at least one hydrated active tab.
    pub fn effective_hydration_limit(&self) -> usize {
        self.config
            .max_hydrated_tabs
            .max(self.active_by_window.len().max(1))
    }

    /// Finds an existing whole-file tab owning `path` by canonical path OR OS `FileIdentity`
    /// (`dev`, `ino`), across hydrated tabs, dormant tabs, AND lazy stubs (plus active working
    /// files). Bounded section tabs are skipped so opening a parent file and a section coexist.
    pub fn find_owner_for_path(&self, path: &Path) -> Option<(WindowId, DocumentId)> {
        let canonical = canonicalize_best_effort(path);
        let identity = FileIdentity::from_path(path).ok().flatten();
        for (&doc_id, slot) in &self.tabs {
            if slot_is_section(slot) {
                continue;
            }
            if tab_matches_path(slot, path, &canonical, identity) {
                return Some((slot.window, doc_id));
            }
        }
        None
    }

    /// Finds a validated `StructuralIndex` from an already-hydrated whole-file tab matching `path`.
    fn find_parent_index_for_path(&self, path: &Path) -> Option<StructuralIndex> {
        let canonical = canonicalize_best_effort(path);
        let identity = FileIdentity::from_path(path).ok().flatten();
        for slot in self.tabs.values() {
            if slot_is_section(slot) {
                continue;
            }
            if tab_matches_path(slot, path, &canonical, identity)
                && let Some(doc) = slot.tab.document.as_ref()
            {
                if let Some(index) = doc.index.as_ref() {
                    return Some(index.clone());
                }
                if let Some(job) = doc.job.as_ref() {
                    return Some(job.snapshot());
                }
            }
        }
        None
    }

    /// True if `path` aliases any open/dormant/stub document source, working copy, Undo/Redo
    /// snapshot directory, private decompressed copy, or pending write destination.
    pub fn path_reserved(&self, path: &Path) -> bool {
        let canonical = canonicalize_best_effort(path);
        let identity = FileIdentity::from_path(path).ok().flatten();
        self.tabs.values().any(|slot| {
            if tab_matches_path(slot, path, &canonical, identity) {
                return true;
            }
            if slot
                .tab
                .write_destination
                .as_ref()
                .is_some_and(|dest| paths_alias(path, dest))
            {
                return true;
            }
            match &slot.residency {
                SlotResidency::Hydrated { .. } => slot
                    .tab
                    .document
                    .as_ref()
                    .and_then(|doc| doc.working_copy.as_ref())
                    .is_some_and(|wc| working_copy_reserves_path(wc, path)),
                SlotResidency::Dormant(dormant) => match &dormant.kind {
                    DormantKind::Dirty(dirty) => dirty
                        .working_copy
                        .as_ref()
                        .is_some_and(|wc| working_copy_reserves_path(wc, path)),
                    _ => false,
                },
                SlotResidency::Stub(_) => false,
            }
        })
    }

    /// Returns the pin reason if `id` is currently hydrated and cannot be evicted.
    pub fn pin_reason(&self, id: DocumentId) -> Option<PinReason> {
        let slot = self.tabs.get(&id)?;
        if !matches!(slot.residency, SlotResidency::Hydrated { .. }) {
            return None;
        }
        if self.active_by_window.values().any(|&active| active == id) {
            return Some(PinReason::ActiveInWindow(slot.window));
        }
        if slot.tab.close_confirmation_open
            || slot.tab.close_after_save
            || slot.tab.close_after_jobs
        {
            return Some(PinReason::CloseFlowPending);
        }
        let doc = slot.tab.document.as_ref()?;
        if doc.join_reserved() {
            return Some(PinReason::JoinReserved);
        }
        if doc.is_indexing_pause_pinned() {
            return Some(PinReason::IndexingPaused);
        }
        if doc.section_verify_job.is_some() || doc.pending_verified_reopen.is_some() {
            return Some(PinReason::SectionVerifying);
        }
        if doc.is_live_watching() {
            return Some(PinReason::LiveWatching);
        }
        if slot.tab.document_writing() {
            return Some(PinReason::ActiveWriteJob);
        }
        if doc.pending_materialization.is_some() {
            return Some(PinReason::PendingMaterialization);
        }
        if matches!(
            doc.structural_job,
            Some(crate::StructuralJob::DuplicatePreview { .. })
        ) {
            return Some(PinReason::PendingDuplicatePreview);
        }
        if let Some(compressed) = doc.compressed.as_ref() {
            match compressed.dormancy_policy() {
                CompressedDormancyPolicy::PinnedPendingStorage => {
                    return Some(PinReason::CompressedPendingStorage);
                }
                CompressedDormancyPolicy::PinnedDecompressing => {
                    return Some(PinReason::CompressedDecompressing);
                }
                CompressedDormancyPolicy::DormantKeepPrivateCopy
                | CompressedDormancyPolicy::DormantPreviewOnly => {}
            }
        }
        None
    }

    /// Number of active file operations currently counted toward the global `MAX_WRITE_JOBS` cap.
    pub fn active_write_operations(&self) -> usize {
        self.tabs
            .values()
            .filter(|slot| matches!(slot.residency, SlotResidency::Hydrated { .. }))
            .map(|slot| slot.tab.unslotted_document_operations(false))
            .sum::<usize>()
            + self.decompression_slots.active()
    }

    /// True when the shared global two-write limit (`MAX_WRITE_JOBS`) has a free slot.
    pub fn write_slot_available(&self) -> bool {
        self.active_write_operations() < MAX_WRITE_JOBS
    }

    /// Pause active background indexing on `id` without parking pool threads; pins the tab.
    pub fn pause_indexing(&mut self, id: DocumentId) -> Result<(), String> {
        let doc = self
            .tab_mut(id)
            .and_then(|tab| tab.document.as_mut())
            .ok_or_else(|| "Document is not hydrated.".to_owned())?;
        doc.pause_indexing()
    }

    /// Resume paused background indexing on `id` after revalidating `SourceStamp`.
    pub fn resume_indexing(&mut self, id: DocumentId) -> Result<(), String> {
        let doc = self
            .tab_mut(id)
            .and_then(|tab| tab.document.as_mut())
            .ok_or_else(|| "Document is not hydrated.".to_owned())?;
        doc.resume_indexing()
    }

    /// Start opt-in append-only Live Watch Mode on hydrated tab `id`, reserving its OS descriptors
    /// in the code-counted FD budget and pinning the tab against dormancy.
    pub fn start_live_watch(&mut self, id: DocumentId) -> Result<(), AppMessage> {
        self.start_live_watch_with_config(id, quarry_core::WatchConfig::default())
    }

    /// Start opt-in append-only Live Watch Mode with an explicit [`quarry_core::WatchConfig`].
    pub fn start_live_watch_with_config(
        &mut self,
        id: DocumentId,
        config: quarry_core::WatchConfig,
    ) -> Result<(), AppMessage> {
        let fd_budget = self.fd_budget.clone();
        let slot = self
            .tabs
            .get_mut(&id)
            .ok_or_else(|| AppMessage::error("Document was not found."))?;
        let SlotResidency::Hydrated { job_fd_lease, .. } = &mut slot.residency else {
            return Err(AppMessage::warning("Document is not hydrated."));
        };
        let doc = slot
            .tab
            .document
            .as_mut()
            .ok_or_else(|| AppMessage::warning("Document is not open."))?;
        if let Some(reason) = doc.live_watch_unavailable_reason() {
            return Err(AppMessage::warning(reason));
        }
        // Reserve worst-case watcher descriptors (1 open FD + 3 kqueue/inotify/pipe FDs) up front.
        let worst_case = doc_active_job_fds(doc) + 4;
        try_sync_optional_lease(
            &fd_budget,
            job_fd_lease,
            FdCategory::BackgroundJob,
            worst_case,
        )
        .map_err(|err| AppMessage::warning(err.to_string()))?;
        let result = doc.start_live_watch_with_config(config);
        let actual = doc_active_job_fds(doc);
        let _ =
            try_sync_optional_lease(&fd_budget, job_fd_lease, FdCategory::BackgroundJob, actual);
        result
    }

    /// Stop Live Watch Mode on hydrated tab `id`, returning any terminal watch alert and releasing
    /// watcher descriptors from the FD ledger.
    pub fn stop_live_watch(
        &mut self,
        id: DocumentId,
    ) -> Result<Option<quarry_core::WatchAlert>, AppMessage> {
        let fd_budget = self.fd_budget.clone();
        let slot = self
            .tabs
            .get_mut(&id)
            .ok_or_else(|| AppMessage::error("Document was not found."))?;
        let SlotResidency::Hydrated { job_fd_lease, .. } = &mut slot.residency else {
            return Err(AppMessage::warning("Document is not hydrated."));
        };
        let doc = slot
            .tab
            .document
            .as_mut()
            .ok_or_else(|| AppMessage::warning("Document is not open."))?;
        let outcome = doc.stop_live_watch();
        let actual = doc_active_job_fds(doc);
        let _ =
            try_sync_optional_lease(&fd_budget, job_fd_lease, FdCategory::BackgroundJob, actual);
        outcome
    }

    /// Cancel an active or paused background indexing job on `id`.
    pub fn cancel_indexing(&mut self, id: DocumentId) -> bool {
        let Some(doc) = self.tab_mut(id).and_then(|tab| tab.document.as_mut()) else {
            return false;
        };
        if doc.job.is_none() || doc.progress.done {
            return false;
        }
        doc.cancel();
        true
    }

    /// Cancel an active Save or Save As job on `id`.
    pub fn cancel_save(&mut self, id: DocumentId) -> bool {
        let Some(doc) = self.tab_mut(id).and_then(|tab| tab.document.as_mut()) else {
            return false;
        };
        if doc.save_job.is_none() || doc.save_cancel_requested {
            return false;
        }
        doc.cancel_save();
        true
    }

    /// Lazily registers a batch of paths (e.g. 200 files) as lightweight stubs without
    /// opening file descriptors or indexing them eagerly. Duplicates select their owner
    /// without stealing tabs owned by another window.
    /// The first stub belonging to `window` is hydrated if the window has no active tab yet.
    pub fn open_many_lazy(
        &mut self,
        window: WindowId,
        paths: impl IntoIterator<Item = PathBuf>,
        options: OpenOptions,
        encoding: Option<Encoding>,
    ) -> Result<Vec<DocumentId>, SchedulerError> {
        let mut ids = Vec::new();
        let mut first_in_window = None;
        for path in paths {
            if let Some((owner_window, existing_id)) = self.find_owner_for_path(&path) {
                if owner_window == window && first_in_window.is_none() {
                    first_in_window = Some(existing_id);
                }
                ids.push(existing_id);
                continue;
            }
            let canonical_path = canonicalize_best_effort(&path);
            let file_identity = FileIdentity::from_path(&path).ok().flatten();
            let id = self.allocate_id();
            let mut tab = WorkbenchTab::new(id.raw());
            tab.delimiter_mode = crate::tab::DelimiterMode::from_delimiter(options.delimiter);
            tab.header_mode = options.header_mode;
            tab.encoding_mode = encoding;
            let stub = LazyDocumentStub {
                logical_path: path,
                canonical_path,
                file_identity,
                open_options: options,
                encoding,
            };
            let tick = self.next_tick();
            self.tabs.insert(
                id,
                ManagedTab {
                    window,
                    tab,
                    residency: SlotResidency::Stub(stub),
                    last_used_tick: tick,
                },
            );
            self.window_tabs.entry(window).or_default().push(id);
            if first_in_window.is_none() {
                first_in_window = Some(id);
            }
            ids.push(id);
        }

        if !self.active_by_window.contains_key(&window)
            && let Some(first_id) = first_in_window
        {
            let _ = self.activate_tab(window, first_id)?;
        }
        Ok(ids)
    }

    /// Opens a bounded byte section of `path` as a distinct tab in `window`.
    /// Automatically borrows a validated `StructuralIndex` from any already-open whole-file tab
    /// of the same file when `options.parent_index` is `None`.
    pub fn open_section(
        &mut self,
        window: WindowId,
        path: PathBuf,
        byte_window: ByteWindow,
        mut options: SectionOpenOptions,
    ) -> Result<WakeOutcome, SchedulerError> {
        if options.parent_index.is_none() {
            options.parent_index = self.find_parent_index_for_path(&path);
        }
        let overlay_used = self.total_dirty_overlay_bytes();
        if overlay_used > self.config.max_dirty_overlay_bytes {
            return Err(SchedulerError::OverlayBudgetExceeded {
                used_bytes: overlay_used,
                cap_bytes: self.config.max_dirty_overlay_bytes,
            });
        }
        let previous_active = self.active_by_window.get(&window).copied();
        let evicted_records = self.ensure_hydration_capacity(Some(window), previous_active)?;
        let evicted: Vec<DocumentId> = evicted_records
            .iter()
            .map(|&(_, doc_id, _)| doc_id)
            .collect();

        let fd_lease = match self.fd_budget.try_reserve(FdCategory::ActiveDocument, 1) {
            Ok(lease) => lease,
            Err(error) => {
                self.restore_evicted_on_failure(&evicted_records);
                return Err(error);
            }
        };

        let delimiter_mode = crate::tab::DelimiterMode::from_delimiter(options.open.delimiter);
        let header_mode = options.open.header_mode;
        let encoding_mode = options.encoding;
        let mut doc = match Document::prepare_section(&path, byte_window, options) {
            Ok(doc) => doc,
            Err(message) => {
                drop(fd_lease);
                self.restore_evicted_on_failure(&evicted_records);
                return Err(SchedulerError::OpenFailed(message));
            }
        };
        doc.working_directory = self.config.working_directory.clone();
        doc.custom_working_directory = self.config.custom_working_directory;
        doc.index_cache_enabled = self.config.index_cache_enabled;
        doc.working_cleanup = self.working_cleanup.clone();
        doc.decompression_slots = self.decompression_slots.clone();

        let job_fd_lease = match self.fd_budget.try_reserve(FdCategory::BackgroundJob, 1) {
            Ok(lease) => Some(lease),
            Err(error) => {
                doc.shutdown();
                drop(fd_lease);
                self.restore_evicted_on_failure(&evicted_records);
                return Err(error);
            }
        };
        if let Err(message) = doc.start_indexing() {
            doc.shutdown();
            drop(job_fd_lease);
            drop(fd_lease);
            self.restore_evicted_on_failure(&evicted_records);
            return Err(SchedulerError::OpenFailed(message));
        }

        let id = self.allocate_id();
        let mut tab = WorkbenchTab::new(id.raw());
        tab.delimiter_mode = delimiter_mode;
        tab.header_mode = header_mode;
        tab.encoding_mode = encoding_mode;
        tab.document = Some(doc);
        let tick = self.next_tick();
        self.tabs.insert(
            id,
            ManagedTab {
                window,
                tab,
                residency: SlotResidency::Hydrated {
                    fd_lease,
                    temp_dir_fd_lease: None,
                    job_fd_lease,
                    pending_filter_restore: None,
                },
                last_used_tick: tick,
            },
        );
        self.window_tabs.entry(window).or_default().push(id);
        self.active_by_window.insert(window, id);
        let _ = self.sync_fd_leases();
        Ok(WakeOutcome::Ready {
            document_id: id,
            kind: WakeKind::StubInitialOpen,
            evicted,
        })
    }

    /// Starts a pausable/cancellable `VerifyBoundaryJob` on section tab `id`, reserving its
    /// background FD and borrowing a validated parent index when available.
    pub fn start_verify_section_boundary(&mut self, id: DocumentId) -> Result<(), AppMessage> {
        let parent_index = self
            .tab_logical_path(id)
            .and_then(|path| self.find_parent_index_for_path(&path));
        self.with_reserved_job_fds(
            id,
            1,
            false,
            |doc| doc.start_verify_section_boundary(parent_index.as_ref()),
            |error| AppMessage::warning(error.to_string()),
        )
    }

    pub fn pause_verify_section_boundary(&mut self, id: DocumentId) -> Result<(), String> {
        let doc = self
            .tab_mut(id)
            .and_then(|tab| tab.document.as_mut())
            .ok_or_else(|| "Document is not hydrated.".to_owned())?;
        doc.pause_verify_section_boundary()
    }

    pub fn resume_verify_section_boundary(&mut self, id: DocumentId) -> Result<(), String> {
        let doc = self
            .tab_mut(id)
            .and_then(|tab| tab.document.as_mut())
            .ok_or_else(|| "Document is not hydrated.".to_owned())?;
        let res = doc.resume_verify_section_boundary();
        let _ = self.sync_fd_leases();
        res
    }

    pub fn cancel_verify_section_boundary(&mut self, id: DocumentId) {
        if let Some(doc) = self.tab_mut(id).and_then(|tab| tab.document.as_mut()) {
            doc.cancel_verify_section_boundary();
        }
        let _ = self.sync_fd_leases();
    }

    /// Reopens section tab `id` at its newly verified exact record boundaries, replacing the
    /// section's session. Refuses to discard unsaved section edits unless `discard_unsaved` is
    /// explicitly `true` (after caller confirmation).
    pub fn reopen_verified_section(
        &mut self,
        id: DocumentId,
        discard_unsaved: bool,
    ) -> Result<String, AppMessage> {
        let (
            path,
            verified_bounds,
            open_options,
            encoding,
            quote_free,
            expected_guard,
            expected_delimiter,
            expected_encoding,
        ) = {
            let Some(slot) = self.tabs.get(&id) else {
                return Err(AppMessage::warning("Document is not hydrated."));
            };
            let Some(doc) = slot.tab.document.as_ref() else {
                return Err(AppMessage::warning("Document is not hydrated."));
            };
            if slot.tab.document_writing()
                || doc.save_job.is_some()
                || doc.export_job.is_some()
                || doc.structural_job.is_some()
            {
                return Err(AppMessage::warning(
                    "Wait for the active save or export to finish before reopening at verified boundaries.",
                ));
            }
            let Some(bounds) = doc.pending_verified_reopen else {
                return Err(AppMessage::warning(
                    "No verified boundary update is pending for this section.",
                ));
            };
            if doc.is_dirty() && !discard_unsaved {
                return Err(AppMessage::warning(
                    "Unsaved section edits would be discarded. Save the section as a new file first, or confirm discarding edits to reopen at verified boundaries.",
                ));
            }
            (
                doc.logical_path.clone(),
                bounds,
                doc.current_open_options(),
                Some(doc.session.encoding()),
                doc.section_quote_free_confirmed,
                doc.session.source_guard(),
                doc.session.dialect.delimiter,
                doc.session.encoding(),
            )
        };
        let options = SectionOpenOptions {
            open: open_options,
            encoding,
            parent_index: None,
            quote_free_confirmed: quote_free,
            ..SectionOpenOptions::default()
        };
        let mut replacement = match Document::prepare_section_preserved(
            &path,
            verified_bounds,
            &expected_guard,
            expected_delimiter,
            expected_encoding,
            options,
        ) {
            Ok(doc) => doc,
            Err(quarry_core::QuarryError::SourceChanged) => {
                if let Some(doc) = self.tabs.get_mut(&id).and_then(|s| s.tab.document.as_mut()) {
                    doc.pending_verified_reopen = None;
                    doc.source_changed = true;
                }
                return Err(AppMessage::warning(
                    "The source file changed after boundary verification; reload or verify boundaries again.",
                ));
            }
            Err(err) => return Err(AppMessage::error(err.to_string())),
        };
        replacement.working_directory = self.config.working_directory.clone();
        replacement.custom_working_directory = self.config.custom_working_directory;
        replacement.index_cache_enabled = self.config.index_cache_enabled;
        replacement.working_cleanup = self.working_cleanup.clone();
        replacement.decompression_slots = self.decompression_slots.clone();
        replacement.section_verify_status = Some(
            if let Some(first) = replacement.global_data_row_number(replacement.data_start) {
                format!(
                    "Reopened at verified boundaries (starts at row {}).",
                    crate::group_digits(first)
                )
            } else {
                "Reopened at verified boundaries.".into()
            },
        );
        replacement.start_indexing().map_err(AppMessage::error)?;

        let notice = replacement
            .section_verify_status
            .clone()
            .unwrap_or_else(|| "Reopened at verified boundaries.".into());
        if let Some(slot) = self.tabs.get_mut(&id) {
            if let Some(mut old) = slot.tab.document.take() {
                old.shutdown();
            }
            slot.tab.document = Some(replacement);
            slot.tab.notice = Some(AppMessage::status(notice.clone()));
        }
        let _ = self.sync_fd_leases();
        Ok(notice)
    }

    /// Polls all hydrated tabs' `VerifyBoundaryJob`s. Clean sections whose boundaries shifted
    /// are automatically reopened at the exact verified bounds unless blocked by an active writer
    /// or open cell editor; dirty or blocked sections keep all state intact and expose
    /// `pending_verified_reopen` for explicit Save As + Reopen.
    pub fn poll_section_verifications(&mut self) -> Vec<SectionVerifyCompletion> {
        self.poll_section_verifications_with_blocked(&BTreeSet::new())
    }

    /// Same as [`Self::poll_section_verifications`], skipping auto-reopen for any `DocumentId`
    /// in `blocked_auto_reopen` (e.g. tabs currently open in a cell editor with an unapplied draft).
    pub fn poll_section_verifications_with_blocked(
        &mut self,
        blocked_auto_reopen: &BTreeSet<DocumentId>,
    ) -> Vec<SectionVerifyCompletion> {
        let ids: Vec<DocumentId> = self.tabs.keys().copied().collect();
        let mut completions = Vec::new();
        for id in ids {
            let outcome = self
                .tabs
                .get_mut(&id)
                .and_then(|slot| slot.tab.document.as_mut())
                .and_then(Document::poll_section_verify);
            let Some(result) = outcome else {
                continue;
            };
            let mut reopened = false;
            if let Ok(VerifyBoundaryOutcome::Verified {
                boundary_changed: true,
                ..
            }) = &result
            {
                let can_auto_reopen = !blocked_auto_reopen.contains(&id)
                    && self.tabs.get(&id).is_some_and(|slot| {
                        !slot.tab.document_writing()
                            && slot.tab.document.as_ref().is_some_and(|doc| {
                                !doc.is_dirty()
                                    && doc.save_job.is_none()
                                    && doc.export_job.is_none()
                                    && doc.structural_job.is_none()
                            })
                    });
                if can_auto_reopen && self.reopen_verified_section(id, false).is_ok() {
                    reopened = true;
                }
            }
            completions.push(SectionVerifyCompletion {
                document_id: id,
                result,
                reopened,
            });
        }
        if !completions.is_empty() {
            let _ = self.sync_fd_leases();
        }
        completions
    }

    /// Opens a single path immediately in `window`, deduplicating canonical paths / hard links
    /// and evicting an unpinned LRU tab if the hydration cap is reached.
    pub fn open_path(
        &mut self,
        window: WindowId,
        path: PathBuf,
        options: OpenOptions,
        encoding: Option<Encoding>,
    ) -> Result<WakeOutcome, SchedulerError> {
        if let Some((owner_window, existing_id)) = self.find_owner_for_path(&path) {
            return self.activate_tab(owner_window, existing_id);
        }
        let canonical_path = canonicalize_best_effort(&path);
        let file_identity = FileIdentity::from_path(&path).ok().flatten();
        let id = self.allocate_id();
        let mut tab = WorkbenchTab::new(id.raw());
        tab.delimiter_mode = crate::tab::DelimiterMode::from_delimiter(options.delimiter);
        tab.header_mode = options.header_mode;
        tab.encoding_mode = encoding;
        let stub = LazyDocumentStub {
            logical_path: path,
            canonical_path,
            file_identity,
            open_options: options,
            encoding,
        };
        let tick = self.next_tick();
        self.tabs.insert(
            id,
            ManagedTab {
                window,
                tab,
                residency: SlotResidency::Stub(stub),
                last_used_tick: tick,
            },
        );
        self.window_tabs.entry(window).or_default().push(id);
        match self.activate_tab(window, id) {
            Ok(outcome) => Ok(outcome),
            Err(error) => {
                self.tabs.remove(&id);
                self.remove_tab_from_window_lists(id);
                Err(error)
            }
        }
    }

    /// Activates `id` inside `window`, waking it from stub/dormant state if necessary and
    /// evicting LRU unpinned hydrated tabs to stay within `effective_hydration_limit()`.
    pub fn activate_tab(
        &mut self,
        window: WindowId,
        id: DocumentId,
    ) -> Result<WakeOutcome, SchedulerError> {
        self.activate_tab_internal(window, id, true)
    }

    fn activate_tab_internal(
        &mut self,
        window: WindowId,
        id: DocumentId,
        enforce_limits: bool,
    ) -> Result<WakeOutcome, SchedulerError> {
        if !self.tabs.contains_key(&id) {
            return Err(SchedulerError::NotFound(id));
        }

        // Move tab to the requested window if needed and check if already hydrated.
        let already_hydrated = matches!(
            self.tabs.get(&id).map(|slot| &slot.residency),
            Some(SlotResidency::Hydrated { .. })
        );
        if already_hydrated {
            let tick = self.next_tick();
            if let Some(slot) = self.tabs.get_mut(&id) {
                slot.window = window;
                slot.last_used_tick = tick;
            }
            self.detach_from_other_windows(window, id);
            self.assign_tab_to_window_list(window, id);
            self.active_by_window.insert(window, id);
            return Ok(WakeOutcome::Ready {
                document_id: id,
                kind: WakeKind::AlreadyHydrated,
                evicted: Vec::new(),
            });
        }

        let waking_dormant_dirty = matches!(
            self.tabs.get(&id).map(|slot| &slot.residency),
            Some(SlotResidency::Dormant(dormant)) if matches!(dormant.kind, DormantKind::Dirty(_))
        );

        // Enforce the overlay admission gate BEFORE evicting any hydrated tab so a rejected
        // switch never disturbs the window's active document. Waking an already-dormant dirty
        // tab stays allowed so users can always wake it to save or discard edits.
        if enforce_limits && !waking_dormant_dirty {
            let overlay_used = self.total_dirty_overlay_bytes();
            if overlay_used > self.config.max_dirty_overlay_bytes {
                return Err(SchedulerError::OverlayBudgetExceeded {
                    used_bytes: overlay_used,
                    cap_bytes: self.config.max_dirty_overlay_bytes,
                });
            }
        }

        // Before hydrating a new tab, verify we can stay within the hydration limit.
        // Rollback paths (`enforce_limits == false`) skip eviction so restoring an evicted
        // active tab cannot recurse into the same failing eviction.
        let previous_active = self.active_by_window.get(&window).copied();
        let evicted_records = if enforce_limits {
            self.ensure_hydration_capacity(Some(window), previous_active)?
        } else {
            Vec::new()
        };
        let evicted: Vec<DocumentId> = evicted_records
            .iter()
            .map(|&(_, doc_id, _)| doc_id)
            .collect();

        let waking_dormant_dirty_has_original = matches!(
            self.tabs.get(&id).map(|slot| &slot.residency),
            Some(SlotResidency::Dormant(dormant))
                if matches!(&dormant.kind, DormantKind::Dirty(dirty) if dirty.original_session.is_some())
        );

        // Reserve an active-document FD lease BEFORE opening the file.
        let needed_doc_fds = 1 + usize::from(waking_dormant_dirty_has_original);
        let fd_lease = match self
            .fd_budget
            .try_reserve(FdCategory::ActiveDocument, needed_doc_fds)
        {
            Ok(lease) => lease,
            Err(error) => {
                self.restore_evicted_on_failure(&evicted_records);
                return Err(error);
            }
        };

        let write_slot_available = self.write_slot_available();
        let tick = self.next_tick();
        let fd_budget = self.fd_budget.clone();
        let slot = self.tabs.get_mut(&id).unwrap();
        let previous_residency = std::mem::replace(
            &mut slot.residency,
            SlotResidency::Hydrated {
                fd_lease,
                temp_dir_fd_lease: None,
                job_fd_lease: None,
                pending_filter_restore: None,
            },
        );

        let wake_result: Result<HydratedWake, (SlotResidency, SchedulerError)> =
            match previous_residency {
                SlotResidency::Stub(stub) => {
                    match hydrate_from_stub(
                        &stub,
                        &self.config,
                        &self.working_cleanup,
                        &self.decompression_slots,
                        &fd_budget,
                        write_slot_available,
                    ) {
                        Ok((doc, temp_dir_lease, job_lease)) => {
                            slot.tab.document = Some(doc);
                            Ok(HydratedWake {
                                kind: WakeKind::StubInitialOpen,
                                conflict_notice: None,
                                temp_dir_lease,
                                job_lease,
                                pending_filter_restore: None,
                            })
                        }
                        Err(error) => Err((SlotResidency::Stub(stub), error)),
                    }
                }
                SlotResidency::Dormant(dormant) => {
                    let saved_filter_restore = dormant
                        .view
                        .filter_query
                        .as_ref()
                        .map(|_| PendingFilterRestore::from_view(&dormant.view));
                    match wake_dormant_document(
                        dormant,
                        &self.config,
                        &self.working_cleanup,
                        &self.decompression_slots,
                        &fd_budget,
                    ) {
                        Ok((doc, kind, conflict_notice, temp_dir_lease, job_lease)) => {
                            let pending_filter_restore = if doc.filter_job.is_some() {
                                saved_filter_restore
                            } else {
                                None
                            };
                            slot.tab.document = Some(doc);
                            Ok(HydratedWake {
                                kind,
                                conflict_notice,
                                temp_dir_lease,
                                job_lease,
                                pending_filter_restore,
                            })
                        }
                        Err((restored_dormant, error)) => {
                            Err((SlotResidency::Dormant(restored_dormant), error))
                        }
                    }
                }
                SlotResidency::Hydrated { .. } => unreachable!(),
            };

        match wake_result {
            Ok(HydratedWake {
                kind,
                conflict_notice,
                temp_dir_lease,
                job_lease,
                pending_filter_restore: restored_filter_anchor,
            }) => {
                if let SlotResidency::Hydrated {
                    temp_dir_fd_lease,
                    job_fd_lease,
                    pending_filter_restore,
                    ..
                } = &mut slot.residency
                {
                    *temp_dir_fd_lease = temp_dir_lease;
                    *job_fd_lease = job_lease;
                    *pending_filter_restore = restored_filter_anchor;
                }
                slot.window = window;
                slot.last_used_tick = tick;
                let notice_out = conflict_notice.clone();
                if let Some(notice) = conflict_notice {
                    slot.tab.notice = Some(AppMessage::warning(notice));
                }
                self.detach_from_other_windows(window, id);
                self.assign_tab_to_window_list(window, id);
                self.active_by_window.insert(window, id);
                if let Some(notice) = notice_out {
                    Ok(WakeOutcome::SourceChangedConflict {
                        document_id: id,
                        notice,
                        evicted,
                    })
                } else {
                    Ok(WakeOutcome::Ready {
                        document_id: id,
                        kind,
                        evicted,
                    })
                }
            }
            Err((restored, error)) => {
                // Roll back residency cleanly (drops `_fd_lease` automatically) and restore
                // any tabs evicted for this failed open without re-applying the admission gate.
                slot.residency = restored;
                self.restore_evicted_on_failure(&evicted_records);
                Err(error)
            }
        }
    }

    fn detach_from_other_windows(&mut self, target_window: WindowId, moved_id: DocumentId) {
        let stale_windows: Vec<WindowId> = self
            .active_by_window
            .iter()
            .filter_map(|(&win, &active)| {
                (win != target_window && active == moved_id).then_some(win)
            })
            .collect();
        for old_window in stale_windows {
            let replacement = self
                .tabs
                .iter()
                .filter(|&(&doc_id, slot)| {
                    doc_id != moved_id
                        && slot.window == old_window
                        && matches!(slot.residency, SlotResidency::Hydrated { .. })
                })
                .max_by_key(|(_, slot)| slot.last_used_tick)
                .map(|(&doc_id, _)| doc_id);
            if let Some(next_id) = replacement {
                self.active_by_window.insert(old_window, next_id);
            } else {
                self.active_by_window.remove(&old_window);
            }
        }
    }

    fn restore_evicted_on_failure(&mut self, evicted: &[(WindowId, DocumentId, bool)]) {
        // Restore previously active tabs first without re-applying the new-document overlay gate.
        for &(win, doc_id, was_active) in evicted.iter().rev() {
            if was_active && self.tabs.contains_key(&doc_id) {
                let _ = self.activate_tab_internal(win, doc_id, false);
            }
        }
    }

    /// Explicitly demotes a specific unpinned hydrated tab to dormant state.
    pub fn make_dormant(&mut self, id: DocumentId) -> Result<(), SchedulerError> {
        if let Some(reason) = self.pin_reason(id) {
            return Err(SchedulerError::AllTabsPinned {
                hydrated: self.hydrated_count(),
                limit: self.effective_hydration_limit(),
                reasons: vec![(id, reason)],
            });
        }
        self.evict_tab_unchecked(id)
    }

    /// Starts a filter on hydrated tab `id` while reserving its `BackgroundJob` descriptors
    /// (2 FDs for the scan file + cloned guard handle plus 1 pre-reserved FD for the concurrent
    /// filtered-read worker) against the process `< 128` FD budget.
    pub fn start_filter(
        &mut self,
        id: DocumentId,
        query: FilterQuery,
    ) -> Result<(), SchedulerError> {
        let fd_budget = self.fd_budget.clone();
        let Some(slot) = self.tabs.get_mut(&id) else {
            return Err(SchedulerError::NotFound(id));
        };
        let SlotResidency::Hydrated { job_fd_lease, .. } = &mut slot.residency else {
            return Err(SchedulerError::OpenFailed(
                "Activate the document before starting a filter.".into(),
            ));
        };
        let Some(doc) = slot.tab.document.as_mut() else {
            return Err(SchedulerError::OpenFailed("Document is not loaded.".into()));
        };
        let needed_jobs = doc_non_filter_job_fds(doc) + FILTER_JOB_AND_READ_FDS;
        try_sync_optional_lease(
            &fd_budget,
            job_fd_lease,
            FdCategory::BackgroundJob,
            needed_jobs,
        )?;
        if let Err(message) = doc.start_filter(query) {
            let _ = try_sync_optional_lease(
                &fd_budget,
                job_fd_lease,
                FdCategory::BackgroundJob,
                doc_active_job_fds(doc),
            );
            return Err(SchedulerError::OpenFailed(message));
        }
        let _ = try_sync_optional_lease(
            &fd_budget,
            job_fd_lease,
            FdCategory::BackgroundJob,
            doc_active_job_fds(doc),
        );
        Ok(())
    }

    /// Pre-reserves the active-document, temp-directory, and background-job FD leases needed
    /// before starting a worker on hydrated tab `id`, invokes `op`, and reconciles all live
    /// leases afterward (rolling back unused reservations if `op` fails or finishes synchronously).
    pub fn with_reserved_job_fds<T, E>(
        &mut self,
        id: DocumentId,
        extra_job_fds: usize,
        needs_working_copy: bool,
        op: impl FnOnce(&mut Document) -> Result<T, E>,
        on_unavailable: impl FnOnce(SchedulerError) -> E,
    ) -> Result<T, E> {
        let fd_budget = self.fd_budget.clone();
        let Some(slot) = self.tabs.get_mut(&id) else {
            return Err(on_unavailable(SchedulerError::NotFound(id)));
        };
        let SlotResidency::Hydrated {
            fd_lease,
            temp_dir_fd_lease,
            job_fd_lease,
            ..
        } = &mut slot.residency
        else {
            return Err(on_unavailable(SchedulerError::OpenFailed(
                "Activate the document before starting this operation.".into(),
            )));
        };
        let Some(doc) = slot.tab.document.as_mut() else {
            return Err(on_unavailable(SchedulerError::OpenFailed(
                "Document is not loaded.".into(),
            )));
        };

        let base_doc_fds = doc_active_document_fds(doc);
        let base_dir_fds = doc_retained_dir_fds(doc);
        let base_job_fds = doc_active_job_fds(doc);

        let target_doc_fds = if needs_working_copy {
            base_doc_fds.max(2)
        } else {
            base_doc_fds
        };
        let target_dir_fds =
            base_dir_fds + usize::from(needs_working_copy && doc.working_copy.is_none());
        let target_job_fds = base_job_fds.saturating_add(extra_job_fds);

        let reserve_result = (|| -> Result<(), SchedulerError> {
            fd_lease.try_resize(target_doc_fds)?;
            try_sync_optional_lease(
                &fd_budget,
                temp_dir_fd_lease,
                FdCategory::TempDirectory,
                target_dir_fds,
            )?;
            try_sync_optional_lease(
                &fd_budget,
                job_fd_lease,
                FdCategory::BackgroundJob,
                target_job_fds,
            )?;
            Ok(())
        })();

        if let Err(err) = reserve_result {
            let _ = fd_lease.try_resize(base_doc_fds);
            let _ = try_sync_optional_lease(
                &fd_budget,
                temp_dir_fd_lease,
                FdCategory::TempDirectory,
                base_dir_fds,
            );
            let _ = try_sync_optional_lease(
                &fd_budget,
                job_fd_lease,
                FdCategory::BackgroundJob,
                base_job_fds,
            );
            return Err(on_unavailable(err));
        }

        let outcome = op(doc);
        let _ = fd_lease.try_resize(doc_active_document_fds(doc));
        let _ = try_sync_optional_lease(
            &fd_budget,
            temp_dir_fd_lease,
            FdCategory::TempDirectory,
            doc_retained_dir_fds(doc),
        );
        let _ = try_sync_optional_lease(
            &fd_budget,
            job_fd_lease,
            FdCategory::BackgroundJob,
            doc_active_job_fds(doc),
        );
        outcome
    }

    /// Synchronizes live `ActiveDocument`, `TempDirectory`, and `BackgroundJob` FD reservations
    /// across all hydrated tabs (accounting for `original_session`, working copies, completed gzip
    /// workspaces, and active index, filter, search, export, save, and structural workers).
    pub fn sync_fd_leases(&mut self) -> Result<(), SchedulerError> {
        let fd_budget = self.fd_budget.clone();
        for slot in self.tabs.values_mut() {
            let SlotResidency::Hydrated {
                fd_lease,
                temp_dir_fd_lease,
                job_fd_lease,
                ..
            } = &mut slot.residency
            else {
                continue;
            };
            let Some(doc) = slot.tab.document.as_ref() else {
                *temp_dir_fd_lease = None;
                *job_fd_lease = None;
                continue;
            };
            fd_lease.try_resize(doc_active_document_fds(doc))?;
            try_sync_optional_lease(
                &fd_budget,
                temp_dir_fd_lease,
                FdCategory::TempDirectory,
                doc_retained_dir_fds(doc),
            )?;
            try_sync_optional_lease(
                &fd_budget,
                job_fd_lease,
                FdCategory::BackgroundJob,
                doc_active_job_fds(doc),
            )?;
        }
        Ok(())
    }

    /// Polls active indexing and filter workers on all hydrated tabs and updates live
    /// document, directory, and background-job FD leases.
    pub fn poll_all(&mut self) -> Vec<(DocumentId, String)> {
        let mut errors = Vec::new();
        let fd_budget = self.fd_budget.clone();
        for (&id, slot) in &mut self.tabs {
            let SlotResidency::Hydrated {
                fd_lease,
                temp_dir_fd_lease,
                job_fd_lease,
                pending_filter_restore,
            } = &mut slot.residency
            else {
                continue;
            };
            let Some(doc) = slot.tab.document.as_mut() else {
                continue;
            };
            if let Err(err) = doc.poll() {
                errors.push((id, err));
            }
            if let Err(err) = doc.poll_live_watch() {
                errors.push((id, err));
            }
            if doc.bookmark_offset_refresh_pending
                && doc.bookmark_offset_job.is_none()
                && doc.job.is_none()
                && doc.index.is_some()
                && !doc.saving_in_place
                && !doc.is_live_watching()
            {
                doc.refresh_bookmark_offsets_from_buffers();
                if doc
                    .bookmarks
                    .pending_offset_rows(doc.source_generation)
                    .is_empty()
                {
                    doc.bookmark_offset_refresh_pending = false;
                } else {
                    let needed = doc_active_job_fds(doc) + 1;
                    if try_sync_optional_lease(
                        &fd_budget,
                        job_fd_lease,
                        FdCategory::BackgroundJob,
                        needed,
                    )
                    .is_ok()
                    {
                        doc.start_bookmark_offset_refresh();
                    }
                }
            }
            // Reserve descriptors BEFORE polling filter/read workers so `poll_filter()` or
            // `poll_filter_read()` cannot start a queued filter refresh or filtered-read worker
            // without an FD lease.
            let non_filter_jobs = doc_non_filter_job_fds(doc);
            let can_run_filter_read = if doc.filter_job.is_some() || doc.filter_refresh_pending {
                let needed = non_filter_jobs + FILTER_JOB_AND_READ_FDS;
                try_sync_optional_lease(&fd_budget, job_fd_lease, FdCategory::BackgroundJob, needed)
                    .is_ok()
            } else if doc.filter_read.is_some() || doc.pending_filter_read.is_some() {
                let needed = non_filter_jobs + 1;
                try_sync_optional_lease(&fd_budget, job_fd_lease, FdCategory::BackgroundJob, needed)
                    .is_ok()
            } else {
                true
            };
            if can_run_filter_read {
                if let Err(err) = doc.poll_filter() {
                    errors.push((id, err));
                }
                if let Err(err) = doc.poll_filter_read() {
                    errors.push((id, err));
                }
            } else if doc.filter_job.is_some() {
                // If an external caller started a filter directly without reserving its 3 FDs,
                // keep the scan progressing without launching an unreserved filtered read.
                if let Err(err) = doc.poll_filter_scan() {
                    errors.push((id, err));
                }
            }
            if pending_filter_restore.is_some() {
                if doc.filter_query.is_none() {
                    *pending_filter_restore = None;
                } else if doc.filter_job.is_none() && !doc.filter_rows_loading() {
                    let has_matches = doc
                        .filter_index
                        .as_ref()
                        .is_some_and(|index| index.matches_found() > 0);
                    if (!doc.filtered_rows.is_empty() || !has_matches)
                        && let Some(pending) = pending_filter_restore.take()
                        && has_matches
                    {
                        pending.apply_to(doc);
                    }
                }
            }
            let _ = fd_lease.try_resize(doc_active_document_fds(doc));
            let _ = try_sync_optional_lease(
                &fd_budget,
                temp_dir_fd_lease,
                FdCategory::TempDirectory,
                doc_retained_dir_fds(doc),
            );
            let _ = try_sync_optional_lease(
                &fd_budget,
                job_fd_lease,
                FdCategory::BackgroundJob,
                doc_active_job_fds(doc),
            );
        }
        errors
    }

    /// Begins a cross-window quit review without dropping any tabs until approved.
    pub fn begin_quit_review(&mut self) {
        self.quit_pending = true;
        self.quit_approved.clear();
    }

    /// Cancels a pending quit review, retaining every active, dormant, dirty, and stub tab
    /// along with all working copies and Undo/Redo directories.
    pub fn cancel_quit(&mut self) {
        self.quit_pending = false;
        self.quit_approved.clear();
        for slot in self.tabs.values_mut() {
            slot.tab.cancel_close();
        }
    }

    /// Approves one tab for window/tab close at its current edit revision. Any subsequent edit
    /// changes its revision and automatically invalidates this approval in [`Self::close_window`].
    pub fn approve_tab_for_close(&mut self, id: DocumentId) -> Result<(), SchedulerError> {
        let Some(slot) = self.tabs.get_mut(&id) else {
            return Err(SchedulerError::NotFound(id));
        };
        slot.tab.approve_discard_and_close();
        let revision = slot_edit_revision(slot);
        self.close_approved.insert(id, revision);
        Ok(())
    }

    /// Revokes any pending window/tab close approval on `id`.
    pub fn revoke_tab_close_approval(&mut self, id: DocumentId) {
        self.close_approved.remove(&id);
        if let Some(slot) = self.tabs.get_mut(&id) {
            slot.tab.cancel_close();
        }
    }

    fn tab_is_close_approved(&self, id: DocumentId, slot: &ManagedTab) -> bool {
        slot.tab.close_ready
            && self
                .close_approved
                .get(&id)
                .is_some_and(|&approved_rev| approved_rev == slot_edit_revision(slot))
    }

    /// Approves one tab for quit at its current edit revision without releasing it yet
    /// (cancelled quit stays lossless; any subsequent edit invalidates this approval).
    pub fn approve_tab_for_quit(&mut self, id: DocumentId) -> Result<(), SchedulerError> {
        let Some(slot) = self.tabs.get(&id) else {
            return Err(SchedulerError::NotFound(id));
        };
        let revision = slot_edit_revision(slot);
        self.quit_approved.insert(id, revision);
        Ok(())
    }

    fn tab_is_quit_approved(&self, id: DocumentId, slot: &ManagedTab) -> bool {
        self.quit_approved
            .get(&id)
            .is_some_and(|&approved_rev| approved_rev == slot_edit_revision(slot))
    }

    /// Finalizes quit only after every dirty tab (hydrated or dormant) has been approved via
    /// [`Self::approve_tab_for_quit`] at its current edit revision. If any dirty tab is
    /// unapproved or was edited after approval, returns [`SchedulerError::UnapprovedDirtyTabs`]
    /// while leaving every tab and working copy intact. Otherwise shuts down and drops every
    /// tab, releasing all Quarry-owned private working copies, Undo/Redo directories, and FD
    /// leases.
    pub fn finalize_quit(&mut self) -> Result<Vec<String>, SchedulerError> {
        let unapproved: Vec<DocumentId> = self
            .tabs
            .iter()
            .filter_map(|(&id, slot)| {
                (slot_is_dirty(slot) && !self.tab_is_quit_approved(id, slot)).then_some(id)
            })
            .collect();
        if !unapproved.is_empty() {
            return Err(SchedulerError::UnapprovedDirtyTabs(unapproved));
        }
        self.quit_pending = false;
        self.quit_approved.clear();
        self.close_approved.clear();
        self.active_by_window.clear();
        self.window_tabs.clear();
        let tabs = std::mem::take(&mut self.tabs);
        for (_, mut slot) in tabs {
            if let Some(doc) = slot.tab.document.as_mut() {
                doc.shutdown();
            }
            drop(slot);
        }
        Ok(self.working_cleanup.take_errors())
    }

    /// Moves `id` from its current window into `target_window`, activating it in `target_window`
    /// and activating an adjacent tab in the previous window if any tabs remain there.
    pub fn move_tab_to_window(
        &mut self,
        id: DocumentId,
        target_window: WindowId,
    ) -> Result<WakeOutcome, SchedulerError> {
        let Some(old_window) = self.tabs.get(&id).map(|slot| slot.window) else {
            return Err(SchedulerError::NotFound(id));
        };
        self.register_window(target_window);
        if old_window == target_window {
            return self.activate_tab(target_window, id);
        }
        let outcome = self.activate_tab(target_window, id)?;
        if !self.active_by_window.contains_key(&old_window)
            && let Some(next_id) = self
                .window_tabs
                .get(&old_window)
                .and_then(|ids| ids.first().copied())
        {
            let _ = self.activate_tab(old_window, next_id);
        }
        Ok(outcome)
    }

    /// Shuts down and removes `id` from its window, releasing its FD leases and working copies,
    /// and activates an adjacent tab in that window if any remain.
    pub fn close_tab(
        &mut self,
        id: DocumentId,
    ) -> Result<(Option<DocumentId>, Vec<String>), SchedulerError> {
        let Some(old_window) = self.tabs.get(&id).map(|slot| slot.window) else {
            return Err(SchedulerError::NotFound(id));
        };
        let was_active = self.active_by_window.get(&old_window) == Some(&id);
        let removed_index = self
            .window_tabs
            .get(&old_window)
            .and_then(|ids| ids.iter().position(|&entry| entry == id));
        if let Some(mut slot) = self.tabs.remove(&id) {
            if let Some(doc) = slot.tab.document.as_mut() {
                doc.shutdown();
            }
            drop(slot);
        }
        self.quit_approved.remove(&id);
        self.close_approved.remove(&id);
        self.remove_tab_from_window_lists(id);
        if was_active {
            self.active_by_window.remove(&old_window);
        }
        let next_active = if was_active {
            let candidate = self.window_tabs.get(&old_window).and_then(|ids| {
                if ids.is_empty() {
                    None
                } else {
                    let idx = removed_index.unwrap_or(0).min(ids.len() - 1);
                    Some(ids[idx])
                }
            });
            if let Some(next_id) = candidate {
                let _ = self.activate_tab(old_window, next_id);
                self.active_by_window.get(&old_window).copied()
            } else {
                None
            }
        } else {
            self.active_by_window.get(&old_window).copied()
        };
        Ok((next_active, self.working_cleanup.take_errors()))
    }

    /// Closes `window` and all tabs it owns if no dirty tab in `window` remains unapproved.
    /// If any tab in `window` is dirty and neither `close_ready` nor quit-approved, returns
    /// [`SchedulerError::UnapprovedDirtyTabs`] while leaving every tab and edit intact.
    pub fn close_window(&mut self, window: WindowId) -> Result<Vec<String>, SchedulerError> {
        let ids = self.tabs_in_window(window);
        let unapproved: Vec<DocumentId> = ids
            .iter()
            .copied()
            .filter(|&id| {
                self.tabs.get(&id).is_some_and(|slot| {
                    slot_is_dirty(slot)
                        && !self.tab_is_close_approved(id, slot)
                        && !self.tab_is_quit_approved(id, slot)
                })
            })
            .collect();
        if !unapproved.is_empty() {
            return Err(SchedulerError::UnapprovedDirtyTabs(unapproved));
        }
        for id in ids {
            if let Some(mut slot) = self.tabs.remove(&id) {
                if let Some(doc) = slot.tab.document.as_mut() {
                    doc.shutdown();
                }
                drop(slot);
            }
            self.quit_approved.remove(&id);
            self.close_approved.remove(&id);
        }
        self.window_tabs.remove(&window);
        self.active_by_window.remove(&window);
        Ok(self.working_cleanup.take_errors())
    }

    /// Reopens a clean tab in-place with updated format/encoding options (or Reload from Disk),
    /// preserving its `DocumentId` and tab-bar position. Prepares the replacement before mutating
    /// any live tab state so a failed format/encoding change never blanks the active document.
    pub fn reopen_tab(
        &mut self,
        window: WindowId,
        id: DocumentId,
        options: OpenOptions,
        encoding: Option<Encoding>,
    ) -> Result<WakeOutcome, SchedulerError> {
        let Some(slot) = self.tabs.get(&id) else {
            return Err(SchedulerError::NotFound(id));
        };
        if slot_is_dirty(slot) {
            return Err(SchedulerError::OpenFailed(
                "Discard or save your changes before reopening this document.".into(),
            ));
        }
        if slot.tab.document_writing() {
            return Err(SchedulerError::OpenFailed(
                "Wait for the active file operation before reopening this document.".into(),
            ));
        }
        if slot
            .tab
            .document
            .as_ref()
            .is_some_and(Document::is_live_watching)
        {
            return Err(SchedulerError::OpenFailed(
                crate::document::LIVE_WATCH_READ_ONLY_REASON.into(),
            ));
        }
        if slot
            .tab
            .document
            .as_ref()
            .is_some_and(Document::join_reserved)
        {
            return Err(SchedulerError::OpenFailed(JOIN_INPUT_RESERVED.into()));
        }
        let Some(path) = self.tab_logical_path(id) else {
            return Err(SchedulerError::NotFound(id));
        };
        let was_hydrated = matches!(slot.residency, SlotResidency::Hydrated { .. });
        let preserved_bookmarks = slot.tab.document.as_ref().and_then(|doc| {
            (!doc.source_changed && !doc.bookmarks.is_empty()).then(|| {
                (
                    doc.session.source_guard(),
                    doc.session.dialect,
                    doc.session.encoding(),
                    doc.data_start,
                    doc.section_bounds(),
                    doc.source_generation,
                    doc.bookmarks.clone(),
                )
            })
        });
        let existing_section = slot
            .tab
            .document
            .as_ref()
            .and_then(|doc| {
                doc.section_bounds().map(|bounds| {
                    (
                        bounds,
                        doc.section_quote_free_confirmed,
                        doc.session.source_guard(),
                        doc.session.dialect.delimiter,
                        doc.session.encoding(),
                    )
                })
            })
            .or(match &slot.residency {
                SlotResidency::Dormant(dormant) => match &dormant.kind {
                    DormantKind::CleanPlain {
                        section: Some(spec),
                        ..
                    } => Some((
                        spec.bounds,
                        spec.quote_free_confirmed,
                        dormant.source_guard.clone(),
                        spec.delimiter,
                        spec.encoding,
                    )),
                    DormantKind::Dirty(dirty) => dirty
                        .verified_bounds_override
                        .or_else(|| dirty.session.section_bounds())
                        .map(|bounds| {
                            (
                                bounds,
                                dirty.section_quote_free_confirmed,
                                dormant.source_guard.clone(),
                                dirty.session.dialect.delimiter,
                                dirty.session.encoding(),
                            )
                        }),
                    _ => None,
                },
                _ => None,
            });

        // Validate and prepare the replacement document BEFORE touching the existing slot.
        // For section tabs, stay bounded to the section window instead of opening the whole parent.
        // Core resolves Auto encoding/delimiter first and only reuses `prev_bounds` when both the
        // file guard AND resolved `(delimiter, encoding)` match; otherwise it re-snaps `requested`.
        let (mut replacement, source_reloaded) = if let Some((
            prev_bounds,
            quote_free_confirmed,
            prev_guard,
            prev_delimiter,
            prev_encoding,
        )) = existing_section
        {
            let parent_index = self.find_parent_index_for_path(&path);
            let sec_opts = SectionOpenOptions {
                open: options,
                encoding,
                parent_index,
                quote_free_confirmed,
                ..SectionOpenOptions::default()
            };
            match Document::prepare_section_preserved(
                &path,
                prev_bounds,
                &prev_guard,
                prev_delimiter,
                prev_encoding,
                sec_opts.clone(),
            ) {
                Ok(doc) => (doc, false),
                Err(quarry_core::QuarryError::SourceChanged) => (
                    Document::prepare_section(&path, prev_bounds.requested, sec_opts)
                        .map_err(SchedulerError::OpenFailed)?,
                    true,
                ),
                Err(err) => return Err(SchedulerError::OpenFailed(err.to_string())),
            }
        } else {
            (
                Document::prepare_checked(&path, options, encoding)
                    .map_err(open_failure_to_scheduler_error)?,
                false,
            )
        };
        if replacement.is_compressed() && !self.write_slot_available() {
            replacement.shutdown();
            return Err(SchedulerError::OpenFailed(
                compressed::WRITE_SLOTS_FULL.into(),
            ));
        }
        replacement.working_directory = self.config.working_directory.clone();
        replacement.custom_working_directory = self.config.custom_working_directory;
        replacement.index_cache_enabled = self.config.index_cache_enabled;
        replacement.working_cleanup = self.working_cleanup.clone();
        replacement.decompression_slots = self.decompression_slots.clone();
        if !source_reloaded
            && let Some((
                prev_guard,
                prev_dialect,
                prev_encoding,
                prev_data_start,
                prev_bounds,
                prev_generation,
                prev_bookmarks,
            )) = preserved_bookmarks
            && replacement.session.source_guard() == prev_guard
            && replacement.session.dialect.delimiter == prev_dialect.delimiter
            && replacement.session.dialect.has_header == prev_dialect.has_header
            && replacement.session.encoding() == prev_encoding
            && replacement.data_start == prev_data_start
            && replacement.section_bounds() == prev_bounds
        {
            replacement.source_generation = prev_generation;
            replacement.bookmarks = prev_bookmarks;
            replacement.refresh_bookmark_offsets_from_buffers();
        }
        replacement
            .start_indexing()
            .map_err(SchedulerError::OpenFailed)?;

        if was_hydrated {
            let tick = self.next_tick();
            if let Some(slot) = self.tabs.get_mut(&id) {
                if let Some(doc) = slot.tab.document.as_mut() {
                    doc.shutdown();
                }
                slot.tab.document = Some(replacement);
                slot.tab.delimiter_mode =
                    crate::tab::DelimiterMode::from_delimiter(options.delimiter);
                slot.tab.header_mode = options.header_mode;
                slot.tab.encoding_mode = encoding;
                slot.tab.notice = None;
                slot.tab.footer_status = None;
                slot.window = window;
                slot.last_used_tick = tick;
            }
            self.active_by_window.insert(window, id);
            let _ = self.sync_fd_leases();
            return Ok(WakeOutcome::Ready {
                document_id: id,
                kind: WakeKind::AlreadyHydrated,
                evicted: Vec::new(),
            });
        }

        // For Stub or Dormant tabs, shut down the pre-check document and hydrate normally now
        // that we know the target path + options + encoding open cleanly. Save prior slot state
        // so an activation failure (e.g. FD budget exhausted) leaves the dormant/stub tab intact.
        let replacement_section = replacement
            .section_bounds()
            .map(|bounds| DormantSectionSpec {
                bounds,
                quote_free_confirmed: replacement.section_quote_free_confirmed,
                delimiter: replacement.session.dialect.delimiter,
                encoding: replacement.session.encoding(),
            });
        let replacement_guard = replacement.session.source_guard();
        let replacement_view = DormantViewSnapshot {
            viewport_start: replacement.viewport_start,
            scroll_points: replacement.scroll_points,
            visible_rows: replacement.visible_rows,
            data_start: replacement.data_start,
            selection: replacement.selection,
            cursor: replacement.cursor.clone(),
            selected_rows: replacement.selected_rows.clone(),
            selected_columns: replacement.selected_columns.clone(),
            column_selection_anchor: replacement.column_selection_anchor,
            columns: replacement.columns.clone(),
            headers: replacement.headers.clone(),
            total_columns: replacement.total_columns,
            filter_query: None,
            filter_viewport_start: 0,
            edit_history: crate::history::EditHistory::default(),
            source_generation: replacement.source_generation,
            bookmarks: RowBookmarks::new(),
        };
        replacement.shutdown();
        let canonical_path = canonicalize_best_effort(&path);
        let file_identity = FileIdentity::from_path(&path).ok().flatten();
        let prev_active = self.active_by_window.get(&window).copied();
        let prev_slot_state = self.tabs.get_mut(&id).map(|slot| {
            let old_delimiter_mode = slot.tab.delimiter_mode;
            let old_header_mode = slot.tab.header_mode;
            let old_encoding_mode = slot.tab.encoding_mode;
            let old_notice = slot.tab.notice.take();
            let old_footer_status = slot.tab.footer_status.take();
            let new_residency = if let Some(sec_spec) = replacement_section {
                let view = match &slot.residency {
                    SlotResidency::Dormant(dormant) => {
                        let (prev_bounds, prev_delimiter, prev_encoding) = match &dormant.kind {
                            DormantKind::CleanPlain {
                                section: Some(s), ..
                            } => (Some(s.bounds), Some(s.delimiter), Some(s.encoding)),
                            DormantKind::Dirty(d) => (
                                d.verified_bounds_override
                                    .or_else(|| d.session.section_bounds()),
                                Some(d.session.dialect.delimiter),
                                Some(d.session.encoding()),
                            ),
                            _ => (None, None, None),
                        };
                        let format_changed = source_reloaded
                            || dormant.source_changed
                            || old_header_mode != options.header_mode
                            || old_encoding_mode != encoding
                            || dormant.view.data_start != replacement_view.data_start
                            || prev_bounds.map(|b| b.snapped) != Some(sec_spec.bounds.snapped)
                            || prev_delimiter != Some(sec_spec.delimiter)
                            || prev_encoding != Some(sec_spec.encoding);
                        if format_changed {
                            replacement_view.clone()
                        } else {
                            dormant.view.clone()
                        }
                    }
                    _ => replacement_view.clone(),
                };
                SlotResidency::Dormant(Box::new(DormantDocument {
                    logical_path: path.clone(),
                    canonical_path: canonical_path.clone(),
                    file_identity,
                    source_guard: replacement_guard,
                    source_changed: false,
                    view,
                    kind: DormantKind::CleanPlain {
                        open_options: options,
                        encoding,
                        section: Some(sec_spec),
                    },
                }))
            } else {
                SlotResidency::Stub(LazyDocumentStub {
                    logical_path: path.clone(),
                    canonical_path,
                    file_identity,
                    open_options: options,
                    encoding,
                })
            };
            let old_residency = std::mem::replace(&mut slot.residency, new_residency);
            slot.tab.delimiter_mode = crate::tab::DelimiterMode::from_delimiter(options.delimiter);
            slot.tab.header_mode = options.header_mode;
            slot.tab.encoding_mode = encoding;
            (
                old_delimiter_mode,
                old_header_mode,
                old_encoding_mode,
                old_notice,
                old_footer_status,
                old_residency,
            )
        });
        if self.active_by_window.get(&window) == Some(&id) {
            self.active_by_window.remove(&window);
        }
        match self.activate_tab(window, id) {
            Ok(outcome) => Ok(outcome),
            Err(error) => {
                if let (
                    Some(slot),
                    Some((
                        old_delimiter_mode,
                        old_header_mode,
                        old_encoding_mode,
                        old_notice,
                        old_footer_status,
                        old_residency,
                    )),
                ) = (self.tabs.get_mut(&id), prev_slot_state)
                {
                    slot.tab.delimiter_mode = old_delimiter_mode;
                    slot.tab.header_mode = old_header_mode;
                    slot.tab.encoding_mode = old_encoding_mode;
                    slot.tab.notice = old_notice;
                    slot.tab.footer_status = old_footer_status;
                    slot.residency = old_residency;
                }
                if let Some(active) = prev_active {
                    self.active_by_window.insert(window, active);
                }
                Err(error)
            }
        }
    }

    /// Applies one value-edit or structural-working-copy undo/redo step on hydrated tab `id`.
    pub fn apply_undo_redo(&mut self, id: DocumentId, redo: bool) -> Result<String, AppMessage> {
        if let Some(conflict) = self.sibling_dirty_conflict(id) {
            return Err(AppMessage::warning(conflict));
        }
        let Some(slot) = self.tabs.get_mut(&id) else {
            return Err(AppMessage::warning("Open a file before undoing a change."));
        };
        let Some(current) = slot.tab.document.as_mut() else {
            return Err(AppMessage::warning("Open a file before undoing a change."));
        };
        if !current.history_ready() {
            return Err(AppMessage::warning(if current.source_changed {
                SOURCE_CHANGED_NOTICE
            } else {
                "Finish editing and wait for active operations before Undo or Redo."
            }));
        }
        match current.session.ensure_source_unchanged() {
            Ok(()) => {}
            Err(QuarryError::SourceChanged) => {
                current.invalidate_changed_source();
                return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            }
            Err(error) => return Err(AppMessage::core_error(error)),
        }
        let edit = if redo {
            current.edit_history.redo.pop()
        } else {
            current.edit_history.undo.pop_back()
        };
        if let Some(edit) = edit {
            current.set_edit_value(
                edit.target,
                if redo {
                    edit.after.clone()
                } else {
                    edit.before.clone()
                },
            );
            current.clear_edit_search();
            if redo {
                current.edit_history.undo.push_back(edit);
            } else {
                current.edit_history.redo.push(edit);
            }
            let message = if redo {
                "Edit restored."
            } else {
                "Edit undone."
            };
            slot.tab.footer_status = Some(AppMessage::status(message));
            let _ = self.sync_fd_leases();
            return Ok(message.into());
        }
        if current.filter_active() {
            return Err(AppMessage::warning(
                "Clear the filter before undoing structural changes.",
            ));
        }
        current.commit_edits();
        if !redo && (!current.header_renames.is_empty() || current.has_cell_edits()) {
            return Err(AppMessage::warning(
                "Save or discard later header and cell edits before undoing the change.",
            ));
        }
        if current.save_job.is_some()
            || current.export_job.is_some()
            || current.structural_job.is_some()
        {
            return Err(AppMessage::warning(
                "Wait for the active file operation to finish.",
            ));
        }
        let state = current
            .working_copy
            .as_ref()
            .ok_or_else(|| AppMessage::warning("There is no change to undo or redo."))?;
        let target = if redo {
            state.redo.clone()
        } else {
            state.undo.clone()
        }
        .ok_or_else(|| {
            AppMessage::warning(if redo {
                "There is no change to redo."
            } else {
                "There is no change to undo."
            })
        })?;
        if !redo
            && target.path == current.logical_path
            && let Some(original) = current.original_session.as_ref()
        {
            match original.ensure_source_unchanged() {
                Ok(()) => {}
                Err(QuarryError::SourceChanged) => {
                    current.invalidate_changed_source();
                    return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
                }
                Err(error) => return Err(AppMessage::core_error(error)),
            }
        }

        let options = current.current_open_options();
        let logical_path = current.logical_path.clone();
        let encoding = current.session.encoding();
        let current_snapshot = WorkingCopySnapshot {
            path: current.session.path().to_path_buf(),
            overlay: StructuralOverlay {
                header_renames: current.header_renames.as_ref().clone(),
                cell_edits: current.cell_edits.as_ref().clone(),
            },
            edit_history: current.edit_history.clone(),
            source_generation: current.source_generation,
            bookmarks: current.bookmarks.clone(),
        };
        // Reserve descriptor headroom for the concurrent replacement open + its index worker
        // BEFORE opening or mutating live state so admission failure leaves history untouched.
        let temp_open_fd = self
            .fd_budget
            .try_reserve(FdCategory::ActiveDocument, 1)
            .map_err(|error| AppMessage::warning(error.to_string()))?;
        let index_job_fd = self
            .fd_budget
            .try_reserve(FdCategory::BackgroundJob, 1)
            .map_err(|error| AppMessage::warning(error.to_string()))?;

        let is_section = current.is_section();
        let section_quote_free = current.section_quote_free_confirmed;
        let mut replacement = if is_section {
            let original = current.original_session.as_ref().ok_or_else(|| {
                AppMessage::error("Missing original section provenance for undo/redo.")
            })?;
            if target.path == logical_path {
                let orig_bounds = original.section_bounds().ok_or_else(|| {
                    AppMessage::error("Original session is missing section bounds.")
                })?;
                let sec_opts = SectionOpenOptions {
                    open: options,
                    encoding: Some(encoding),
                    parent_index: None,
                    quote_free_confirmed: section_quote_free,
                    ..SectionOpenOptions::default()
                };
                Document::prepare_section_preserved(
                    &logical_path,
                    orig_bounds,
                    &original.source_guard(),
                    original.dialect.delimiter,
                    original.encoding(),
                    sec_opts,
                )
                .map_err(|error| match error {
                    QuarryError::SourceChanged => AppMessage::warning(SOURCE_CHANGED_NOTICE),
                    other => AppMessage::core_error(other),
                })?
            } else {
                Document::prepare_section_working_copy(
                    &target.path,
                    original,
                    options,
                    section_quote_free,
                )
                .map_err(AppMessage::error)?
            }
        } else {
            Document::prepare_with_encoding(&target.path, options, Some(encoding))
                .map_err(AppMessage::error)?
        };
        replacement.header_renames = Arc::new(target.overlay.header_renames.clone());
        replacement.cell_edits = Arc::new(target.overlay.cell_edits.clone());
        replacement.edit_history = target.edit_history;
        replacement.source_generation = target.source_generation;
        replacement.bookmarks = target.bookmarks;
        replacement.refresh_bookmark_offsets_from_buffers();
        replacement.refresh_column_headers();
        replacement.index_cache_enabled = self.config.index_cache_enabled;
        replacement.start_indexing().map_err(AppMessage::error)?;

        let mut state = current
            .working_copy
            .take()
            .expect("the history target came from working-copy state");
        if redo {
            state.redo = None;
            state.undo = Some(current_snapshot);
        } else {
            state.undo = None;
            state.redo = Some(current_snapshot);
        }
        replacement.logical_path = logical_path;
        replacement.original_session = current.original_session.take();
        replacement.working_copy = Some(state);
        replacement.working_directory = self.config.working_directory.clone();
        replacement.custom_working_directory = self.config.custom_working_directory;
        replacement.working_cleanup = self.working_cleanup.clone();
        replacement.decompression_slots = self.decompression_slots.clone();
        current.shutdown();
        drop(temp_open_fd);
        slot.tab.document = Some(replacement);
        if let SlotResidency::Hydrated { job_fd_lease, .. } = &mut slot.residency {
            *job_fd_lease = Some(index_job_fd);
        }
        slot.tab.structural_dialog = None;
        slot.tab.columns_open = false;
        slot.tab.notice = None;
        let message = if redo {
            "Change restored."
        } else {
            "Change undone."
        };
        slot.tab.footer_status = Some(AppMessage::status(message));
        let _ = self.sync_fd_leases();
        Ok(message.into())
    }

    /// Discards unsaved edits and working copies on hydrated tab `id`.
    pub fn discard_tab_edits(&mut self, id: DocumentId) -> Result<(), AppMessage> {
        let Some(slot) = self.tabs.get_mut(&id) else {
            return Err(AppMessage::warning("Open a file first."));
        };
        let Some(doc) = slot.tab.document.as_mut() else {
            return Err(AppMessage::warning("Open a file first."));
        };
        if doc.join_reserved() {
            return Err(AppMessage::warning(JOIN_INPUT_RESERVED));
        }
        if doc.save_job.is_some() || doc.export_job.is_some() || doc.structural_job.is_some() {
            return Err(AppMessage::warning(
                "Wait for the active file operation to finish before discarding changes.",
            ));
        }
        let logical_path = doc.logical_path.clone();
        let options = doc.current_open_options();
        let encoding = Some(doc.session.encoding());
        let had_working_copy = doc.working_copy.is_some() || doc.original_session.is_some();
        let is_section = doc.is_section();
        let section_bounds = doc.section_bounds();
        let section_quote_free = doc.section_quote_free_confirmed;
        let orig_guard = doc
            .original_session
            .as_ref()
            .unwrap_or(&doc.session)
            .source_guard();
        let orig_delimiter = doc.session.dialect.delimiter;
        let orig_encoding = doc.session.encoding();
        if had_working_copy {
            let temp_open_fd = self
                .fd_budget
                .try_reserve(FdCategory::ActiveDocument, 1)
                .map_err(|error| AppMessage::warning(error.to_string()))?;
            let index_job_fd = self
                .fd_budget
                .try_reserve(FdCategory::BackgroundJob, 1)
                .map_err(|error| AppMessage::warning(error.to_string()))?;
            let mut replacement = if is_section && let Some(bounds) = section_bounds {
                let sec_opts = SectionOpenOptions {
                    open: options,
                    encoding,
                    parent_index: None,
                    quote_free_confirmed: section_quote_free,
                    ..SectionOpenOptions::default()
                };
                match Document::prepare_section_preserved(
                    &logical_path,
                    bounds,
                    &orig_guard,
                    orig_delimiter,
                    orig_encoding,
                    sec_opts.clone(),
                ) {
                    Ok(doc) => doc,
                    Err(QuarryError::SourceChanged) => {
                        Document::prepare_section(&logical_path, bounds.requested, sec_opts)
                            .map_err(AppMessage::error)?
                    }
                    Err(error) => return Err(AppMessage::core_error(error)),
                }
            } else {
                Document::prepare_with_encoding(&logical_path, options, encoding)
                    .map_err(AppMessage::error)?
            };
            replacement.working_directory = self.config.working_directory.clone();
            replacement.custom_working_directory = self.config.custom_working_directory;
            replacement.index_cache_enabled = self.config.index_cache_enabled;
            replacement.working_cleanup = self.working_cleanup.clone();
            replacement.decompression_slots = self.decompression_slots.clone();
            replacement.start_indexing().map_err(AppMessage::error)?;
            doc.shutdown();
            drop(temp_open_fd);
            slot.tab.document = Some(replacement);
            if let SlotResidency::Hydrated { job_fd_lease, .. } = &mut slot.residency {
                *job_fd_lease = Some(index_job_fd);
            }
        } else {
            doc.discard_edits();
        }
        slot.tab.notice = None;
        slot.tab.footer_status = Some(AppMessage::status("Changes discarded."));
        let _ = self.sync_fd_leases();
        Ok(())
    }

    /// Polls active save jobs across all hydrated tabs, reloading saved documents cleanly and
    /// clearing write reservations on finish or cancellation.
    pub fn poll_saves(&mut self) -> Vec<SaveCompletion> {
        let mut results = Vec::new();
        let mut any_finished = false;
        let mut in_place_saved_aliases: Vec<(
            DocumentId,
            Option<FileIdentity>,
            Option<FileIdentity>,
            PathBuf,
        )> = Vec::new();
        for (&id, slot) in &mut self.tabs {
            if !matches!(slot.residency, SlotResidency::Hydrated { .. }) {
                continue;
            }
            let Some(doc) = slot.tab.document.as_mut() else {
                continue;
            };
            if doc.save_job.is_none() {
                continue;
            }
            match doc.poll_save() {
                Ok(Some((destination, in_place))) => {
                    any_finished = true;
                    slot.tab.write_destination = None;
                    if !in_place {
                        doc.save_status = None;
                        results.push(SaveCompletion {
                            document_id: id,
                            result: Ok((destination, false)),
                        });
                        continue;
                    }
                    let old_identity = doc
                        .original_session
                        .as_ref()
                        .unwrap_or(&doc.session)
                        .file_identity();
                    let canonical_dest = canonicalize_best_effort(&destination);
                    let splice_summary = doc.last_splice_summary.take();
                    let section_quote_free = doc.section_quote_free_confirmed;
                    let dialect = doc.session.dialect;
                    let options = OpenOptions {
                        delimiter: Some(dialect.delimiter),
                        header_mode: if dialect.has_header {
                            quarry_core::HeaderMode::FirstRow
                        } else {
                            quarry_core::HeaderMode::NoHeader
                        },
                        ..OpenOptions::default()
                    };
                    let output_encoding = doc
                        .save_output_encoding
                        .unwrap_or_else(|| doc.session.encoding());
                    let source_encoding = doc.session.encoding();
                    let prior_encoding_mode = slot.tab.encoding_mode;

                    let next_generation = doc.source_generation.saturating_add(1);
                    let mut carried_bookmarks = doc.bookmarks.clone();
                    carried_bookmarks
                        .apply_transform(next_generation, &BookmarkTransform::PreserveSameRows);
                    let reload = if let Some(splice) = splice_summary {
                        let sec_opts = SectionOpenOptions {
                            open: options,
                            encoding: Some(output_encoding),
                            parent_index: None,
                            quote_free_confirmed: section_quote_free,
                            ..SectionOpenOptions::default()
                        };
                        Document::prepare_section_preserved(
                            &destination,
                            splice.new_bounds,
                            &splice.published_guard,
                            dialect.delimiter,
                            output_encoding,
                            sec_opts,
                        )
                        .map_err(|error| match error {
                            QuarryError::SourceChanged => {
                                "the file was modified externally right after saving".to_string()
                            }
                            other => other.to_string(),
                        })
                    } else {
                        Document::prepare_with_encoding(
                            &destination,
                            options,
                            Some(output_encoding),
                        )
                    }
                    .and_then(|mut replacement| {
                        replacement.working_directory = self.config.working_directory.clone();
                        replacement.custom_working_directory = self.config.custom_working_directory;
                        replacement.index_cache_enabled = self.config.index_cache_enabled;
                        replacement.working_cleanup = self.working_cleanup.clone();
                        replacement.decompression_slots = self.decompression_slots.clone();
                        replacement.source_generation = next_generation;
                        replacement.bookmarks = carried_bookmarks;
                        replacement.refresh_bookmark_offsets_from_buffers();
                        replacement.start_indexing()?;
                        Ok(replacement)
                    });

                    match reload {
                        Ok(replacement) => {
                            let new_identity = replacement.session.file_identity();
                            doc.shutdown();
                            slot.tab.document = Some(replacement);
                            slot.tab.encoding_mode = if output_encoding == source_encoding {
                                prior_encoding_mode
                            } else {
                                Some(output_encoding)
                            };
                            slot.tab.notice = None;
                            in_place_saved_aliases.push((
                                id,
                                old_identity,
                                new_identity,
                                canonical_dest,
                            ));
                            results.push(SaveCompletion {
                                document_id: id,
                                result: Ok((destination, true)),
                            });
                        }
                        Err(error) => {
                            doc.shutdown();
                            slot.tab.document = None;
                            in_place_saved_aliases.push((id, old_identity, None, canonical_dest));
                            results.push(SaveCompletion {
                                document_id: id,
                                result: Err(AppMessage::error(format!(
                                    "Saved {} but could not reload it: {error}. Reopen the file to continue.",
                                    destination.display()
                                ))),
                            });
                        }
                    }
                }
                Ok(None) => {
                    if doc.save_job.is_none() {
                        // Save was cancelled cleanly; report a terminal outcome so any
                        // in-flight close/quit review cancels cleanly while keeping edits.
                        any_finished = true;
                        slot.tab.write_destination = None;
                        slot.tab.close_after_save = false;
                        let notice = doc
                            .save_status
                            .take()
                            .unwrap_or_else(|| "Save cancelled.".into());
                        results.push(SaveCompletion {
                            document_id: id,
                            result: Err(AppMessage::status(notice)),
                        });
                    }
                }
                Err(error) => {
                    any_finished = true;
                    slot.tab.write_destination = None;
                    results.push(SaveCompletion {
                        document_id: id,
                        result: Err(error),
                    });
                }
            }
        }
        // ADR 0015 post-splice tab refresh: immediately mark all other open clean/dormant parent
        // and sibling section tabs aliasing a file that was just saved in-place as `source_changed`
        // rather than silently reloading them.
        for (saved_id, old_identity, new_identity, saved_canonical) in in_place_saved_aliases {
            let other_ids: Vec<DocumentId> = self
                .tabs
                .keys()
                .copied()
                .filter(|&other_id| other_id != saved_id)
                .collect();
            for other_id in other_ids {
                let Some((other_identity, other_canonical)) = self.tab_file_alias_key(other_id)
                else {
                    continue;
                };
                let aliases = other_canonical == saved_canonical
                    || other_identity
                        .is_some_and(|oid| old_identity == Some(oid) || new_identity == Some(oid));
                if !aliases {
                    continue;
                }
                let Some(other_slot) = self.tabs.get_mut(&other_id) else {
                    continue;
                };
                match &mut other_slot.residency {
                    SlotResidency::Hydrated { .. } => {
                        if let Some(other_doc) = other_slot.tab.document.as_mut() {
                            if other_doc
                                .export_job
                                .as_ref()
                                .is_some_and(|job| job.progress().done)
                            {
                                let _ = other_doc.poll_filtered_export();
                            }
                            other_doc.invalidate_changed_source();
                            other_slot.tab.write_destination = None;
                            other_slot.tab.close_after_save = false;
                            other_slot.tab.notice =
                                Some(AppMessage::warning(SOURCE_CHANGED_NOTICE));
                        }
                    }
                    SlotResidency::Dormant(dormant) => {
                        dormant.source_changed = true;
                        other_slot.tab.write_destination = None;
                        other_slot.tab.close_after_save = false;
                        other_slot.tab.notice = Some(AppMessage::warning(SOURCE_CHANGED_NOTICE));
                    }
                    SlotResidency::Stub(_) => {}
                }
            }
        }
        if any_finished {
            let _ = self.sync_fd_leases();
        }
        results
    }

    /// Polls active search jobs across all hydrated tabs, returning newly revealed cell coordinates.
    pub fn poll_searches(&mut self) -> Vec<SearchCompletion> {
        let mut results = Vec::new();
        let mut any_finished = false;
        for (&id, slot) in &mut self.tabs {
            if !matches!(slot.residency, SlotResidency::Hydrated { .. }) {
                continue;
            }
            let Some(doc) = slot.tab.document.as_mut() else {
                continue;
            };
            if doc.search_job.is_none() {
                continue;
            }
            match doc.poll_search() {
                Ok(()) => {
                    if doc.search_job.is_none() {
                        any_finished = true;
                        let revealed = doc.reveal_cell.take();
                        results.push(SearchCompletion {
                            document_id: id,
                            result: Ok(revealed),
                        });
                    }
                }
                Err(error) => {
                    any_finished = true;
                    results.push(SearchCompletion {
                        document_id: id,
                        result: Err(error),
                    });
                }
            }
        }
        if any_finished {
            let _ = self.sync_fd_leases();
        }
        results
    }

    /// Polls active filtered-export jobs across all hydrated tabs, clearing write reservations.
    pub fn poll_filtered_exports(&mut self) -> Vec<ExportCompletion> {
        let mut results = Vec::new();
        let mut any_finished = false;
        for (&id, slot) in &mut self.tabs {
            if !matches!(slot.residency, SlotResidency::Hydrated { .. }) {
                continue;
            }
            let Some(doc) = slot.tab.document.as_mut() else {
                continue;
            };
            if doc.export_job.is_none() {
                continue;
            }
            match doc.poll_filtered_export() {
                Ok(()) => {
                    if doc.export_job.is_none() {
                        any_finished = true;
                        slot.tab.write_destination = None;
                        results.push(ExportCompletion {
                            document_id: id,
                            result: Ok(doc.export_status.clone()),
                        });
                    }
                }
                Err(error) => {
                    any_finished = true;
                    slot.tab.write_destination = None;
                    results.push(ExportCompletion {
                        document_id: id,
                        result: Err(error),
                    });
                }
            }
        }
        if any_finished {
            let _ = self.sync_fd_leases();
        }
        results
    }

    fn prepare_materialized_document(
        &self,
        path: &Path,
        options: OpenOptions,
        encoding: Encoding,
        output_index: Option<CompletedIndex>,
    ) -> Result<Document, String> {
        let mut replacement = Document::prepare_with_encoding(path, options, Some(encoding))?;
        replacement.working_directory = self.config.working_directory.clone();
        replacement.custom_working_directory = self.config.custom_working_directory;
        replacement.index_cache_enabled = self.config.index_cache_enabled;
        replacement.working_cleanup = self.working_cleanup.clone();
        replacement.decompression_slots = self.decompression_slots.clone();
        if let Some(index) = output_index {
            let index = replacement
                .session
                .adopt_completed_index(index)
                .map_err(|error| error.to_string())?;
            replacement.progress.bytes_scanned = index.indexed_bytes();
            replacement.progress.rows_scanned = index.indexed_rows();
            replacement.progress.done = true;
            replacement.progress.columns_found = index.maximum_width();
            replacement.apply_indexed_width(index.maximum_width());
            replacement.index = Some(index);
            replacement.load_buffer(replacement.viewport_start)?;
        } else {
            replacement.start_indexing()?;
        }
        Ok(replacement)
    }

    fn install_prepared_working_copy(
        &mut self,
        id: DocumentId,
        mut replacement: Document,
        ready: MaterializedWorkingCopy,
        index_job_fd: Option<FdLease>,
    ) {
        let Some(slot) = self.tabs.get_mut(&id) else {
            replacement.shutdown();
            return;
        };
        let Some(current) = slot.tab.document.as_mut() else {
            replacement.shutdown();
            return;
        };
        let mut undo = ready.undo;
        undo.bookmarks = current.bookmarks.clone();
        undo.source_generation = current.source_generation;
        current.accept_materialized_change(undo);
        let next_generation = current.source_generation.saturating_add(1);
        let mut carried_bookmarks = current.bookmarks.clone();
        let bookmark_affected =
            carried_bookmarks.apply_transform(next_generation, &ready.bookmark_transform);
        let mut notice = ready.notice;
        if bookmark_affected > 0 {
            match &ready.bookmark_transform {
                BookmarkTransform::ClearRowOrderChanged => {
                    notice.push_str(
                        " Row bookmarks were cleared because row order changed (Undo to restore).",
                    );
                }
                BookmarkTransform::ShiftDeleteRows { .. } => {
                    notice.push_str(&format!(
                        " {bookmark_affected} row bookmark{} on deleted rows removed (Undo to restore).",
                        if bookmark_affected == 1 { "" } else { "s" }
                    ));
                }
                _ => {}
            }
        }
        replacement.source_generation = next_generation;
        replacement.bookmarks = carried_bookmarks;
        let mut job_fd_lease_to_install = index_job_fd;
        if replacement.index.is_some() {
            replacement.refresh_bookmark_offsets_from_buffers();
            if !replacement
                .bookmarks
                .pending_offset_rows(replacement.source_generation)
                .is_empty()
            {
                if job_fd_lease_to_install.is_none() {
                    job_fd_lease_to_install = self
                        .fd_budget
                        .try_reserve(FdCategory::BackgroundJob, 1)
                        .ok();
                }
                if job_fd_lease_to_install.is_some() {
                    if !replacement.start_bookmark_offset_refresh() {
                        job_fd_lease_to_install = None;
                        replacement.bookmark_offset_refresh_pending = true;
                    }
                } else {
                    replacement.bookmark_offset_refresh_pending = true;
                }
            }
        } else {
            replacement.refresh_bookmark_offsets_from_buffers();
        }
        replacement.logical_path = current.logical_path.clone();
        replacement.original_session = current.original_session.take();
        replacement.working_copy = current.working_copy.take();
        replacement.section_quote_free_confirmed = current.section_quote_free_confirmed;
        replacement.working_directory = self.config.working_directory.clone();
        replacement.custom_working_directory = self.config.custom_working_directory;
        replacement.working_cleanup = self.working_cleanup.clone();
        replacement.decompression_slots = self.decompression_slots.clone();
        replacement.selected_columns = ready.selected_columns;
        let first_selected = replacement.selected_columns.iter().next().copied();
        replacement.column_selection_anchor = first_selected;
        if let Some(first_selected) = first_selected
            && replacement.columns.view(first_selected)
        {
            replacement.refresh_column_headers();
        }
        current.shutdown();
        slot.tab.document = Some(replacement);
        if let Some(lease) = job_fd_lease_to_install
            && let SlotResidency::Hydrated { job_fd_lease, .. } = &mut slot.residency
        {
            *job_fd_lease = Some(lease);
        }
        slot.tab.format_draft = None;
        slot.tab.columns_open = false;
        slot.tab.structural_dialog = None;
        slot.tab.notice = None;
        slot.tab.footer_status = Some(AppMessage::status(notice));
        self.close_approved.remove(&id);
    }

    /// Polls active structural edit jobs (Sort, Replace All, Cleanup, Split, Combine, Insert/Delete
    /// Rows/Columns, Find Duplicates) across all hydrated tabs, installing materialized working
    /// copies with pre-reserved FD leases.
    pub fn poll_structural_edits(&mut self) -> Vec<StructuralCompletion> {
        let mut results = Vec::new();
        let mut ready_to_install: Vec<(DocumentId, MaterializedWorkingCopy)> = Vec::new();
        let mut any_finished = false;
        let fd_budget = self.fd_budget.clone();
        let ids: Vec<DocumentId> = self.tabs.keys().copied().collect();
        for id in ids {
            let Some(slot) = self.tabs.get_mut(&id) else {
                continue;
            };
            let SlotResidency::Hydrated {
                fd_lease,
                temp_dir_fd_lease,
                job_fd_lease,
                ..
            } = &mut slot.residency
            else {
                continue;
            };
            let Some(doc) = slot.tab.document.as_mut() else {
                continue;
            };
            if doc.structural_job.is_none()
                || matches!(
                    doc.structural_job,
                    Some(StructuralJob::DuplicatePreview { .. })
                )
            {
                continue;
            }
            let advancing_split_analysis = matches!(
                doc.structural_job,
                Some(StructuralJob::AnalyzingSplit { .. })
            ) && !doc.structural_cancel_requested;
            if advancing_split_analysis {
                // `poll_structural_edit()` can transition a completed split analysis directly into
                // `begin_materialization()`, which allocates `original_session` and working-copy
                // storage. Pre-reserve those descriptors before polling, or defer until capacity
                // is available so the transition can never bypass the scheduler's FD cap.
                let base_doc_fds = doc_active_document_fds(doc);
                let base_dir_fds = doc_retained_dir_fds(doc);
                let base_job_fds = doc_active_job_fds(doc);
                let target_doc_fds = base_doc_fds.max(2);
                let target_dir_fds = base_dir_fds + usize::from(doc.working_copy.is_none());
                let target_job_fds = base_job_fds.max(2);
                let reserve_result = (|| -> Result<(), SchedulerError> {
                    fd_lease.try_resize(target_doc_fds)?;
                    try_sync_optional_lease(
                        &fd_budget,
                        temp_dir_fd_lease,
                        FdCategory::TempDirectory,
                        target_dir_fds,
                    )?;
                    try_sync_optional_lease(
                        &fd_budget,
                        job_fd_lease,
                        FdCategory::BackgroundJob,
                        target_job_fds,
                    )?;
                    Ok(())
                })();
                if reserve_result.is_err() {
                    let _ = fd_lease.try_resize(base_doc_fds);
                    let _ = try_sync_optional_lease(
                        &fd_budget,
                        temp_dir_fd_lease,
                        FdCategory::TempDirectory,
                        base_dir_fds,
                    );
                    let _ = try_sync_optional_lease(
                        &fd_budget,
                        job_fd_lease,
                        FdCategory::BackgroundJob,
                        base_job_fds,
                    );
                    continue;
                }
            }
            match doc.poll_structural_edit() {
                Ok(Some(ready)) => {
                    any_finished = true;
                    ready_to_install.push((id, ready));
                }
                Ok(None) => {
                    if doc.structural_job.is_none()
                        || matches!(
                            doc.structural_job,
                            Some(StructuralJob::DuplicatePreview { .. })
                        )
                    {
                        any_finished = true;
                        results.push(StructuralCompletion {
                            document_id: id,
                            result: Ok(doc.structural_status.clone()),
                        });
                    }
                }
                Err(error) => {
                    any_finished = true;
                    results.push(StructuralCompletion {
                        document_id: id,
                        result: Err(error),
                    });
                }
            }
            let _ = fd_lease.try_resize(doc_active_document_fds(doc));
            let _ = try_sync_optional_lease(
                &fd_budget,
                temp_dir_fd_lease,
                FdCategory::TempDirectory,
                doc_retained_dir_fds(doc),
            );
            let _ = try_sync_optional_lease(
                &fd_budget,
                job_fd_lease,
                FdCategory::BackgroundJob,
                doc_active_job_fds(doc),
            );
        }

        for (id, mut ready) in ready_to_install {
            let Some((options, encoding, is_section, section_quote_free)) =
                self.tabs.get(&id).and_then(|slot| {
                    slot.tab.document.as_ref().map(|doc| {
                        (
                            doc.current_open_options(),
                            doc.session.encoding(),
                            doc.is_section(),
                            doc.section_quote_free_confirmed,
                        )
                    })
                })
            else {
                continue;
            };
            let needs_index_worker = ready.output_index.is_none();
            let temp_open_fd = match self.fd_budget.try_reserve(FdCategory::ActiveDocument, 1) {
                Ok(lease) => lease,
                Err(error) => {
                    self.cleanup_abandoned_working_copy_path(id, &ready.path);
                    results.push(StructuralCompletion {
                        document_id: id,
                        result: Err(AppMessage::warning(error.to_string())),
                    });
                    continue;
                }
            };
            let index_job_fd = if needs_index_worker {
                match self.fd_budget.try_reserve(FdCategory::BackgroundJob, 1) {
                    Ok(lease) => Some(lease),
                    Err(error) => {
                        drop(temp_open_fd);
                        self.cleanup_abandoned_working_copy_path(id, &ready.path);
                        results.push(StructuralCompletion {
                            document_id: id,
                            result: Err(AppMessage::warning(error.to_string())),
                        });
                        continue;
                    }
                }
            } else {
                None
            };
            let output_index = ready.output_index.take();
            let prepared = if is_section {
                let original_parent = self.tabs.get(&id).and_then(|slot| {
                    slot.tab
                        .document
                        .as_ref()
                        .and_then(|doc| doc.original_session.as_ref())
                });
                match original_parent {
                    Some(original) => Document::prepare_section_working_copy(
                        &ready.path,
                        original,
                        options,
                        section_quote_free,
                    )
                    .and_then(|mut replacement| {
                        replacement.working_directory = self.config.working_directory.clone();
                        replacement.custom_working_directory = self.config.custom_working_directory;
                        replacement.index_cache_enabled = self.config.index_cache_enabled;
                        replacement.working_cleanup = self.working_cleanup.clone();
                        replacement.decompression_slots = self.decompression_slots.clone();
                        replacement.start_indexing()?;
                        Ok(replacement)
                    }),
                    None => Err("Missing original section session for working copy.".into()),
                }
            } else {
                self.prepare_materialized_document(&ready.path, options, encoding, output_index)
            };
            match prepared {
                Ok(replacement) => {
                    let fallback_notice = ready.notice.clone();
                    self.install_prepared_working_copy(id, replacement, ready, index_job_fd);
                    let notice = self
                        .tabs
                        .get(&id)
                        .and_then(|slot| slot.tab.footer_status.as_ref())
                        .map(|msg| msg.text.clone())
                        .unwrap_or(fallback_notice);
                    drop(temp_open_fd);
                    results.push(StructuralCompletion {
                        document_id: id,
                        result: Ok(Some(notice)),
                    });
                }
                Err(error) => {
                    drop(index_job_fd);
                    drop(temp_open_fd);
                    self.cleanup_abandoned_working_copy_path(id, &ready.path);
                    results.push(StructuralCompletion {
                        document_id: id,
                        result: Err(AppMessage::error(error)),
                    });
                }
            }
        }

        if any_finished {
            let _ = self.sync_fd_leases();
        }
        results
    }

    fn cleanup_abandoned_working_copy_path(&mut self, id: DocumentId, path: &Path) {
        if let Some(doc) = self
            .tabs
            .get_mut(&id)
            .and_then(|slot| slot.tab.document.as_mut())
            && let Some(wc) = doc.working_copy.as_ref()
            && wc.owns(path)
            && let Err(error) = wc.remove(path)
        {
            doc.working_cleanup.report(error);
        }
    }

    /// Confirms removal of duplicates from a completed `StructuralJob::DuplicatePreview`, adopting
    /// its pre-built working copy and completed index cleanly with FD pre-reservation.
    pub fn confirm_duplicate_removal(&mut self, id: DocumentId) -> Result<String, AppMessage> {
        let (destination, options, encoding, output_index) = {
            let Some(slot) = self.tabs.get(&id) else {
                return Err(AppMessage::warning("Open a file first."));
            };
            let Some(doc) = slot.tab.document.as_ref() else {
                return Err(AppMessage::warning("Open a file first."));
            };
            let Some(StructuralJob::DuplicatePreview { summary, .. }) = &doc.structural_job else {
                return Err(AppMessage::warning(
                    "No duplicate preview is awaiting confirmation.",
                ));
            };
            (
                summary.destination.clone(),
                doc.current_open_options(),
                doc.session.encoding(),
                summary.output_index.clone(),
            )
        };
        {
            let doc = self
                .tabs
                .get_mut(&id)
                .and_then(|slot| slot.tab.document.as_mut())
                .ok_or_else(|| AppMessage::warning("Open a file first."))?;
            if doc.session.ensure_source_unchanged().is_err()
                || doc
                    .original_session
                    .as_ref()
                    .is_some_and(|source| source.ensure_source_unchanged().is_err())
            {
                doc.cancel_structural_edit();
                doc.invalidate_changed_source();
                let _ = self.sync_fd_leases();
                return Err(AppMessage::warning(SOURCE_CHANGED_NOTICE));
            }
        }
        let temp_open_fd = self
            .fd_budget
            .try_reserve(FdCategory::ActiveDocument, 1)
            .map_err(|error| AppMessage::warning(error.to_string()))?;
        let index_job_fd = if output_index.is_none() {
            Some(
                self.fd_budget
                    .try_reserve(FdCategory::BackgroundJob, 1)
                    .map_err(|error| AppMessage::warning(error.to_string()))?,
            )
        } else {
            None
        };
        let replacement = self
            .prepare_materialized_document(&destination, options, encoding, output_index)
            .map_err(AppMessage::error)?;
        let Some(StructuralJob::DuplicatePreview {
            summary,
            selected_columns,
            undo,
        }) = self
            .tabs
            .get_mut(&id)
            .and_then(|slot| slot.tab.document.as_mut())
            .and_then(|doc| doc.structural_job.take())
        else {
            return Err(AppMessage::warning(
                "No duplicate preview is awaiting confirmation.",
            ));
        };
        let notice = format!(
            "Removed {} extra duplicate row{}. Save to keep the change, or discard changes.",
            summary.duplicate_rows,
            if summary.duplicate_rows == 1 { "" } else { "s" }
        );
        self.install_prepared_working_copy(
            id,
            replacement,
            MaterializedWorkingCopy {
                path: summary.destination,
                output_index: None,
                undo,
                selected_columns,
                notice: notice.clone(),
                bookmark_transform: BookmarkTransform::ClearRowOrderChanged,
            },
            index_job_fd,
        );
        let final_notice = self
            .tabs
            .get(&id)
            .and_then(|slot| slot.tab.footer_status.as_ref())
            .map(|msg| msg.text.clone())
            .unwrap_or(notice);
        drop(temp_open_fd);
        let _ = self.sync_fd_leases();
        Ok(final_notice)
    }

    /// Cancels a pending `DuplicatePreview` or active structural job on tab `id`.
    pub fn cancel_structural_edit(&mut self, id: DocumentId) {
        if let Some(doc) = self
            .tabs
            .get_mut(&id)
            .and_then(|slot| slot.tab.document.as_mut())
        {
            doc.cancel_structural_edit();
        }
        let _ = self.sync_fd_leases();
    }

    /// Confirms a deferred split materialization whose temporary storage estimate exceeded the
    /// storage-review threshold.
    pub fn confirm_pending_materialization(&mut self, id: DocumentId) -> Result<(), AppMessage> {
        {
            let Some(doc) = self
                .tabs
                .get(&id)
                .and_then(|slot| slot.tab.document.as_ref())
            else {
                return Err(AppMessage::warning("Open a file first."));
            };
            if doc.pending_materialization.is_none() {
                return Ok(());
            }
            if let Some(reason) = doc.pending_materialization_confirm_disabled_reason() {
                return Err(AppMessage::warning(reason));
            }
        }
        self.with_reserved_job_fds(
            id,
            2,
            true,
            |doc| {
                let Some((transformation, selected_columns, records)) =
                    doc.pending_materialization.take()
                else {
                    return Ok(());
                };
                let retry_backup = (transformation.clone(), selected_columns.clone(), records);
                match doc.begin_materialization(transformation, selected_columns) {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        // Restore the pending split request on retryable failures (for example,
                        // insufficient scratch space); leave cleared when the source changed and
                        // `begin_materialization` invalidated the document.
                        if !doc.source_changed {
                            doc.pending_materialization = Some(retry_backup);
                        }
                        Err(error)
                    }
                }
            },
            |err| AppMessage::warning(err.to_string()),
        )
    }

    /// Dismisses a deferred split materialization without modifying the document.
    pub fn cancel_pending_materialization(&mut self, id: DocumentId) {
        if let Some(doc) = self
            .tabs
            .get_mut(&id)
            .and_then(|slot| slot.tab.document.as_mut())
        {
            doc.pending_materialization = None;
            doc.structural_status = Some("Split cancelled. The document was not changed.".into());
        }
    }

    pub fn working_directory(&self) -> &Path {
        &self.config.working_directory
    }

    pub fn custom_working_directory(&self) -> bool {
        self.config.custom_working_directory
    }

    /// Updates the scratch working directory on the scheduler and every hydrated tab.
    pub fn set_working_directory(&mut self, directory: PathBuf, custom: bool) {
        self.config.working_directory = directory.clone();
        self.config.custom_working_directory = custom;
        for slot in self.tabs.values_mut() {
            if let Some(doc) = slot.tab.document.as_mut() {
                doc.working_directory = directory.clone();
                doc.custom_working_directory = custom;
            }
        }
    }

    pub fn index_cache_enabled(&self) -> bool {
        self.config.index_cache_enabled
    }

    /// Applies the persistent sidecar index cache toggle across the scheduler and hydrated tabs.
    pub fn set_index_cache_enabled(&mut self, enabled: bool) {
        self.config.index_cache_enabled = enabled;
        for slot in self.tabs.values_mut() {
            if let Some(doc) = slot.tab.document.as_mut() {
                doc.index_cache_enabled = enabled;
            }
        }
    }

    pub fn working_cleanup(&self) -> WorkingCopyCleanup {
        self.working_cleanup.clone()
    }

    /// Lists all retained working-copy and completed private decompressed `.gz` files across
    /// hydrated and dormant tabs for storage review, deduplicating shared paths.
    pub fn retained_storage_paths(&self) -> Vec<PathBuf> {
        let mut paths = BTreeSet::new();
        for slot in self.tabs.values() {
            if let Some(doc) = slot.tab.document.as_ref() {
                paths.extend(doc.retained_storage_paths());
                if doc.compressed_save_as_ready() && !doc.session.path().as_os_str().is_empty() {
                    paths.insert(doc.session.path().to_path_buf());
                }
            } else if let SlotResidency::Dormant(dormant) = &slot.residency {
                match &dormant.kind {
                    DormantKind::Dirty(dirty) => {
                        if let Some(wc) = dirty.working_copy.as_ref() {
                            if wc.owns(dirty.session.path()) {
                                paths.insert(dirty.session.path().to_path_buf());
                            }
                            if let Some(u) = wc.undo.as_ref()
                                && wc.owns(&u.path)
                            {
                                paths.insert(u.path.clone());
                            }
                            if let Some(r) = wc.redo.as_ref()
                                && wc.owns(&r.path)
                            {
                                paths.insert(r.path.clone());
                            }
                        }
                    }
                    DormantKind::CompressedComplete { session, .. } => {
                        if !session.path().as_os_str().is_empty() {
                            paths.insert(session.path().to_path_buf());
                        }
                    }
                    DormantKind::CleanPlain { .. } | DormantKind::CompressedPreview { .. } => {}
                }
            }
        }
        paths.into_iter().collect()
    }

    /// Starts background decompression for a `.gz` tab paused in `Phase::Pending`, enforcing
    /// the write-slot cap and pre-reserving its decompression job and private-copy FDs.
    pub fn start_pending_decompression(
        &mut self,
        id: DocumentId,
        extra_write_slot_available: bool,
    ) -> Result<bool, String> {
        if !extra_write_slot_available || !self.write_slot_available() {
            return Err(compressed::WRITE_SLOTS_FULL.to_owned());
        }
        self.with_reserved_job_fds(
            id,
            4,
            false,
            |doc| doc.start_decompression(),
            |err| err.to_string(),
        )
    }

    /// Transitions a `.gz` tab paused in `Phase::Pending` directly into bounded `Phase::Preview`.
    pub fn stay_in_compressed_preview(&mut self, id: DocumentId) -> Result<bool, String> {
        let Some(doc) = self
            .tabs
            .get_mut(&id)
            .and_then(|slot| slot.tab.document.as_mut())
        else {
            return Err("Open a file first.".into());
        };
        let result = doc.stay_in_compressed_preview();
        let _ = self.sync_fd_leases();
        result
    }

    /// Cancels an active `.gz` decompression pass on tab `id`, removing its private copy and
    /// reconciling FD leases.
    pub fn cancel_decompression(&mut self, id: DocumentId) -> Option<Result<&'static str, String>> {
        let result = self
            .tabs
            .get_mut(&id)
            .and_then(|slot| slot.tab.document.as_mut())
            .and_then(Document::cancel_decompression);
        let _ = self.sync_fd_leases();
        result
    }

    /// Lists hydrated non-compressed whole-file tabs eligible as open-document inputs to Join Files.
    pub fn hydrated_join_choices(&self) -> Vec<(DocumentId, String, PathBuf)> {
        let mut choices = Vec::new();
        for (&id, slot) in &self.tabs {
            if let Some(doc) = slot.tab.document.as_ref()
                && !doc.is_compressed()
                && !doc.is_section()
            {
                let label = doc
                    .logical_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("Untitled")
                    .to_owned();
                choices.push((id, label, doc.logical_path.clone()));
            }
        }
        choices
    }

    fn assign_tab_to_window_list(&mut self, target_window: WindowId, id: DocumentId) {
        for (&win, ids) in &mut self.window_tabs {
            if win != target_window {
                ids.retain(|&entry| entry != id);
            }
        }
        let list = self.window_tabs.entry(target_window).or_default();
        if !list.contains(&id) {
            list.push(id);
        }
    }

    fn remove_tab_from_window_lists(&mut self, id: DocumentId) {
        for ids in self.window_tabs.values_mut() {
            ids.retain(|&entry| entry != id);
        }
    }

    fn ensure_hydration_capacity(
        &mut self,
        activating_window: Option<WindowId>,
        replacing_active: Option<DocumentId>,
    ) -> Result<Vec<(WindowId, DocumentId, bool)>, SchedulerError> {
        let mut evicted: Vec<(WindowId, DocumentId, bool)> = Vec::new();
        // Account for a potentially new window entry so its active slot stays guaranteed.
        let projected_windows = match activating_window {
            Some(window) if !self.active_by_window.contains_key(&window) => {
                self.active_by_window.len() + 1
            }
            _ => self.active_by_window.len().max(1),
        };
        let limit = self.config.max_hydrated_tabs.max(projected_windows);

        while self.hydrated_count() >= limit {
            let candidates = self.eligible_eviction_candidates(replacing_active);
            if candidates.is_empty() {
                let reasons = self
                    .tabs
                    .keys()
                    .filter_map(|&id| self.pin_reason(id).map(|reason| (id, reason)))
                    .collect();
                let hydrated = self.hydrated_count();
                self.restore_evicted_on_failure(&evicted);
                return Err(SchedulerError::AllTabsPinned {
                    hydrated,
                    limit,
                    reasons,
                });
            }
            let mut evicted_one = false;
            let mut last_error = None;
            for victim_id in candidates {
                let victim_window = self.tabs.get(&victim_id).map(|s| s.window).unwrap();
                let was_active = self.active_by_window.get(&victim_window) == Some(&victim_id);
                match self.evict_tab_unchecked(victim_id) {
                    Ok(()) => {
                        evicted.push((victim_window, victim_id, was_active));
                        evicted_one = true;
                        break;
                    }
                    Err(error) => {
                        last_error = Some(error);
                    }
                }
            }
            if !evicted_one {
                self.restore_evicted_on_failure(&evicted);
                return Err(last_error.unwrap());
            }
        }
        Ok(evicted)
    }

    fn eligible_eviction_candidates(
        &self,
        replacing_active: Option<DocumentId>,
    ) -> Vec<DocumentId> {
        let mut candidates: Vec<(bool, u64, DocumentId)> = Vec::new();
        for (&id, slot) in &self.tabs {
            if !matches!(slot.residency, SlotResidency::Hydrated { .. }) {
                continue;
            }
            let pin = self.pin_reason(id);
            let evictable = match pin {
                None => true,
                // The tab being replaced as the current window's active tab is eligible if
                // it has no other pin (no write job, decompression, join reserve, or close modal).
                Some(PinReason::ActiveInWindow(_)) if Some(id) == replacing_active => {
                    !self.has_non_active_pin(slot)
                }
                Some(_) => false,
            };
            if evictable {
                // Prefer evicting clean tabs before dirty tabs, then LRU tick.
                let is_dirty = slot.tab.is_dirty();
                candidates.push((is_dirty, slot.last_used_tick, id));
            }
        }
        candidates.sort_by_key(|&(is_dirty, tick, id)| (is_dirty, tick, id));
        candidates.into_iter().map(|(_, _, id)| id).collect()
    }

    fn has_non_active_pin(&self, slot: &ManagedTab) -> bool {
        if slot.tab.close_confirmation_open
            || slot.tab.close_after_save
            || slot.tab.close_after_jobs
        {
            return true;
        }
        let Some(doc) = slot.tab.document.as_ref() else {
            return false;
        };
        if doc.join_reserved()
            || doc.is_indexing_pause_pinned()
            || doc.is_live_watching()
            || slot.tab.document_writing()
            || doc.pending_materialization.is_some()
            || matches!(
                doc.structural_job,
                Some(crate::StructuralJob::DuplicatePreview { .. })
            )
        {
            return true;
        }
        doc.compressed.as_ref().is_some_and(|compressed| {
            matches!(
                compressed.dormancy_policy(),
                CompressedDormancyPolicy::PinnedPendingStorage
                    | CompressedDormancyPolicy::PinnedDecompressing
            )
        })
    }

    fn evict_tab_unchecked(&mut self, id: DocumentId) -> Result<(), SchedulerError> {
        let total_overlay_before = self.total_dirty_overlay_bytes();
        let Some(slot) = self.tabs.get_mut(&id) else {
            return Err(SchedulerError::NotFound(id));
        };
        if !matches!(slot.residency, SlotResidency::Hydrated { .. }) {
            return Ok(());
        }
        let Some(doc) = slot.tab.document.as_ref() else {
            return Ok(());
        };

        // Enforce explicit sparse overlay cap (including pending inline drafts and history)
        // and scratch disk-space headroom for dirty tabs BEFORE mutating or dropping anything,
        // so failure preserves every open document and its edits.
        let has_pending_draft = doc.cell_edit.is_some() || doc.header_edit.is_some();
        if doc.is_dirty() || has_pending_draft || doc.edit_history.bytes > 0 {
            let doc_overlay = document_overlay_bytes(doc);
            if total_overlay_before > self.config.max_dirty_overlay_bytes
                || doc_overlay > self.config.max_dirty_overlay_bytes
            {
                return Err(SchedulerError::OverlayBudgetExceeded {
                    used_bytes: total_overlay_before.max(doc_overlay),
                    cap_bytes: self.config.max_dirty_overlay_bytes,
                });
            }
        }
        if doc.is_dirty() || has_pending_draft {
            let available = match self.config.simulated_scratch_available_bytes {
                Some(bytes) => bytes,
                None => inspect_storage(&doc.working_directory)
                    .map(|space| space.available_bytes)
                    .unwrap_or(0),
            };
            if available < MIN_DORMANT_SCRATCH_HEADROOM_BYTES {
                return Err(SchedulerError::ScratchDiskFull {
                    required_bytes: MIN_DORMANT_SCRATCH_HEADROOM_BYTES,
                    available_bytes: available,
                });
            }
        }

        // Synchronize persistent directory FD leases for any live WorkingCopyStorage or
        // completed compressed SpillWorkspace BEFORE demoting so dormant directories cannot
        // exceed the < 128 FD cap and any preflight error leaves the tab untouched.
        let retained_dir_fds = doc_retained_dir_fds(doc);
        let fd_budget = self.fd_budget.clone();
        let (working_dir_fd_lease, pending_filter_restore) = match &mut slot.residency {
            SlotResidency::Hydrated {
                temp_dir_fd_lease,
                pending_filter_restore,
                ..
            } => {
                try_sync_optional_lease(
                    &fd_budget,
                    temp_dir_fd_lease,
                    FdCategory::TempDirectory,
                    retained_dir_fds,
                )?;
                (temp_dir_fd_lease.take(), pending_filter_restore.take())
            }
            _ => (None, None),
        };

        let mut doc = slot.tab.document.take().unwrap();
        if let Some(pending) = pending_filter_restore
            && doc.filter_query.is_some()
        {
            pending.apply_to(&mut doc);
        }
        let dormant = demote_document(doc_into_dormant_parts(&mut doc), doc, working_dir_fd_lease)?;
        // Replacing `Hydrated { _fd_lease, job_fd_lease, .. }` drops active & job FD leases.
        slot.residency = SlotResidency::Dormant(Box::new(dormant));
        if self.active_by_window.get(&slot.window) == Some(&id) {
            self.active_by_window.remove(&slot.window);
        }
        Ok(())
    }

    fn allocate_id(&mut self) -> DocumentId {
        let id = DocumentId::new(self.next_document_id);
        self.next_document_id = self.next_document_id.saturating_add(1);
        id
    }

    fn next_tick(&mut self) -> u64 {
        self.access_clock = self.access_clock.saturating_add(1);
        self.access_clock
    }
}

fn slot_is_dirty(slot: &ManagedTab) -> bool {
    match &slot.residency {
        SlotResidency::Hydrated { .. } => slot.tab.is_dirty(),
        SlotResidency::Dormant(dormant) => match &dormant.kind {
            DormantKind::Dirty(dirty) => {
                let reading_working_copy =
                    !dirty.session.is_compressed() && dirty.session.path() != dormant.logical_path;
                reading_working_copy
                    || !dirty.cell_edits.is_empty()
                    || !dirty.header_renames.is_empty()
            }
            _ => false,
        },
        SlotResidency::Stub(_) => false,
    }
}

/// Computes a residency-independent fingerprint of a tab's unsaved edit state so quit approvals
/// survive normal hydration <-> dormancy eviction while still invalidating on any new edit.
fn slot_edit_revision(slot: &ManagedTab) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    match &slot.residency {
        SlotResidency::Hydrated { .. } => {
            if let Some(doc) = slot.tab.document.as_ref()
                && doc.is_dirty()
            {
                1u8.hash(&mut hasher);
                doc.session.path().hash(&mut hasher);
                doc.original_session.is_some().hash(&mut hasher);
                doc.cell_edits.len().hash(&mut hasher);
                for (coord, value) in doc.cell_edits.iter() {
                    coord.hash(&mut hasher);
                    value.as_ref().hash(&mut hasher);
                }
                doc.header_renames.len().hash(&mut hasher);
                for (col, name) in doc.header_renames.iter() {
                    col.hash(&mut hasher);
                    name.as_str().hash(&mut hasher);
                }
                if let Some(edit) = doc.cell_edit.as_ref() {
                    1u8.hash(&mut hasher);
                    edit.row.hash(&mut hasher);
                    edit.column.hash(&mut hasher);
                    edit.draft.hash(&mut hasher);
                }
                if let Some(edit) = doc.header_edit.as_ref() {
                    2u8.hash(&mut hasher);
                    edit.column.hash(&mut hasher);
                    edit.draft.hash(&mut hasher);
                }
            } else {
                0u8.hash(&mut hasher);
            }
        }
        SlotResidency::Dormant(dormant) => {
            if let DormantKind::Dirty(dirty) = &dormant.kind
                && slot_is_dirty(slot)
            {
                1u8.hash(&mut hasher);
                dirty.session.path().hash(&mut hasher);
                dirty.original_session.is_some().hash(&mut hasher);
                dirty.cell_edits.len().hash(&mut hasher);
                for (coord, value) in dirty.cell_edits.iter() {
                    coord.hash(&mut hasher);
                    value.as_ref().hash(&mut hasher);
                }
                dirty.header_renames.len().hash(&mut hasher);
                for (col, name) in dirty.header_renames.iter() {
                    col.hash(&mut hasher);
                    name.as_str().hash(&mut hasher);
                }
            } else {
                0u8.hash(&mut hasher);
            }
        }
        SlotResidency::Stub(_) => {
            0u8.hash(&mut hasher);
        }
    }
    hasher.finish()
}

fn doc_active_document_fds(doc: &Document) -> usize {
    1 + usize::from(doc.original_session.is_some())
}

fn doc_retained_dir_fds(doc: &Document) -> usize {
    doc.working_copy
        .as_ref()
        .map_or(0, |wc| 1 + wc.previous_directories.len())
        + usize::from(doc.compressed.as_ref().is_some_and(|compressed| {
            compressed.dormancy_policy() == CompressedDormancyPolicy::DormantKeepPrivateCopy
        }))
}

/// Filter scan opens source + cloned guard handle (2 FDs) and may concurrently spawn a
/// filtered-read worker (1 FD) while scanning, so a running filter reserves 3 FDs upfront.
const FILTER_JOB_AND_READ_FDS: usize = 3;

fn doc_non_filter_job_fds(doc: &Document) -> usize {
    // Index holds 1 FD; search holds 2 FDs (source + cloned source-stamp guard handle);
    // filtered export, save, and running structural workers each hold up to 2 FDs
    // (source + destination); gzip decompression holds up to 4 FDs; DuplicatePreview holds no
    // running worker FDs.
    let save_fds = if doc.save_job.is_some() {
        if doc.saving_in_place && doc.is_section() {
            5
        } else {
            2
        }
    } else {
        0
    };
    usize::from(doc.job.is_some())
        + usize::from(doc.bookmark_offset_job.is_some())
        + usize::from(doc.section_verify_job.is_some())
        + doc.live_watch_fd_count()
        + usize::from(doc.search_job.is_some()) * 2
        + usize::from(doc.export_job.is_some()) * 2
        + save_fds
        + usize::from(doc.decompression_running()) * 4
        + usize::from(
            doc.structural_job
                .as_ref()
                .is_some_and(|job| !matches!(job, StructuralJob::DuplicatePreview { .. })),
        ) * 2
}

fn doc_active_job_fds(doc: &Document) -> usize {
    doc_non_filter_job_fds(doc)
        + if doc.filter_job.is_some() {
            FILTER_JOB_AND_READ_FDS
        } else {
            usize::from(doc.filter_read.is_some())
        }
}

fn try_sync_optional_lease(
    ledger: &FdBudgetLedger,
    lease: &mut Option<FdLease>,
    category: FdCategory,
    needed: usize,
) -> Result<(), SchedulerError> {
    if needed == 0 {
        *lease = None;
        return Ok(());
    }
    if let Some(existing) = lease.as_mut() {
        existing.try_resize(needed)?;
    } else {
        *lease = Some(ledger.try_reserve(category, needed)?);
    }
    Ok(())
}

/// Resolves an existing path via `canonicalize`, or resolves existing parent directories
/// while lexically collapsing `.` and `..` components for not-yet-created output paths.
fn normalize_path_with_existing_ancestor(path: &Path) -> PathBuf {
    if let Ok(canon) = std::fs::canonicalize(path) {
        return canon;
    }
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(path)
    } else {
        path.to_path_buf()
    };
    let mut resolved = PathBuf::new();
    for component in abs.components() {
        match component {
            std::path::Component::Prefix(prefix) => resolved.push(prefix.as_os_str()),
            std::path::Component::RootDir => resolved.push(component.as_os_str()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if let Ok(canon) = std::fs::canonicalize(&resolved) {
                    resolved = canon;
                }
                resolved.pop();
            }
            std::path::Component::Normal(part) => {
                resolved.push(part);
                if let Ok(canon) = std::fs::canonicalize(&resolved) {
                    resolved = canon;
                }
            }
        }
    }
    resolved
}

fn canonicalize_best_effort(path: &Path) -> PathBuf {
    normalize_path_with_existing_ancestor(path)
}

fn paths_alias(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    if normalize_path_with_existing_ancestor(left) == normalize_path_with_existing_ancestor(right) {
        return true;
    }
    if let (Ok(Some(a)), Ok(Some(b))) = (
        FileIdentity::from_path(left),
        FileIdentity::from_path(right),
    ) {
        return a == b;
    }
    false
}

fn slot_is_section(slot: &ManagedTab) -> bool {
    match &slot.residency {
        SlotResidency::Hydrated { .. } => {
            slot.tab.document.as_ref().is_some_and(Document::is_section)
        }
        SlotResidency::Dormant(dormant) => match &dormant.kind {
            DormantKind::CleanPlain { section, .. } => section.is_some(),
            DormantKind::Dirty(dirty) => dirty.session.is_section(),
            DormantKind::CompressedComplete { .. } | DormantKind::CompressedPreview { .. } => false,
        },
        SlotResidency::Stub(_) => false,
    }
}

fn tab_matches_path(
    slot: &ManagedTab,
    path: &Path,
    canonical: &Path,
    identity: Option<FileIdentity>,
) -> bool {
    match &slot.residency {
        // Compare already-captured metadata and exact logical paths without reopening
        // every stub on disk during O(N) bulk lazy registration.
        SlotResidency::Stub(stub) => {
            path == stub.logical_path
                || stub.canonical_path == canonical
                || (identity.is_some() && identity == stub.file_identity)
        }
        SlotResidency::Dormant(dormant) => {
            path == dormant.logical_path
                || dormant.canonical_path == canonical
                || (identity.is_some() && identity == dormant.file_identity)
                || match &dormant.kind {
                    DormantKind::Dirty(dirty) => {
                        path == dirty.session.path()
                            || (identity.is_some() && identity == dirty.session.file_identity())
                            || (!dirty.session.path().as_os_str().is_empty()
                                && canonical
                                    == normalize_path_with_existing_ancestor(dirty.session.path()))
                    }
                    DormantKind::CompressedComplete { session, .. } => {
                        path == session.path()
                            || (identity.is_some()
                                && (identity == session.file_identity()
                                    || identity == session.compressed_source_file_identity()))
                            || (!session.path().as_os_str().is_empty()
                                && canonical
                                    == normalize_path_with_existing_ancestor(session.path()))
                    }
                    DormantKind::CompressedPreview { session, .. } => {
                        identity.is_some()
                            && (identity == session.file_identity()
                                || identity == session.compressed_source_file_identity())
                    }
                    _ => false,
                }
        }
        SlotResidency::Hydrated { .. } => slot.tab.document.as_ref().is_some_and(|doc| {
            path == doc.logical_path
                || (identity.is_some()
                    && (identity == doc.session.file_identity()
                        || identity == doc.session.compressed_source_file_identity()))
                || (identity.is_some()
                    && doc
                        .original_session
                        .as_ref()
                        .and_then(Session::file_identity)
                        == identity)
                || canonical == normalize_path_with_existing_ancestor(&doc.logical_path)
                || (!doc.session.path().as_os_str().is_empty()
                    && canonical == normalize_path_with_existing_ancestor(doc.session.path()))
        }),
    }
}

fn working_copy_reserves_path(wc: &WorkingCopyState, path: &Path) -> bool {
    if wc.owns(path) {
        return true;
    }
    for storage in std::iter::once(&wc.directory).chain(wc.previous_directories.iter()) {
        if path.starts_with(storage.path()) || paths_alias(path, storage.path()) {
            return true;
        }
        if let Ok(current) = storage.current_path()
            && (path.starts_with(&current) || paths_alias(path, &current))
        {
            return true;
        }
    }
    wc.undo
        .iter()
        .chain(wc.redo.iter())
        .any(|snapshot| paths_alias(path, &snapshot.path))
}

/// Computes tracked sparse-overlay memory bytes for a hydrated document, including
/// any uncommitted inline cell or header edit drafts, value edit history, and retained
/// structural undo/redo snapshots.
pub fn document_overlay_bytes(doc: &Document) -> usize {
    let cell_bytes: usize = doc.cell_edits.values().map(|value| value.len() + 32).sum();
    let header_bytes: usize = doc
        .header_renames
        .values()
        .map(|name| name.len() + 24)
        .sum();
    let pending_cell_bytes = doc.cell_edit.as_ref().map_or(0, |edit| {
        // Account for both the committed cell_edits entry and the recorded ValueEdit history entry.
        edit.draft
            .len()
            .saturating_mul(2)
            .saturating_add(edit.source.len())
            .saturating_add(32)
    });
    let pending_header_bytes = doc.header_edit.as_ref().map_or(0, |edit| {
        edit.draft.len().saturating_mul(2).saturating_add(24)
    });
    let snapshot_bytes: usize = doc.working_copy.as_ref().map_or(0, |wc| {
        wc.undo
            .iter()
            .chain(wc.redo.iter())
            .map(|snapshot| {
                let cells: usize = snapshot
                    .overlay
                    .cell_edits
                    .values()
                    .map(|value| value.len() + 32)
                    .sum();
                let headers: usize = snapshot
                    .overlay
                    .header_renames
                    .values()
                    .map(|name| name.len() + 24)
                    .sum();
                cells
                    .saturating_add(headers)
                    .saturating_add(snapshot.edit_history.bytes)
            })
            .sum()
    });
    cell_bytes
        .saturating_add(header_bytes)
        .saturating_add(pending_cell_bytes)
        .saturating_add(pending_header_bytes)
        .saturating_add(snapshot_bytes)
        .saturating_add(doc.edit_history.bytes)
}

struct DormantPrep {
    open_options: OpenOptions,
    encoding: Option<Encoding>,
    source_guard: SourceGuard,
    file_identity: Option<FileIdentity>,
    canonical_path: PathBuf,
    overlay_bytes: usize,
}

fn doc_into_dormant_parts(doc: &mut Document) -> Result<DormantPrep, SchedulerError> {
    // Commit any active inline edit draft so dormancy never drops typed characters.
    doc.commit_edits();
    // Cancel active background read workers cleanly before eviction.
    if let Some(job) = doc.job.take() {
        job.cancel();
        let _ = job.wait();
    }
    doc.cancel_search();
    if let Some(job) = doc.search_job.take() {
        job.cancel();
        let _ = job.wait();
    }
    if let Some(job) = doc.filter_job.take() {
        job.cancel();
        let _ = job.wait();
    }
    if let Some(active_read) = doc.filter_read.take() {
        active_read.job.cancel();
        let _ = active_read.job.wait();
    }
    doc.pending_filter_read = None;

    let open_options = doc.current_open_options();
    let encoding = Some(doc.session.encoding());
    let canonical_path = canonicalize_best_effort(&doc.logical_path);
    let source_guard = if doc.compressed.is_some() {
        if doc.session.ensure_compressed_source_unchanged().is_err() {
            doc.source_changed = true;
        }
        SourceGuard::capture_path(&doc.logical_path).unwrap_or_else(|_| doc.session.source_guard())
    } else if let Some(original) = doc.original_session.as_ref() {
        original.source_guard()
    } else {
        doc.session.source_guard()
    };
    let file_identity = source_guard
        .file_identity()
        .or_else(|| doc.session.file_identity());
    let overlay_bytes = document_overlay_bytes(doc);

    Ok(DormantPrep {
        open_options,
        encoding,
        source_guard,
        file_identity,
        canonical_path,
        overlay_bytes,
    })
}

fn demote_document(
    prep: Result<DormantPrep, SchedulerError>,
    mut doc: Document,
    working_dir_fd_lease: Option<FdLease>,
) -> Result<DormantDocument, SchedulerError> {
    let prep = prep?;
    let source_changed = doc.source_changed;
    // Evict heavy index, row buffers, filter index, and whitespace cache.
    doc.index = None;
    doc.buffered_rows.clear();
    doc.filtered_rows.clear();
    doc.filter_index = None;
    doc.whitespace = crate::whitespace::Cache::default();

    let view = DormantViewSnapshot {
        viewport_start: doc.viewport_start,
        scroll_points: doc.scroll_points,
        visible_rows: doc.visible_rows,
        data_start: doc.data_start,
        selection: doc.selection,
        cursor: doc.cursor.clone(),
        selected_rows: doc.selected_rows.clone(),
        selected_columns: doc.selected_columns.clone(),
        column_selection_anchor: doc.column_selection_anchor,
        columns: doc.columns.clone(),
        headers: doc.headers.clone(),
        total_columns: doc.total_columns,
        filter_query: doc.filter_query.clone(),
        filter_viewport_start: doc.filter_viewport_start,
        edit_history: std::mem::take(&mut doc.edit_history),
        source_generation: doc.source_generation,
        bookmarks: std::mem::take(&mut doc.bookmarks),
    };

    let logical_path = doc.logical_path.clone();
    let kind = if let Some(compressed_doc) = doc.compressed.take() {
        match compressed_doc.dormancy_policy() {
            CompressedDormancyPolicy::DormantKeepPrivateCopy => DormantKind::CompressedComplete {
                session: Arc::clone(&doc.session),
                compressed: compressed_doc,
                _workspace_fd_lease: working_dir_fd_lease,
            },
            CompressedDormancyPolicy::DormantPreviewOnly => DormantKind::CompressedPreview {
                session: Arc::clone(&doc.session),
                compressed: compressed_doc,
            },
            CompressedDormancyPolicy::PinnedPendingStorage
            | CompressedDormancyPolicy::PinnedDecompressing => {
                unreachable!("pinned compressed tabs are never demoted");
            }
        }
    } else if doc.is_dirty() || doc.working_copy.is_some() {
        DormantKind::Dirty(Box::new(DormantDirtyState {
            session: Arc::clone(&doc.session),
            original_session: doc.original_session.take(),
            working_copy: doc.working_copy.take(),
            header_renames: Arc::clone(&doc.header_renames),
            cell_edits: Arc::clone(&doc.cell_edits),
            overlay_bytes: prep.overlay_bytes,
            verified_bounds_override: doc.verified_bounds_override,
            pending_verified_reopen: doc.pending_verified_reopen,
            section_quote_free_confirmed: doc.section_quote_free_confirmed,
            _working_dir_fd_lease: working_dir_fd_lease,
        }))
    } else {
        // Clean plain document: drop `Document` (including `Session`, `StructuralIndex`, buffers).
        let section = doc.section_bounds().map(|bounds| DormantSectionSpec {
            bounds,
            quote_free_confirmed: doc.section_quote_free_confirmed,
            delimiter: doc.session.dialect.delimiter,
            encoding: doc.session.encoding(),
        });
        doc.shutdown();
        drop(doc);
        DormantKind::CleanPlain {
            open_options: prep.open_options,
            encoding: prep.encoding,
            section,
        }
    };

    Ok(DormantDocument {
        logical_path,
        canonical_path: prep.canonical_path,
        file_identity: prep.file_identity,
        source_guard: prep.source_guard,
        source_changed,
        view,
        kind,
    })
}

fn open_failure_to_scheduler_error(failure: compressed::OpenFailure) -> SchedulerError {
    if failure.inspectable {
        SchedulerError::OpenFailed(failure.message)
    } else {
        SchedulerError::OpenFailedUninspectable(failure.message)
    }
}

fn hydrate_from_stub(
    stub: &LazyDocumentStub,
    config: &WorkbenchSchedulerConfig,
    working_cleanup: &WorkingCopyCleanup,
    decompression_slots: &compressed::DecompressionSlots,
    fd_budget: &FdBudgetLedger,
    write_slot_available: bool,
) -> Result<(Document, Option<FdLease>, Option<FdLease>), SchedulerError> {
    let mut doc = Document::prepare_checked(&stub.logical_path, stub.open_options, stub.encoding)
        .map_err(open_failure_to_scheduler_error)?;
    doc.working_directory = config.working_directory.clone();
    doc.custom_working_directory = config.custom_working_directory;
    doc.index_cache_enabled = config.index_cache_enabled;
    doc.working_cleanup = working_cleanup.clone();
    doc.decompression_slots = decompression_slots.clone();
    let mut job_fd_lease = None;
    if doc.awaiting_decompression() {
        if !config.defer_gzip_decompression {
            if !write_slot_available {
                return Err(SchedulerError::OpenFailed(
                    compressed::WRITE_SLOTS_FULL.to_owned(),
                ));
            }
            job_fd_lease = Some(fd_budget.try_reserve(FdCategory::BackgroundJob, 4)?);
            doc.start_indexing().map_err(SchedulerError::OpenFailed)?;
        }
    } else {
        job_fd_lease = Some(fd_budget.try_reserve(FdCategory::BackgroundJob, 1)?);
        doc.start_indexing().map_err(SchedulerError::OpenFailed)?;
    }
    let _ = try_sync_optional_lease(
        fd_budget,
        &mut job_fd_lease,
        FdCategory::BackgroundJob,
        doc_active_job_fds(&doc),
    );
    let mut temp_dir_fd_lease = None;
    let retained_dirs = doc_retained_dir_fds(&doc);
    if retained_dirs > 0 {
        try_sync_optional_lease(
            fd_budget,
            &mut temp_dir_fd_lease,
            FdCategory::TempDirectory,
            retained_dirs,
        )?;
    }
    Ok((doc, temp_dir_fd_lease, job_fd_lease))
}

struct HydratedWake {
    kind: WakeKind,
    conflict_notice: Option<String>,
    temp_dir_lease: Option<FdLease>,
    job_lease: Option<FdLease>,
    pending_filter_restore: Option<PendingFilterRestore>,
}

type WakeResult = Result<
    (
        Document,
        WakeKind,
        Option<String>,
        Option<FdLease>,
        Option<FdLease>,
    ),
    (Box<DormantDocument>, SchedulerError),
>;

fn cancel_woken_read_jobs(doc: &mut Document) {
    if let Some(job) = doc.job.take() {
        job.cancel();
    }
    if let Some(job) = doc.filter_job.take() {
        job.cancel();
    }
}

fn wake_dormant_document(
    dormant: Box<DormantDocument>,
    config: &WorkbenchSchedulerConfig,
    working_cleanup: &WorkingCopyCleanup,
    decompression_slots: &compressed::DecompressionSlots,
    fd_budget: &FdBudgetLedger,
) -> WakeResult {
    let dormant = *dormant;
    let source_unchanged = !dormant.source_changed
        && dormant
            .source_guard
            .matches_path(&dormant.logical_path)
            .unwrap_or(false);

    match dormant.kind {
        DormantKind::CleanPlain {
            open_options,
            encoding,
            section,
        } => {
            let prepare_fresh = |path: &Path| {
                if let Some(spec) = section {
                    Document::prepare_section(
                        path,
                        spec.bounds.requested,
                        SectionOpenOptions {
                            open: open_options,
                            encoding,
                            quote_free_confirmed: spec.quote_free_confirmed,
                            ..SectionOpenOptions::default()
                        },
                    )
                } else {
                    Document::prepare_with_encoding(path, open_options, encoding)
                }
            };
            if !source_unchanged {
                // External file changed or vanished while dormant: never silently reload!
                // Try opening a read-only shell if the file exists, or surface a conflict error.
                let mut doc = match prepare_fresh(&dormant.logical_path) {
                    Ok(doc) => doc,
                    Err(message) => {
                        let restored = Box::new(DormantDocument {
                            logical_path: dormant.logical_path,
                            canonical_path: dormant.canonical_path,
                            file_identity: dormant.file_identity,
                            source_guard: dormant.source_guard,
                            source_changed: dormant.source_changed,
                            view: dormant.view,
                            kind: DormantKind::CleanPlain {
                                open_options,
                                encoding,
                                section,
                            },
                        });
                        return Err((
                            restored,
                            SchedulerError::OpenFailed(format!(
                                "{SOURCE_CHANGED_NOTICE} ({message})"
                            )),
                        ));
                    }
                };
                apply_view_snapshot(
                    &mut doc,
                    &dormant.view,
                    config,
                    working_cleanup,
                    decompression_slots,
                );
                doc.bookmarks.clear();
                doc.source_changed = true;
                return Ok((
                    doc,
                    WakeKind::CleanCold,
                    Some(SOURCE_CHANGED_NOTICE.to_owned()),
                    None,
                    None,
                ));
            }

            let opened = if let Some(spec) = section {
                match Document::prepare_section_preserved(
                    &dormant.logical_path,
                    spec.bounds,
                    &dormant.source_guard,
                    spec.delimiter,
                    spec.encoding,
                    SectionOpenOptions {
                        open: open_options,
                        encoding,
                        quote_free_confirmed: spec.quote_free_confirmed,
                        ..SectionOpenOptions::default()
                    },
                ) {
                    Ok(doc) => Ok(doc),
                    Err(quarry_core::QuarryError::SourceChanged) => {
                        // File changed between matches_path and open: reopen freshly without
                        // reusing the old proof and mark source_changed below.
                        prepare_fresh(&dormant.logical_path)
                    }
                    Err(err) => Err(err.to_string()),
                }
            } else {
                Document::prepare_with_encoding(&dormant.logical_path, open_options, encoding)
            };
            let mut doc = match opened {
                Ok(doc) => doc,
                Err(message) => {
                    let restored = Box::new(DormantDocument {
                        logical_path: dormant.logical_path,
                        canonical_path: dormant.canonical_path,
                        file_identity: dormant.file_identity,
                        source_guard: dormant.source_guard,
                        source_changed: dormant.source_changed,
                        view: dormant.view,
                        kind: DormantKind::CleanPlain {
                            open_options,
                            encoding,
                            section,
                        },
                    });
                    return Err((restored, SchedulerError::OpenFailed(message)));
                }
            };
            // Verify the opened session's actual SourceGuard against the dormant guard so a
            // concurrent replacement between matches_path and prepare_with_encoding cannot slip
            // through silently.
            if doc.session.source_guard() != dormant.source_guard {
                apply_view_snapshot(
                    &mut doc,
                    &dormant.view,
                    config,
                    working_cleanup,
                    decompression_slots,
                );
                doc.bookmarks.clear();
                doc.source_changed = true;
                return Ok((
                    doc,
                    WakeKind::CleanCold,
                    Some(SOURCE_CHANGED_NOTICE.to_owned()),
                    None,
                    None,
                ));
            }

            let needed_job_fds = 1 + if dormant.view.filter_query.is_some() {
                FILTER_JOB_AND_READ_FDS
            } else {
                0
            };
            let mut job_fd_lease =
                match fd_budget.try_reserve(FdCategory::BackgroundJob, needed_job_fds) {
                    Ok(lease) => Some(lease),
                    Err(error) => {
                        let restored = Box::new(DormantDocument {
                            logical_path: dormant.logical_path,
                            canonical_path: dormant.canonical_path,
                            file_identity: dormant.file_identity,
                            source_guard: dormant.source_guard,
                            source_changed: dormant.source_changed,
                            view: dormant.view,
                            kind: DormantKind::CleanPlain {
                                open_options,
                                encoding,
                                section,
                            },
                        });
                        return Err((restored, error));
                    }
                };

            apply_view_snapshot(
                &mut doc,
                &dormant.view,
                config,
                working_cleanup,
                decompression_slots,
            );
            doc.edit_history = dormant.view.edit_history.clone();
            if let Err(message) = doc.start_indexing() {
                let restored = Box::new(DormantDocument {
                    logical_path: dormant.logical_path,
                    canonical_path: dormant.canonical_path,
                    file_identity: dormant.file_identity,
                    source_guard: dormant.source_guard,
                    source_changed: dormant.source_changed,
                    view: dormant.view,
                    kind: DormantKind::CleanPlain {
                        open_options,
                        encoding,
                        section,
                    },
                });
                return Err((restored, SchedulerError::OpenFailed(message)));
            }
            let kind = if let Some(query) = dormant.view.filter_query.clone() {
                if let Err(message) = doc.start_filter(query) {
                    cancel_woken_read_jobs(&mut doc);
                    let restored = Box::new(DormantDocument {
                        logical_path: dormant.logical_path,
                        canonical_path: dormant.canonical_path,
                        file_identity: dormant.file_identity,
                        source_guard: dormant.source_guard,
                        source_changed: dormant.source_changed,
                        view: dormant.view,
                        kind: DormantKind::CleanPlain {
                            open_options,
                            encoding,
                            section,
                        },
                    });
                    return Err((restored, SchedulerError::OpenFailed(message)));
                }
                restore_filtered_view_state(&mut doc, &dormant.view);
                WakeKind::FilteredRebuilding
            } else {
                WakeKind::CleanCold
            };
            let _ = try_sync_optional_lease(
                fd_budget,
                &mut job_fd_lease,
                FdCategory::BackgroundJob,
                doc_active_job_fds(&doc),
            );
            Ok((doc, kind, None, None, job_fd_lease))
        }
        DormantKind::Dirty(dirty) => {
            let DormantDirtyState {
                session,
                original_session,
                working_copy,
                header_renames,
                cell_edits,
                overlay_bytes,
                verified_bounds_override,
                pending_verified_reopen,
                section_quote_free_confirmed,
                _working_dir_fd_lease: working_dir_fd_lease,
            } = *dirty;
            let mut doc = Document::from_arc_session(session, &dormant.logical_path);
            doc.original_session = original_session;
            doc.working_copy = working_copy;
            doc.edit_history = dormant.view.edit_history.clone();
            doc.header_renames = header_renames;
            doc.cell_edits = cell_edits;
            doc.section_quote_free_confirmed = section_quote_free_confirmed;
            if source_unchanged {
                doc.verified_bounds_override = verified_bounds_override;
                doc.pending_verified_reopen = pending_verified_reopen;
            }
            apply_view_snapshot(
                &mut doc,
                &dormant.view,
                config,
                working_cleanup,
                decompression_slots,
            );
            if !source_unchanged {
                doc.source_changed = true;
                return Ok((
                    doc,
                    WakeKind::DirtyRehydrated,
                    Some(SOURCE_CHANGED_NOTICE.to_owned()),
                    working_dir_fd_lease,
                    None,
                ));
            }
            let needed_job_fds = 1 + if dormant.view.filter_query.is_some() {
                FILTER_JOB_AND_READ_FDS
            } else {
                0
            };
            let mut job_fd_lease =
                match fd_budget.try_reserve(FdCategory::BackgroundJob, needed_job_fds) {
                    Ok(lease) => Some(lease),
                    Err(error) => {
                        let restored = Box::new(DormantDocument {
                            logical_path: dormant.logical_path,
                            canonical_path: dormant.canonical_path,
                            file_identity: dormant.file_identity,
                            source_guard: dormant.source_guard,
                            source_changed: dormant.source_changed,
                            view: dormant.view,
                            kind: DormantKind::Dirty(Box::new(DormantDirtyState {
                                session: Arc::clone(&doc.session),
                                original_session: doc.original_session.take(),
                                working_copy: doc.working_copy.take(),
                                header_renames: Arc::clone(&doc.header_renames),
                                cell_edits: Arc::clone(&doc.cell_edits),
                                overlay_bytes,
                                verified_bounds_override,
                                pending_verified_reopen,
                                section_quote_free_confirmed,
                                _working_dir_fd_lease: working_dir_fd_lease,
                            })),
                        });
                        return Err((restored, error));
                    }
                };
            if let Err(message) = doc.start_indexing() {
                let restored = Box::new(DormantDocument {
                    logical_path: dormant.logical_path,
                    canonical_path: dormant.canonical_path,
                    file_identity: dormant.file_identity,
                    source_guard: dormant.source_guard,
                    source_changed: dormant.source_changed,
                    view: dormant.view,
                    kind: DormantKind::Dirty(Box::new(DormantDirtyState {
                        session: Arc::clone(&doc.session),
                        original_session: doc.original_session.take(),
                        working_copy: doc.working_copy.take(),
                        header_renames: Arc::clone(&doc.header_renames),
                        cell_edits: Arc::clone(&doc.cell_edits),
                        overlay_bytes,
                        verified_bounds_override,
                        pending_verified_reopen,
                        section_quote_free_confirmed,
                        _working_dir_fd_lease: working_dir_fd_lease,
                    })),
                });
                return Err((restored, SchedulerError::OpenFailed(message)));
            }
            if let Some(query) = dormant.view.filter_query.clone() {
                if let Err(message) = doc.start_filter(query) {
                    cancel_woken_read_jobs(&mut doc);
                    let restored = Box::new(DormantDocument {
                        logical_path: dormant.logical_path,
                        canonical_path: dormant.canonical_path,
                        file_identity: dormant.file_identity,
                        source_guard: dormant.source_guard,
                        source_changed: dormant.source_changed,
                        view: dormant.view,
                        kind: DormantKind::Dirty(Box::new(DormantDirtyState {
                            session: Arc::clone(&doc.session),
                            original_session: doc.original_session.take(),
                            working_copy: doc.working_copy.take(),
                            header_renames: Arc::clone(&doc.header_renames),
                            cell_edits: Arc::clone(&doc.cell_edits),
                            overlay_bytes,
                            verified_bounds_override,
                            pending_verified_reopen,
                            section_quote_free_confirmed,
                            _working_dir_fd_lease: working_dir_fd_lease,
                        })),
                    });
                    return Err((restored, SchedulerError::OpenFailed(message)));
                }
                restore_filtered_view_state(&mut doc, &dormant.view);
            }
            let _ = try_sync_optional_lease(
                fd_budget,
                &mut job_fd_lease,
                FdCategory::BackgroundJob,
                doc_active_job_fds(&doc),
            );
            Ok((
                doc,
                WakeKind::DirtyRehydrated,
                None,
                working_dir_fd_lease,
                job_fd_lease,
            ))
        }
        DormantKind::CompressedComplete {
            session,
            compressed,
            _workspace_fd_lease: workspace_fd_lease,
        } => {
            let compressed_unchanged =
                source_unchanged && session.ensure_compressed_source_unchanged().is_ok();
            let mut doc = Document::from_arc_session(session, &dormant.logical_path);
            doc.compressed = Some(compressed);
            apply_view_snapshot(
                &mut doc,
                &dormant.view,
                config,
                working_cleanup,
                decompression_slots,
            );
            if !compressed_unchanged {
                doc.source_changed = true;
                return Ok((
                    doc,
                    WakeKind::CompressedCompleteRehydrated,
                    Some(SOURCE_CHANGED_NOTICE.to_owned()),
                    workspace_fd_lease,
                    None,
                ));
            }
            let needed_job_fds = 1 + if dormant.view.filter_query.is_some() {
                FILTER_JOB_AND_READ_FDS
            } else {
                0
            };
            let mut job_fd_lease =
                match fd_budget.try_reserve(FdCategory::BackgroundJob, needed_job_fds) {
                    Ok(lease) => Some(lease),
                    Err(error) => {
                        let restored = Box::new(DormantDocument {
                            logical_path: dormant.logical_path,
                            canonical_path: dormant.canonical_path,
                            file_identity: dormant.file_identity,
                            source_guard: dormant.source_guard,
                            source_changed: dormant.source_changed,
                            view: dormant.view,
                            kind: DormantKind::CompressedComplete {
                                session: Arc::clone(&doc.session),
                                compressed: doc.compressed.take().unwrap(),
                                _workspace_fd_lease: workspace_fd_lease,
                            },
                        });
                        return Err((restored, error));
                    }
                };
            // Re-index the retained private decompressed copy directly (never re-decompresses).
            if let Err(message) = doc.start_indexing() {
                let restored = Box::new(DormantDocument {
                    logical_path: dormant.logical_path,
                    canonical_path: dormant.canonical_path,
                    file_identity: dormant.file_identity,
                    source_guard: dormant.source_guard,
                    source_changed: dormant.source_changed,
                    view: dormant.view,
                    kind: DormantKind::CompressedComplete {
                        session: Arc::clone(&doc.session),
                        compressed: doc.compressed.take().unwrap(),
                        _workspace_fd_lease: workspace_fd_lease,
                    },
                });
                return Err((restored, SchedulerError::OpenFailed(message)));
            }
            if let Some(query) = dormant.view.filter_query.clone() {
                if let Err(message) = doc.start_filter(query) {
                    cancel_woken_read_jobs(&mut doc);
                    let restored = Box::new(DormantDocument {
                        logical_path: dormant.logical_path,
                        canonical_path: dormant.canonical_path,
                        file_identity: dormant.file_identity,
                        source_guard: dormant.source_guard,
                        source_changed: dormant.source_changed,
                        view: dormant.view,
                        kind: DormantKind::CompressedComplete {
                            session: Arc::clone(&doc.session),
                            compressed: doc.compressed.take().unwrap(),
                            _workspace_fd_lease: workspace_fd_lease,
                        },
                    });
                    return Err((restored, SchedulerError::OpenFailed(message)));
                }
                restore_filtered_view_state(&mut doc, &dormant.view);
            }
            let _ = try_sync_optional_lease(
                fd_budget,
                &mut job_fd_lease,
                FdCategory::BackgroundJob,
                doc_active_job_fds(&doc),
            );
            Ok((
                doc,
                WakeKind::CompressedCompleteRehydrated,
                None,
                workspace_fd_lease,
                job_fd_lease,
            ))
        }
        DormantKind::CompressedPreview {
            session,
            compressed,
        } => {
            let mut doc = Document::from_arc_session(session, &dormant.logical_path);
            doc.compressed = Some(compressed);
            apply_view_snapshot(
                &mut doc,
                &dormant.view,
                config,
                working_cleanup,
                decompression_slots,
            );
            doc.progress.done = true;
            doc.progress.cancelled = true;
            if !source_unchanged {
                doc.source_changed = true;
                return Ok((
                    doc,
                    WakeKind::CompressedPreviewRestored,
                    Some(SOURCE_CHANGED_NOTICE.to_owned()),
                    None,
                    None,
                ));
            }
            // Never restart decompression silently on wake for a Preview tab!
            Ok((doc, WakeKind::CompressedPreviewRestored, None, None, None))
        }
    }
}

fn restore_filtered_view_state(doc: &mut Document, view: &DormantViewSnapshot) {
    doc.filter_viewport_start = view.filter_viewport_start;
    doc.scroll_points = view.scroll_points;
    doc.selection = view.selection;
    doc.cursor = view.cursor.clone();
    doc.selected_rows = view.selected_rows.clone();
    doc.selected_columns = view.selected_columns.clone();
    doc.column_selection_anchor = view.column_selection_anchor;
}

fn apply_view_snapshot(
    doc: &mut Document,
    view: &DormantViewSnapshot,
    config: &WorkbenchSchedulerConfig,
    working_cleanup: &WorkingCopyCleanup,
    decompression_slots: &compressed::DecompressionSlots,
) {
    doc.viewport_start = view.viewport_start;
    doc.scroll_points = view.scroll_points;
    doc.visible_rows = view.visible_rows;
    doc.data_start = view.data_start;
    doc.selection = view.selection;
    doc.cursor = view.cursor.clone();
    doc.selected_rows = view.selected_rows.clone();
    doc.selected_columns = view.selected_columns.clone();
    doc.column_selection_anchor = view.column_selection_anchor;
    doc.columns = view.columns.clone();
    doc.headers = view.headers.clone();
    doc.total_columns = view.total_columns;
    doc.source_generation = view.source_generation;
    doc.bookmarks = view.bookmarks.clone();
    doc.working_directory = config.working_directory.clone();
    doc.custom_working_directory = config.custom_working_directory;
    doc.index_cache_enabled = config.index_cache_enabled;
    doc.working_cleanup = working_cleanup.clone();
    doc.decompression_slots = decompression_slots.clone();
    doc.last_poll = Instant::now();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fd_budget_ledger_enforces_strict_cap_and_releases_on_drop() {
        let ledger = FdBudgetLedger::new(MAX_PROCESS_FD_BUDGET);
        let gui = ledger.try_reserve(FdCategory::GuiSystem, 16).unwrap();
        let sidecar = ledger.try_reserve(FdCategory::SidecarCache, 8).unwrap();
        let undo = ledger.try_reserve(FdCategory::UndoRedo, 4).unwrap();
        let temp = ledger.try_reserve(FdCategory::TempDirectory, 4).unwrap();
        let cleanup = ledger.try_reserve(FdCategory::CleanupWorker, 2).unwrap();
        let jobs = ledger.try_reserve(FdCategory::BackgroundJob, 8).unwrap();
        let docs = ledger.try_reserve(FdCategory::ActiveDocument, 85).unwrap();
        let snap = ledger.snapshot();
        assert_eq!(snap.total_reserved, 127);
        assert!(snap.total_reserved < MAX_PROCESS_FD_BUDGET);

        // Requesting 1 more FD would hit 128 and must fail.
        let err = ledger
            .try_reserve(FdCategory::ActiveDocument, 1)
            .unwrap_err();
        assert!(matches!(
            err,
            SchedulerError::FdBudgetExhausted { cap: 128, .. }
        ));

        drop(docs);
        assert_eq!(ledger.snapshot().total_reserved, 42);
        drop((gui, sidecar, undo, temp, cleanup, jobs));
        assert_eq!(ledger.snapshot().total_reserved, 0);
        assert_eq!(ledger.snapshot().peak_reserved, 127);
    }

    #[test]
    fn raise_nofile_limit_never_lowers_soft_limit() {
        let status = quarry_core::raise_nofile_limit(DEFAULT_NOFILE_TARGET);
        if let (Some(prev), Some(curr)) = (status.previous_soft, status.current_soft) {
            assert!(
                curr >= prev,
                "soft limit must never decrease: {prev} -> {curr}"
            );
        }
    }
}
