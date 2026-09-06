use super::*;
use quarry_core::{
    StorageSpace, estimate_duplicate_temporary_bytes, estimate_edited_output_bytes, inspect_storage,
};
use std::thread::{self, JoinHandle};

// Small operations still receive the engine's space check, without another dialog.
pub(super) const STORAGE_REVIEW_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone)]
pub(super) enum PendingStorageOperation {
    Structural(StructuralDialog),
    Materialize(ColumnTransformation, BTreeSet<usize>, u64),
    DeleteColumns(Vec<usize>),
    DeleteRows(Vec<RangeInclusive<u64>>),
    ReplaceAll {
        query: Vec<u8>,
        replacement: Vec<u8>,
        case_sensitivity: CaseSensitivity,
    },
    Save(Option<PathBuf>),
    Export(PathBuf),
}

impl PendingStorageOperation {
    fn uses_working_directory(&self) -> bool {
        !matches!(self, Self::Save(_) | Self::Export(_))
    }

    fn allowance(&self, document: &Document) -> u64 {
        let plain = || document.materialization_storage_estimate(None, None);
        match self {
            Self::Structural(dialog) => match dialog.request {
                // Read-only analysis determines the actual padded width before its review.
                StructuralRequest::Split => 0,
                StructuralRequest::Sort => {
                    document.sort_temporary_disk_estimate().unwrap_or(u64::MAX)
                }
                StructuralRequest::Duplicates => estimate_duplicate_temporary_bytes(
                    document.reordered_storage_estimate(),
                    document
                        .storage_record_count()
                        .saturating_sub(document.data_start),
                    dialog.columns.len(),
                ),
                StructuralRequest::Combine => document.materialization_storage_estimate(
                    Some(&ColumnTransformation::Join {
                        source_columns: dialog.columns.clone(),
                        separator: dialog.separator.as_bytes().to_vec(),
                        output_header: document.session.dialect.has_header.then(|| {
                            document
                                .current_header_fields()
                                .get(dialog.columns[0])
                                .cloned()
                                .unwrap_or_default()
                        }),
                    }),
                    None,
                ),
                StructuralRequest::Move => document.materialization_storage_estimate(
                    Some(&ColumnTransformation::Arrange {
                        source_width: document.total_columns,
                        output_columns: (0..document.total_columns).collect(),
                    }),
                    None,
                ),
            },
            Self::Materialize(transformation, _, records) => document
                .materialization_storage_estimate_for_records(Some(transformation), None, *records),
            Self::DeleteColumns(columns) => document.materialization_storage_estimate(
                Some(&ColumnTransformation::Arrange {
                    source_width: document.total_columns,
                    output_columns: (0..document.total_columns)
                        .filter(|column| !columns.contains(column))
                        .collect(),
                }),
                None,
            ),
            Self::ReplaceAll {
                query,
                replacement,
                case_sensitivity,
            } => document.materialization_storage_estimate(
                None,
                Some(&LiteralReplacement {
                    needle: query.clone(),
                    replacement: replacement.clone(),
                    case_sensitivity: *case_sensitivity,
                }),
            ),
            Self::Export(_) => document.session.file_size,
            Self::DeleteRows(_) | Self::Save(_) => plain(),
        }
    }
}

pub(super) struct StorageReview {
    operation: Option<PendingStorageOperation>,
    path_input: String,
    required_bytes: u64,
    retained_paths: Vec<PathBuf>,
    checked_path: PathBuf,
    result: Option<Result<(StorageSpace, u64), String>>,
    worker: Option<JoinHandle<Result<(StorageSpace, u64), String>>>,
}

impl StorageReview {
    fn new(
        directory: PathBuf,
        required_bytes: u64,
        retained_paths: Vec<PathBuf>,
        operation: Option<PendingStorageOperation>,
    ) -> Self {
        let mut review = Self {
            operation,
            path_input: directory.display().to_string(),
            required_bytes,
            retained_paths,
            checked_path: PathBuf::new(),
            result: None,
            worker: None,
        };
        review.check();
        review
    }

