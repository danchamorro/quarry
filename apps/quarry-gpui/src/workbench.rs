//! Process-wide multi-window workbench controller backed by `quarry_workbench::WorkbenchScheduler`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use gpui_kit::component::Root;
use gpui_kit::{AnyWindowHandle, App, AppContext, Context, Entity, Global, WindowHandle};
use quarry_core::{
    ByteWindow, CaseSensitivity, ColumnCleanup, DateFormatSuggestion, DuplicateTarget, Encoding,
    FilterQuery, HeaderMode, JoinMode, OpenOptions, SectionBounds, SectionOpenOptions, SortPlan,
    StorageSpace, VerifyBoundaryOutcome, VerifyBoundaryProgress, raise_nofile_limit,
};
use quarry_workbench::compressed::{
    DecompressionStatus, decompression_status, gzip_destination_warning,
};
use quarry_workbench::{
    AppMessage, CombineWorkflow, CommandContext, CommandId, CommandResolution,
    DEFAULT_NOFILE_TARGET, DelimiterMode, Document, DocumentId, FailedOpen, HudAnnouncement,
    HudAnnouncementTracker, HudIndexPhase, InspectWorkflow, JOIN_INPUT_RESERVED, JoinWorkflow,
    MessageSeverity, Palette, ResidencyState, ResolvedTheme, ResourceHudSample, ResourceHudSampler,
    ResourceHudTargets, SchedulerError, SelectionModifiers, StoragePreferences, StructuralDialog,
    StructuralJob, ThemeAppearance, ThemePreference, ThemePreferences, WakeOutcome, WindowId,
    WorkbenchScheduler, WorkbenchSchedulerConfig, WorkbenchTab, clear_index_cache_summary,
    evaluate_command, inspect_temporary_storage, materialization_progress,
    omarchy_theme_fingerprint, resolve_theme, select_column, sort_progress,
};

/// Shared global concurrency cap across document saves, exports, structural edits, decompressions,
/// Combine Files, Join Files, and Recovery copies.
const MAX_GLOBAL_WRITE_OPERATIONS: usize = 2;

/// Maximum single-cell edit payload accepted by the modal cell editor (1 MiB).
pub const MAX_EDIT_BYTES: usize = 1024 * 1024;

/// Summary of a completed duplicate scan awaiting user confirmation to remove duplicates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DuplicatePreviewInfo {
    pub duplicate_rows: u64,
    pub unique_rows: u64,
    pub total_rows: u64,
}

/// Read-only snapshot of bounded section document state (boxed on `DocumentSnapshot` to keep stack frames small).
#[derive(Clone, Debug, PartialEq)]
pub struct SectionSnapshot {
    pub bounds: SectionBounds,
    pub first_global_data_row: Option<u64>,
    pub verify_running: bool,
    pub verify_paused: bool,
    pub verify_pausing: bool,
    pub verify_progress: Option<VerifyBoundaryProgress>,
    pub verify_status: Option<String>,
    pub pending_verified_reopen: Option<SectionBounds>,
    pub can_verify_boundary: bool,
    pub speculative_save_warning: Option<&'static str>,
}

/// Read-only snapshot of an active hydrated document for toolbar, panels, and status bar rendering.
#[derive(Clone, Debug, PartialEq)]
pub struct DocumentSnapshot {
    pub document_id: DocumentId,
    pub instance_id: u64,
    pub path: PathBuf,
    pub filename: String,
    pub file_size: u64,
    pub active_byte_len: u64,
    pub section: Option<Box<SectionSnapshot>>,
    pub delimiter: u8,
    pub has_header: bool,
    pub encoding: Encoding,
    pub headers: Vec<String>,
    pub shown_columns: Vec<usize>,
    pub total_rows: u64,
    pub available_rows: u64,
    pub filter_active: bool,
    pub filter_done: bool,
    pub filter_running: bool,
    pub filter_status: Option<String>,
    pub can_export_filtered: bool,
    pub export_running: bool,
    pub export_status: Option<String>,
    pub search_running: bool,
    pub search_status: Option<String>,
    pub structural_running: bool,
    pub structural_status: Option<String>,
    pub structural_progress: Option<f32>,
    pub duplicate_preview: Option<DuplicatePreviewInfo>,
    pub pending_split_confirm_bytes: Option<u64>,
    pub selected_rows_count: u64,
    pub selected_columns: Vec<usize>,
    pub index_done: bool,
    pub index_paused: bool,
    pub index_pausing: bool,
    pub index_fraction: f32,
    pub index_committed_bytes: u64,
    pub index_bytes_read: u64,
    pub index_committed_rows: u64,
    pub can_pause_indexing: bool,
    pub can_resume_indexing: bool,
    pub dirty: bool,
    pub saving: bool,
    pub busy: bool,
    pub can_undo: bool,
    pub can_redo: bool,
    pub source_changed: bool,
    pub applied_delimiter: Option<u8>,
    pub applied_header_mode: HeaderMode,
    pub applied_encoding: Option<Encoding>,
    pub is_compressed: bool,
    pub compressed_label: Option<String>,
    pub decompression_status: Option<DecompressionStatus>,
    pub compressed_pending: bool,
    pub compressed_preview: bool,
    pub compressed_unreadable_text: bool,
    pub join_reserved: bool,
    pub has_diagnostic: bool,
}

/// Summary of a tab rendered inside a window's tab bar (works for Stub, Dormant, and Hydrated).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabBarEntry {
    pub id: DocumentId,
    pub title: String,
    pub active: bool,
    pub dirty: bool,
    pub residency: ResidencyState,
}

/// Outcome of requesting a tab close, window close, or application quit review.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloseReviewAction {
    /// No dirty tabs blocked the operation; caller can close the window/app immediately.
    Proceed,
    /// Present Save / Discard / Keep Editing for `document_id` in `window_id`.
    ConfirmTab {
        review_token: u64,
        window_id: WindowId,
        document_id: DocumentId,
        filename: String,
    },
    /// Active file operations are running; wait before closing.
    Wait { message: String },
}

#[derive(Clone, Debug)]
pub enum PendingReviewScope {
    SingleTab {
        window_id: WindowId,
        document_id: DocumentId,
        close_window_if_last: bool,
    },
    Window {
        window_id: WindowId,
        remaining_dirty: Vec<DocumentId>,
        approved_dirty: Vec<DocumentId>,
    },
    Quit {
        remaining_dirty: Vec<(WindowId, DocumentId)>,
    },
}

impl PendingReviewScope {
    fn current_window(&self) -> Option<WindowId> {
        match self {
            Self::SingleTab { window_id, .. } | Self::Window { window_id, .. } => Some(*window_id),
            Self::Quit { remaining_dirty } => remaining_dirty.first().map(|(win, _)| *win),
        }
    }

    fn current_document(&self) -> Option<DocumentId> {
        match self {
            Self::SingleTab { document_id, .. } => Some(*document_id),
            Self::Window {
                remaining_dirty, ..
            } => remaining_dirty.first().copied(),
            Self::Quit { remaining_dirty } => remaining_dirty.first().map(|(_, doc)| *doc),
        }
    }
}

struct WorkbenchGlobal(Entity<WorkbenchController>);

impl Global for WorkbenchGlobal {}

