use super::*;
use quarry_core::{
    CombineJob, CombineOptions, CombinePlan, CombineSummary, StorageSpace,
    check_combine_files_with_encoding, check_storage,
};
use std::thread::{self, JoinHandle};

pub(super) struct CombineDialog {
    paths: Vec<PathBuf>,
    has_header: bool,
    delimiter: DelimiterMode,
    encoding: Option<Encoding>,
    plan: Option<CombinePlan>,
    checking: Option<CombineJob<CombinePlan>>,
    writing: Option<CombineJob<CombineSummary>>,
    storage_check: Option<JoinHandle<Result<StorageSpace, String>>>,
    destination: Option<PathBuf>,
    space: Option<StorageSpace>,
    result: Option<CombineSummary>,
    message: Option<String>,
    problem_path: Option<PathBuf>,
    cancel_requested: bool,
    protected_paths: Vec<PathBuf>,
    cleanup_reservations: Vec<tabs::CleanupReservation>,
    start_allowed: bool,
}

impl Default for CombineDialog {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            has_header: true,
            delimiter: DelimiterMode::Auto,
            encoding: None,
            plan: None,
            checking: None,
            writing: None,
            storage_check: None,
            destination: None,
            space: None,
            result: None,
            message: None,
            problem_path: None,
            cancel_requested: false,
            protected_paths: Vec::new(),
            cleanup_reservations: Vec::new(),
            start_allowed: true,
        }
    }
}

impl CombineDialog {
    /// Blocks implicit quit without changing selections, plans or running operations.
    /// Users must explicitly close this dialog or cancel its operation before quitting.
    pub(super) fn preserve_for_quit(&mut self) {
        let warning = "Close Combine Files before quitting Quarry. Your selected files and running operation are kept.";
        if self
            .message
            .as_ref()
            .is_none_or(|message| !message.contains(warning))
        {
            self.message = Some(self.message.take().map_or_else(
                || warning.to_owned(),
                |message| format!("{warning}\n{message}"),
            ));
        }
    }

    pub(super) fn reserved_output(&self) -> Option<&Path> {
        self.destination.as_deref()
    }

    pub(super) fn busy(&self) -> bool {
        self.checking.is_some() || self.writing.is_some() || self.storage_check.is_some()
    }

    fn invalidate(&mut self) {
        self.plan = None;
        self.destination = None;
        self.space = None;
        self.message = None;
        self.problem_path = None;
    }

    fn record_error(&mut self, error: QuarryError) {
        self.problem_path = match &error {
            QuarryError::CombineFile { path, .. } => Some(path.clone()),
            _ => None,
        };
        self.message = Some(quarry_core::Diagnostic::from_error(&error).details());
    }

    fn start_check(&mut self) {
        if !self.start_allowed {
            self.message = Some("Two file operations are already running.".into());
            return;
        }
        self.invalidate();
        self.cancel_requested = false;
        match check_combine_files_with_encoding(
            self.paths.clone(),
            CombineOptions {
                delimiter: self.delimiter.delimiter(),
                has_header: self.has_header,
            },
            self.encoding,
        ) {
            Ok(job) => self.checking = Some(job),
            Err(error) => self.record_error(error),
        }
    }

    pub(super) fn cancel(&mut self) {
        if let Some(job) = &self.checking {
            job.cancel();
        }
        if let Some(job) = &self.writing {
            job.cancel();
        }
        self.cancel_requested = true;
    }