    fn check(&mut self) {
        if self.worker.is_some() {
            return;
        }
        let directory = PathBuf::from(&self.path_input);
        self.checked_path = directory.clone();
        self.result = None;
        let retained_paths = self.retained_paths.clone();
        self.worker = Some(thread::spawn(move || {
            let space = inspect_storage(&directory).map_err(|error| error.to_string())?;
            let retained_bytes = retained_paths.iter().try_fold(0_u64, |bytes, path| {
                std::fs::metadata(path).map(|metadata| bytes.saturating_add(metadata.len()))
                    .map_err(|error| format!("Cannot inspect retained working file {}: {error}. Reconnect its drive before continuing.", path.display()))
            })?;
            Ok((space, retained_bytes))
        }));
    }

    fn poll(&mut self) {
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| worker.is_finished())
        {
            self.result = Some(self.worker.take().unwrap().join().unwrap_or_else(|_| {
                Err("Storage check failed. Try checking the folder again.".into())
            }));
        }
    }

    fn ready(&self) -> bool {
        self.checked_path == Path::new(&self.path_input)
            && self.result.as_ref().is_some_and(|result| {
                result
                    .as_ref()
                    .is_ok_and(|(space, _)| space.available_bytes >= self.required_bytes)
            })
    }
}

impl QuarryApp {
    pub(super) fn defer_storage(&mut self, operation: PendingStorageOperation) -> bool {
        if self.storage_approved {
            return false;
        }
        let Some(document) = self.document.as_mut() else {
            return false;
        };
        document.commit_edits();
        if operation.allowance(document) < STORAGE_REVIEW_BYTES {
            return false;
        }
        // A completed index prevents a byte-count fallback from greatly overstating padding/spill needs.
        if document.index.is_none() {
            self.notice = Some(AppMessage::warning(
                "Wait for indexing to finish so Quarry can calculate storage requirements.",
            ));
            return true;
        }
        self.begin_storage_review(operation);
        true
    }

    pub(super) fn begin_storage_review(&mut self, operation: PendingStorageOperation) {
        let Some(document) = self.document.as_ref() else {
            return;
        };
        let directory = match &operation {
            PendingStorageOperation::Save(destination) => destination
                .as_ref()
                .unwrap_or(&document.logical_path)
                .parent(),
            PendingStorageOperation::Export(destination) => destination.parent(),
            _ => Some(self.working_directory.as_path()),
        }
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf();
        self.storage_review = Some(StorageReview::new(
            directory,
            operation.allowance(document),
            document.retained_storage_paths(),
            Some(operation),
        ));
        self.notice = None;
    }

    pub(super) fn open_storage_settings(&mut self) {
        self.storage_review = Some(StorageReview::new(
            self.working_directory.clone(),
            0,
            self.document
                .as_ref()
                .map(Document::retained_storage_paths)
                .unwrap_or_default(),
            None,
        ));
    }

