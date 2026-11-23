//! Process-wide multi-window workbench controller backed by `quarry_workbench::WorkbenchScheduler`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::component::Root;
use gpui_kit::{App, AppContext, Context, Entity, Global, WindowHandle};
use quarry_core::{
    CaseSensitivity, ColumnCleanup, DateFormatSuggestion, DuplicateTarget, Encoding, FilterQuery,
    HeaderMode, OpenOptions, SortPlan, raise_nofile_limit,
};
use quarry_workbench::{
    AppMessage, CommandContext, CommandId, CommandResolution, DEFAULT_NOFILE_TARGET, Document,
    DocumentId, MessageSeverity, ResidencyState, SchedulerError, SelectionModifiers,
    StructuralDialog, StructuralJob, WakeOutcome, WindowId, WorkbenchScheduler,
    WorkbenchSchedulerConfig, WorkbenchTab, evaluate_command, materialization_progress,
    select_column, sort_progress,
};

/// Maximum single-cell edit payload accepted by the modal cell editor (1 MiB).
pub const MAX_EDIT_BYTES: usize = 1024 * 1024;

/// Summary of a completed duplicate scan awaiting user confirmation to remove duplicates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DuplicatePreviewInfo {
    pub duplicate_rows: u64,
    pub unique_rows: u64,
    pub total_rows: u64,
}

