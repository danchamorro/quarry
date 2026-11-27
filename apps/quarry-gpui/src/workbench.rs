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
    FilterOperator, FilterPredicate, FilterQuery, HeaderMode, JoinMode, OpenOptions, SectionBounds,
    SectionOpenOptions, SortPlan, StorageSpace, VerifyBoundaryOutcome, VerifyBoundaryProgress,
    raise_nofile_limit,
};
use quarry_workbench::compressed::{
    DecompressionStatus, decompression_status, gzip_destination_warning,
};
use quarry_workbench::{
    AppMessage, CombineWorkflow, CommandContext, CommandId, CommandResolution,
    DEFAULT_NOFILE_TARGET, DelimiterMode, Document, DocumentId, EffectiveWordWrap, FailedOpen,
    GridSelection, HudAnnouncement, HudAnnouncementTracker, HudIndexPhase, InspectWorkflow,
    JOIN_INPUT_RESERVED, JoinWorkflow, MessageSeverity, OmarchyThemeWatcher, Palette,
    ResidencyState, ResolvedTheme, ResourceHudSample, ResourceHudSampler, ResourceHudTargets,
    SchedulerError, SelectionModifiers, StoragePreferences, StructuralDialog, StructuralJob,
    ThemeAppearance, ThemePreference, ThemePreferences, WakeOutcome, WindowId, WordWrapPreferences,
    WordWrapToggleOutcome, WorkbenchScheduler, WorkbenchSchedulerConfig, WorkbenchTab,
    clear_index_cache_summary, evaluate_command, inspect_temporary_storage,
    materialization_progress, omarchy_theme_fingerprint, resolve_theme, resolve_word_wrap,
    select_column, whitespace,
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
    pub can_splice_save: bool,
    pub splice_unavailable_reason: Option<&'static str>,
    pub speculative_save_warning: Option<&'static str>,
}

/// Summary of one bookmarked logical CSV record rendered in the Bookmarks popover.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BookmarkListEntry {
    pub local_row: u64,
    pub label: String,
    pub record_start_offset: Option<u64>,
    pub global_ordinal: Option<u64>,
}

/// Read-only snapshot of Live Watch Mode state for the active tab.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveWatchUiSnapshot {
    pub active: bool,
    pub tail_follow: bool,
    pub can_start: bool,
    pub unavailable_reason: Option<&'static str>,
    pub backend_label: Option<&'static str>,
    pub following_unlinked: bool,
    pub appended_rows: u64,
    pub provisional_tail_bytes: Option<u64>,
    pub alert: Option<quarry_core::WatchAlert>,
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
    pub word_wrap: EffectiveWordWrap,
    pub word_wrap_threshold_bytes: u64,
    pub section: Option<Box<SectionSnapshot>>,
    pub live_watch: LiveWatchUiSnapshot,
    pub bookmark_count: usize,
    pub bookmarks: Vec<BookmarkListEntry>,
    pub split_at_bookmarks_disabled_reason: Option<String>,
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
    pub index_cancelled: bool,
    pub index_paused: bool,
    pub index_pausing: bool,
    pub index_fraction: f32,
    pub index_committed_bytes: u64,
    pub index_bytes_read: u64,
    pub index_committed_rows: u64,
    pub can_pause_indexing: bool,
    pub can_resume_indexing: bool,
    pub can_cancel_indexing: bool,
    pub dirty: bool,
    pub saving: bool,
    pub save_cancel_requested: bool,
    pub save_status: Option<String>,
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
    pub pending_diagnostic_message: Option<String>,
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

#[derive(Default)]
struct UiWakeupState {
    signaled: bool,
    shutdown: bool,
    waker: Option<std::task::Waker>,
}

/// Zero-dependency `Future`-based wakeup primitive allowing the GPUI background coordinator loop
/// to sleep with zero timers, zero extra threads, and zero main-thread wake-ups while idle,
/// waking immediately when background work starts, the Resource HUD changes, or an `inotify`
/// Omarchy theme change arrives.
struct UiWakeup {
    state: std::sync::Mutex<UiWakeupState>,
}