    pub(super) fn show_storage_review(&mut self, ctx: &egui::Context) {
        let Some(review) = self.storage_review.as_mut() else {
            return;
        };
        review.poll();
        let working = review
            .operation
            .as_ref()
            .is_none_or(PendingStorageOperation::uses_working_directory);
        let mut accept = false;
        let mut cancel = false;
        let mut check = false;
        egui::CentralPanel::default().show(ctx, |_| {});
        let modal = egui::Modal::new(egui::Id::new("quarry-storage-review")).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.heading(if review.operation.is_some() { "Review storage requirements" } else { "Temporary storage" });
            ui.label(if working { "Working folder" } else { "Save / export staging folder" });
            let label = ui.label(if working { "Folder" } else { "Destination volume" });
            ui.add_enabled(working && review.worker.is_none(), egui::TextEdit::singleline(&mut review.path_input).desired_width(f32::INFINITY)).labelled_by(label.id);
            if working {
                ui.add_enabled_ui(review.worker.is_none(), |ui| {
                    ui.horizontal(|ui| {
                        if ui.button("Choose folder…").clicked() {
                            if let Some(path) = rfd::FileDialog::new().set_title("Choose temporary working folder").set_directory(&review.path_input).pick_folder() {
                                review.path_input = path.display().to_string(); check = true;
                            }
                        }
                        if ui.button("Use system temporary folder").clicked() {
                            review.path_input = std::env::temp_dir().display().to_string(); check = true;
                        }
                    });
                });
                ui.small("New operations use this folder for this app session. Existing working and Undo files stay on their current drive until no longer needed. Keep that drive connected.");
            } else {
                ui.small("Atomic Save staging stays beside the destination. To use another drive, cancel and choose Save As or another export destination.");
            }
            if review.operation.is_some() {
                ui.label(format!("Required additional space (conservative allowance): {}", format_bytes(review.required_bytes)));
            }
            if review.checked_path != Path::new(&review.path_input) {
                ui.label("Folder changed. Click Check space to refresh its available space.");
            } else {
            match &review.result {
                Some(Ok((space, retained))) => {
                    ui.label(format!("Available space: {}", format_bytes(space.available_bytes)));
                    ui.label(format!("Retained working and Undo/Redo files: {}", format_bytes(*retained)));
                    ui.small("Retained files already reduce the free space on their own volumes; they are not added twice to this allowance.");
                    if space.available_bytes < review.required_bytes {
                        ui.colored_label(ERROR_TEXT, "Not enough space. Free space or choose a working folder on another drive.");
                    }
                }
                Some(Err(error)) => { ui.colored_label(ERROR_TEXT, error); }
                None => { ui.label("Checking available space and folder access…"); ui.spinner(); }
            }
            }
            ui.small("Space can change. Quarry checks again before writing and removes unpublished output if a write fails. Your current document and required Undo files are preserved.");
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() { cancel = true; }
                if ui.add_enabled(review.worker.is_none(), egui::Button::new("Check space")).clicked() { check = true; }
                if ui.add_enabled(review.ready(), egui::Button::new(if review.operation.is_some() { "Continue" } else { "Use folder" })).clicked() { accept = true; }
            });
        });
        cancel |= modal.should_close();
        if check {
            review.check();
        }
        if cancel {
            self.storage_review = None;
            if self.close_after_save {
                self.close_after_save = false;
                self.close_confirmation_open = true;
            }
        } else if accept {
            let review = self.storage_review.take().unwrap();
            if working {
                self.working_directory = PathBuf::from(review.path_input);
                if let Some(document) = self.document.as_mut() {
                    document.working_directory = self.working_directory.clone();
                }
            }
            if let Some(operation) = review.operation {
                // Only the operation resumed by this dialog bypasses the presentation gate.
                // Engine checks still run immediately before writes.
                self.storage_approved = true;
                self.resume_storage_operation(operation, ctx);
                self.storage_approved = false;
            }
        }
        ctx.request_repaint_after(POLL_INTERVAL);
    }

    fn resume_storage_operation(
        &mut self,
        operation: PendingStorageOperation,
        ctx: &egui::Context,
    ) {
        match operation {
            PendingStorageOperation::Structural(dialog) => {
                self.structural_dialog = Some(dialog);
                self.apply_structural_dialog_action(StructuralDialogAction::Apply);
            }
            PendingStorageOperation::Materialize(transformation, columns, _) => {
                self.notice = self
                    .document
                    .as_mut()
                    .unwrap()
                    .begin_materialization(transformation, columns)
                    .err();
            }
            PendingStorageOperation::DeleteColumns(columns) => self.apply_delete_columns(columns),
            PendingStorageOperation::DeleteRows(rows) => self.apply_delete_rows(rows),
            PendingStorageOperation::ReplaceAll {
                query,
                replacement,
                case_sensitivity,
            } => {
                self.notice = self
                    .document
                    .as_mut()
                    .unwrap()
                    .start_replace_all_with_case(&query, &replacement, case_sensitivity)
                    .err();
            }
            PendingStorageOperation::Save(destination) => {
                let started = if let Some(destination) = destination {
                    self.save_as_picker_result(Some(destination))
                } else {
                    self.save_current()
                };
                if !started && self.close_after_save {
                    self.close_after_save = false;
                    self.close_confirmation_open = true;
                }
            }
            PendingStorageOperation::Export(destination) => {
                self.export_picker_result(Some(destination))
            }
        }
        ctx.request_repaint();
    }
}

impl Document {
    fn storage_record_count(&self) -> u64 {
        self.index.as_ref().map_or(
            self.session.file_size.saturating_add(1),
            StructuralIndex::indexed_rows,
        )
    }

    pub(super) fn materialization_storage_estimate(
        &self,
        transformation: Option<&ColumnTransformation>,
        replacement: Option<&LiteralReplacement>,
    ) -> u64 {
        self.materialization_storage_estimate_for_records(
            transformation,
            replacement,
            if transformation.is_some() || replacement.is_some() {
                self.storage_record_count()
            } else {
                0
            },
        )
    }

