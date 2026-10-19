use super::*;
use quarry_core::JoinHeader as HeaderInfo;
#[cfg(test)]
use quarry_core::analyze_join_with_output;
use quarry_core::{
    JoinCase, JoinConfig, JoinFormat, JoinInput, JoinJob, JoinMode, JoinOutputColumn,
    JoinOutputOptions, JoinPlan, JoinSource, JoinSummary, JoinWorkingSource,
    analyze_join_with_working, read_join_header_with_format, read_join_working_header,
};
use std::thread::{self, JoinHandle};

pub(super) const INPUT_RESERVED: &str =
    "This document is being used by Join Files. Reset or close the join before changing it.";

struct DocumentLease {
    counter: Arc<std::sync::atomic::AtomicUsize>,
    generation: Option<Arc<quarry_core::WorkingCopyStorage>>,
}
impl Drop for DocumentLease {
    fn drop(&mut self) {
        // Release generation ownership before allowing another document action.
        drop(self.generation.take());
        self.counter
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

type CellEditMap = BTreeMap<(u64, usize), Arc<Vec<u8>>>;

/// Weak metadata signatures detect changed drafts without retaining idle input data.
struct DocumentProbe {
    tab_id: u64,
    session: std::sync::Weak<Session>,
    headers: std::sync::Weak<BTreeMap<usize, Arc<String>>>,
    cells: std::sync::Weak<CellEditMap>,
    header_cells: bool,
}
impl DocumentProbe {
    fn capture(tab_id: u64, document: &Document) -> Self {
        Self {
            tab_id,
            session: Arc::downgrade(&document.session),
            headers: Arc::downgrade(&document.header_renames),
            cells: Arc::downgrade(&document.cell_edits),
            header_cells: Self::has_header_cells(document),
        }
    }
    fn has_header_cells(document: &Document) -> bool {
        document.session.dialect.has_header
            && document
                .cell_edits
                .first_key_value()
                .is_some_and(|((row, _), _)| *row == 0)
    }

    fn matches(&self, tab_id: u64, document: &Document) -> bool {
        self.tab_id == tab_id
            && !document.source_changed
            && self.session.ptr_eq(&Arc::downgrade(&document.session))
            && self
                .headers
                .ptr_eq(&Arc::downgrade(&document.header_renames))
            && (self.cells.ptr_eq(&Arc::downgrade(&document.cell_edits))
                || !self.header_cells && !Self::has_header_cells(document))
    }
}

struct CapturedDocument {
    path: PathBuf,
    source: JoinWorkingSource,
    probe: DocumentProbe,
    columns: usize,
}

struct OpenDocumentChoice {
    tab_id: u64,
    label: String,
    path: PathBuf,
}

impl Document {
    pub(super) fn join_reserved(&self) -> bool {
        self.join_reservations
            .load(std::sync::atomic::Ordering::Acquire)
            != 0
    }

    fn join_capture_disabled_reason(&self) -> Option<&'static str> {
        if self.join_reserved() {
            return Some(INPUT_RESERVED);
        }
        if self.filter_active() || self.filter_refresh_pending {
            return Some("Clear this document's filters before using it in a join.");
        }
        if self.source_changed {
            return Some("Reopen this changed document before using it in a join.");
        }
        if self.job.is_some()
            || self.pending_materialization.is_some()
            || self.save_job.is_some()
            || self.export_job.is_some()
            || self.structural_job.is_some()
            || self.search_job.is_some()
            || self.filter_job.is_some()
        {
            return Some("Wait for this document's active operation to finish before joining.");
        }
        None
    }

    fn commit_join_drafts(&mut self) -> Result<(), String> {
        if let Some(reason) = self.join_capture_disabled_reason() {
            return Err(reason.into());
        }
        let limit = JoinConfig::default().max_record_bytes;
        if self
            .header_edit
            .as_ref()
            .is_some_and(|edit| edit.draft.len() > limit)
            || self
                .cell_edit
                .as_ref()
                .is_some_and(|edit| edit.draft.len() > limit)
        {
            return Err(
                "An editor draft exceeds the record limit. Shorten it before joining.".into(),
            );
        }
        if self
            .header_edit
            .as_ref()
            .is_some_and(|edit| !self.header_is_editable(edit.column))
        {
            return Err("Finish or cancel the invalid header draft before joining.".into());
        }
        if let Some(edit) = &self.cell_edit
            && self.cell_edit_disabled_reason(Some(&edit.source)).is_some()
        {
            return Err("Finish or cancel the invalid cell draft before joining.".into());
        }
        self.commit_edits();
        Ok(())
    }

    fn capture_join_document(&mut self, tab_id: u64) -> Result<CapturedDocument, String> {
        self.commit_join_drafts()?;
        if self.has_structural_edits() && self.original_session.is_none() {
            return Err("The original source cannot be identified. Save a copy and reopen it before joining.".into());
        }
        let generation =
            if self.has_structural_edits() {
                let name = self
                    .session
                    .path()
                    .file_name()
                    .ok_or("Missing private generation name.")?;
                Some(self.working_copy.as_ref()
                .and_then(|state| std::iter::once(&state.directory)
                    .chain(state.previous_directories.iter())
                    .find(|directory| directory.has_generation_name(name)))
                .cloned()
                .ok_or("The current working generation is no longer owned. Reopen this document.")?)
            } else {
                None
            };
        let probe = DocumentProbe::capture(tab_id, self);
        self.join_reservations
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let owner = Arc::new(DocumentLease {
            counter: Arc::clone(&self.join_reservations),
            generation,
        });
        let source = JoinWorkingSource::capture(
            Arc::clone(&self.session),
            self.original_session.as_ref().unwrap_or(&self.session),
            Arc::clone(&self.header_renames),
            Arc::clone(&self.cell_edits),
            owner,
        );
        Ok(CapturedDocument {
            path: self.logical_path.clone(),
            source,
            probe,
            columns: self.total_columns,
        })
    }
}

struct OutputColumnDraft {
    source: JoinSource,
    column: usize,
    included: bool,
    name: String,
}

/// Modeless join draft; source and destination reservations live with this state.
pub(super) struct JoinDialog {
    paths: [Option<PathBuf>; 2],
    document_inputs: [Option<DocumentProbe>; 2],
    headers: [Option<HeaderInfo>; 2],
    formats: [JoinFormat; 2],
    keys: Vec<[usize; 2]>,
    ignore_ascii_case: bool,
    output_columns: Vec<OutputColumnDraft>,
    output_selected: Option<usize>,
    output_has_header: bool,
    require_unique: [bool; 2],
    acknowledge_expansion: bool,
    acknowledge_layout: bool,
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
    SelectDocument(usize, u64),
    ReadFormat(usize),
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
            document_inputs: [None, None],
            headers: [None, None],
            formats: [JoinFormat::default(); 2],
            keys: vec![[0, 0]],
            ignore_ascii_case: false,
            output_columns: Vec::new(),
            output_selected: None,
            output_has_header: true,
            require_unique: [false, false],
            acknowledge_expansion: false,
            acknowledge_layout: false,
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