impl UiWakeup {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: std::sync::Mutex::new(UiWakeupState::default()),
        })
    }

    fn notify(&self) {
        let waker = {
            let mut state = self.state.lock().unwrap();
            state.signaled = true;
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn shutdown(&self) {
        let waker = {
            let mut state = self.state.lock().unwrap();
            state.shutdown = true;
            state.signaled = true;
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn wait(self: &Arc<Self>) -> UiWakeupFuture {
        UiWakeupFuture {
            wakeup: Arc::clone(self),
        }
    }
}

struct UiWakeupFuture {
    wakeup: Arc<UiWakeup>,
}

impl std::future::Future for UiWakeupFuture {
    type Output = bool;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<bool> {
        let mut state = self.wakeup.state.lock().unwrap();
        if state.shutdown {
            state.signaled = false;
            return std::task::Poll::Ready(false);
        }
        if state.signaled {
            state.signaled = false;
            return std::task::Poll::Ready(true);
        }
        state.waker = Some(cx.waker().clone());
        std::task::Poll::Pending
    }
}

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
    omarchy_watcher: Option<OmarchyThemeWatcher>,
    ui_wakeup: Arc<UiWakeup>,
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
    live_watch_tail_read_in_flight: BTreeMap<DocumentId, u64>,
    pending_reveal_cells: BTreeMap<DocumentId, (u64, usize)>,
    whitespace_prefs: whitespace::Preferences,
    word_wrap_prefs: WordWrapPreferences,
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
        let ui_wakeup = UiWakeup::new();
        let hud_wakeup = Arc::clone(&ui_wakeup);
        let hud_sampler = ResourceHudSampler::start_with_notify(
            hud_targets.clone(),
            Arc::new(move || hud_wakeup.notify()),
        );
        let hud_sample = hud_sampler.snapshot();
        let omarchy_watcher = if theme_prefs.preference == ThemePreference::Omarchy {
            theme_home.clone().map(|home| {
                let theme_wakeup = Arc::clone(&ui_wakeup);
                OmarchyThemeWatcher::start(
                    home,
                    system_appearance,
                    resolved_theme.clone(),
                    Arc::new(move || theme_wakeup.notify()),
                )
            })
        } else {
            None
        };
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
            omarchy_watcher,
            ui_wakeup,
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
            live_watch_tail_read_in_flight: BTreeMap::new(),
            pending_reveal_cells: BTreeMap::new(),
            whitespace_prefs: whitespace::Preferences::load(None),
            word_wrap_prefs: WordWrapPreferences::load(None),
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
        let whitespace_prefs = whitespace::Preferences::application();
        let word_wrap_prefs = WordWrapPreferences::application();
        let theme_home = std::env::var_os("HOME").map(PathBuf::from);
        let config = WorkbenchSchedulerConfig {
            defer_gzip_decompression: true,
            ..WorkbenchSchedulerConfig::default()
        };
        let entity = cx.new(|_| {
            Self::new_with_all_prefs(config, storage_prefs, theme_prefs, theme_home)
                .with_whitespace_prefs(whitespace_prefs)
                .with_word_wrap_prefs(word_wrap_prefs)
        });
        cx.set_global(WorkbenchGlobal(entity.clone()));

        let wakeup = Arc::clone(&entity.read(cx).ui_wakeup);
        let observe_wakeup = Arc::clone(&wakeup);
        cx.observe(&entity, move |this, cx| {
            if this.read(cx).has_active_background_work() {
                observe_wakeup.notify();
            }
        })
        .detach();

        let poll_entity = entity.clone();
        let loop_wakeup = Arc::clone(&wakeup);
        cx.spawn(async move |cx| {
            loop {
                let action = cx.update(|cx| {
                    if Self::try_global(cx).is_none() {
                        return None;
                    }
                    let (busy, omarchy_update) = poll_entity.update(cx, |this, cx| {
                        this.poll_background(cx);
                        let update = this.take_omarchy_theme_update(cx);
                        let busy = this.has_active_background_work();
                        this.hud_sampler.set_idle(!busy);
                        (busy, update)
                    });
                    if let Some(resolved) = omarchy_update {
                        crate::apply_resolved_theme(&resolved, cx);
                        cx.refresh_windows();
                    }
                    Some(busy)
                });
                match action {
                    None => break,
                    Some(true) => {
                        cx.background_executor()
                            .timer(Duration::from_millis(32))
                            .await;
                    }
                    Some(false) => {
                        if !loop_wakeup.wait().await {
                            break;
                        }
                    }
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
        self.omarchy_watcher = if preference == ThemePreference::Omarchy {
            self.theme_home.clone().map(|home| {
                let theme_wakeup = Arc::clone(&self.ui_wakeup);
                OmarchyThemeWatcher::start(
                    home,
                    self.system_appearance,
                    resolved.clone(),
                    Arc::new(move || theme_wakeup.notify()),
                )
            })
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
        if let Some(watcher) = &self.omarchy_watcher {
            watcher.set_system_appearance(appearance);
        }
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

    /// Consumes any event-driven Omarchy theme update resolved off-thread by `OmarchyThemeWatcher`.
    pub fn take_omarchy_theme_update(&mut self, cx: &mut Context<Self>) -> Option<ResolvedTheme> {
        if self.theme_prefs.preference != ThemePreference::Omarchy {
            return None;
        }
        let resolved = self.omarchy_watcher.as_ref()?.take_update()?;
        self.omarchy_last_check = Instant::now();
        self.omarchy_fingerprint = if resolved.warning.is_none() {
            self.theme_home
                .as_deref()
                .and_then(omarchy_theme_fingerprint)
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

    #[allow(dead_code)]
    pub fn poll_omarchy_theme(&mut self, cx: &mut Context<Self>) -> Option<ResolvedTheme> {
        if let Some(updated) = self.take_omarchy_theme_update(cx) {
            return Some(updated);
        }
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
        self.live_watch_tail_read_in_flight.remove(&doc_id);
        let seq = {
            let entry = self.viewport_read_seq.entry(doc_id).or_insert(0);
            *entry = entry.wrapping_add(1);
            *entry
        };
        let Some(plan) = plan else {
            return;
        };
        self.spawn_viewport_read_plan_with_seq(doc_id, seq, plan, false, 0, cx);
    }

    fn clear_viewport_read_state(&mut self, doc_id: DocumentId) {
        self.viewport_read_seq.remove(&doc_id);
        self.live_watch_tail_read_in_flight.remove(&doc_id);
    }

    fn is_document_active(&self, doc_id: DocumentId) -> bool {
        self.windows
            .keys()
            .any(|&win| self.scheduler.active_document(win) == Some(doc_id))
    }

    #[cfg(test)]
    pub(crate) fn viewport_read_sequence(&self, doc_id: DocumentId) -> Option<u64> {
        self.viewport_read_seq.get(&doc_id).copied()
    }

    pub(crate) fn has_live_watch_tail_read_in_flight(&self, doc_id: DocumentId) -> bool {
        let Some(&in_flight_seq) = self.live_watch_tail_read_in_flight.get(&doc_id) else {
            return false;
        };
        self.viewport_read_seq.get(&doc_id) == Some(&in_flight_seq)
    }

    fn spawn_live_watch_viewport_refresh(&mut self, doc_id: DocumentId, cx: &mut Context<Self>) {
        // Explicit watch start or Follow Tail jump supersedes any earlier non-tail viewport read.
        self.live_watch_tail_read_in_flight.remove(&doc_id);
        let seq = {
            let entry = self.viewport_read_seq.entry(doc_id).or_insert(0);
            *entry = entry.wrapping_add(1);
            *entry
        };
        let plan = self
            .scheduler
            .tab_mut(doc_id)
            .and_then(|t| t.document.as_mut())
            .and_then(|doc| doc.take_live_watch_viewport_plan());
        if let Some(plan) = plan {
            self.live_watch_tail_read_in_flight.insert(doc_id, seq);
            self.spawn_viewport_read_plan_with_seq(doc_id, seq, plan, true, 1, cx);
        }
    }

    fn maybe_spawn_live_watch_tail_refresh(
        &mut self,
        doc_id: DocumentId,
        cx: &mut Context<Self>,
    ) -> bool {
        self.maybe_spawn_live_watch_tail_refresh_with_retries(doc_id, 1, cx)
    }

    fn maybe_spawn_live_watch_tail_refresh_with_retries(
        &mut self,
        doc_id: DocumentId,
        retries_remaining: u8,
        cx: &mut Context<Self>,
    ) -> bool {
        // Keep at most one live-watch tail read in flight per document and coalesce arriving
        // appends into `doc.live_watch_viewport_dirty` until the current read finishes, so
        // sustained appends on slow storage never starve the grid.
        if self.has_live_watch_tail_read_in_flight(doc_id) {
            return false;
        }
        let Some(plan) = self
            .scheduler
            .tab_mut(doc_id)
            .and_then(|t| t.document.as_mut())
            .and_then(|doc| doc.take_live_watch_viewport_plan())
        else {
            return false;
        };
        let seq = {
            let entry = self.viewport_read_seq.entry(doc_id).or_insert(0);
            *entry = entry.wrapping_add(1);
            *entry
        };
        self.live_watch_tail_read_in_flight.insert(doc_id, seq);
        self.spawn_viewport_read_plan_with_seq(doc_id, seq, plan, true, retries_remaining, cx);
        true
    }

    fn spawn_viewport_read_plan_with_seq(
        &mut self,
        doc_id: DocumentId,
        seq: u64,
        plan: quarry_workbench::ViewportReadPlan,
        is_live_watch_tail: bool,
        retries_remaining: u8,
        cx: &mut Context<Self>,
    ) {
        let session = Arc::clone(&plan.session);
        let start = plan.start;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let began = Instant::now();
                    let read = plan.execute();
                    (session, read, began.elapsed())
                })
                .await;
            let _ = this.update(cx, |controller, cx| {
                if is_live_watch_tail
                    && controller.live_watch_tail_read_in_flight.get(&doc_id) == Some(&seq)
                {
                    controller.live_watch_tail_read_in_flight.remove(&doc_id);
                }
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
                let mut follow_up_retries = None;
                match read {
                    Ok(rows) => {
                        doc.install_buffer_rows(start, rows, elapsed);
                        if is_live_watch_tail {
                            follow_up_retries = Some(1);
                        }
                    }
                    Err(error) => {
                        let source_changed =
                            matches!(error, quarry_core::QuarryError::SourceChanged);
                        let msg = doc.diagnostic_error(error);
                        tab.notice = Some(AppMessage::error(msg));
                        if is_live_watch_tail
                            && retries_remaining > 0
                            && !source_changed
                            && doc.is_live_watching()
                            && doc.live_watch_tail_follow
                            && !doc.filter_active()
                        {
                            doc.live_watch_viewport_dirty = true;
                            follow_up_retries = Some(retries_remaining - 1);
                        }
                    }
                }
                if let Some(next_retries) = follow_up_retries
                    && controller.is_document_active(doc_id)
                {
                    let _ = controller.maybe_spawn_live_watch_tail_refresh_with_retries(
                        doc_id,
                        next_retries,
                        cx,
                    );
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

    pub fn with_whitespace_prefs(mut self, prefs: whitespace::Preferences) -> Self {
        self.whitespace_prefs = prefs;
        self
    }

    pub fn with_word_wrap_prefs(mut self, prefs: WordWrapPreferences) -> Self {
        self.word_wrap_prefs = prefs;
        self
    }

    pub fn word_wrap_auto_disable_bytes(&self) -> u64 {
        self.word_wrap_prefs.auto_disable_bytes
    }

    pub fn effective_word_wrap(&self, window: WindowId) -> EffectiveWordWrap {
        let Some(doc_id) = self.scheduler.active_document(window) else {
            return resolve_word_wrap(&self.word_wrap_prefs, None, None);
        };
        let Some(tab) = self.scheduler.tab(doc_id) else {
            return resolve_word_wrap(&self.word_wrap_prefs, None, None);
        };
        let active_byte_len = tab
            .document
            .as_ref()
            .map(|doc| doc.session.active_byte_len());
        resolve_word_wrap(
            &self.word_wrap_prefs,
            tab.word_wrap_override,
            active_byte_len,
        )
    }

    pub fn toggle_word_wrap(
        &mut self,
        window: WindowId,
        cx: &mut Context<Self>,
    ) -> WordWrapToggleOutcome {
        let active_doc_id = self.scheduler.active_document(window);
        let (tab_override, active_byte_len) = active_doc_id
            .and_then(|id| self.scheduler.tab(id))
            .map(|tab| {
                (
                    tab.word_wrap_override,
                    tab.document
                        .as_ref()
                        .map(|doc| doc.session.active_byte_len()),
                )
            })
            .unwrap_or((None, None));
        let prev_effective =
            resolve_word_wrap(&self.word_wrap_prefs, tab_override, active_byte_len);
        let outcome = self
            .word_wrap_prefs
            .toggle_for_tab(tab_override, active_byte_len);
        if let Some(doc_id) = active_doc_id
            && let Some(tab) = self.scheduler.tab_mut(doc_id)
        {
            tab.word_wrap_override = outcome.next_tab_override;
        }
        if outcome.effective != prev_effective {
            self.last_hud_announcement.insert(
                window,
                HudAnnouncement {
                    text: outcome.effective.transition_announcement(),
                    is_alert: false,
                },
            );
        }
        cx.notify();
        outcome
    }

    pub fn set_word_wrap_auto_disable_bytes(
        &mut self,
        bytes: u64,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        self.word_wrap_prefs
            .set_auto_disable_bytes(bytes)
            .map_err(|error| format!("Could not save word wrap threshold: {error}"))?;
        cx.notify();
        Ok(())
    }

    pub fn push_hud_announcement(
        &mut self,
        window: WindowId,
        text: String,
        is_alert: bool,
        cx: &mut Context<Self>,
    ) {
        self.last_hud_announcement
            .insert(window, HudAnnouncement { text, is_alert });
        cx.notify();
    }

    pub fn show_all_whitespace(&self) -> bool {
        self.whitespace_prefs.show_all
    }

    pub fn toggle_show_all_whitespace(&mut self, cx: &mut Context<Self>) -> Option<String> {
        let next = !self.whitespace_prefs.show_all;
        let warning = self
            .whitespace_prefs
            .set(next)
            .err()
            .map(|error| format!("Could not save whitespace preference: {error}"));
        cx.notify();
        warning
    }

    /// Queries the active document's bounded async whitespace cache for a cell's full raw bytes.
    pub fn inspect_cell_whitespace(
        &mut self,
        window: WindowId,
        source_row: u64,
        source_col: usize,
        raw_len: usize,
    ) -> Option<whitespace::Summary> {
        let summary = self
            .active_document_mut(window)?
            .inspect_cell_whitespace(source_row, source_col, raw_len);
        if summary.is_none() {
            self.ui_wakeup.notify();
        }
        summary
    }

    /// Queries the active document's bounded async whitespace cache for a header's full raw bytes.
    pub fn inspect_header_whitespace(
        &mut self,
        window: WindowId,
        source_col: usize,
        raw_len: usize,
    ) -> Option<whitespace::Summary> {
        let summary = self
            .active_document_mut(window)?
            .inspect_header_whitespace(source_col, raw_len);
        if summary.is_none() {
            self.ui_wakeup.notify();
        }
        summary
    }

    /// Returns a human-readable explanation of the full value's whitespace for tooltips and a11y.
    pub fn whitespace_explanation(
        &self,
        window: WindowId,
        summary: Option<whitespace::Summary>,
    ) -> String {
        let show_all = self.whitespace_prefs.show_all;
        self.active_document(window)
            .map(|doc| doc.whitespace.explanation(summary, show_all))
            .unwrap_or_default()
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
            show_all_whitespace: self.whitespace_prefs.show_all,
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
        let filter_done = !doc.is_filter_scanning() && !doc.filter_rows_loading();
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
        let word_wrap = resolve_word_wrap(
            &self.word_wrap_prefs,
            tab.word_wrap_override,
            Some(active_byte_len),
        );
        let word_wrap_threshold_bytes = self.word_wrap_prefs.auto_disable_bytes;
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
                can_splice_save: doc.can_splice_save_section(),
                splice_unavailable_reason: doc.section_splice_unavailable_reason(),
                speculative_save_warning: doc.speculative_save_warning(),
            })
        });
        let live_watch = LiveWatchUiSnapshot {
            active: doc.is_live_watching(),
            tail_follow: doc.live_watch_tail_follow,
            can_start: doc.can_start_live_watch(),
            unavailable_reason: doc.live_watch_unavailable_reason(),
            backend_label: doc
                .live_watch
                .as_ref()
                .map(|w| w.backend().label())
                .or_else(|| doc.live_watch_snapshot.as_ref().map(|s| s.backend.label())),
            following_unlinked: doc
                .live_watch_snapshot
                .as_ref()
                .is_some_and(|s| s.status == quarry_core::WatchStatus::FollowingUnlinked),
            appended_rows: doc
                .live_watch_snapshot
                .as_ref()
                .map_or(0, |s| s.appended_rows),
            provisional_tail_bytes: doc
                .live_watch_snapshot
                .as_ref()
                .and_then(|s| s.provisional_tail.as_ref().map(|tail| tail.byte_len)),
            alert: doc.live_watch_alert.clone(),
        };
        let bookmark_count = doc.bookmarks.len();
        let bookmarks = doc
            .bookmarks
            .iter()
            .map(|bookmark| BookmarkListEntry {
                local_row: bookmark.local_row,
                label: doc.bookmark_row_label(bookmark),
                record_start_offset: bookmark.key.record_start_offset,
                global_ordinal: bookmark.key.global_ordinal,
            })
            .collect::<Vec<_>>();
        let split_at_bookmarks_disabled_reason = doc.split_at_bookmarks_disabled_reason();
        Some(DocumentSnapshot {
            document_id: doc_id,
            instance_id: doc.instance_id,
            path: doc.logical_path.clone(),
            filename,
            file_size: doc.session.file_size,
            active_byte_len,
            word_wrap,
            word_wrap_threshold_bytes,
            section,
            live_watch,
            bookmark_count,
            bookmarks,
            split_at_bookmarks_disabled_reason,
            delimiter: doc.session.dialect.delimiter,
            has_header: doc.session.dialect.has_header,
            encoding: doc.session.encoding(),
            headers,
            shown_columns: doc.columns.visible.clone(),
            total_rows: doc.available_data_rows(),
            available_rows: doc.grid_total_rows(),
            filter_active,
            filter_done,
            filter_running: doc.is_filter_scanning(),
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
            selected_rows_count: doc.selected_row_count(),
            selected_columns: doc.selected_columns.iter().copied().collect(),
            index_done: doc.job.is_none() && doc.progress.done && !doc.progress.cancelled,
            index_cancelled: doc.job.is_none() && doc.progress.cancelled,
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
            can_cancel_indexing: doc.job.is_some() && !doc.progress.done,
            dirty: doc.is_dirty(),
            saving: doc.save_job.is_some(),
            save_cancel_requested: doc.save_cancel_requested,
            save_status: doc.save_status.clone(),
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
            pending_diagnostic_message: doc
                .pending_diagnostic
                .as_ref()
                .and_then(|_| doc.pending_diagnostic_message.clone()),
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

    pub fn toggle_live_watch_active(
        &mut self,
        window: WindowId,
        cx: &mut Context<Self>,
    ) -> Result<String, AppMessage> {
        let Some(id) = self.scheduler.active_document(window) else {
            return Err(AppMessage::warning("Open a file first."));
        };
        let watching = self
            .scheduler
            .tab(id)
            .and_then(|t| t.document.as_ref())
            .is_some_and(quarry_workbench::Document::is_live_watching);
        if watching {
            self.stop_live_watch_active(window, cx)
        } else {
            self.start_live_watch_active_with_config(
                window,
                quarry_core::WatchConfig {
                    cache_config: if self.scheduler.index_cache_enabled() {
                        quarry_core::IndexCacheConfig::default()
                    } else {
                        quarry_core::IndexCacheConfig::disabled()
                    },
                    ..Default::default()
                },
                cx,
            )
        }
    }

    pub fn start_live_watch_active_with_config(
        &mut self,
        window: WindowId,
        config: quarry_core::WatchConfig,
        cx: &mut Context<Self>,
    ) -> Result<String, AppMessage> {
        let Some(id) = self.scheduler.active_document(window) else {
            return Err(AppMessage::warning("Open a file first."));
        };
        if let Some(doc) = self.scheduler.tab_mut(id).and_then(|t| t.document.as_mut()) {
            doc.live_watch_async_viewport = true;
        }
        self.scheduler.start_live_watch_with_config(id, config)?;
        self.spawn_live_watch_viewport_refresh(id, cx);
        let backend = self
            .scheduler
            .tab(id)
            .and_then(|t| t.document.as_ref())
            .and_then(|d| d.live_watch.as_ref())
            .map_or("poll", |w| w.backend().label());
        let msg = format!("Live Watch started ({backend}).");
        self.push_hud_announcement(window, msg.clone(), false, cx);
        cx.notify();
        Ok(msg)
    }

    pub fn stop_live_watch_active(
        &mut self,
        window: WindowId,
        cx: &mut Context<Self>,
    ) -> Result<String, AppMessage> {
        let Some(id) = self.scheduler.active_document(window) else {
            return Err(AppMessage::warning("Open a file first."));
        };
        let alert = self.scheduler.stop_live_watch(id)?;
        self.live_watch_tail_read_in_flight.remove(&id);
        let entry = self.viewport_read_seq.entry(id).or_insert(0);
        *entry = entry.wrapping_add(1);
        let (msg, is_alert) = match alert {
            Some(alert) => (alert.summary(), true),
            None => ("Live Watch stopped.".to_owned(), false),
        };
        self.push_hud_announcement(window, msg.clone(), is_alert, cx);
        cx.notify();
        Ok(msg)
    }

    pub fn set_live_watch_tail_follow_active(
        &mut self,
        window: WindowId,
        follow: bool,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(id) = self.scheduler.active_document(window) else {
            return Ok(());
        };
        let changed = {
            let Some(doc) = self.scheduler.tab_mut(id).and_then(|t| t.document.as_mut()) else {
                return Ok(());
            };
            if !doc.is_live_watching() || doc.live_watch_tail_follow == follow {
                return Ok(());
            }
            doc.set_live_watch_tail_follow(follow)?;
            true
        };
        if changed {
            if follow {
                self.spawn_live_watch_viewport_refresh(id, cx);
            }
            let text = if follow {
                "Following live tail.".to_owned()
            } else {
                "Tail follow paused; still reading new rows.".to_owned()
            };
            self.push_hud_announcement(window, text, false, cx);
            cx.notify();
        }
        Ok(())
    }

    pub fn dismiss_live_watch_alert_active(&mut self, window: WindowId, cx: &mut Context<Self>) {
        if let Some(id) = self.scheduler.active_document(window)
            && let Some(doc) = self.scheduler.tab_mut(id).and_then(|t| t.document.as_mut())
        {
            doc.dismiss_live_watch_alert();
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
        self.clear_viewport_read_state(id);
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
                    SchedulerError::OpenFailedUninspectable(message) => AppMessage::error(message),
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
                SchedulerError::OpenFailedUninspectable(message) => AppMessage::error(message),
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
        let _ = self.maybe_spawn_live_watch_tail_refresh(id, cx);
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
            let _ = self.maybe_spawn_live_watch_tail_refresh(target, cx);
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
        self.clear_viewport_read_state(id);
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
        self.clear_viewport_read_state(id);
        self.pending_reveal_cells.remove(&id);
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

    pub fn set_active_selection(&mut self, window: WindowId, selection: Option<GridSelection>) {
        if let Some(doc) = self.active_document_mut(window) {
            doc.selection = selection;
        }
    }

    /// Selects a data cell and clears competing row/column selections so Copy and dialogs target it unambiguously.
    pub fn select_cell_active(
        &mut self,
        window: WindowId,
        target: Option<(u64, usize)>,
        cx: &mut Context<Self>,
    ) {
        if let Some(doc) = self.active_document_mut(window) {
            doc.selection = target.map(|(row, column)| GridSelection::Cell { row, column });
            if target.is_some() {
                doc.clear_row_selection();
                doc.selected_columns.clear();
                doc.column_selection_anchor = None;
            }
            cx.notify();
        }
    }

    pub fn active_selection(&self, window: WindowId) -> Option<GridSelection> {
        self.active_document(window)?.selection
    }

    /// Applies an immediate single-predicate filter for a cell's raw value (`Filter to/out This Value`).
    pub fn filter_by_cell_value_active(
        &mut self,
        window: WindowId,
        source_col: usize,
        value: Vec<u8>,
        equals: bool,
        case_sensitivity: CaseSensitivity,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let query = FilterQuery {
            predicates: vec![FilterPredicate {
                column: source_col,
                operator: if equals {
                    FilterOperator::Equals
                } else {
                    FilterOperator::NotEquals
                },
                upper_bound: None,
                value,
            }],
            case_sensitivity,
        };
        self.start_filter_active(window, query, cx)
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
        if let Some(conflict) = self.scheduler.sibling_dirty_conflict(expected_doc_id) {
            return Err(conflict);
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
        if let Some(conflict) = self.scheduler.sibling_dirty_conflict(expected_doc_id) {
            return Err(conflict);
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

    pub fn copy_cell_active(
        &mut self,
        window: WindowId,
        source_row: u64,
        source_col: usize,
    ) -> Result<String, String> {
        let Some(doc) = self.active_document_mut(window) else {
            return Err("Open a file first.".into());
        };
        let previous = doc.selection;
        doc.selection = Some(GridSelection::Cell {
            row: source_row,
            column: source_col,
        });
        let result = doc.copy_selection_text();
        if result.is_err() {
            doc.selection = previous;
        }
        result
    }

    pub fn copy_row_active(&mut self, window: WindowId, source_row: u64) -> Result<String, String> {
        let Some(doc) = self.active_document_mut(window) else {
            return Err("Open a file first.".into());
        };
        let previous = doc.selection;
        doc.selection = Some(GridSelection::Row { row: source_row });
        let result = doc.copy_selection_text();
        if result.is_err() {
            doc.selection = previous;
        }
        result
    }

    pub fn cancel_save_active(&mut self, window: WindowId, cx: &mut Context<Self>) {
        if let Some(id) = self.scheduler.active_document(window)
            && self.scheduler.cancel_save(id)
        {
            cx.notify();
        }
    }

    pub fn cancel_indexing_active(&mut self, window: WindowId, cx: &mut Context<Self>) {
        if let Some(id) = self.scheduler.active_document(window)
            && self.scheduler.cancel_indexing(id)
        {
            cx.notify();
        }
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
        let doc_path = self.scheduler.tab_logical_path(id);
        if let Some(dest) = destination.as_ref() {
            if let Some(warning) = gzip_destination_warning(dest) {
                return Err(warning);
            }
            let aliases_other =
                self.path_reserved(dest) && doc_path.as_ref().is_none_or(|p| p != dest);
            if aliases_other {
                return Err(AppMessage::warning(
                    "That destination is already open or reserved in another tab.",
                ));
            }
        } else {
            if let Some(conflict) = self.scheduler.sibling_dirty_conflict(id) {
                return Err(AppMessage::warning(conflict));
            }
            if let Some(parent_path) = doc_path.as_deref() {
                let target_norm = Self::normalize_output_alias(parent_path);
                let matches_parent = |res: &Path| {
                    res == parent_path || Self::normalize_output_alias(res) == target_norm
                };
                let write_conflict = self.combine.reserved_output().is_some_and(matches_parent)
                    || self.join.reserved_output().is_some_and(matches_parent)
                    || self
                        .join
                        .retired_reserved_outputs()
                        .into_iter()
                        .any(matches_parent)
                    || self.inspect.reserved_output().is_some_and(matches_parent)
                    || self
                        .inspect
                        .retired_reserved_outputs()
                        .into_iter()
                        .any(matches_parent);
                if write_conflict {
                    return Err(AppMessage::warning(
                        "Another file operation is currently writing to that destination. Wait for it to finish before saving.",
                    ));
                }
            }
        }
        let target_dest = destination.clone().or(doc_path);
        let save_fds = if destination.is_none() && self.document_is_section(id) {
            5
        } else {
            2
        };
        self.scheduler.with_reserved_job_fds(
            id,
            save_fds,
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
        self.clear_viewport_read_state(id);
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
                self.clear_viewport_read_state(document_id);
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

    /// Returns true when `document_id` is a bounded section document.
    pub fn document_is_section(&self, document_id: DocumentId) -> bool {
        self.scheduler.tab_section_bounds(document_id).is_some()
    }

    /// Returns true when `document_id` is a `Verified` bounded section that supports in-place
    /// guarded splice-save back into its parent file.
    pub fn document_can_splice_save(&self, document_id: DocumentId) -> bool {
        self.scheduler
            .tab(document_id)
            .and_then(|tab| tab.document.as_ref())
            .is_some_and(quarry_workbench::Document::can_splice_save_section)
    }

    /// Returns `(path, speculative_save_warning)` for `document_id` if available.
    pub fn document_save_as_info(
        &self,
        document_id: DocumentId,
    ) -> Option<(PathBuf, Option<&'static str>)> {
        let path = self.scheduler.tab_logical_path(document_id)?;
        let warning = self
            .scheduler
            .tab(document_id)
            .and_then(|tab| tab.document.as_ref())
            .and_then(quarry_workbench::Document::speculative_save_warning);
        Some((path, warning))
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
        self.save_in_review_with_destination(expected_token, document_id, None, cx)
    }

    /// Same as [`Self::save_in_review`], writing to an explicit Save As destination (used for
    /// dirty section documents whose in-place Save is disabled).
    pub fn save_as_in_review(
        &mut self,
        expected_token: u64,
        document_id: DocumentId,
        destination: PathBuf,
        cx: &mut Context<Self>,
    ) -> Result<(), AppMessage> {
        self.save_in_review_with_destination(expected_token, document_id, Some(destination), cx)
    }

    fn save_in_review_with_destination(
        &mut self,
        expected_token: u64,
        document_id: DocumentId,
        destination: Option<PathBuf>,
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
        if let Err(error) = self.start_save_document(document_id, destination, None, cx) {
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

    /// Toggles a session-only bookmark on the currently selected/active row.
    pub fn toggle_bookmark_active(
        &mut self,
        window: WindowId,
        cx: &mut Context<Self>,
    ) -> Result<String, String> {
        let Some(doc) = self.active_document_mut(window) else {
            return Err("Open a file before adding bookmarks.".into());
        };
        let (local_row, added) = doc.toggle_bookmark_current()?;
        let label = doc.bookmarks.get(local_row).map_or_else(
            || {
                let display = doc
                    .bookmark_global_ordinal(local_row)
                    .unwrap_or_else(|| local_row.saturating_add(1).saturating_sub(doc.data_start));
                format!("Row {}", quarry_workbench::group_digits(display))
            },
            |bookmark| doc.bookmark_row_label(bookmark),
        );
        let message = if added {
            format!("Bookmarked {label}.")
        } else {
            format!("Removed bookmark on {label}.")
        };
        self.set_window_message(window, Some(AppMessage::status(message.clone())), cx);
        cx.notify();
        Ok(message)
    }

    /// Toggles a session-only bookmark on a specific physical row (e.g. gutter right-click).
    pub fn toggle_bookmark_row_active(
        &mut self,
        window: WindowId,
        local_row: u64,
        cx: &mut Context<Self>,
    ) -> Result<String, String> {
        let Some(doc) = self.active_document_mut(window) else {
            return Err("Open a file before adding bookmarks.".into());
        };
        let added = doc.toggle_bookmark_row(local_row)?;
        let label = doc.bookmarks.get(local_row).map_or_else(
            || {
                let display = doc
                    .bookmark_global_ordinal(local_row)
                    .unwrap_or_else(|| local_row.saturating_add(1).saturating_sub(doc.data_start));
                format!("Row {}", quarry_workbench::group_digits(display))
            },
            |bookmark| doc.bookmark_row_label(bookmark),
        );
        let message = if added {
            format!("Bookmarked {label}.")
        } else {
            format!("Removed bookmark on {label}.")
        };
        self.set_window_message(window, Some(AppMessage::status(message.clone())), cx);
        cx.notify();
        Ok(message)
    }

    pub fn remove_bookmark_row_active(
        &mut self,
        window: WindowId,
        local_row: u64,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(doc) = self.active_document_mut(window) else {
            return false;
        };
        let removed = doc.remove_bookmark_row(local_row);
        if removed {
            self.set_window_message(window, Some(AppMessage::status("Bookmark removed.")), cx);
            cx.notify();
        }
        removed
    }

    pub fn clear_bookmarks_active(&mut self, window: WindowId, cx: &mut Context<Self>) -> usize {
        let Some(doc) = self.active_document_mut(window) else {
            return 0;
        };
        let cleared = doc.clear_bookmarks();
        if cleared > 0 {
            self.set_window_message(
                window,
                Some(AppMessage::status(format!(
                    "Cleared {cleared} row bookmark{}.",
                    if cleared == 1 { "" } else { "s" }
                ))),
                cx,
            );
            cx.notify();
        }
        cleared
    }

    /// Jumps to a specific bookmarked physical row, returning `(table_row, status_message)`.
    pub fn jump_to_bookmark_active(
        &mut self,
        window: WindowId,
        local_row: u64,
        cx: &mut Context<Self>,
    ) -> Result<(usize, String), String> {
        let Some(doc) = self.active_document_mut(window) else {
            return Err("Open a file first.".into());
        };
        if doc.source_changed {
            return Err(quarry_workbench::SOURCE_CHANGED_NOTICE.into());
        }
        let Some(bookmark) = doc.bookmarks.get(local_row).copied() else {
            return Err("Bookmark no longer exists.".into());
        };
        let label = doc.bookmark_row_label(&bookmark);
        let table_row = if doc.filter_active() {
            let Some(rel) = doc
                .filtered_rows
                .iter()
                .position(|matched| matched.row == local_row)
            else {
                return Err(format!(
                    "{label} is not in the currently visible filtered page. Clear the filter to jump directly to any bookmarked row."
                ));
            };
            let table_row = usize::try_from(doc.filter_buffer_start)
                .ok()
                .and_then(|start| start.checked_add(rel))
                .ok_or_else(|| {
                    "Bookmarked row is outside the scrollable table range.".to_owned()
                })?;
            doc.selection = Some(quarry_workbench::GridSelection::Row { row: local_row });
            doc.cursor.select_row(local_row);
            doc.selected_rows.clear();
            doc.selected_filtered_rows
                .select(table_row as u64, SelectionModifiers::NONE);
            doc.selected_columns.clear();
            doc.column_selection_anchor = None;
            table_row
        } else {
            if local_row < doc.data_start || local_row >= doc.available_rows() {
                return Err(format!(
                    "Wait for indexing to reach {label} before jumping to it."
                ));
            }
            let rel = local_row - doc.data_start;
            let table_row = usize::try_from(rel)
                .map_err(|_| "Bookmarked row is outside the scrollable table range.".to_owned())?;
            doc.viewport_start = local_row;
            let _ = doc.load_buffer(local_row);
            doc.selection = Some(quarry_workbench::GridSelection::Row { row: local_row });
            doc.cursor.select_row(local_row);
            doc.selected_filtered_rows.clear();
            doc.selected_rows
                .select(local_row, SelectionModifiers::NONE);
            doc.selected_columns.clear();
            doc.column_selection_anchor = None;
            table_row
        };
        let message = format!("Jumped to {label}.");
        self.set_window_message(window, Some(AppMessage::status(message.clone())), cx);
        cx.notify();
        Ok((table_row, message))
    }

    /// Steps to the next (`forward == true`) or previous (`forward == false`) bookmarked row.
    pub fn step_bookmark_active(
        &mut self,
        window: WindowId,
        forward: bool,
        cx: &mut Context<Self>,
    ) -> Result<(usize, String), String> {
        let target_row = {
            let Some(doc) = self.active_document(window) else {
                return Err("Open a file first.".into());
            };
            if doc.source_changed {
                return Err(quarry_workbench::SOURCE_CHANGED_NOTICE.into());
            }
            if doc.bookmarks.is_empty() {
                return Err("No row bookmarks in this document.".into());
            }
            let anchor = doc.bookmark_target_row();
            if doc.filter_active() {
                let visible_bookmarked: Vec<u64> = doc
                    .filtered_rows
                    .iter()
                    .filter(|matched| doc.bookmarks.contains_row(matched.row))
                    .map(|matched| matched.row)
                    .collect();
                if visible_bookmarked.is_empty() {
                    return Err(
                        "No bookmarked rows are visible in the current filtered page. Clear the filter to jump to any bookmark."
                            .into(),
                    );
                }
                if forward {
                    anchor
                        .and_then(|cur| visible_bookmarked.iter().copied().find(|&r| r > cur))
                        .unwrap_or(visible_bookmarked[0])
                } else {
                    anchor
                        .and_then(|cur| visible_bookmarked.iter().rev().copied().find(|&r| r < cur))
                        .unwrap_or(*visible_bookmarked.last().unwrap())
                }
            } else {
                let candidate = if forward {
                    doc.next_bookmark(anchor)
                } else {
                    doc.previous_bookmark(anchor)
                };
                candidate
                    .map(|bm| bm.local_row)
                    .ok_or_else(|| "No row bookmarks in this document.".to_owned())?
            }
        };
        self.jump_to_bookmark_active(window, target_row, cx)
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
                let Some(ordinal) = doc.filtered_ordinal_for_source_row(source_row) else {
                    return;
                };
                if !shift
                    && !toggle
                    && doc.selected_filtered_rows.contains(ordinal)
                    && doc.selected_filtered_rows.count() == 1
                {
                    doc.clear_row_selection();
                    if matches!(doc.selection, Some(GridSelection::Row { .. })) {
                        doc.selection = None;
                    }
                } else {
                    doc.selected_filtered_rows.select(
                        ordinal,
                        SelectionModifiers {
                            shift,
                            command: toggle,
                        },
                    );
                    let active_row = doc
                        .selected_filtered_rows
                        .contains(ordinal)
                        .then_some(source_row)
                        .or_else(|| {
                            doc.filtered_rows
                                .iter()
                                .enumerate()
                                .find(|(rel, _)| {
                                    doc.selected_filtered_rows.contains(
                                        doc.filter_buffer_start.saturating_add(*rel as u64),
                                    )
                                })
                                .map(|(_, r)| r.row)
                        });
                    doc.selection = active_row.map(|row| GridSelection::Row { row });
                    if !doc.selected_filtered_rows.is_empty() {
                        doc.selected_columns.clear();
                        doc.column_selection_anchor = None;
                    }
                }
            } else if !shift
                && !toggle
                && doc.selected_rows.contains(source_row)
                && doc.selected_rows.count() == 1
            {
                doc.clear_row_selection();
                if matches!(doc.selection, Some(GridSelection::Row { .. })) {
                    doc.selection = None;
                }
            } else {
                doc.selected_rows.select(
                    source_row,
                    SelectionModifiers {
                        shift,
                        command: toggle,
                    },
                );
                let active_row = doc
                    .selected_rows
                    .contains(source_row)
                    .then_some(source_row)
                    .or_else(|| {
                        (0..doc.visible_row_count())
                            .filter_map(|ix| doc.visible_row(ix).map(|(row, _)| row))
                            .find(|&row| doc.selected_rows.contains(row))
                    })
                    .or_else(|| doc.selected_rows.first());
                doc.selection = active_row.map(|row| GridSelection::Row { row });
                if !doc.selected_rows.is_empty() {
                    doc.selected_columns.clear();
                    doc.column_selection_anchor = None;
                }
            }
            cx.notify();
        }
    }

    /// Clears cell, row, and column selections on the active document (e.g., on Escape).
    pub fn clear_selection_active(&mut self, window: WindowId, cx: &mut Context<Self>) {
        if let Some(doc) = self.active_document_mut(window) {
            doc.selection = None;
            doc.clear_row_selection();
            doc.selected_columns.clear();
            doc.column_selection_anchor = None;
            cx.notify();
        }
    }

    /// Selects `source_row` via keyboard navigation within the `#` row gutter.
    pub fn select_row_keyboard_active(
        &mut self,
        window: WindowId,
        source_row: u64,
        cx: &mut Context<Self>,
    ) {
        if let Some(doc) = self.active_document_mut(window) {
            if doc.filter_active() {
                if let Some(ordinal) = doc.filtered_ordinal_for_source_row(source_row) {
                    doc.selected_filtered_rows
                        .select(ordinal, SelectionModifiers::NONE);
                }
            } else {
                doc.selected_rows
                    .select(source_row, SelectionModifiers::NONE);
            }
            doc.selection = Some(GridSelection::Row { row: source_row });
            doc.selected_columns.clear();
            doc.column_selection_anchor = None;
            cx.notify();
        }
    }

    /// Ensures `source_row` is selected before opening the row gutter context menu.
    pub fn ensure_row_selected_for_context_menu(
        &mut self,
        window: WindowId,
        source_row: u64,
        cx: &mut Context<Self>,
    ) {
        if let Some(doc) = self.active_document_mut(window) {
            if doc.filter_active() {
                if let Some(ordinal) = doc.filtered_ordinal_for_source_row(source_row)
                    && !doc.selected_filtered_rows.contains(ordinal)
                {
                    doc.selected_filtered_rows
                        .select(ordinal, SelectionModifiers::NONE);
                }
            } else if !doc.selected_rows.contains(source_row) {
                doc.selected_rows
                    .select(source_row, SelectionModifiers::NONE);
            }
            doc.selection = Some(GridSelection::Row { row: source_row });
            doc.selected_columns.clear();
            doc.column_selection_anchor = None;
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
            doc.selection = None;
            doc.clear_row_selection();
            cx.notify();
        }
    }

    /// Ensures `source_col` is selected before opening the column header context menu.
    pub fn ensure_column_selected_for_context_menu(
        &mut self,
        window: WindowId,
        source_col: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(doc) = self.active_document_mut(window) {
            let total = doc.known_width();
            if source_col >= total {
                return;
            }
            if !doc.selected_columns.contains(&source_col) {
                doc.selected_columns.clear();
                doc.selected_columns.insert(source_col);
                doc.column_selection_anchor = Some(source_col);
            }
            doc.selection = None;
            doc.clear_row_selection();
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
        if let Some(conflict) = self.scheduler.sibling_dirty_conflict(doc_id) {
            return Err(conflict);
        }
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
        if let Some(conflict) = self.scheduler.sibling_dirty_conflict(expected_doc_id) {
            return Err(AppMessage::warning(conflict));
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
        if let Some(conflict) = self.scheduler.sibling_dirty_conflict(expected_doc_id) {
            return Err(AppMessage::warning(conflict));
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
        if let Some(conflict) = self.scheduler.sibling_dirty_conflict(expected_doc_id) {
            return Err(AppMessage::warning(conflict));
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
        if let Some(conflict) = self.scheduler.sibling_dirty_conflict(doc_id) {
            return Err(AppMessage::warning(conflict));
        }
        let notice = self.scheduler.confirm_duplicate_removal(doc_id)?;
        self.clear_viewport_read_state(doc_id);
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
        if let Some(conflict) = self.scheduler.sibling_dirty_conflict(doc_id) {
            return Err(AppMessage::warning(conflict));
        }
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
        if let Some(conflict) = self.scheduler.sibling_dirty_conflict(expected_doc_id) {
            return Err(AppMessage::warning(conflict));
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
        if let Some(conflict) = self.scheduler.sibling_dirty_conflict(expected_doc_id) {
            return Err(AppMessage::warning(conflict));
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
        if let Some(conflict) = self.scheduler.sibling_dirty_conflict(expected_doc_id) {
            return Err(AppMessage::warning(conflict));
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
        if let Some(conflict) = self.scheduler.sibling_dirty_conflict(doc_id) {
            return Err(AppMessage::warning(conflict));
        }
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
        if let Some(conflict) = self.scheduler.sibling_dirty_conflict(doc_id) {
            return Err(AppMessage::warning(conflict));
        }
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
        if let Some(conflict) = self.scheduler.sibling_dirty_conflict(expected_doc_id) {
            return Err(AppMessage::warning(conflict));
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
        if let Some(conflict) = self.scheduler.sibling_dirty_conflict(expected_doc_id) {
            return Err(AppMessage::warning(conflict));
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
        if let Some(conflict) = self.scheduler.sibling_dirty_conflict(expected_doc_id) {
            return Err(AppMessage::warning(conflict));
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
        let default_dir = quarry_core::default_temp_dir();
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

    /// Returns `true` whenever any workflow, document job, Live Watch session, or whitespace
    /// inspection batch requires active 32 ms background polling. When `false`, the GPUI
    /// coordinator parks with zero timers and zero main-thread wake-ups until signalled.
    pub fn has_active_background_work(&self) -> bool {
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
                        .is_some_and(WorkbenchTab::has_background_activity)
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
        if !latest.metrics_eq(&self.hud_sample) {
            self.hud_sample = latest;
            changed = true;
        }
        for window in windows {
            let (doc_instance, doc_dir, phase, committed_rows, committed_bytes, file_size) =
                match self.active_document(window) {
                    Some(doc) => {
                        let phase =
                            if doc.job.is_none() && doc.progress.done && !doc.progress.cancelled {
                                HudIndexPhase::Complete
                            } else if doc.job.is_none() && doc.progress.cancelled {
                                HudIndexPhase::Cancelled
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
        let pre_watch_states: Vec<(WindowId, DocumentId, u64, bool)> = self
            .scheduler
            .windows()
            .into_iter()
            .filter_map(|win| {
                let doc_id = self.scheduler.active_document(win)?;
                let doc = self.scheduler.tab(doc_id)?.document.as_ref()?;
                let watch_gen = doc.live_watch_snapshot.as_ref().map_or(0, |s| s.generation);
                Some((win, doc_id, watch_gen, doc.live_watch_alert.is_some()))
            })
            .collect();
        let poll_errors = self.scheduler.poll_all();
        for (win, doc_id, prev_gen, had_alert) in pre_watch_states {
            if let Some(doc) = self.scheduler.tab(doc_id).and_then(|t| t.document.as_ref()) {
                let cur_gen = doc.live_watch_snapshot.as_ref().map_or(0, |s| s.generation);
                if cur_gen != prev_gen {
                    changed = true;
                }
                if !had_alert && let Some(alert) = &doc.live_watch_alert {
                    self.last_hud_announcement.insert(
                        win,
                        HudAnnouncement {
                            text: alert.summary(),
                            is_alert: true,
                        },
                    );
                    changed = true;
                }
            }
        }
        if !poll_errors.is_empty() {
            for (doc_id, error) in poll_errors {
                if let Some(tab) = self.scheduler.tab_mut(doc_id) {
                    tab.notice = Some(AppMessage::error(error));
                }
            }
            changed = true;
        }
        let active_doc_ids: Vec<DocumentId> = self
            .windows
            .keys()
            .filter_map(|&win| self.scheduler.active_document(win))
            .collect();
        for doc_id in active_doc_ids {
            if let Some(doc) = self
                .scheduler
                .tab_mut(doc_id)
                .and_then(|t| t.document.as_mut())
                && doc.whitespace.poll()
            {
                changed = true;
            }
            if self.maybe_spawn_live_watch_tail_refresh(doc_id, cx) {
                changed = true;
            }
        }
        let blocked_auto_reopen: BTreeSet<DocumentId> = self
            .editing_windows
            .iter()
            .filter_map(|&window_id| self.scheduler.active_document(window_id))
            .collect();
        for completion in self
            .scheduler
            .poll_section_verifications_with_blocked(&blocked_auto_reopen)
        {
            changed = true;
            if completion.reopened {
                self.clear_viewport_read_state(completion.document_id);
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
                            let is_dirty = tab
                                .document
                                .as_ref()
                                .is_some_and(quarry_workbench::Document::is_dirty);
                            tab.notice = Some(AppMessage::warning(if is_dirty {
                                "Verified section boundaries shifted. Save your unsaved draft as a new file before reopening at the verified bounds."
                            } else {
                                "Verified section boundaries shifted. Finish the open edit or active operation, then click Reopen Verified."
                            }));
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
                    self.clear_viewport_read_state(completion.document_id);
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

impl Drop for WorkbenchController {
    fn drop(&mut self) {
        self.ui_wakeup.shutdown();
    }
}

#[cfg(test)]
mod wakeup_tests {
    use super::UiWakeup;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll, Wake, Waker};

    struct CountWaker(AtomicUsize);
    impl Wake for CountWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }

    #[test]
    fn ui_wakeup_parks_until_notified_and_exits_on_shutdown() {
        let wakeup = UiWakeup::new();
        let counter = Arc::new(CountWaker(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counter));
        let mut cx = Context::from_waker(&waker);

        let mut fut = wakeup.wait();
        assert_eq!(Pin::new(&mut fut).poll(&mut cx), Poll::Pending);
        assert_eq!(counter.0.load(Ordering::Acquire), 0);

        wakeup.notify();
        assert_eq!(counter.0.load(Ordering::Acquire), 1);
        assert_eq!(Pin::new(&mut fut).poll(&mut cx), Poll::Ready(true));

        let mut fut2 = wakeup.wait();
        assert_eq!(Pin::new(&mut fut2).poll(&mut cx), Poll::Pending);
        wakeup.shutdown();
        assert_eq!(counter.0.load(Ordering::Acquire), 2);
        assert_eq!(Pin::new(&mut fut2).poll(&mut cx), Poll::Ready(false));
    }
}