    pub(super) fn materialization_storage_estimate_for_records(
        &self,
        transformation: Option<&ColumnTransformation>,
        replacement: Option<&LiteralReplacement>,
        records: u64,
    ) -> u64 {
        let renames = self
            .header_renames
            .iter()
            .map(|(column, value)| (*column, value.as_bytes().to_vec()))
            .collect();
        estimate_edited_output_bytes(
            self.session.file_size,
            records,
            &renames,
            &self.cell_edits,
            transformation,
            replacement,
        )
    }

    fn reordered_storage_estimate(&self) -> u64 {
        let renames = self
            .header_renames
            .iter()
            .map(|(column, value)| (*column, value.as_bytes().to_vec()))
            .collect();
        estimate_edited_output_bytes(
            self.session.file_size,
            self.storage_record_count(),
            &renames,
            &self.cell_edits,
            None,
            None,
        )
    }

    fn retained_storage_paths(&self) -> Vec<PathBuf> {
        let Some(state) = self.working_copy.as_ref() else {
            return Vec::new();
        };
        std::iter::once(self.session.path())
            .chain(
                state
                    .undo
                    .iter()
                    .chain(state.redo.iter())
                    .map(|snapshot| snapshot.path.as_path()),
            )
            .filter(|path| state.owns(path))
            .map(Path::to_path_buf)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
}

impl WorkingCopyState {
    pub(super) fn use_directory(&mut self, parent: &Path) -> Result<(), String> {
        if self.directory.path().parent() != Some(parent) {
            let replacement = Self::new_in(parent)?;
            self.previous_directories.push(std::mem::replace(
                &mut self.directory,
                replacement.directory,
            ));
        }
        Ok(())
    }

    pub(super) fn prune_directories(&mut self, current: &Path) {
        self.previous_directories.retain(|directory| {
            current.starts_with(directory.path())
                || self
                    .undo
                    .iter()
                    .chain(self.redo.iter())
                    .any(|snapshot| snapshot.path.starts_with(directory.path()))
        });
    }

