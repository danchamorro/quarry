//! Per-tab inspection state and accessible report/recovery controls.
use super::*;
use crate::tabs::paths_alias;
use quarry_core::{
    InspectionEdits, InspectionJob, InspectionOptions, InspectionReport, InspectionStatus,
    RecoveryJob, SourcePreview,
};

struct Snapshot {
    session: Arc<Session>,
    headers: BTreeMap<usize, Arc<String>>,
    cells: BTreeMap<(u64, usize), Arc<Vec<u8>>>,
}
impl Snapshot {
    /// Shares the current source and effective edit values for later freshness checks.
    fn capture(doc: &Document) -> Self {
        Self {
            session: doc.session.clone(),
            headers: doc.header_renames.clone(),
            cells: doc.cell_edits.clone(),
        }
    }
    /// Checks source identity and effective drafts without treating editor focus as an edit.
    fn current(&self, doc: &Document) -> bool {
        Arc::ptr_eq(&self.session, &doc.session)
            && !doc.source_changed
            && doc.cell_edit.as_ref().is_none_or(|edit| {
                doc.cell_edits
                    .get(&(edit.row, edit.column))
                    .map_or(edit.source.as_slice(), |v| v.as_slice())
                    == edit.draft.as_bytes()
            })
            && doc.header_edit.as_ref().is_none_or(|edit| {
                doc.header_renames
                    .get(&edit.column)
                    .map(|v| v.as_str())
                    .or_else(|| doc.source_header_name(edit.column))
                    == Some(edit.draft.as_str())
            })
            && self.headers.len() == doc.header_renames.len()
            && self.cells.len() == doc.cell_edits.len()
            && self
                .headers
                .iter()
                .all(|(k, v)| doc.header_renames.get(k).is_some_and(|n| Arc::ptr_eq(v, n)))
            && self
                .cells
                .iter()
                .all(|(k, v)| doc.cell_edits.get(k).is_some_and(|n| Arc::ptr_eq(v, n)))
    }
    /// Materializes the captured edit values once for the background worker.
    fn edits(&self) -> InspectionEdits {
        InspectionEdits {
            headers: self
                .headers
                .iter()
                .map(|(k, v)| (*k, v.as_bytes().to_vec()))
                .collect(),
            cells: self
                .cells
                .iter()
                .map(|(k, v)| (*k, v.as_ref().clone()))
                .collect(),
        }
    }
}
pub(super) struct InspectionUi {
    pub open: bool,
    path: Option<PathBuf>,
    options: InspectionOptions,
    snapshot: Option<Snapshot>,
    job: Option<InspectionJob>,
    recovery: Option<RecoveryJob>,
    report: Option<InspectionReport>,
    selected: BTreeSet<usize>,
    focused: usize,
    preview: Option<SourcePreview>,
    message: Option<String>,
    operation_diagnostic: Option<Box<quarry_core::Diagnostic>>,
    stale: bool,
    last_poll: Instant,
    delimiter: DelimiterMode,
    header: HeaderMode,
    encoding: Option<Encoding>,
    confirm_destination: Option<PathBuf>,
}
impl Default for InspectionUi {
    /// Starts with no target, report, worker, or recovery selection.
    fn default() -> Self {
        Self {
            open: false,
            path: None,
            options: Default::default(),
            snapshot: None,
            job: None,
            recovery: None,
            report: None,
            selected: Default::default(),
            focused: 0,
            preview: None,
            message: None,
            operation_diagnostic: None,
            stale: false,
            last_poll: Instant::now() - POLL_INTERVAL,
            delimiter: DelimiterMode::Auto,
            header: HeaderMode::Auto,
            encoding: None,
            confirm_destination: None,
        }
    }
}
impl InspectionUi {
    /// Reports whether inspection or recovery still owns a worker.
    pub fn busy(&self) -> bool {
        self.job.is_some() || self.recovery.is_some()
    }
    /// Reports whether this tab currently reserves a recovery output.
    pub fn recovering(&self) -> bool {
        self.recovery.is_some()
    }
    /// Requests cancellation while leaving completed reports and edits intact.
    pub fn cancel(&mut self) {
        if let Some(j) = &self.job {
            j.cancel();
        }
        if let Some(j) = &self.recovery {
            j.cancel();
        }
    }
    /// Retains a failed-open target and a bounded preview without replacing the current document.
    pub fn failed(
        path: PathBuf,
        options: OpenOptions,
        encoding: Option<Encoding>,
        message: String,
    ) -> Self {
        let mut s = Self {
            open: true,
            path: Some(path.clone()),
            options: InspectionOptions {
                delimiter: options.delimiter,
                header_mode: options.header_mode,
                encoding,
                ..Default::default()
            },
            message: Some(message),
            encoding,
            header: options.header_mode,
            ..Default::default()
        };
        s.delimiter = DelimiterMode::ALL
            .into_iter()
            .find(|m| m.delimiter() == options.delimiter)
            .unwrap_or(DelimiterMode::Auto);
        s.preview = SourcePreview::prefix(&path, encoding).ok();
        s
    }
    /// Starts inspection of the retained target and clears previous selections.
    fn start(&mut self) -> Result<(), String> {
        let path = self.path.as_ref().ok_or("Choose a file to inspect.")?;
        self.job = Some(
            InspectionJob::start(
                path,
                self.options,
                self.snapshot
                    .as_ref()
                    .map_or_else(InspectionEdits::default, Snapshot::edits),
            )
            .map_err(|e| e.to_string())?,
        );
        self.report = None;
        self.selected.clear();
        self.focused = 0;
        self.preview = None;
        self.stale = false;
        self.message = None;
        self.confirm_destination = None;
        self.open = true;
        Ok(())
    }
    /// Cancels stale work and disables actions tied to an old source or edit snapshot.
    fn invalidate(&mut self) {
        self.cancel();
        self.selected.clear();
        self.preview = None;
        self.confirm_destination = None;
        self.stale = true;
        self.message = Some(
            "The document or source changed. Inspect again before using these findings.".into(),
        );
    }
    /// Adopts completed work only while its snapshot is current.
    fn poll(&mut self, doc: Option<&Document>) {
        if self.last_poll.elapsed() < POLL_INTERVAL {
            return;
        }
        self.last_poll = Instant::now();
        if self
            .snapshot
            .as_ref()
            .is_some_and(|s| doc.is_none_or(|d| !s.current(d)))
            && !self.stale
        {
            self.invalidate();
        }
        if self.report.as_ref().is_some_and(|r| !r.is_current()) && !self.stale {
            self.invalidate();
        }
        if self.job.as_ref().is_some_and(|j| j.progress().done) {
            let result = self.job.take().unwrap().wait();
            if !self.stale {
                match result {
                    Ok(report) => {
                        self.preview = if report.findings.is_empty() {
                            None
                        } else {
                            report.preview(0).ok()
                        };
                        self.report = Some(report);
                    }
                    Err(e) => self.message = Some(e.to_string()),
                }
            }
        }
        if self.recovery.as_ref().is_some_and(|j| j.progress().done) {
            match self.recovery.take().unwrap().wait() {
                Ok(Some(s)) => {
                    self.message = Some(format!(
                        "Recovery copy created: {}\n{} record(s) removed; {} retained. The original is unchanged.",
                        s.destination.display(),
                        s.removed_records,
                        s.retained_records
                    ))
                }
                Ok(None) => {
                    self.message = Some("Recovery cancelled. No copy was published.".into())
                }
                Err(e) => self.message = Some(e.to_string()),
            }
        }
    }
}

