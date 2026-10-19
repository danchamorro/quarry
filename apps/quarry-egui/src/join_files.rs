use super::*;
use quarry_core::JoinHeader as HeaderInfo;
use quarry_core::{
    JoinCase, JoinConfig, JoinInput, JoinJob, JoinMode, JoinPlan, JoinSummary, analyze_join,
    read_join_header,
};
use std::thread::{self, JoinHandle};

/// Modeless join draft; source and destination reservations live with this state.
pub(super) struct JoinDialog {
    paths: [Option<PathBuf>; 2],
    headers: [Option<HeaderInfo>; 2],
    keys: Vec<[usize; 2]>,
    ignore_ascii_case: bool,
    require_unique: [bool; 2],
    acknowledge_expansion: bool,
    mode: JoinMode,
    temporary: PathBuf,
    header_job: Option<(usize, JoinJob<HeaderInfo>)>,
    checking: Option<JoinJob<JoinPlan>>,
    writing: Option<JoinJob<JoinSummary>>,
    cleanup: Option<JoinHandle<Result<(), String>>>,
    plan: Option<JoinPlan>,
    result: Option<JoinSummary>,
    destination: Option<PathBuf>,
    output_delimiter: u8,
    message: Option<String>,
    closing: bool,
    quit_after_close: bool,
    cancel_requested: bool,
}

enum DialogAction {
    None,
    Select(usize),
    Analyze,
    ChooseOutput,
    Create,
    OpenResult,
    Closed(bool),
}

impl JoinDialog {
    /// Initialize an empty draft without opening files or allocating private data.
    fn new(temporary: PathBuf) -> Self {
        Self {
            paths: [None, None],
            headers: [None, None],
            keys: vec![[0, 0]],
            ignore_ascii_case: false,
            require_unique: [false, false],
            acknowledge_expansion: false,
            mode: JoinMode::Left,
            temporary,
            header_job: None,
            checking: None,
            writing: None,
            cleanup: None,
            plan: None,
            result: None,
            destination: None,
            output_delimiter: b',',
            message: None,
            closing: false,
            quit_after_close: false,
            cancel_requested: false,
        }
    }

    /// Count header, analysis and creation work against the shared operation limit.
    pub(super) fn running_operation(&self) -> bool {
        self.header_job.is_some() || self.checking.is_some() || self.writing.is_some()
    }

    /// Keep the dialog alive until both workers and explicit cleanup have finished.
    fn busy(&self) -> bool {
        self.running_operation() || self.cleanup.is_some()
    }

    /// Protect selected paths only while a worker or valid preview uses them.
    pub(super) fn reserved_inputs(&self) -> impl Iterator<Item = &Path> {
        let retained = self.busy() || self.plan.is_some();
        self.paths
            .iter()
            .filter(move |_| retained)
            .filter_map(|path| path.as_deref())
    }

    /// Reserve the chosen destination until the retained plan is released.
    pub(super) fn reserved_output(&self) -> Option<&Path> {
        if self.plan.is_some() || self.writing.is_some() {
            self.destination.as_deref()
        } else {
            None
        }
    }

    /// Release a retained plan off-thread before allowing another analysis.
    fn reset(&mut self) {
        self.acknowledge_expansion = false;
        if let Some(plan) = self.plan.take() {
            self.cleanup = Some(thread::spawn(move || {
                plan.release().map_err(|e| e.to_string())
            }));
        }
        self.destination = None;
        self.result = None;
        self.message = None;
    }

    /// Cancel active work and defer close or quit until cleanup completes.
    pub(super) fn request_close(&mut self, quit: bool) {
        self.closing = true;
        self.quit_after_close |= quit;
        self.cancel();
        if self.plan.is_some() {
            self.reset();
        }
        self.message = Some("Finishing join cleanup before closing…".into());
    }

    /// Request worker cancellation without dropping a running writer on the UI thread.
    fn cancel(&mut self) {
        if let Some((_, job)) = &self.header_job {
            job.cancel();
        }
        if let Some(job) = &self.checking {
            job.cancel();
        }
        if let Some(job) = &self.writing {
            job.cancel();
        }
        self.cancel_requested = true;
    }

    /// Preserve worker failures and cleanup warnings for explicit user review.
    fn worker_failed(&mut self, message: String) {
        self.closing = false;
        self.quit_after_close = false;
        self.message = Some(message);
    }

