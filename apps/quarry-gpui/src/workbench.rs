//! Process-wide multi-window workbench controller backed by `quarry_workbench::WorkbenchScheduler`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use gpui_kit::component::Root;
use gpui_kit::{App, AppContext, Context, Entity, Global, WindowHandle};
use quarry_core::{Encoding, FilterQuery, HeaderMode, OpenOptions, raise_nofile_limit};
use quarry_workbench::{
    AppMessage, CommandContext, CommandId, CommandResolution, DEFAULT_NOFILE_TARGET, Document,
    DocumentId, MessageSeverity, ResidencyState, SchedulerError, WakeOutcome, WindowId,
    WorkbenchScheduler, WorkbenchSchedulerConfig, WorkbenchTab, evaluate_command,
};

/// Maximum single-cell edit payload accepted by the modal cell editor (1 MiB).
pub const MAX_EDIT_BYTES: usize = 1024 * 1024;

/// Read-only snapshot of an active hydrated document for toolbar, panels, and status bar rendering.
#[derive(Clone, Debug, PartialEq)]
pub struct DocumentSnapshot {
    pub document_id: DocumentId,
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
    },
    Quit {
        remaining_dirty: Vec<(WindowId, DocumentId)>,
    },
}

struct WorkbenchGlobal(Entity<WorkbenchController>);

impl Global for WorkbenchGlobal {}

pub struct WorkbenchController {
    scheduler: WorkbenchScheduler,
    next_window_id: u64,
    windows: BTreeMap<WindowId, WindowHandle<Root>>,
    show_all_whitespace: bool,
    window_messages: BTreeMap<WindowId, AppMessage>,
    pending_review: Option<PendingReviewScope>,
    pending_close_windows: Vec<WindowId>,
    quit_requested_complete: bool,
}