    fn poll(&mut self) {
        if self
            .checking
            .as_ref()
            .is_some_and(|job| job.progress().done)
        {
            match self.checking.take().unwrap().wait() {
                Ok(Some(plan)) => self.plan = Some(plan),
                Ok(None) => {
                    self.message = Some("File check cancelled. No output was written.".into())
                }
                Err(error) => self.record_error(error),
            }
            self.cancel_requested = false;
        }
        if self
            .storage_check
            .as_ref()
            .is_some_and(|job| job.is_finished())
        {
            match self.storage_check.take().unwrap().join() {
                Ok(Ok(space)) => self.space = Some(space),
                Ok(Err(error)) => self.message = Some(error),
                Err(_) => {
                    self.message = Some("The storage check failed. Choose the output again.".into())
                }
            }
        }
        if self.writing.as_ref().is_some_and(|job| job.progress().done) {
            match self.writing.take().unwrap().wait() {
                Ok(Some(result)) => self.result = Some(result),
                Ok(None) => {
                    self.message =
                        Some("Combination cancelled. No combined file was published.".into())
                }
                Err(error) => {
                    self.record_error(error);
                    self.plan = None;
                    self.space = None;
                }
            }
            self.cancel_requested = false;
        }
    }

    fn choose_destination(&mut self) {
        let extension = if self
            .plan
            .as_ref()
            .is_some_and(|plan| plan.delimiter == b'\t')
        {
            "tsv"
        } else {
            "csv"
        };
        let mut dialog = rfd::FileDialog::new()
            .set_title("Save combined file")
            .set_file_name(format!("combined.{extension}"));
        if let Some(parent) = self.paths.first().and_then(|path| path.parent()) {
            dialog = dialog.set_directory(parent);
        }
        if let Some(destination) = dialog.save_file() {
            self.prepare_destination(destination);
        }
    }

    fn output_is_protected(&self, path: &Path) -> bool {
        self.protected_paths
            .iter()
            .any(|protected| tabs::paths_alias(protected, path))
            || self
                .cleanup_reservations
                .iter()
                .any(|reservation| reservation.contains(path))
    }

    fn prepare_destination(&mut self, destination: PathBuf) {
        if self.output_is_protected(&destination) {
            self.space = None;
            self.destination = None;
            self.message = Some(
                "This file is open or being written in another tab. Choose another output.".into(),
            );
            return;
        }
        let Some(plan) = self.plan.as_ref() else {
            return;
        };
        self.message = None;
        self.space = None;
        let required = plan.output_bytes;
        let path = destination.clone();
        self.storage_check = Some(thread::spawn(move || {
            if path.try_exists().map_err(|error| error.to_string())? {
                return Err("The output file already exists. Choose a new file name; existing files are never overwritten.".into());
            }
            check_storage(
                path.parent()
                    .filter(|path| !path.as_os_str().is_empty())
                    .unwrap_or(Path::new(".")),
                required,
            )
            .map_err(|error| error.to_string())
        }));
        self.destination = Some(destination);
    }

    fn start_write(&mut self) {
        if !self.start_allowed {
            self.message = Some("Two file operations are already running.".into());
            return;
        }
        let (Some(plan), Some(destination), Some(_)) = (&self.plan, &self.destination, &self.space)
        else {
            return;
        };
        if self.output_is_protected(destination) {
            self.message = Some(
                "The output is now reserved by another operation. Choose a different path.".into(),
            );
            return;
        }
        match plan.clone().start_write(destination.clone()) {
            Ok(job) => {
                self.writing = Some(job);
                self.message = None;
                self.cancel_requested = false;
            }
            Err(error) => self.record_error(error),
        }
    }

    fn add_paths(&mut self, paths: Vec<PathBuf>) {
        self.invalidate();
        for path in paths {
            if !self.paths.contains(&path) {
                self.paths.push(path);
            }
        }
    }

