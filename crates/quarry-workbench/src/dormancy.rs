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
    AppMessage, ColumnView, Document, DocumentId, EditHistory, GridCursor, GridSelection,
    RowSelection, SOURCE_CHANGED_NOTICE, SharedCellEdits, WindowId, WorkbenchTab, WorkingCopyState,
    compressed,
};
use quarry_core::{
    Encoding, FileIdentity, FilterQuery, OpenOptions, Session, SourceGuard, WorkingCopyCleanup,
    inspect_storage,
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
            Self::OpenFailed(message) => write!(f, "{message}"),
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
}

struct DormantDirtyState {
    session: Arc<Session>,
    original_session: Option<Session>,
    working_copy: Option<WorkingCopyState>,
    header_renames: Arc<BTreeMap<usize, Arc<String>>>,
    cell_edits: SharedCellEdits,
    overlay_bytes: usize,
    /// Retains code-counted directory FD reservations for any live `WorkingCopyStorage`.
    _working_dir_fd_lease: Option<FdLease>,
}

enum DormantKind {
    /// Clean plain file: `Session`, `StructuralIndex`, and row buffers fully dropped.
    CleanPlain {
        open_options: OpenOptions,
        encoding: Option<Encoding>,
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
        /// Code-counted active-document FD lease; released immediately on dormancy.
        _fd_lease: FdLease,
        /// Code-counted directory FD lease for live `WorkingCopyStorage` or `CompressedBacking`
        /// workspaces; retained across hydrate <-> dormant transitions until the owner drops.
        temp_dir_fd_lease: Option<FdLease>,
        /// Code-counted background-job FD lease covering active index (1), filter (2),
        /// and filtered-read (1) workers for the lifetime of those jobs.
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
    /// Optional test hook simulating available scratch bytes (None uses OS `check_storage`).
    pub simulated_scratch_available_bytes: Option<u64>,
}

impl Default for WorkbenchSchedulerConfig {
    fn default() -> Self {
        Self {
            max_hydrated_tabs: DEFAULT_MAX_HYDRATED_TABS,
            max_dirty_overlay_bytes: DEFAULT_MAX_DIRTY_OVERLAY_BYTES,
            max_process_fds: MAX_PROCESS_FD_BUDGET,
            working_directory: std::env::temp_dir(),
            custom_working_directory: false,
            index_cache_enabled: true,
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
    active_by_window: BTreeMap<WindowId, DocumentId>,
    quit_pending: bool,
    quit_approved: BTreeSet<DocumentId>,
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
            active_by_window: BTreeMap::new(),
            quit_pending: false,
            quit_approved: BTreeSet::new(),
        }
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

    /// Finds an existing tab owning `path` by canonical path OR OS `FileIdentity` (`dev`, `ino`),
    /// across hydrated tabs, dormant tabs, AND lazy stubs (plus active working files).
    pub fn find_owner_for_path(&self, path: &Path) -> Option<(WindowId, DocumentId)> {
        let canonical = canonicalize_best_effort(path);
        let identity = FileIdentity::from_path(path).ok().flatten();
        for (&doc_id, slot) in &self.tabs {
            if tab_matches_path(slot, path, &canonical, identity) {
                return Some((slot.window, doc_id));
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

    /// True when the shared global two-write limit (`MAX_WRITE_JOBS`) has a free slot.
    pub fn write_slot_available(&self) -> bool {
        let in_use: usize = self
            .tabs
            .values()
            .filter(|slot| matches!(slot.residency, SlotResidency::Hydrated { .. }))
            .map(|slot| slot.tab.unslotted_document_operations(false))
            .sum::<usize>()
            + self.decompression_slots.active();
        in_use < MAX_WRITE_JOBS
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
        match self.activate_tab(window, id) {
            Ok(outcome) => Ok(outcome),
            Err(error) => {
                self.tabs.remove(&id);
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

        // Reserve an active-document FD lease BEFORE opening the file.
        let fd_lease = match self.fd_budget.try_reserve(FdCategory::ActiveDocument, 1) {
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
                _fd_lease: fd_lease,
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
        let needed_jobs = usize::from(doc.job.is_some()) + FILTER_JOB_AND_READ_FDS;
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

    /// Synchronizes live `TempDirectory` and `BackgroundJob` FD reservations across all
    /// hydrated tabs (accounting for newly acquired working copies, completed gzip workspaces,
    /// or externally started/completed index, filter, and filtered-read workers).
    pub fn sync_fd_leases(&mut self) -> Result<(), SchedulerError> {
        let fd_budget = self.fd_budget.clone();
        for slot in self.tabs.values_mut() {
            let SlotResidency::Hydrated {
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
    /// directory and background-job FD leases.
    pub fn poll_all(&mut self) -> Vec<(DocumentId, String)> {
        let mut errors = Vec::new();
        let fd_budget = self.fd_budget.clone();
        for (&id, slot) in &mut self.tabs {
            let SlotResidency::Hydrated {
                temp_dir_fd_lease,
                job_fd_lease,
                pending_filter_restore,
                ..
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
            // Reserve descriptors BEFORE polling filter/read workers so `poll_filter()` or
            // `poll_filter_read()` cannot start a filtered-read worker without an FD lease.
            let can_run_filter_read = if doc.filter_job.is_some() || doc.filter_read.is_some() {
                try_sync_optional_lease(
                    &fd_budget,
                    job_fd_lease,
                    FdCategory::BackgroundJob,
                    doc_active_job_fds(doc),
                )
                .is_ok()
            } else if doc.pending_filter_read.is_some() {
                let needed = usize::from(doc.job.is_some()) + 1;
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

    /// Approves one tab for quit without releasing it yet (cancelled quit stays lossless).
    pub fn approve_tab_for_quit(&mut self, id: DocumentId) -> Result<(), SchedulerError> {
        if !self.tabs.contains_key(&id) {
            return Err(SchedulerError::NotFound(id));
        }
        self.quit_approved.insert(id);
        Ok(())
    }

    /// Finalizes quit only after every dirty tab (hydrated or dormant) has been approved via
    /// [`Self::approve_tab_for_quit`]. If any dirty tab is unapproved, returns
    /// [`SchedulerError::UnapprovedDirtyTabs`] while leaving every tab and working copy intact.
    /// Otherwise shuts down and drops every tab, releasing all Quarry-owned private working
    /// copies, Undo/Redo directories, and FD leases.
    pub fn finalize_quit(&mut self) -> Result<Vec<String>, SchedulerError> {
        let unapproved: Vec<DocumentId> = self
            .tabs
            .iter()
            .filter_map(|(&id, slot)| {
                (slot_is_dirty(slot) && !self.quit_approved.contains(&id)).then_some(id)
            })
            .collect();
        if !unapproved.is_empty() {
            return Err(SchedulerError::UnapprovedDirtyTabs(unapproved));
        }
        self.quit_pending = false;
        self.quit_approved.clear();
        self.active_by_window.clear();
        let tabs = std::mem::take(&mut self.tabs);
        for (_, mut slot) in tabs {
            if let Some(doc) = slot.tab.document.as_mut() {
                doc.shutdown();
            }
            drop(slot);
        }
        Ok(self.working_cleanup.take_errors())
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
                dirty.original_session.is_some()
                    || !dirty.cell_edits.is_empty()
                    || !dirty.header_renames.is_empty()
            }
            _ => false,
        },
        SlotResidency::Stub(_) => false,
    }
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

fn doc_active_job_fds(doc: &Document) -> usize {
    // Index holds 1 file descriptor; an active filter reserves 3 FDs (2 for scan + 1 pre-reserved
    // for its concurrent filtered-read worker); a standalone filtered read holds 1 descriptor.
    usize::from(doc.job.is_some())
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
            _working_dir_fd_lease: working_dir_fd_lease,
        }))
    } else {
        // Clean plain document: drop `Document` (including `Session`, `StructuralIndex`, buffers).
        doc.shutdown();
        drop(doc);
        DormantKind::CleanPlain {
            open_options: prep.open_options,
            encoding: prep.encoding,
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

fn hydrate_from_stub(
    stub: &LazyDocumentStub,
    config: &WorkbenchSchedulerConfig,
    working_cleanup: &WorkingCopyCleanup,
    decompression_slots: &compressed::DecompressionSlots,
    fd_budget: &FdBudgetLedger,
    write_slot_available: bool,
) -> Result<(Document, Option<FdLease>, Option<FdLease>), SchedulerError> {
    let mut doc =
        Document::prepare_with_encoding(&stub.logical_path, stub.open_options, stub.encoding)
            .map_err(SchedulerError::OpenFailed)?;
    if doc.awaiting_decompression() && !write_slot_available {
        return Err(SchedulerError::OpenFailed(
            compressed::WRITE_SLOTS_FULL.to_owned(),
        ));
    }
    doc.working_directory = config.working_directory.clone();
    doc.custom_working_directory = config.custom_working_directory;
    doc.index_cache_enabled = config.index_cache_enabled;
    doc.working_cleanup = working_cleanup.clone();
    doc.decompression_slots = decompression_slots.clone();
    let mut job_fd_lease = Some(fd_budget.try_reserve(FdCategory::BackgroundJob, 1)?);
    doc.start_indexing().map_err(SchedulerError::OpenFailed)?;
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
        } => {
            if !source_unchanged {
                // External file changed or vanished while dormant: never silently reload!
                // Try opening a read-only shell if the file exists, or surface a conflict error.
                let mut doc = match Document::prepare_with_encoding(
                    &dormant.logical_path,
                    open_options,
                    encoding,
                ) {
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
                doc.source_changed = true;
                return Ok((
                    doc,
                    WakeKind::CleanCold,
                    Some(SOURCE_CHANGED_NOTICE.to_owned()),
                    None,
                    None,
                ));
            }

            let mut doc = match Document::prepare_with_encoding(
                &dormant.logical_path,
                open_options,
                encoding,
            ) {
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
                _working_dir_fd_lease: working_dir_fd_lease,
            } = *dirty;
            let mut doc = Document::from_arc_session(session, &dormant.logical_path);
            doc.original_session = original_session;
            doc.working_copy = working_copy;
            doc.edit_history = dormant.view.edit_history.clone();
            doc.header_renames = header_renames;
            doc.cell_edits = cell_edits;
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