enum Command {
    Inspect,
    Retry,
    GoTo(usize),
    ChooseCopy,
    CreateCopy,
    Cancel,
    Close,
}
impl QuarryApp {
    /// Repeats the visible failed-file target or captures the current document again.
    fn restart_visible_inspection(&mut self) {
        let separate = self.active.inspection.snapshot.is_none()
            && self.active.inspection.path.as_ref().is_some_and(|path| {
                self.active
                    .document
                    .as_ref()
                    .is_none_or(|doc| !paths_alias(path, doc.session.path()))
            });
        if separate {
            if let Err(error) = self.active.inspection.start() {
                self.active.inspection.message = Some(error);
            }
        } else {
            self.start_current_inspection();
        }
    }
    /// Commits current drafts and inspects effective values under document-operation rules.
    pub(super) fn start_current_inspection(&mut self) {
        if self.active.inspection.recovering() {
            return;
        }
        if let Some(doc) = self.active.document.as_mut() {
            if doc.save_job.is_some()
                || doc.export_job.is_some()
                || doc.structural_job.is_some()
                || doc.search_job.is_some()
                || doc.filter_job.is_some()
                || doc.source_changed
            {
                self.active.notice = Some(AppMessage::warning(
                    "Finish the active operation or reopen the changed source before inspecting.",
                ));
                return;
            }
            doc.commit_edits();
            let snapshot = Snapshot::capture(doc);
            self.active.inspection = InspectionUi {
                open: true,
                path: Some(doc.session.path().to_owned()),
                options: InspectionOptions {
                    delimiter: Some(doc.session.dialect.delimiter),
                    header_mode: if doc.session.dialect.has_header {
                        HeaderMode::FirstRow
                    } else {
                        HeaderMode::NoHeader
                    },
                    encoding: Some(doc.session.encoding()),
                    ..Default::default()
                },
                delimiter: self.active.delimiter_mode,
                header: self.active.header_mode,
                encoding: self.active.encoding_mode,
                snapshot: Some(snapshot),
                ..Default::default()
            };
        } else if self.active.inspection.path.is_none() {
            if let Some(path) = rfd::FileDialog::new()
                .set_title("Inspect a delimited file")
                .pick_file()
            {
                self.active.inspection =
                    InspectionUi::failed(path, OpenOptions::default(), None, String::new());
            } else {
                return;
            }
        }
        if let Err(e) = self.active.inspection.start() {
            self.active.inspection.message = Some(e);
        }
    }
    /// Advances every tab worker and releases completed recovery reservations immediately.
    pub(super) fn poll_inspections(&mut self, ctx: &egui::Context) {
        for tab in std::iter::once(&mut self.active).chain(self.tabs.iter_mut()) {
            let was_recovering = tab.inspection.recovering();
            tab.inspection.poll(tab.document.as_ref());
            if was_recovering && !tab.inspection.recovering() {
                tab.write_destination = None;
            }
            if tab.inspection.busy() {
                ctx.request_repaint_after(POLL_INTERVAL);
            }
        }
    }
    /// Returns only the diagnostic associated with the currently displayed notice.
    fn notice_diagnostic(&self) -> Option<Box<quarry_core::Diagnostic>> {
        let notice = self.active.notice.as_ref()?;
        notice.diagnostic.clone().or_else(|| {
            let doc = self.active.document.as_ref()?;
            let original = doc.pending_diagnostic_message.as_ref()?;
            if !original.is_empty() && notice.text.contains(original) {
                doc.pending_diagnostic.clone()
            } else {
                None
            }
        })
    }

