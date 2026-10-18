use super::*;

const MAX_DOCUMENT_TABS: usize = 8;
const MAX_WRITE_JOBS: usize = 2;

/// Owns one document's drafts, feedback, worker reservations and close decisions.
pub(super) struct DocumentTab {
    pub(super) id: u64,
    pub(super) storage_review: Option<StorageReview>,
    pub(super) storage_approved: bool,
    pub(super) jump_input: String,
    pub(super) find_input: String,
    pub(super) replace_input: String,
    pub(super) find_match_case: bool,
    pub(super) find_bar_open: bool,
    pub(super) replace_expanded: bool,
    pub(super) filter_match_case: bool,
    pub(super) sort_match_case: bool,
    pub(super) column_search_input: String,
    pub(super) columns_open: bool,
    pub(super) structural_dialog: Option<StructuralDialog>,
    pub(super) filter_rules: Vec<FilterRuleDraft>,
    pub(super) filters_open: bool,
    pub(super) delimiter_mode: DelimiterMode,
    pub(super) header_mode: HeaderMode,
    pub(super) encoding_mode: Option<Encoding>,
    pub(super) format_draft: Option<(DelimiterMode, HeaderMode, Option<Encoding>)>,
    pub(super) document: Option<Document>,
    pub(super) notice: Option<AppMessage>,
    pub(super) footer_status: Option<AppMessage>,
    pub(super) close_confirmation_open: bool,
    pub(super) close_after_save: bool,
    pub(super) close_ready: bool,
    pub(super) close_after_jobs: bool,
    managed_column: Option<usize>,
    editor_states: Vec<(egui::Id, egui::text_edit::TextEditState)>,
    pub(super) write_destination: Option<PathBuf>,
}

impl DocumentTab {
    /// Creates an empty tab with an identity that remains stable across switches.
    pub(super) fn new(id: u64) -> Self {
        Self {
            id,
            storage_review: None,
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
            notice: None,
            footer_status: None,
            close_confirmation_open: false,
            close_after_save: false,
            close_ready: false,
            close_after_jobs: false,
            managed_column: None,
            editor_states: Vec::new(),
            write_destination: None,
        }
    }
}

impl DocumentTab {
    /// Lists shared text-widget IDs whose cursor and Undo state must follow this tab.
    fn editor_ids(&self) -> Vec<egui::Id> {
        let mut ids = [
            JUMP_INPUT_ID,
            FIND_INPUT_ID,
            REPLACE_INPUT_ID,
            COLUMN_SEARCH_INPUT_ID,
            STRUCTURAL_SEPARATOR_INPUT_ID,
            STRUCTURAL_POSITION_INPUT_ID,
        ]
        .into_iter()
        .map(egui::Id::new)
        .collect::<Vec<_>>();
        for rule in 0..self.filter_rules.len() {
            ids.extend([
                filter_column_input_id(rule),
                filter_value_input_id(rule),
                filter_upper_bound_input_id(rule),
            ]);
        }
        ids
    }

    /// Adds unsaved-change and activity markers to the document's filename.
    fn title(&self) -> String {
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
                    if document.is_dirty() { " ●" } else { "" },
                    if self.busy() { " ⋯" } else { "" }
                )
            },
        )
    }

    /// Includes read workers as well as operations that materialize files.
    pub(super) fn busy(&self) -> bool {
        self.document.as_ref().is_some_and(|document| {
            document.job.is_some()
                || document.search_job.is_some()
                || document.filter_job.is_some()
                || document.filter_rows_loading()
                || document.save_job.is_some()
                || document.export_job.is_some()
                || document.structural_job.is_some()
        })
    }

    /// Counts file operations and structural analysis against the global job limit.
    fn writing(&self) -> bool {
        self.document.as_ref().is_some_and(|document| {
            document.save_job.is_some()
                || document.export_job.is_some()
                || document
                    .structural_job
                    .as_ref()
                    .is_some_and(|job| !matches!(job, StructuralJob::DuplicatePreview { .. }))
        })
    }
}

/// Compares canonical paths, canonical parents of new outputs, and Unix file identities.
/// This protects destinations reached through relative paths, symlinks or hard links.
pub(super) fn paths_alias(left: &Path, right: &Path) -> bool {
    /// Resolves a new destination through its parent when the file does not exist yet.
    fn normalized(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| {
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            std::fs::canonicalize(parent)
                .unwrap_or_else(|_| parent.to_path_buf())
                .join(path.file_name().unwrap_or_default())
        })
    }
    let left_normalized = normalized(left);
    let right_normalized = normalized(right);
    if left_normalized == right_normalized {
        return true;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let (Ok(left), Ok(right)) = (std::fs::metadata(left), std::fs::metadata(right)) {
            return left.dev() == right.dev() && left.ino() == right.ino();
        }
    }
    #[cfg(target_os = "macos")]
    if left_normalized.parent() == right_normalized.parent()
        && let (Some(parent), Some(left), Some(right)) = (
            left_normalized.parent(),
            left_normalized.file_name(),
            right_normalized.file_name(),
        )
    {
        // Existing file identities above remain authoritative. Pending outputs
        // need the target volume's comparison rules before either file exists.
        return macos_names_alias(
            left,
            right,
            macos_volume_case_sensitive(parent).unwrap_or(false),
        );
    }
    false
}

/// Reads the target volume's case-sensitivity capability without creating files.
#[cfg(target_os = "macos")]
fn macos_volume_case_sensitive(parent: &Path) -> Option<bool> {
    use std::os::unix::ffi::OsStrExt;
    let parent = std::ffi::CString::new(parent.as_os_str().as_bytes()).ok()?;
    let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // SAFETY: CString supplies a terminated path and statfs writes the complete
    // correctly sized output on success. No uninitialized output is read on failure.
    if unsafe { libc::statfs(parent.as_ptr(), filesystem.as_mut_ptr()) } != 0 {
        return None;
    }
    // SAFETY: statfs succeeded and initialized its output, including the mount path.
    let filesystem = unsafe { filesystem.assume_init() };
    let mut attributes = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_CAPABILITIES,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    #[repr(C)]
    struct VolumeAttributes {
        length: u32,
        capabilities: libc::vol_capabilities_attr_t,
    }
    let mut output = VolumeAttributes {
        length: 0,
        capabilities: libc::vol_capabilities_attr_t {
            capabilities: [0; 4],
            valid: [0; 4],
        },
    };
    // SAFETY: statfs returned a terminated mount path. The C-layout buffer has
    // a length prefix and exactly the requested volume-capabilities attribute.
    let status = unsafe {
        libc::getattrlist(
            filesystem.f_mntonname.as_ptr(),
            (&mut attributes as *mut libc::attrlist).cast(),
            (&mut output as *mut VolumeAttributes).cast(),
            std::mem::size_of::<VolumeAttributes>(),
            0,
        )
    };
    let format = libc::VOL_CAPABILITIES_FORMAT;
    let sensitive = libc::VOL_CAP_FMT_CASE_SENSITIVE;
    if status != 0
        || output.length as usize != std::mem::size_of::<VolumeAttributes>()
        || output.capabilities.valid[format] & sensitive == 0
    {
        return None;
    }
    Some(output.capabilities.capabilities[format] & sensitive != 0)
}