    /// Adopt completed workers and retain published results even after cancellation.
    fn poll(&mut self) {
        if self
            .header_job
            .as_ref()
            .is_some_and(|(_, job)| job.progress().done)
        {
            let (side, job) = self.header_job.take().unwrap();
            let result = job.wait();
            if self.cancel_requested {
                self.message = Some("Header reading cancelled.".into());
            } else {
                match result {
                    Ok(Some(header)) => {
                        self.headers[side] = Some(header);
                        for pair in &mut self.keys {
                            pair[side] = 0;
                        }
                    }
                    Ok(None) => self.message = Some("Header reading cancelled.".into()),
                    Err(error) => self.worker_failed(error.to_string()),
                }
            }
            self.cancel_requested = false;
        }
        if self
            .checking
            .as_ref()
            .is_some_and(|job| job.progress().done)
        {
            match self.checking.take().unwrap().wait() {
                Ok(Some(plan)) => {
                    self.output_delimiter = plan.inputs()[0].delimiter;
                    if self.closing {
                        self.cleanup = Some(thread::spawn(move || {
                            plan.release().map_err(|e| e.to_string())
                        }));
                    } else {
                        self.plan = Some(plan);
                    }
                }
                Ok(None) => {
                    self.message = Some("Analysis cancelled. No output was created.".into())
                }
                Err(error) => self.worker_failed(error.to_string()),
            }
            self.cancel_requested = false;
        }
        if self.writing.as_ref().is_some_and(|job| job.progress().done) {
            match self.writing.take().unwrap().wait() {
                Ok(Some(result)) => {
                    if let Some(warning) = &result.cleanup_warning {
                        self.worker_failed(format!(
                            "The joined file is saved. Cleanup needs attention: {warning}"
                        ));
                    } else {
                        self.message = None;
                    }
                    self.result = Some(result);
                }
                Ok(None) => self.message = Some("Join cancelled. No output was created.".into()),
                Err(error) => self.worker_failed(error.to_string()),
            }
            self.cancel_requested = false;
        }
        if self.cleanup.as_ref().is_some_and(JoinHandle::is_finished) {
            match self.cleanup.take().unwrap().join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    self.closing = false;
                    self.quit_after_close = false;
                    self.message = Some(format!("Could not remove private join files: {error}"));
                }
                Err(_) => {
                    self.closing = false;
                    self.quit_after_close = false;
                    self.message = Some("Join cleanup failed.".into());
                }
            }
        }
    }

    /// Read picker metadata off-thread with a separate bounded label budget.
    fn read_header(&mut self, side: usize, path: PathBuf) {
        self.paths[side] = Some(path.clone());
        self.headers[side] = None;
        self.message = None;
        self.result = None;
        self.destination = None;
        self.cancel_requested = false;
        match read_join_header(path) {
            Ok(job) => self.header_job = Some((side, job)),
            Err(error) => self.message = Some(error.to_string()),
        }
    }

    /// Capture explicit saved-file settings for the full analysis worker.
    fn start_analysis(&mut self) {
        self.acknowledge_expansion = false;
        let inputs = std::array::from_fn(|side| JoinInput {
            path: self.paths[side].as_ref().unwrap().clone(),
            delimiter: Some(self.headers[side].as_ref().unwrap().delimiter),
            has_header: true,
            encoding: Some(Encoding::Utf8),
            key_columns: self.keys.iter().map(|pair| pair[side]).collect(),
            expected_version: Some(self.headers[side].as_ref().unwrap().source_version.clone()),
            require_unique: self.require_unique[side],
        });
        self.message = None;
        self.result = None;
        self.cancel_requested = false;
        match analyze_join(
            inputs,
            self.temporary.clone(),
            JoinConfig {
                mode: self.mode,
                case: if self.ignore_ascii_case {
                    JoinCase::AsciiInsensitive
                } else {
                    JoinCase::Sensitive
                },
                ..JoinConfig::default()
            },
        ) {
            Ok(job) => self.checking = Some(job),
            Err(error) => self.message = Some(error.to_string()),
        }
    }

    /// Consume the retained plan only after application-level guards pass.
    fn start_write(&mut self) {
        let Some(plan) = self.plan.take() else {
            return;
        };
        let Some(destination) = self.destination.clone() else {
            self.plan = Some(plan);
            return;
        };
        self.message = None;
        self.cancel_requested = false;
        match plan.start_write_acknowledged(destination, self.acknowledge_expansion) {
            Ok(job) => self.writing = Some(job),
            Err(error) => self.message = Some(error.to_string()),
        }
    }

    /// Pair identity and ordering are shared by both sources.
    fn show_key_pairs(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.strong("Key column pairs");
        ui.small("All pairs must match. Empty or missing components never match.");
        ui.checkbox(
            &mut self.ignore_ascii_case,
            "Ignore ASCII letter case (A–Z only)",
        );
        let count = self.keys.len();
        let mut edit = None;
        for (index, pair) in self.keys.iter_mut().enumerate() {
            ui.push_id(("join-key-pair", index), |ui| {
                ui.label(format!("Pair {}", index + 1));
                ui.columns(2, |columns| {
                    for (side, column) in columns.iter_mut().enumerate() {
                        let role = if side == 0 { "Main" } else { "Additional" };
                        if let Some(header) = &self.headers[side] {
                            let label = header
                                .columns
                                .get(pair[side])
                                .map(|name| format!("{role} {}: {name}", pair[side] + 1))
                                .unwrap_or_else(|| format!("{role}: choose a column"));
                            egui::ComboBox::from_id_salt(("column", side))
                                .selected_text(format!("Pair {} · {label}", index + 1))
                                .width(245.0)
                                .show_ui(column, |ui| {
                                    egui::ScrollArea::vertical().max_height(220.0).show_rows(
                                        ui,
                                        22.0,
                                        header.columns.len(),
                                        |ui, range| {
                                            for i in range {
                                                ui.selectable_value(
                                                    &mut pair[side],
                                                    i,
                                                    format!(
                                                        "{role} {}: {}",
                                                        i + 1,
                                                        header.columns[i]
                                                    ),
                                                );
                                            }
                                        },
                                    );
                                });
                        } else {
                            column.label(format!("{role}: choose a file first"));
                        }
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add_enabled(
                            index > 0,
                            egui::Button::new(format!("Move pair {} up", index + 1)),
                        )
                        .clicked()
                    {
                        edit = Some((index, 0));
                    }
                    if ui
                        .add_enabled(
                            index + 1 < count,
                            egui::Button::new(format!("Move pair {} down", index + 1)),
                        )
                        .clicked()
                    {
                        edit = Some((index, 1));
                    }
                    if ui
                        .add_enabled(
                            count > 1,
                            egui::Button::new(format!("Remove pair {}", index + 1)),
                        )
                        .clicked()
                    {
                        edit = Some((index, 2));
                    }
                });
            });
        }
        if let Some((index, action)) = edit {
            match action {
                0 => self.keys.swap(index, index - 1),
                1 => self.keys.swap(index, index + 1),
                _ => {
                    self.keys.remove(index);
                }
            }
        }
        if ui
            .add_enabled(
                self.keys.len() < 16 && self.headers.iter().all(Option::is_some),
                egui::Button::new("Add key pair"),
            )
            .clicked()
        {
            let next = self.keys.len();
            self.keys.push(std::array::from_fn(|side| {
                next.min(self.headers[side].as_ref().unwrap().columns.len() - 1)
            }));
        }
    }

    /// Render the modeless workflow while leaving unrelated documents usable.
    fn show(&mut self, ctx: &egui::Context, can_start: bool) -> DialogAction {
        self.poll();
        if self.closing && !self.busy() && self.plan.is_none() {
            return DialogAction::Closed(self.quit_after_close);
        }
        let busy = self.busy();
        let editable = !busy && self.plan.is_none() && self.result.is_none();
        let mut action = DialogAction::None;
        let mut open = true;
        egui::Window::new("Join Files").id(egui::Id::new("join-files"))
            .open(&mut open).default_width(680.0).min_width(480.0)
            .show(ctx,|ui|{
                ui.label("Match key columns and create a separate joined file.");
                ui.small("Saved UTF-8 files with headers. Retain every matching pair. Empty keys never match.");
                ui.small("Keys match whole values. Leading zeros, whitespace and non-ASCII case remain significant.");
                egui::ScrollArea::vertical().max_height((ctx.content_rect().height()-210.0).max(180.0)).show(ui,|ui|{
                    ui.add_enabled_ui(editable, |ui| {
                        ui.horizontal(|ui| {
                            ui.label("Rows to keep:");
                            egui::ComboBox::from_id_salt("join-mode").selected_text(self.mode.label()).show_ui(ui, |ui| {
                                for mode in [JoinMode::Left, JoinMode::Inner, JoinMode::Right, JoinMode::Full] {
                                    ui.selectable_value(&mut self.mode, mode, mode.label());
                                }
                            });
                        });
                    });
                    ui.columns(2,|columns|{
                        for (side,column) in columns.iter_mut().enumerate(){
                            column.strong(if side==0{"Main file"}else{"Additional file"});
                            column.add(egui::Label::new(self.paths[side].as_ref().and_then(|p|p.file_name()).map_or_else(||"No file selected".into(),|n|n.to_string_lossy().into_owned())).truncate());
                            if column.add_enabled(editable && can_start,egui::Button::new(if side==0{"Choose Main…"}else{"Choose Additional…"})).clicked(){action=DialogAction::Select(side);}
                            column.add_enabled_ui(editable,|ui|{
                                ui.checkbox(&mut self.require_unique[side], if side == 0 { "Require unique Main key" } else { "Require unique Additional key" });
                                if let Some(header)=&self.headers[side]{
                                    ui.small(format!("{} columns · UTF-8 · header row",header.columns.len()));
                                }else{ui.label("Key column: choose a file first");}
                            });
                        }
                    });
                    ui.add_enabled_ui(editable, |ui| self.show_key_pairs(ui));
                    if let Some(plan)=&self.plan{
                        let a=plan.analysis();
                        ui.separator();
                        ui.label(format!("Main: {} rows · Additional: {} rows",a.input_rows[0],a.input_rows[1]));
                        ui.label(format!("Matching pairs: {} · Result: {} rows",a.matching_pairs,a.output_rows));
                        ui.label(format!("Unmatched: {} Main, {} Additional",a.unmatched_rows[0],a.unmatched_rows[1]));
                        ui.label(format!("Empty keys: {} Main, {} Additional",a.empty_key_rows[0],a.empty_key_rows[1]));
                        ui.label(format!("Duplicate-key groups: {} Main, {} Additional",a.duplicate_groups[0],a.duplicate_groups[1]));
                        if a.many_to_many_groups > 0 {
                            ui.label(RichText::new(format!("{} many-to-many groups produce {} matching pairs.",a.many_to_many_groups,a.many_to_many_pairs)).strong());
                            ui.checkbox(&mut self.acknowledge_expansion,format!("I understand this will create {} rows.",a.output_rows));
                        }
                        for sample in &a.duplicate_samples {
                            ui.small(format!("Key {}: {} Main × {} Additional",sample.key,sample.main_rows,sample.additional_rows));
                        }
                        ui.small(format!("Output: {} · Conservative additional storage: {}",format_bytes(a.output_bytes),format_bytes(a.additional_temporary_bytes)));
                        ui.small("Sample in output order; at most 20 rows and columns. Long values are clipped.");
                        egui::ScrollArea::both().max_height(150.0).show(ui,|ui|{
                            egui::Grid::new("join-preview").striped(true).show(ui,|ui|{
                                for row in &a.preview {for field in row{ui.label(field);}ui.end_row();}
                            });
                        });
                    }
                    if let Some(result)=&self.result {
                        ui.separator();ui.strong(format!("Saved {} rows · {}",result.data_rows,format_bytes(result.bytes_written)));
                        ui.add(egui::Label::new(result.destination.display().to_string()).wrap());
                    }
                    if let Some(path)=&self.destination {ui.add(egui::Label::new(format!("Output: {}",path.display())).truncate());}
                    if let Some(message)=&self.message {ui.add(egui::Label::new(RichText::new(message).color(ERROR_TEXT)).wrap());}
                    if let Some(progress)=self.checking.as_ref().map(JoinJob::progress).or_else(||self.writing.as_ref().map(JoinJob::progress)){
                        ui.horizontal(|ui|{ui.spinner();ui.label(format!("{} · {} rows processed",progress.phase.label(),progress.records));});
                    }else if busy {ui.horizontal(|ui|{ui.spinner();ui.label(if self.cleanup.is_some(){"Releasing private join data…"}else{"Reading header…"});});}
                });
                ui.separator();
                ui.horizontal_wrapped(|ui|{
                    if ui.button(if busy{"Cancel and Close"}else{"Close"}).clicked(){self.request_close(false);}
                    if busy {
                        if ui.add_enabled(!self.cancel_requested && self.cleanup.is_none(),egui::Button::new("Cancel")).clicked(){self.cancel();}
                    }else if self.result.is_some(){
                        if ui.button("Open Joined File").clicked(){action=DialogAction::OpenResult;}
                    }else if self.plan.is_some(){
                        if ui.button("Reset Analysis").clicked(){self.reset();}
                        if ui.button("Choose Output…").clicked(){action=DialogAction::ChooseOutput;}
                        if ui.add_enabled(can_start && self.destination.is_some() && self.plan.as_ref().is_none_or(|plan| plan.analysis().many_to_many_groups == 0 || self.acknowledge_expansion),egui::Button::new("Create Joined File")).clicked(){action=DialogAction::Create;}
                    }else if ui.add_enabled(can_start && self.headers.iter().all(Option::is_some),egui::Button::new("Analyze & Preview")).clicked(){
                        action=DialogAction::Analyze;
                    }
                });
                if !can_start && !busy {ui.small("Two file operations are running. Wait for a free slot before starting another.");}
            });
        if !open {
            self.request_close(false);
        }
        if self.busy() {
            ctx.request_repaint_after(POLL_INTERVAL);
        }
        action
    }
}