type StorageCheckOutcome = Result<(StorageSpace, u64), String>;
type StorageCheckWorker = (u64, JoinHandle<StorageCheckOutcome>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkflowDialogKind {
    Combine,
    Join,
    Inspect,
}

pub struct WorkbenchController {
    scheduler: WorkbenchScheduler,
    storage_prefs: StoragePreferences,
    theme_prefs: ThemePreferences,
    theme_home: Option<PathBuf>,
    system_appearance: ThemeAppearance,
    resolved_theme: ResolvedTheme,
    theme_error: Option<String>,
    omarchy_last_check: Instant,
    omarchy_fingerprint: Option<String>,
    combine: Box<CombineWorkflow>,
    join: Box<JoinWorkflow>,
    inspect: Box<InspectWorkflow>,
    combine_dialog_owner: Option<WindowId>,
    join_dialog_owner: Option<WindowId>,
    inspect_dialog_owner: Option<WindowId>,
    cache_clear_worker: Option<JoinHandle<Result<String, String>>>,
    cache_clear_status: Option<Result<String, String>>,
    storage_check_gen: u64,
    storage_check_worker: Option<StorageCheckWorker>,
    storage_check_result: Option<StorageCheckOutcome>,
    next_window_id: u64,
    next_review_token: u64,
    windows: BTreeMap<WindowId, WindowHandle<Root>>,
    editing_windows: BTreeSet<WindowId>,
    viewport_read_seq: BTreeMap<DocumentId, u64>,
    pending_reveal_cells: BTreeMap<DocumentId, (u64, usize)>,
    show_all_whitespace: bool,
    window_messages: BTreeMap<WindowId, AppMessage>,
    pending_review: Option<PendingReviewScope>,
    active_review_token: Option<u64>,
    pending_review_action: Option<CloseReviewAction>,
    pending_close_windows: Vec<WindowId>,
    quit_requested_complete: bool,
    hud_sampler: ResourceHudSampler,
    hud_targets: ResourceHudTargets,
    hud_sample: ResourceHudSample,
    hud_announcements: BTreeMap<WindowId, HudAnnouncementTracker>,
    last_hud_announcement: BTreeMap<WindowId, HudAnnouncement>,
}

impl WorkbenchController {
    #[cfg(test)]
    pub fn new(config: WorkbenchSchedulerConfig) -> Self {
        Self::new_with_prefs(config, StoragePreferences::default())
    }

    #[cfg(test)]
    pub fn new_with_prefs(
        config: WorkbenchSchedulerConfig,
        storage_prefs: StoragePreferences,
    ) -> Self {
        Self::new_with_all_prefs(
            config,
            storage_prefs,
            ThemePreferences::load_from(None, false),
            None,
        )
    }

    pub fn new_with_all_prefs(
        mut config: WorkbenchSchedulerConfig,
        storage_prefs: StoragePreferences,
        theme_prefs: ThemePreferences,
        theme_home: Option<PathBuf>,
    ) -> Self {
        if let Some(dir) = storage_prefs.scratch_directory.clone() {
            config.working_directory = dir;
            config.custom_working_directory = true;
        }
        if let Some(enabled) = storage_prefs.index_cache_enabled {
            config.index_cache_enabled = enabled;
        }
        let system_appearance = ThemeAppearance::Light;
        let pre_fingerprint = (theme_prefs.preference == ThemePreference::Omarchy)
            .then(|| theme_home.as_deref().and_then(omarchy_theme_fingerprint))
            .flatten();
        let resolved_theme = resolve_theme(
            theme_prefs.preference,
            system_appearance,
            theme_home.as_deref(),
        );
        let omarchy_fingerprint = if theme_prefs.preference == ThemePreference::Omarchy
            && resolved_theme.warning.is_none()
        {
            pre_fingerprint
        } else {
            None
        };
        let scheduler = WorkbenchScheduler::new(config);
        let working_dir = scheduler.working_directory().to_path_buf();
        let custom_dir = scheduler.custom_working_directory();
        let cleanup = scheduler.working_cleanup();
        let slots = scheduler.decompression_slots().clone();
        let hud_targets = ResourceHudTargets {
            document_dirs: BTreeSet::new(),
            scratch_dir: working_dir.clone(),
        };
        let hud_sampler = ResourceHudSampler::start(hud_targets.clone());
        let hud_sample = hud_sampler.snapshot();
        Self {
            scheduler,
            storage_prefs,
            theme_prefs,
            theme_home,
            system_appearance,
            resolved_theme,
            theme_error: None,
            omarchy_last_check: Instant::now(),
            omarchy_fingerprint,
            combine: Box::default(),
            join: Box::new(JoinWorkflow::new(working_dir.clone())),
            inspect: Box::new(InspectWorkflow::new(
                working_dir,
                custom_dir,
                cleanup,
                slots,
            )),
            combine_dialog_owner: None,
            join_dialog_owner: None,
            inspect_dialog_owner: None,
            cache_clear_worker: None,
            cache_clear_status: None,
            storage_check_gen: 0,
            storage_check_worker: None,
            storage_check_result: None,
            next_window_id: 1,
            next_review_token: 1,
            windows: BTreeMap::new(),
            editing_windows: BTreeSet::new(),
            viewport_read_seq: BTreeMap::new(),
            pending_reveal_cells: BTreeMap::new(),
            show_all_whitespace: false,
            window_messages: BTreeMap::new(),
            pending_review: None,
            active_review_token: None,
            pending_review_action: None,
            pending_close_windows: Vec::new(),
            quit_requested_complete: false,
            hud_sampler,
            hud_targets,
            hud_sample,
            hud_announcements: BTreeMap::new(),
            last_hud_announcement: BTreeMap::new(),
        }
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<WorkbenchGlobal>().map(|g| g.0.clone())
    }

    pub fn init(cx: &mut App) -> Entity<Self> {
        if let Some(existing) = Self::try_global(cx) {
            return existing;
        }
        let _ = raise_nofile_limit(DEFAULT_NOFILE_TARGET);
        let storage_prefs = StoragePreferences::application();
        let theme_prefs = ThemePreferences::load_default();
        let theme_home = std::env::var_os("HOME").map(PathBuf::from);
        let config = WorkbenchSchedulerConfig {
            defer_gzip_decompression: true,
            ..WorkbenchSchedulerConfig::default()
        };
        let entity =
            cx.new(|_| Self::new_with_all_prefs(config, storage_prefs, theme_prefs, theme_home));
        cx.set_global(WorkbenchGlobal(entity.clone()));

        let poll_entity = entity.clone();
        cx.spawn(async move |cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(32))
                    .await;
                let keep_going = cx.update(|cx| {
                    if Self::try_global(cx).is_none() {
                        return false;
                    }
                    let omarchy_update = poll_entity.update(cx, |this, cx| {
                        this.poll_background(cx);
                        this.poll_omarchy_theme(cx)
                    });
                    if let Some(resolved) = omarchy_update {
                        crate::apply_resolved_theme(&resolved, cx);
                        cx.refresh_windows();
                    }
                    true
                });
                if !keep_going {
                    break;
                }
            }
        })
        .detach();

        entity
    }

    #[cfg(test)]
    pub fn reset_for_test(cx: &mut App, config: WorkbenchSchedulerConfig) -> Entity<Self> {
        let entity = cx.new(|_| Self::new(config));
        cx.set_global(WorkbenchGlobal(entity.clone()));
        entity
    }

    #[cfg(test)]
    pub fn reset_for_test_with_prefs(
        cx: &mut App,
        config: WorkbenchSchedulerConfig,
        prefs: StoragePreferences,
    ) -> Entity<Self> {
        let entity = cx.new(|_| Self::new_with_prefs(config, prefs));
        cx.set_global(WorkbenchGlobal(entity.clone()));
        entity
    }

    #[cfg(test)]
    pub fn reset_for_test_with_theme(
        cx: &mut App,
        config: WorkbenchSchedulerConfig,
        theme_prefs: ThemePreferences,
        theme_home: Option<PathBuf>,
    ) -> Entity<Self> {
        let entity = cx.new(|_| {
            Self::new_with_all_prefs(
                config,
                StoragePreferences::default(),
                theme_prefs,
                theme_home,
            )
        });
        cx.set_global(WorkbenchGlobal(entity.clone()));
        entity
    }

    // --- Theme management (Light / Dark / System / Omarchy) ---

    pub fn active_palette(&self) -> Palette {
        self.resolved_theme.palette
    }

    pub fn resolved_theme(&self) -> &ResolvedTheme {
        &self.resolved_theme
    }

    pub fn theme_warning(&self) -> Option<&str> {
        self.theme_error
            .as_deref()
            .or(self.resolved_theme.warning.as_deref())
    }

    pub fn clear_theme_error(&mut self, cx: &mut Context<Self>) {
        if self.theme_error.take().is_some() {
            cx.notify();
        }
    }

    pub fn set_theme_preference(
        &mut self,
        preference: ThemePreference,
        cx: &mut Context<Self>,
    ) -> Result<ResolvedTheme, String> {
        if let Err(error) = self.theme_prefs.set_preference(preference) {
            self.theme_error = Some(error.clone());
            cx.notify();
            return Err(error);
        }
        self.theme_error = None;
        self.omarchy_last_check = Instant::now();
        let pre_fingerprint = (preference == ThemePreference::Omarchy)
            .then(|| {
                self.theme_home
                    .as_deref()
                    .and_then(omarchy_theme_fingerprint)
            })
            .flatten();
        let resolved = resolve_theme(
            preference,
            self.system_appearance,
            self.theme_home.as_deref(),
        );
        self.resolved_theme = resolved.clone();
        self.omarchy_fingerprint =
            if preference == ThemePreference::Omarchy && resolved.warning.is_none() {
                pre_fingerprint
            } else {
                None
            };
        cx.notify();
        Ok(resolved)
    }

    pub fn sync_system_appearance(
        &mut self,
        appearance: ThemeAppearance,
        cx: &mut Context<Self>,
    ) -> Option<ResolvedTheme> {
        if self.system_appearance == appearance {
            return None;
        }
        self.system_appearance = appearance;
        let resolved = resolve_theme(
            self.theme_prefs.preference,
            self.system_appearance,
            self.theme_home.as_deref(),
        );
        if resolved != self.resolved_theme {
            self.resolved_theme = resolved.clone();
            cx.notify();
            Some(resolved)
        } else {
            None
        }
    }

    pub fn poll_omarchy_theme(&mut self, cx: &mut Context<Self>) -> Option<ResolvedTheme> {
        if self.theme_prefs.preference != ThemePreference::Omarchy {
            return None;
        }
        if self.omarchy_last_check.elapsed() < Duration::from_secs(1) {
            return None;
        }
        self.reload_omarchy_theme_now(cx)
    }

    pub fn reload_omarchy_theme_now(&mut self, cx: &mut Context<Self>) -> Option<ResolvedTheme> {
        if self.theme_prefs.preference != ThemePreference::Omarchy {
            return None;
        }
        self.omarchy_last_check = Instant::now();
        let next_fp = self
            .theme_home
            .as_deref()
            .and_then(omarchy_theme_fingerprint);
        if self.resolved_theme.warning.is_none() && next_fp == self.omarchy_fingerprint {
            return None;
        }
        let resolved = resolve_theme(
            ThemePreference::Omarchy,
            self.system_appearance,
            self.theme_home.as_deref(),
        );
        self.omarchy_fingerprint = if resolved.warning.is_none() {
            next_fp
        } else {
            None
        };
        if resolved != self.resolved_theme {
            self.resolved_theme = resolved.clone();
            cx.notify();
            Some(resolved)
        } else {
            None
        }
    }

    pub fn scheduler(&self) -> &WorkbenchScheduler {
        &self.scheduler
    }

    pub fn allocate_window(&mut self) -> WindowId {
        let id = WindowId::new(self.next_window_id);
        self.next_window_id = self.next_window_id.saturating_add(1);
        self.scheduler.register_window(id);
        id
    }

    pub fn register_window_handle(&mut self, id: WindowId, handle: WindowHandle<Root>) {
        self.scheduler.register_window(id);
        self.windows.insert(id, handle);
    }

    pub fn unregister_window_handle(&mut self, id: WindowId) {
        self.windows.remove(&id);
        self.editing_windows.remove(&id);
        self.window_messages.remove(&id);
        self.hud_announcements.remove(&id);
        self.last_hud_announcement.remove(&id);
        if self.combine_dialog_owner == Some(id) {
            self.combine_dialog_owner = None;
        }
        if self.join_dialog_owner == Some(id) {
            self.join_dialog_owner = None;
            self.join.release_all_async();
        }
        if self.inspect_dialog_owner == Some(id) {
            self.inspect_dialog_owner = None;
        }
    }

    /// Claims process-wide ownership of a shared workflow dialog (`Combine`, `Join`, or `Inspect`).
    /// Returns `false` and focuses the owning window if another window already has that dialog open.
    pub fn claim_workflow_dialog(
        &mut self,
        kind: WorkflowDialogKind,
        window: WindowId,
        cx: &mut Context<Self>,
    ) -> bool {
        let current = match kind {
            WorkflowDialogKind::Combine => self.combine_dialog_owner,
            WorkflowDialogKind::Join => self.join_dialog_owner,
            WorkflowDialogKind::Inspect => self.inspect_dialog_owner,
        };
        if let Some(owner) = current {
            if owner == window {
                return false;
            }
            if let Some(handle) = self.window_handle(owner) {
                let _ = handle.update(cx, |_, win, _| win.activate_window());
                return false;
            }
        }
        match kind {
            WorkflowDialogKind::Combine => self.combine_dialog_owner = Some(window),
            WorkflowDialogKind::Join => self.join_dialog_owner = Some(window),
            WorkflowDialogKind::Inspect => self.inspect_dialog_owner = Some(window),
        }
        true
    }

    pub fn release_workflow_dialog(&mut self, kind: WorkflowDialogKind, window: WindowId) {
        let slot = match kind {
            WorkflowDialogKind::Combine => &mut self.combine_dialog_owner,
            WorkflowDialogKind::Join => &mut self.join_dialog_owner,
            WorkflowDialogKind::Inspect => &mut self.inspect_dialog_owner,
        };
        if *slot == Some(window) {
            *slot = None;
        }
    }

    pub fn write_slot_available(&self) -> bool {
        let workflow_ops = usize::from(self.combine.running_writer())
            + self.join.active_operation_count()
            + self.inspect.active_recovery_count();
        self.scheduler.active_write_operations() + workflow_ops < MAX_GLOBAL_WRITE_OPERATIONS
    }

    pub fn path_reserved(&self, path: &Path) -> bool {
        self.path_reserved_excluding(path, false, false, false)
    }

    pub fn set_window_editing(&mut self, id: WindowId, editing: bool, cx: &mut Context<Self>) {
        if editing {
            self.editing_windows.insert(id);
        } else {
            self.editing_windows.remove(&id);
        }
        cx.notify();
    }

    pub fn request_viewport_rows(
        &mut self,
        window: WindowId,
        position: u64,
        visible_rows: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(doc_id) = self.scheduler.active_document(window) else {
            return;
        };
        let plan = {
            let Some(doc) = self.active_document_mut(window) else {
                return;
            };
            match doc.prepare_async_buffer_read(position, visible_rows) {
                Ok(plan) => plan,
                Err(error) => {
                    if let Some(tab) = self.scheduler.tab_mut(doc_id) {
                        tab.notice = Some(AppMessage::error(error));
                    }
                    cx.notify();
                    return;
                }
            }
        };
        // Always advance the generation — even on cache hits or bootstrap reads — so an
        // in-flight async read from an earlier scroll position cannot overwrite current rows.
        let seq = {
            let entry = self.viewport_read_seq.entry(doc_id).or_insert(0);
            *entry = entry.wrapping_add(1);
            *entry
        };
        let Some(plan) = plan else {
            return;
        };
        let session = Arc::clone(&plan.session);
        let index = plan.index;
        let start = plan.start;
        let count = plan.count;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let began = Instant::now();
                    let read = session.read_rows(&index, start, count);
                    (session, read, began.elapsed())
                })
                .await;
            let _ = this.update(cx, |controller, cx| {
                if controller.viewport_read_seq.get(&doc_id) != Some(&seq) {
                    return;
                }
                let Some(tab) = controller.scheduler.tab_mut(doc_id) else {
                    return;
                };
                let Some(doc) = tab.document.as_mut() else {
                    return;
                };
                let (expected_session, read, elapsed) = result;
                if !Arc::ptr_eq(&doc.session, &expected_session) {
                    return;
                }
                match read {
                    Ok(rows) => {
                        doc.install_buffer_rows(start, rows, elapsed);
                    }
                    Err(error) => {
                        let msg = doc.diagnostic_error(error);
                        tab.notice = Some(AppMessage::error(msg));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn window_handle(&self, id: WindowId) -> Option<WindowHandle<Root>> {
        self.windows.get(&id).copied()
    }

    /// Selects the window that should receive an externally opened document (such as a Finder
    /// `application:openURLs:` request): prefers the active GPUI window if it does not have an
    /// unapplied cell editor open, otherwise prefers another non-editing window, falling back to
    /// the active or first open window.
    pub fn preferred_window_for_external_open(
        &self,
        active_window: Option<AnyWindowHandle>,
    ) -> Option<WindowId> {
        let active_id = active_window.and_then(|active| {
            self.windows
                .iter()
                .find(|(_, handle)| AnyWindowHandle::from(**handle) == active)
                .map(|(&window_id, _)| window_id)
        });
        if let Some(window_id) = active_id
            && !self.editing_windows.contains(&window_id)
        {
            return Some(window_id);
        }
        if let Some(&non_editing) = self
            .windows
            .keys()
            .find(|window_id| !self.editing_windows.contains(window_id))
        {
            return Some(non_editing);
        }
        active_id.or_else(|| self.windows.keys().next().copied())
    }

    /// Handles an OS-level document open request (e.g., macOS Finder double-click or `open -a`).
    /// Returns `None` when `paths` is empty or no window is open yet so the caller can open a new
    /// window. Existing documents (including hard-link aliases owned by another window) route to
    /// and activate their owning window rather than opening a duplicate tab.
    pub fn open_external_paths(
        &mut self,
        preferred_window: Option<WindowId>,
        paths: Vec<PathBuf>,
        cx: &mut Context<Self>,
    ) -> Option<Result<(WindowId, DocumentId), AppMessage>> {
        if paths.is_empty() {
            return None;
        }
        let target_window = preferred_window.or_else(|| self.windows.keys().next().copied())?;
        let conflict_owner = if paths.len() == 1 {
            self.scheduler
                .find_owner_for_path(&paths[0])
                .and_then(|(owner_win, doc_id)| {
                    (self.editing_windows.contains(&owner_win)
                        && self.scheduler.active_document(owner_win) != Some(doc_id))
                    .then_some(owner_win)
                })
        } else {
            None
        };
        let result = self.open_paths(target_window, paths, OpenOptions::default(), None, cx);
        match &result {
            Ok((owner_window, _)) => {
                if let Some(handle) = self.window_handle(*owner_window) {
                    let _ = handle.update(cx, |_, window, _| window.activate_window());
                }
            }
            Err(message) => {
                let notice_window = conflict_owner.unwrap_or(target_window);
                self.set_window_message(notice_window, Some(message.clone()), cx);
                if conflict_owner.is_none()
                    && let Some(handle) = self.window_handle(target_window)
                {
                    let _ = handle.update(cx, |_, window, _| window.activate_window());
                }
            }
        }
        Some(result)
    }

    /// Best-effort non-blocking cleanup invoked when GPUI notifies `on_app_quit`.
    pub fn shutdown_cleanup(&mut self) {
        self.join.release_all_async();
    }

    pub fn show_all_whitespace(&self) -> bool {
        self.show_all_whitespace
    }

    pub fn toggle_show_all_whitespace(&mut self, cx: &mut Context<Self>) {
        self.show_all_whitespace = !self.show_all_whitespace;
        cx.notify();
    }

    pub fn set_window_message(
        &mut self,
        window: WindowId,
        message: Option<AppMessage>,
        cx: &mut Context<Self>,
    ) {
        if let Some(doc_id) = self.scheduler.active_document(window)
            && let Some(tab) = self.scheduler.tab_mut(doc_id)
        {
            tab.notice = message;
            self.window_messages.remove(&window);
        } else if let Some(msg) = message {
            self.window_messages.insert(window, msg);
        } else {
            self.window_messages.remove(&window);
        }
        cx.notify();
    }

    pub fn window_message(&self, window: WindowId) -> Option<&AppMessage> {
        if let Some(doc_id) = self.scheduler.active_document(window)
            && let Some(tab) = self.scheduler.tab(doc_id)
            && let Some(notice) = tab.notice.as_ref()
        {
            return Some(notice);
        }
        self.window_messages.get(&window)
    }

    pub fn active_tab(&self, window: WindowId) -> Option<&WorkbenchTab> {
        let id = self.scheduler.active_document(window)?;
        self.scheduler.tab(id)
    }

    pub fn active_document(&self, window: WindowId) -> Option<&Document> {
        self.active_tab(window)?.document.as_ref()
    }

    pub fn active_document_mut(&mut self, window: WindowId) -> Option<&mut Document> {
        let id = self.scheduler.active_document(window)?;
        self.scheduler.tab_mut(id)?.document.as_mut()
    }

    pub fn tabs_for_window(&self, window: WindowId) -> Vec<TabBarEntry> {
        let active = self.scheduler.active_document(window);
        self.scheduler
            .tabs_in_window(window)
            .into_iter()
            .map(|id| TabBarEntry {
                id,
                title: self
                    .scheduler
                    .tab_display_name(id)
                    .unwrap_or_else(|| "Untitled".into()),
                active: active == Some(id),
                dirty: self.scheduler.tab_is_dirty(id),
                residency: self.scheduler.residency(id).unwrap_or(ResidencyState::Stub),
            })
            .collect()
    }

    pub fn evaluate_window_command(
        &self,
        window: WindowId,
        command: CommandId,
        external_modal_blocked: bool,
        text_field_focused: bool,
    ) -> CommandResolution {
        let fallback = WorkbenchTab::new(0);
        let active_tab = self.active_tab(window).unwrap_or(&fallback);
        let ctx = CommandContext {
            window,
            active_tab,
            open_document_count: self.scheduler.tabs_in_window(window).len(),
            write_slot_available: self.write_slot_available(),
            external_modal_blocked,
            text_field_focused,
            show_all_whitespace: self.show_all_whitespace,
        };
        evaluate_command(command, &ctx)
    }

    pub fn active_snapshot(&self, window: WindowId) -> Option<DocumentSnapshot> {
        let doc_id = self.scheduler.active_document(window)?;
        let tab = self.scheduler.tab(doc_id)?;
        let doc = tab.document.as_ref()?;
        let filename = doc
            .logical_path
            .file_name()
            .unwrap_or(doc.logical_path.as_os_str())
            .to_string_lossy()
            .into_owned();
        let headers = (0..doc.known_width())
            .map(|col| doc.column_name(col))
            .collect::<Vec<_>>();
        let filter_active = doc.filter_active();
        let filter_done = doc.filter_job.is_none() && !doc.filter_rows_loading();
        let (structural_running, structural_progress, duplicate_preview) = match &doc.structural_job
        {
            None => (false, None, None),
            Some(StructuralJob::DuplicatePreview { summary, .. }) => (
                false,
                None,
                Some(DuplicatePreviewInfo {
                    duplicate_rows: summary.duplicate_rows,
                    unique_rows: summary.retained_rows,
                    total_rows: summary.rows_scanned,
                }),
            ),
            Some(StructuralJob::Sorting { job, .. }) => {
                let sp = job.progress();
                let p = sort_progress(
                    sp.preparing,
                    sp.preparation_bytes_scanned,
                    sp.bytes_scanned,
                    sp.total_bytes,
                    sp.done,
                );
                (true, Some(p.fraction), None)
            }
            Some(StructuralJob::Materializing { job, .. }) => {
                let p = materialization_progress(job.progress(), "Applying column edit");
                (true, Some(p.fraction), None)
            }
            Some(StructuralJob::InsertingRows { job, .. }) => {
                let p = materialization_progress(job.progress(), "Inserting rows");
                (true, Some(p.fraction), None)
            }
            Some(StructuralJob::DeletingRows { job, .. }) => {
                let p = materialization_progress(job.progress(), "Deleting rows");
                (true, Some(p.fraction), None)
            }
            Some(_) => (true, None, None),
        };
        let pending_split_confirm_bytes =
            doc.pending_materialization
                .as_ref()
                .map(|(transformation, _, records)| {
                    doc.materialization_storage_estimate_for_records(
                        Some(transformation),
                        None,
                        *records,
                    )
                });
        let active_byte_len = doc.session.active_byte_len();
        let section = doc.section_bounds().map(|bounds| {
            Box::new(SectionSnapshot {
                bounds,
                first_global_data_row: doc.global_data_row_number(doc.data_start),
                verify_running: doc.is_section_verifying(),
                verify_paused: doc.is_section_verify_paused(),
                verify_pausing: doc.is_section_verify_pausing(),
                verify_progress: doc.section_verify_progress,
                verify_status: doc.section_verify_status.clone(),
                pending_verified_reopen: doc.pending_verified_reopen,
                can_verify_boundary: doc.can_verify_section_boundary(),
                speculative_save_warning: doc.speculative_save_warning(),
            })
        });
        Some(DocumentSnapshot {
            document_id: doc_id,
            instance_id: doc.instance_id,
            path: doc.logical_path.clone(),
            filename,
            file_size: doc.session.file_size,
            active_byte_len,
            section,
            delimiter: doc.session.dialect.delimiter,
            has_header: doc.session.dialect.has_header,
            encoding: doc.session.encoding(),
            headers,
            shown_columns: doc.columns.visible.clone(),
            total_rows: doc.available_data_rows(),
            available_rows: doc.grid_total_rows(),
            filter_active,
            filter_done,
            filter_running: doc.filter_job.is_some(),
            filter_status: doc.filter_status.clone(),
            can_export_filtered: doc.is_filtered_export_ready(),
            export_running: doc.export_job.is_some(),
            export_status: doc.export_status.clone(),
            search_running: doc.search_job.is_some(),
            search_status: doc.search_status.clone(),
            structural_running,
            structural_status: doc.structural_status.clone(),
            structural_progress,
            duplicate_preview,
            pending_split_confirm_bytes,
            selected_rows_count: doc.selected_rows.count(),
            selected_columns: doc.selected_columns.iter().copied().collect(),
            index_done: doc.job.is_none() && doc.progress.done,
            index_paused: doc.is_indexing_paused(),
            index_pausing: doc.is_indexing_pausing(),
            index_fraction: if active_byte_len == 0 {
                1.0
            } else {
                (doc.progress.bytes_scanned as f32 / active_byte_len as f32).clamp(0.0, 1.0)
            },
            index_committed_bytes: doc.progress.bytes_scanned,
            index_bytes_read: doc.progress.bytes_read,
            index_committed_rows: doc.committed_data_rows(),
            can_pause_indexing: doc.can_pause_indexing(),
            can_resume_indexing: doc.can_resume_indexing(),
            dirty: doc.is_dirty(),
            saving: doc.save_job.is_some(),
            busy: tab.document_writing(),
            can_undo: doc.can_undo(),
            can_redo: doc.can_redo(),
            source_changed: doc.source_changed,
            applied_delimiter: tab.delimiter_mode.delimiter(),
            applied_header_mode: tab.header_mode,
            applied_encoding: tab.encoding_mode,
            is_compressed: doc.is_compressed(),
            compressed_label: doc.compressed_label(),
            decompression_status: decompression_status(doc),
            compressed_pending: doc.awaiting_decompression(),
            compressed_preview: doc.compressed_is_preview(),
            compressed_unreadable_text: doc.compressed_unreadable_text(),
            join_reserved: doc.join_reserved(),
            has_diagnostic: doc.pending_diagnostic.is_some(),
        })
    }

    /// Opens a bounded byte section of `path` as a distinct tab in `target_window`.
    pub fn open_section(
        &mut self,
        target_window: WindowId,
        path: PathBuf,
        byte_window: ByteWindow,
        options: SectionOpenOptions,
        cx: &mut Context<Self>,
    ) -> Result<DocumentId, AppMessage> {
        if self.editing_windows.contains(&target_window) {
            return Err(AppMessage::warning(
                "Apply or cancel the open edit before opening a section in this window.",
            ));
        }
        let outcome = self
            .scheduler
            .open_section(target_window, path.clone(), byte_window, options)
            .map_err(|err| match err {
                SchedulerError::OpenFailed(message) => AppMessage::error(message),
                other => AppMessage::warning(other.to_string()),
            })?;
        let doc_id = match outcome {
            WakeOutcome::Ready { document_id, .. }
            | WakeOutcome::SourceChangedConflict { document_id, .. } => document_id,
        };
        self.window_messages.remove(&target_window);
        cx.notify();
        Ok(doc_id)
    }

    pub fn start_verify_section_boundary_active(
        &mut self,
        window: WindowId,
        cx: &mut Context<Self>,
    ) -> Result<(), AppMessage> {
        let Some(id) = self.scheduler.active_document(window) else {
            return Err(AppMessage::warning("Open a section first."));
        };
        self.scheduler.start_verify_section_boundary(id)?;
        cx.notify();
        Ok(())
    }

    pub fn pause_verify_section_boundary_active(
        &mut self,
        window: WindowId,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(id) = self.scheduler.active_document(window) else {
            return Err("Open a section first.".into());
        };
        self.scheduler.pause_verify_section_boundary(id)?;
        cx.notify();
        Ok(())
    }

    pub fn resume_verify_section_boundary_active(
        &mut self,
        window: WindowId,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(id) = self.scheduler.active_document(window) else {
            return Err("Open a section first.".into());
        };
        self.scheduler.resume_verify_section_boundary(id)?;
        cx.notify();
        Ok(())
    }

    pub fn cancel_verify_section_boundary_active(
        &mut self,
        window: WindowId,
        cx: &mut Context<Self>,
    ) {
        if let Some(id) = self.scheduler.active_document(window) {
            self.scheduler.cancel_verify_section_boundary(id);
            cx.notify();
        }
    }

    pub fn reopen_verified_section_active(
        &mut self,
        window: WindowId,
        discard_unsaved: bool,
        cx: &mut Context<Self>,
    ) -> Result<String, AppMessage> {
        let Some(id) = self.scheduler.active_document(window) else {
            return Err(AppMessage::warning("Open a section first."));
        };
        let notice = self
            .scheduler
            .reopen_verified_section(id, discard_unsaved)?;
        self.viewport_read_seq.remove(&id);
        self.pending_reveal_cells.remove(&id);
        cx.notify();
        Ok(notice)
    }

    /// Opens one or more paths into `target_window`, routing existing hard-link/canonical aliases
    /// back to their owning window without duplicating documents.
    pub fn open_paths(
        &mut self,
        target_window: WindowId,
        paths: Vec<PathBuf>,
        options: OpenOptions,
        encoding: Option<Encoding>,
        cx: &mut Context<Self>,
    ) -> Result<(WindowId, DocumentId), AppMessage> {
        if paths.is_empty() {
            return Err(AppMessage::warning("No file selected."));
        }
        if paths.len() == 1 {
            let path = paths.into_iter().next().unwrap();
            let existing_owner = self.scheduler.find_owner_for_path(&path);
            if let Some((owner_win, doc_id)) = existing_owner {
                if self.editing_windows.contains(&owner_win)
                    && self.scheduler.active_document(owner_win) != Some(doc_id)
                {
                    if let Some(handle) = self.window_handle(owner_win) {
                        let _ = handle.update(cx, |_, window, _| window.activate_window());
                    }
                    return Err(AppMessage::warning(
                        "Apply or cancel the open edit in the owning window before switching tabs.",
                    ));
                }
            } else if self.editing_windows.contains(&target_window) {
                return Err(AppMessage::warning(
                    "Apply or cancel the open edit before opening another file in this window.",
                ));
            }
            let outcome = self
                .scheduler
                .open_path(target_window, path.clone(), options, encoding)
                .map_err(|err| match err {
                    SchedulerError::OpenFailed(message) => {
                        AppMessage::file_open_error(path, options, encoding, message)
                    }
                    other => AppMessage::warning(other.to_string()),
                })?;
            let (doc_id, owner_window) = match outcome {
                WakeOutcome::Ready { document_id, .. }
                | WakeOutcome::SourceChangedConflict { document_id, .. } => {
                    let owner = existing_owner
                        .map(|(w, _)| w)
                        .or_else(|| self.scheduler.window_of(document_id))
                        .unwrap_or(target_window);
                    (document_id, owner)
                }
            };
            self.window_messages.remove(&target_window);
            if owner_window != target_window
                && let Some(handle) = self.window_handle(owner_window)
            {
                let _ = handle.update(cx, |_, window, _| window.activate_window());
            }
            let _ = self.sync_resource_hud();
            cx.notify();
            return Ok((owner_window, doc_id));
        }

        if self.editing_windows.contains(&target_window) {
            return Err(AppMessage::warning(
                "Apply or cancel the open edit before opening additional files in this window.",
            ));
        }

        let first_local_path = paths
            .iter()
            .find(|p| {
                self.scheduler
                    .find_owner_for_path(p)
                    .is_none_or(|(owner, _)| owner == target_window)
            })
            .cloned();
        let ids = self
            .scheduler
            .open_many_lazy(target_window, paths, options, encoding)
            .map_err(|err| match err {
                SchedulerError::OpenFailed(message) => {
                    if let Some(failed_path) = first_local_path {
                        AppMessage::file_open_error(failed_path, options, encoding, message)
                    } else {
                        AppMessage::error(message)
                    }
                }
                other => AppMessage::warning(other.to_string()),
            })?;
        let local_active = self
            .scheduler
            .active_document(target_window)
            .filter(|id| ids.contains(id) && self.scheduler.window_of(*id) == Some(target_window));
        let (owner_window, active_id) = if let Some(local_id) = local_active {
            (target_window, local_id)
        } else {
            let first_id = ids
                .first()
                .copied()
                .ok_or_else(|| AppMessage::warning("No files were opened."))?;
            let owner = self.scheduler.window_of(first_id).unwrap_or(target_window);
            if !self.editing_windows.contains(&owner) {
                let _ = self.scheduler.activate_tab(owner, first_id);
            }
            if owner != target_window
                && let Some(handle) = self.window_handle(owner)
            {
                let _ = handle.update(cx, |_, window, _| window.activate_window());
            }
            (owner, first_id)
        };
        self.window_messages.remove(&target_window);
        cx.notify();
        Ok((owner_window, active_id))
    }

    pub fn activate_tab(
        &mut self,
        window: WindowId,
        id: DocumentId,
        cx: &mut Context<Self>,
    ) -> Result<WakeOutcome, String> {
        let outcome = self
            .scheduler
            .activate_tab(window, id)
            .map_err(|e| e.to_string())?;
        cx.notify();
        Ok(outcome)
    }

    pub fn cycle_tab(&mut self, window: WindowId, delta: isize, cx: &mut Context<Self>) {
        let ids = self.scheduler.tabs_in_window(window);
        if ids.len() <= 1 {
            return;
        }
        let current = self
            .scheduler
            .active_document(window)
            .and_then(|active| ids.iter().position(|&id| id == active))
            .unwrap_or(0);
        let len = ids.len() as isize;
        let next_index = (current as isize + delta).rem_euclid(len) as usize;
        let target = ids[next_index];
        if let Err(error) = self.scheduler.activate_tab(window, target) {
            self.set_window_message(window, Some(AppMessage::warning(error.to_string())), cx);
        } else {
            cx.notify();
        }
    }

    pub fn move_tab_to_window(
        &mut self,
        id: DocumentId,
        target_window: WindowId,
        cx: &mut Context<Self>,
    ) -> Result<WakeOutcome, String> {
        let outcome = self
            .scheduler
            .move_tab_to_window(id, target_window)
            .map_err(|e| e.to_string())?;
        cx.notify();
        Ok(outcome)
    }

    pub fn reopen_active_tab(
        &mut self,
        window: WindowId,
        options: OpenOptions,
        encoding: Option<Encoding>,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(id) = self.scheduler.active_document(window) else {
            return Err("Open a file first.".into());
        };
        let is_compressed = self
            .scheduler
            .tab(id)
            .and_then(|t| t.document.as_ref())
            .is_some_and(Document::is_compressed)
            || self
                .scheduler
                .tab_logical_path(id)
                .is_some_and(|p| quarry_workbench::compressed::is_gzip(&p));
        if is_compressed && !self.write_slot_available() {
            return Err(quarry_workbench::compressed::WRITE_SLOTS_FULL.into());
        }
        self.scheduler
            .reopen_tab(window, id, options, encoding)
            .map_err(|e| e.to_string())?;
        if let Some(tab) = self.scheduler.tab_mut(id) {
            tab.delimiter_mode = match options.delimiter {
                None => quarry_workbench::DelimiterMode::Auto,
                Some(b',') => quarry_workbench::DelimiterMode::Comma,
                Some(b'\t') => quarry_workbench::DelimiterMode::Tab,
                Some(b'|') => quarry_workbench::DelimiterMode::Pipe,
                Some(b';') => quarry_workbench::DelimiterMode::Semicolon,
                Some(_) => quarry_workbench::DelimiterMode::Auto,
            };
        }
        self.viewport_read_seq.remove(&id);
        self.pending_reveal_cells.remove(&id);
        cx.notify();
        Ok(())
    }

    pub fn reload_active_tab(
        &mut self,
        window: WindowId,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(id) = self.scheduler.active_document(window) else {
            return Err("Open a file first.".into());
        };
        let (options, encoding, is_compressed) = {
            let tab = self
                .scheduler
                .tab(id)
                .ok_or_else(|| "Open a file first.".to_string())?;
            let options = tab
                .document
                .as_ref()
                .map_or_else(|| tab.open_options(), Document::current_open_options);
            let compressed = tab.document.as_ref().is_some_and(Document::is_compressed)
                || self
                    .scheduler
                    .tab_logical_path(id)
                    .is_some_and(|p| quarry_workbench::compressed::is_gzip(&p));
            (options, tab.encoding_mode, compressed)
        };
        if is_compressed && !self.write_slot_available() {
            return Err(quarry_workbench::compressed::WRITE_SLOTS_FULL.into());
        }
        self.scheduler
            .reopen_tab(window, id, options, encoding)
            .map_err(|e| e.to_string())?;
        cx.notify();
        Ok(())
    }

    pub fn start_filter_active(
        &mut self,
        window: WindowId,
        query: FilterQuery,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(id) = self.scheduler.active_document(window) else {
            return Err("Open a file first.".into());
        };
        self.scheduler
            .start_filter(id, query)
            .map_err(|e| e.to_string())?;
        cx.notify();
        Ok(())
    }

    pub fn clear_filter_active(
        &mut self,
        window: WindowId,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(doc) = self.active_document_mut(window) else {
            return Ok(());
        };
        doc.clear_filter()?;
        let _ = self.scheduler.sync_fd_leases();
        cx.notify();
        Ok(())
    }

    pub fn set_active_shown_columns(
        &mut self,
        window: WindowId,
        shown: Vec<usize>,
        cx: &mut Context<Self>,
    ) {
        if let Some(doc) = self.active_document_mut(window) {
            let total = doc.columns.hidden.len();
            let mut is_shown = vec![false; total];
            let mut next_order = Vec::with_capacity(total);
            for col in shown {
                if col < total && !is_shown[col] {
                    is_shown[col] = true;
                    next_order.push(col);
                }
            }
            for &col in &doc.columns.order {
                if col < total && !is_shown[col] {
                    next_order.push(col);
                }
            }
            doc.columns.order = next_order;
            doc.columns.hidden = is_shown.iter().map(|&flag| !flag).collect();
            doc.columns.shown = is_shown.iter().filter(|&&flag| flag).count();
            doc.columns.refresh();
            doc.clear_hidden_selection();
            cx.notify();
        }
    }

    pub fn set_active_selection(
        &mut self,
        window: WindowId,
        selection: Option<quarry_workbench::GridSelection>,
    ) {
        if let Some(doc) = self.active_document_mut(window) {
            doc.selection = selection;
        }
    }

    pub fn undo_redo_active(
        &mut self,
        window: WindowId,
        redo: bool,
        cx: &mut Context<Self>,
    ) -> Result<String, AppMessage> {
        let Some(id) = self.scheduler.active_document(window) else {
            return Err(AppMessage::warning("Open a file first."));
        };
        let result = self.scheduler.apply_undo_redo(id, redo);
        if result.is_ok() {
            self.pending_reveal_cells.remove(&id);
        }
        cx.notify();
        result
    }

    pub fn discard_active_edits(
        &mut self,
        window: WindowId,
        cx: &mut Context<Self>,
    ) -> Result<(), AppMessage> {
        let Some(id) = self.scheduler.active_document(window) else {
            return Err(AppMessage::warning("Open a file first."));
        };
        let result = self.scheduler.discard_tab_edits(id);
        if result.is_ok() {
            self.pending_reveal_cells.remove(&id);
        }
        cx.notify();
        result
    }

    pub fn apply_cell_edit(
        &mut self,
        window: WindowId,
        expected_doc_id: DocumentId,
        source_row: u64,
        source_col: usize,
        value: String,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if value.len() > MAX_EDIT_BYTES {
            return Err(
                "Cell values must be 1 MiB or smaller. Shorten the value to apply it.".into(),
            );
        }
        if self.scheduler.active_document(window) != Some(expected_doc_id) {
            return Err(
                "The active document changed before this edit was applied. Switch back to the original tab or cancel."
                    .into(),
            );
        }
        let Some(doc) = self.active_document_mut(window) else {
            return Err("Open a file first.".into());
        };
        let source = doc
            .source_cell(source_row, source_col)
            .map(ToOwned::to_owned)
            .unwrap_or_default();
        doc.begin_cell_edit(source_row, source_col, source)?;
        if let Some(edit) = doc.cell_edit.as_mut() {
            edit.draft = value;
        }
        doc.commit_cell_edit();
        if let Some(tab) = self.scheduler.tab_mut(expected_doc_id) {
            tab.notice = None;
        }
        self.scheduler.revoke_tab_close_approval(expected_doc_id);
        let _ = self.scheduler.sync_fd_leases();
        cx.notify();
        Ok(())
    }

    pub fn rename_header_active(
        &mut self,
        window: WindowId,
        expected_doc_id: DocumentId,
        source_col: usize,
        name: String,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        if self.scheduler.active_document(window) != Some(expected_doc_id) {
            return Err(
                "The active document changed before this header rename was applied. Switch back to the original tab or cancel."
                    .into(),
            );
        }
        let Some(doc) = self.active_document_mut(window) else {
            return Err("Open a file first.".into());
        };
        doc.rename_header(source_col, name)?;
        self.scheduler.revoke_tab_close_approval(expected_doc_id);
        let _ = self.scheduler.sync_fd_leases();
        cx.notify();
        Ok(())
    }

    pub fn copy_active_selection(&self, window: WindowId) -> Result<String, String> {
        let Some(doc) = self.active_document(window) else {
            return Err("Open a file first.".into());
        };
        doc.copy_selection_text()
    }

    pub fn start_save_active(
        &mut self,
        window: WindowId,
        expected_doc: Option<DocumentId>,
        destination: Option<PathBuf>,
        encoding: Option<Encoding>,
        cx: &mut Context<Self>,
    ) -> Result<(), AppMessage> {
        let Some(active_id) = self.scheduler.active_document(window) else {
            return Err(AppMessage::warning("Open a file first."));
        };
        if let Some(expected) = expected_doc
            && active_id != expected
        {
            return Err(AppMessage::warning(
                "The active document changed before saving started. Try Save As again.",
            ));
        }
        self.start_save_document(active_id, destination, encoding, cx)
    }

    pub fn start_save_document(
        &mut self,
        id: DocumentId,
        destination: Option<PathBuf>,
        encoding: Option<Encoding>,
        cx: &mut Context<Self>,
    ) -> Result<(), AppMessage> {
        if !self.write_slot_available() {
            return Err(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish.",
            ));
        }
        if let Some(dest) = destination.as_ref() {
            if let Some(warning) = gzip_destination_warning(dest) {
                return Err(warning);
            }
            let doc_path = self.scheduler.tab_logical_path(id);
            let aliases_other =
                self.path_reserved(dest) && doc_path.as_ref().is_none_or(|p| p != dest);
            if aliases_other {
                return Err(AppMessage::warning(
                    "That destination is already open or reserved in another tab.",
                ));
            }
        }
        let target_dest = destination.clone();
        self.scheduler.with_reserved_job_fds(
            id,
            2,
            false,
            |doc| {
                doc.commit_edits();
                if encoding == Some(Encoding::Utf8) {
                    doc.start_save_operation_format(destination, true)
                } else {
                    doc.start_save_operation(destination)
                }
            },
            |err| AppMessage::warning(err.to_string()),
        )?;
        if let Some(tab) = self.scheduler.tab_mut(id) {
            tab.write_destination = target_dest;
            tab.notice = None;
        }
        cx.notify();
        Ok(())
    }

    fn allocate_review_token(&mut self) -> u64 {
        let token = self.next_review_token;
        self.next_review_token = self.next_review_token.wrapping_add(1).max(1);
        self.active_review_token = Some(token);
        token
    }

    fn overlapping_review_wait(&self, cx: &mut Context<Self>) -> Option<CloseReviewAction> {
        let review = self.pending_review.as_ref()?;
        if let Some(win) = review.current_window()
            && let Some(handle) = self.window_handle(win)
        {
            let _ = handle.update(cx, |_, window, _| window.activate_window());
        }
        Some(CloseReviewAction::Wait {
            message:
                "Finish the open save confirmation dialog before closing another tab or window."
                    .into(),
        })
    }

    /// Starts a single-tab close review. Closes immediately if clean & idle; otherwise asks the
    /// window to prompt Save / Discard / Keep Editing.
    pub fn request_close_tab(
        &mut self,
        window: WindowId,
        id: DocumentId,
        close_window_if_last: bool,
        cx: &mut Context<Self>,
    ) -> CloseReviewAction {
        if let Some(wait) = self.overlapping_review_wait(cx) {
            return wait;
        }
        if self.editing_windows.contains(&window)
            && self.scheduler.active_document(window) == Some(id)
        {
            return CloseReviewAction::Wait {
                message: "Apply or cancel the open edit before closing this tab.".into(),
            };
        }
        if let Some(tab) = self.scheduler.tab(id)
            && tab.document.as_ref().is_some_and(Document::join_reserved)
        {
            return CloseReviewAction::Wait {
                message: JOIN_INPUT_RESERVED.into(),
            };
        }
        if let Some(tab) = self.scheduler.tab(id)
            && tab.document_writing()
        {
            return CloseReviewAction::Wait {
                message: "Wait for the active file operation to finish before closing.".into(),
            };
        }
        if self.scheduler.tab_is_dirty(id) {
            if let Err(error) = self.scheduler.activate_tab(window, id) {
                return CloseReviewAction::Wait {
                    message: error.to_string(),
                };
            }
            if let Some(tab) = self.scheduler.tab_mut(id) {
                tab.close_confirmation_open = true;
            }
            let filename = self
                .scheduler
                .tab_display_name(id)
                .unwrap_or_else(|| "Untitled".into());
            let review_token = self.allocate_review_token();
            self.pending_review = Some(PendingReviewScope::SingleTab {
                window_id: window,
                document_id: id,
                close_window_if_last,
            });
            cx.notify();
            return CloseReviewAction::ConfirmTab {
                review_token,
                window_id: window,
                document_id: id,
                filename,
            };
        }

        let _ = self.scheduler.close_tab(id);
        self.viewport_read_seq.remove(&id);
        self.pending_reveal_cells.remove(&id);
        let empty_now = self.scheduler.tabs_in_window(window).is_empty();
        if empty_now && close_window_if_last && self.scheduler.close_window(window).is_ok() {
            self.pending_close_windows.push(window);
        }
        cx.notify();
        CloseReviewAction::Proceed
    }

    /// Starts a window close review for all dirty tabs owned by `window`.
    pub fn request_close_window(
        &mut self,
        window: WindowId,
        cx: &mut Context<Self>,
    ) -> CloseReviewAction {
        if let Some(wait) = self.overlapping_review_wait(cx) {
            return wait;
        }
        if self.editing_windows.contains(&window) {
            return CloseReviewAction::Wait {
                message: "Apply or cancel the open edit before closing this window.".into(),
            };
        }
        let ids = self.scheduler.tabs_in_window(window);
        if ids.iter().any(|&id| {
            self.scheduler
                .tab(id)
                .and_then(|t| t.document.as_ref())
                .is_some_and(Document::join_reserved)
        }) {
            return CloseReviewAction::Wait {
                message: JOIN_INPUT_RESERVED.into(),
            };
        }
        if ids.iter().any(|&id| {
            self.scheduler
                .tab(id)
                .is_some_and(WorkbenchTab::document_writing)
        }) {
            return CloseReviewAction::Wait {
                message: "Wait for active file operations in this window to finish before closing."
                    .into(),
            };
        }
        let dirty = self.scheduler.dirty_tabs_in_window(window);
        if let Some(&first_dirty) = dirty.first() {
            if let Err(error) = self.scheduler.activate_tab(window, first_dirty) {
                return CloseReviewAction::Wait {
                    message: error.to_string(),
                };
            }
            if let Some(tab) = self.scheduler.tab_mut(first_dirty) {
                tab.close_confirmation_open = true;
            }
            let filename = self
                .scheduler
                .tab_display_name(first_dirty)
                .unwrap_or_else(|| "Untitled".into());
            let review_token = self.allocate_review_token();
            self.pending_review = Some(PendingReviewScope::Window {
                window_id: window,
                remaining_dirty: dirty,
                approved_dirty: Vec::new(),
            });
            cx.notify();
            return CloseReviewAction::ConfirmTab {
                review_token,
                window_id: window,
                document_id: first_dirty,
                filename,
            };
        }
        if self.scheduler.close_window(window).is_ok() {
            self.pending_close_windows.push(window);
            cx.notify();
            return CloseReviewAction::Proceed;
        }
        cx.notify();
        CloseReviewAction::Wait {
            message: "Modified documents remain unapproved before closing this window.".into(),
        }
    }

    /// Starts unified cross-window Quit review across every open window and dormant dirty tab.
    pub fn request_quit(&mut self, cx: &mut Context<Self>) -> CloseReviewAction {
        if let Some(wait) = self.overlapping_review_wait(cx) {
            return wait;
        }
        if let Some(&editing_window) = self.editing_windows.first() {
            if let Some(handle) = self.window_handle(editing_window) {
                let _ = handle.update(cx, |_, window, _| window.activate_window());
            }
            return CloseReviewAction::Wait {
                message: "Apply or cancel open edits before quitting Quarry.".into(),
            };
        }
        if self.combine.busy() || self.join.busy() || self.inspect.busy() {
            return CloseReviewAction::Wait {
                message: "Wait for active Combine, Join, or Inspect operations to finish before quitting."
                    .into(),
            };
        }
        for window in self.scheduler.windows() {
            for id in self.scheduler.tabs_in_window(window) {
                if self
                    .scheduler
                    .tab(id)
                    .is_some_and(WorkbenchTab::document_writing)
                {
                    return CloseReviewAction::Wait {
                        message: "Wait for active file operations to finish before quitting."
                            .into(),
                    };
                }
            }
        }
        self.scheduler.begin_quit_review();
        let dirty = self.scheduler.dirty_tabs();
        if let Some(&(first_window, first_doc)) = dirty.first() {
            if let Err(error) = self.scheduler.activate_tab(first_window, first_doc) {
                self.scheduler.cancel_quit();
                return CloseReviewAction::Wait {
                    message: error.to_string(),
                };
            }
            if let Some(tab) = self.scheduler.tab_mut(first_doc) {
                tab.close_confirmation_open = true;
            }
            let filename = self
                .scheduler
                .tab_display_name(first_doc)
                .unwrap_or_else(|| "Untitled".into());
            let review_token = self.allocate_review_token();
            self.pending_review = Some(PendingReviewScope::Quit {
                remaining_dirty: dirty,
            });
            cx.notify();
            return CloseReviewAction::ConfirmTab {
                review_token,
                window_id: first_window,
                document_id: first_doc,
                filename,
            };
        }

        let _ = self.scheduler.finalize_quit();
        self.quit_requested_complete = true;
        cx.notify();
        CloseReviewAction::Proceed
    }

    /// Cancels an active tab, window, or quit review (Keep Editing), preserving all state.
    pub fn cancel_close_review(&mut self, expected_token: Option<u64>, cx: &mut Context<Self>) {
        if let Some(token) = expected_token
            && self.active_review_token != Some(token)
        {
            return;
        }
        self.active_review_token = None;
        self.pending_review_action = None;
        if let Some(review) = self.pending_review.take() {
            match review {
                PendingReviewScope::SingleTab { document_id, .. } => {
                    self.scheduler.revoke_tab_close_approval(document_id);
                }
                PendingReviewScope::Window {
                    remaining_dirty,
                    approved_dirty,
                    ..
                } => {
                    for id in remaining_dirty.into_iter().chain(approved_dirty) {
                        self.scheduler.revoke_tab_close_approval(id);
                    }
                }
                PendingReviewScope::Quit { .. } => {
                    self.scheduler.cancel_quit();
                }
            }
        }
        cx.notify();
    }

    /// Approves discarding edits on the current tab under review and advances to the next dirty
    /// tab (or finishes closing the tab / window / app).
    pub fn approve_discard_in_review(
        &mut self,
        expected_token: u64,
        cx: &mut Context<Self>,
    ) -> CloseReviewAction {
        if self.active_review_token != Some(expected_token) {
            return CloseReviewAction::Proceed;
        }
        self.active_review_token = None;
        let Some(review) = self.pending_review.take() else {
            return CloseReviewAction::Proceed;
        };
        match review {
            PendingReviewScope::SingleTab {
                window_id,
                document_id,
                close_window_if_last,
            } => {
                if self.editing_windows.contains(&window_id)
                    && self.scheduler.active_document(window_id) == Some(document_id)
                {
                    self.scheduler.revoke_tab_close_approval(document_id);
                    cx.notify();
                    return CloseReviewAction::Wait {
                        message: "Apply or cancel the open edit before closing this tab.".into(),
                    };
                }
                let _ = self.scheduler.approve_tab_for_close(document_id);
                let _ = self.scheduler.close_tab(document_id);
                self.viewport_read_seq.remove(&document_id);
                self.pending_reveal_cells.remove(&document_id);
                if close_window_if_last
                    && self.scheduler.tabs_in_window(window_id).is_empty()
                    && self.scheduler.close_window(window_id).is_ok()
                {
                    self.pending_close_windows.push(window_id);
                }
                cx.notify();
                CloseReviewAction::Proceed
            }
            PendingReviewScope::Window {
                window_id,
                mut remaining_dirty,
                mut approved_dirty,
            } => {
                if !remaining_dirty.is_empty() {
                    let approved = remaining_dirty.remove(0);
                    let _ = self.scheduler.approve_tab_for_close(approved);
                    approved_dirty.push(approved);
                }
                if let Some(&next_id) = remaining_dirty.first() {
                    if let Err(error) = self.scheduler.activate_tab(window_id, next_id) {
                        for id in remaining_dirty.into_iter().chain(approved_dirty) {
                            self.scheduler.revoke_tab_close_approval(id);
                        }
                        cx.notify();
                        return CloseReviewAction::Wait {
                            message: error.to_string(),
                        };
                    }
                    if let Some(tab) = self.scheduler.tab_mut(next_id) {
                        tab.close_confirmation_open = true;
                    }
                    let filename = self
                        .scheduler
                        .tab_display_name(next_id)
                        .unwrap_or_else(|| "Untitled".into());
                    let review_token = self.allocate_review_token();
                    self.pending_review = Some(PendingReviewScope::Window {
                        window_id,
                        remaining_dirty,
                        approved_dirty,
                    });
                    cx.notify();
                    CloseReviewAction::ConfirmTab {
                        review_token,
                        window_id,
                        document_id: next_id,
                        filename,
                    }
                } else {
                    if self.editing_windows.contains(&window_id) {
                        for id in approved_dirty {
                            self.scheduler.revoke_tab_close_approval(id);
                        }
                        cx.notify();
                        return CloseReviewAction::Wait {
                            message: "Apply or cancel the open edit before closing this window."
                                .into(),
                        };
                    }
                    match self.scheduler.close_window(window_id) {
                        Ok(_) => {
                            self.pending_close_windows.push(window_id);
                            cx.notify();
                            CloseReviewAction::Proceed
                        }
                        Err(SchedulerError::UnapprovedDirtyTabs(newly_dirty)) => {
                            if let Some(&next_id) = newly_dirty.first() {
                                if let Err(error) = self.scheduler.activate_tab(window_id, next_id)
                                {
                                    for id in newly_dirty.into_iter().chain(approved_dirty) {
                                        self.scheduler.revoke_tab_close_approval(id);
                                    }
                                    cx.notify();
                                    return CloseReviewAction::Wait {
                                        message: error.to_string(),
                                    };
                                }
                                if let Some(tab) = self.scheduler.tab_mut(next_id) {
                                    tab.close_confirmation_open = true;
                                }
                                let filename = self
                                    .scheduler
                                    .tab_display_name(next_id)
                                    .unwrap_or_else(|| "Untitled".into());
                                let review_token = self.allocate_review_token();
                                self.pending_review = Some(PendingReviewScope::Window {
                                    window_id,
                                    remaining_dirty: newly_dirty,
                                    approved_dirty,
                                });
                                cx.notify();
                                CloseReviewAction::ConfirmTab {
                                    review_token,
                                    window_id,
                                    document_id: next_id,
                                    filename,
                                }
                            } else {
                                for id in approved_dirty {
                                    self.scheduler.revoke_tab_close_approval(id);
                                }
                                cx.notify();
                                CloseReviewAction::Wait {
                                    message: "Modified documents remain unapproved before closing this window.".into(),
                                }
                            }
                        }
                        Err(error) => {
                            for id in approved_dirty {
                                self.scheduler.revoke_tab_close_approval(id);
                            }
                            cx.notify();
                            CloseReviewAction::Wait {
                                message: error.to_string(),
                            }
                        }
                    }
                }
            }
            PendingReviewScope::Quit {
                mut remaining_dirty,
            } => {
                if !remaining_dirty.is_empty() {
                    let (_, approved) = remaining_dirty.remove(0);
                    if let Some(tab) = self.scheduler.tab_mut(approved) {
                        tab.close_confirmation_open = false;
                    }
                    let _ = self.scheduler.approve_tab_for_quit(approved);
                }
                if let Some(&(next_window, next_doc)) = remaining_dirty.first() {
                    if let Err(error) = self.scheduler.activate_tab(next_window, next_doc) {
                        self.scheduler.cancel_quit();
                        return CloseReviewAction::Wait {
                            message: error.to_string(),
                        };
                    }
                    if let Some(tab) = self.scheduler.tab_mut(next_doc) {
                        tab.close_confirmation_open = true;
                    }
                    let filename = self
                        .scheduler
                        .tab_display_name(next_doc)
                        .unwrap_or_else(|| "Untitled".into());
                    let review_token = self.allocate_review_token();
                    self.pending_review = Some(PendingReviewScope::Quit { remaining_dirty });
                    cx.notify();
                    CloseReviewAction::ConfirmTab {
                        review_token,
                        window_id: next_window,
                        document_id: next_doc,
                        filename,
                    }
                } else {
                    if let Some(&editing_window) = self.editing_windows.first() {
                        self.scheduler.cancel_quit();
                        if let Some(handle) = self.window_handle(editing_window) {
                            let _ = handle.update(cx, |_, window, _| window.activate_window());
                        }
                        cx.notify();
                        return CloseReviewAction::Wait {
                            message: "Apply or cancel open edits before quitting Quarry.".into(),
                        };
                    }
                    for win in self.scheduler.windows() {
                        for id in self.scheduler.tabs_in_window(win) {
                            if self
                                .scheduler
                                .tab(id)
                                .is_some_and(WorkbenchTab::document_writing)
                            {
                                self.scheduler.cancel_quit();
                                cx.notify();
                                return CloseReviewAction::Wait {
                                    message:
                                        "Wait for active file operations to finish before quitting."
                                            .into(),
                                };
                            }
                        }
                    }
                    match self.scheduler.finalize_quit() {
                        Ok(_) => {
                            self.quit_requested_complete = true;
                            cx.notify();
                            CloseReviewAction::Proceed
                        }
                        Err(SchedulerError::UnapprovedDirtyTabs(_)) => {
                            self.scheduler.cancel_quit();
                            cx.notify();
                            CloseReviewAction::Wait {
                                message: "Modified documents remain unapproved for quit.".into(),
                            }
                        }
                        Err(error) => CloseReviewAction::Wait {
                            message: error.to_string(),
                        },
                    }
                }
            }
        }
    }

    /// Starts saving the specific reviewed dirty tab `document_id` and marks it to finish closing
    /// once saved. On startup failure (e.g., write slots busy), restores `close_confirmation_open`
    /// and preserves `pending_review` so the confirmation remains retryable.
    pub fn save_in_review(
        &mut self,
        expected_token: u64,
        document_id: DocumentId,
        cx: &mut Context<Self>,
    ) -> Result<(), AppMessage> {
        if self.active_review_token != Some(expected_token) {
            return Ok(());
        }
        if self
            .pending_review
            .as_ref()
            .and_then(PendingReviewScope::current_document)
            != Some(document_id)
        {
            return Err(AppMessage::warning(
                "The reviewed document is no longer active in this confirmation.",
            ));
        }
        if let Some(tab) = self.scheduler.tab_mut(document_id) {
            tab.close_confirmation_open = false;
            tab.close_after_save = true;
        }
        if let Err(error) = self.start_save_document(document_id, None, None, cx) {
            if let Some(tab) = self.scheduler.tab_mut(document_id) {
                tab.close_after_save = false;
                tab.close_confirmation_open = true;
            }
            cx.notify();
            return Err(error);
        }
        Ok(())
    }

    /// Routes a `ConfirmTab` (or `Wait`) review action to its owning window, activating that
    /// window so its own `sync_from_controller` presents the confirmation dialog in the foreground.
    pub fn queue_review_action(&mut self, action: CloseReviewAction, cx: &mut Context<Self>) {
        if let CloseReviewAction::ConfirmTab { window_id, .. } = &action
            && let Some(handle) = self.window_handle(*window_id)
        {
            let _ = handle.update(cx, |_, window, _| window.activate_window());
        }
        self.pending_review_action = Some(action);
        cx.notify();
    }

    pub fn take_pending_review_action(&mut self, window: WindowId) -> Option<CloseReviewAction> {
        let should_take = match self.pending_review_action.as_ref() {
            Some(CloseReviewAction::ConfirmTab { window_id, .. }) => {
                *window_id == window || !self.windows.contains_key(window_id)
            }
            Some(CloseReviewAction::Wait { .. }) | Some(CloseReviewAction::Proceed) => true,
            None => false,
        };
        if should_take {
            self.pending_review_action.take()
        } else {
            None
        }
    }

    pub fn take_pending_close_windows(&mut self) -> Vec<WindowId> {
        std::mem::take(&mut self.pending_close_windows)
    }

    pub fn take_quit_complete(&mut self) -> bool {
        std::mem::take(&mut self.quit_requested_complete)
    }

    pub fn take_pending_reveal_cell(&mut self, window: WindowId) -> Option<(u64, usize)> {
        let active_doc = self.scheduler.active_document(window)?;
        self.pending_reveal_cells.remove(&active_doc)
    }

    #[cfg(test)]
    pub(crate) fn queue_pending_reveal_cell_for_test(
        &mut self,
        doc_id: DocumentId,
        cell: (u64, usize),
    ) {
        self.pending_reveal_cells.insert(doc_id, cell);
    }

    pub fn select_row_active(
        &mut self,
        window: WindowId,
        source_row: u64,
        shift: bool,
        toggle: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(doc) = self.active_document_mut(window) {
            if doc.filter_active() {
                return;
            }
            if !shift
                && !toggle
                && doc.selected_rows.contains(source_row)
                && doc.selected_rows.count() == 1
            {
                doc.selected_rows.clear();
            } else {
                doc.selected_rows.select(
                    source_row,
                    SelectionModifiers {
                        shift,
                        command: toggle,
                    },
                );
            }
            cx.notify();
        }
    }

    pub fn select_column_active(
        &mut self,
        window: WindowId,
        source_col: usize,
        shift: bool,
        toggle: bool,
        cx: &mut Context<Self>,
    ) {
        if let Some(doc) = self.active_document_mut(window) {
            let total = doc.known_width();
            if source_col >= total {
                return;
            }
            select_column(
                &doc.columns,
                &mut doc.selected_columns,
                &mut doc.column_selection_anchor,
                source_col,
                SelectionModifiers {
                    shift,
                    command: toggle,
                },
            );
            cx.notify();
        }
    }

    // -------------------------------------------------------------------------
    // Phase 4B-2b operations: Find/Replace, Filter Export, Sort, Duplicates,
    // Cleanup, and Structural Row/Column operations
    // -------------------------------------------------------------------------

    pub fn can_find_previous(
        &self,
        window: WindowId,
        query: &[u8],
        case_sensitivity: CaseSensitivity,
    ) -> bool {
        self.active_document(window)
            .is_some_and(|doc| doc.can_find_previous_with_case(query, case_sensitivity))
    }

    pub fn can_replace_current(
        &self,
        window: WindowId,
        query: &[u8],
        case_sensitivity: CaseSensitivity,
    ) -> bool {
        self.active_document(window)
            .is_some_and(|doc| doc.can_replace_current_with_case(query, case_sensitivity))
    }

    pub fn find_next_active(
        &mut self,
        window: WindowId,
        query: Vec<u8>,
        case_sensitivity: CaseSensitivity,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(doc_id) = self.scheduler.active_document(window) else {
            return Err("Open a file first.".into());
        };
        let reveal = self.scheduler.with_reserved_job_fds(
            doc_id,
            2,
            false,
            |doc| {
                doc.start_find_next_with_case(&query, case_sensitivity)?;
                Ok(doc.reveal_cell.take())
            },
            |err| err.to_string(),
        )?;
        if let Some(cell) = reveal {
            self.pending_reveal_cells.insert(doc_id, cell);
        }
        cx.notify();
        Ok(())
    }

    pub fn find_previous_active(
        &mut self,
        window: WindowId,
        query: Vec<u8>,
        case_sensitivity: CaseSensitivity,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(doc_id) = self.scheduler.active_document(window) else {
            return Err("Open a file first.".into());
        };
        let reveal = self.scheduler.with_reserved_job_fds(
            doc_id,
            2,
            false,
            |doc| {
                doc.start_find_previous_with_case(&query, case_sensitivity)?;
                Ok(doc.reveal_cell.take())
            },
            |err| err.to_string(),
        )?;
        if let Some(cell) = reveal {
            self.pending_reveal_cells.insert(doc_id, cell);
        }
        cx.notify();
        Ok(())
    }

    pub fn replace_current_active(
        &mut self,
        window: WindowId,
        query: Vec<u8>,
        replacement: Vec<u8>,
        case_sensitivity: CaseSensitivity,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(doc_id) = self.scheduler.active_document(window) else {
            return Err("Open a file first.".into());
        };
        let reveal = self.scheduler.with_reserved_job_fds(
            doc_id,
            2,
            false,
            |doc| {
                doc.replace_current_match_with_case(&query, &replacement, case_sensitivity)?;
                Ok(doc.reveal_cell.take())
            },
            |err| err.to_string(),
        )?;
        if let Some(cell) = reveal {
            self.pending_reveal_cells.insert(doc_id, cell);
        }
        self.scheduler.revoke_tab_close_approval(doc_id);
        cx.notify();
        Ok(())
    }

    pub fn replace_all_active(
        &mut self,
        window: WindowId,
        expected_doc_id: DocumentId,
        query: Vec<u8>,
        replacement: Vec<u8>,
        case_sensitivity: CaseSensitivity,
        cx: &mut Context<Self>,
    ) -> Result<(), AppMessage> {
        if !self.write_slot_available() {
            return Err(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish.",
            ));
        }
        if self.scheduler.active_document(window) != Some(expected_doc_id) {
            return Err(AppMessage::warning(
                "The active document changed before Replace All started.",
            ));
        }
        self.scheduler.with_reserved_job_fds(
            expected_doc_id,
            2,
            true,
            |doc| doc.start_replace_all_with_case(&query, &replacement, case_sensitivity),
            |err| AppMessage::warning(err.to_string()),
        )?;
        self.scheduler.revoke_tab_close_approval(expected_doc_id);
        cx.notify();
        Ok(())
    }

    pub fn cancel_search_active(&mut self, window: WindowId, cx: &mut Context<Self>) {
        if let Some(doc) = self.active_document_mut(window) {
            doc.cancel_search();
            let _ = self.scheduler.sync_fd_leases();
            cx.notify();
        }
    }

    pub fn start_filtered_export_active(
        &mut self,
        window: WindowId,
        expected_doc_id: DocumentId,
        destination: PathBuf,
        cx: &mut Context<Self>,
    ) -> Result<(), AppMessage> {
        if !self.write_slot_available() {
            return Err(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish.",
            ));
        }
        if self.scheduler.active_document(window) != Some(expected_doc_id) {
            return Err(AppMessage::warning(
                "The active document changed before Export Filtered started.",
            ));
        }
        if let Some(warning) = gzip_destination_warning(&destination) {
            return Err(warning);
        }
        if self.path_reserved(&destination) {
            return Err(AppMessage::warning(
                "That destination is already open or reserved in another tab.",
            ));
        }
        let dest_for_tab = destination.clone();
        self.scheduler.with_reserved_job_fds(
            expected_doc_id,
            2,
            false,
            |doc| doc.start_filtered_export(destination),
            |err| AppMessage::warning(err.to_string()),
        )?;
        if let Some(tab) = self.scheduler.tab_mut(expected_doc_id) {
            tab.write_destination = Some(dest_for_tab);
            tab.notice = None;
        }
        cx.notify();
        Ok(())
    }

    pub fn cancel_filter_or_export_active(&mut self, window: WindowId, cx: &mut Context<Self>) {
        if let Some(doc) = self.active_document_mut(window) {
            if doc.export_job.is_some() {
                doc.cancel_filtered_export();
            } else if let Some(job) = doc.filter_job.as_ref() {
                job.cancel();
            }
            let _ = self.scheduler.sync_fd_leases();
            cx.notify();
        }
    }

    pub fn suggest_date_time_formats(
        &self,
        window: WindowId,
        column: usize,
    ) -> Option<DateFormatSuggestion> {
        self.active_document(window)?
            .suggest_date_time_formats(column)
    }

    pub fn sort_plan_disk_estimate(&self, window: WindowId, plan: &SortPlan) -> Option<u64> {
        self.active_document(window)?.sort_plan_disk_estimate(plan)
    }

    pub fn start_sort_active(
        &mut self,
        window: WindowId,
        expected_doc_id: DocumentId,
        plan: SortPlan,
        cx: &mut Context<Self>,
    ) -> Result<(), AppMessage> {
        if !self.write_slot_available() {
            return Err(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish.",
            ));
        }
        if self.scheduler.active_document(window) != Some(expected_doc_id) {
            return Err(AppMessage::warning(
                "The active document changed before Sort started.",
            ));
        }
        self.scheduler.with_reserved_job_fds(
            expected_doc_id,
            2,
            true,
            |doc| doc.start_sort_plan(plan),
            |err| AppMessage::warning(err.to_string()),
        )?;
        self.scheduler.revoke_tab_close_approval(expected_doc_id);
        cx.notify();
        Ok(())
    }

    pub fn start_find_duplicates_active(
        &mut self,
        window: WindowId,
        expected_doc_id: DocumentId,
        target: DuplicateTarget,
        case_sensitivity: CaseSensitivity,
        cx: &mut Context<Self>,
    ) -> Result<(), AppMessage> {
        if !self.write_slot_available() {
            return Err(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish.",
            ));
        }
        if self.scheduler.active_document(window) != Some(expected_doc_id) {
            return Err(AppMessage::warning(
                "The active document changed before duplicate search started.",
            ));
        }
        self.scheduler.with_reserved_job_fds(
            expected_doc_id,
            2,
            true,
            |doc| doc.start_find_duplicates_target(target, case_sensitivity),
            |err| AppMessage::warning(err.to_string()),
        )?;
        cx.notify();
        Ok(())
    }

    pub fn confirm_duplicate_removal_active(
        &mut self,
        window: WindowId,
        cx: &mut Context<Self>,
    ) -> Result<String, AppMessage> {
        let Some(doc_id) = self.scheduler.active_document(window) else {
            return Err(AppMessage::warning("Open a file first."));
        };
        let notice = self.scheduler.confirm_duplicate_removal(doc_id)?;
        self.viewport_read_seq.remove(&doc_id);
        self.pending_reveal_cells.remove(&doc_id);
        cx.notify();
        Ok(notice)
    }

    pub fn cancel_structural_edit_active(&mut self, window: WindowId, cx: &mut Context<Self>) {
        if let Some(doc_id) = self.scheduler.active_document(window) {
            self.scheduler.cancel_structural_edit(doc_id);
            cx.notify();
        }
    }

    pub fn confirm_pending_materialization_active(
        &mut self,
        window: WindowId,
        cx: &mut Context<Self>,
    ) -> Result<(), AppMessage> {
        if !self.write_slot_available() {
            return Err(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish.",
            ));
        }
        let Some(doc_id) = self.scheduler.active_document(window) else {
            return Err(AppMessage::warning("Open a file first."));
        };
        self.scheduler.confirm_pending_materialization(doc_id)?;
        cx.notify();
        Ok(())
    }

    pub fn cancel_pending_materialization_active(
        &mut self,
        window: WindowId,
        cx: &mut Context<Self>,
    ) {
        if let Some(doc_id) = self.scheduler.active_document(window) {
            self.scheduler.cancel_pending_materialization(doc_id);
            cx.notify();
        }
    }

    pub fn start_cleanup_active(
        &mut self,
        window: WindowId,
        expected_doc_id: DocumentId,
        cleanup: ColumnCleanup,
        cx: &mut Context<Self>,
    ) -> Result<(), AppMessage> {
        if !self.write_slot_available() {
            return Err(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish.",
            ));
        }
        if self.scheduler.active_document(window) != Some(expected_doc_id) {
            return Err(AppMessage::warning(
                "The active document changed before cleanup started.",
            ));
        }
        self.scheduler.with_reserved_job_fds(
            expected_doc_id,
            2,
            true,
            |doc| doc.start_cleanup(cleanup),
            |err| AppMessage::warning(err.to_string()),
        )?;
        self.scheduler.revoke_tab_close_approval(expected_doc_id);
        cx.notify();
        Ok(())
    }

    pub fn start_insert_rows_active(
        &mut self,
        window: WindowId,
        expected_doc_id: DocumentId,
        dialog: &StructuralDialog,
        cx: &mut Context<Self>,
    ) -> Result<(), AppMessage> {
        if !self.write_slot_available() {
            return Err(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish.",
            ));
        }
        if self.scheduler.active_document(window) != Some(expected_doc_id) {
            return Err(AppMessage::warning(
                "The active document changed before row insertion started.",
            ));
        }
        self.scheduler.with_reserved_job_fds(
            expected_doc_id,
            2,
            true,
            |doc| doc.insert_rows_from_dialog(dialog),
            |err| AppMessage::warning(err.to_string()),
        )?;
        self.scheduler.revoke_tab_close_approval(expected_doc_id);
        cx.notify();
        Ok(())
    }

    pub fn start_insert_columns_active(
        &mut self,
        window: WindowId,
        expected_doc_id: DocumentId,
        dialog: &StructuralDialog,
        cx: &mut Context<Self>,
    ) -> Result<(), AppMessage> {
        if !self.write_slot_available() {
            return Err(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish.",
            ));
        }
        if self.scheduler.active_document(window) != Some(expected_doc_id) {
            return Err(AppMessage::warning(
                "The active document changed before column insertion started.",
            ));
        }
        self.scheduler.with_reserved_job_fds(
            expected_doc_id,
            2,
            true,
            |doc| doc.insert_columns_from_dialog(dialog),
            |err| AppMessage::warning(err.to_string()),
        )?;
        self.scheduler.revoke_tab_close_approval(expected_doc_id);
        cx.notify();
        Ok(())
    }

    pub fn start_delete_selected_rows_active(
        &mut self,
        window: WindowId,
        cx: &mut Context<Self>,
    ) -> Result<(), AppMessage> {
        if !self.write_slot_available() {
            return Err(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish.",
            ));
        }
        let Some(doc_id) = self.scheduler.active_document(window) else {
            return Err(AppMessage::warning("Open a file first."));
        };
        self.scheduler.with_reserved_job_fds(
            doc_id,
            2,
            true,
            |doc| {
                let ranges = doc.selected_rows.ranges.clone();
                doc.start_delete_rows(ranges)
            },
            |err| AppMessage::warning(err.to_string()),
        )?;
        self.scheduler.revoke_tab_close_approval(doc_id);
        cx.notify();
        Ok(())
    }

    pub fn start_delete_selected_columns_active(
        &mut self,
        window: WindowId,
        columns: Vec<usize>,
        cx: &mut Context<Self>,
    ) -> Result<(), AppMessage> {
        if !self.write_slot_available() {
            return Err(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish.",
            ));
        }
        let Some(doc_id) = self.scheduler.active_document(window) else {
            return Err(AppMessage::warning("Open a file first."));
        };
        self.scheduler.with_reserved_job_fds(
            doc_id,
            2,
            true,
            |doc| doc.start_delete_columns(columns),
            |err| AppMessage::warning(err.to_string()),
        )?;
        self.scheduler.revoke_tab_close_approval(doc_id);
        cx.notify();
        Ok(())
    }

    pub fn start_move_columns_active(
        &mut self,
        window: WindowId,
        expected_doc_id: DocumentId,
        columns: Vec<usize>,
        position: usize,
        cx: &mut Context<Self>,
    ) -> Result<(), AppMessage> {
        if !self.write_slot_available() {
            return Err(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish.",
            ));
        }
        if self.scheduler.active_document(window) != Some(expected_doc_id) {
            return Err(AppMessage::warning(
                "The active document changed before moving columns.",
            ));
        }
        self.scheduler.with_reserved_job_fds(
            expected_doc_id,
            2,
            true,
            |doc| doc.start_move_columns(columns, position),
            |err| AppMessage::warning(err.to_string()),
        )?;
        self.scheduler.revoke_tab_close_approval(expected_doc_id);
        cx.notify();
        Ok(())
    }

    pub fn start_split_column_active(
        &mut self,
        window: WindowId,
        expected_doc_id: DocumentId,
        source_column: usize,
        separator: Vec<u8>,
        cx: &mut Context<Self>,
    ) -> Result<(), AppMessage> {
        if !self.write_slot_available() {
            return Err(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish.",
            ));
        }
        if self.scheduler.active_document(window) != Some(expected_doc_id) {
            return Err(AppMessage::warning(
                "The active document changed before splitting the column.",
            ));
        }
        self.scheduler.with_reserved_job_fds(
            expected_doc_id,
            2,
            true,
            |doc| doc.start_split(source_column, separator),
            |err| AppMessage::warning(err.to_string()),
        )?;
        self.scheduler.revoke_tab_close_approval(expected_doc_id);
        cx.notify();
        Ok(())
    }

    pub fn start_combine_columns_active(
        &mut self,
        window: WindowId,
        expected_doc_id: DocumentId,
        columns: Vec<usize>,
        separator: Vec<u8>,
        cx: &mut Context<Self>,
    ) -> Result<(), AppMessage> {
        if !self.write_slot_available() {
            return Err(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish.",
            ));
        }
        if self.scheduler.active_document(window) != Some(expected_doc_id) {
            return Err(AppMessage::warning(
                "The active document changed before combining columns.",
            ));
        }
        self.scheduler.with_reserved_job_fds(
            expected_doc_id,
            2,
            true,
            |doc| doc.start_combine(columns, separator),
            |err| AppMessage::warning(err.to_string()),
        )?;
        self.scheduler.revoke_tab_close_approval(expected_doc_id);
        cx.notify();
        Ok(())
    }

    // -------------------------------------------------------------------------
    // Phase 4B-2c operations: .gz streaming lifecycle, Combine Files,
    // 4-mode Join Files, Inspect File & Recovery, Temporary Storage & Cache
    // -------------------------------------------------------------------------

    pub fn start_pending_decompression_active(
        &mut self,
        window: WindowId,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(doc_id) = self.scheduler.active_document(window) else {
            return Err("Open a file first.".into());
        };
        let extra_available = self.write_slot_available();
        self.scheduler
            .start_pending_decompression(doc_id, extra_available)?;
        if let Some(tab) = self.scheduler.tab_mut(doc_id) {
            tab.notice = None;
        }
        cx.notify();
        Ok(())
    }

    pub fn stay_in_compressed_preview_active(
        &mut self,
        window: WindowId,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(doc_id) = self.scheduler.active_document(window) else {
            return Err("Open a file first.".into());
        };
        self.scheduler.stay_in_compressed_preview(doc_id)?;
        cx.notify();
        Ok(())
    }

    pub fn cancel_decompression_active(&mut self, window: WindowId, cx: &mut Context<Self>) {
        let Some(doc_id) = self.scheduler.active_document(window) else {
            return;
        };
        if let Some(result) = self.scheduler.cancel_decompression(doc_id) {
            let msg = match result {
                Ok(text) => AppMessage::status(text),
                Err(err) => AppMessage::error(err),
            };
            if let Some(tab) = self.scheduler.tab_mut(doc_id) {
                tab.notice = Some(msg);
            }
            cx.notify();
        }
    }

    // --- Combine Files ---

    pub fn combine(&self) -> &CombineWorkflow {
        &self.combine
    }

    pub fn combine_add_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.combine.add_paths(paths);
        cx.notify();
    }

    pub fn combine_remove_path(&mut self, index: usize, cx: &mut Context<Self>) {
        self.combine.remove_path(index);
        cx.notify();
    }

    pub fn combine_move_path(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        self.combine.move_path(from, to);
        cx.notify();
    }

    pub fn combine_set_options(
        &mut self,
        delimiter: DelimiterMode,
        has_header: bool,
        encoding: Option<Encoding>,
        cx: &mut Context<Self>,
    ) {
        self.combine.set_options(delimiter, has_header, encoding);
        cx.notify();
    }

    pub fn combine_start_check(&mut self, cx: &mut Context<Self>) {
        let slot = self.write_slot_available();
        self.combine.start_check(slot);
        cx.notify();
    }

    fn normalize_output_alias(path: &Path) -> PathBuf {
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_path_buf())
        };
        if let Ok(canon) = std::fs::canonicalize(&abs) {
            return canon;
        }
        // Normalize lexical `.` and `..` components, then canonicalize the deepest existing
        // ancestor so aliases like `/data/out.csv` and `/data/sub/../out.csv` match before creation.
        let mut lexical = PathBuf::new();
        for component in abs.components() {
            match component {
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    lexical.pop();
                }
                other => lexical.push(other.as_os_str()),
            }
        }
        if let Ok(canon) = std::fs::canonicalize(&lexical) {
            return canon;
        }
        let mut suffix: Vec<std::ffi::OsString> = Vec::new();
        let mut current = lexical.as_path();
        while let Some(parent) = current.parent() {
            if let Some(name) = current.file_name() {
                suffix.push(name.to_os_string());
            }
            if let Ok(mut resolved) = std::fs::canonicalize(parent) {
                for seg in suffix.into_iter().rev() {
                    resolved.push(seg);
                }
                return resolved;
            }
            if parent == current {
                break;
            }
            current = parent;
        }
        lexical
    }

    fn path_reserved_excluding(
        &self,
        path: &Path,
        exclude_combine: bool,
        exclude_join: bool,
        exclude_inspect: bool,
    ) -> bool {
        if self.scheduler.path_reserved(path) {
            return true;
        }
        let target_norm = Self::normalize_output_alias(path);
        let matches_path = |res: &Path| -> bool {
            res == path || Self::normalize_output_alias(res) == target_norm
        };
        (!exclude_combine && self.combine.reserved_output().is_some_and(matches_path))
            || (!exclude_join && self.join.reserved_output().is_some_and(matches_path))
            || self
                .join
                .retired_reserved_outputs()
                .into_iter()
                .any(matches_path)
            || (!exclude_inspect && self.inspect.reserved_output().is_some_and(matches_path))
            || self
                .inspect
                .retired_reserved_outputs()
                .into_iter()
                .any(matches_path)
    }

    pub fn combine_prepare_destination(&mut self, destination: PathBuf, cx: &mut Context<Self>) {
        let protected = self.path_reserved_excluding(&destination, true, false, false);
        self.combine.prepare_destination(destination, |_| protected);
        cx.notify();
    }

    pub fn combine_start_write(&mut self, cx: &mut Context<Self>) {
        let slot = self.write_slot_available();
        let protected = self
            .combine
            .reserved_output()
            .is_some_and(|p| self.path_reserved_excluding(p, true, false, false));
        self.combine.start_write(slot, |_| protected);
        cx.notify();
    }

    pub fn combine_cancel(&mut self, cx: &mut Context<Self>) {
        self.combine.cancel();
        cx.notify();
    }

    // --- Join Files ---

    pub fn join(&self) -> &JoinWorkflow {
        &self.join
    }

    pub fn join_open_tab_choices(&self) -> Vec<(DocumentId, String, PathBuf)> {
        self.scheduler.hydrated_join_choices()
    }

    pub fn join_set_file_side(
        &mut self,
        side: usize,
        path: PathBuf,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let slot = self.write_slot_available();
        let res = self.join.set_file_side(side, path, slot);
        cx.notify();
        res
    }

    pub fn join_set_side_has_header(
        &mut self,
        side: usize,
        has_header: bool,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let slot = self.write_slot_available();
        let res = self.join.set_side_has_header(side, has_header, slot);
        cx.notify();
        res
    }

    pub fn join_set_open_tab_side(
        &mut self,
        side: usize,
        doc_id: DocumentId,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let slot = self.write_slot_available();
        let label = self
            .scheduler
            .tab_display_name(doc_id)
            .unwrap_or_else(|| "Untitled".into());
        let Some(doc) = self
            .scheduler
            .tab_mut(doc_id)
            .and_then(|tab| tab.document.as_mut())
        else {
            return Err("That tab is no longer open.".into());
        };
        let res = self.join.set_open_tab_side(side, doc_id, label, doc, slot);
        cx.notify();
        res
    }

    pub fn join_clear_side(&mut self, side: usize, cx: &mut Context<Self>) {
        self.join.clear_side(side);
        cx.notify();
    }

    pub fn join_set_mode(&mut self, mode: JoinMode, cx: &mut Context<Self>) {
        self.join.set_mode(mode);
        cx.notify();
    }

    pub fn join_set_ignore_ascii_case(&mut self, ignore: bool, cx: &mut Context<Self>) {
        self.join.set_ignore_ascii_case(ignore);
        cx.notify();
    }

    pub fn join_set_require_unique(&mut self, side: usize, required: bool, cx: &mut Context<Self>) {
        self.join.set_require_unique(side, required);
        cx.notify();
    }

    pub fn join_add_key_pair(&mut self, cx: &mut Context<Self>) {
        self.join.add_key_pair();
        cx.notify();
    }

    pub fn join_remove_key_pair(&mut self, index: usize, cx: &mut Context<Self>) {
        self.join.remove_key_pair(index);
        cx.notify();
    }

    pub fn join_set_key_column(
        &mut self,
        pair_index: usize,
        side: usize,
        column: usize,
        cx: &mut Context<Self>,
    ) {
        self.join.set_key_column(pair_index, side, column);
        cx.notify();
    }

    pub fn join_toggle_output_column(
        &mut self,
        index: usize,
        included: bool,
        cx: &mut Context<Self>,
    ) {
        self.join.toggle_output_column(index, included);
        cx.notify();
    }

    pub fn join_move_output_column(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        self.join.move_output_column(from, to);
        cx.notify();
    }

    pub fn join_set_acknowledge_expansion(&mut self, ack: bool, cx: &mut Context<Self>) {
        self.join.acknowledge_expansion = ack;
        cx.notify();
    }

    pub fn join_set_acknowledge_layout(&mut self, ack: bool, cx: &mut Context<Self>) {
        self.join.acknowledge_layout = ack;
        cx.notify();
    }

    pub fn join_start_analysis(&mut self, cx: &mut Context<Self>) {
        let slot = self.write_slot_available();
        self.join.start_analysis(slot);
        cx.notify();
    }

    pub fn join_prepare_destination(
        &mut self,
        destination: PathBuf,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let protected = self.path_reserved_excluding(&destination, false, true, false);
        let res = self.join.prepare_destination(destination, |_| protected);
        cx.notify();
        res
    }

    pub fn join_start_write(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        let slot = self.write_slot_available();
        let protected = self
            .join
            .reserved_output()
            .is_some_and(|p| self.path_reserved_excluding(p, false, true, false));
        let res = self.join.start_write(slot, |_| protected);
        cx.notify();
        res
    }

    pub fn join_cancel(&mut self, cx: &mut Context<Self>) {
        self.join.cancel();
        cx.notify();
    }

    pub fn join_release_all(&mut self, cx: &mut Context<Self>) {
        self.join.release_all_async();
        let _ = self.scheduler.sync_fd_leases();
        cx.notify();
    }

    // --- Inspect File & Recovery ---

    pub fn inspect(&self) -> &InspectWorkflow {
        &self.inspect
    }

    pub fn inspect_configure_for_active(&mut self, window: WindowId, cx: &mut Context<Self>) {
        if let Some(doc_id) = self.scheduler.active_document(window)
            && let Some(doc) = self
                .scheduler
                .tab(doc_id)
                .and_then(|tab| tab.document.as_ref())
        {
            self.inspect.configure_for_document(doc_id, doc);
        }
        cx.notify();
    }

    pub fn inspect_configure_failed_open(&mut self, failed: &FailedOpen, cx: &mut Context<Self>) {
        self.inspect.configure_for_failed_open(failed);
        cx.notify();
    }

    pub fn inspect_configure_path(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.inspect.configure_for_path(path);
        cx.notify();
    }

    pub fn inspect_set_options(
        &mut self,
        delimiter: DelimiterMode,
        header_mode: HeaderMode,
        encoding: Option<Encoding>,
        cx: &mut Context<Self>,
    ) {
        if !self.inspect.busy()
            && (self.inspect.delimiter != delimiter
                || self.inspect.header_mode != header_mode
                || self.inspect.encoding != encoding)
        {
            self.inspect.delimiter = delimiter;
            self.inspect.header_mode = header_mode;
            self.inspect.encoding = encoding;
            self.inspect.clear_report();
            cx.notify();
        }
    }

    pub fn inspect_start(&mut self, cx: &mut Context<Self>) {
        let slot = self.write_slot_available();
        self.inspect.start_inspect(slot);
        cx.notify();
    }

    pub fn inspect_focus_finding(&mut self, index: usize, cx: &mut Context<Self>) {
        self.inspect.focus_finding(index);
        cx.notify();
    }

    pub fn inspect_toggle_finding(&mut self, index: usize, cx: &mut Context<Self>) {
        self.inspect.toggle_finding(index);
        cx.notify();
    }

    pub fn inspect_select_all_eligible(&mut self, cx: &mut Context<Self>) {
        self.inspect.select_all_eligible();
        cx.notify();
    }

    pub fn inspect_start_recovery(
        &mut self,
        destination: PathBuf,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let slot = self.write_slot_available();
        let protected = self.path_reserved_excluding(&destination, false, false, false);
        let res = self
            .inspect
            .start_recovery(destination, slot, |_| protected);
        cx.notify();
        res
    }

    pub fn inspect_cancel(&mut self, cx: &mut Context<Self>) {
        self.inspect.cancel();
        cx.notify();
    }

    pub fn inspect_jump_to_finding(
        &mut self,
        window: WindowId,
        index: usize,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(target_doc) = self.inspect.target_doc_id else {
            return false;
        };
        let Some((source_row, opt_col)) = self.inspect.finding_jump_target(index) else {
            return false;
        };
        if self.scheduler.window_of(target_doc) == Some(window)
            && self.scheduler.activate_tab(window, target_doc).is_ok()
        {
            let visible_col = self
                .scheduler
                .tab(target_doc)
                .and_then(|tab| tab.document.as_ref())
                .and_then(|doc| {
                    if let Some(col) = opt_col
                        && doc.columns.visible.contains(&col)
                    {
                        Some(col)
                    } else {
                        doc.columns.visible.first().copied()
                    }
                });
            let Some(col) = visible_col else {
                return false;
            };
            self.pending_reveal_cells
                .insert(target_doc, (source_row, col));
            cx.notify();
            return true;
        }
        false
    }

    // --- Temporary Storage & Sidecar Index Cache ---

    pub fn working_directory(&self) -> &Path {
        self.scheduler.working_directory()
    }

    pub fn custom_working_directory(&self) -> bool {
        self.scheduler.custom_working_directory()
    }

    pub fn storage_load_error(&self) -> Option<&str> {
        self.storage_prefs.load_error.as_deref()
    }

    pub fn storage_check_running(&self) -> bool {
        self.storage_check_worker.is_some()
    }

    pub fn storage_check_result(&self) -> Option<&Result<(StorageSpace, u64), String>> {
        self.storage_check_result.as_ref()
    }

    pub fn cache_clear_running(&self) -> bool {
        self.cache_clear_worker.is_some()
    }

    pub fn cache_clear_status(&self) -> Option<&Result<String, String>> {
        self.cache_clear_status.as_ref()
    }

    pub fn index_cache_enabled(&self) -> bool {
        self.scheduler.index_cache_enabled()
    }

    pub fn start_storage_inspection(&mut self, cx: &mut Context<Self>) {
        if self.storage_check_worker.is_some() {
            return;
        }
        let gen_id = self.storage_check_gen;
        let dir = self.scheduler.working_directory().to_path_buf();
        let custom = self.scheduler.custom_working_directory();
        let retained = self.scheduler.retained_storage_paths();
        self.storage_check_worker = Some((
            gen_id,
            thread::spawn(move || inspect_temporary_storage(&dir, custom, &retained)),
        ));
        cx.notify();
    }

    pub fn set_scratch_directory(
        &mut self,
        directory: PathBuf,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        quarry_core::validate_scratch_directory(&directory).map_err(|e| e.to_string())?;
        self.storage_prefs
            .set_scratch_directory(Some(&directory))
            .map_err(|e| e.to_string())?;
        self.scheduler
            .set_working_directory(directory.clone(), true);
        self.join.temporary = directory.clone();
        self.inspect.set_working_directory(directory, true);
        self.storage_check_gen = self.storage_check_gen.wrapping_add(1);
        self.storage_check_result = None;
        self.start_storage_inspection(cx);
        cx.notify();
        Ok(())
    }

    pub fn reset_scratch_directory(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        self.storage_prefs
            .set_scratch_directory(None)
            .map_err(|e| e.to_string())?;
        let default_dir = std::env::temp_dir();
        self.scheduler
            .set_working_directory(default_dir.clone(), false);
        self.join.temporary = default_dir.clone();
        self.inspect.set_working_directory(default_dir, false);
        self.storage_check_gen = self.storage_check_gen.wrapping_add(1);
        self.storage_check_result = None;
        self.start_storage_inspection(cx);
        cx.notify();
        Ok(())
    }

    pub fn set_index_cache_enabled(
        &mut self,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.storage_prefs
            .set_index_cache_enabled(enabled)
            .map_err(|e| e.to_string())?;
        self.scheduler.set_index_cache_enabled(enabled);
        cx.notify();
        Ok(())
    }

    pub fn start_clear_index_cache(&mut self, cx: &mut Context<Self>) {
        if self.cache_clear_worker.is_some() {
            return;
        }
        self.cache_clear_status = None;
        self.cache_clear_worker = Some(thread::spawn(clear_index_cache_summary));
        cx.notify();
    }

    fn any_hydrated_tab_busy(&self) -> bool {
        self.combine.busy()
            || self.join.running_operation()
            || self.inspect.busy()
            || self.inspect.recovering()
            || self.cache_clear_worker.is_some()
            || self.storage_check_worker.is_some()
            || self.scheduler.windows().into_iter().any(|window| {
                self.scheduler.tabs_in_window(window).into_iter().any(|id| {
                    self.scheduler
                        .tab(id)
                        .is_some_and(WorkbenchTab::document_busy)
                })
            })
    }

    #[cfg(test)]
    pub fn resource_hud_sample(&self) -> &ResourceHudSample {
        &self.hud_sample
    }

    /// Resolve the window-specific volume readings (active document volume + scratch volume)
    /// and process memory sample for `window`.
    pub fn resource_hud_for_window(
        &self,
        window: WindowId,
    ) -> (
        Option<quarry_core::ProcessMemorySample>,
        Vec<quarry_workbench::VolumeSpaceSample>,
    ) {
        let doc_dir = self
            .active_document(window)
            .and_then(|doc| doc.logical_path.parent());
        let (volumes, _) = self.hud_sample.volumes_for(doc_dir);
        (self.hud_sample.memory, volumes)
    }

    pub fn hud_announcement(&self, window: WindowId) -> Option<&HudAnnouncement> {
        self.last_hud_announcement.get(&window)
    }

    pub fn pause_active_indexing(
        &mut self,
        window: WindowId,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(id) = self.scheduler.active_document(window) else {
            return Err("Open a file first.".into());
        };
        // Record the active document's pre-pause phase before issuing the pause request so a
        // fast pause (before the first periodic poll tick) still announces Indexing -> Paused.
        let _ = self.sync_resource_hud();
        self.scheduler.pause_indexing(id)?;
        let _ = self.sync_resource_hud();
        cx.notify();
        Ok(())
    }

    pub fn resume_active_indexing(
        &mut self,
        window: WindowId,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(id) = self.scheduler.active_document(window) else {
            return Err("Open a file first.".into());
        };
        let _ = self.sync_resource_hud();
        self.scheduler.resume_indexing(id)?;
        let _ = self.sync_resource_hud();
        cx.notify();
        Ok(())
    }

    pub fn toggle_active_indexing_pause(
        &mut self,
        window: WindowId,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let paused = self
            .active_document(window)
            .is_some_and(Document::is_indexing_paused);
        if paused {
            self.resume_active_indexing(window, cx)
        } else {
            self.pause_active_indexing(window, cx)
        }
    }

    fn sync_resource_hud(&mut self) -> bool {
        let mut changed = false;
        let windows = self.scheduler.windows();
        let document_dirs: BTreeSet<PathBuf> = windows
            .iter()
            .filter_map(|&window| self.active_document(window))
            .filter_map(|doc| doc.logical_path.parent().map(Path::to_path_buf))
            .collect();
        let next_targets = ResourceHudTargets {
            document_dirs,
            scratch_dir: self.scheduler.working_directory().to_path_buf(),
        };
        if next_targets != self.hud_targets {
            self.hud_targets = next_targets.clone();
            self.hud_sampler.set_targets(next_targets);
        }
        let latest = self.hud_sampler.snapshot();
        if latest != self.hud_sample {
            self.hud_sample = latest;
            changed = true;
        }
        for window in windows {
            let (doc_instance, doc_dir, phase, committed_rows, committed_bytes, file_size) =
                match self.active_document(window) {
                    Some(doc) => {
                        let phase = if doc.job.is_none() && doc.progress.done {
                            HudIndexPhase::Complete
                        } else if doc.is_indexing_paused() {
                            HudIndexPhase::Paused
                        } else {
                            HudIndexPhase::Indexing
                        };
                        (
                            Some(doc.instance_id),
                            doc.logical_path.parent().map(Path::to_path_buf),
                            phase,
                            doc.committed_data_rows(),
                            doc.progress.bytes_scanned,
                            doc.session.active_byte_len(),
                        )
                    }
                    None => (None, None, HudIndexPhase::Idle, 0, 0, 0),
                };
            let (window_volumes, sample_incomplete) =
                self.hud_sample.volumes_for(doc_dir.as_deref());
            let tracker = self.hud_announcements.entry(window).or_default();
            if let Some(announcement) = tracker.update(
                doc_instance,
                phase,
                committed_rows,
                committed_bytes,
                file_size,
                &window_volumes,
                sample_incomplete,
            ) {
                self.last_hud_announcement.insert(window, announcement);
                changed = true;
            }
        }
        changed
    }

    pub fn poll_background(&mut self, cx: &mut Context<Self>) {
        // Track whether any hydrated tab had an active background job BEFORE polling so
        // the exact tick a job finishes cleanly still notifies observers.
        let mut changed = self.any_hydrated_tab_busy();
        if self.combine.poll() {
            changed = true;
        }
        if self.join.poll() {
            let _ = self.scheduler.sync_fd_leases();
            changed = true;
        }
        if self.inspect.poll() {
            changed = true;
        }
        if self
            .cache_clear_worker
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            changed = true;
            let outcome = match self.cache_clear_worker.take().unwrap().join() {
                Ok(res) => res,
                Err(_) => Err("Clearing the index cache failed unexpectedly.".into()),
            };
            self.cache_clear_status = Some(outcome);
        }
        if self
            .storage_check_worker
            .as_ref()
            .is_some_and(|(_, handle)| handle.is_finished())
        {
            changed = true;
            let (worker_gen, handle) = self.storage_check_worker.take().unwrap();
            let outcome = match handle.join() {
                Ok(res) => res,
                Err(_) => Err("Checking temporary storage failed unexpectedly.".into()),
            };
            if worker_gen == self.storage_check_gen {
                self.storage_check_result = Some(outcome);
            } else {
                // Discard stale result for the previous folder and immediately re-inspect current.
                self.start_storage_inspection(cx);
            }
        }
        let poll_errors = self.scheduler.poll_all();
        if !poll_errors.is_empty() {
            for (doc_id, error) in poll_errors {
                if let Some(tab) = self.scheduler.tab_mut(doc_id) {
                    tab.notice = Some(AppMessage::error(error));
                }
            }
            changed = true;
        }
        for completion in self.scheduler.poll_section_verifications() {
            changed = true;
            if completion.reopened {
                self.viewport_read_seq.remove(&completion.document_id);
                self.pending_reveal_cells.remove(&completion.document_id);
            }
            match completion.result {
                Ok(VerifyBoundaryOutcome::Verified {
                    bounds: _,
                    boundary_changed,
                }) => {
                    if let Some(tab) = self.scheduler.tab_mut(completion.document_id) {
                        if completion.reopened {
                            // `reopen_verified_section` already set a status notice.
                        } else if boundary_changed {
                            tab.notice = Some(AppMessage::warning(
                                "Verified section boundaries shifted. Save your unsaved draft as a new file before reopening at the verified bounds.",
                            ));
                        } else {
                            let msg = if let Some(first) = tab
                                .document
                                .as_ref()
                                .and_then(|d| d.global_data_row_number(d.data_start))
                            {
                                format!(
                                    "Section boundaries verified (starts at row {}).",
                                    quarry_workbench::group_digits(first)
                                )
                            } else {
                                "Section boundaries verified.".into()
                            };
                            tab.notice = Some(AppMessage::status(msg));
                        }
                    }
                }
                Ok(VerifyBoundaryOutcome::Cancelled) => {
                    if let Some(tab) = self.scheduler.tab_mut(completion.document_id) {
                        tab.notice = Some(AppMessage::status("Boundary verification cancelled."));
                    }
                }
                Err(error) => {
                    if let Some(tab) = self.scheduler.tab_mut(completion.document_id) {
                        tab.notice = Some(error);
                    }
                }
            }
        }
        for completion in self.scheduler.poll_searches() {
            changed = true;
            match completion.result {
                Ok(Some(cell)) => {
                    self.pending_reveal_cells
                        .insert(completion.document_id, cell);
                }
                Ok(None) => {}
                Err(error) => {
                    if let Some(tab) = self.scheduler.tab_mut(completion.document_id) {
                        tab.notice = Some(AppMessage::error(error));
                    }
                }
            }
        }
        for completion in self.scheduler.poll_filtered_exports() {
            changed = true;
            match completion.result {
                Ok(Some(status)) => {
                    if let Some(tab) = self.scheduler.tab_mut(completion.document_id) {
                        tab.notice = Some(AppMessage::new(status, MessageSeverity::Status));
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    if let Some(tab) = self.scheduler.tab_mut(completion.document_id) {
                        tab.notice = Some(AppMessage::error(error));
                    }
                }
            }
        }
        for completion in self.scheduler.poll_structural_edits() {
            changed = true;
            match completion.result {
                Ok(Some(status)) => {
                    self.viewport_read_seq.remove(&completion.document_id);
                    self.pending_reveal_cells.remove(&completion.document_id);
                    if let Some(tab) = self.scheduler.tab_mut(completion.document_id) {
                        tab.notice = Some(AppMessage::new(status, MessageSeverity::Status));
                    }
                }
                Ok(None) => {}
                Err(error) => {
                    if let Some(tab) = self.scheduler.tab_mut(completion.document_id) {
                        tab.notice = Some(error);
                    }
                }
            }
        }
        let save_results = self.scheduler.poll_saves();
        let mut advance_review = false;
        let reviewed_doc = self
            .pending_review
            .as_ref()
            .and_then(PendingReviewScope::current_document);
        for completion in save_results {
            changed = true;
            match completion.result {
                Ok((path, replaced_source)) => {
                    if let Some(tab) = self.scheduler.tab_mut(completion.document_id) {
                        let text = if replaced_source {
                            format!("Saved: {}", path.display())
                        } else {
                            format!("Saved copy: {}", path.display())
                        };
                        tab.notice = Some(AppMessage::new(text, MessageSeverity::Status));
                        if tab.close_after_save {
                            tab.close_after_save = false;
                            if reviewed_doc == Some(completion.document_id) {
                                advance_review = true;
                            }
                        }
                    }
                }
                Err(error) => {
                    if let Some(tab) = self.scheduler.tab_mut(completion.document_id) {
                        tab.close_after_save = false;
                        tab.notice = Some(error);
                    }
                    if reviewed_doc == Some(completion.document_id) {
                        self.cancel_close_review(None, cx);
                    }
                }
            }
        }
        if advance_review && let Some(token) = self.active_review_token {
            let next = self.approve_discard_in_review(token, cx);
            match &next {
                CloseReviewAction::ConfirmTab { window_id, .. } => {
                    if let Some(handle) = self.window_handle(*window_id) {
                        let _ = handle.update(cx, |_, window, _| window.activate_window());
                    }
                    self.pending_review_action = Some(next);
                }
                CloseReviewAction::Wait { .. } => {
                    self.pending_review_action = Some(next);
                }
                CloseReviewAction::Proceed => {}
            }
            changed = true;
        }
        if self.sync_resource_hud() {
            changed = true;
        }
        if !changed && self.any_hydrated_tab_busy() {
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }
}
