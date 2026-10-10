use super::*;
use quarry_core::{
    CombineJob, CombineOptions, CombinePlan, CombineSummary, StorageSpace, check_combine_files,
    check_storage,
};
use std::thread::{self, JoinHandle};

pub(super) struct CombineDialog {
    paths: Vec<PathBuf>,
    has_header: bool,
    delimiter: DelimiterMode,
    plan: Option<CombinePlan>,
    checking: Option<CombineJob<CombinePlan>>,
    writing: Option<CombineJob<CombineSummary>>,
    storage_check: Option<JoinHandle<Result<StorageSpace, String>>>,
    destination: Option<PathBuf>,
    space: Option<StorageSpace>,
    result: Option<CombineSummary>,
    message: Option<String>,
    cancel_requested: bool,
}

impl Default for CombineDialog {
    fn default() -> Self {
        Self {
            paths: Vec::new(),
            has_header: true,
            delimiter: DelimiterMode::Auto,
            plan: None,
            checking: None,
            writing: None,
            storage_check: None,
            destination: None,
            space: None,
            result: None,
            message: None,
            cancel_requested: false,
        }
    }
}

impl CombineDialog {
    pub(super) fn busy(&self) -> bool {
        self.checking.is_some() || self.writing.is_some() || self.storage_check.is_some()
    }

    fn invalidate(&mut self) {
        self.plan = None;
        self.destination = None;
        self.space = None;
        self.message = None;
    }

    fn start_check(&mut self) {
        self.invalidate();
        self.cancel_requested = false;
        match check_combine_files(
            self.paths.clone(),
            CombineOptions {
                delimiter: self.delimiter.delimiter(),
                has_header: self.has_header,
            },
        ) {
            Ok(job) => self.checking = Some(job),
            Err(error) => self.message = Some(error.to_string()),
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
                Err(error) => self.message = Some(error.to_string()),
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
                    self.message = Some(error.to_string());
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

    fn prepare_destination(&mut self, destination: PathBuf) {
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
        let (Some(plan), Some(destination), Some(_)) = (&self.plan, &self.destination, &self.space)
        else {
            return;
        };
        match plan.clone().start_write(destination.clone()) {
            Ok(job) => {
                self.writing = Some(job);
                self.message = None;
                self.cancel_requested = false;
            }
            Err(error) => self.message = Some(error.to_string()),
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
                            action = DialogAction::Open(result.destination.clone(), self.plan.as_ref().map(|p| p.delimiter).unwrap_or(b','), self.has_header);
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
                    let previous = self.delimiter;
                    ui.horizontal(|ui| {
                        ui.label("Delimiter");
                        egui::ComboBox::from_id_salt("combine-delimiter").selected_text(self.delimiter.label()).show_ui(ui, |ui| {
                            for mode in DelimiterMode::ALL { ui.selectable_value(&mut self.delimiter, mode, mode.label()); }
                        });
                    });
                    if header_changed || previous != self.delimiter { self.invalidate(); }
                });
                ui.small(if self.has_header { "Headers must match exactly, in the same order. The output keeps one header." } else { "Every row must have the same number of columns. All rows are kept." });
                ui.small("Uses saved files on disk. Duplicates are kept. Unsaved edits are not included.");
                if let Some(plan) = &self.plan {
                    ui.separator();
                    ui.label(format!("{} matching files · {} columns · {} data rows", plan.inputs.len(), plan.columns, plan.data_rows));
                    ui.label(format!("Output size: {}", format_bytes(plan.output_bytes)));
                }
                if let Some(destination) = &self.destination {
                    ui.add(egui::Label::new(format!("Output: {}", destination.display())).truncate()).on_hover_text(destination.display().to_string());
                }
                if let Some(space) = &self.space { ui.label(format!("Available space: {}", format_bytes(space.available_bytes))); }
                if let Some(message) = &self.message { ui.add(egui::Label::new(RichText::new(message).color(ERROR_TEXT)).wrap()); }
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
    Close,
    Open(PathBuf, u8, bool),
}

impl QuarryApp {
    pub(super) fn open_combine_files(&mut self) {
        if self.document.as_ref().is_some_and(|doc| {
            doc.save_job.is_some() || doc.export_job.is_some() || doc.structural_job.is_some()
        }) {
            self.notice = Some(AppMessage::warning(
                "Wait for the active file operation before combining files.",
            ));
            return;
        }
        self.combine_files = Some(CombineDialog::default());
        self.notice = None;
    }

    pub(super) fn show_combine_files(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.combine_files.as_mut() else {
            return;
        };
        match dialog.show(ctx) {
            DialogAction::None => {}
            DialogAction::Close => {
                self.combine_files = None;
            }
            DialogAction::Open(path, delimiter, has_header) => {
                // The normal document guard preserves pending edits. Temporarily remove
                // our modal so this intentional open can use that guard.
                let dialog = self.combine_files.take();
                if let Err(error) = self.open_path_with_options(
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
                ) {
                    self.combine_files = dialog;
                    self.combine_files.as_mut().unwrap().message = Some(format!(
                        "The combined file is saved. {} Close this dialog to save or discard the open document first.",
                        error.text
                    ));
                } else {
                    self.delimiter_mode = DelimiterMode::ALL
                        .into_iter()
                        .find(|mode| mode.delimiter() == Some(delimiter))
                        .unwrap_or(DelimiterMode::Auto);
                    self.header_mode = if has_header {
                        HeaderMode::FirstRow
                    } else {
                        HeaderMode::NoHeader
                    };
                    self.notice = None;
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
    fn workflow_preserves_unsaved_document_and_rechecks_inputs_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.csv");
        let b = dir.path().join("b.csv");
        fs::write(&a, b"ID,Name\n1,a\n").unwrap();
        fs::write(&b, b"ID,Name\n2,b\n").unwrap();
        let mut app = QuarryApp::new(Some(a.clone()), Instant::now());
        let doc = app.document.as_mut().unwrap();
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
            app.document
                .as_ref()
                .unwrap()
                .cell_edit
                .as_ref()
                .unwrap()
                .draft,
            "unsaved"
        );
        assert!(app.open_new_path(out).is_err());
        assert_eq!(app.document.as_ref().unwrap().logical_path, a);
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
}