/// Absent output names still have an existing parent. Resolve that parent so
/// aliases and nested folders cannot escape a live working directory reservation.
fn output_is_inside_working_directory(path: &Path, directory: &Path) -> bool {
    if path.starts_with(directory) {
        return true;
    }
    path.parent()
        .and_then(|parent| parent.canonicalize().ok())
        .zip(directory.canonicalize().ok())
        .is_some_and(|(parent, directory)| parent.starts_with(directory))
}

impl QuarryApp {
    /// Apply the existing alias rules to live join input and output reservations.
    pub(super) fn join_reserves_path(&self, path: &Path) -> bool {
        self.join_files.as_ref().is_some_and(|join| {
            join.reserved_inputs()
                .chain(join.reserved_output())
                .any(|reserved| tabs::paths_alias(path, reserved))
        })
    }

    /// Refuse saved-file input that would silently omit an open document’s edits.
    fn join_input_is_open(&self, path: &Path) -> bool {
        self.document_tabs().any(|tab| {
            tab.document.as_ref().is_some_and(|doc| {
                tabs::paths_alias(path, &doc.logical_path)
                    || tabs::paths_alias(path, doc.session.path())
            }) || tab
                .write_destination
                .as_ref()
                .is_some_and(|destination| tabs::paths_alias(path, destination))
        })
    }