    /// Simple status and selection warnings do not replace an existing report.
    pub(super) fn notice_has_inspection_details(&self) -> bool {
        self.active.notice.as_ref().is_some_and(|notice| {
            notice.inspection_target.is_some() || self.notice_diagnostic().is_some()
        })
    }

    /// Opens the notice's source, even if a subsequent successful open changed tabs.
    pub(super) fn show_operation_diagnostic(&mut self) {
        if !self.notice_has_inspection_details() {
            return;
        }
        if let Some(target) = self
            .active
            .notice
            .as_ref()
            .and_then(|n| n.inspection_target.clone())
        {
            let target = *target;
            if !self
                .active
                .inspection
                .path
                .as_ref()
                .is_some_and(|path| paths_alias(path, &target.path))
            {
                self.active.inspection = InspectionUi::failed(
                    target.path,
                    target.options,
                    target.encoding,
                    target.message,
                );
            }
            self.active.inspection.open = true;
            return;
        }
        let diagnostic = self.notice_diagnostic();
        let message = self
            .active
            .notice
            .as_ref()
            .map(|m| m.text.clone())
            .unwrap_or_default();
        if let Some(doc) = &self.active.document {
            self.active.inspection = InspectionUi::failed(
                doc.session.path().to_owned(),
                OpenOptions {
                    delimiter: Some(doc.session.dialect.delimiter),
                    header_mode: if doc.session.dialect.has_header {
                        HeaderMode::FirstRow
                    } else {
                        HeaderMode::NoHeader
                    },
                    ..Default::default()
                },
                Some(doc.session.encoding()),
                format!(
                    "{message}\nAn operation error may refer to edited or generated data; the source prefix below is not an inferred error location."
                ),
            );
        }
        self.active.inspection.operation_diagnostic = diagnostic;
        self.active.inspection.open = true;
    }
    /// Renders the active report and dispatches explicit user actions.
    pub(super) fn show_inspection(&mut self, ctx: &egui::Context) {
        if !self.active.inspection.open {
            return;
        }
        let navigable_source = self
            .active
            .document
            .as_ref()
            .zip(self.active.inspection.path.as_ref())
            .is_some_and(|(doc, path)| paths_alias(doc.session.path(), path));
        let mut command = None;
        let mut open = true;
        let state = &mut self.active.inspection;
        let busy = state.busy();
        egui::Window::new("Inspect File")
            .id(egui::Id::new(("inspection", self.active.id)))
            .open(&mut open)
            .default_size([730.0, 600.0])
            .resizable(true)
            .vscroll(true)
            .show(ctx, |ui| {
                command = draw_inspection(ui, state, busy, navigable_source);
            });
        if !open {
            command = Some(Command::Close);
        }
        match command {
            Some(Command::Inspect) => self.restart_visible_inspection(),
            Some(Command::Cancel) => self.active.inspection.cancel(),
            Some(Command::Close) => {
                self.active.inspection.cancel();
                self.active.inspection.open = false;
            }
            Some(Command::Retry) => self.retry_inspection_format(),
            Some(Command::GoTo(i)) => self.navigate_inspection(i),
            Some(Command::ChooseCopy) => {
                if !self.write_slot_available() {
                    self.active.inspection.message =
                        Some("Wait for an active file operation to finish.".into());
                    return;
                }
                let name = self
                    .active
                    .inspection
                    .path
                    .as_ref()
                    .and_then(|p| p.file_stem())
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
                    + "-recovered.csv";
                if let Some(path) = rfd::FileDialog::new()
                    .set_title("Create Recovery Copy")
                    .set_file_name(name)
                    .save_file()
                {
                    if self.destination_available(&path) {
                        self.active.inspection.confirm_destination = Some(path);
                    } else {
                        self.active.inspection.message = Some(
                            "This destination is already in use by an open document or writer."
                                .into(),
                        );
                    }
                }
            }
            Some(Command::CreateCopy) => {
                if self
                    .active
                    .inspection
                    .snapshot
                    .as_ref()
                    .is_some_and(|snap| {
                        self.active
                            .document
                            .as_ref()
                            .is_none_or(|doc| !snap.current(doc))
                    })
                {
                    self.active.inspection.invalidate();
                    return;
                }
                if !self.write_slot_available() {
                    self.active.inspection.message =
                        Some("Wait for another writer to finish.".into());
                    return;
                }
                if self
                    .active
                    .inspection
                    .confirm_destination
                    .as_ref()
                    .is_some_and(|p| !self.destination_available(p))
                {
                    self.active.inspection.message =
                        Some("This destination is in use by another document or writer.".into());
                    return;
                }
                let state = &mut self.active.inspection;
                if let (Some(report), Some(path)) =
                    (&state.report, state.confirm_destination.take())
                {
                    match RecoveryJob::start(report, &state.selected, &path) {
                        Ok(job) => {
                            state.recovery = Some(job);
                            self.active.write_destination = Some(path);
                        }
                        Err(e) => state.message = Some(e.to_string()),
                    }
                }
            }
            None => {}
        }
    }
    /// Retries the selected format without discarding unsaved work on failure.
    fn retry_inspection_format(&mut self) {
        let state = &self.active.inspection;
        let Some(path) = state.path.clone() else {
            return;
        };
        let (delimiter, header, encoding) = (state.delimiter, state.header, state.encoding);
        let same = self
            .active
            .document
            .as_ref()
            .is_some_and(|d| paths_alias(d.session.path(), &path));
        if same {
            if self
                .active
                .document
                .as_ref()
                .is_some_and(Document::is_dirty)
            {
                self.active.inspection.message=Some("Save or discard your changes before changing the document format. Your edits are preserved.".into());
                return;
            }
            let result = self.reopen_current_path_with_encoding(
                path,
                OpenOptions {
                    delimiter: delimiter.delimiter(),
                    header_mode: header,
                    ..Default::default()
                },
                encoding,
            );
            match result {
                Ok(()) => {
                    self.active.delimiter_mode = delimiter;
                    self.active.header_mode = header;
                    self.active.notice = None;
                    self.start_current_inspection();
                }
                Err(error) => {
                    self.active.inspection.message =
                        Some(format!("The document format is unchanged. {}", error.text));
                    self.active.notice = Some(error);
                }
            }
        } else {
            match self.open_document_tab_with_encoding(
                path.clone(),
                OpenOptions {
                    delimiter: delimiter.delimiter(),
                    header_mode: header,
                    ..Default::default()
                },
                encoding,
            ) {
                Ok(()) => self.start_current_inspection(),
                Err(e) => {
                    self.active.inspection = InspectionUi::failed(
                        path,
                        OpenOptions {
                            delimiter: delimiter.delimiter(),
                            header_mode: header,
                            ..Default::default()
                        },
                        encoding,
                        e.text,
                    );
                    if let Err(e) = self.active.inspection.start() {
                        self.active.inspection.message = Some(e);
                    }
                }
            }
        }
    }
    /// Selects a known record in its owning document without guessing through filters.
    fn navigate_inspection(&mut self, index: usize) {
        let Some(report) = &self.active.inspection.report else {
            return;
        };
        let Some(finding) = report.findings.get(index) else {
            return;
        };
        let Some(record) = finding.record else {
            return;
        };
        let Some(doc) = self.active.document.as_mut() else {
            return;
        };
        if !paths_alias(doc.session.path(), &report.path) || !report.is_current() {
            return;
        }
        if doc.filter_active() {
            self.active.inspection.message=Some("Clear the filter before showing this source record. The finding and edits are preserved.".into());
            return;
        }
        let row = record - 1;
        if let Err(e) = doc.navigate(row) {
            self.active.inspection.message = Some(e);
            return;
        }
        if let Some(column) = finding.column {
            let column = column - 1;
            if doc.columns.hidden.get(column).copied().unwrap_or(true) {
                self.active.inspection.message = Some(
                    "This column is hidden. Show it in Columns before navigating to the cell."
                        .into(),
                );
                return;
            }
            doc.selection = Some(GridSelection::Cell { row, column });
            doc.reveal_cell = Some((row, column));
        }
        if finding.column.is_none() {
            doc.selection = Some(GridSelection::Row { row });
        }
        self.active.inspection.open = false;
    }
}