impl WorkbenchController {
    pub fn new(config: WorkbenchSchedulerConfig) -> Self {
        Self {
            scheduler: WorkbenchScheduler::new(config),
            next_window_id: 1,
            windows: BTreeMap::new(),
            show_all_whitespace: false,
            window_messages: BTreeMap::new(),
            pending_review: None,
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
        self.window_messages.remove(&id);
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
        Some(DocumentSnapshot {
            document_id: doc_id,
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
        let active_id = self
            .scheduler
            .active_document(target_window)
            .or_else(|| ids.first().copied())
            .ok_or_else(|| "No files were opened.".to_string())?;
        let owner_window = self.scheduler.window_of(active_id).unwrap_or(target_window);
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
        self.scheduler
            .reopen_tab(window, id, options, encoding)
            .map_err(|e| e.to_string())?;
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
        if let Some(id) = self.scheduler.active_document(window)
            && let Some(tab) = self.scheduler.tab_mut(id)
        {
            tab.notice = None;
        }
        let _ = self.scheduler.sync_fd_leases();
        cx.notify();
        Ok(())
    }

    pub fn rename_header_active(
        &mut self,
        window: WindowId,
        source_col: usize,
        name: String,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let Some(doc) = self.active_document_mut(window) else {
            return Err("Open a file first.".into());
        };
        doc.rename_header(source_col, name)?;
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
            let active_path = self
                .active_document(window)
                .map(|doc| doc.logical_path.clone());
            let aliases_other = self.scheduler.path_reserved(dest)
                && active_path.as_ref().is_none_or(|p| p != dest);
            if aliases_other {
                return Err(AppMessage::warning(
                    "That destination is already open or reserved in another tab.",
                ));
            }
        }
        let Some(id) = self.scheduler.active_document(window) else {
            return Err(AppMessage::warning("Open a file first."));
        };
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

    /// Starts a single-tab close review. Closes immediately if clean & idle; otherwise asks the
    /// window to prompt Save / Discard / Keep Editing.
    pub fn request_close_tab(
        &mut self,
        window: WindowId,
        id: DocumentId,
        close_window_if_last: bool,
        cx: &mut Context<Self>,
    ) -> CloseReviewAction {
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
            self.pending_review = Some(PendingReviewScope::SingleTab {
                window_id: window,
                document_id: id,
                close_window_if_last,
            });
            cx.notify();
            return CloseReviewAction::ConfirmTab {
                window_id: window,
                document_id: id,
                filename,
            };
        }

        let _ = self.scheduler.close_tab(id);
        let empty_now = self.scheduler.tabs_in_window(window).is_empty();
        if empty_now && close_window_if_last {
            let _ = self.scheduler.close_window(window);
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
            self.pending_review = Some(PendingReviewScope::Window {
                window_id: window,
                remaining_dirty: dirty,
            });
            cx.notify();
            return CloseReviewAction::ConfirmTab {
                window_id: window,
                document_id: first_dirty,
                filename,
            };
        }
        let _ = self.scheduler.close_window(window);
        self.pending_close_windows.push(window);
        cx.notify();
        CloseReviewAction::Proceed
    }

    /// Starts unified cross-window Quit review across every open window and dormant dirty tab.
    pub fn request_quit(&mut self, cx: &mut Context<Self>) -> CloseReviewAction {
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
            self.pending_review = Some(PendingReviewScope::Quit {
                remaining_dirty: dirty,
            });
            cx.notify();
            return CloseReviewAction::ConfirmTab {
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
    pub fn cancel_close_review(&mut self, cx: &mut Context<Self>) {
        if let Some(review) = self.pending_review.take() {
            match review {
                PendingReviewScope::SingleTab { document_id, .. } => {
                    if let Some(tab) = self.scheduler.tab_mut(document_id) {
                        tab.cancel_close();
                    }
                }
                PendingReviewScope::Window {
                    remaining_dirty, ..
                } => {
                    for id in remaining_dirty {
                        if let Some(tab) = self.scheduler.tab_mut(id) {
                            tab.cancel_close();
                        }
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
    pub fn approve_discard_in_review(&mut self, cx: &mut Context<Self>) -> CloseReviewAction {
        let Some(review) = self.pending_review.take() else {
            return CloseReviewAction::Proceed;
        };
        match review {
            PendingReviewScope::SingleTab {
                window_id,
                document_id,
                close_window_if_last,
            } => {
                if let Some(tab) = self.scheduler.tab_mut(document_id) {
                    tab.approve_discard_and_close();
                }
                let _ = self.scheduler.close_tab(document_id);
                if close_window_if_last && self.scheduler.tabs_in_window(window_id).is_empty() {
                    let _ = self.scheduler.close_window(window_id);
                    self.pending_close_windows.push(window_id);
                }
                cx.notify();
                CloseReviewAction::Proceed
            }
            PendingReviewScope::Window {
                window_id,
                mut remaining_dirty,
            } => {
                if !remaining_dirty.is_empty() {
                    let approved = remaining_dirty.remove(0);
                    if let Some(tab) = self.scheduler.tab_mut(approved) {
                        tab.approve_discard_and_close();
                    }
                }
                if let Some(&next_id) = remaining_dirty.first() {
                    if let Err(error) = self.scheduler.activate_tab(window_id, next_id) {
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
                    self.pending_review = Some(PendingReviewScope::Window {
                        window_id,
                        remaining_dirty,
                    });
                    cx.notify();
                    CloseReviewAction::ConfirmTab {
                        window_id,
                        document_id: next_id,
                        filename,
                    }
                } else {
                    let _ = self.scheduler.close_window(window_id);
                    self.pending_close_windows.push(window_id);
                    cx.notify();
                    CloseReviewAction::Proceed
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
                    self.pending_review = Some(PendingReviewScope::Quit { remaining_dirty });
                    cx.notify();
                    CloseReviewAction::ConfirmTab {
                        window_id: next_window,
                        document_id: next_doc,
                        filename,
                    }
                } else {
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

    /// Starts saving the currently reviewed dirty tab and marks it to finish closing once saved.
    pub fn save_in_review(
        &mut self,
        window: WindowId,
        cx: &mut Context<Self>,
    ) -> Result<(), AppMessage> {
        let Some(id) = self.scheduler.active_document(window) else {
            return Err(AppMessage::warning("Open a file first."));
        };
        if let Some(tab) = self.scheduler.tab_mut(id) {
            tab.close_confirmation_open = false;
            tab.close_after_save = true;
        }
        if let Err(error) = self.start_save_active(window, None, None, cx) {
            if let Some(tab) = self.scheduler.tab_mut(id) {
                tab.close_after_save = false;
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn take_pending_close_windows(&mut self) -> Vec<WindowId> {
        std::mem::take(&mut self.pending_close_windows)
    }

    pub fn take_quit_complete(&mut self) -> bool {
        std::mem::take(&mut self.quit_requested_complete)
    }

    pub fn poll_background(&mut self, cx: &mut Context<Self>) {
        let mut changed = false;
        let poll_errors = self.scheduler.poll_all();
        if !poll_errors.is_empty() {
            for (doc_id, error) in poll_errors {
                if let Some(tab) = self.scheduler.tab_mut(doc_id) {
                    tab.notice = Some(AppMessage::error(error));
                }
            }
            changed = true;
        }
        let save_results = self.scheduler.poll_saves();
        let mut advance_review = false;
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
                            advance_review = true;
                        }
                    }
                }
                Err(error) => {
                    if let Some(tab) = self.scheduler.tab_mut(completion.document_id) {
                        tab.close_after_save = false;
                        tab.notice = Some(error);
                    }
                    self.cancel_close_review(cx);
                }
            }
        }
        if advance_review {
            let _ = self.approve_discard_in_review(cx);
        }
        // Check if any hydrated document has live indexing/filtering/saving progress to paint.
        for window in self.scheduler.windows() {
            if let Some(doc_id) = self.scheduler.active_document(window)
                && let Some(tab) = self.scheduler.tab(doc_id)
                && tab.document_busy()
            {
                changed = true;
                break;
            }
        }
        if changed {
            cx.notify();
        }
    }
}