    /// Recheck all live document and writer paths before starting output.
    fn join_destination_is_available(&self, path: &Path) -> bool {
        // Keep the retained analysis when a picker chooses an existing output.
        // symlink_metadata also rejects dangling links; only absence is writable.
        if !matches!(std::fs::symlink_metadata(path), Err(error)
            if error.kind() == std::io::ErrorKind::NotFound)
        {
            return false;
        }
        !self.document_tabs().any(|tab| {
            tab.document.as_ref().is_some_and(|doc| {
                tabs::paths_alias(path, &doc.logical_path)
                    || tabs::paths_alias(path, doc.session.path())
                    || doc.working_copy.as_ref().is_some_and(|state| {
                        std::iter::once(state.directory.path())
                            .chain(
                                state
                                    .previous_directories
                                    .iter()
                                    .map(|directory| directory.path()),
                            )
                            .any(|directory| output_is_inside_working_directory(path, directory))
                    })
            }) || tab
                .write_destination
                .as_ref()
                .is_some_and(|destination| tabs::paths_alias(path, destination))
        }) && !self
            .combine_files
            .as_ref()
            .and_then(combine_files::CombineDialog::reserved_output)
            .is_some_and(|destination| tabs::paths_alias(path, destination))
    }

    /// Keep one join draft, using the configured private-storage location.
    pub(super) fn open_join_files(&mut self) {
        if self.join_files.is_none() {
            self.join_files = Some(JoinDialog::new(self.working_directory.clone()));
        }
    }

