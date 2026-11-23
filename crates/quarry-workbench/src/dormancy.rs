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
    /// while `StructuralIndex` and row buffers are evicted.
    CompressedComplete {
        session: Arc<Session>,
        compressed: compressed::CompressedDocument,
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

enum SlotResidency {
    Stub(LazyDocumentStub),
    Hydrated {
        /// Code-counted active-document FD lease; released immediately on dormancy.
        _fd_lease: FdLease,
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

        // Before hydrating a new tab, verify we can stay within the hydration limit.
        // Temporarily consider `window`'s previous active tab eligible for eviction if
        // `id` will replace it as `window`'s active tab!
        let previous_active = self.active_by_window.get(&window).copied();
        let evicted = self.ensure_hydration_capacity(Some(window), previous_active)?;

        // Reserve an active-document FD lease BEFORE opening the file.
        let fd_lease = match self.fd_budget.try_reserve(FdCategory::ActiveDocument, 1) {
            Ok(lease) => lease,
            Err(error) => {
                self.restore_previous_active_on_failure(window, previous_active, &evicted);
                return Err(error);
            }
        };

        // Re-check overlay budget before hydrating a non-dirty tab; waking an already-dormant
        // dirty tab stays allowed so users can always wake it to save or discard edits.
        if !waking_dormant_dirty {
            let overlay_used = self.total_dirty_overlay_bytes();
            if overlay_used > self.config.max_dirty_overlay_bytes {
                drop(fd_lease);
                self.restore_previous_active_on_failure(window, previous_active, &evicted);
                return Err(SchedulerError::OverlayBudgetExceeded {
                    used_bytes: overlay_used,
                    cap_bytes: self.config.max_dirty_overlay_bytes,
                });
            }
        }

        let write_slot_available = self.write_slot_available();
        let tick = self.next_tick();
        let slot = self.tabs.get_mut(&id).unwrap();
        let previous_residency = std::mem::replace(
            &mut slot.residency,
            SlotResidency::Hydrated {
                _fd_lease: fd_lease,
            },
        );

        let wake_result: Result<(WakeKind, Option<String>), (SlotResidency, SchedulerError)> =
            match previous_residency {
                SlotResidency::Stub(stub) => {
                    match hydrate_from_stub(
                        &stub,
                        &self.config,
                        &self.working_cleanup,
                        &self.decompression_slots,
                        write_slot_available,
                    ) {
                        Ok(doc) => {
                            slot.tab.document = Some(doc);
                            Ok((WakeKind::StubInitialOpen, None))
                        }
                        Err(error) => Err((SlotResidency::Stub(stub), error)),
                    }
                }
                SlotResidency::Dormant(dormant) => {
                    match wake_dormant_document(
                        dormant,
                        &self.config,
                        &self.working_cleanup,
                        &self.decompression_slots,
                    ) {
                        Ok((doc, kind, conflict_notice)) => {
                            slot.tab.document = Some(doc);
                            Ok((kind, conflict_notice))
                        }
                        Err((restored_dormant, error)) => {
                            Err((SlotResidency::Dormant(restored_dormant), error))
                        }
                    }
                }
                SlotResidency::Hydrated { .. } => unreachable!(),
            };

        match wake_result {
            Ok((kind, conflict_notice)) => {
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
                // the window's previously active tab if it was evicted for this failed open.
                slot.residency = restored;
                self.restore_previous_active_on_failure(window, previous_active, &evicted);
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

    fn restore_previous_active_on_failure(
        &mut self,
        window: WindowId,
        previous_active: Option<DocumentId>,
        evicted: &[DocumentId],
    ) {
        if let Some(prev_id) = previous_active
            && evicted.contains(&prev_id)
            && self.tabs.contains_key(&prev_id)
        {
            let _ = self.activate_tab(window, prev_id);
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

    /// Polls active indexing and filter workers on all hydrated tabs.
    pub fn poll_all(&mut self) -> Vec<(DocumentId, String)> {
        let mut errors = Vec::new();
        for (&id, slot) in &mut self.tabs {
            if !matches!(slot.residency, SlotResidency::Hydrated { .. }) {
                continue;
            }
            let Some(doc) = slot.tab.document.as_mut() else {
                continue;
            };
            if let Err(err) = doc.poll() {
                errors.push((id, err));
            }
            if let Err(err) = doc.poll_filter() {
                errors.push((id, err));
            }
            if let Err(err) = doc.poll_filter_read() {
                errors.push((id, err));
            }
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

    /// Finalizes quit: shuts down and drops every tab (hydrated, dormant, stub), releasing
    /// all Quarry-owned private working copies, Undo/Redo directories, and FD leases.
    pub fn finalize_quit(&mut self) -> Vec<String> {
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
        self.working_cleanup.take_errors()
    }

    fn ensure_hydration_capacity(
        &mut self,
        activating_window: Option<WindowId>,
        replacing_active: Option<DocumentId>,
    ) -> Result<Vec<DocumentId>, SchedulerError> {
        let mut evicted = Vec::new();
        // Account for a potentially new window entry so its active slot stays guaranteed.
        let projected_windows = match activating_window {
            Some(window) if !self.active_by_window.contains_key(&window) => {
                self.active_by_window.len() + 1
            }
            _ => self.active_by_window.len().max(1),
        };
        let limit = self.config.max_hydrated_tabs.max(projected_windows);

        while self.hydrated_count() >= limit {
            let candidate = self.pick_lru_eviction_candidate(replacing_active);
            let Some(victim_id) = candidate else {
                let reasons = self
                    .tabs
                    .keys()
                    .filter_map(|&id| self.pin_reason(id).map(|reason| (id, reason)))
                    .collect();
                return Err(SchedulerError::AllTabsPinned {
                    hydrated: self.hydrated_count(),
                    limit,
                    reasons,
                });
            };
            self.evict_tab_unchecked(victim_id)?;
            evicted.push(victim_id);
        }
        Ok(evicted)
    }

    fn pick_lru_eviction_candidate(
        &self,
        replacing_active: Option<DocumentId>,
    ) -> Option<DocumentId> {
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
        candidates.first().map(|&(_, _, id)| id)
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

        // Reserve persistent directory FD leases for any retained WorkingCopyStorage
        // BEFORE demoting so dormant working directories cannot exceed the < 128 FD cap.
        let working_dir_fd_lease = if let Some(wc) = doc.working_copy.as_ref() {
            let dir_fds = 1 + wc.previous_directories.len();
            Some(
                self.fd_budget
                    .try_reserve(FdCategory::TempDirectory, dir_fds)?,
            )
        } else {
            None
        };

        let mut doc = slot.tab.document.take().unwrap();
        let dormant = demote_document(doc_into_dormant_parts(&mut doc), doc, working_dir_fd_lease)?;
        // Replacing `Hydrated { _fd_lease }` drops `_fd_lease`, freeing the FD budget slot.
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

fn canonicalize_best_effort(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn paths_alias(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    if let (Ok(a), Ok(b)) = (std::fs::canonicalize(left), std::fs::canonicalize(right))
        && a == b
    {
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
        SlotResidency::Stub(stub) => {
            paths_alias(path, &stub.logical_path)
                || stub.canonical_path == canonical
                || (identity.is_some() && identity == stub.file_identity)
        }
        SlotResidency::Dormant(dormant) => {
            paths_alias(path, &dormant.logical_path)
                || dormant.canonical_path == canonical
                || (identity.is_some() && identity == dormant.file_identity)
                || match &dormant.kind {
                    DormantKind::Dirty(dirty) => {
                        paths_alias(path, dirty.session.path())
                            || (identity.is_some() && identity == dirty.session.file_identity())
                    }
                    DormantKind::CompressedComplete { session, .. } => {
                        paths_alias(path, session.path())
                            || (identity.is_some() && identity == session.file_identity())
                    }
                    _ => false,
                }
        }
        SlotResidency::Hydrated { .. } => slot.tab.document.as_ref().is_some_and(|doc| {
            paths_alias(path, &doc.logical_path)
                || (!doc.session.path().as_os_str().is_empty()
                    && paths_alias(path, doc.session.path()))
                || (identity.is_some() && identity == doc.session.file_identity())
                || (identity.is_some()
                    && doc
                        .original_session
                        .as_ref()
                        .and_then(Session::file_identity)
                        == identity)
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
/// any uncommitted inline cell or header edit drafts (and their prospective history cost).
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
    cell_bytes
        .saturating_add(header_bytes)
        .saturating_add(pending_cell_bytes)
        .saturating_add(pending_header_bytes)
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
    write_slot_available: bool,
) -> Result<Document, SchedulerError> {
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
    doc.start_indexing().map_err(SchedulerError::OpenFailed)?;
    Ok(doc)
}

type WakeResult =
    Result<(Document, WakeKind, Option<String>), (Box<DormantDocument>, SchedulerError)>;

fn wake_dormant_document(
    dormant: Box<DormantDocument>,
    config: &WorkbenchSchedulerConfig,
    working_cleanup: &WorkingCopyCleanup,
    decompression_slots: &compressed::DecompressionSlots,
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
                let _ = doc.start_filter(query);
                restore_filtered_view_state(&mut doc, &dormant.view);
                WakeKind::FilteredRebuilding
            } else {
                WakeKind::CleanCold
            };
            Ok((doc, kind, None))
        }
        DormantKind::Dirty(dirty) => {
            let mut doc = Document::from_arc_session(dirty.session, &dormant.logical_path);
            doc.original_session = dirty.original_session;
            doc.working_copy = dirty.working_copy;
            doc.edit_history = dormant.view.edit_history.clone();
            doc.header_renames = dirty.header_renames;
            doc.cell_edits = dirty.cell_edits;
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
                ));
            }
            let _ = doc.start_indexing();
            if let Some(query) = dormant.view.filter_query.clone() {
                let _ = doc.start_filter(query);
                restore_filtered_view_state(&mut doc, &dormant.view);
            }
            Ok((doc, WakeKind::DirtyRehydrated, None))
        }
        DormantKind::CompressedComplete {
            session,
            compressed,
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
                ));
            }
            // Re-index the retained private decompressed copy directly (never re-decompresses).
            let _ = doc.start_indexing();
            if let Some(query) = dormant.view.filter_query.clone() {
                let _ = doc.start_filter(query);
                restore_filtered_view_state(&mut doc, &dormant.view);
            }
            Ok((doc, WakeKind::CompressedCompleteRehydrated, None))
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
                ));
            }
            // Never restart decompression silently on wake for a Preview tab!
            Ok((doc, WakeKind::CompressedPreviewRestored, None))
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