    pub(super) fn owns(&self, path: &Path) -> bool {
        path.starts_with(self.directory.path())
            || self
                .previous_directories
                .iter()
                .any(|directory| path.starts_with(directory.path()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update_review_until(
        app: &mut QuarryApp,
        ctx: &egui::Context,
        ready: impl Fn(&QuarryApp) -> bool,
    ) -> egui::FullOutput {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(app.storage_review.is_some());
            let output = ctx.run(egui::RawInput::default(), |ctx| {
                eframe::App::update(app, ctx, &mut eframe::Frame::_new_kittest());
            });
            if ready(app) {
                return output;
            }
            assert!(
                Instant::now() < deadline,
                "background job stalled behind storage review"
            );
            std::thread::yield_now();
        }
    }

    #[test]
    fn storage_review_keeps_background_jobs_advancing_and_blocks_document_input() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.csv");
        let other = directory.path().join("other.csv");
        let output = directory.path().join("filtered.csv");
        let original = b"name,value\nAda Lovelace,1\nGrace Hopper,2\n";
        std::fs::write(&source, original).unwrap();
        std::fs::write(&other, b"other\nfile\n").unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        assert!(app.document.as_ref().unwrap().index.is_none());
        app.open_storage_settings();
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .events
            .push(egui::ViewportEvent::Close);
        input.dropped_files.push(egui::DroppedFile {
            path: Some(other),
            ..Default::default()
        });
        let close_output = ctx.run(input, |ctx| {
            eframe::App::update(&mut app, ctx, &mut eframe::Frame::_new_kittest());
        });
        assert!(
            close_output.viewport_output[&egui::ViewportId::ROOT]
                .commands
                .iter()
                .any(|command| matches!(command, egui::ViewportCommand::CancelClose))
        );
        assert!(!app.close_confirmation_open);
        assert_eq!(app.document.as_ref().unwrap().session.path(), source);
        let frame = update_review_until(&mut app, &ctx, |app| {
            app.document.as_ref().unwrap().index.is_some()
                && app.storage_review.as_ref().unwrap().worker.is_none()
        });
        assert!(
            frame
                .platform_output
                .accesskit_update
                .unwrap()
                .nodes
                .iter()
                .any(|(_, node)| node.label() == Some("Use folder"))
        );

        app.storage_review = None;
        app.document
            .as_mut()
            .unwrap()
            .start_find_next(b"Grace")
            .unwrap();
        app.open_storage_settings();
        update_review_until(&mut app, &ctx, |app| {
            app.document.as_ref().unwrap().search_job.is_none()
        });
        assert_eq!(app.document.as_ref().unwrap().last_match.unwrap().row, 2);

        app.storage_review = None;
        app.document
            .as_mut()
            .unwrap()
            .start_filter(FilterQuery::single(
                1,
                FilterOperator::Equals,
                b"1".to_vec(),
            ))
            .unwrap();
        app.open_storage_settings();
        update_review_until(&mut app, &ctx, |app| {
            let document = app.document.as_ref().unwrap();
            document.filter_job.is_none() && document.visible_filter_rows().len() == 1
        });
        assert!(
            app.document
                .as_ref()
                .unwrap()
                .filter_progress()
                .unwrap()
                .done
        );

        app.storage_review = None;
        app.document
            .as_mut()
            .unwrap()
            .start_filtered_export(output.clone())
            .unwrap();
        app.open_storage_settings();
        update_review_until(&mut app, &ctx, |app| {
            app.document.as_ref().unwrap().export_job.is_none()
        });
        assert_eq!(
            std::fs::read(output).unwrap(),
            b"name,value\nAda Lovelace,1\n"
        );

        app.storage_review = None;
        let document = app.document.as_mut().unwrap();
        document.clear_filter().unwrap();
        document.start_split(0, b" ".to_vec()).unwrap();
        // Exercise the large-operation gate without allocating a large fixture.
        document.session.file_size = STORAGE_REVIEW_BYTES;
        app.open_storage_settings();
        update_review_until(&mut app, &ctx, |app| {
            app.document
                .as_ref()
                .unwrap()
                .pending_materialization
                .is_some()
        });
        assert!(app.document.as_ref().unwrap().structural_job.is_none());
        assert!(app.storage_review.as_ref().unwrap().operation.is_none());
        assert_eq!(std::fs::read(&source).unwrap(), original);
        app.storage_review = None;
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            eframe::App::update(&mut app, ctx, &mut eframe::Frame::_new_kittest());
        });
        assert!(matches!(
            app.storage_review.as_ref().unwrap().operation,
            Some(PendingStorageOperation::Materialize(..))
        ));
        assert!(
            app.document
                .as_ref()
                .unwrap()
                .pending_materialization
                .is_none()
        );
    }

    fn finish(app: &mut QuarryApp) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let document = app.document.as_mut().unwrap();
            document.poll().unwrap();
            if let Some(ready) = document.poll_structural_edit().unwrap() {
                app.install_materialized_working_copy(ready).unwrap();
            }
            let document = app.document.as_ref().unwrap();
            if document.structural_job.is_none() && document.index.is_some() {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
    }