    /// Route UI actions through fresh capacity and path-reservation checks.
    pub(super) fn show_join_files(&mut self, ctx: &egui::Context) {
        let can_start = self.write_slot_available();
        let Some(dialog) = self.join_files.as_mut() else {
            return;
        };
        let action = dialog.show(ctx, can_start);
        match action {
            DialogAction::None => {}
            DialogAction::Closed(quit) => {
                self.join_files = None;
                if quit {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            DialogAction::Select(side) => {
                if let Some(path) = rfd::FileDialog::new()
                    .set_title("Choose a saved UTF-8 file")
                    .pick_file()
                {
                    if self.join_input_is_open(&path) {
                        self.join_files.as_mut().unwrap().message=Some("This file is open in Quarry. The first join version uses saved files only. Close that document before selecting it.".into());
                    } else if self.write_slot_available() {
                        self.join_files.as_mut().unwrap().read_header(side, path);
                    }
                }
            }
            DialogAction::Analyze => {
                let reserved = self
                    .join_files
                    .as_ref()
                    .unwrap()
                    .paths
                    .iter()
                    .flatten()
                    .any(|path| self.join_input_is_open(path));
                if reserved {
                    self.join_files.as_mut().unwrap().message = Some(
                        "An input is now open or being written. Close it and analyze again.".into(),
                    );
                } else if self.write_slot_available() {
                    self.join_files.as_mut().unwrap().start_analysis();
                }
            }
            DialogAction::ChooseOutput => {
                let delimiter = self.join_files.as_ref().unwrap().output_delimiter;
                let extension = if delimiter == b'\t' { "tsv" } else { "csv" };
                if let Some(path) = rfd::FileDialog::new()
                    .set_title("Save joined file")
                    .set_file_name(format!("joined.{extension}"))
                    .save_file()
                {
                    if self.join_destination_is_available(&path) {
                        self.join_files.as_mut().unwrap().destination = Some(path);
                    } else {
                        self.join_files.as_mut().unwrap().message=Some("Choose a new output path. Existing files, open documents and active output destinations cannot be replaced.".into());
                    }
                }
            }
            DialogAction::Create => {
                let path = self.join_files.as_ref().unwrap().destination.clone();
                if path
                    .as_ref()
                    .is_some_and(|path| self.join_destination_is_available(path))
                    && self.write_slot_available()
                {
                    self.join_files.as_mut().unwrap().start_write();
                } else {
                    self.join_files.as_mut().unwrap().message=Some("The output or operation slot is no longer available. Choose another output or wait.".into());
                }
            }
            DialogAction::OpenResult => {
                let dialog = self.join_files.take().unwrap();
                let path = dialog.result.as_ref().unwrap().destination.clone();
                if let Err(error) = self.open_document_tab_with_encoding(
                    path,
                    OpenOptions {
                        delimiter: Some(dialog.output_delimiter),
                        header_mode: HeaderMode::FirstRow,
                        ..Default::default()
                    },
                    Some(Encoding::Utf8),
                ) {
                    self.join_files = Some(dialog);
                    self.join_files.as_mut().unwrap().message = Some(format!(
                        "The joined file is saved. {} Free a tab and try Open Joined File again.",
                        error.text
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Render the real workflow with synthetic accessibility input.
    fn frame(
        app: &mut QuarryApp,
        ctx: &egui::Context,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
        ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 650.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| app.show_join_files(ctx),
        )
    }
    /// Activate a visible control through its AccessKit action.
    fn click(app: &mut QuarryApp, ctx: &egui::Context, label: &str) {
        frame(app, ctx, vec![]);
        let output = frame(app, ctx, vec![]);
        let tree = output.platform_output.accesskit_update.unwrap();
        let (id, _) = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some(label) || node.value() == Some(label))
            .unwrap_or_else(|| panic!("missing button {label}"));
        frame(
            app,
            ctx,
            vec![egui::Event::AccessKitActionRequest(
                egui::accesskit::ActionRequest {
                    action: egui::accesskit::Action::Click,
                    target: *id,
                    data: None,
                },
            )],
        );
    }
    /// Poll real worker completion for deterministic integration assertions.
    fn finish(app: &mut QuarryApp) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.join_files.as_ref().is_some_and(JoinDialog::busy) {
            assert!(Instant::now() < deadline, "join workflow timed out");
            app.join_files.as_mut().unwrap().poll();
            thread::sleep(Duration::from_millis(1));
        }
    }
    /// Create synthetic CSV sources isolated from user data.
    fn fixture(dir: &Path, name: &str, body: &[u8]) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        path
    }
    /// Select both sources through the same asynchronous header path as the picker.
    fn prepare(app: &mut QuarryApp, dir: &Path) -> (PathBuf, PathBuf) {
        let a = fixture(dir, "main.csv", b"id,name\nb,Bob\na,Alice\nz,Last\n");
        let b = fixture(dir, "additional.csv", b"id,city\na,Seattle\nb,LA\n");
        app.open_join_files();
        app.join_files.as_mut().unwrap().temporary = dir.to_path_buf();
        app.join_files.as_mut().unwrap().read_header(0, a.clone());
        finish(app);
        app.join_files.as_mut().unwrap().read_header(1, b.clone());
        finish(app);
        (a, b)
    }
    /// Enable accessibility actions on the production visual style.
    fn context() -> egui::Context {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        configure_style(&ctx);
        ctx
    }

    #[test]
    /// Check the complete create/open flow while another document retains edits.
    fn complete_workflow_opens_result_and_preserves_other_dirty_documents() {
        let dir = tempfile::tempdir().unwrap();
        let other = fixture(dir.path(), "other.csv", b"name,id\nOther,1\n");
        let mut app = QuarryApp::new(Some(other.clone()), Instant::now());
        app.active
            .document
            .as_mut()
            .unwrap()
            .rename_header(0, "Changed".into())
            .unwrap();
        let other_id = app.active.id;
        let ctx = context();
        let (a, b) = prepare(&mut app, dir.path());
        click(&mut app, &ctx, "Analyze & Preview");
        finish(&mut app);
        assert!(app.join_files.as_ref().unwrap().plan.is_some());
        assert!(app.open_new_path(a.clone()).is_err());
        assert!(app.active.document.as_ref().unwrap().is_dirty());
        let output = dir.path().join("joined.csv");
        app.join_files.as_mut().unwrap().destination = Some(output.clone());
        assert!(!app.destination_available(&output));
        click(&mut app, &ctx, "Create Joined File");
        finish(&mut app);
        let result = app.join_files.as_ref().unwrap().result.as_ref().unwrap();
        assert_eq!(result.data_rows, 3);
        assert_eq!(
            fs::read(&output).unwrap(),
            b"id (Main),name,id (Additional),city\nb,Bob,b,LA\na,Alice,a,Seattle\nz,Last,,\n"
        );
        click(&mut app, &ctx, "Open Joined File");
        assert!(app.join_files.is_none());
        assert_eq!(app.active.document.as_ref().unwrap().logical_path, output);
        assert!(
            app.document_tabs()
                .any(|tab| tab.id == other_id && tab.document.as_ref().unwrap().is_dirty())
        );
        assert_eq!(fs::read(a).unwrap(), b"id,name\nb,Bob\na,Alice\nz,Last\n");
        assert_eq!(fs::read(b).unwrap(), b"id,city\na,Seattle\nb,LA\n");
    }

    #[test]
    /// Verify reservations follow retained analysis rather than dialog visibility alone.
    fn analysis_rechecks_open_inputs_and_reset_releases_reservations() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = QuarryApp::new(None, Instant::now());
        let ctx = context();
        let (a, _) = prepare(&mut app, dir.path());
        app.open_new_path(a.clone()).unwrap();
        click(&mut app, &ctx, "Analyze & Preview");
        assert!(app.join_files.as_ref().unwrap().checking.is_none());
        assert!(
            app.join_files
                .as_ref()
                .unwrap()
                .message
                .as_ref()
                .unwrap()
                .contains("now open")
        );
        // Start with fresh documents so the selected saved inputs can be analyzed.
        let mut app = QuarryApp::new(None, Instant::now());
        prepare(&mut app, dir.path());
        click(&mut app, &ctx, "Analyze & Preview");
        finish(&mut app);
        assert!(app.join_reserves_path(&a));
        let other = fixture(dir.path(), "unrelated.csv", b"id\nok\n");
        app.open_new_path(other).unwrap();
        click(&mut app, &ctx, "Reset Analysis");
        finish(&mut app);
        assert!(!app.join_reserves_path(&a));
        assert!(app.open_new_path(a).is_ok());
        click(&mut app, &ctx, "Close");
        frame(&mut app, &ctx, vec![]);
        assert!(app.join_files.is_none());
    }

    #[test]
    /// Keep an idle preview cheap while preventing a third active file operation.
    fn operation_limit_is_rechecked_and_idle_preview_does_not_take_a_slot() {
        let dir = tempfile::tempdir().unwrap();
        let other = fixture(dir.path(), "other.csv", b"name,id\none,1\n");
        let another = fixture(dir.path(), "another.csv", b"name,id\ntwo,2\n");
        let mut app = QuarryApp::new(Some(other), Instant::now());
        let ctx = context();
        prepare(&mut app, dir.path());
        click(&mut app, &ctx, "Analyze & Preview");
        finish(&mut app);
        assert!(app.write_slot_available());
        app.active
            .document
            .as_mut()
            .unwrap()
            .rename_header(0, "First".into())
            .unwrap();
        assert!(app.save_as_picker_result(Some(dir.path().join("out-1.csv"))));
        app.open_new_path(another).unwrap();
        app.active
            .document
            .as_mut()
            .unwrap()
            .rename_header(0, "Second".into())
            .unwrap();
        assert!(app.save_as_picker_result(Some(dir.path().join("out-2.csv"))));
        assert!(!app.write_slot_available());
        let output = dir.path().join("joined.csv");
        app.join_files.as_mut().unwrap().destination = Some(output.clone());
        click(&mut app, &ctx, "Create Joined File");
        assert!(app.join_files.as_ref().unwrap().writing.is_none());
        assert!(!output.exists());
    }

    #[test]
    /// Preserve the published file and all existing tabs when opening is refused.
    fn full_tab_strip_keeps_saved_result_available() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = QuarryApp::new(None, Instant::now());
        let ctx = context();
        for i in 0..8 {
            app.open_new_path(fixture(dir.path(), &format!("tab-{i}.csv"), b"id\nvalue\n"))
                .unwrap();
        }
        prepare(&mut app, dir.path());
        click(&mut app, &ctx, "Analyze & Preview");
        finish(&mut app);
        let output = dir.path().join("joined.csv");
        app.join_files.as_mut().unwrap().destination = Some(output.clone());
        click(&mut app, &ctx, "Create Joined File");
        finish(&mut app);
        click(&mut app, &ctx, "Open Joined File");
        assert_eq!(app.document_tabs().count(), 8);
        let dialog = app.join_files.as_ref().unwrap();
        assert!(dialog.result.is_some());
        assert!(output.exists());
        assert!(dialog.message.as_ref().unwrap().contains("saved"));
    }

    #[test]
    /// Exercise deferred close without discarding unrelated unsaved state.
    fn closing_during_analysis_waits_for_cleanup_and_retains_other_work() {
        let dir = tempfile::tempdir().unwrap();
        let other = fixture(dir.path(), "other.csv", b"name,id\nBefore,1\n");
        let mut app = QuarryApp::new(Some(other.clone()), Instant::now());
        let ctx = context();
        app.active
            .document
            .as_mut()
            .unwrap()
            .rename_header(0, "Unsaved".into())
            .unwrap();
        prepare(&mut app, dir.path());
        app.join_files.as_mut().unwrap().start_analysis();
        app.join_files.as_mut().unwrap().request_close(true);
        finish(&mut app);
        frame(&mut app, &ctx, vec![]);
        assert!(app.join_files.is_none());
        assert!(app.active.document.as_ref().unwrap().is_dirty());
        assert_eq!(fs::read(other).unwrap(), b"name,id\nBefore,1\n");
        assert!(!fs::read_dir(dir.path()).unwrap().any(|p| {
            p.unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".quarry-")
        }));
    }
    #[test]
    /// A failed cleanup cancels quitting, so a later dialog Close stays local.
    fn failed_cleanup_does_not_keep_an_old_quit_request() {
        for panic in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let mut dialog = JoinDialog::new(dir.path().to_path_buf());
            dialog.request_close(true);
            dialog.cleanup = Some(thread::spawn(move || {
                if panic {
                    panic!("synthetic cleanup worker failure");
                }
                Err("synthetic cleanup error".to_owned())
            }));
            while !dialog.cleanup.as_ref().unwrap().is_finished() {
                thread::yield_now();
            }
            dialog.poll();
            assert!(!dialog.closing);
            assert!(
                dialog.message.as_ref().unwrap().contains("cleanup")
                    || dialog
                        .message
                        .as_ref()
                        .unwrap()
                        .contains("private join files")
            );
            dialog.request_close(false);
            assert!(dialog.closing);
            assert!(
                !dialog.quit_after_close,
                "closing the dialog must not retry the cancelled quit"
            );
        }
    }
    #[test]
    fn cancelling_a_header_read_discards_its_unadopted_result() {
        let dir = tempfile::tempdir().unwrap();
        let path = fixture(dir.path(), "header.csv", b"id,name\nx,A\n");
        let mut dialog = JoinDialog::new(dir.path().to_path_buf());
        dialog.read_header(0, path);
        while !dialog.header_job.as_ref().unwrap().1.progress().done {
            thread::yield_now();
        }
        dialog.cancel();
        dialog.poll();
        assert!(
            dialog.headers[0].is_none(),
            "cancelled header must not populate the draft"
        );
        assert!(dialog.message.as_ref().unwrap().contains("cancelled"));
        assert!(!dialog.running_operation());
    }
    #[test]
    fn review_regression_changed_probe_source_requires_reselection() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = QuarryApp::new(None, Instant::now());
        let ctx = context();
        let (a, _) = prepare(&mut app, dir.path());
        let replacement = fixture(
            dir.path(),
            "replacement.csv",
            b"name,id\nBob,b\nAlice,a\nLast,z\n",
        );
        fs::rename(replacement, &a).unwrap();
        click(&mut app, &ctx, "Analyze & Preview");
        finish(&mut app);
        let dialog = app.join_files.as_ref().unwrap();
        assert!(
            dialog.plan.is_none(),
            "a stale selected column must not silently join the reordered file"
        );
        assert!(dialog.message.as_ref().unwrap().contains("changed"));
    }

    #[cfg(unix)]
    #[test]
    fn review_regression_worker_cleanup_failure_cancels_closing() {
        use std::os::unix::fs::PermissionsExt;
        struct Restore(PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let mut app = QuarryApp::new(None, Instant::now());
        let ctx = context();
        prepare(&mut app, dir.path());
        click(&mut app, &ctx, "Analyze & Preview");
        finish(&mut app);
        let workspace = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".quarry-join-")
            })
            .unwrap();
        let _restore = Restore(workspace.clone());
        fs::set_permissions(&workspace, fs::Permissions::from_mode(0o500)).unwrap();
        let dialog = app.join_files.as_mut().unwrap();
        dialog.destination = Some(dir.path().join("result.csv"));
        dialog.start_write();
        while !dialog.writing.as_ref().unwrap().progress().done {
            thread::yield_now();
        }
        dialog.request_close(true);
        dialog.poll();
        assert!(
            dialog
                .message
                .as_ref()
                .unwrap()
                .contains("Could not remove private join data")
        );
        assert!(
            !dialog.closing,
            "a cleanup failure must remain visible instead of closing the dialog"
        );
        assert!(!dialog.quit_after_close);
    }

    #[test]
    fn many_to_many_creation_requires_acknowledgement_for_each_analysis() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = QuarryApp::new(None, Instant::now());
        let ctx = context();
        let (a, b) = prepare(&mut app, dir.path());
        fs::write(&a, b"id,name\nx,A\nx,B\n").unwrap();
        fs::write(&b, b"id,city\nx,One\nx,Two\n").unwrap();
        app.join_files.as_mut().unwrap().read_header(0, a);
        finish(&mut app);
        app.join_files.as_mut().unwrap().read_header(1, b);
        finish(&mut app);
        click(&mut app, &ctx, "Analyze & Preview");
        finish(&mut app);
        assert_eq!(
            app.join_files
                .as_ref()
                .unwrap()
                .plan
                .as_ref()
                .unwrap()
                .analysis()
                .output_rows,
            4
        );
        let output = dir.path().join("expanded.csv");
        app.join_files.as_mut().unwrap().destination = Some(output.clone());
        click(&mut app, &ctx, "Create Joined File");
        assert!(app.join_files.as_ref().unwrap().writing.is_none());
        assert!(!output.exists());
        click(&mut app, &ctx, "I understand this will create 4 rows.");
        assert!(app.join_files.as_ref().unwrap().acknowledge_expansion);
        click(&mut app, &ctx, "Reset Analysis");
        finish(&mut app);
        assert!(!app.join_files.as_ref().unwrap().acknowledge_expansion);
        click(&mut app, &ctx, "Analyze & Preview");
        finish(&mut app);
        app.join_files.as_mut().unwrap().destination = Some(output.clone());
        click(&mut app, &ctx, "I understand this will create 4 rows.");
        click(&mut app, &ctx, "Create Joined File");
        finish(&mut app);
        assert_eq!(
            fs::read(output).unwrap(),
            b"id (Main),name,id (Additional),city\nx,A,x,One\nx,A,x,Two\nx,B,x,One\nx,B,x,Two\n"
        );
    }

    #[test]
    fn a_selected_uniqueness_requirement_blocks_repeated_input_keys() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = QuarryApp::new(None, Instant::now());
        let ctx = context();
        let (a, _) = prepare(&mut app, dir.path());
        fs::write(&a, b"id,name\nx,A\nx,B\n").unwrap();
        app.join_files.as_mut().unwrap().read_header(0, a);
        finish(&mut app);
        click(&mut app, &ctx, "Require unique Main key");
        click(&mut app, &ctx, "Analyze & Preview");
        finish(&mut app);
        assert!(app.join_files.as_ref().unwrap().plan.is_none());
        assert!(
            app.join_files
                .as_ref()
                .unwrap()
                .message
                .as_ref()
                .unwrap()
                .contains("Require unique key")
        );
    }
    #[test]
    fn mode_picker_controls_preview_creation_and_output_order() {
        for (mode, expected) in [
            (
                JoinMode::Left,
                b"b,Bob,b,LA\na,Alice,a,Seattle\nz,Last,,\n".as_slice(),
            ),
            (
                JoinMode::Inner,
                b"b,Bob,b,LA\na,Alice,a,Seattle\n".as_slice(),
            ),
            (
                JoinMode::Right,
                b"a,Alice,a,Seattle\nb,Bob,b,LA\n".as_slice(),
            ),
            (
                JoinMode::Full,
                b"b,Bob,b,LA\na,Alice,a,Seattle\nz,Last,,\n".as_slice(),
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut app = QuarryApp::new(None, Instant::now());
            let ctx = context();
            prepare(&mut app, dir.path());
            if mode != JoinMode::Left {
                click(&mut app, &ctx, JoinMode::Left.label());
                click(&mut app, &ctx, mode.label());
            }
            assert_eq!(app.join_files.as_ref().unwrap().mode, mode);
            click(&mut app, &ctx, "Analyze & Preview");
            finish(&mut app);
            let output = dir.path().join("mode.csv");
            app.join_files.as_mut().unwrap().destination = Some(output.clone());
            click(&mut app, &ctx, "Create Joined File");
            finish(&mut app);
            let mut wanted = b"id (Main),name,id (Additional),city\n".to_vec();
            wanted.extend_from_slice(expected);
            assert_eq!(fs::read(output).unwrap(), wanted, "{mode:?}");
        }
    }
    #[test]
    fn composite_pair_controls_and_ascii_case_reach_the_created_file() {
        let dir = tempfile::tempdir().unwrap();
        let a = fixture(
            dir.path(),
            "main.csv",
            b"region,account,label\nEU,001,A\neu,001,B\nEU,002,C\n",
        );
        let b = fixture(
            dir.path(),
            "additional.csv",
            b"acct,area,city\n001,eu,Paris\n002,EU,Rome\n",
        );
        let mut app = QuarryApp::new(None, Instant::now());
        let ctx = context();
        app.open_join_files();
        app.join_files.as_mut().unwrap().temporary = dir.path().to_path_buf();
        app.join_files.as_mut().unwrap().read_header(0, a.clone());
        finish(&mut app);
        app.join_files.as_mut().unwrap().read_header(1, b.clone());
        finish(&mut app);
        click(&mut app, &ctx, "Pair 1 · Additional 1: acct");
        click(&mut app, &ctx, "Additional 2: area");
        click(&mut app, &ctx, "Add key pair");
        click(&mut app, &ctx, "Pair 2 · Additional 2: area");
        click(&mut app, &ctx, "Additional 1: acct");
        click(&mut app, &ctx, "Move pair 2 up");
        assert_eq!(app.join_files.as_ref().unwrap().keys, vec![[1, 0], [0, 1]]);
        click(&mut app, &ctx, "Analyze & Preview");
        finish(&mut app);
        assert_eq!(
            app.join_files
                .as_ref()
                .unwrap()
                .plan
                .as_ref()
                .unwrap()
                .analysis()
                .matching_pairs,
            2
        );
        click(&mut app, &ctx, "Reset Analysis");
        finish(&mut app);
        click(&mut app, &ctx, "Ignore ASCII letter case (A–Z only)");
        click(&mut app, &ctx, "Analyze & Preview");
        finish(&mut app);
        assert_eq!(
            app.join_files
                .as_ref()
                .unwrap()
                .plan
                .as_ref()
                .unwrap()
                .analysis()
                .matching_pairs,
            3
        );
        let output = dir.path().join("composite.csv");
        app.join_files.as_mut().unwrap().destination = Some(output.clone());
        click(&mut app, &ctx, "Create Joined File");
        finish(&mut app);
        assert_eq!(fs::read(output).unwrap(),
            b"region,account,label,acct,area,city\nEU,001,A,001,eu,Paris\neu,001,B,001,eu,Paris\nEU,002,C,002,EU,Rome\n");
        assert_eq!(
            fs::read(a).unwrap(),
            b"region,account,label\nEU,001,A\neu,001,B\nEU,002,C\n"
        );
        assert_eq!(
            fs::read(b).unwrap(),
            b"acct,area,city\n001,eu,Paris\n002,EU,Rome\n"
        );
    }

    #[test]
    fn pair_removal_preserves_one_pair_and_source_change_resets_its_columns() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = QuarryApp::new(None, Instant::now());
        let ctx = context();
        prepare(&mut app, dir.path());
        click(&mut app, &ctx, "Add key pair");
        assert_eq!(app.join_files.as_ref().unwrap().keys, vec![[0, 0], [1, 1]]);
        click(&mut app, &ctx, "Move pair 1 down");
        assert_eq!(app.join_files.as_ref().unwrap().keys, vec![[1, 1], [0, 0]]);
        click(&mut app, &ctx, "Remove pair 2");
        assert_eq!(app.join_files.as_ref().unwrap().keys, vec![[1, 1]]);
        click(&mut app, &ctx, "Remove pair 1");
        assert_eq!(app.join_files.as_ref().unwrap().keys.len(), 1);
        let replacement = fixture(dir.path(), "replacement.csv", b"only\nvalue\n");
        app.join_files.as_mut().unwrap().read_header(0, replacement);
        finish(&mut app);
        assert_eq!(app.join_files.as_ref().unwrap().keys, vec![[0, 1]]);
    }
    #[test]
    fn existing_output_keeps_analysis_and_existing_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = QuarryApp::new(None, Instant::now());
        let ctx = context();
        prepare(&mut app, dir.path());
        click(&mut app, &ctx, "Analyze & Preview");
        finish(&mut app);
        let output = fixture(dir.path(), "existing.csv", b"keep,this\n");
        app.join_files.as_mut().unwrap().destination = Some(output.clone());
        click(&mut app, &ctx, "Create Joined File");
        finish(&mut app);
        let dialog = app.join_files.as_ref().unwrap();
        assert!(
            dialog.plan.is_some(),
            "an existing output must not consume the analyzed plan"
        );
        assert!(dialog.writing.is_none());
        assert_eq!(fs::read(&output).unwrap(), b"keep,this\n");
        assert!(!app.join_destination_is_available(&output));
        #[cfg(unix)]
        {
            let link = dir.path().join("dangling.csv");
            std::os::unix::fs::symlink(dir.path().join("missing.csv"), &link).unwrap();
            assert!(!app.join_destination_is_available(&link));
        }
        let fresh = dir.path().join("new.csv");
        app.join_files.as_mut().unwrap().destination = Some(fresh.clone());
        click(&mut app, &ctx, "Create Joined File");
        finish(&mut app);
        assert_eq!(
            fs::read(fresh).unwrap(),
            b"id (Main),name,id (Additional),city\nb,Bob,b,LA\na,Alice,a,Seattle\nz,Last,,\n"
        );
    }
    #[test]
    fn join_outputs_cannot_enter_current_or_retained_working_directories() {
        let directory = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let source = fixture(directory.path(), "source.csv", b"id,value\na,one\n");
        let mut app = QuarryApp::new(Some(source), Instant::now());
        let mut state = WorkingCopyState::new_in(directory.path()).unwrap();
        let previous = state.directory.path().to_owned();
        state.use_directory(other.path()).unwrap();
        let current = state.directory.path().to_owned();
        app.active.document.as_mut().unwrap().working_copy = Some(state);
        for root in [&previous, &current] {
            fs::create_dir(root.join("nested")).unwrap();
            assert!(!app.join_destination_is_available(&root.join("joined.csv")));
            assert!(!app.join_destination_is_available(&root.join("nested/joined.csv")));
            assert!(
                !app.join_destination_is_available(
                    &root.canonicalize().unwrap().join("joined.csv")
                )
            );
        }
        #[cfg(unix)]
        {
            let alias = directory.path().join("working-alias");
            std::os::unix::fs::symlink(&previous, &alias).unwrap();
            assert!(!app.join_destination_is_available(&alias.join("joined.csv")));
            assert!(!app.join_destination_is_available(&alias.join("nested/joined.csv")));
        }
        assert!(app.join_destination_is_available(&directory.path().join("normal-result.csv")));
    }
}