    fn show(&mut self, ctx: &egui::Context) -> DialogAction {
        self.poll();
        let mut action = DialogAction::None;
        let busy = self.busy();
        let popup_was_open = egui::Popup::is_any_open(ctx);
        let modal = egui::Modal::new(egui::Id::new("quarry-combine-files"))
            .frame(tool_dialog_frame(ctx)).show(ctx, |ui| {
                tool_dialog_style(ui.style_mut());
                ui.set_width((ctx.content_rect().width() - 64.0).clamp(260.0, 620.0));
                ui.heading("Combine Files");
                ui.label("Put rows from matching files into one new file.");
                ui.separator();
                if let Some(result) = &self.result {
                    ui.label(format!("Saved {} rows ({}).", result.data_rows, format_bytes(result.bytes_written)));
                    ui.add(egui::Label::new(result.destination.display().to_string()).wrap());
                    ui.label("All source files were left unchanged.");
                    if let Some(message) = &self.message { ui.colored_label(ERROR_TEXT, message); }
                    ui.horizontal(|ui| {
                        if ui.button("Close").clicked() { action = DialogAction::Close; }
                        if ui.button("Open Combined File").clicked() {
                            action = DialogAction::Open(result.destination.clone(), self.plan.as_ref().map(|p| p.delimiter).unwrap_or(b','), self.has_header, self.plan.as_ref().map(|p| p.encoding).unwrap_or(Encoding::Utf8));
                        }
                    });
                    return;
                }
                egui::ScrollArea::vertical().id_salt("combine-body")
                    .max_height((ctx.content_rect().height() - 175.0).max(120.0))
                    .show(ui, |ui| {
                ui.add_enabled_ui(!busy, |ui| {
                    let mut move_row = None;
                    let mut remove_row = None;
                    egui::ScrollArea::vertical().id_salt("combine-file-list")
                        .max_height((ctx.content_rect().height() - 350.0).clamp(72.0, 230.0))
                        .show_rows(ui, 28.0, self.paths.len(), |ui, rows| {
                            for index in rows {
                                ui.push_id(index, |ui| {
                                    ui.horizontal(|ui| {
                                        let path = &self.paths[index];
                                        let name = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy();
                                        ui.label(format!("{}.", index + 1));
                                        ui.add_sized([ui.available_width() - 190.0, 24.0], egui::Label::new(name.as_ref()).truncate()).on_hover_text(path.display().to_string());
                                        let up = ui.add_enabled(index > 0, egui::Button::new("Up"));
                                        let down = ui.add_enabled(index + 1 < self.paths.len(), egui::Button::new("Down"));
                                        let remove = ui.button("Remove");
                                        let _ = ui.ctx().accesskit_node_builder(up.id, |node| node.set_label(format!("Move file {} up", index + 1)));
                                        let _ = ui.ctx().accesskit_node_builder(down.id, |node| node.set_label(format!("Move file {} down", index + 1)));
                                        let _ = ui.ctx().accesskit_node_builder(remove.id, |node| node.set_label(format!("Remove file {}", index + 1)));
                                        if up.clicked() { move_row = Some((index, index - 1)); }
                                        if down.clicked() { move_row = Some((index, index + 1)); }
                                        if remove.clicked() { remove_row = Some(index); }
                                    });
                                });
                            }
                        });
                    if let Some((from, to)) = move_row { self.paths.swap(from, to); self.invalidate(); }
                    if let Some(index) = remove_row { self.paths.remove(index); self.invalidate(); }
                    if ui.button("Add Files…").clicked() {
                        if let Some(paths) = rfd::FileDialog::new().set_title("Choose files to combine").pick_files() { self.add_paths(paths); }
                    }
                    let header_changed = ui.checkbox(&mut self.has_header, "Files have a header row").changed();
                    let previous = (self.delimiter, self.encoding);
                    ui.horizontal(|ui| {
                        ui.label("Delimiter");
                        egui::ComboBox::from_id_salt("combine-delimiter").selected_text(self.delimiter.label()).show_ui(ui, |ui| {
                            for mode in DelimiterMode::ALL { ui.selectable_value(&mut self.delimiter, mode, mode.label()); }
                        });
                    });
                    ui.horizontal(|ui| {
                        ui.label("Encoding");
                        egui::ComboBox::from_id_salt("combine-encoding")
                            .selected_text(encoding_choice_label(self.encoding))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.encoding, None, "Auto (BOM or UTF-8)");
                                for encoding in Encoding::ALL {
                                    ui.selectable_value(&mut self.encoding, Some(encoding), encoding.label());
                                }
                            });
                    });
                    if header_changed || previous != (self.delimiter, self.encoding) { self.invalidate(); }
                });
                ui.small(if self.has_header { "Headers must match exactly, in the same order. The output keeps one header." } else { "Every row must have the same number of columns. All rows are kept." });
                ui.small("Uses saved files on disk. Duplicates are kept. Unsaved edits are not included.");
                if let Some(plan) = &self.plan {
                    ui.separator();
                    ui.label(format!("{} matching files · {} columns · {} data rows", plan.inputs.len(), plan.columns, plan.data_rows));
                    ui.label(format!("Output size: {} · {}", format_bytes(plan.output_bytes), plan.encoding));
                }
                if let Some(destination) = &self.destination {
                    ui.add(egui::Label::new(format!("Output: {}", destination.display())).truncate()).on_hover_text(destination.display().to_string());
                }
                if let Some(space) = &self.space { ui.label(format!("Available space: {}", format_bytes(space.available_bytes))); }
                if let Some(message) = &self.message {
                    ui.add(egui::Label::new(RichText::new(message).color(ERROR_TEXT)).wrap());
                    if ui.button("Copy Details").clicked() { ui.ctx().copy_text(message.clone()); }
                    if ui.add_enabled(!busy && self.problem_path.is_some(), egui::Button::new("Inspect problem file…")).clicked() { action = DialogAction::Inspect; }
                }
                if let Some(progress) = self.checking.as_ref().map(CombineJob::progress).or_else(|| self.writing.as_ref().map(CombineJob::progress)) {
                    ui.horizontal(|ui| { ui.spinner(); ui.label(format!("{} file {} of {}…", if self.checking.is_some() { "Checking" } else { "Combining" }, progress.file_index + 1, progress.files)); });
                    if self.writing.is_some() { ui.add(egui::ProgressBar::new(progress_fraction(progress.bytes_scanned, progress.total_bytes, progress.done)).show_percentage()); }
                } else if self.storage_check.is_some() { ui.horizontal(|ui| { ui.spinner(); ui.label("Checking output space…"); }); }
                });
                ui.separator();
                ui.horizontal(|ui| {
                    if busy {
                        if ui.add_enabled(!self.cancel_requested && self.storage_check.is_none(), egui::Button::new(if self.cancel_requested { "Cancelling…" } else { "Cancel" })).clicked() { self.cancel(); }
                    } else {
                        if ui.button("Close").clicked() { action = DialogAction::Close; }
                        if self.plan.is_none() {
                            if ui.add_enabled(self.paths.len() >= 2, egui::Button::new("Check Files")).clicked() { self.start_check(); }
                        } else {
                            if ui.button(if self.destination.is_some() { "Change Output…" } else { "Choose Output…" }).clicked() { self.choose_destination(); }
                            if ui.add_enabled(self.space.is_some(), egui::Button::new("Combine and Save")).clicked() { self.start_write(); }
                        }
                    }
                });
            });
        if modal.should_close() && !popup_was_open {
            if self.busy() {
                self.cancel();
            } else {
                action = DialogAction::Close;
            }
        }
        if self.busy() {
            ctx.request_repaint_after(POLL_INTERVAL);
        }
        action
    }
}