/// Read-only snapshot of an active hydrated document for toolbar, panels, and status bar rendering.
#[derive(Clone, Debug, PartialEq)]
pub struct DocumentSnapshot {
    pub document_id: DocumentId,
    pub instance_id: u64,
    pub path: PathBuf,
    pub filename: String,
    pub file_size: u64,
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
    pub index_fraction: f32,
    pub dirty: bool,
    pub saving: bool,
    pub busy: bool,
    pub can_undo: bool,
    pub can_redo: bool,
    pub source_changed: bool,
    pub applied_delimiter: Option<u8>,
    pub applied_header_mode: HeaderMode,
    pub applied_encoding: Option<Encoding>,
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

pub struct WorkbenchController {
    scheduler: WorkbenchScheduler,
    next_window_id: u64,
    next_review_token: u64,
    windows: BTreeMap<WindowId, WindowHandle<Root>>,
    editing_windows: BTreeSet<WindowId>,
    viewport_read_seq: BTreeMap<DocumentId, u64>,
    pending_reveal_cells: BTreeMap<WindowId, (u64, usize)>,
    show_all_whitespace: bool,
    window_messages: BTreeMap<WindowId, AppMessage>,
    pending_review: Option<PendingReviewScope>,
    active_review_token: Option<u64>,
    pending_review_action: Option<CloseReviewAction>,
    pending_close_windows: Vec<WindowId>,
    quit_requested_complete: bool,
}

impl WorkbenchController {
    pub fn new(config: WorkbenchSchedulerConfig) -> Self {
        Self {
            scheduler: WorkbenchScheduler::new(config),
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
        let entity = cx.new(|_| Self::new(WorkbenchSchedulerConfig::default()));
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
                    poll_entity.update(cx, |this, cx| {
                        this.poll_background(cx);
                    });
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
            write_slot_available: self.scheduler.write_slot_available(),
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
        Some(DocumentSnapshot {
            document_id: doc_id,
            instance_id: doc.instance_id,
            path: doc.logical_path.clone(),
            filename,
            file_size: doc.session.file_size,
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
            index_fraction: if doc.session.file_size == 0 {
                1.0
            } else {
                (doc.progress.bytes_scanned as f32 / doc.session.file_size as f32).clamp(0.0, 1.0)
            },
            dirty: doc.is_dirty(),
            saving: doc.save_job.is_some(),
            busy: tab.document_writing(),
            can_undo: doc.can_undo(),
            can_redo: doc.can_redo(),
            source_changed: doc.source_changed,
            applied_delimiter: tab.delimiter_mode.delimiter(),
            applied_header_mode: tab.header_mode,
            applied_encoding: tab.encoding_mode,
        })
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
    ) -> Result<(WindowId, DocumentId), String> {
        if paths.is_empty() {
            return Err("No file selected.".into());
        }
        if paths.len() == 1 {
            let path = paths.into_iter().next().unwrap();
            let existing_owner = self.scheduler.find_owner_for_path(&path);
            if let Some((owner_win, doc_id)) = existing_owner
                && self.editing_windows.contains(&owner_win)
                && self.scheduler.active_document(owner_win) != Some(doc_id)
            {
                if let Some(handle) = self.window_handle(owner_win) {
                    let _ = handle.update(cx, |_, window, _| window.activate_window());
                }
                return Err(
                    "Apply or cancel the open edit in the owning window before switching tabs."
                        .into(),
                );
            }
            let outcome = self
                .scheduler
                .open_path(target_window, path, options, encoding)
                .map_err(|e| e.to_string())?;
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
            cx.notify();
            return Ok((owner_window, doc_id));
        }

        let ids = self
            .scheduler
            .open_many_lazy(target_window, paths, options, encoding)
            .map_err(|e| e.to_string())?;
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
                .ok_or_else(|| "No files were opened.".to_string())?;
            let owner = self.scheduler.window_of(first_id).unwrap_or(target_window);
            let _ = self.scheduler.activate_tab(owner, first_id);
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
        let (options, encoding) = {
            let tab = self
                .scheduler
                .tab(id)
                .ok_or_else(|| "Open a file first.".to_string())?;
            let options = tab
                .document
                .as_ref()
                .map_or_else(|| tab.open_options(), Document::current_open_options);
            (options, tab.encoding_mode)
        };
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
        if !self.scheduler.write_slot_available() {
            return Err(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish.",
            ));
        }
        if let Some(dest) = destination.as_ref() {
            let doc_path = self.scheduler.tab_logical_path(id);
            let aliases_other =
                self.scheduler.path_reserved(dest) && doc_path.as_ref().is_none_or(|p| p != dest);
            if aliases_other {
                return Err(AppMessage::warning(
                    "That destination is already open or reserved in another tab.",
                ));
            }
        }
        let tab = self
            .scheduler
            .tab_mut(id)
            .ok_or_else(|| AppMessage::warning("Open a file first."))?;
        let doc = tab
            .document
            .as_mut()
            .ok_or_else(|| AppMessage::warning("Open a file first."))?;
        doc.commit_edits();
        let target_dest = destination.clone();
        if encoding == Some(Encoding::Utf8) {
            doc.start_save_operation_format(destination, true)?;
        } else {
            doc.start_save_operation(destination)?;
        }
        tab.write_destination = target_dest;
        tab.notice = None;
        let _ = self.scheduler.sync_fd_leases();
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
        self.pending_reveal_cells.remove(&window)
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
        let Some(doc) = self.active_document_mut(window) else {
            return Err("Open a file first.".into());
        };
        doc.start_find_next_with_case(&query, case_sensitivity)?;
        if let Some(cell) = doc.reveal_cell.take() {
            self.pending_reveal_cells.insert(window, cell);
        }
        let _ = self.scheduler.sync_fd_leases();
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
        let Some(doc) = self.active_document_mut(window) else {
            return Err("Open a file first.".into());
        };
        doc.start_find_previous_with_case(&query, case_sensitivity)?;
        if let Some(cell) = doc.reveal_cell.take() {
            self.pending_reveal_cells.insert(window, cell);
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
        let Some(doc) = self.active_document_mut(window) else {
            return Err("Open a file first.".into());
        };
        doc.replace_current_match_with_case(&query, &replacement, case_sensitivity)?;
        if let Some(cell) = doc.reveal_cell.take() {
            self.pending_reveal_cells.insert(window, cell);
        }
        self.scheduler.revoke_tab_close_approval(doc_id);
        let _ = self.scheduler.sync_fd_leases();
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
        if !self.scheduler.write_slot_available() {
            return Err(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish.",
            ));
        }
        if self.scheduler.active_document(window) != Some(expected_doc_id) {
            return Err(AppMessage::warning(
                "The active document changed before Replace All started.",
            ));
        }
        let Some(doc) = self.active_document_mut(window) else {
            return Err(AppMessage::warning("Open a file first."));
        };
        doc.start_replace_all_with_case(&query, &replacement, case_sensitivity)?;
        self.scheduler.revoke_tab_close_approval(expected_doc_id);
        let _ = self.scheduler.sync_fd_leases();
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
        if !self.scheduler.write_slot_available() {
            return Err(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish.",
            ));
        }
        if self.scheduler.active_document(window) != Some(expected_doc_id) {
            return Err(AppMessage::warning(
                "The active document changed before Export Filtered started.",
            ));
        }
        if self.scheduler.path_reserved(&destination) {
            return Err(AppMessage::warning(
                "That destination is already open or reserved in another tab.",
            ));
        }
        let Some(tab) = self.scheduler.tab_mut(expected_doc_id) else {
            return Err(AppMessage::warning("Open a file first."));
        };
        let Some(doc) = tab.document.as_mut() else {
            return Err(AppMessage::warning("Open a file first."));
        };
        doc.start_filtered_export(destination.clone())?;
        tab.write_destination = Some(destination);
        tab.notice = None;
        let _ = self.scheduler.sync_fd_leases();
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
        if !self.scheduler.write_slot_available() {
            return Err(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish.",
            ));
        }
        if self.scheduler.active_document(window) != Some(expected_doc_id) {
            return Err(AppMessage::warning(
                "The active document changed before Sort started.",
            ));
        }
        let Some(doc) = self.active_document_mut(window) else {
            return Err(AppMessage::warning("Open a file first."));
        };
        doc.start_sort_plan(plan)?;
        self.scheduler.revoke_tab_close_approval(expected_doc_id);
        let _ = self.scheduler.sync_fd_leases();
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
        if !self.scheduler.write_slot_available() {
            return Err(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish.",
            ));
        }
        if self.scheduler.active_document(window) != Some(expected_doc_id) {
            return Err(AppMessage::warning(
                "The active document changed before duplicate search started.",
            ));
        }
        let Some(doc) = self.active_document_mut(window) else {
            return Err(AppMessage::warning("Open a file first."));
        };
        doc.start_find_duplicates_target(target, case_sensitivity)?;
        let _ = self.scheduler.sync_fd_leases();
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
        if !self.scheduler.write_slot_available() {
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
        if !self.scheduler.write_slot_available() {
            return Err(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish.",
            ));
        }
        if self.scheduler.active_document(window) != Some(expected_doc_id) {
            return Err(AppMessage::warning(
                "The active document changed before cleanup started.",
            ));
        }
        let Some(doc) = self.active_document_mut(window) else {
            return Err(AppMessage::warning("Open a file first."));
        };
        doc.start_cleanup(cleanup)?;
        self.scheduler.revoke_tab_close_approval(expected_doc_id);
        let _ = self.scheduler.sync_fd_leases();
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
        if !self.scheduler.write_slot_available() {
            return Err(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish.",
            ));
        }
        if self.scheduler.active_document(window) != Some(expected_doc_id) {
            return Err(AppMessage::warning(
                "The active document changed before row insertion started.",
            ));
        }
        let Some(doc) = self.active_document_mut(window) else {
            return Err(AppMessage::warning("Open a file first."));
        };
        doc.insert_rows_from_dialog(dialog)?;
        self.scheduler.revoke_tab_close_approval(expected_doc_id);
        let _ = self.scheduler.sync_fd_leases();
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
        if !self.scheduler.write_slot_available() {
            return Err(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish.",
            ));
        }
        if self.scheduler.active_document(window) != Some(expected_doc_id) {
            return Err(AppMessage::warning(
                "The active document changed before column insertion started.",
            ));
        }
        let Some(doc) = self.active_document_mut(window) else {
            return Err(AppMessage::warning("Open a file first."));
        };
        doc.insert_columns_from_dialog(dialog)?;
        self.scheduler.revoke_tab_close_approval(expected_doc_id);
        let _ = self.scheduler.sync_fd_leases();
        cx.notify();
        Ok(())
    }

    pub fn start_delete_selected_rows_active(
        &mut self,
        window: WindowId,
        cx: &mut Context<Self>,
    ) -> Result<(), AppMessage> {
        if !self.scheduler.write_slot_available() {
            return Err(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish.",
            ));
        }
        let Some(doc_id) = self.scheduler.active_document(window) else {
            return Err(AppMessage::warning("Open a file first."));
        };
        let Some(doc) = self.active_document_mut(window) else {
            return Err(AppMessage::warning("Open a file first."));
        };
        let ranges = doc.selected_rows.ranges.clone();
        doc.start_delete_rows(ranges)?;
        self.scheduler.revoke_tab_close_approval(doc_id);
        let _ = self.scheduler.sync_fd_leases();
        cx.notify();
        Ok(())
    }

    pub fn start_delete_selected_columns_active(
        &mut self,
        window: WindowId,
        columns: Vec<usize>,
        cx: &mut Context<Self>,
    ) -> Result<(), AppMessage> {
        if !self.scheduler.write_slot_available() {
            return Err(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish.",
            ));
        }
        let Some(doc_id) = self.scheduler.active_document(window) else {
            return Err(AppMessage::warning("Open a file first."));
        };
        let Some(doc) = self.active_document_mut(window) else {
            return Err(AppMessage::warning("Open a file first."));
        };
        doc.start_delete_columns(columns)?;
        self.scheduler.revoke_tab_close_approval(doc_id);
        let _ = self.scheduler.sync_fd_leases();
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
        if !self.scheduler.write_slot_available() {
            return Err(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish.",
            ));
        }
        if self.scheduler.active_document(window) != Some(expected_doc_id) {
            return Err(AppMessage::warning(
                "The active document changed before moving columns.",
            ));
        }
        let Some(doc) = self.active_document_mut(window) else {
            return Err(AppMessage::warning("Open a file first."));
        };
        doc.start_move_columns(columns, position)?;
        self.scheduler.revoke_tab_close_approval(expected_doc_id);
        let _ = self.scheduler.sync_fd_leases();
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
        if !self.scheduler.write_slot_available() {
            return Err(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish.",
            ));
        }
        if self.scheduler.active_document(window) != Some(expected_doc_id) {
            return Err(AppMessage::warning(
                "The active document changed before splitting the column.",
            ));
        }
        let Some(doc) = self.active_document_mut(window) else {
            return Err(AppMessage::warning("Open a file first."));
        };
        doc.start_split(source_column, separator)?;
        self.scheduler.revoke_tab_close_approval(expected_doc_id);
        let _ = self.scheduler.sync_fd_leases();
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
        if !self.scheduler.write_slot_available() {
            return Err(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish.",
            ));
        }
        if self.scheduler.active_document(window) != Some(expected_doc_id) {
            return Err(AppMessage::warning(
                "The active document changed before combining columns.",
            ));
        }
        let Some(doc) = self.active_document_mut(window) else {
            return Err(AppMessage::warning("Open a file first."));
        };
        doc.start_combine(columns, separator)?;
        self.scheduler.revoke_tab_close_approval(expected_doc_id);
        let _ = self.scheduler.sync_fd_leases();
        cx.notify();
        Ok(())
    }

    fn any_hydrated_tab_busy(&self) -> bool {
        self.scheduler.windows().into_iter().any(|window| {
            self.scheduler.tabs_in_window(window).into_iter().any(|id| {
                self.scheduler
                    .tab(id)
                    .is_some_and(WorkbenchTab::document_busy)
            })
        })
    }

    pub fn poll_background(&mut self, cx: &mut Context<Self>) {
        // Track whether any hydrated tab had an active background job BEFORE polling so
        // the exact tick a job finishes cleanly still notifies observers.
        let mut changed = self.any_hydrated_tab_busy();
        let poll_errors = self.scheduler.poll_all();
        if !poll_errors.is_empty() {
            for (doc_id, error) in poll_errors {
                if let Some(tab) = self.scheduler.tab_mut(doc_id) {
                    tab.notice = Some(AppMessage::error(error));
                }
            }
            changed = true;
        }
        for completion in self.scheduler.poll_searches() {
            changed = true;
            match completion.result {
                Ok(Some(cell)) => {
                    if let Some(win) = self.scheduler.window_of(completion.document_id) {
                        self.pending_reveal_cells.insert(win, cell);
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
        if !changed && self.any_hydrated_tab_busy() {
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }
}