    #[test]
    fn changing_working_folder_preserves_history_failures_and_atomic_save() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let source = directory.path().join("source.csv");
        let original = b"key,value,extra\na,1,x\nb,2,y\nc,3,z\n";
        std::fs::write(&source, original).unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish(&mut app);
        app.working_directory = first.clone();
        app.document.as_mut().unwrap().working_directory = first.clone();
        app.apply_delete_columns(vec![2]);
        finish(&mut app);
        let first_copy = app.document.as_ref().unwrap().session.path().to_path_buf();
        assert!(first_copy.starts_with(&first));
        assert_eq!(
            std::fs::read(&first_copy).unwrap(),
            b"key,value\na,1\nb,2\nc,3\n"
        );
        app.working_directory = second.clone();
        app.document.as_mut().unwrap().working_directory = second.clone();
        app.apply_delete_rows(vec![2..=2]);
        finish(&mut app);
        let second_copy = app.document.as_ref().unwrap().session.path().to_path_buf();
        assert!(second_copy.starts_with(&second));
        assert!(first_copy.exists());
        app.swap_edit_history(false).unwrap();
        finish(&mut app);
        assert_eq!(app.document.as_ref().unwrap().session.path(), first_copy);
        app.swap_edit_history(true).unwrap();
        finish(&mut app);
        assert_eq!(app.document.as_ref().unwrap().session.path(), second_copy);
        app.document.as_mut().unwrap().working_directory = directory.path().join("disconnected");
        app.apply_delete_rows(vec![1..=1]);
        assert!(
            app.notice
                .as_ref()
                .unwrap()
                .contains("Choose another folder")
        );
        assert!(!app.document.as_ref().unwrap().source_changed);
        assert!(first_copy.exists() && second_copy.exists());
        assert_eq!(std::fs::read(&source).unwrap(), original);
        // Saving uses the source volume even though the selected working location vanished.
        assert!(app.save_current());
        let deadline = Instant::now() + Duration::from_secs(10);
        let saved = loop {
            if let Some(saved) = app.document.as_mut().unwrap().poll_save().unwrap() {
                break saved;
            }
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        };
        assert_eq!(saved, (source.clone(), true));
        assert_eq!(std::fs::read(&source).unwrap(), b"key,value\na,1\nc,3\n");
        app.replace_document_with_options(source, OpenOptions::default())
            .unwrap();
        assert!(!first_copy.exists() && !second_copy.exists());
        assert_eq!(std::fs::read_dir(first).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(second).unwrap().count(), 0);
    }

    #[test]
    fn storage_review_exposes_capacity_and_requires_a_current_successful_check() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().to_path_buf();
        let mut app = QuarryApp::new(None, Instant::now());
        app.storage_review = Some(StorageReview {
            operation: Some(PendingStorageOperation::DeleteRows(vec![1..=1])),
            path_input: path.display().to_string(),
            checked_path: path.clone(),
            required_bytes: 200,
            retained_paths: Vec::new(),
            worker: None,
            result: Some(Ok((
                StorageSpace {
                    directory: path,
                    required_bytes: 0,
                    available_bytes: 100,
                },
                50,
            ))),
        });
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 750.0),
                )),
                ..Default::default()
            },
            |ctx| app.show_storage_review(ctx),
        );
        let nodes = output.platform_output.accesskit_update.unwrap().nodes;
        assert!(
            nodes
                .iter()
                .any(|(_, node)| node.label() == Some("Continue") && node.is_disabled())
        );
        for expected in [
            "Required additional space",
            "Available space",
            "Retained working and Undo/Redo files",
        ] {
            assert!(
                nodes.iter().any(|(_, node)| node
                    .value()
                    .is_some_and(|value| value.contains(expected))
                    || node.label().is_some_and(|value| value.contains(expected))),
                "missing {expected}"
            );
        }
        let review = app.storage_review.as_mut().unwrap();
        review
            .result
            .as_mut()
            .unwrap()
            .as_mut()
            .unwrap()
            .0
            .available_bytes = 200;
        assert!(review.ready());
        review.path_input.push_str("/different");
        assert!(!review.ready());
        review.check();
        let deadline = Instant::now() + Duration::from_secs(5);
        while review.worker.is_some() {
            review.poll();
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(review.result.as_ref().unwrap().is_err());
        assert!(!review.ready());
    }

    #[test]
    fn large_save_waits_for_review_without_allocating_or_losing_edits() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.csv");
        std::fs::write(&source, b"key\na\n").unwrap();
        let mut app = QuarryApp::new(Some(source.clone()), Instant::now());
        finish(&mut app);
        let document = app.document.as_mut().unwrap();
        document.cell_edits.insert((1, 0), b"edited".to_vec());
        // Simulate large metadata without writing a large fixture. No worker starts here.
        document.session.file_size = STORAGE_REVIEW_BYTES;
        app.close_after_save = true;
        assert!(app.save_current());
        assert!(app.storage_review.is_some());
        assert!(app.document.as_ref().unwrap().save_job.is_none());
        assert_eq!(std::fs::read(&source).unwrap(), b"key\na\n");
        update_review_until(&mut app, &egui::Context::default(), |app| {
            app.storage_review.as_ref().unwrap().worker.is_none()
        });
        assert!(app.close_after_save);
        assert!(!app.close_confirmation_open);
        app.storage_review = None;
        assert_eq!(
            app.document.as_ref().unwrap().cell_edits[&(1, 0)],
            b"edited"
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