/// Compares native names with canonical Unicode equivalence and the volume's case rule.
#[cfg(target_os = "macos")]
fn macos_names_alias(
    left: &std::ffi::OsStr,
    right: &std::ffi::OsStr,
    case_sensitive: bool,
) -> bool {
    use objc2_foundation::{NSComparisonResult, NSString, NSStringCompareOptions};
    use std::os::unix::ffi::OsStrExt;
    match (left.to_str(), right.to_str()) {
        (Some(left), Some(right)) => {
            let options = if case_sensitive {
                NSStringCompareOptions::empty()
            } else {
                NSStringCompareOptions::CaseInsensitiveSearch
            };
            NSString::from_str(left).compare_options(&NSString::from_str(right), options)
                == NSComparisonResult::Same
        }
        _ if case_sensitive => left == right,
        _ => left.as_bytes().eq_ignore_ascii_case(right.as_bytes()),
    }
}

impl QuarryApp {
    /// Visits the active slot and every retained tab without changing visible focus.
    pub(super) fn document_tabs(&self) -> impl Iterator<Item = &DocumentTab> {
        std::iter::once(&self.active).chain(self.tabs.iter())
    }

    /// Commits grid drafts, swaps ownership, and schedules restoration of tab UI state.
    fn activate_tab(&mut self, id: u64) {
        if self.active.id == id {
            return;
        }
        if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
            if let Some(document) = self.active.document.as_mut() {
                document.commit_edits();
            }
            std::mem::swap(&mut self.active, &mut self.tabs[index]);
            if self.tabs[index].document.is_none() {
                self.tabs.remove(index);
            }
            self.pending_tab_focus = true;
            self.reveal_tab = true;
        }
    }

    /// Saves outgoing editor state and restores the selected tab before further input.
    fn switch_tab(&mut self, ctx: &egui::Context, id: u64) {
        if self.active.id == id {
            return;
        }
        self.remember_tab_focus(ctx);
        self.activate_tab(id);
        self.reset_tab_focus(ctx);
    }

    /// Captures text cursor/Undo state and the column manager's selection for this tab.
    pub(super) fn remember_tab_focus(&mut self, ctx: &egui::Context) {
        if self.pending_tab_focus {
            return;
        }
        self.active.managed_column = ctx
            .data_mut(|data| data.get_persisted(egui::Id::new("quarry-selected-managed-column")));
        self.active.editor_states = self
            .active
            .editor_ids()
            .into_iter()
            .filter_map(|id| egui::TextEdit::load_state(ctx, id).map(|state| (id, state)))
            .collect();
    }

    /// Restores only the incoming tab's widget state, releasing focus and stale popups.
    pub(super) fn reset_tab_focus(&mut self, ctx: &egui::Context) {
        if !self.pending_tab_focus {
            return;
        }
        self.pending_tab_focus = false;
        let mut ids = self.active.editor_ids();
        for tab in self.document_tabs() {
            for (id, _) in &tab.editor_states {
                if !ids.contains(id) {
                    ids.push(*id);
                }
            }
        }
        for id in ids {
            self.active
                .editor_states
                .iter()
                .find(|(saved, _)| *saved == id)
                .map(|(_, state)| state.clone())
                .unwrap_or_default()
                .store(ctx, id);
        }
        ctx.memory_mut(|memory| {
            if let Some(focused) = memory.focused() {
                memory.surrender_focus(focused);
            }
        });
        ctx.data_mut(|data| {
            let key = egui::Id::new("quarry-selected-managed-column");
            data.remove::<usize>(key);
            if let Some(column) = self.active.managed_column {
                data.insert_persisted(key, column);
            }
        });
        egui::Popup::close_all(ctx);
    }

    /// Selects an already-open source or prepares a new tab without replacing old work.
    /// Closing decisions, reserved outputs and the document cap can reject the request.
    pub(super) fn open_document_tab(
        &mut self,
        path: PathBuf,
        options: OpenOptions,
    ) -> Result<(), AppMessage> {
        self.open_document_tab_with_encoding(path, options, None)
    }

    pub(super) fn open_document_tab_with_encoding(
        &mut self,
        path: PathBuf,
        options: OpenOptions,
        encoding: Option<Encoding>,
    ) -> Result<(), AppMessage> {
        if self.quit_pending
            || self.active.close_confirmation_open
            || self.active.close_after_save
            || self.active.close_after_jobs
        {
            return Err(AppMessage::warning(
                "Finish closing the document, or keep editing, before opening another file.",
            ));
        }
        let existing = self.document_tabs().find_map(|tab| {
            tab.document
                .as_ref()
                .filter(|doc| {
                    paths_alias(&path, &doc.logical_path) || paths_alias(&path, doc.session.path())
                })
                .map(|doc| (tab.id, doc.session.encoding()))
        });
        if let Some((id, current)) = existing {
            self.activate_tab(id);
            if encoding.is_some_and(|requested| requested != current) {
                return Err(AppMessage::warning(
                    "This file is already open. Use Format > Encoding to reopen it with another encoding.",
                ));
            }
            return Ok(());
        }
        if self.document_tabs().any(|tab| {
            tab.write_destination
                .as_ref()
                .is_some_and(|destination| paths_alias(&path, destination))
        }) {
            return Err(AppMessage::warning(
                "Wait for the operation writing this file to finish before opening it.",
            ));
        }
        if self
            .document_tabs()
            .filter(|tab| tab.document.is_some())
            .count()
            >= MAX_DOCUMENT_TABS
        {
            return Err(AppMessage::warning(
                "Quarry supports eight open tabs. Close a tab before opening another file.",
            ));
        }
        let mut document =
            Document::prepare_with_encoding(&path, options, encoding).map_err(AppMessage::error)?;
        document.working_directory = self.working_directory.clone();
        document.start_indexing().map_err(AppMessage::error)?;
        if let Some(current) = self.active.document.as_mut() {
            current.commit_edits();
        }
        let mut tab = DocumentTab::new(self.next_tab_id);
        self.next_tab_id += 1;
        tab.delimiter_mode = DelimiterMode::ALL
            .into_iter()
            .find(|mode| mode.delimiter() == options.delimiter)
            .unwrap_or(DelimiterMode::Auto);
        tab.header_mode = options.header_mode;
        tab.encoding_mode = encoding;
        tab.document = Some(document);
        let previous = std::mem::replace(&mut self.active, tab);
        self.pending_tab_focus = true;
        self.reveal_tab = true;
        if previous.document.is_some() {
            self.tabs.push(previous);
        }
        Ok(())
    }

    /// Rejects another tab's source/working file and every reserved writer output.
    pub(super) fn destination_available(&self, destination: &Path) -> bool {
        !self.document_tabs().any(|tab| {
            (tab.id != self.active.id
                && tab.document.as_ref().is_some_and(|document| {
                    paths_alias(destination, &document.logical_path)
                        || paths_alias(destination, document.session.path())
                }))
                || tab
                    .write_destination
                    .as_ref()
                    .is_some_and(|path| paths_alias(destination, path))
        })
    }

    /// Applies the shared two-operation limit to document tabs and Combine Files.
    pub(super) fn write_slot_available(&self) -> bool {
        self.document_tabs().filter(|tab| tab.writing()).count()
            + usize::from(
                self.combine_files
                    .as_ref()
                    .is_some_and(combine_files::CombineDialog::busy),
            )
            < MAX_WRITE_JOBS
    }

    /// Polls jobs against their owning tab, surfaces failed reloads, and advances closes.
    /// Background completions retain the visible document and its focus/history.
    pub(super) fn poll_document_tabs(&mut self, ctx: &egui::Context) {
        self.poll_active_document(ctx, true);
        let mut failed_reloads = Vec::new();
        if self.active.document.is_none()
            && let Some(notice) = self.active.notice.take()
        {
            failed_reloads.push(notice.text);
        }
        for index in 0..self.tabs.len() {
            // Poll completions against their owning document, never the visible tab.
            std::mem::swap(&mut self.active, &mut self.tabs[index]);
            self.poll_active_document(ctx, false);
            if self.active.document.is_none()
                && let Some(notice) = self.active.notice.take()
            {
                failed_reloads.push(notice.text);
            }
            std::mem::swap(&mut self.active, &mut self.tabs[index]);
        }
        self.tabs.retain(|tab| tab.document.is_some());
        for tab in std::iter::once(&mut self.active).chain(self.tabs.iter_mut()) {
            if !tab.writing() {
                tab.write_destination = None;
            }
        }
        if self.active.close_ready {
            self.active.close_ready = false;
            self.finish_document_close(ctx);
        } else if self.active.close_after_jobs && !self.active.writing() {
            self.active.close_after_jobs = false;
            self.review_document_close(ctx);
        }
        if self.active.document.is_none()
            && let Some(id) = self.tabs.iter().min_by_key(|tab| tab.id).map(|tab| tab.id)
        {
            self.switch_tab(ctx, id);
        }
        if !failed_reloads.is_empty() {
            if let Some(notice) = self.active.notice.take() {
                failed_reloads.insert(0, notice.text);
            }
            self.active.notice = Some(AppMessage::error(failed_reloads.join("\n")));
        }
    }

    /// Cancels native window close until all tabs and active operations are reviewed.
    pub(super) fn intercept_dirty_close(&mut self, ctx: &egui::Context) {
        if !ctx.input(|input| input.viewport().close_requested()) || self.quit_authorized {
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        if let Some(dialog) = self.combine_files.as_mut() {
            dialog.preserve_for_quit();
            return;
        }
        if self.quit_pending {
            return;
        }
        self.quit_pending = true;
        self.quit_approved.clear();
        self.review_document_close(ctx);
    }

    /// Starts the active tab's close review without requesting an application quit.
    pub(super) fn request_tab_close(&mut self, ctx: &egui::Context) {
        if self.active.document.is_some() {
            self.review_document_close(ctx);
        }
    }

    /// Commits drafts, waits for file operations, then prompts for unsaved changes.
    fn review_document_close(&mut self, ctx: &egui::Context) {
        if let Some(document) = self.active.document.as_mut() {
            document.commit_edits();
            if document.save_job.is_some() {
                self.active.close_after_save = true;
                return;
            }
            if self.active.writing() {
                self.active.close_after_jobs = true;
                return;
            }
            if self
                .active
                .document
                .as_ref()
                .is_some_and(Document::is_dirty)
            {
                self.active.close_confirmation_open = true;
                return;
            }
        }
        self.finish_document_close(ctx);
    }

    /// Removes an approved tab, or records a quit approval while retaining every tab.
    /// Quit releases documents only after the entire session has been approved.
    fn finish_document_close(&mut self, ctx: &egui::Context) {
        self.active.close_confirmation_open = false;
        self.active.close_after_save = false;
        self.active.close_after_jobs = false;
        if self.quit_pending {
            // Retain even discarded documents until every tab has been reviewed.
            // Cancelling quit therefore leaves the whole session available.
            self.quit_approved.insert(self.active.id);
            let next = self
                .document_tabs()
                .find(|tab| tab.document.is_some() && !self.quit_approved.contains(&tab.id))
                .map(|tab| tab.id);
            if let Some(id) = next {
                self.switch_tab(ctx, id);
                self.review_document_close(ctx);
            } else {
                self.quit_authorized = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        } else {
            self.active = if self.tabs.is_empty() {
                let tab = DocumentTab::new(self.next_tab_id);
                self.next_tab_id += 1;
                tab
            } else {
                self.tabs.remove(
                    self.tabs
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, tab)| tab.id)
                        .unwrap()
                        .0,
                )
            };
            self.pending_tab_focus = true;
            self.reveal_tab = true;
            self.reset_tab_focus(ctx);
            ctx.memory_mut(|memory| {
                if let Some(id) = memory.focused() {
                    memory.surrender_focus(id);
                }
            });
        }
    }

    /// Cancels close/quit intentions and clears approvals without discarding documents.
    pub(super) fn keep_editing(&mut self) {
        self.active.close_confirmation_open = false;
        self.active.close_after_save = false;
        self.active.close_after_jobs = false;
        self.active.close_ready = false;
        self.quit_pending = false;
        self.quit_approved.clear();
    }

    /// Approves the current close; quit keeps the document until all approvals finish.
    pub(super) fn discard_and_close(&mut self, ctx: &egui::Context) {
        self.finish_document_close(ctx);
    }

    /// Renders accessible tabs, handles keyboard switching and reveals active tabs.
    pub(super) fn show_document_tabs(&mut self, ctx: &egui::Context, blocked: bool) {
        let mut tabs: Vec<_> = self
            .document_tabs()
            .filter(|tab| tab.document.is_some())
            .map(|tab| {
                (
                    tab.id,
                    tab.title(),
                    tab.document
                        .as_ref()
                        .unwrap()
                        .logical_path
                        .display()
                        .to_string(),
                    format!(
                        "{}{}{}",
                        if tab.document.as_ref().unwrap().is_dirty() {
                            "● "
                        } else {
                            ""
                        },
                        if tab.busy() { "⋯ " } else { "" },
                        tab.document
                            .as_ref()
                            .unwrap()
                            .logical_path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                    ),
                )
            })
            .collect();
        tabs.sort_by_key(|tab| tab.0);
        let mut select = None;
        let mut close = None;
        let width = ctx.content_rect().width();
        let resized = ctx.data_mut(|data| {
            let key = egui::Id::new("quarry-tab-strip-width");
            let previous = data.get_temp::<f32>(key);
            data.insert_temp(key, width);
            previous != Some(width)
        });
        let reveal = std::mem::take(&mut self.reveal_tab) || resized;
        if !tabs.is_empty() {
            egui::TopBottomPanel::top("quarry-document-tabs")
                .exact_height(30.0)
                .show(ctx, |ui| {
                    ui.add_enabled_ui(!blocked, |ui| {
                        egui::ScrollArea::horizontal()
                            .id_salt("quarry-tab-strip")
                            .animated(false)
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    for (id, title, path, caption) in &tabs {
                                        ui.push_id(id, |ui| {
                                            let response = ui
                                                .add_sized(
                                                    [168.0, 20.0],
                                                    egui::Button::selectable(
                                                        self.active.id == *id,
                                                        egui::RichText::new(caption).size(12.0),
                                                    )
                                                    .truncate(),
                                                )
                                                .on_hover_text(path);
                                            if reveal && self.active.id == *id {
                                                response.scroll_to_me(Some(Align::Center));
                                            }
                                            response.widget_info(|| {
                                                egui::WidgetInfo::selected(
                                                    egui::WidgetType::SelectableLabel,
                                                    !blocked,
                                                    self.active.id == *id,
                                                    format!("Document tab: {title}"),
                                                )
                                            });
                                            if response.clicked() {
                                                select = Some(*id);
                                            }
                                            let response = ui
                                                .small_button("×")
                                                .on_hover_text(format!("Close {path}"));
                                            response.widget_info(|| {
                                                egui::WidgetInfo::labeled(
                                                    egui::WidgetType::Button,
                                                    !blocked,
                                                    format!("Close tab: {title}"),
                                                )
                                            });
                                            if response.clicked() {
                                                close = Some(*id);
                                            }
                                        });
                                    }
                                });
                            });
                    });
                });
        }
        if !blocked {
            if ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::O)) {
                self.choose_file();
            }
            if ctx.input_mut(|input| input.consume_key(egui::Modifiers::COMMAND, egui::Key::W)) {
                close = Some(self.active.id);
            }
            let backwards = ctx.input_mut(|input| {
                input.consume_key(
                    egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
                    egui::Key::Tab,
                )
            });
            let forwards =
                ctx.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, egui::Key::Tab));
            if (backwards || forwards) && !tabs.is_empty() {
                let index = tabs
                    .iter()
                    .position(|tab| tab.0 == self.active.id)
                    .unwrap_or(0);
                select =
                    Some(tabs[(index + if backwards { tabs.len() - 1 } else { 1 }) % tabs.len()].0);
            }
        }
        if let Some(id) = select {
            self.switch_tab(ctx, id);
        }
        if let Some(id) = close {
            self.switch_tab(ctx, id);
            self.request_tab_close(ctx);
        }
        self.reset_tab_focus(ctx);
    }

    /// Shows live progress and cancel/keep-editing controls while a close awaits a job.
    pub(super) fn show_closing_operation(&mut self, ctx: &egui::Context) {
        if !self.active.close_after_jobs && !self.active.close_after_save {
            return;
        }
        if self.active.storage_review.is_some() {
            return;
        }
        let mut keep = false;
        let mut cancel = false;
        let modal = egui::Modal::new(egui::Id::new("quarry-closing-operation")).show(ctx, |ui| {
            ui.heading("Finishing file operation");
            ui.label(self.active.document.as_ref().map(|document| document.logical_path.display().to_string()).unwrap_or_default());
            ui.label("This tab will close after the operation finishes. Unsaved changes will still need your decision.");
            if let Some(job) = self.active.document.as_ref().and_then(active_job_display) { ui.label(job.label); }
            ui.horizontal(|ui| {
                keep = ui.button("Keep Editing").clicked();
                cancel = ui.button("Cancel Operation").clicked();
            });
        });
        if keep || modal.should_close() {
            self.keep_editing();
        } else if cancel && let Some(document) = self.active.document.as_mut() {
            document.cancel_save();
            document.cancel_filtered_export();
            document.cancel_structural_edit();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture(directory: &Path, name: &str) -> PathBuf {
        let path = directory.join(name);
        let mut bytes = b"name,value\n".to_vec();
        for row in 0..200 {
            bytes.extend_from_slice(format!("row{row},{row}\n").as_bytes());
        }
        fs::write(&path, bytes).unwrap();
        path
    }

    fn edited(app: &mut QuarryApp, name: &str) {
        app.active
            .document
            .as_mut()
            .unwrap()
            .rename_header(0, name.into())
            .unwrap();
    }

    fn poll_until_idle(app: &mut QuarryApp, ctx: &egui::Context) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.document_tabs().any(DocumentTab::busy) {
            assert!(Instant::now() < deadline, "tab jobs timed out");
            app.poll_document_tabs(ctx);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn input(events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(860.0, 540.0),
            )),
            events,
            ..Default::default()
        }
    }

    #[test]
    fn switching_preserves_edits_drafts_views_history_and_background_save_owner() {
        let directory = tempfile::tempdir().unwrap();
        let a = fixture(directory.path(), "a.csv");
        let b = fixture(directory.path(), "b.csv");
        let original = fs::read(&a).unwrap();
        let saved = directory.path().join("saved-a.csv");
        let ctx = egui::Context::default();
        let mut app = QuarryApp::new(Some(a.clone()), Instant::now());
        let a_id = app.active.id;
        edited(&mut app, "edited-a");
        app.active
            .document
            .as_mut()
            .unwrap()
            .begin_cell_edit(1, 1, b"0".to_vec())
            .unwrap();
        app.active
            .document
            .as_mut()
            .unwrap()
            .cell_edit
            .as_mut()
            .unwrap()
            .draft = "42".into();
        app.active.find_input = "row100".into();
        app.active.find_bar_open = true;
        app.active.jump_input = "100".into();
        app.active.filter_rules[0].value_input = "private-a".into();
        app.active
            .document
            .as_mut()
            .unwrap()
            .selected_rows
            .select(100, egui::Modifiers::NONE);
        app.open_new_path(b.clone()).unwrap();
        let b_id = app.active.id;
        assert!(!app.active.document.as_ref().unwrap().is_dirty());
        assert!(app.swap_edit_history(false).is_err());
        edited(&mut app, "edited-b");
        app.switch_tab(&ctx, a_id);
        assert_eq!(
            app.active.document.as_ref().unwrap().cell_edits[&(1, 1)].as_slice(),
            b"42"
        );
        assert_eq!(app.active.find_input, "row100");
        assert!(app.active.find_bar_open);
        assert_eq!(app.active.jump_input, "100");
        assert_eq!(app.active.filter_rules[0].value_input, "private-a");
        assert!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .selected_rows
                .contains(100)
        );
        app.swap_edit_history(false).unwrap();
        assert!(
            !app.active
                .document
                .as_ref()
                .unwrap()
                .cell_edits
                .contains_key(&(1, 1))
        );
        app.swap_edit_history(true).unwrap();
        assert!(app.save_as_picker_result(Some(saved.clone())));
        app.switch_tab(&ctx, b_id);
        poll_until_idle(&mut app, &ctx);
        assert_eq!(app.active.id, b_id);
        assert_eq!(
            app.active.document.as_ref().unwrap().column_name(0),
            "edited-b"
        );
        assert!(app.active.document.as_ref().unwrap().can_undo());
        assert_eq!(fs::read(&a).unwrap(), original);
        assert!(
            fs::read_to_string(&saved)
                .unwrap()
                .starts_with("edited-a,value\nrow0,42\n")
        );
        app.switch_tab(&ctx, a_id);
        assert_eq!(app.active.document.as_ref().unwrap().logical_path, saved);
        assert!(!app.active.document.as_ref().unwrap().is_dirty());
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn pending_destinations_follow_volume_case_rules_and_unicode_equivalence() {
        use std::ffi::OsStr;
        let directory = tempfile::tempdir().unwrap();
        let sensitive = macos_volume_case_sensitive(directory.path()).unwrap();
        // Check the capability against actual filesystem behavior, independently
        // of the name comparison used for still-nonexistent reservations below.
        let probe = directory.path().join("case-probe.csv");
        let upper_probe = directory.path().join("CASE-PROBE.csv");
        fs::write(&probe, b"lower").unwrap();
        fs::write(&upper_probe, b"upper").unwrap();
        assert_eq!(
            fs::read(&probe).unwrap(),
            if sensitive { b"lower" } else { b"upper" }
        );
        assert_eq!(paths_alias(&probe, &upper_probe), !sensitive);
        eprintln!("volume case-sensitive: {sensitive}; test directory: {directory:?}");
        let output = directory.path().join("out.csv");
        let upper = directory.path().join("OUT.csv");
        assert!(!output.exists());
        assert!(!upper.exists());
        assert_eq!(paths_alias(&output, &upper), !sensitive);
        assert!(paths_alias(
            &directory.path().join("caf\u{e9}.csv"),
            &directory.path().join("cafe\u{301}.csv")
        ));
        assert!(!paths_alias(
            &output,
            &directory.path().join("different.csv")
        ));
        assert!(!macos_names_alias(
            OsStr::new("out.csv"),
            OsStr::new("OUT.csv"),
            true
        ));
        assert!(macos_names_alias(
            OsStr::new("out.csv"),
            OsStr::new("OUT.csv"),
            false
        ));
        assert!(macos_names_alias(
            OsStr::new("CAF\u{c9}.csv"),
            OsStr::new("cafe\u{301}.CSV"),
            false
        ));
        assert!(!macos_names_alias(
            OsStr::new("caf\u{e9}.csv"),
            OsStr::new("cafe.csv"),
            false
        ));

        let a = fixture(directory.path(), "a.csv");
        let b = fixture(directory.path(), "b.csv");
        let mut app = QuarryApp::new(Some(a), Instant::now());
        // Reserve a still-nonexistent output, as a writer does before publication.
        app.active.write_destination = Some(output.clone());
        app.open_new_path(b).unwrap();
        edited(&mut app, "retained-b");
        assert_eq!(app.destination_available(&upper), sensitive);
        if !sensitive {
            assert!(!app.save_as_picker_result(Some(upper.clone())));
            assert!(
                app.active
                    .notice
                    .as_ref()
                    .unwrap()
                    .contains("being written")
            );
            assert!(app.active.document.as_ref().unwrap().save_job.is_none());
            assert!(!output.exists());
            assert!(!upper.exists());
        }
    }

    #[test]
    fn concurrent_writes_have_a_limit_and_reserve_destinations_across_tabs() {
        let directory = tempfile::tempdir().unwrap();
        let a = fixture(directory.path(), "a.csv");
        let b = fixture(directory.path(), "b.csv");
        let c = fixture(directory.path(), "c.csv");
        let a_out = directory.path().join("a-out.csv");
        let b_out = directory.path().join("b-out.csv");
        let c_out = directory.path().join("c-out.csv");
        let mut app = QuarryApp::new(Some(a.clone()), Instant::now());
        edited(&mut app, "a-edited");
        assert!(app.save_as_picker_result(Some(a_out.clone())));
        app.open_new_path(b.clone()).unwrap();
        edited(&mut app, "b-edited");
        assert!(!app.save_as_picker_result(Some(a.clone())));
        assert!(!app.save_as_picker_result(Some(a_out.clone())));
        assert!(app.open_new_path(a_out.clone()).is_err());
        assert!(app.save_as_picker_result(Some(b_out.clone())));
        app.open_new_path(c.clone()).unwrap();
        edited(&mut app, "c-edited");
        assert!(!app.save_as_picker_result(Some(c_out.clone())));
        assert!(
            app.active
                .notice
                .as_ref()
                .unwrap()
                .contains("Two file operations")
        );
        poll_until_idle(&mut app, &egui::Context::default());
        assert_eq!(app.active.document.as_ref().unwrap().logical_path, c);
        assert!(
            fs::read_to_string(a_out)
                .unwrap()
                .starts_with("a-edited,value")
        );
        assert!(
            fs::read_to_string(b_out)
                .unwrap()
                .starts_with("b-edited,value")
        );
        assert!(!c_out.exists());
        assert!(app.save_as_picker_result(Some(c_out)));
    }

    #[test]
    fn cancelling_quit_retains_even_previously_discarded_tabs_and_their_undo() {
        let directory = tempfile::tempdir().unwrap();
        let a = fixture(directory.path(), "a.csv");
        let b = fixture(directory.path(), "b.csv");
        let mut app = QuarryApp::new(Some(a.clone()), Instant::now());
        edited(&mut app, "a-edited");
        let a_id = app.active.id;
        app.open_new_path(b.clone()).unwrap();
        edited(&mut app, "b-edited");
        let b_id = app.active.id;
        let ctx = egui::Context::default();
        app.switch_tab(&ctx, a_id);
        app.quit_pending = true;
        app.review_document_close(&ctx);
        assert!(app.active.close_confirmation_open);
        app.discard_and_close(&ctx);
        assert_eq!(app.active.id, b_id);
        assert!(app.active.close_confirmation_open);
        app.keep_editing();
        assert!(!app.quit_pending);
        assert!(app.quit_approved.is_empty());
        assert_eq!(
            app.document_tabs()
                .filter(|tab| tab.document.as_ref().is_some_and(Document::is_dirty))
                .count(),
            2
        );
        app.switch_tab(&ctx, a_id);
        assert_eq!(
            app.active.document.as_ref().unwrap().column_name(0),
            "a-edited"
        );
        assert!(app.active.document.as_ref().unwrap().can_undo());
        assert!(fs::read_to_string(a).unwrap().starts_with("name,value"));
        assert!(fs::read_to_string(b).unwrap().starts_with("name,value"));
        app.quit_pending = true;
        let output = ctx.run(input(vec![]), |ctx| {
            app.review_document_close(ctx);
            app.discard_and_close(ctx);
            app.discard_and_close(ctx);
        });
        assert!(app.quit_authorized);
        assert!(
            output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .iter()
                .any(|cmd| matches!(cmd, egui::ViewportCommand::Close))
        );
    }

    #[test]
    fn save_and_close_removes_only_its_tab_and_retains_other_unsaved_work() {
        let directory = tempfile::tempdir().unwrap();
        let a = fixture(directory.path(), "a.csv");
        let b = fixture(directory.path(), "b.csv");
        let out = directory.path().join("saved.csv");
        let mut app = QuarryApp::new(Some(a), Instant::now());
        edited(&mut app, "a-edited");
        let a_id = app.active.id;
        app.open_new_path(b.clone()).unwrap();
        edited(&mut app, "b-edited");
        let b_id = app.active.id;
        let ctx = egui::Context::default();
        app.switch_tab(&ctx, a_id);
        app.request_tab_close(&ctx);
        assert!(app.active.close_confirmation_open);
        app.keep_editing();
        assert_eq!(app.active.id, a_id);
        app.active.close_after_save = true;
        assert!(app.save_as_picker_result(Some(out.clone())));
        poll_until_idle(&mut app, &ctx);
        assert_eq!(app.active.id, b_id);
        assert!(app.tabs.is_empty());
        assert_eq!(
            app.active.document.as_ref().unwrap().column_name(0),
            "b-edited"
        );
        assert!(app.active.document.as_ref().unwrap().can_undo());
        assert!(!app.quit_authorized);
        assert!(
            fs::read_to_string(out)
                .unwrap()
                .starts_with("a-edited,value")
        );
    }

    #[test]
    fn duplicate_opens_preserve_state_and_the_tab_limit_never_evicts_a_document() {
        let directory = tempfile::tempdir().unwrap();
        let paths: Vec<_> = (0..9)
            .map(|index| fixture(directory.path(), &format!("{index}.csv")))
            .collect();
        let mut app = QuarryApp::new(Some(paths[0].clone()), Instant::now());
        let first_id = app.active.id;
        edited(&mut app, "retained");
        app.active.delimiter_mode = DelimiterMode::Comma;
        let alias = directory.path().join("alias.csv");
        fs::hard_link(&paths[0], &alias).unwrap();
        app.open_new_path(alias).unwrap();
        assert_eq!(app.active.id, first_id);
        assert_eq!(app.active.delimiter_mode, DelimiterMode::Comma);
        app.handle_dropped_paths(paths[1..].iter().cloned().map(Some).collect());
        assert_eq!(app.document_tabs().count(), MAX_DOCUMENT_TABS);
        assert!(
            app.active
                .notice
                .as_ref()
                .unwrap()
                .contains("eight open tabs")
        );
        app.open_new_path(paths[0].clone()).unwrap();
        assert_eq!(app.active.id, first_id);
        assert_eq!(
            app.active.document.as_ref().unwrap().column_name(0),
            "retained"
        );
        let before = app.active.id;
        assert!(
            app.open_new_path(directory.path().join("missing.csv"))
                .is_err()
        );
        assert_eq!(app.active.id, before);
        assert!(app.active.document.as_ref().unwrap().can_undo());
    }

    #[test]
    fn opening_a_materialized_working_file_selects_its_owner_without_sharing_its_lifetime() {
        let directory = tempfile::tempdir().unwrap();
        let a = fixture(directory.path(), "a.csv");
        let b = fixture(directory.path(), "b.csv");
        let original = fs::read(&a).unwrap();
        let ctx = egui::Context::default();
        let mut app = QuarryApp::new(Some(a.clone()), Instant::now());
        let a_id = app.active.id;
        poll_until_idle(&mut app, &ctx);
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_sort_rows(
                0,
                SortDirection::Ascending,
                SortMode::Reverse,
                CaseSensitivity::Insensitive,
            )
            .unwrap();
        poll_until_idle(&mut app, &ctx);
        let working = app
            .active
            .document
            .as_ref()
            .unwrap()
            .session
            .path()
            .to_path_buf();
        assert_ne!(working, a);
        assert!(working.exists());
        let alias = directory.path().join("working-alias.csv");
        fs::hard_link(&working, &alias).unwrap();
        app.open_new_path(b.clone()).unwrap();
        let b_id = app.active.id;
        edited(&mut app, "retained-b");
        for path in [working.clone(), alias] {
            app.open_new_path(path).unwrap();
            assert_eq!(app.active.id, a_id);
            assert_eq!(app.document_tabs().count(), 2);
            let document = app.active.document.as_ref().unwrap();
            assert_eq!(document.logical_path, a);
            assert!(document.is_dirty());
            assert!(document.can_undo());
            app.switch_tab(&ctx, b_id);
        }
        app.switch_tab(&ctx, a_id);
        app.discard_and_close(&ctx);
        assert!(!working.exists());
        assert!(app.tabs.is_empty());
        assert_eq!(app.active.id, b_id);
        let retained = app.active.document.as_ref().unwrap();
        assert_eq!(retained.logical_path, b);
        assert_eq!(retained.column_name(0), "retained-b");
        assert!(retained.can_undo());
        assert_eq!(fs::read(a).unwrap(), original);
    }

    #[test]
    fn live_filters_and_structural_working_files_belong_to_their_tabs() {
        let directory = tempfile::tempdir().unwrap();
        let a = fixture(directory.path(), "a.csv");
        let b = fixture(directory.path(), "b.csv");
        let before = fs::read(&b).unwrap();
        let ctx = egui::Context::default();
        let mut app = QuarryApp::new(Some(a), Instant::now());
        let a_id = app.active.id;
        poll_until_idle(&mut app, &ctx);
        app.active.filter_rules[0].value_input = "row1".into();
        app.apply(&ctx, Action::ApplyFilter);
        app.open_new_path(b).unwrap();
        let b_id = app.active.id;
        poll_until_idle(&mut app, &ctx);
        app.active.filter_rules[0].value_input = "row2".into();
        app.apply(&ctx, Action::ApplyFilter);
        poll_until_idle(&mut app, &ctx);
        assert_eq!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .available_filter_rows(),
            11
        );
        app.switch_tab(&ctx, a_id);
        assert_eq!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .available_filter_rows(),
            111
        );
        app.switch_tab(&ctx, b_id);
        app.apply(&ctx, Action::ClearFilter);
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_sort_rows(
                0,
                SortDirection::Ascending,
                SortMode::Reverse,
                CaseSensitivity::Insensitive,
            )
            .unwrap();
        app.switch_tab(&ctx, a_id);
        poll_until_idle(&mut app, &ctx);
        assert_eq!(app.active.id, a_id);
        assert_eq!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .available_filter_rows(),
            111
        );
        app.switch_tab(&ctx, b_id);
        let document = app.active.document.as_ref().unwrap();
        assert!(document.is_dirty());
        let working = document
            .working_copy
            .as_ref()
            .unwrap()
            .directory
            .path()
            .to_path_buf();
        assert!(working.exists());
        assert_eq!(fs::read(&document.logical_path).unwrap(), before);
        app.discard_and_close(&ctx);
        assert!(!working.exists());
        assert_eq!(app.active.id, a_id);
        assert_eq!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .available_filter_rows(),
            111
        );
    }

    #[test]
    fn failed_background_save_reload_surfaces_its_error_and_removes_the_empty_tab() {
        let directory = tempfile::tempdir().unwrap();
        let a = fixture(directory.path(), "a.csv");
        let b = fixture(directory.path(), "b.csv");
        let saved = directory.path().join("moved-saved-a.csv");
        let ctx = egui::Context::default();
        let mut app = QuarryApp::new(Some(a.clone()), Instant::now());
        edited(&mut app, "saved-a");
        assert!(app.save_current());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !app
            .active
            .document
            .as_ref()
            .unwrap()
            .save_job
            .as_ref()
            .unwrap()
            .progress()
            .done
        {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        fs::rename(&a, &saved).unwrap();
        app.open_new_path(b.clone()).unwrap();
        edited(&mut app, "unsaved-b");
        ctx.memory_mut(|memory| memory.request_focus(egui::Id::new(FIND_INPUT_ID)));
        app.poll_document_tabs(&ctx);
        assert!(app.tabs.is_empty());
        assert_eq!(app.active.document.as_ref().unwrap().logical_path, b);
        assert_eq!(
            app.active.document.as_ref().unwrap().column_name(0),
            "unsaved-b"
        );
        assert!(app.active.document.as_ref().unwrap().can_undo());
        let notice = app.active.notice.as_ref().unwrap();
        assert_eq!(notice.severity, MessageSeverity::Error);
        assert!(notice.contains("could not reload"));
        assert!(notice.contains(&a.display().to_string()));
        assert!(ctx.memory(|memory| memory.has_focus(egui::Id::new(FIND_INPUT_ID))));
        assert!(
            fs::read_to_string(saved)
                .unwrap()
                .starts_with("saved-a,value")
        );
    }

    #[test]
    fn failed_active_save_reload_selects_remaining_work_and_preserves_the_error() {
        let directory = tempfile::tempdir().unwrap();
        let a = fixture(directory.path(), "a.csv");
        let b = fixture(directory.path(), "b.csv");
        let saved = directory.path().join("moved-saved-a.csv");
        let ctx = egui::Context::default();
        let mut app = QuarryApp::new(Some(a.clone()), Instant::now());
        let a_id = app.active.id;
        app.open_new_path(b.clone()).unwrap();
        edited(&mut app, "retained-b");
        let b_id = app.active.id;
        app.switch_tab(&ctx, a_id);
        edited(&mut app, "saved-a");
        assert!(app.save_current());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !app
            .active
            .document
            .as_ref()
            .unwrap()
            .save_job
            .as_ref()
            .unwrap()
            .progress()
            .done
        {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        fs::rename(&a, &saved).unwrap();
        app.poll_document_tabs(&ctx);
        assert_eq!(app.active.id, b_id);
        assert!(app.tabs.is_empty());
        let document = app.active.document.as_ref().unwrap();
        assert_eq!(document.logical_path, b);
        assert_eq!(document.column_name(0), "retained-b");
        assert!(document.can_undo());
        let notice = app.active.notice.as_ref().unwrap();
        assert_eq!(notice.severity, MessageSeverity::Error);
        assert!(notice.contains("could not reload"));
        assert!(notice.contains(&a.display().to_string()));
        app.swap_edit_history(false).unwrap();
        assert_eq!(app.active.document.as_ref().unwrap().column_name(0), "name");
        app.swap_edit_history(true).unwrap();
        assert_eq!(
            app.active.document.as_ref().unwrap().column_name(0),
            "retained-b"
        );
        assert!(
            fs::read_to_string(saved)
                .unwrap()
                .starts_with("saved-a,value")
        );
    }

    #[test]
    fn text_editor_cursor_state_is_restored_without_crossing_documents() {
        let directory = tempfile::tempdir().unwrap();
        let a = fixture(directory.path(), "a.csv");
        let b = fixture(directory.path(), "b.csv");
        let ctx = egui::Context::default();
        let id = egui::Id::new(FIND_INPUT_ID);
        let mut app = QuarryApp::new(Some(a), Instant::now());
        app.reset_tab_focus(&ctx);
        let a_id = app.active.id;
        let set_cursor = |position| {
            let mut state = egui::TextEdit::load_state(&ctx, id).unwrap_or_default();
            state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::one(
                    egui::text::CCursor::new(position),
                )));
            state.store(&ctx, id);
        };
        set_cursor(7);
        app.remember_tab_focus(&ctx);
        app.open_new_path(b).unwrap();
        let b_id = app.active.id;
        app.reset_tab_focus(&ctx);
        assert!(
            egui::TextEdit::load_state(&ctx, id)
                .unwrap()
                .cursor
                .char_range()
                .is_none()
        );
        set_cursor(2);
        app.switch_tab(&ctx, a_id);
        assert_eq!(
            egui::TextEdit::load_state(&ctx, id)
                .unwrap()
                .cursor
                .char_range()
                .unwrap()
                .primary
                .index,
            7
        );
        app.switch_tab(&ctx, b_id);
        assert_eq!(
            egui::TextEdit::load_state(&ctx, id)
                .unwrap()
                .cursor
                .char_range()
                .unwrap()
                .primary
                .index,
            2
        );
    }

    #[test]
    fn compact_strip_reveals_the_active_tab_after_open_and_keyboard_navigation() {
        let directory = tempfile::tempdir().unwrap();
        let paths = (0..8)
            .map(|index| fixture(directory.path(), &format!("document-{index}.csv")))
            .collect();
        let mut app = QuarryApp::with_paths(paths, Instant::now());
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut frame = eframe::Frame::_new_kittest();
        let mut width = 1280.0;
        let render = |app: &mut QuarryApp, events, width| {
            let mut raw = input(events);
            raw.screen_rect = Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, 540.0),
            ));
            ctx.run(raw, |ctx| eframe::App::update(app, ctx, &mut frame))
        };
        let mut render = render;
        for _ in 0..4 {
            render(&mut app, vec![], width);
        }
        let assert_visible = |output: egui::FullOutput, filename: &str| {
            let tree = output.platform_output.accesskit_update.unwrap();
            let node = &tree
                .nodes
                .iter()
                .find(|(_, node)| {
                    node.label().is_some_and(|label| {
                        label.starts_with(&format!("Document tab: {filename}"))
                    })
                })
                .unwrap()
                .1;
            let bounds = node.bounds().unwrap();
            assert!(
                bounds.x0 >= 0.0 && bounds.x1 <= 860.0,
                "selected tab is outside strip: {bounds:?}"
            );
        };
        width = 860.0;
        for _ in 0..4 {
            render(&mut app, vec![], width);
        }
        assert_visible(render(&mut app, vec![], width), "document-7.csv");
        render(
            &mut app,
            vec![egui::Event::Key {
                key: egui::Key::Tab,
                modifiers: egui::Modifiers::CTRL,
                pressed: true,
                physical_key: None,
                repeat: false,
            }],
            width,
        );
        for _ in 0..4 {
            render(&mut app, vec![], width);
        }
        assert_visible(render(&mut app, vec![], width), "document-0.csv");
    }

    #[test]
    fn accessible_tabs_and_keyboard_switching_clear_text_focus_and_close_only_one_tab() {
        let directory = tempfile::tempdir().unwrap();
        let a = fixture(directory.path(), "a.csv");
        let b = fixture(directory.path(), "b.csv");
        let mut app = QuarryApp::with_paths(vec![a, b], Instant::now());
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut frame = eframe::Frame::_new_kittest();
        let output = ctx.run(input(vec![]), |ctx| {
            eframe::App::update(&mut app, ctx, &mut frame)
        });
        let tree = output.platform_output.accesskit_update.unwrap();
        for prefix in [
            "Document tab: a.csv",
            "Document tab: b.csv",
            "Close tab: a.csv",
            "Close tab: b.csv",
        ] {
            let node = tree
                .nodes
                .iter()
                .find(|(_, node)| node.label().is_some_and(|label| label.starts_with(prefix)))
                .unwrap()
                .1
                .clone();
            assert!(node.supports_action(egui::accesskit::Action::Click));
            let bounds = node.bounds().unwrap();
            assert!(bounds.y1 <= 30.0 && bounds.x1 <= 860.0);
        }
        ctx.memory_mut(|memory| memory.request_focus(egui::Id::new(FIND_INPUT_ID)));
        let key = |key, modifiers| egui::Event::Key {
            key,
            modifiers,
            pressed: true,
            physical_key: None,
            repeat: false,
        };
        let _ = ctx.run(
            input(vec![key(egui::Key::Tab, egui::Modifiers::CTRL)]),
            |ctx| eframe::App::update(&mut app, ctx, &mut frame),
        );
        assert_eq!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .logical_path
                .file_name()
                .unwrap(),
            "a.csv"
        );
        assert!(!ctx.memory(|memory| memory.has_focus(egui::Id::new(FIND_INPUT_ID))));
        let _ = ctx.run(
            input(vec![key(egui::Key::W, egui::Modifiers::COMMAND)]),
            |ctx| eframe::App::update(&mut app, ctx, &mut frame),
        );
        assert_eq!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .logical_path
                .file_name()
                .unwrap(),
            "b.csv"
        );
        assert!(app.tabs.is_empty());
        assert!(!app.quit_pending);
    }
}