/// Draws the report independently of application actions and native dialogs.
fn draw_inspection(
    ui: &mut egui::Ui,
    state: &mut InspectionUi,
    busy: bool,
    navigable_source: bool,
) -> Option<Command> {
    let mut command = None;
    if let Some(path) = &state.path {
        ui.label(path.display().to_string());
    }
    if let Some(message) = &state.message {
        ui.label(message);
        if ui.button("Copy Details").clicked() {
            ui.ctx().copy_text(message.clone());
        }
    }
    if let Some(d) = &state.operation_diagnostic {
        ui.label(d.details());
        if ui.button("Copy operation details").clicked() {
            ui.ctx().copy_text(d.details());
        }
    }
    if let Some(job) = &state.job {
        let p = job.progress();
        ui.add(
            egui::ProgressBar::new(progress_fraction(p.bytes_scanned, p.total_bytes, p.done)).text(
                format!(
                    "{} — {} / {} bytes",
                    p.phase, p.bytes_scanned, p.total_bytes
                ),
            ),
        );
    }
    if let Some(job) = &state.recovery {
        let p = job.progress();
        ui.add(
            egui::ProgressBar::new(progress_fraction(p.bytes_scanned, p.total_bytes, p.done))
                .text(p.phase),
        );
    }
    ui.horizontal(|ui| {
        if ui
            .add_enabled(!busy, egui::Button::new("Inspect again"))
            .clicked()
        {
            command = Some(Command::Inspect);
        }
        if ui
            .add_enabled(busy, egui::Button::new("Cancel operation"))
            .clicked()
        {
            command = Some(Command::Cancel);
        }
        if ui.button("Close").clicked() {
            command = Some(Command::Close);
        }
    });
    ui.collapsing("Change Format and Retry", |ui| {
        ui.add_enabled_ui(!busy, |ui| {
            egui::ComboBox::from_id_salt("inspect-delimiter")
                .selected_text(state.delimiter.label())
                .show_ui(ui, |ui| {
                    for d in DelimiterMode::ALL {
                        ui.selectable_value(&mut state.delimiter, d, d.label());
                    }
                });
            egui::ComboBox::from_id_salt("inspect-header")
                .selected_text(match state.header {
                    HeaderMode::Auto => "Auto header",
                    HeaderMode::FirstRow => "First row is header",
                    HeaderMode::NoHeader => "No header",
                })
                .show_ui(ui, |ui| {
                    for (h, label) in [
                        (HeaderMode::Auto, "Auto header"),
                        (HeaderMode::FirstRow, "First row is header"),
                        (HeaderMode::NoHeader, "No header"),
                    ] {
                        ui.selectable_value(&mut state.header, h, label);
                    }
                });
            egui::ComboBox::from_id_salt("inspect-encoding")
                .selected_text(state.encoding.map_or("Auto (BOM or UTF-8)", |e| e.label()))
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut state.encoding, None, "Auto (BOM or UTF-8)");
                    for e in Encoding::ALL {
                        ui.selectable_value(&mut state.encoding, Some(e), e.label());
                    }
                });
            if ui.button("Apply format and retry").clicked() {
                command = Some(Command::Retry);
            }
        });
    });
    if let Some(report) = &state.report {
        ui.separator();
        ui.label(format!(
            "{:?} — {} / {} source bytes; {} records; {} finding(s) ({} shown)",
            report.status,
            report.bytes_scanned,
            report.total_bytes,
            report.records,
            report.total_findings,
            report.findings.len()
        ));
        ui.label(format!(
            "Encoding: {} · delimiter: {:?} · header: {}",
            report.encoding,
            report.delimiter as char,
            if report.header_certain {
                report.has_header.to_string()
            } else {
                "not determined".into()
            }
        ));
        if report.status != InspectionStatus::Complete {
            ui.strong("Coverage is partial. Uninspected data is not known to be valid.");
        }
        if report.total_findings > report.findings.len() as u64 {
            ui.label(
                "Additional findings were counted but are not retained in this bounded report.",
            );
        }
        if report.findings.is_empty() {
            ui.label(if report.status == InspectionStatus::Complete {
                "No data issues found."
            } else {
                "No findings in the inspected portion."
            });
        }
        let old = state.focused;
        egui::ScrollArea::vertical()
            .id_salt("findings")
            .max_height(160.0)
            .show_rows(ui, 24.0, report.findings.len(), |ui, rows| {
                for i in rows {
                    let d = &report.findings[i];
                    ui.push_id(i, |ui| {
                        ui.horizontal(|ui| {
                            let mut selected = state.selected.contains(&i);
                            if ui
                                .add_enabled(
                                    !busy && !state.stale && report.can_omit(i),
                                    egui::Checkbox::new(
                                        &mut selected,
                                        format!("Omit record {}", d.record.unwrap_or(0)),
                                    ),
                                )
                                .changed()
                            {
                                if selected {
                                    state.selected.insert(i);
                                } else {
                                    state.selected.remove(&i);
                                }
                            }
                            if ui
                                .selectable_label(
                                    state.focused == i,
                                    format!("{}: {}", d.kind.label(), d.message),
                                )
                                .clicked()
                            {
                                state.focused = i;
                            }
                        });
                    });
                }
            });
        if old != state.focused && !state.stale {
            state.preview = report.preview(state.focused).ok();
        }
        if let Some(d) = report.findings.get(state.focused) {
            ui.separator();
            ui.label(d.details());
            ui.horizontal(|ui| {
                if ui.button("Copy Details").clicked() {
                    ui.ctx()
                        .copy_text(format!("{}\n{}", report.path.display(), d.details()));
                }
                if ui
                    .add_enabled(
                        !busy && !state.stale && navigable_source && d.data_row.is_some(),
                        egui::Button::new("Show Location"),
                    )
                    .clicked()
                {
                    command = Some(Command::GoTo(state.focused));
                }
            });
        }
        ui.separator();
        if !report.header_certain {
            ui.label("Choose the header setting explicitly before creating a recovery copy.");
        }
        if ui
            .add_enabled(
                !busy
                    && !state.stale
                    && !state.selected.is_empty()
                    && report.status == InspectionStatus::Complete,
                egui::Button::new("Create Recovery Copy…"),
            )
            .clicked()
        {
            command = Some(Command::ChooseCopy);
        }
    }
    if let Some(preview) = &state.preview {
        ui.collapsing(
            if state.report.is_some() {
                "Source context (bounded to 8 KiB)"
            } else {
                "Source prefix (not an inferred error location; bounded to 8 KiB)"
            },
            |ui| {
                ui.label(format!("Source bytes {}..{}", preview.start, preview.end));
                egui::ScrollArea::both()
                    .id_salt("source-preview")
                    .max_height(150.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(egui::RichText::new(&preview.text).monospace())
                                .selectable(true),
                        );
                    });
            },
        );
    }
    if let Some(path) = &state.confirm_destination {
        ui.separator();
        let count = state
            .report
            .as_ref()
            .and_then(|r| r.selected_records(&state.selected).ok())
            .map_or(0, |r| r.len());
        ui.strong(format!(
            "Remove {count} unique data record{} into {}?",
            if count == 1 { "" } else { "s" },
            path.display()
        ));
        ui.label("The original stays unchanged. Retained records are validated before this new file is published.");
        if ui.button("Create confirmed copy").clicked() {
            command = Some(Command::CreateCopy);
        }
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Builds an indexed synthetic document for desktop-state regression tests.
    fn app(bytes: &[u8]) -> (tempfile::TempDir, QuarryApp) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.csv");
        std::fs::write(&path, bytes).unwrap();
        let mut app = QuarryApp::new(Some(path), Instant::now());
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.active.document.as_ref().unwrap().job.is_some() {
            app.active.document.as_mut().unwrap().poll().unwrap();
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        (dir, app)
    }
    /// Polls a test worker until its source-bound report has been adopted.
    fn wait_report(app: &mut QuarryApp) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.active.inspection.job.is_some() {
            app.active.inspection.last_poll = Instant::now() - POLL_INTERVAL;
            app.poll_inspections(&egui::Context::default());
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
    }
    #[test]
    /// Verifies that inspection uses unsaved values without owning copies each frame.
    fn inspection_uses_unsaved_values_without_owning_copies_each_frame() {
        let (_dir, mut app) = app(b"name,id\nAlice,1\nBob,2\n");
        app.active
            .document
            .as_mut()
            .unwrap()
            .cell_edits
            .insert((1, 1), Arc::new(vec![0xff]));
        app.start_current_inspection();
        wait_report(&mut app);
        let report = app.active.inspection.report.as_ref().unwrap();
        assert!(
            report
                .findings
                .iter()
                .any(|d| d.edited && d.byte_offset.is_none())
        );
        assert!(
            app.active
                .inspection
                .snapshot
                .as_ref()
                .unwrap()
                .current(app.active.document.as_ref().unwrap())
        );
    }
    #[test]
    /// Verifies that editing invalidates old findings before recovery.
    fn editing_invalidates_old_findings_before_recovery() {
        let (_dir, mut app) = app(b"name,id\nAlice,1\nBob,2\n");
        app.start_current_inspection();
        wait_report(&mut app);
        app.active
            .document
            .as_mut()
            .unwrap()
            .cell_edits
            .insert((1, 1), Arc::new(b"3".to_vec()));
        app.active.inspection.last_poll = Instant::now() - POLL_INTERVAL;
        app.poll_inspections(&egui::Context::default());
        assert!(app.active.inspection.stale);
        assert!(app.active.inspection.selected.is_empty());
        assert!(app.active.inspection.preview.is_none());
    }
    #[test]
    /// Verifies that failed open retains current dirty document and shows bounded prefix.
    fn failed_open_retains_current_dirty_document_and_shows_bounded_prefix() {
        let (dir, mut app) = app(b"name,id\nAlice,1\nBob,2\n");
        app.active
            .document
            .as_mut()
            .unwrap()
            .cell_edits
            .insert((1, 1), Arc::new(b"3".to_vec()));
        let old = app.active.id;
        let bad = dir.path().join("bad.csv");
        std::fs::write(&bad, b"a,bo\"gus\n").unwrap();
        assert!(
            app.open_document_tab_with_encoding(bad.clone(), OpenOptions::default(), None)
                .is_err()
        );
        assert_eq!(app.active.id, old);
        assert_eq!(app.active.inspection.path, Some(bad));
        assert!(app.active.inspection.open);
        assert!(app.active.inspection.preview.as_ref().unwrap().end <= 8192);
        assert_eq!(
            app.active.document.as_ref().unwrap().cell_edits[&(1, 1)].as_slice(),
            b"3"
        );
    }
    #[test]
    /// Verifies that report stays with owning tab and view changes do not invalidate.
    fn report_stays_with_owning_tab_and_view_changes_do_not_invalidate() {
        let (dir, mut app) = app(b"name,id\nAlice,1\nBob,2\n");
        app.start_current_inspection();
        wait_report(&mut app);
        app.active.document.as_mut().unwrap().viewport_start = 2;
        app.active.inspection.last_poll = Instant::now() - POLL_INTERVAL;
        app.poll_inspections(&egui::Context::default());
        assert!(!app.active.inspection.stale);
        let owner = app.active.id;
        let other = dir.path().join("other.csv");
        std::fs::write(&other, b"name,id\nCarol,3\n").unwrap();
        app.open_document_tab_with_encoding(other, OpenOptions::default(), None)
            .unwrap();
        assert!(app.active.inspection.report.is_none());
        assert!(
            app.tabs
                .iter()
                .any(|t| t.id == owner && t.inspection.report.is_some())
        );
    }
    #[test]
    /// Verifies that source change invalidates preview even without an edit.
    fn source_change_invalidates_preview_even_without_an_edit() {
        let (_dir, mut app) = app(b"name,id\nAlice,1\nBob,2\n");
        app.start_current_inspection();
        wait_report(&mut app);
        let path = app.active.document.as_ref().unwrap().session.path();
        std::fs::write(path, b"name,id\nChanged,99\n").unwrap();
        app.active.inspection.last_poll = Instant::now() - POLL_INTERVAL;
        app.poll_inspections(&egui::Context::default());
        assert!(app.active.inspection.stale);
    }
    #[test]
    /// Verifies that failed file can be inspected without becoming the current document.
    fn failed_file_can_be_inspected_without_becoming_the_current_document() {
        let (dir, mut app) = app(b"name,id\nAlice,1\nBob,2\n");
        let bad = dir.path().join("bad.csv");
        std::fs::write(&bad, b"a,b\nx,\"bad\n").unwrap();
        app.active.inspection = InspectionUi::failed(
            bad.clone(),
            OpenOptions {
                delimiter: Some(b','),
                ..Default::default()
            },
            None,
            "bad quoting".into(),
        );
        app.active.inspection.start().unwrap();
        wait_report(&mut app);
        let report = app.active.inspection.report.as_ref().unwrap();
        assert_eq!(report.status, InspectionStatus::Stopped);
        assert_eq!(report.path, bad);
        assert_ne!(app.active.document.as_ref().unwrap().session.path(), bad);
    }
    #[test]
    /// Verifies that failed file inspect again does not switch to the open document.
    fn failed_file_inspect_again_does_not_switch_to_the_open_document() {
        let (dir, mut app) = app(b"name,id\nAlice,1\nBob,2\n");
        let failed = dir.path().join("failed.csv");
        std::fs::write(&failed, b"a,b\nx,\"bad\n").unwrap();
        app.active.inspection = InspectionUi::failed(
            failed.clone(),
            OpenOptions {
                delimiter: Some(b','),
                ..Default::default()
            },
            None,
            "bad".into(),
        );
        app.restart_visible_inspection();
        wait_report(&mut app);
        assert_eq!(app.active.inspection.report.as_ref().unwrap().path, failed);
        assert_ne!(app.active.document.as_ref().unwrap().session.path(), failed);
    }
    #[test]
    /// Verifies that failed open details preserve the failed target.
    fn failed_open_details_preserve_the_failed_target() {
        let (dir, mut app) = app(b"name,id\nAlice,1\nBob,2\n");
        let failed = dir.path().join("failed.csv");
        std::fs::write(&failed, b"a,b\nx,\"bad\n").unwrap();
        app.open_path_and_report(failed.clone());
        app.show_operation_diagnostic();
        assert_eq!(app.active.inspection.path.as_ref(), Some(&failed));
        assert!(app.active.inspection.open);
    }

    #[test]
    /// Verifies that failed format retry preserves current document and report.
    fn failed_format_retry_preserves_current_document_and_report() {
        let (_dir, mut app) = app(b"name,id\nAlice,1\nBob,22\n");
        app.start_current_inspection();
        wait_report(&mut app);
        let session = app.active.document.as_ref().unwrap().session.clone();
        app.active.inspection.encoding = Some(Encoding::Utf16Le);
        app.retry_inspection_format();
        assert!(Arc::ptr_eq(
            &session,
            &app.active.document.as_ref().unwrap().session
        ));
        assert!(app.active.inspection.job.is_none());
        assert!(app.active.inspection.report.is_some());
        assert!(
            app.active
                .inspection
                .message
                .as_ref()
                .is_some_and(|s| s.contains("unchanged"))
        );
    }
    #[test]
    /// Verifies that inspection accessibility actions and keyboard close work.
    fn inspection_accessibility_actions_and_keyboard_close_work() {
        let (_dir, mut app) = app(b"name,id\nAlice,1\nBob\n");
        app.start_current_inspection();
        wait_report(&mut app);
        let ctx = egui::Context::default();
        configure_style(&ctx);
        ctx.enable_accesskit();
        let mut frame = eframe::Frame::_new_kittest();
        let mut render = |app: &mut QuarryApp, events| {
            ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1280.0, 764.0),
                    )),
                    events,
                    ..Default::default()
                },
                |ctx| eframe::App::update(app, ctx, &mut frame),
            )
        };
        let action = |target, action| {
            egui::Event::AccessKitActionRequest(egui::accesskit::ActionRequest {
                target,
                action,
                data: None,
            })
        };
        let _ = render(&mut app, vec![]);
        let out = render(&mut app, vec![]);
        let nodes = &out.platform_output.accesskit_update.as_ref().unwrap().nodes;
        let (omit, node) = nodes
            .iter()
            .find(|(_, n)| n.label() == Some("Omit record 3"))
            .unwrap();
        assert_eq!(node.role(), egui::accesskit::Role::CheckBox);
        let _ = render(
            &mut app,
            vec![action(*omit, egui::accesskit::Action::Click)],
        );
        assert!(app.active.inspection.selected.contains(&0));
        let out = render(&mut app, vec![]);
        let close = out
            .platform_output
            .accesskit_update
            .as_ref()
            .unwrap()
            .nodes
            .iter()
            .find(|(_, n)| n.label() == Some("Close"))
            .unwrap()
            .0;
        let _ = render(
            &mut app,
            vec![action(close, egui::accesskit::Action::Focus)],
        );
        let _ = render(
            &mut app,
            vec![egui::Event::Key {
                key: egui::Key::Space,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(!app.active.inspection.open);
    }
    #[test]
    /// Verifies that failed open preview honors a utf16 bom in auto mode.
    fn failed_open_preview_honors_a_utf16_bom_in_auto_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.csv");
        let mut bytes = vec![0xff, 0xfe];
        bytes.extend(
            "name,value\nx,\"bad\n"
                .encode_utf16()
                .flat_map(u16::to_le_bytes),
        );
        std::fs::write(&path, bytes).unwrap();
        let ui = InspectionUi::failed(path, OpenOptions::default(), None, "bad quoting".into());
        assert!(ui.preview.unwrap().text.contains("name,value[LF]"));
    }
    #[test]
    /// Verifies that unchanged editor keeps findings but changed draft invalidates.
    fn unchanged_editor_keeps_findings_but_changed_draft_invalidates() {
        let (_dir, mut app) = app(b"name,id\nAlice,1\nBob,2\n");
        app.start_current_inspection();
        wait_report(&mut app);
        app.active.document.as_mut().unwrap().cell_edit = Some(CellEdit {
            row: 1,
            column: 0,
            source: b"Alice".to_vec(),
            draft: "Alice".into(),
            focus_requested: false,
        });
        assert!(
            app.active
                .inspection
                .snapshot
                .as_ref()
                .unwrap()
                .current(app.active.document.as_ref().unwrap())
        );
        app.active
            .document
            .as_mut()
            .unwrap()
            .cell_edit
            .as_mut()
            .unwrap()
            .draft = "Changed".into();
        assert!(
            !app.active
                .inspection
                .snapshot
                .as_ref()
                .unwrap()
                .current(app.active.document.as_ref().unwrap())
        );
        app.active.document.as_mut().unwrap().cell_edit = None;
        app.active.document.as_mut().unwrap().begin_header_edit(0);
        assert!(
            app.active
                .inspection
                .snapshot
                .as_ref()
                .unwrap()
                .current(app.active.document.as_ref().unwrap())
        );
    }

    #[test]
    /// Verifies that unrelated notice does not destroy inspection.
    fn unrelated_notice_does_not_destroy_inspection() {
        let (_dir, mut app) = app(b"name,id\nAlice,1\nBob\n");
        app.start_current_inspection();
        wait_report(&mut app);
        app.active.inspection.selected.insert(0);
        app.active.notice = Some(AppMessage::warning("Choose a column first."));
        app.show_operation_diagnostic();
        assert!(app.active.inspection.report.is_some());
        assert!(app.active.inspection.selected.contains(&0));
    }

    #[test]
    /// Verifies that failed target survives other opened files.
    fn failed_target_survives_other_opened_files() {
        let (dir, mut app) = app(b"name,id\nAlice,1\n");
        let bad = dir.path().join("bad.csv");
        let other = dir.path().join("other.csv");
        std::fs::write(&bad, b"a,b\n1,\"bad\n").unwrap();
        std::fs::write(&other, b"name,id\nBob,2\n").unwrap();
        let second = dir.path().join("second-bad.csv");
        std::fs::write(&second, b"a,b\n1,\"bad\n").unwrap();
        app.handle_dropped_paths(vec![Some(bad.clone()), Some(other), Some(second), None]);
        app.show_operation_diagnostic();
        assert_eq!(app.active.inspection.path, Some(bad));
    }

    #[test]
    /// Verifies that recovery releases its reservation in the completion poll.
    fn recovery_releases_its_reservation_in_the_completion_poll() {
        let (dir, mut app) = app(b"name,id\nAlice,1\nBob\n");
        app.start_current_inspection();
        wait_report(&mut app);
        let out = dir.path().join("copy.csv");
        app.active.inspection.recovery = Some(
            RecoveryJob::start(
                app.active.inspection.report.as_ref().unwrap(),
                &BTreeSet::from([0]),
                &out,
            )
            .unwrap(),
        );
        app.active.write_destination = Some(out);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !app
            .active
            .inspection
            .recovery
            .as_ref()
            .unwrap()
            .progress()
            .done
        {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        app.active.inspection.last_poll = Instant::now() - POLL_INTERVAL;
        app.poll_inspections(&egui::Context::default());
        assert!(app.active.write_destination.is_none());
    }
}