    fn inputs_editable(&self) -> bool {
        !self.busy() && !self.closing && self.plan.is_none() && self.result.is_none()
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
        self.acknowledge_layout = false;
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
                        self.refresh_output_columns();
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
            let job = self.checking.take().unwrap();
            if !self.closing && !self.cancel_requested {
                if let Some(headers) = job.take_discovered_headers() {
                    self.adopt_discovered_headers(headers);
                }
            }
            match job.wait() {
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
        self.document_inputs[side] = None;
        self.paths[side] = Some(path.clone());
        self.headers[side] = None;
        self.refresh_output_columns();
        self.message = None;
        self.result = None;
        self.destination = None;
        self.cancel_requested = false;
        match read_join_header_with_format(path, self.formats[side]) {
            Ok(job) => self.header_job = Some((side, job)),
            Err(error) => self.message = Some(error.to_string()),
        }
    }

    fn read_document_header(&mut self, side: usize, captured: CapturedDocument) {
        let CapturedDocument {
            path,
            source,
            probe,
            columns,
        } = captured;
        self.paths[side] = Some(path);
        self.formats[side] = source.format();
        self.document_inputs[side] = Some(probe);
        self.headers[side] = None;
        self.refresh_output_columns();
        self.message = None;
        self.result = None;
        self.destination = None;
        self.cancel_requested = false;
        match read_join_working_header(source, columns) {
            Ok(job) => self.header_job = Some((side, job)),
            Err(error) => self.message = Some(error.to_string()),
        }
    }

    /// Capture explicit saved-file settings for the full analysis worker.
    fn start_analysis(&mut self, working: [Option<JoinWorkingSource>; 2]) {
        self.acknowledge_expansion = false;
        self.acknowledge_layout = false;
        let inputs = std::array::from_fn(|side| JoinInput {
            path: self.paths[side].as_ref().unwrap().clone(),
            delimiter: Some(self.headers[side].as_ref().unwrap().delimiter),
            has_header: self.formats[side].has_header,
            encoding: Some(self.headers[side].as_ref().unwrap().encoding),
            key_columns: self.keys.iter().map(|pair| pair[side]).collect(),
            expected_version: Some(self.headers[side].as_ref().unwrap().source_version.clone()),
            require_unique: self.require_unique[side],
        });
        self.message = None;
        self.result = None;
        self.cancel_requested = false;
        let output = JoinOutputOptions {
            known_widths: Some(std::array::from_fn(|side| {
                self.headers[side].as_ref().unwrap().columns.len()
            })),
            columns: Some(
                self.output_columns
                    .iter()
                    .filter(|column| column.included)
                    .map(|column| JoinOutputColumn {
                        source: column.source,
                        column: column.column,
                        name: if column.name.is_empty() {
                            None
                        } else {
                            Some(column.name.clone())
                        },
                    })
                    .collect(),
            ),
            include_header: Some(self.output_has_header),
        };
        match analyze_join_with_working(
            inputs,
            working,
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
            output,
        ) {
            Ok(job) => self.checking = Some(job),
            Err(error) => self.message = Some(error.to_string()),
        }
    }

    /// Consume the retained plan only after application-level guards pass.
    fn start_write(&mut self) {
        if self
            .plan
            .as_ref()
            .is_some_and(|plan| plan.analysis().new_columns != [0, 0])
            && !self.acknowledge_layout
        {
            self.message = Some("Review the newly discovered output columns first.".into());
            return;
        }
        let Some(plan) = self.plan.take() else {
            return;
        };
        let Some(destination) = self.destination.clone() else {
            self.plan = Some(plan);
            return;
        };
        self.message = None;
        self.cancel_requested = false;
        match plan.start_write_reviewed(
            destination,
            self.acknowledge_expansion,
            self.acknowledge_layout,
        ) {
            Ok(job) => self.writing = Some(job),
            Err(error) => self.message = Some(error.to_string()),
        }
    }

    /// Replacing either input invalidates the old projection identities.
    fn refresh_output_columns(&mut self) {
        self.output_columns = [JoinSource::Main, JoinSource::Additional]
            .into_iter()
            .flat_map(|source| {
                (0..self.headers[source.index()]
                    .as_ref()
                    .map_or(0, |h| h.columns.len()))
                    .map(move |column| OutputColumnDraft {
                        source,
                        column,
                        included: true,
                        name: String::new(),
                    })
            })
            .collect();
        self.output_selected = (!self.output_columns.is_empty()).then_some(0);
        self.output_has_header = self
            .headers
            .iter()
            .flatten()
            .any(|header| header.has_header);
    }

    /// Merge late columns by original identity, preserving the user's chosen order.
    fn adopt_discovered_headers(&mut self, headers: [HeaderInfo; 2]) {
        for (side, header) in headers.into_iter().enumerate() {
            let previous = self.headers[side].as_ref().map_or(0, |h| h.columns.len());
            let source = if side == 0 {
                JoinSource::Main
            } else {
                JoinSource::Additional
            };
            for column in previous..header.columns.len() {
                self.output_columns.push(OutputColumnDraft {
                    source,
                    column,
                    included: true,
                    name: String::new(),
                });
            }
            self.headers[side] = Some(header);
        }
    }

    /// Format changes use the same asynchronous, version-bound metadata path.
    fn show_input_format(&mut self, ui: &mut egui::Ui, side: usize) -> bool {
        let role = if side == 0 { "Main" } else { "Additional" };
        let format = &mut self.formats[side];
        let previous = *format;
        ui.push_id(("join-format", side), |ui| {
            ui.checkbox(&mut format.has_header, format!("{role} has a header row"));
            let selected = DelimiterMode::ALL
                .into_iter()
                .find(|mode| mode.delimiter() == format.delimiter)
                .unwrap_or(DelimiterMode::Auto);
            egui::ComboBox::from_id_salt("delimiter")
                .selected_text(format!("{role} delimiter: {}", selected.label()))
                .show_ui(ui, |ui| {
                    for mode in DelimiterMode::ALL {
                        ui.selectable_value(&mut format.delimiter, mode.delimiter(), mode.label());
                    }
                });
            egui::ComboBox::from_id_salt("encoding")
                .selected_text(format!(
                    "{role} encoding: {}",
                    encoding_choice_label(format.encoding)
                ))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut format.encoding, None, "Auto (BOM or UTF-8)");
                    for encoding in Encoding::ALL {
                        ui.selectable_value(&mut format.encoding, Some(encoding), encoding.label());
                    }
                });
        });
        (previous.delimiter, previous.encoding, previous.has_header)
            != (format.delimiter, format.encoding, format.has_header)
    }

    fn show_output_columns(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        ui.strong("Output columns");
        ui.checkbox(
            &mut self.output_has_header,
            "Include a header row in the output",
        );
        ui.horizontal_wrapped(|ui| {
            for source in [JoinSource::Main, JoinSource::Additional] {
                for include in [true, false] {
                    let label = format!(
                        "Select {} {} columns",
                        if include { "all" } else { "no" },
                        source.label()
                    );
                    if ui.button(label).clicked() {
                        for column in &mut self.output_columns {
                            if column.source == source {
                                column.included = include;
                            }
                        }
                    }
                }
            }
        });
        let count = self.output_columns.len();
        let selected = self.output_selected;
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(
                    selected.is_some_and(|i| i > 0),
                    egui::Button::new("Move output column up"),
                )
                .clicked()
            {
                let i = selected.unwrap();
                self.output_columns.swap(i, i - 1);
                self.output_selected = Some(i - 1);
            }
            if ui
                .add_enabled(
                    selected.is_some_and(|i| i + 1 < count),
                    egui::Button::new("Move output column down"),
                )
                .clicked()
            {
                let i = selected.unwrap();
                self.output_columns.swap(i, i + 1);
                self.output_selected = Some(i + 1);
            }
        });
        let headers = &self.headers;
        let row_height = ui.spacing().interact_size.y.max(
            ui.text_style_height(&egui::TextStyle::Button) + 2.0 * ui.spacing().button_padding.y,
        );
        let list_height = ((row_height + ui.spacing().item_spacing.y) * count as f32).min(180.0);
        egui::ScrollArea::vertical()
            .id_salt("join-output-columns")
            .max_height(180.0)
            .min_scrolled_height(list_height)
            .show_rows(ui, row_height, count, |ui, range| {
                for i in range {
                    let column = &mut self.output_columns[i];
                    let role = column.source.label();
                    let original =
                        &headers[column.source.index()].as_ref().unwrap().columns[column.column];
                    let original = if original.is_empty() {
                        format!("Column {}", column.column + 1)
                    } else {
                        original.clone()
                    };
                    ui.horizontal(|ui| {
                        let included = ui.checkbox(
                            &mut column.included,
                            format!("Include {role} {}", column.column + 1),
                        );
                        if included.has_focus() || included.clicked() {
                            self.output_selected = Some(i);
                        }
                        if ui
                            .add(
                                egui::Button::selectable(
                                    self.output_selected == Some(i),
                                    format!(
                                        "Output from {role} column {}: {original}",
                                        column.column + 1
                                    ),
                                )
                                .truncate(),
                            )
                            .clicked()
                        {
                            self.output_selected = Some(i);
                        }
                    });
                }
            });
        if let Some(i) = self.output_selected.filter(|&i| i < count) {
            let column = &mut self.output_columns[i];
            ui.label(format!(
                "New heading for {} column {} (optional)",
                column.source.label(),
                column.column + 1
            ));
            ui.add(
                egui::TextEdit::singleline(&mut column.name)
                    .id_salt("join-output-heading")
                    .hint_text("Keep original heading")
                    .desired_width(280.0),
            );
        }
        let selected = self
            .output_columns
            .iter()
            .filter(|column| column.included)
            .count();
        ui.small(format!("{selected} output columns selected"));
        if selected == 0 && self.headers.iter().all(Option::is_some) {
            ui.label(RichText::new("Select at least one output column.").color(ERROR_TEXT));
        }
        if self.output_has_header {
            let mut seen = std::collections::HashSet::new();
            if self
                .output_columns
                .iter()
                .filter(|c| c.included && !c.name.is_empty())
                .any(|c| !seen.insert(c.name.as_str()))
            {
                ui.label("Some renamed headings are identical. The output will keep those names.");
            }
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
                                .map(|name| {
                                    format!(
                                        "{role} {}: {}",
                                        pair[side] + 1,
                                        if name.is_empty() {
                                            format!("Column {}", pair[side] + 1)
                                        } else {
                                            name.clone()
                                        }
                                    )
                                })
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
    fn show(
        &mut self,
        ctx: &egui::Context,
        can_start: bool,
        documents: &[OpenDocumentChoice],
    ) -> DialogAction {
        self.poll();
        if self.closing && !self.busy() && self.plan.is_none() {
            return DialogAction::Closed(self.quit_after_close);
        }
        let busy = self.busy();
        let editable = self.inputs_editable();
        let mut action = DialogAction::None;
        let mut open = true;
        egui::Window::new("Join Files").id(egui::Id::new("join-files"))
            .open(&mut open).default_width(680.0).min_width(480.0)
            .show(ctx,|ui|{
                ui.label("Match key columns and create a separate joined file.");
                ui.small("Use current open documents or saved files. Retain every matching pair. Empty keys never match.");
                ui.small("Keys match whole values. Leading zeros, whitespace and non-ASCII case remain significant. Joins use all current rows; clear source filters first.");
                egui::ScrollArea::vertical().max_height((ui.available_height()-70.0).min(ctx.content_rect().height()-180.0).max(40.0)).show(ui,|ui|{
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
                            column.add_enabled_ui(editable && can_start, |ui| {
                                let role = if side == 0 { "Main" } else { "Additional" };
                                egui::ComboBox::from_id_salt(("join-open-document", side))
                                    .selected_text(format!("Open {role} document…"))
                                    .show_ui(ui, |ui| {
                                        ui.set_max_width(320.0);
                                        if documents.is_empty() { ui.label("No open documents"); }
                                        for choice in documents {
                                            if ui.selectable_label(false, format!("Use {role} document: {}", choice.label))
                                                .on_hover_text(choice.path.display().to_string()).clicked() {
                                                action = DialogAction::SelectDocument(side, choice.tab_id);
                                            }
                                        }
                                    });
                            });
                            if self.document_inputs[side].is_some() {
                                column.small("Open document · current values · inherited format");
                            } else { column.small("Saved file"); }
                            column.add_enabled_ui(editable && can_start,|ui|{
                                ui.add_enabled_ui(self.document_inputs[side].is_none(), |ui| {
                                    if self.show_input_format(ui, side) { action = DialogAction::ReadFormat(side); }
                                });
                                ui.checkbox(&mut self.require_unique[side], if side == 0 { "Require unique Main key" } else { "Require unique Additional key" });
                                if let Some(header)=&self.headers[side]{
                                    ui.small(format!("{} columns · {} · {}",header.columns.len(),header.encoding.label(),if header.has_header {"header row"} else {"no header"}));
                                }else{ui.label("Key column: choose a file first");}
                            });
                        }
                    });
                    ui.add_enabled_ui(editable, |ui| self.show_key_pairs(ui));
                    ui.add_enabled_ui(editable, |ui| self.show_output_columns(ui));
                    if let Some(plan)=&self.plan{
                        let a=plan.analysis();
                        ui.separator();
                        ui.label(format!("Main: {} rows · Additional: {} rows",a.input_rows[0],a.input_rows[1]));
                        ui.label(format!("Matching pairs: {} · Result: {} rows",a.matching_pairs,a.output_rows));
                        ui.label(format!("Unmatched: {} Main, {} Additional",a.unmatched_rows[0],a.unmatched_rows[1]));
                        ui.label(format!("Empty keys: {} Main, {} Additional",a.empty_key_rows[0],a.empty_key_rows[1]));
                        ui.label(format!("Duplicate-key groups: {} Main, {} Additional",a.duplicate_groups[0],a.duplicate_groups[1]));
                        if a.new_columns != [0, 0] {
                            ui.label(format!("Full scan found {} new Main columns and {} new Additional columns. They are included in the output.", a.new_columns[0], a.new_columns[1]));
                            ui.small("Review the output columns above. Use Reset Analysis to change their selection, order or headings.");
                            ui.checkbox(&mut self.acknowledge_layout, "I reviewed the newly discovered output columns.");
                        }
                        if a.many_to_many_groups > 0 {
                            ui.label(RichText::new(format!("{} many-to-many groups produce {} matching pairs.",a.many_to_many_groups,a.many_to_many_pairs)).strong());
                            ui.checkbox(&mut self.acknowledge_expansion,format!("I understand this will create {} rows.",a.output_rows));
                        }
                        for sample in &a.duplicate_samples {
                            ui.small(format!("Key {}: {} Main × {} Additional",sample.key,sample.main_rows,sample.additional_rows));
                        }
                        ui.small(format!("Output: {} · Conservative additional storage: {}",format_bytes(a.output_bytes),format_bytes(a.additional_temporary_bytes)));
                        ui.small("Sample in output order; at most 20 rows and columns. Long values are clipped.");
                        if a.duplicate_headers && a.include_header { ui.label("The output contains duplicate headings."); }
                        egui::ScrollArea::both().max_height(150.0).show(ui,|ui|{
                            egui::Grid::new("join-preview").striped(true).show(ui,|ui|{
                                for name in a.output_headers.iter().take(20) { ui.strong(name.chars().take(64).collect::<String>()); }
                                ui.end_row();
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
                        if ui.add_enabled(can_start && self.destination.is_some() && self.plan.as_ref().is_none_or(|plan| (plan.analysis().many_to_many_groups == 0 || self.acknowledge_expansion) && (plan.analysis().new_columns == [0, 0] || self.acknowledge_layout)),egui::Button::new("Create Joined File")).clicked(){action=DialogAction::Create;}
                    }else if ui.add_enabled(can_start && self.headers.iter().all(Option::is_some) && self.output_columns.iter().any(|column|column.included),egui::Button::new("Analyze & Preview")).clicked(){
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

fn private_working_path(document: &Document, path: &Path) -> bool {
    document.working_copy.as_ref().is_some_and(|state| {
        state.owns(path)
            || std::iter::once(&state.directory)
                .chain(state.previous_directories.iter())
                .any(|directory| {
                    output_is_inside_working_directory(path, directory.path())
                        || directory
                            .current_path()
                            .is_ok_and(|current| output_is_inside_working_directory(path, &current))
                })
            || state
                .undo
                .iter()
                .chain(state.redo.iter())
                .any(|snapshot| tabs::paths_alias(path, &snapshot.path))
    })
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

    fn join_document_for_path(&self, path: &Path) -> Option<u64> {
        self.document_tabs()
            .find(|tab| {
                tab.document.as_ref().is_some_and(|doc| {
                    tabs::paths_alias(path, &doc.logical_path)
                        || tabs::paths_alias(path, doc.session.path())
                })
            })
            .map(|tab| tab.id)
    }

    fn join_input_writer_busy(&self, path: &Path) -> bool {
        self.cleanup_reserves_path(path)
            || self.document_tabs().any(|tab| {
                tab.write_destination
                    .as_ref()
                    .is_some_and(|destination| tabs::paths_alias(path, destination))
                    || tab
                        .document
                        .as_ref()
                        .and_then(Document::reserved_working_destination)
                        .is_some_and(|destination| tabs::paths_alias(path, destination))
            })
            || self
                .combine_files
                .as_ref()
                .and_then(combine_files::CombineDialog::reserved_output)
                .is_some_and(|destination| tabs::paths_alias(path, destination))
    }

    fn join_tab_for_capture(&mut self, id: u64) -> Result<&mut DocumentTab, String> {
        let tab = std::iter::once(&mut self.active)
            .chain(self.tabs.iter_mut())
            .find(|tab| tab.id == id && tab.document.is_some())
            .ok_or("This document is no longer open. Choose the input again.")?;
        if tab.close_confirmation_open
            || tab.close_ready
            || tab.close_after_jobs
            || tab.close_after_save
            || tab.storage_review.is_some()
            || tab.inspection.recovering()
        {
            return Err(
                "Finish this document's pending dialog or close request before joining.".into(),
            );
        }
        Ok(tab)
    }

    fn select_join_document(&mut self, side: usize, id: u64) -> Result<(), String> {
        if !self.join_files.as_ref().unwrap().inputs_editable() {
            return Err("Reset the retained analysis before changing inputs.".into());
        }
        if !self.write_slot_available() || self.join_files.as_ref().is_some_and(JoinDialog::busy) {
            return Err("Wait for an operation slot before reading this input.".into());
        }
        if self.join_files.as_ref().unwrap().document_inputs[1 - side]
            .as_ref()
            .is_some_and(|probe| probe.tab_id == id)
        {
            return Err("Choose two different inputs. Self joins are not supported.".into());
        }
        let captured = self
            .join_tab_for_capture(id)?
            .document
            .as_mut()
            .unwrap()
            .capture_join_document(id)?;
        if self.join_files.as_ref().unwrap().paths[1 - side]
            .as_ref()
            .is_some_and(|path| tabs::paths_alias(path, &captured.path))
        {
            return Err("Choose two different inputs. Self joins are not supported.".into());
        }
        self.join_files
            .as_mut()
            .unwrap()
            .read_document_header(side, captured);
        Ok(())
    }

    pub(super) fn select_join_input_path(
        &mut self,
        side: usize,
        path: PathBuf,
    ) -> Result<(), String> {
        if !self.join_files.as_ref().unwrap().inputs_editable() {
            return Err("Reset the retained analysis before changing inputs.".into());
        }
        if self.join_input_writer_busy(&path) {
            return Err("Wait for the writer using this input to finish.".into());
        }
        if let Some(id) = self.join_document_for_path(&path) {
            self.select_join_document(side, id)
        } else if self.document_tabs().any(|tab| {
            tab.document
                .as_ref()
                .is_some_and(|document| private_working_path(document, &path))
        }) {
            Err("This is a private history file. Select the current document from the open-document menu instead.".into())
        } else if self.write_slot_available() && !self.join_files.as_ref().unwrap().busy() {
            self.join_files.as_mut().unwrap().read_header(side, path);
            Ok(())
        } else {
            Err("Wait for an operation slot before reading this input.".into())
        }
    }

    /// Bind current values to the schema shown by the picker, then acquire leases.
    fn analyze_join_draft(&mut self) -> Result<(), String> {
        if !self.join_files.as_ref().unwrap().inputs_editable() {
            return Err("Reset the retained analysis before analyzing again.".into());
        }
        if !self.write_slot_available() || self.join_files.as_ref().unwrap().busy() {
            return Err("Wait for an operation slot before analyzing.".into());
        }
        let paths: [PathBuf; 2] = std::array::from_fn(|side| {
            self.join_files.as_ref().unwrap().paths[side]
                .clone()
                .unwrap()
        });
        let ids = paths
            .each_ref()
            .map(|path| self.join_document_for_path(path));
        if ids[0].is_some() && ids[0] == ids[1] {
            return Err("Choose two different inputs. Self joins are not supported.".into());
        }
        for side in 0..2 {
            if self.join_input_writer_busy(&paths[side]) {
                return Err("Wait for the writer using a selected input to finish.".into());
            }
            if let Some(id) = ids[side] {
                self.join_tab_for_capture(id)?
                    .document
                    .as_mut()
                    .unwrap()
                    .commit_join_drafts()?;
                let unchanged = self.join_files.as_ref().unwrap().document_inputs[side]
                    .as_ref()
                    .is_some_and(|probe| {
                        self.document_tabs().any(|tab| {
                            tab.id == id
                                && tab
                                    .document
                                    .as_ref()
                                    .is_some_and(|doc| probe.matches(id, doc))
                        })
                    });
                if !unchanged {
                    self.select_join_document(side, id)?;
                    return Err("An input's current values or format changed. Review its refreshed columns and analyze again.".into());
                }
            } else if self.join_files.as_ref().unwrap().document_inputs[side].is_some() {
                self.join_files
                    .as_mut()
                    .unwrap()
                    .read_header(side, paths[side].clone());
                return Err("An input document was closed. Review the saved file's columns and analyze again.".into());
            }
        }
        let mut working = [None, None];
        for side in 0..2 {
            if let Some(id) = ids[side] {
                let captured = self
                    .join_tab_for_capture(id)?
                    .document
                    .as_mut()
                    .unwrap()
                    .capture_join_document(id)?;
                working[side] = Some(captured.source);
            }
        }
        if !self.write_slot_available() {
            return Err("Wait for an operation slot before analyzing.".into());
        }
        self.join_files.as_mut().unwrap().start_analysis(working);
        Ok(())
    }

    /// Recheck all live document and writer paths before starting output.
    pub(super) fn join_destination_is_available(&self, path: &Path) -> bool {
        // Keep the retained analysis when a picker chooses an existing output.
        // symlink_metadata also rejects dangling links; only absence is writable.
        if !matches!(std::fs::symlink_metadata(path), Err(error)
            if error.kind() == std::io::ErrorKind::NotFound)
        {
            return false;
        }
        !self.cleanup_reserves_path(path)
            && !self.document_tabs().any(|tab| {
                tab.document.as_ref().is_some_and(|doc| {
                    tabs::paths_alias(path, &doc.logical_path)
                        || tabs::paths_alias(path, doc.session.path())
                        || private_working_path(doc, path)
                        || doc
                            .reserved_working_destination()
                            .is_some_and(|destination| tabs::paths_alias(path, destination))
                }) || tab
                    .write_destination
                    .as_ref()
                    .is_some_and(|destination| tabs::paths_alias(path, destination))
            })
            && !self
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
        let documents: Vec<_> = self
            .document_tabs()
            .filter_map(|tab| {
                let document = tab.document.as_ref()?;
                let path = document.logical_path.clone();
                let name = path
                    .file_name()
                    .unwrap_or(path.as_os_str())
                    .to_string_lossy();
                let folder = path
                    .parent()
                    .and_then(Path::file_name)
                    .map(|name| name.to_string_lossy());
                Some(OpenDocumentChoice {
                    tab_id: tab.id,
                    label: format!(
                        "{}{}{}",
                        name,
                        if document.is_dirty() { " ●" } else { "" },
                        folder.map_or(String::new(), |folder| format!(" — {folder}"))
                    ),
                    path,
                })
            })
            .collect();
        let Some(dialog) = self.join_files.as_mut() else {
            return;
        };
        let action = dialog.show(ctx, can_start, &documents);
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
                    .set_title("Choose a delimited file")
                    .pick_file()
                    && let Err(error) = self.select_join_input_path(side, path)
                {
                    self.join_files.as_mut().unwrap().message = Some(error);
                }
            }
            DialogAction::SelectDocument(side, id) => {
                if let Err(error) = self.select_join_document(side, id) {
                    self.join_files.as_mut().unwrap().message = Some(error);
                }
            }
            DialogAction::ReadFormat(side) => {
                if let Some(path) = self.join_files.as_ref().unwrap().paths[side].clone()
                    && let Err(error) = self.select_join_input_path(side, path)
                {
                    self.join_files.as_mut().unwrap().message = Some(error);
                }
            }
            DialogAction::Analyze => {
                if let Err(error) = self.analyze_join_draft() {
                    self.join_files.as_mut().unwrap().message = Some(error);
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
                        header_mode: if dialog.result.as_ref().unwrap().include_header {
                            HeaderMode::FirstRow
                        } else {
                            HeaderMode::NoHeader
                        },
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
    /// Focus a real control and activate it with the keyboard.
    fn keyboard_activate(app: &mut QuarryApp, ctx: &egui::Context, label: &str) {
        frame(app, ctx, vec![]);
        let output = frame(app, ctx, vec![]);
        let tree = output.platform_output.accesskit_update.unwrap();
        let (id, _) = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some(label) || node.value() == Some(label))
            .unwrap_or_else(|| panic!("missing keyboard control {label}"));
        frame(
            app,
            ctx,
            vec![egui::Event::AccessKitActionRequest(
                egui::accesskit::ActionRequest {
                    action: egui::accesskit::Action::Focus,
                    target: *id,
                    data: None,
                },
            )],
        );
        for pressed in [true, false] {
            frame(
                app,
                ctx,
                vec![egui::Event::Key {
                    key: egui::Key::Space,
                    physical_key: None,
                    pressed,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
            );
        }
    }

    /// Enter a heading through the displayed text editor.
    fn type_heading(app: &mut QuarryApp, ctx: &egui::Context, value: &str) {
        frame(app, ctx, vec![]);
        let output = frame(app, ctx, vec![]);
        let tree = output.platform_output.accesskit_update.unwrap();
        let (id, _) = tree
            .nodes
            .iter()
            .find(|(_, node)| node.role() == egui::accesskit::Role::TextInput)
            .expect("heading text editor");
        frame(
            app,
            ctx,
            vec![egui::Event::AccessKitActionRequest(
                egui::accesskit::ActionRequest {
                    action: egui::accesskit::Action::Focus,
                    target: *id,
                    data: None,
                },
            )],
        );
        frame(app, ctx, vec![egui::Event::Text(value.into())]);
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
    fn finish_document(app: &mut QuarryApp) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while app
            .active
            .document
            .as_ref()
            .unwrap()
            .structural_job
            .is_some()
        {
            let ready = app
                .active
                .document
                .as_mut()
                .unwrap()
                .poll_structural_edit()
                .unwrap();
            if let Some(ready) = ready {
                app.install_materialized_working_copy(ready).unwrap();
            }
            assert!(Instant::now() < deadline, "document operation stalled");
            std::thread::yield_now();
        }
        let document = app.active.document.as_mut().unwrap();
        if let Some(job) = document.job.take() {
            document.index = Some(job.wait().unwrap());
            document.progress.done = true;
            document.progress.bytes_scanned = document.session.file_size;
        }
    }

    fn choose_open_document(app: &mut QuarryApp, ctx: &egui::Context, side: usize, filename: &str) {
        let role = if side == 0 { "Main" } else { "Additional" };
        click(app, ctx, &format!("Open {role} document…"));
        let output = frame(app, ctx, vec![]);
        let tree = output.platform_output.accesskit_update.unwrap();
        let prefix = format!("Use {role} document: {filename}");
        let (id, _) = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label().is_some_and(|label| label.starts_with(&prefix)))
            .unwrap_or_else(|| panic!("missing open-document choice {prefix}"));
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
        finish(app);
        assert!(
            app.join_files.as_ref().unwrap().headers[side].is_some(),
            "{:?}",
            app.join_files.as_ref().unwrap().message
        );
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
        finish_document(&mut app);
        click(&mut app, &ctx, "Analyze & Preview");
        assert!(app.join_files.as_ref().unwrap().checking.is_none());
        assert!(
            app.join_files
                .as_ref()
                .unwrap()
                .message
                .as_ref()
                .unwrap()
                .contains("Review its refreshed columns")
        );
        finish(&mut app);
        assert!(app.join_files.as_ref().unwrap().document_inputs[0].is_some());
        assert!(!app.active.document.as_ref().unwrap().join_reserved());
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
        app.join_files
            .as_mut()
            .unwrap()
            .start_analysis([None, None]);
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
    fn footer_actions_remain_visible_when_pair_list_scrolls() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = QuarryApp::new(None, Instant::now());
        let ctx = context();
        prepare(&mut app, dir.path());
        app.join_files.as_mut().unwrap().keys = vec![[0, 0], [1, 1]];
        app.join_files
            .as_mut()
            .unwrap()
            .start_analysis([None, None]);
        finish(&mut app);
        let mut output = None;
        for _ in 0..3 {
            output = Some(ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(900.0, 450.0),
                    )),
                    ..Default::default()
                },
                |ctx| app.show_join_files(ctx),
            ));
        }
        let output = output.unwrap();
        let labels = output
            .shapes
            .iter()
            .filter(|clipped| {
                matches!(&clipped.shape,
            egui::Shape::Text(text) if text.galley.job.text=="Reset Analysis")
            })
            .collect::<Vec<_>>();
        assert!(!labels.is_empty(), "the footer must be painted");
        assert!(
            labels.iter().all(|shape| shape
                .clip_rect
                .contains_rect(shape.shape.visual_bounding_rect())),
            "footer actions must fit inside their clipping rectangle"
        );
    }
    #[test]
    fn output_controls_project_reorder_rename_and_open_headerless_data() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = QuarryApp::new(None, Instant::now());
        let ctx = context();
        let (a, b) = prepare(&mut app, dir.path());
        assert_eq!(app.join_files.as_ref().unwrap().output_columns.len(), 4);
        assert!(
            app.join_files
                .as_ref()
                .unwrap()
                .output_columns
                .iter()
                .all(|c| c.included)
        );
        keyboard_activate(&mut app, &ctx, "Include Main 1");
        keyboard_activate(&mut app, &ctx, "Include Additional 1");
        keyboard_activate(&mut app, &ctx, "Output from Additional column 2: city");
        for _ in 0..3 {
            keyboard_activate(&mut app, &ctx, "Move output column up");
        }
        type_heading(&mut app, &ctx, "Destination");
        let draft = app.join_files.as_ref().unwrap();
        assert_eq!(draft.output_columns[0].source, JoinSource::Additional);
        assert_eq!(draft.output_columns[0].column, 1);
        assert_eq!(draft.output_columns[0].name, "Destination");
        keyboard_activate(&mut app, &ctx, "Include a header row in the output");
        click(&mut app, &ctx, "Analyze & Preview");
        finish(&mut app);
        let plan = app
            .join_files
            .as_ref()
            .unwrap()
            .plan
            .as_ref()
            .expect("projection plan");
        assert_eq!(
            plan.analysis().output_headers.as_ref(),
            ["Destination", "name"]
        );
        assert!(!plan.analysis().include_header);
        assert_eq!(plan.analysis().preview[0], ["LA", "Bob"]);
        let output = dir.path().join("projected.csv");
        app.join_files.as_mut().unwrap().destination = Some(output.clone());
        click(&mut app, &ctx, "Create Joined File");
        finish(&mut app);
        assert_eq!(
            fs::read(&output).unwrap(),
            b"LA,Bob
Seattle,Alice
,Last
"
        );
        click(&mut app, &ctx, "Open Joined File");
        assert!(app.join_files.is_none());
        let doc = app.active.document.as_ref().unwrap();
        assert_eq!(doc.logical_path, output);
        assert_eq!(
            doc.data_start, 0,
            "first data row must not become the header"
        );
        assert_eq!(
            doc.session.first_rows[0].fields,
            [b"LA".to_vec(), b"Bob".to_vec()]
        );
        assert!(!doc.is_dirty());
        assert_eq!(
            fs::read(a).unwrap(),
            b"id,name
b,Bob
a,Alice
z,Last
"
        );
        assert_eq!(
            fs::read(b).unwrap(),
            b"id,city
a,Seattle
b,LA
"
        );
    }

    #[test]
    fn selection_buttons_empty_guard_duplicate_warning_and_source_reset() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = QuarryApp::new(None, Instant::now());
        let ctx = context();
        prepare(&mut app, dir.path());
        click(&mut app, &ctx, "Select no Main columns");
        click(&mut app, &ctx, "Select no Additional columns");
        click(&mut app, &ctx, "Analyze & Preview");
        assert!(!app.join_files.as_ref().unwrap().busy());
        assert!(app.join_files.as_ref().unwrap().plan.is_none());
        click(&mut app, &ctx, "Select all Main columns");
        click(&mut app, &ctx, "Select all Additional columns");
        click(&mut app, &ctx, "Output from Main column 2: name");
        type_heading(&mut app, &ctx, "same");
        click(&mut app, &ctx, "Output from Additional column 2: city");
        type_heading(&mut app, &ctx, "same");
        assert_eq!(
            app.join_files.as_ref().unwrap().output_columns[1].name,
            "same"
        );
        assert_eq!(
            app.join_files.as_ref().unwrap().output_columns[3].name,
            "same"
        );
        // Render the output panel fully, as after scrolling down in the dialog.
        let warning_ctx = context();
        let output = warning_ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(900.0, 900.0),
                )),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    app.join_files.as_mut().unwrap().show_output_columns(ui);
                });
            },
        );
        assert!(
            output
                .shapes
                .iter()
                .any(|s| matches!(&s.shape, egui::Shape::Text(t)
            if t.galley.text().contains("Some renamed headings are identical")))
        );
        click(&mut app, &ctx, "Analyze & Preview");
        finish(&mut app);
        let plan = app.join_files.as_ref().unwrap().plan.as_ref().unwrap();
        assert_eq!(
            plan.analysis().output_headers.as_ref(),
            ["id (Main)", "same", "id (Additional)", "same"]
        );
        assert!(plan.analysis().duplicate_headers);
        click(&mut app, &ctx, "Reset Analysis");
        finish(&mut app);
        let replacement = fixture(
            dir.path(),
            "replacement.csv",
            b"only
value
",
        );
        app.join_files.as_mut().unwrap().read_header(0, replacement);
        finish(&mut app);
        let draft = app.join_files.as_ref().unwrap();
        assert_eq!(draft.output_columns.len(), 3);
        assert!(
            draft
                .output_columns
                .iter()
                .all(|c| c.included && c.name.is_empty())
        );
        assert!(draft.output_has_header);
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

    #[test]
    fn format_controls_join_utf16_headerless_and_windows1252_by_composite_keys() {
        let dir = tempfile::tempdir().unwrap();
        let text = "é;EU;Left\né;US;Other\n";
        let mut a = vec![0xfe, 0xff];
        for unit in text.encode_utf16() {
            a.extend_from_slice(&unit.to_be_bytes());
        }
        let b = b"acct\tarea\tcity\n\xe9\tEU\t\x80\n";
        let ap = fixture(dir.path(), "main.csv", &a);
        let bp = fixture(dir.path(), "additional.csv", b);
        let mut app = QuarryApp::new(None, Instant::now());
        let ctx = context();
        app.open_join_files();
        app.join_files.as_mut().unwrap().temporary = dir.path().to_path_buf();
        app.join_files.as_mut().unwrap().read_header(0, ap.clone());
        finish(&mut app);
        click(&mut app, &ctx, "Main has a header row");
        finish(&mut app);
        click(&mut app, &ctx, "Main delimiter: Auto");
        click(&mut app, &ctx, "Semicolon");
        finish(&mut app);
        app.join_files.as_mut().unwrap().read_header(1, bp.clone());
        finish(&mut app);
        assert_eq!(
            app.join_files.as_ref().unwrap().headers[1]
                .as_ref()
                .unwrap()
                .encoding,
            Encoding::Utf8,
            "Auto does not guess a legacy encoding from an ASCII header"
        );
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
                .contains("Invalid UTF-8")
        );
        click(&mut app, &ctx, "Additional encoding: Auto (BOM or UTF-8)");
        click(&mut app, &ctx, "Windows-1252");
        finish(&mut app);
        click(&mut app, &ctx, "Additional delimiter: Auto");
        click(&mut app, &ctx, "Tab");
        finish(&mut app);
        let d = app.join_files.as_ref().unwrap();
        assert_eq!(d.headers[0].as_ref().unwrap().encoding, Encoding::Utf16Be);
        assert_eq!(
            d.headers[1].as_ref().unwrap().encoding,
            Encoding::Windows1252
        );
        assert!(!d.formats[0].has_header);
        assert!(
            d.output_has_header,
            "either source header enables the output header"
        );
        click(&mut app, &ctx, "Add key pair");
        click(&mut app, &ctx, "Analyze & Preview");
        finish(&mut app);
        let p = app.join_files.as_ref().unwrap().plan.as_ref().unwrap();
        assert_eq!(p.analysis().matching_pairs, 1);
        let output = dir.path().join("mixed.csv");
        app.join_files.as_mut().unwrap().destination = Some(output.clone());
        click(&mut app, &ctx, "Create Joined File");
        finish(&mut app);
        assert_eq!(fs::read(&output).unwrap(),
            "Main column 1;Main column 2;Main column 3;acct;area;city\né;EU;Left;é;EU;€\né;US;Other;;;\n".as_bytes());
        click(&mut app, &ctx, "Open Joined File");
        assert_eq!(app.active.document.as_ref().unwrap().data_start, 1);
        assert_eq!(fs::read(ap).unwrap(), a);
        assert_eq!(fs::read(bp).unwrap(), b);
    }

    #[test]
    fn late_columns_require_review_and_can_be_excluded_after_reset() {
        for review_current in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let ap = fixture(dir.path(), "main.csv", b"id\nx,late\n");
            let bp = fixture(dir.path(), "additional.csv", b"id\nx\n");
            let mut app = QuarryApp::new(None, Instant::now());
            let ctx = context();
            app.open_join_files();
            app.join_files.as_mut().unwrap().temporary = dir.path().to_path_buf();
            app.join_files.as_mut().unwrap().read_header(0, ap);
            finish(&mut app);
            app.join_files.as_mut().unwrap().read_header(1, bp);
            finish(&mut app);
            click(&mut app, &ctx, "Analyze & Preview");
            finish(&mut app);
            let d = app.join_files.as_ref().unwrap();
            assert_eq!(d.headers[0].as_ref().unwrap().columns.len(), 2);
            assert_eq!(d.output_columns.len(), 3);
            assert!(d.output_columns[2].included);
            assert_eq!(d.output_columns[2].source, JoinSource::Main);
            assert_eq!(d.output_columns[2].column, 1);
            let output = dir.path().join("late.csv");
            app.join_files.as_mut().unwrap().destination = Some(output.clone());
            click(&mut app, &ctx, "Create Joined File");
            finish(&mut app);
            assert!(!output.exists());
            assert!(app.join_files.as_ref().unwrap().plan.is_some());
            if review_current {
                click(
                    &mut app,
                    &ctx,
                    "I reviewed the newly discovered output columns.",
                );
            } else {
                click(&mut app, &ctx, "Reset Analysis");
                finish(&mut app);
                click(&mut app, &ctx, "Include Main 2");
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
                        .new_columns,
                    [0, 0]
                );
                app.join_files.as_mut().unwrap().destination = Some(output.clone());
            }
            click(&mut app, &ctx, "Create Joined File");
            finish(&mut app);
            let wanted = if review_current {
                b"id (Main),id (Additional),Main column 2\nx,x,late\n".as_slice()
            } else {
                b"id (Main),id (Additional)\nx,x\n".as_slice()
            };
            assert_eq!(fs::read(output).unwrap(), wanted);
        }
    }

    #[test]
    fn failed_layout_analysis_still_reveals_fields_for_a_new_selection() {
        let dir = tempfile::tempdir().unwrap();
        let ap = fixture(dir.path(), "main.csv", b"id\nx,late\n");
        let bp = fixture(dir.path(), "additional.csv", b"id\nx,more\n");
        let mut app = QuarryApp::new(None, Instant::now());
        let ctx = context();
        app.open_join_files();
        app.join_files.as_mut().unwrap().temporary = dir.path().to_path_buf();
        app.join_files.as_mut().unwrap().read_header(0, ap.clone());
        finish(&mut app);
        app.join_files.as_mut().unwrap().read_header(1, bp.clone());
        finish(&mut app);
        let args = std::array::from_fn(|side| JoinInput {
            path: if side == 0 { ap.clone() } else { bp.clone() },
            delimiter: Some(b','),
            has_header: true,
            encoding: Some(Encoding::Utf8),
            key_columns: vec![0],
            expected_version: None,
            require_unique: false,
        });
        // Force the same public limit path with two columns instead of 65,536.
        app.join_files.as_mut().unwrap().checking = Some(
            analyze_join_with_output(
                args,
                dir.path().to_path_buf(),
                JoinConfig {
                    max_columns: 2,
                    ..JoinConfig::default()
                },
                JoinOutputOptions {
                    known_widths: Some([1, 1]),
                    columns: Some(vec![
                        JoinOutputColumn {
                            source: JoinSource::Main,
                            column: 0,
                            name: None,
                        },
                        JoinOutputColumn {
                            source: JoinSource::Additional,
                            column: 0,
                            name: None,
                        },
                    ]),
                    include_header: None,
                },
            )
            .unwrap(),
        );
        finish(&mut app);
        let d = app.join_files.as_ref().unwrap();
        assert!(d.plan.is_none());
        assert!(d.message.as_ref().unwrap().contains("column limit"));
        assert_eq!(d.output_columns.len(), 4);
        click(&mut app, &ctx, "Select no Main columns");
        click(&mut app, &ctx, "Select no Additional columns");
        click(&mut app, &ctx, "Include Additional 2");
        click(&mut app, &ctx, "Analyze & Preview");
        finish(&mut app);
        let output = dir.path().join("recovered.csv");
        app.join_files.as_mut().unwrap().destination = Some(output.clone());
        click(&mut app, &ctx, "Create Joined File");
        finish(&mut app);
        assert_eq!(fs::read(output).unwrap(), b"Additional column 2\nmore\n");
    }
    #[test]
    fn current_document_picker_joins_structural_generations_and_both_unsaved_overlays() {
        let dir = tempfile::tempdir().unwrap();
        let a = fixture(dir.path(), "current-main.csv", b"id,name\nb,Bob\na,Alice\n");
        let b = fixture(
            dir.path(),
            "current-additional.csv",
            b"id,city\nb,LA\na,Seattle\n",
        );
        let original_a = fs::read(&a).unwrap();
        let original_b = fs::read(&b).unwrap();
        let mut app = QuarryApp::new(None, Instant::now());
        app.open_path_with_options(
            a.clone(),
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..Default::default()
            },
        )
        .unwrap();
        finish_document(&mut app);
        let main_id = app.active.id;
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_sort_rows(
                0,
                SortDirection::Ascending,
                SortMode::Reverse,
                CaseSensitivity::Sensitive,
            )
            .unwrap();
        finish_document(&mut app);
        let generation = app
            .active
            .document
            .as_ref()
            .unwrap()
            .session
            .path()
            .to_owned();
        assert_ne!(generation, a);
        {
            let doc = app.active.document.as_mut().unwrap();
            doc.rename_header(1, "customer".into()).unwrap();
            doc.columns.order.reverse();
            doc.begin_cell_edit(1, 1, b"Alice".to_vec()).unwrap();
            doc.cell_edit.as_mut().unwrap().draft = "Current Alice".into();
        }
        let ctx = context();
        app.open_join_files();
        choose_open_document(&mut app, &ctx, 0, "current-main.csv");
        assert!(app.active.document.as_ref().unwrap().cell_edit.is_none());
        assert!(
            !app.active.document.as_ref().unwrap().join_reserved(),
            "idle selection holds no lease"
        );
        app.open_document_tab_with_encoding(
            b.clone(),
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..Default::default()
            },
            Some(Encoding::Utf8),
        )
        .unwrap();
        finish_document(&mut app);
        let additional_id = app.active.id;
        {
            let doc = app.active.document.as_mut().unwrap();
            doc.rename_header(1, "place".into()).unwrap();
            doc.begin_cell_edit(1, 1, b"LA".to_vec()).unwrap();
            doc.cell_edit.as_mut().unwrap().draft = "Current LA".into();
        }
        choose_open_document(&mut app, &ctx, 1, "current-additional.csv");
        let histories: Vec<_> = app
            .document_tabs()
            .filter_map(|tab| {
                tab.document
                    .as_ref()
                    .map(|doc| (tab.id, doc.edit_history.clone()))
            })
            .collect();
        click(&mut app, &ctx, "Analyze & Preview");
        finish(&mut app);
        assert!(app.join_files.as_ref().unwrap().plan.is_some());
        for tab in app.document_tabs() {
            let doc = tab.document.as_ref().unwrap();
            assert!(doc.join_reserved());
            assert!(!doc.can_undo());
            assert!(!doc.save_operations_ready());
        }
        {
            let doc = app.active.document.as_mut().unwrap();
            let edits = Arc::clone(&doc.cell_edits);
            assert!(doc.begin_cell_edit(1, 1, b"LA".to_vec()).is_err());
            assert!(doc.rename_header(1, "blocked".into()).is_err());
            assert!(doc.start_save().is_err());
            assert!(
                doc.start_cleanup(ColumnCleanup {
                    columns: vec![1],
                    operation: CleanupOperation::Trim
                })
                .is_err()
            );
            doc.discard_edits();
            assert!(Arc::ptr_eq(&edits, &doc.cell_edits));
        }
        app.reload_document();
        assert!(
            app.active
                .notice
                .as_ref()
                .unwrap()
                .text
                .contains("Join Files")
        );
        app.request_tab_close(&ctx);
        assert_eq!(app.active.id, additional_id);
        assert!(!app.active.close_confirmation_open);
        app.discard_and_close(&ctx);
        assert_eq!(app.active.id, additional_id);
        let unrelated = fixture(
            dir.path(),
            "unrelated-current.csv",
            b"id,value\nu,unchanged\n",
        );
        app.open_new_path(unrelated).unwrap();
        finish_document(&mut app);
        let unrelated_id = app.active.id;
        assert!(
            app.select_join_document(0, unrelated_id)
                .unwrap_err()
                .contains("Reset")
        );
        assert_eq!(app.join_files.as_ref().unwrap().paths[0].as_ref(), Some(&a));
        app.active
            .document
            .as_mut()
            .unwrap()
            .begin_cell_edit(1, 1, b"unchanged".to_vec())
            .unwrap();
        app.active
            .document
            .as_mut()
            .unwrap()
            .cell_edit
            .as_mut()
            .unwrap()
            .draft = "Unrelated edit".into();
        assert!(
            !app.join_destination_is_available(&generation.parent().unwrap().join("joined.csv"))
        );
        let output = dir.path().join("joined-current.csv");
        app.join_files.as_mut().unwrap().destination = Some(output.clone());
        click(&mut app, &ctx, "Create Joined File");
        finish(&mut app);
        assert_eq!(fs::read(&output).unwrap(),
            b"id (Main),customer,id (Additional),place\na,Current Alice,a,Seattle\nb,Bob,b,Current LA\n");
        assert_eq!(fs::read(&a).unwrap(), original_a);
        assert_eq!(fs::read(&b).unwrap(), original_b);
        for (id, history) in histories {
            let doc = app
                .document_tabs()
                .find(|tab| tab.id == id)
                .unwrap()
                .document
                .as_ref()
                .unwrap();
            assert_eq!(doc.edit_history, history);
            assert!(!doc.join_reserved());
        }
        assert_eq!(
            app.active
                .document
                .as_ref()
                .unwrap()
                .cell_edit
                .as_ref()
                .unwrap()
                .draft,
            "Unrelated edit"
        );
        assert_ne!(app.active.id, main_id);
        click(&mut app, &ctx, "Open Joined File");
        assert_eq!(app.active.document.as_ref().unwrap().logical_path, output);
        assert!(
            app.active
                .document
                .as_mut()
                .unwrap()
                .begin_cell_edit(1, 1, b"Current Alice".to_vec())
                .is_ok()
        );
    }

    #[cfg(unix)]
    #[test]
    fn browse_alias_uses_current_values_and_stale_picker_metadata_is_refreshed() {
        let dir = tempfile::tempdir().unwrap();
        let a = fixture(dir.path(), "alias-main.csv", b"id,name\nx,saved\n");
        let b = fixture(dir.path(), "alias-additional.csv", b"id,value\nx,match\n");
        let alias = dir.path().join("alias.csv");
        std::os::unix::fs::symlink(&a, &alias).unwrap();
        let mut app = QuarryApp::new(None, Instant::now());
        app.open_path_with_options(
            a.clone(),
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..Default::default()
            },
        )
        .unwrap();
        finish_document(&mut app);
        let ctx = context();
        app.open_join_files();
        app.select_join_input_path(0, alias).unwrap();
        finish(&mut app);
        assert_eq!(app.join_files.as_ref().unwrap().paths[0].as_ref(), Some(&a));
        assert!(app.join_files.as_ref().unwrap().document_inputs[0].is_some());
        {
            let doc = app.active.document.as_mut().unwrap();
            assert!(!doc.join_reserved());
            doc.begin_cell_edit(1, 1, b"saved".to_vec()).unwrap();
            doc.cell_edit.as_mut().unwrap().draft = "current".into();
            doc.rename_header(1, "current name".into()).unwrap();
        }
        app.select_join_input_path(1, b).unwrap();
        finish(&mut app);
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
                .contains("refreshed columns")
        );
        click(&mut app, &ctx, "Analyze & Preview");
        finish(&mut app);
        assert!(app.active.document.as_ref().unwrap().join_reserved());
        click(&mut app, &ctx, "Reset Analysis");
        finish(&mut app);
        assert!(!app.active.document.as_ref().unwrap().join_reserved());
        app.active
            .document
            .as_mut()
            .unwrap()
            .rename_header(1, "allowed again".into())
            .unwrap();
        assert_eq!(fs::read(a).unwrap(), b"id,name\nx,saved\n");
    }

    #[test]
    fn an_oversized_editor_draft_blocks_capture_without_losing_the_draft() {
        let dir = tempfile::tempdir().unwrap();
        let a = fixture(dir.path(), "draft.csv", b"id,value\nx,saved\n");
        let mut app = QuarryApp::new(Some(a), Instant::now());
        finish_document(&mut app);
        let id = app.active.id;
        {
            let doc = app.active.document.as_mut().unwrap();
            doc.begin_cell_edit(1, 1, b"saved".to_vec()).unwrap();
            doc.cell_edit.as_mut().unwrap().draft =
                "x".repeat(JoinConfig::default().max_record_bytes + 1);
        }
        app.open_join_files();
        assert!(
            app.select_join_document(0, id)
                .unwrap_err()
                .contains("record limit")
        );
        let doc = app.active.document.as_ref().unwrap();
        assert!(!doc.join_reserved());
        assert_eq!(
            doc.cell_edit.as_ref().unwrap().draft.len(),
            JoinConfig::default().max_record_bytes + 1
        );
        assert!(doc.cell_edits.is_empty());
        assert!(app.join_files.as_ref().unwrap().header_job.is_none());
    }
    #[test]
    fn current_native_encoding_and_unrepresentable_unsaved_values_join_as_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let original = b"id\tname\n\xe9\tsaved\n";
        let a = fixture(dir.path(), "current-native.tsv", original);
        let b = fixture(
            dir.path(),
            "utf8-other.csv",
            "id,city\né,Paris\n".as_bytes(),
        );
        let mut app = QuarryApp::new(None, Instant::now());
        app.open_document_tab_with_encoding(
            a.clone(),
            OpenOptions {
                delimiter: Some(b'\t'),
                header_mode: HeaderMode::FirstRow,
                ..Default::default()
            },
            Some(Encoding::Windows1252),
        )
        .unwrap();
        finish_document(&mut app);
        let id = app.active.id;
        {
            let doc = app.active.document.as_mut().unwrap();
            doc.rename_header(1, "λ result".into()).unwrap();
            doc.begin_cell_edit(1, 1, b"saved".to_vec()).unwrap();
            doc.cell_edit.as_mut().unwrap().draft = "λ".into();
        }
        app.open_join_files();
        app.select_join_document(0, id).unwrap();
        finish(&mut app);
        assert_eq!(
            app.join_files.as_ref().unwrap().formats[0].encoding,
            Some(Encoding::Windows1252)
        );
        assert_eq!(
            app.join_files.as_ref().unwrap().formats[0].delimiter,
            Some(b'\t')
        );
        app.select_join_input_path(1, b).unwrap();
        finish(&mut app);
        let ctx = context();
        click(&mut app, &ctx, "Analyze & Preview");
        finish(&mut app);
        let output = dir.path().join("current-utf8.tsv");
        app.join_files.as_mut().unwrap().destination = Some(output.clone());
        click(&mut app, &ctx, "Create Joined File");
        finish(&mut app);
        assert_eq!(
            fs::read(output).unwrap(),
            "id (Main)\tλ result\tid (Additional)\tcity\né\tλ\té\tParis\n".as_bytes()
        );
        assert_eq!(fs::read(a).unwrap(), original);
        assert!(app.active.document.as_ref().unwrap().is_dirty());
        assert!(!app.active.document.as_ref().unwrap().join_reserved());
    }

    #[test]
    fn current_input_requires_idle_unfiltered_documents_and_rejects_self_join() {
        let dir = tempfile::tempdir().unwrap();
        let a = fixture(dir.path(), "idle.csv", b"id,value\nx,one\n");
        let mut app = QuarryApp::new(Some(a), Instant::now());
        let id = app.active.id;
        app.open_join_files();
        assert!(
            app.select_join_document(0, id)
                .unwrap_err()
                .contains("active operation")
        );
        assert!(!app.active.document.as_ref().unwrap().join_reserved());
        finish_document(&mut app);
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_filter(FilterQuery::single(
                0,
                FilterOperator::Equals,
                b"x".to_vec(),
            ))
            .unwrap();
        assert!(
            app.select_join_document(0, id)
                .unwrap_err()
                .contains("filters")
        );
        app.active
            .document
            .as_mut()
            .unwrap()
            .clear_filter()
            .unwrap();
        app.select_join_document(0, id).unwrap();
        finish(&mut app);
        assert!(
            app.select_join_document(1, id)
                .unwrap_err()
                .contains("Self joins")
        );
        assert!(!app.active.document.as_ref().unwrap().join_reserved());
        assert!(app.join_files.as_ref().unwrap().paths[1].is_none());
    }

    #[test]
    fn closing_a_current_input_preview_releases_leases_and_preserves_edits() {
        let dir = tempfile::tempdir().unwrap();
        let a = fixture(dir.path(), "close-current.csv", b"id,value\nx,saved\n");
        let b = fixture(dir.path(), "close-other.csv", b"id,city\nx,Paris\n");
        let mut app = QuarryApp::new(Some(a.clone()), Instant::now());
        finish_document(&mut app);
        let id = app.active.id;
        {
            let doc = app.active.document.as_mut().unwrap();
            doc.begin_cell_edit(1, 1, b"saved".to_vec()).unwrap();
            doc.cell_edit.as_mut().unwrap().draft = "kept".into();
        }
        app.open_join_files();
        app.select_join_document(0, id).unwrap();
        finish(&mut app);
        app.select_join_input_path(1, b).unwrap();
        finish(&mut app);
        let ctx = context();
        click(&mut app, &ctx, "Analyze & Preview");
        finish(&mut app);
        let history = app.active.document.as_ref().unwrap().edit_history.clone();
        assert!(app.active.document.as_ref().unwrap().join_reserved());
        app.join_files.as_mut().unwrap().request_close(false);
        finish(&mut app);
        frame(&mut app, &ctx, vec![]);
        assert!(app.join_files.is_none());
        let doc = app.active.document.as_ref().unwrap();
        assert!(!doc.join_reserved());
        assert_eq!(doc.edit_history, history);
        assert_eq!(doc.cell_edits.get(&(1, 1)).unwrap().as_slice(), b"kept");
        assert!(doc.is_dirty());
        assert_eq!(fs::read(a).unwrap(), b"id,value\nx,saved\n");
    }
    #[test]
    fn data_edits_after_column_selection_use_current_keys_without_resetting_the_layout() {
        let dir = tempfile::tempdir().unwrap();
        let a = fixture(dir.path(), "data-change.csv", b"id,value\nx,saved\n");
        let b = fixture(dir.path(), "data-match.csv", b"id,city\ny,Paris\n");
        let mut app = QuarryApp::new(None, Instant::now());
        app.open_path_with_options(
            a.clone(),
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..Default::default()
            },
        )
        .unwrap();
        finish_document(&mut app);
        let id = app.active.id;
        app.open_join_files();
        app.select_join_document(0, id).unwrap();
        finish(&mut app);
        app.select_join_input_path(1, b).unwrap();
        finish(&mut app);
        app.join_files.as_mut().unwrap().output_columns[1].name = "current value".into();
        {
            let doc = app.active.document.as_mut().unwrap();
            doc.begin_cell_edit(1, 0, b"x".to_vec()).unwrap();
            doc.cell_edit.as_mut().unwrap().draft = "y".into();
            doc.begin_cell_edit(1, 1, b"saved".to_vec()).unwrap();
            doc.cell_edit.as_mut().unwrap().draft = "current".into();
        }
        let ctx = context();
        click(&mut app, &ctx, "Analyze & Preview");
        finish(&mut app);
        assert!(
            app.join_files.as_ref().unwrap().plan.is_some(),
            "{:?}",
            app.join_files.as_ref().unwrap().message
        );
        assert_eq!(
            app.join_files.as_ref().unwrap().output_columns[1].name,
            "current value"
        );
        let output = dir.path().join("data-current-result.csv");
        app.join_files.as_mut().unwrap().destination = Some(output.clone());
        click(&mut app, &ctx, "Create Joined File");
        finish(&mut app);
        assert_eq!(
            fs::read(output).unwrap(),
            b"id (Main),current value,id (Additional),city\ny,current,y,Paris\n"
        );
        assert_eq!(fs::read(a).unwrap(), b"id,value\nx,saved\n");
    }
    #[test]
    fn browsing_private_history_requires_the_current_document_instead() {
        let dir = tempfile::tempdir().unwrap();
        let a = fixture(
            dir.path(),
            "history-source.csv",
            b"id,value\nb,Bob\na,Alice\n",
        );
        let mut app = QuarryApp::new(None, Instant::now());
        app.open_path_with_options(
            a.clone(),
            OpenOptions {
                header_mode: HeaderMode::FirstRow,
                ..Default::default()
            },
        )
        .unwrap();
        finish_document(&mut app);
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_sort_rows(
                0,
                SortDirection::Ascending,
                SortMode::Reverse,
                CaseSensitivity::Sensitive,
            )
            .unwrap();
        finish_document(&mut app);
        let previous = app
            .active
            .document
            .as_ref()
            .unwrap()
            .session
            .path()
            .to_owned();
        app.active
            .document
            .as_mut()
            .unwrap()
            .start_cleanup(ColumnCleanup {
                columns: vec![1],
                operation: CleanupOperation::Uppercase,
            })
            .unwrap();
        finish_document(&mut app);
        let current = app
            .active
            .document
            .as_ref()
            .unwrap()
            .session
            .path()
            .to_owned();
        assert_ne!(previous, current);
        app.open_join_files();
        assert!(
            app.select_join_input_path(0, previous)
                .unwrap_err()
                .contains("private history")
        );
        app.select_join_input_path(0, current).unwrap();
        finish(&mut app);
        assert_eq!(app.join_files.as_ref().unwrap().paths[0].as_ref(), Some(&a));
        assert!(app.join_files.as_ref().unwrap().document_inputs[0].is_some());
    }
}