enum DialogAction {
    None,
    Inspect,
    Close,
    Open(PathBuf, u8, bool, Encoding),
}

impl QuarryApp {
    /// Opens Combine Files with the shared worker limit and all tab destinations protected.
    pub(super) fn open_combine_files(&mut self) {
        if !self.write_slot_available() {
            self.active.notice = Some(AppMessage::warning(
                "Two file operations are already running. Wait for one to finish or cancel it before combining files.",
            ));
            return;
        }
        let protected_paths = self
            .document_tabs()
            .flat_map(|tab| {
                tab.document
                    .as_ref()
                    .map(|document| document.logical_path.clone())
                    .into_iter()
                    .chain(tab.write_destination.clone())
            })
            .collect();
        self.combine_files = Some(CombineDialog {
            protected_paths,
            ..Default::default()
        });
        self.active.notice = None;
    }

    /// Handles dialog actions; opening a saved result creates a tab and retains old work.
    fn inspect_combine_problem(&mut self) {
        let Some(dialog) = self.combine_files.as_ref() else {
            return;
        };
        let Some(path) = dialog.problem_path.clone() else {
            return;
        };
        self.active.inspection = inspection_ui::InspectionUi::failed(
            path,
            OpenOptions {
                delimiter: dialog.delimiter.delimiter(),
                header_mode: if dialog.has_header {
                    HeaderMode::FirstRow
                } else {
                    HeaderMode::NoHeader
                },
                ..Default::default()
            },
            dialog.encoding,
            dialog.message.clone().unwrap_or_default(),
        );
    }

    pub(super) fn show_combine_files(&mut self, ctx: &egui::Context) {
        // Suspend this modal while inspecting its input, retaining the combine draft.
        if self.active.inspection.open {
            return;
        }
        let start_allowed = self.write_slot_available();
        let cleanup_reservations = self
            .document_cleanup
            .as_ref()
            .map(tabs::DocumentCleanup::path_reservations)
            .unwrap_or_default();
        let mut protected_paths: Vec<PathBuf> = self
            .document_tabs()
            .flat_map(|tab| {
                tab.document
                    .as_ref()
                    .into_iter()
                    .flat_map(|doc| [doc.logical_path.clone(), doc.session.path().to_path_buf()])
                    .chain(tab.write_destination.clone())
            })
            .collect();
        if let Some(join) = &self.join_files {
            protected_paths.extend(join.reserved_inputs().map(Path::to_path_buf));
            protected_paths.extend(join.reserved_output().map(Path::to_path_buf));
        }
        let Some(dialog) = self.combine_files.as_mut() else {
            return;
        };
        dialog.start_allowed = start_allowed;
        dialog.protected_paths = protected_paths;
        dialog.cleanup_reservations = cleanup_reservations;
        match dialog.show(ctx) {
            DialogAction::None => {}
            DialogAction::Inspect => self.inspect_combine_problem(),
            DialogAction::Close => {
                self.combine_files = None;
            }
            DialogAction::Open(path, delimiter, has_header, encoding) => {
                // Opening the result creates a tab, preserving all existing documents.
                let dialog = self.combine_files.take();
                if let Err(error) = self.open_document_tab_with_encoding(
                    path,
                    OpenOptions {
                        delimiter: Some(delimiter),
                        header_mode: if has_header {
                            HeaderMode::FirstRow
                        } else {
                            HeaderMode::NoHeader
                        },
                        ..OpenOptions::default()
                    },
                    Some(encoding),
                ) {
                    self.combine_files = dialog;
                    self.combine_files.as_mut().unwrap().message = Some(format!(
                        "The combined file is saved. {} Close this dialog and free a tab before opening the result.",
                        error.text
                    ));
                } else {
                    self.active.delimiter_mode = DelimiterMode::ALL
                        .into_iter()
                        .find(|mode| mode.delimiter() == Some(delimiter))
                        .unwrap_or(DelimiterMode::Auto);
                    self.active.header_mode = if has_header {
                        HeaderMode::FirstRow
                    } else {
                        HeaderMode::NoHeader
                    };
                    self.active.notice = None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn finish(dialog: &mut CombineDialog) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while dialog.busy() {
            assert!(Instant::now() < deadline, "combine job timed out");
            dialog.poll();
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn render(
        ctx: &egui::Context,
        dialog: &mut CombineDialog,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(860.0, 540.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                dialog.show(ctx);
            },
        )
    }

    #[cfg(unix)]
    #[test]
    fn retired_working_roots_block_combine_selection_and_start_after_moves() {
        for stage in ["select-moved", "select-alias", "start-original"] {
            let dir = tempfile::tempdir().unwrap();
            let a = dir.path().join("a.csv");
            let b = dir.path().join("b.csv");
            fs::write(&a, b"id,value\n1,a\n").unwrap();
            fs::write(&b, b"id,value\n2,b\n").unwrap();
            let mut app = QuarryApp::new(Some(a.clone()), Instant::now());
            let state =
                WorkingCopyState::new_in_with_cleanup(dir.path(), app.working_cleanup.clone())
                    .unwrap();
            let root = state.directory.path().to_owned();
            app.active.document.as_mut().unwrap().working_copy = Some(state);
            app.open_new_path(b.clone()).unwrap();
            app.open_combine_files();
            let dialog = app.combine_files.as_mut().unwrap();
            dialog.add_paths(vec![a.clone(), b.clone()]);
            dialog.start_check();
            finish(dialog);
            let old_destination = root.join("joined.csv");
            if stage == "start-original" {
                dialog.prepare_destination(old_destination.clone());
                finish(dialog);
                assert!(dialog.space.is_some());
            }
            let closing = app.tabs.remove(0);
            let (release, wait) = std::sync::mpsc::channel();
            let ctx = egui::Context::default();
            app.retire_document_tabs_with(&ctx, vec![closing], false, move |work| {
                std::thread::Builder::new().spawn(move || {
                    let _ = wait.recv();
                    work();
                })
            });
            let moved = dir.path().join("moved");
            fs::rename(&root, &moved).unwrap();
            fs::create_dir(&root).unwrap();
            fs::write(root.join("foreign.csv"), b"preserve\n").unwrap();
            let alias = dir.path().join("alias");
            std::os::unix::fs::symlink(&moved, &alias).unwrap();
            let _ = ctx.run(egui::RawInput::default(), |ctx| app.show_combine_files(ctx));
            let dialog = app.combine_files.as_mut().unwrap();
            if stage == "start-original" {
                dialog.start_write();
                assert!(
                    dialog.writing.is_none(),
                    "a retired reservation must block starting Combine"
                );
            } else {
                let parent = if stage == "select-alias" {
                    &alias
                } else {
                    &moved
                };
                dialog.prepare_destination(parent.join("joined.csv"));
                assert!(
                    dialog.destination.is_none(),
                    "a retired reservation must block choosing Combine output"
                );
                assert!(dialog.storage_check.is_none());
            }
            assert!(!old_destination.exists());
            assert!(!moved.join("joined.csv").exists());
            assert!(fs::read_dir(&moved).unwrap().next().is_none());
            release.send(()).unwrap();
            crate::tests::finish_document_cleanup(&mut app, &ctx);
            assert!(!moved.exists());
            assert_eq!(fs::read(root.join("foreign.csv")).unwrap(), b"preserve\n");
            assert_eq!(fs::read(&a).unwrap(), b"id,value\n1,a\n");
            assert_eq!(fs::read(&b).unwrap(), b"id,value\n2,b\n");
        }
    }

    #[test]
    fn ordering_invalidates_validation_and_compact_footer_stays_accessible() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        configure_style(&ctx);
        let mut dialog = CombineDialog {
            paths: (0..10)
                .map(|i| PathBuf::from(format!("/files/long-file-name-{i}.csv")))
                .collect(),
            message: Some("A detailed incompatibility message. ".repeat(30)),
            ..Default::default()
        };
        render(&ctx, &mut dialog, vec![]);
        let output = render(&ctx, &mut dialog, vec![]);
        for label in ["Close", "Check Files"] {
            assert!(
                output.shapes.iter().any(|shape| match &shape.shape {
                    egui::Shape::Text(text) =>
                        text.galley.text() == label
                            && shape.clip_rect.contains_rect(text.visual_bounding_rect())
                            && text.visual_bounding_rect().bottom() <= 540.0,
                    _ => false,
                }),
                "{label} is visible in a compact window"
            );
        }
        let tree = output.platform_output.accesskit_update.unwrap();
        let (id, _) = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some("Move file 1 down"))
            .expect("named ordering control");
        let first = dialog.paths[0].clone();
        render(
            &ctx,
            &mut dialog,
            vec![egui::Event::AccessKitActionRequest(
                egui::accesskit::ActionRequest {
                    action: egui::accesskit::Action::Click,
                    target: *id,
                    data: None,
                },
            )],
        );
        assert_eq!(dialog.paths[1], first);
        assert!(dialog.message.is_none());
    }

    #[test]
    fn native_quit_retains_combine_selections_checks_and_writes() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.csv");
        let b = dir.path().join("b.csv");
        fs::write(&a, b"ID,Name\n1,a\n").unwrap();
        fs::write(&b, b"ID,Name\n2,b\n").unwrap();
        for stage in 0..3 {
            let ctx = egui::Context::default();
            let mut app = QuarryApp::new(Some(a.clone()), Instant::now());
            app.active
                .document
                .as_mut()
                .unwrap()
                .rename_header(0, "Unsaved".into())
                .unwrap();
            app.open_combine_files();
            let dialog = app.combine_files.as_mut().unwrap();
            dialog.delimiter = DelimiterMode::Comma;
            dialog.add_paths(vec![b.clone(), a.clone()]);
            let out = dir.path().join(format!("out-{stage}.csv"));
            if stage > 0 {
                dialog.start_check();
            }
            if stage == 2 {
                finish(dialog);
                dialog.prepare_destination(out.clone());
                finish(dialog);
                dialog.start_write();
            }
            let mut input = egui::RawInput::default();
            input
                .viewports
                .get_mut(&egui::ViewportId::ROOT)
                .unwrap()
                .events
                .push(egui::ViewportEvent::Close);
            let output = ctx.run(input, |ctx| {
                eframe::App::update(&mut app, ctx, &mut eframe::Frame::_new_kittest());
            });
            assert!(
                output.viewport_output[&egui::ViewportId::ROOT]
                    .commands
                    .contains(&egui::ViewportCommand::CancelClose)
            );
            assert!(!app.quit_pending);
            assert!(!app.quit_authorized);
            let dialog = app.combine_files.as_mut().unwrap();
            assert_eq!(dialog.paths, vec![b.clone(), a.clone()]);
            assert_eq!(dialog.delimiter, DelimiterMode::Comma);
            assert!(dialog.has_header);
            assert!(!dialog.cancel_requested);
            assert!(
                dialog
                    .message
                    .as_ref()
                    .unwrap()
                    .contains("Close Combine Files before quitting")
            );
            finish(dialog);
            if stage > 0 {
                assert_eq!(dialog.plan.as_ref().unwrap().data_rows, 2);
            }
            if stage == 2 {
                assert!(dialog.result.is_some());
                assert_eq!(fs::read(&out).unwrap(), b"ID,Name\n2,b\n1,a\n");
            }
            app.keep_editing();
            assert_eq!(
                app.combine_files.as_ref().unwrap().paths,
                vec![b.clone(), a.clone()]
            );
            assert_eq!(
                app.active.document.as_ref().unwrap().column_name(0),
                "Unsaved"
            );
            assert!(app.active.document.as_ref().unwrap().can_undo());
        }
        assert_eq!(fs::read(&a).unwrap(), b"ID,Name\n1,a\n");
        assert_eq!(fs::read(&b).unwrap(), b"ID,Name\n2,b\n");
    }

    #[test]
    fn workflow_preserves_unsaved_document_and_rechecks_inputs_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.csv");
        let b = dir.path().join("b.csv");
        fs::write(&a, b"ID,Name\n1,a\n").unwrap();
        fs::write(&b, b"ID,Name\n2,b\n").unwrap();
        let mut app = QuarryApp::new(Some(a.clone()), Instant::now());
        let doc = app.active.document.as_mut().unwrap();
        doc.begin_cell_edit(1, 1, b"a".to_vec()).unwrap();
        doc.cell_edit.as_mut().unwrap().draft = "unsaved".into();
        app.open_combine_files();
        let dialog = app.combine_files.as_mut().unwrap();
        dialog.add_paths(vec![a.clone(), b.clone()]);
        dialog.start_check();
        finish(dialog);
        assert_eq!(dialog.plan.as_ref().unwrap().data_rows, 2);
        let out = dir.path().join("out.csv");
        dialog.prepare_destination(out.clone());
        finish(dialog);
        dialog.start_write();
        finish(dialog);
        assert!(dialog.result.is_some());
        assert_eq!(fs::read(&out).unwrap(), b"ID,Name\n1,a\n2,b\n");
        assert_eq!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .cell_edit
                .as_ref()
                .unwrap()
                .draft,
            "unsaved"
        );
        assert!(app.open_new_path(out).is_err());
        assert_eq!(app.active.document.as_ref().unwrap().logical_path, a);
        assert_eq!(fs::read(&b).unwrap(), b"ID,Name\n2,b\n");
    }

    #[test]
    fn stale_plan_and_existing_output_leave_files_intact() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.csv");
        let b = dir.path().join("b.csv");
        fs::write(&a, b"ID,Name\n1,a\n").unwrap();
        fs::write(&b, b"ID,Name\n2,b\n").unwrap();
        let mut dialog = CombineDialog::default();
        dialog.add_paths(vec![a, b.clone()]);
        dialog.start_check();
        finish(&mut dialog);
        let out = dir.path().join("out.csv");
        fs::write(&out, b"existing").unwrap();
        dialog.prepare_destination(out.clone());
        finish(&mut dialog);
        assert!(dialog.space.is_none());
        assert_eq!(fs::read(&out).unwrap(), b"existing");
        fs::remove_file(&out).unwrap();
        dialog.prepare_destination(out.clone());
        finish(&mut dialog);
        fs::write(&b, b"ID,Name\n3,c\n").unwrap();
        dialog.start_write();
        finish(&mut dialog);
        assert!(dialog.plan.is_none());
        assert!(dialog.result.is_none());
        assert!(!out.exists());
    }
    #[test]
    fn late_combine_error_retains_the_failing_input_and_record() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("good.csv");
        let b = dir.path().join("bad.csv");
        fs::write(&a, b"a,b\n1,2\n").unwrap();
        let mut data = b"a,b\n".to_vec();
        for _ in 0..30 {
            data.extend_from_slice(b"1,2\n");
        }
        data.extend_from_slice(b"bad\n");
        fs::write(&b, data).unwrap();
        let mut dialog = CombineDialog {
            paths: vec![a, b.clone()],
            delimiter: DelimiterMode::Comma,
            ..Default::default()
        };
        dialog.start_check();
        finish(&mut dialog);
        let details = dialog.message.as_ref().unwrap();
        assert!(details.contains(&b.display().to_string()));
        assert!(details.contains("Record: 32"), "{details}");
        assert_eq!(dialog.problem_path, Some(fs::canonicalize(&b).unwrap()));
        let paths = dialog.paths.clone();
        let mut app = QuarryApp::new(Some(b), Instant::now());
        app.combine_files = Some(dialog);
        app.inspect_combine_problem();
        assert!(app.active.inspection.open);
        assert_eq!(app.combine_files.as_ref().unwrap().paths, paths);
    }
}
